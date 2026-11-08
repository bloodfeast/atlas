//! The PNG/BMP reader and writer, deflate and the crop command against
//! hostile input, and PNG round trips through an independent decoder and
//! an independent encoder written here (bitwise CRC-32, a small inflate,
//! stored deflate blocks, every filter type).

use std::time::{Duration, Instant};

use crate::gen::{each, Rng, DEEP};
use atlas::crop::{self, Cut};
use atlas::picture::{self, Image};

// ------------------------------------------------- an independent PNG reader

fn crc32_bitwise(d: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in d {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn adler32_plain(d: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in d {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// A deliberately simple inflate (RFC 1951), sharing nothing with
/// `picture::inflate`.
fn inflate_ref(d: &[u8]) -> Vec<u8> {
    struct In<'a> {
        d: &'a [u8],
        bit: usize,
    }
    impl In<'_> {
        fn get(&mut self, n: usize) -> u32 {
            let mut v = 0u32;
            for i in 0..n {
                let byte = self.d[self.bit / 8];
                v |= u32::from((byte >> (self.bit % 8)) & 1) << i;
                self.bit += 1;
            }
            v
        }
    }
    // Decode by walking codes bit by bit against (length, code) pairs.
    fn table(lengths: &[u8]) -> Vec<(u8, u32, u16)> {
        let mut bl = [0u32; 16];
        for &l in lengths {
            bl[l as usize] += 1;
        }
        bl[0] = 0;
        let mut next = [0u32; 16];
        let mut code = 0;
        for b in 1..16 {
            code = (code + bl[b - 1]) << 1;
            next[b] = code;
        }
        let mut t = Vec::new();
        for (sym, &l) in lengths.iter().enumerate() {
            if l > 0 {
                t.push((l, next[l as usize], sym as u16));
                next[l as usize] += 1;
            }
        }
        t
    }
    fn sym(i: &mut In, t: &[(u8, u32, u16)]) -> u16 {
        let (mut code, mut len) = (0u32, 0u8);
        loop {
            code = (code << 1) | i.get(1);
            len += 1;
            if let Some(&(_, _, s)) = t.iter().find(|&&(l, c, _)| l == len && c == code) {
                return s;
            }
            assert!(len < 16, "bad code");
        }
    }
    const LB: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
    const LE: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
    const DB: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
    const DE: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
    let mut i = In { d, bit: 0 };
    let mut out = Vec::new();
    loop {
        let last = i.get(1);
        match i.get(2) {
            0 => {
                i.bit = i.bit.div_ceil(8) * 8;
                let len = i.get(16) as usize;
                let _nlen = i.get(16);
                for _ in 0..len {
                    out.push(i.get(8) as u8);
                }
            }
            t => {
                let (lit, dist) = if t == 1 {
                    let mut l = vec![8u8; 288];
                    l[144..256].fill(9);
                    l[256..280].fill(7);
                    (table(&l), table(&[5u8; 30]))
                } else {
                    let hlit = i.get(5) as usize + 257;
                    let hdist = i.get(5) as usize + 1;
                    let hclen = i.get(4) as usize + 4;
                    const ORD: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                    let mut cl = [0u8; 19];
                    for &o in ORD.iter().take(hclen) {
                        cl[o] = i.get(3) as u8;
                    }
                    let ct = table(&cl);
                    let mut lens = Vec::new();
                    while lens.len() < hlit + hdist {
                        match sym(&mut i, &ct) {
                            s @ 0..=15 => lens.push(s as u8),
                            16 => {
                                let p = *lens.last().unwrap();
                                for _ in 0..3 + i.get(2) {
                                    lens.push(p);
                                }
                            }
                            17 => lens.extend(std::iter::repeat_n(0, 3 + i.get(3) as usize)),
                            _ => lens.extend(std::iter::repeat_n(0, 11 + i.get(7) as usize)),
                        }
                    }
                    (table(&lens[..hlit]), table(&lens[hlit..]))
                };
                loop {
                    let s = sym(&mut i, &lit);
                    if s < 256 {
                        out.push(s as u8);
                    } else if s == 256 {
                        break;
                    } else {
                        let k = (s - 257) as usize;
                        let len = LB[k] as usize + i.get(LE[k] as usize) as usize;
                        let dk = sym(&mut i, &dist) as usize;
                        let dd = DB[dk] as usize + i.get(DE[dk] as usize) as usize;
                        for _ in 0..len {
                            out.push(out[out.len() - dd]);
                        }
                    }
                }
            }
        }
        if last == 1 {
            return out;
        }
    }
}

fn unfilter_ref(raw: &[u8], w: usize, h: usize, bpp: usize) -> Vec<u8> {
    let stride = w * bpp;
    let mut out = vec![0u8; stride * h];
    for y in 0..h {
        let f = raw[y * (stride + 1)];
        for x in 0..stride {
            let v = raw[y * (stride + 1) + 1 + x];
            let a = if x >= bpp { out[y * stride + x - bpp] } else { 0 } as i32;
            let b = if y > 0 { out[(y - 1) * stride + x] } else { 0 } as i32;
            let c = if x >= bpp && y > 0 { out[(y - 1) * stride + x - bpp] } else { 0 } as i32;
            let pred = match f {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                4 => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => panic!("filter {f}"),
            };
            out[y * stride + x] = (i32::from(v) + pred) as u8;
        }
    }
    out
}

/// Decodes a PNG written by `picture::encode_png` with the code above.
fn decode_ref(png: &[u8]) -> Image {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let (mut at, mut w, mut h, mut ctype) = (8, 0, 0, 0);
    let mut idat = Vec::new();
    loop {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
        let kind = &png[at + 4..at + 8];
        let data = &png[at + 8..at + 8 + len];
        let crc = u32::from_be_bytes(png[at + 8 + len..at + 12 + len].try_into().unwrap());
        assert_eq!(crc, crc32_bitwise(&png[at + 4..at + 8 + len]), "chunk CRC");
        match kind {
            b"IHDR" => {
                w = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
                h = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
                assert_eq!(data[8], 8, "bit depth");
                ctype = data[9];
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        at += 12 + len;
    }
    assert_eq!(idat[0] & 0x0f, 8, "zlib method");
    assert_eq!((u16::from(idat[0]) << 8 | u16::from(idat[1])) % 31, 0, "zlib check bits");
    let raw = inflate_ref(&idat[2..]);
    let adler = u32::from_be_bytes(idat[idat.len() - 4..].try_into().unwrap());
    assert_eq!(adler, adler32_plain(&raw), "adler32");
    let bpp = if ctype == 6 { 4 } else { 3 };
    let px = unfilter_ref(&raw, w, h, bpp);
    let rgba = if bpp == 4 { px } else { px.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect() };
    Image { width: w, height: h, rgba }
}

// --------------------------------------------- an independent PNG writer

fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32_bitwise(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// RGBA PNG with stored deflate blocks and per-row filter `filters[y % n]`.
fn encode_ref(img: &Image, filters: &[u8]) -> Vec<u8> {
    let stride = img.width * 4;
    let mut raw = Vec::new();
    for y in 0..img.height {
        let f = filters[y % filters.len()];
        raw.push(f);
        for x in 0..stride {
            let v = img.rgba[y * stride + x] as i32;
            let a = if x >= 4 { img.rgba[y * stride + x - 4] } else { 0 } as i32;
            let b = if y > 0 { img.rgba[(y - 1) * stride + x] } else { 0 } as i32;
            let c = if x >= 4 && y > 0 { img.rgba[(y - 1) * stride + x - 4] } else { 0 } as i32;
            let pred = match f {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                _ => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
            };
            raw.push((v - pred) as u8);
        }
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = if raw.is_empty() { vec![&[][..]] } else { raw.chunks(65535).collect() };
    for (i, b) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32_plain(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(img.width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(img.height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn gen_image(r: &mut Rng) -> Image {
    let (w, h) = (1 + r.below(40), 1 + r.below(40));
    let flat = r.chance(1, 3);
    let base = r.bytes(4);
    let rgba = (0..w * h * 4).map(|i| if flat { base[i % 4] } else { (r.next() >> (r.below(3) * 8)) as u8 }).collect();
    Image { width: w, height: h, rgba }
}

fn png_round_trips(n: usize) {
    each(50, n, gen_image, |img| {
        let png = picture::encode_png(img);
        let back = decode_ref(&png);
        assert_eq!((back.width, back.height), (img.width, img.height));
        assert!(back.rgba == img.rgba, "the independent decoder reads different pixels");
        let own = picture::decode_png(&png).unwrap();
        assert!(own.rgba == img.rgba);
        let mut filters = [0u8, 1, 2, 3, 4];
        filters.rotate_left(img.width % 5);
        let ref_png = encode_ref(img, &filters);
        let got = picture::decode_png(&ref_png).unwrap();
        assert!(got.rgba == img.rgba, "picture::decode_png misreads an independently written PNG");
        assert_eq!(picture::crc32(&png[12..29]), crc32_bitwise(&png[12..29]));
        assert_eq!(picture::adler32(&img.rgba), adler32_plain(&img.rgba));
        let d = picture::deflate(&img.rgba);
        assert!(inflate_ref(&d) == img.rgba, "picture::deflate output is not standard deflate");
        assert!(picture::inflate(&d).unwrap() == img.rgba);
    });
}

#[test]
fn png_round_trips_through_an_independent_codec() {
    png_round_trips(300);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_png_round_trips() {
    png_round_trips(300 * DEEP);
}

fn decoders_never_panic(n: usize) {
    let seeds: Vec<Vec<u8>> = {
        let mut r = Rng::new(51);
        (0..8).map(|_| picture::encode_png(&gen_image(&mut r))).collect()
    };
    let bmp = {
        let mut b = b"BM".to_vec();
        b.extend_from_slice(&[0; 8]);
        b.extend_from_slice(&54u32.to_le_bytes());
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&2i32.to_le_bytes());
        b.extend_from_slice(&(-2i32).to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(&[0; 24]);
        b.extend_from_slice(&[9; 16]);
        b
    };
    each(52, n, |r| {
        let mut b = if r.chance(1, 5) { bmp.clone() } else { r.pick(&seeds).clone() };
        r.mutate(&mut b);
        b
    }, |b| {
        if let Ok(img) = picture::decode(b) {
            assert_eq!(img.rgba.len(), img.width * img.height * 4);
        }
        let _ = picture::inflate(b);
    });
}

#[test]
fn fuzz_image_decoders() {
    decoders_never_panic(4000);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_fuzz_image_decoders() {
    decoders_never_panic(4000 * DEEP);
}

fn png_with(ihdr: &[u8], idat: &[u8]) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", ihdr);
    chunk(&mut out, b"IDAT", idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn ihdr(w: u32, h: u32, depth: u8, ctype: u8) -> Vec<u8> {
    let mut v = w.to_be_bytes().to_vec();
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[depth, ctype, 0, 0, 0]);
    v
}

#[test]
fn every_prefix_of_a_png_is_an_error_not_a_panic() {
    let png = picture::encode_png(&Image { width: 7, height: 5, rgba: (0..140).map(|i| i as u8).collect() });
    for n in 0..png.len() {
        let _ = picture::decode(&png[..n]);
    }
    // A short IHDR.
    for len in 0..13 {
        let _ = picture::decode(&png_with(&ihdr(1, 1, 8, 6)[..len], &picture::deflate(&[0, 1, 2, 3, 4])));
    }
}

/// Dimensions of 0, of 2^32 - 1, and products that overflow: an error,
/// never an abort from a giant allocation.
#[test]
fn hostile_dimensions_are_refused() {
    let small = {
        let mut z = vec![0x78, 0x01];
        z.extend(picture::deflate(&[0u8; 64]));
        z
    };
    for (w, h, depth, ctype) in [
        (0, 0, 8, 6),
        (0, 10, 8, 6),
        (10, 0, 8, 6),
        (u32::MAX, u32::MAX, 16, 6),
        (u32::MAX, 1, 16, 6),
        (1, u32::MAX, 8, 6),
        (1 << 31, 1 << 31, 8, 6),
        (0x4000_0000, 4, 16, 6),
        (0x2000_0001, 0x2000_0001, 16, 6),
    ] {
        let t = Instant::now();
        let r = picture::decode(&png_with(&ihdr(w, h, depth, ctype), &small));
        if let Ok(img) = &r {
            assert_eq!(img.rgba.len(), img.width * img.height * 4);
        }
        assert!(t.elapsed() < Duration::from_secs(2), "{w}x{h} took {:?}", t.elapsed());
    }
}

/// A 1x1 picture whose IDAT inflates to hundreds of megabytes is refused
/// once the data passes what the header allows.
#[test]
fn a_decompression_bomb_is_refused_early() {
    // Fixed Huffman: literal 0, then (length 258, distance 1) repeated.
    let mut bits: Vec<u8> = Vec::new();
    let mut put = |v: u32, n: u32, rev: bool| {
        for i in 0..n {
            let b = if rev { (v >> (n - 1 - i)) & 1 } else { (v >> i) & 1 };
            bits.push(b as u8);
        }
    };
    put(1, 1, false);
    put(1, 2, false);
    put(0x30, 8, true);
    for _ in 0..1_000_000 {
        put(0b1100_0101, 8, true);
        put(0, 5, true);
    }
    put(0, 7, true);
    let mut d = vec![0x78, 0x01];
    for c in bits.chunks(8) {
        d.push(c.iter().enumerate().fold(0u8, |a, (i, &b)| a | (b << i)));
    }
    let png = png_with(&ihdr(1, 1, 8, 6), &d);
    let t = Instant::now();
    let r = picture::decode(&png);
    let took = t.elapsed();
    assert!(r.is_err(), "a bomb decoded");
    assert!(took < Duration::from_millis(150), "the bomb was inflated in full: {took:?}");
}

#[test]
fn crop_rects_out_of_bounds_and_overflowing() {
    let img = Image { width: 4, height: 3, rgba: (0..48).map(|i| i as u8).collect() };
    let max = usize::MAX;
    for rect in [(0, 0, 0, 0), (4, 3, 1, 1), (3, 2, max, max), (max, max, max, max), (0, 0, max, 1), (1, 1, 2, 1)] {
        let out = crop::apply(&img, &Cut { rect: Some(rect), ..Cut::default() }).unwrap();
        assert!(out.width <= 4 && out.height <= 3 && out.rgba.len() == out.width * out.height * 4, "{rect:?}");
    }
    for m in [(0, 0), (1, 1), (max, max), (1, max)] {
        let out = crop::apply(&img, &Cut { max: Some(m), ..Cut::default() }).unwrap();
        assert_eq!(out.rgba.len(), out.width * out.height * 4);
    }
    for f in [(0, 1, 1), (2, 1, 3), (1, 4, 3), (1, 1, 0)] {
        assert!(crop::apply(&img, &Cut { frames: Some(f), ..Cut::default() }).is_err(), "{f:?}");
    }
    // The whole range of a huge frame count is the whole picture.
    let all = crop::apply(&img, &Cut { frames: Some((1, max, max)), ..Cut::default() }).unwrap();
    assert_eq!((all.width, all.height), (4, 3), "frames 1-MAX of MAX");
    let last = crop::apply(&img, &Cut { frames: Some((max, max, max)), along_x: Some(true), ..Cut::default() }).unwrap();
    assert!(last.width <= 4);
    for args in [vec!["a.png"], vec!["a.png", "b.png", "--rect", "1,2,3"], vec!["a.png", "b.png", "--rect", "-1,0,1,1"], vec!["a.png", "b.png", "--frames", "99999999999999999999999-1"], vec!["--max"]] {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let _ = crop::parse(&a);
    }
    // Shrinking by any factor stays in bounds.
    for k in [0, 1, 2, 3, 5, max] {
        let s = img.shrink(k);
        assert_eq!(s.rgba.len(), s.width * s.height * 4);
    }
}
