//! Pictures: PNG and BMP decoding to RGBA, and PNG encoding, owned (no
//! image crate). Enough for `atlas crop`: 8- and 16-bit PNG in every
//! colour type except sub-byte depths and interlacing, 24- and 32-bit BMP.
//!
//! The PNG encoder writes 8-bit RGB (RGBA when any pixel is not opaque),
//! each row filtered by the filter with the smallest sum of absolute
//! values, compressed by deflate with a one-candidate hash match finder and,
//! per block of 65,536 tokens, Huffman codes built from that block's own
//! symbol counts (the fixed codes when those come out smaller).

/// An 8-bit RGBA picture, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl Image {
    /// A sub-rectangle (clipped to the picture).
    pub fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> Image {
        let x = x.min(self.width);
        let y = y.min(self.height);
        let w = w.min(self.width - x);
        let h = h.min(self.height - y);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for row in y..y + h {
            let s = (row * self.width + x) * 4;
            rgba.extend_from_slice(&self.rgba[s..s + w * 4]);
        }
        Image { width: w, height: h, rgba }
    }

    /// Shrunk by an integer factor `k`, each output pixel the mean of its
    /// `k` by `k` block (clipped at the right and bottom edges).
    pub fn shrink(&self, k: usize) -> Image {
        if k <= 1 {
            return self.clone();
        }
        let (w, h) = (self.width.div_ceil(k), self.height.div_ceil(k));
        let mut rgba = vec![0u8; w * h * 4];
        for oy in 0..h {
            for ox in 0..w {
                let mut sum = [0u32; 4];
                let mut n = 0u32;
                for y in oy * k..((oy + 1) * k).min(self.height) {
                    for x in ox * k..((ox + 1) * k).min(self.width) {
                        let p = (y * self.width + x) * 4;
                        for c in 0..4 {
                            sum[c] += u32::from(self.rgba[p + c]);
                        }
                        n += 1;
                    }
                }
                let o = (oy * w + ox) * 4;
                for c in 0..4 {
                    rgba[o + c] = ((sum[c] + n / 2) / n) as u8;
                }
            }
        }
        Image { width: w, height: h, rgba }
    }
}

/// Decodes a PNG or BMP file's bytes, by signature.
///
/// # Errors
///
/// A description of what is not supported or malformed.
pub fn decode(bytes: &[u8]) -> Result<Image, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png(bytes)
    } else if bytes.starts_with(b"BM") {
        decode_bmp(bytes)
    } else {
        Err("not a PNG or BMP file".into())
    }
}

fn be32(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| "truncated".to_string())
}

fn le32(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| "truncated".to_string())
}

/// Decodes a PNG.
///
/// # Errors
///
/// Interlaced pictures, bit depths below 8, and malformed data.
pub fn decode_png(b: &[u8]) -> Result<Image, String> {
    let mut at = 8;
    let (mut w, mut h, mut depth, mut ctype) = (0usize, 0usize, 0u8, 0u8);
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut idat = Vec::new();
    while at + 8 <= b.len() {
        let len = be32(b, at)? as usize;
        let kind = &b[at + 4..at + 8];
        let data = b.get(at + 8..at + 8 + len).ok_or("truncated chunk")?;
        match kind {
            b"IHDR" => {
                if data.len() < 13 {
                    return Err("IHDR is shorter than 13 bytes".into());
                }
                w = be32(data, 0)? as usize;
                h = be32(data, 4)? as usize;
                depth = data[8];
                ctype = data[9];
                if data[12] != 0 {
                    return Err("interlaced PNG is not supported".into());
                }
                if depth != 8 && depth != 16 {
                    return Err(format!("PNG bit depth {depth} is not supported"));
                }
            }
            b"PLTE" => palette = data.chunks_exact(3).map(|c| [c[0], c[1], c[2], 255]).collect(),
            b"tRNS" if ctype == 3 => {
                for (i, a) in data.iter().enumerate() {
                    if let Some(p) = palette.get_mut(i) {
                        p[3] = *a;
                    }
                }
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        at += 12 + len;
    }
    let channels = match ctype {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        6 => 4,
        c => return Err(format!("PNG colour type {c}")),
    };
    if ctype == 3 && depth != 8 {
        return Err("16-bit palette".into());
    }
    let bpp = channels * usize::from(depth) / 8;
    let too_large = || format!("PNG of {w} by {h} pixels is too large");
    let stride = w.checked_mul(bpp).ok_or_else(too_large)?;
    let need = stride.checked_add(1).and_then(|s| s.checked_mul(h)).ok_or_else(too_large)?;
    let px_bytes = w.checked_mul(h).and_then(|p| p.checked_mul(4)).ok_or_else(too_large)?;
    // Never inflate past what the header allows: a small IDAT can expand
    // a thousandfold.
    let raw = inflate_max(idat.get(2..).ok_or("empty IDAT")?, need)?;
    if raw.len() < need {
        return Err("short image data".into());
    }
    let mut cur = vec![0u8; stride];
    let mut prev = vec![0u8; stride];
    let mut rgba = Vec::with_capacity(px_bytes);
    for y in 0..h {
        let line = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
        let f = line[0];
        cur.copy_from_slice(&line[1..]);
        for i in 0..stride {
            let a = if i >= bpp { cur[i - bpp] } else { 0 };
            let up = prev[i];
            let c = if i >= bpp { prev[i - bpp] } else { 0 };
            cur[i] = cur[i].wrapping_add(match f {
                0 => 0,
                1 => a,
                2 => up,
                3 => ((u16::from(a) + u16::from(up)) / 2) as u8,
                4 => paeth(a, up, c),
                _ => return Err(format!("PNG filter {f}")),
            });
        }
        let step = usize::from(depth) / 8;
        for px in cur.chunks_exact(bpp) {
            let s = |i: usize| px[i * step];
            match ctype {
                0 => rgba.extend_from_slice(&[s(0), s(0), s(0), 255]),
                2 => rgba.extend_from_slice(&[s(0), s(1), s(2), 255]),
                3 => rgba.extend_from_slice(palette.get(usize::from(px[0])).ok_or("palette index")?),
                4 => rgba.extend_from_slice(&[s(0), s(0), s(0), s(1)]),
                _ => rgba.extend_from_slice(&[s(0), s(1), s(2), s(3)]),
            }
        }
        std::mem::swap(&mut cur, &mut prev);
    }
    Ok(Image { width: w, height: h, rgba })
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = ((p - i16::from(a)).abs(), (p - i16::from(b)).abs(), (p - i16::from(c)).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Decodes an uncompressed 24- or 32-bit BMP (bottom-up or top-down; a
/// 32-bit one's fourth byte is alpha only when it is not all zero).
///
/// # Errors
///
/// Other depths, compression, and truncated data.
pub fn decode_bmp(b: &[u8]) -> Result<Image, String> {
    let off = le32(b, 10)? as usize;
    let w = le32(b, 18)? as i32;
    let hh = le32(b, 22)? as i32;
    let bpp = u16::from_le_bytes([*b.get(28).ok_or("truncated")?, *b.get(29).ok_or("truncated")?]);
    let comp = le32(b, 30)?;
    if !(bpp == 24 || bpp == 32) || !(comp == 0 || (comp == 3 && bpp == 32)) || w <= 0 {
        return Err(format!("BMP of {bpp} bits, compression {comp}, is not supported"));
    }
    let (w, h) = (w as usize, hh.unsigned_abs() as usize);
    let bytes = usize::from(bpp) / 8;
    let stride = (w * bytes).div_ceil(4) * 4;
    let end = stride.checked_mul(h).and_then(|n| n.checked_add(off)).ok_or("BMP dimensions overflow")?;
    if b.len() < end {
        return Err("short BMP data".into());
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    let alpha = bytes == 4 && (0..h).any(|y| (0..w).any(|x| b[off + y * stride + x * 4 + 3] != 0));
    for y in 0..h {
        let src = if hh > 0 { h - 1 - y } else { y };
        let row = &b[off + src * stride..off + src * stride + w * bytes];
        for p in row.chunks_exact(bytes) {
            rgba.extend_from_slice(&[p[2], p[1], p[0], if alpha { p[3] } else { 255 }]);
        }
    }
    Ok(Image { width: w, height: h, rgba })
}

// ---- inflate -------------------------------------------------------------

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl Bits<'_> {
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        while self.cnt < n {
            let byte = *self.d.get(self.pos).ok_or("deflate data ends early")?;
            self.pos += 1;
            self.buf |= u64::from(byte) << self.cnt;
            self.cnt += 8;
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.cnt -= n;
        Ok(v)
    }
}

/// A canonical Huffman code: symbols by code length, counts per length.
struct Huff {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Huff {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[usize::from(l)] += 1;
        }
        counts[0] = 0;
        let mut offs = [0u16; 16];
        for l in 1..16 {
            offs[l] = offs[l - 1] + counts[l - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[usize::from(offs[usize::from(l)])] = s as u16;
                offs[usize::from(l)] += 1;
            }
        }
        Huff { counts, symbols }
    }

    fn decode(&self, b: &mut Bits) -> Result<u16, String> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.bits(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - first < count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("bad Huffman code".into())
    }
}

const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

fn fixed_lengths() -> (Vec<u8>, Vec<u8>) {
    let mut l = vec![8u8; 288];
    l[144..256].fill(9);
    l[256..280].fill(7);
    (l, vec![5u8; 30])
}

/// Inflates a raw deflate stream (RFC 1951).
///
/// # Errors
///
/// Malformed or truncated data.
pub fn inflate(d: &[u8]) -> Result<Vec<u8>, String> {
    inflate_max(d, usize::MAX)
}

/// Inflates a raw deflate stream, refusing to produce more than `max`
/// bytes (a decompression bomb stops at the cap, not at the end).
///
/// # Errors
///
/// Malformed or truncated data, or output past `max`.
pub fn inflate_max(d: &[u8], max: usize) -> Result<Vec<u8>, String> {
    let mut b = Bits { d, pos: 0, buf: 0, cnt: 0 };
    let mut out: Vec<u8> = Vec::with_capacity(d.len().saturating_mul(4).min(max).min(64 << 20));
    let past = || format!("inflated data runs past {max} bytes");
    loop {
        let last = b.bits(1)?;
        match b.bits(2)? {
            0 => {
                b.buf = 0;
                b.cnt = 0;
                let len = usize::from(u16::from_le_bytes([*d.get(b.pos).ok_or("eof")?, *d.get(b.pos + 1).ok_or("eof")?]));
                b.pos += 4;
                out.extend_from_slice(d.get(b.pos..b.pos + len).ok_or("stored block ends early")?);
                b.pos += len;
                if out.len() > max {
                    return Err(past());
                }
            }
            t @ (1 | 2) => {
                let (lit, dist) = if t == 1 {
                    let (l, dd) = fixed_lengths();
                    (Huff::new(&l), Huff::new(&dd))
                } else {
                    let hlit = b.bits(5)? as usize + 257;
                    let hdist = b.bits(5)? as usize + 1;
                    let hclen = b.bits(4)? as usize + 4;
                    let mut cl = [0u8; 19];
                    for &o in CL_ORDER.iter().take(hclen) {
                        cl[o] = b.bits(3)? as u8;
                    }
                    let ch = Huff::new(&cl);
                    let mut lens = Vec::with_capacity(hlit + hdist);
                    while lens.len() < hlit + hdist {
                        let sym = ch.decode(&mut b)?;
                        match sym {
                            0..=15 => lens.push(sym as u8),
                            16 => {
                                let p = *lens.last().ok_or("repeat with no length")?;
                                for _ in 0..3 + b.bits(2)? {
                                    lens.push(p);
                                }
                            }
                            17 => lens.extend(std::iter::repeat_n(0, 3 + b.bits(3)? as usize)),
                            _ => lens.extend(std::iter::repeat_n(0, 11 + b.bits(7)? as usize)),
                        }
                    }
                    if lens.len() > hlit + hdist {
                        return Err("code lengths overrun".into());
                    }
                    (Huff::new(&lens[..hlit]), Huff::new(&lens[hlit..]))
                };
                loop {
                    let sym = lit.decode(&mut b)?;
                    if sym < 256 {
                        if out.len() == max {
                            return Err(past());
                        }
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let i = usize::from(sym - 257);
                        let len = usize::from(*LEN_BASE.get(i).ok_or("bad length")?) + b.bits(u32::from(LEN_EXTRA[i]))? as usize;
                        let di = usize::from(dist.decode(&mut b)?);
                        let dst = usize::from(*DIST_BASE.get(di).ok_or("bad distance")?) + b.bits(u32::from(DIST_EXTRA[di]))? as usize;
                        if dst > out.len() {
                            return Err("distance before the start".into());
                        }
                        if out.len() + len > max {
                            return Err(past());
                        }
                        let from = out.len() - dst;
                        for k in 0..len {
                            out.push(out[from + k]);
                        }
                    }
                }
            }
            _ => return Err("bad block type".into()),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

// ---- encode --------------------------------------------------------------

struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    cnt: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, n: u32) {
        self.buf |= u64::from(v) << self.cnt;
        self.cnt += n;
        while self.cnt >= 8 {
            self.out.push(self.buf as u8);
            self.buf >>= 8;
            self.cnt -= 8;
        }
    }

    /// A Huffman code, most significant bit first.
    fn code(&mut self, code: u32, n: u32) {
        let mut r = 0;
        for i in 0..n {
            r |= ((code >> i) & 1) << (n - 1 - i);
        }
        self.put(r, n);
    }
}

/// One LZ77 token: a literal byte, or a match (length 3..=258, distance
/// 1..=32768).
#[derive(Debug, Clone, Copy)]
enum Token {
    Lit(u8),
    Match(u16, u16),
}

/// The length code (index into LEN_BASE) of a match length.
fn len_code(len: usize) -> usize {
    LEN_BASE.iter().rposition(|&b| usize::from(b) <= len).unwrap_or(0)
}

/// The distance code (index into DIST_BASE) of a distance.
fn dist_code(dist: usize) -> usize {
    DIST_BASE.iter().rposition(|&b| usize::from(b) <= dist).unwrap_or(0)
}

/// The LZ77 pass: a one-candidate hash match finder.
fn tokens(d: &[u8]) -> Vec<Token> {
    const HBITS: u32 = 15;
    let mut out = Vec::with_capacity(d.len() / 2 + 16);
    let mut head = vec![usize::MAX; 1 << HBITS];
    let hash = |i: usize| ((u32::from(d[i]) << 16 | u32::from(d[i + 1]) << 8 | u32::from(d[i + 2])).wrapping_mul(2_654_435_761) >> (32 - HBITS)) as usize;
    let mut i = 0;
    while i < d.len() {
        let mut best = (0usize, 0usize);
        if i + 3 <= d.len() {
            let h = hash(i);
            let cand = head[h];
            head[h] = i;
            if cand != usize::MAX && i - cand <= 32768 {
                let max = (d.len() - i).min(258);
                let mut l = 0;
                while l < max && d[cand + l] == d[i + l] {
                    l += 1;
                }
                if l >= 3 {
                    best = (l, i - cand);
                }
            }
        }
        if best.0 >= 3 {
            let (len, dist) = best;
            out.push(Token::Match(len as u16, dist as u16));
            // Index the skipped positions so later matches can find them.
            for k in i + 1..(i + len).min(d.len().saturating_sub(2)) {
                head[hash(k)] = k;
            }
            i += len;
        } else {
            out.push(Token::Lit(d[i]));
            i += 1;
        }
    }
    out
}

/// Huffman code lengths for `freq`, none longer than `limit`: a Huffman
/// tree, then lengths past the limit folded back while keeping the Kraft
/// sum exact (the lengths stay a complete prefix code). Symbols of
/// frequency 0 get length 0. At least two symbols get a code, so every
/// decoder sees a complete code.
fn code_lengths(freq: &[u32], limit: u8) -> Vec<u8> {
    let mut f: Vec<u32> = freq.to_vec();
    // A complete code needs two symbols: give the first unused ones a count.
    let mut used = f.iter().filter(|&&x| x > 0).count();
    for x in f.iter_mut() {
        if used >= 2 {
            break;
        }
        if *x == 0 {
            *x = 1;
            used += 1;
        }
    }
    let mut syms: Vec<usize> = (0..f.len()).filter(|&s| f[s] > 0).collect();
    // Most frequent first; ties by symbol, so the code is deterministic.
    syms.sort_by(|&a, &b| f[b].cmp(&f[a]).then(a.cmp(&b)));
    // The tree: leaves 0..n, internal nodes after; two-queue merge over
    // leaves sorted ascending.
    let n = syms.len();
    let mut weight: Vec<u64> = syms.iter().rev().map(|&s| u64::from(f[s])).collect();
    let mut parent = vec![usize::MAX; 2 * n - 1];
    let (mut leaf, mut node) = (0usize, n);
    let mut next = n;
    let pick = |weight: &Vec<u64>, leaf: &mut usize, node: &mut usize, next: usize| -> usize {
        if *leaf < n && (*node >= next || weight[*leaf] <= weight[*node]) {
            *leaf += 1;
            *leaf - 1
        } else {
            *node += 1;
            *node - 1
        }
    };
    while next < 2 * n - 1 {
        let a = pick(&weight, &mut leaf, &mut node, next);
        let b = pick(&weight, &mut leaf, &mut node, next);
        weight.push(weight[a] + weight[b]);
        parent[a] = next;
        parent[b] = next;
        next += 1;
    }
    let mut depth = vec![0u32; 2 * n - 1];
    for i in (0..2 * n - 2).rev() {
        depth[i] = depth[parent[i]] + 1;
    }
    // Counts per length, the long ones clamped to the limit.
    let lim = usize::from(limit);
    let mut count = vec![0u32; lim + 1];
    for &d in &depth[..n] {
        count[(d as usize).min(lim)] += 1;
    }
    // Restore the Kraft sum: each step takes one code off the limit and
    // splits a shorter leaf in two.
    let mut total: u64 = (1..=lim).map(|l| u64::from(count[l]) << (lim - l)).sum();
    while total > 1 << lim {
        count[lim] -= 1;
        for l in (1..lim).rev() {
            if count[l] != 0 {
                count[l] -= 1;
                count[l + 1] += 2;
                break;
            }
        }
        total -= 1;
    }
    // Shortest lengths to the most frequent symbols.
    let mut out = vec![0u8; f.len()];
    let mut it = syms.iter();
    for (l, &c) in count.iter().enumerate().skip(1) {
        for _ in 0..c {
            if let Some(&s) = it.next() {
                out[s] = l as u8;
            }
        }
    }
    out
}

/// Canonical codes for lengths (RFC 1951 3.2.2).
fn canonical(lengths: &[u8]) -> Vec<u32> {
    let mut bl = [0u32; 16];
    for &l in lengths {
        bl[usize::from(l)] += 1;
    }
    bl[0] = 0;
    let mut next = [0u32; 16];
    let mut code = 0;
    for b in 1..16 {
        code = (code + bl[b - 1]) << 1;
        next[b] = code;
    }
    lengths
        .iter()
        .map(|&l| {
            if l == 0 {
                0
            } else {
                let c = next[usize::from(l)];
                next[usize::from(l)] += 1;
                c
            }
        })
        .collect()
}

/// The run-length form of the two code-length tables (symbols 0..=18 with
/// their extra bits).
fn rle(lengths: &[u8]) -> Vec<(u8, u8)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lengths.len() {
        let l = lengths[i];
        let mut run = 1;
        while i + run < lengths.len() && lengths[i + run] == l {
            run += 1;
        }
        let mut left = run;
        if l == 0 {
            while left >= 11 {
                let k = left.min(138);
                out.push((18, (k - 11) as u8));
                left -= k;
            }
            if left >= 3 {
                out.push((17, (left - 3) as u8));
                left = 0;
            }
        } else {
            out.push((l, 0));
            left -= 1;
            while left >= 3 {
                let k = left.min(6);
                out.push((16, (k - 3) as u8));
                left -= k;
            }
        }
        for _ in 0..left {
            out.push((l, 0));
        }
        i += run;
    }
    out
}

/// The order the code-length code's lengths are sent in (RFC 1951 3.2.7).
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// Tokens per block: each block gets its own codes, so a picture whose
/// statistics change down the strip is coded per region.
const BLOCK_TOKENS: usize = 1 << 16;

/// Deflates with a hash match finder and, per block, dynamic Huffman codes
/// built from the block's own symbol counts (or the fixed codes, when they
/// come out smaller).
pub fn deflate(d: &[u8]) -> Vec<u8> {
    let toks = tokens(d);
    let mut w = BitWriter { out: Vec::with_capacity(d.len() / 2 + 16), buf: 0, cnt: 0 };
    let blocks: Vec<&[Token]> = if toks.is_empty() { vec![&toks[..]] } else { toks.chunks(BLOCK_TOKENS).collect() };
    let last = blocks.len() - 1;
    for (bi, block) in blocks.into_iter().enumerate() {
        write_block(&mut w, block, bi == last);
    }
    if w.cnt > 0 {
        w.out.push(w.buf as u8);
    }
    w.out
}

fn write_block(w: &mut BitWriter, block: &[Token], last: bool) {
    let mut lf = vec![0u32; 286];
    let mut df = vec![0u32; 30];
    for t in block {
        match *t {
            Token::Lit(b) => lf[usize::from(b)] += 1,
            Token::Match(l, d) => {
                lf[257 + len_code(usize::from(l))] += 1;
                df[dist_code(usize::from(d))] += 1;
            }
        }
    }
    lf[256] = 1;
    let ll = code_lengths(&lf, 15);
    let dl = code_lengths(&df, 15);
    let hlit = 257.max(ll.iter().rposition(|&l| l != 0).map_or(0, |p| p + 1));
    let hdist = 1.max(dl.iter().rposition(|&l| l != 0).map_or(0, |p| p + 1));
    let mut both = ll[..hlit].to_vec();
    both.extend_from_slice(&dl[..hdist]);
    let runs = rle(&both);
    let mut cf = vec![0u32; 19];
    for &(s, _) in &runs {
        cf[usize::from(s)] += 1;
    }
    let cl = code_lengths(&cf, 7);
    let hclen = 4.max(CL_ORDER.iter().rposition(|&s| cl[s] != 0).map_or(0, |p| p + 1));
    // The cost of each form, in bits, to pick the smaller.
    let extra = |s: u8| match s {
        16 => 2,
        17 => 3,
        18 => 7,
        _ => 0,
    };
    let header = 5 + 5 + 4 + 3 * hclen as u64 + runs.iter().map(|&(s, _)| u64::from(cl[usize::from(s)]) + extra(s)).sum::<u64>();
    let (fl, fd) = fixed_lengths();
    let body = |ll: &[u8], dl: &[u8]| -> u64 {
        let mut bits = u64::from(ll[256]);
        for t in block {
            bits += match *t {
                Token::Lit(b) => u64::from(ll[usize::from(b)]),
                Token::Match(l, d) => {
                    let (li, di) = (len_code(usize::from(l)), dist_code(usize::from(d)));
                    u64::from(ll[257 + li]) + u64::from(LEN_EXTRA[li]) + u64::from(dl[di]) + u64::from(DIST_EXTRA[di])
                }
            };
        }
        bits
    };
    let dynamic = header + body(&ll, &dl) < body(&fl, &fd);
    w.put(u32::from(last), 1);
    let (ll, dl) = if dynamic {
        w.put(2, 2);
        w.put((hlit - 257) as u32, 5);
        w.put((hdist - 1) as u32, 5);
        w.put((hclen - 4) as u32, 4);
        for &s in &CL_ORDER[..hclen] {
            w.put(u32::from(cl[s]), 3);
        }
        let cc = canonical(&cl);
        for &(s, e) in &runs {
            w.code(cc[usize::from(s)], u32::from(cl[usize::from(s)]));
            if s >= 16 {
                w.put(u32::from(e), extra(s) as u32);
            }
        }
        (ll, dl)
    } else {
        w.put(1, 2);
        (fl, fd)
    };
    let (lc, dc) = (canonical(&ll), canonical(&dl));
    for t in block {
        match *t {
            Token::Lit(b) => w.code(lc[usize::from(b)], u32::from(ll[usize::from(b)])),
            Token::Match(l, d) => {
                let (len, dist) = (usize::from(l), usize::from(d));
                let li = len_code(len);
                w.code(lc[257 + li], u32::from(ll[257 + li]));
                w.put((len - usize::from(LEN_BASE[li])) as u32, u32::from(LEN_EXTRA[li]));
                let di = dist_code(dist);
                w.code(dc[di], u32::from(dl[di]));
                w.put((dist - usize::from(DIST_BASE[di])) as u32, u32::from(DIST_EXTRA[di]));
            }
        }
    }
    w.code(lc[256], u32::from(ll[256]));
}

/// The Adler-32 checksum (zlib).
pub fn adler32(d: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in d.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

/// The CRC-32 of PNG chunks.
pub fn crc32(d: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &x in d {
        c ^= u32::from(x);
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// Encodes a picture as an 8-bit PNG: RGB, or RGBA when any pixel is not
/// opaque.
pub fn encode_png(img: &Image) -> Vec<u8> {
    let alpha = img.rgba.chunks_exact(4).any(|p| p[3] != 255);
    let bpp = if alpha { 4 } else { 3 };
    let stride = img.width * bpp;
    let mut raw = Vec::with_capacity((stride + 1) * img.height);
    let mut prev = vec![0u8; stride];
    let mut cur = Vec::with_capacity(stride);
    let mut trial = vec![0u8; stride];
    for y in 0..img.height {
        cur.clear();
        for p in img.rgba[y * img.width * 4..(y + 1) * img.width * 4].chunks_exact(4) {
            cur.extend_from_slice(&p[..bpp]);
        }
        let mut best: (u64, u8, Vec<u8>) = (u64::MAX, 0, Vec::new());
        for f in 0..5u8 {
            for i in 0..stride {
                let a = if i >= bpp { cur[i - bpp] } else { 0 };
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                trial[i] = cur[i].wrapping_sub(match f {
                    0 => 0,
                    1 => a,
                    2 => prev[i],
                    3 => ((u16::from(a) + u16::from(prev[i])) / 2) as u8,
                    _ => paeth(a, prev[i], c),
                });
            }
            let cost: u64 = trial.iter().map(|&v| u64::from((v as i8).unsigned_abs())).sum();
            if cost < best.0 {
                best = (cost, f, trial.clone());
            }
        }
        raw.push(best.1);
        raw.extend_from_slice(&best.2);
        std::mem::swap(&mut prev, &mut cur);
    }
    let mut z = vec![0x78, 0x01];
    z.extend_from_slice(&deflate(&raw));
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(img.width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(img.height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, if alpha { 6 } else { 2 }, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn inflates_a_dynamic_huffman_stream_from_zlib() {
        // zlib.compress(600 random bytes of a skewed alphabet, 9): one dynamic block.
        let z = hex("78da2592db0dc5300843ff9982d50c84d7fe035cd3db4a5145126c1f6a021d605ccc466412864d700df8646543c17df38ada54297b707b02f3607db85d401417e413dfbb0970bfc281f71499bd29582f8bf20d79a8078abc5675aa43bde399da530fd6116bd4b0bafbd6062e6ec0b2e6ef1463bdb54eb56875b6c4d0d7f9d4bf93bc3bb00dc691eb03177a353b6746634ceae6bb8579f83f74b23b8f013533940eea821c126d494ae9bcaa660371d4674ff9d5478c444e30b1e440c7d72fe23a9e245f5a33b34ed7a388a445ab9daffdcc322523ee332123fbe0db1119d4580bee60d33ec1d0fe77f80304700edaef5055d201831130fd118863559b7378b2a4a4200ed3be54af24ac870598482adc8ffcacdbeacbfeff170445d795974996fe5d035d1c74ccc61c6a1ba37963cced1b7ddf3ced0787d0d5c9");
        assert_eq!((z[2] >> 1) & 3, 2);
        let out = inflate(&z[2..]).unwrap();
        assert_eq!(out.len(), 600);
        assert_eq!(adler32(&out), 2_278_610_377);
    }

    #[test]
    fn deflate_round_trips_through_inflate() {
        let mut d = Vec::new();
        for i in 0..20_000u32 {
            d.push((i % 251) as u8 ^ (i / 97) as u8);
        }
        d.extend(std::iter::repeat_n(7u8, 5000));
        let z = deflate(&d);
        assert!(z.len() < d.len());
        assert_eq!(inflate(&z).unwrap(), d);
        assert_eq!(inflate(&deflate(&[])).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn dynamic_blocks_round_trip_and_beat_fixed_codes() {
        // A skewed alphabet with few repeats: dynamic codes pay off. Long
        // enough for several blocks, so block boundaries are crossed.
        let mut d = Vec::new();
        let mut x = 12_345u32;
        for _ in 0..200_000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let r = (x >> 16) % 100;
            d.push(match r {
                0..=59 => b'a',
                60..=84 => b'b',
                85..=94 => b'c' + (r % 5) as u8,
                _ => (x >> 8) as u8,
            });
        }
        let z = deflate(&d);
        assert_eq!(inflate(&z).unwrap(), d);
        // The first block is dynamic (BTYPE 2).
        assert_eq!((z[0] >> 1) & 3, 2);
        // Fixed codes spend 8 or 9 bits on a literal; this alphabet's entropy
        // is about 2.6 bits a byte (3.1 measured, with the code lengths).
        assert!(z.len() * 2 < d.len(), "{} of {}", z.len(), d.len());
    }

    #[test]
    fn code_lengths_are_complete_and_limited() {
        // Fibonacci counts force a deep tree: the limit must fold it.
        let mut f = vec![0u32; 30];
        let (mut a, mut b) = (1u32, 1u32);
        for x in f.iter_mut() {
            *x = a;
            (a, b) = (b, a.saturating_add(b));
        }
        for limit in [7u8, 15] {
            let l = code_lengths(&f, limit);
            assert!(l.iter().all(|&x| x >= 1 && x <= limit), "{l:?}");
            let kraft: f64 = l.iter().map(|&x| 0.5f64.powi(i32::from(x))).sum();
            assert!((kraft - 1.0).abs() < 1e-12, "{kraft}");
        }
        // One used symbol still gets a complete two-code tree.
        let l = code_lengths(&[0, 5, 0], 15);
        assert_eq!(l.iter().filter(|&&x| x == 1).count(), 2);
    }

    #[test]
    fn hostile_headers_and_bombs_are_errors() {
        fn png(ihdr: &[u8], idat: &[u8]) -> Vec<u8> {
            let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
            for (kind, data) in [(&b"IHDR"[..], ihdr), (b"IDAT", idat), (b"IEND", &[])] {
                out.extend_from_slice(&(data.len() as u32).to_be_bytes());
                let mut c = kind.to_vec();
                c.extend_from_slice(data);
                out.extend_from_slice(&c);
                out.extend_from_slice(&crc32(&c).to_be_bytes());
            }
            out
        }
        let ihdr = |w: u32, h: u32, depth: u8| -> Vec<u8> {
            let mut v = w.to_be_bytes().to_vec();
            v.extend_from_slice(&h.to_be_bytes());
            v.extend_from_slice(&[depth, 6, 0, 0, 0]);
            v
        };
        let mut z = vec![0x78, 0x01];
        z.extend(deflate(&[0u8; 5]));
        // A short IHDR used to index past its end.
        for len in 0..13 {
            assert!(decode_png(&png(&ihdr(1, 1, 8)[..len], &z)).is_err());
        }
        // Dimensions whose byte counts overflow.
        assert!(decode_png(&png(&ihdr(u32::MAX, u32::MAX, 16), &z)).is_err());
        // A bomb: 16 MiB of zeros for a 1x1 picture stops at 5 bytes.
        let mut bomb = vec![0x78, 0x01];
        bomb.extend(deflate(&vec![0u8; 16 << 20]));
        let e = decode_png(&png(&ihdr(1, 1, 8), &bomb)).unwrap_err();
        assert!(e.contains("past 5 bytes"), "{e}");
        assert!(inflate_max(&deflate(&[7u8; 1000]), 999).is_err());
        assert_eq!(inflate_max(&deflate(&[7u8; 1000]), 1000).unwrap().len(), 1000);
        // A BMP whose stride times height overflows.
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0; 8]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&i32::MAX.to_le_bytes());
        bmp.extend_from_slice(&i32::MIN.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&32u16.to_le_bytes());
        bmp.extend_from_slice(&[0; 32]);
        assert!(decode_bmp(&bmp).is_err());
    }

    #[test]
    fn png_round_trips_with_and_without_alpha() {
        let (w, h) = (37, 19);
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 7) as u8, (y * 13) as u8, ((x ^ y) * 3) as u8, 255]);
            }
        }
        let img = Image { width: w, height: h, rgba };
        let png = encode_png(&img);
        assert_eq!(png[25], 2);
        assert_eq!(decode(&png).unwrap(), img);
        let mut a = img.clone();
        a.rgba[3] = 9;
        let png = encode_png(&a);
        assert_eq!(png[25], 6);
        assert_eq!(decode(&png).unwrap(), a);
    }

    #[test]
    fn crops_and_shrinks() {
        let img = Image { width: 4, height: 2, rgba: (0..32).collect() };
        let c = img.crop(1, 1, 2, 5);
        assert_eq!((c.width, c.height), (2, 1));
        assert_eq!(c.rgba, (20..28).collect::<Vec<u8>>());
        let s = img.shrink(2);
        assert_eq!((s.width, s.height), (2, 1));
        // Mean of pixels 0, 1, 4, 5 per channel.
        assert_eq!(&s.rgba[..4], &[10, 11, 12, 13]);
    }

    #[test]
    fn reads_a_bottom_up_bmp() {
        let (w, h) = (2usize, 2usize);
        let stride = 8;
        let mut b = vec![0u8; 54 + stride * h];
        b[0..2].copy_from_slice(b"BM");
        b[10..14].copy_from_slice(&54u32.to_le_bytes());
        b[18..22].copy_from_slice(&(w as u32).to_le_bytes());
        b[22..26].copy_from_slice(&(h as u32).to_le_bytes());
        b[28..30].copy_from_slice(&24u16.to_le_bytes());
        // Bottom row first: blue, green; then top: red, white.
        b[54..60].copy_from_slice(&[255, 0, 0, 0, 255, 0]);
        b[62..68].copy_from_slice(&[0, 0, 255, 255, 255, 255]);
        let img = decode(&b).unwrap();
        assert_eq!(img.rgba, vec![255, 0, 0, 255, 255, 255, 255, 255, 0, 0, 255, 255, 0, 255, 0, 255]);
    }
}
