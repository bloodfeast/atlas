//! Byte scans: find a byte, find a substring, find a line end. The hot
//! loops of extraction and of the regex prefilter.
//!
//! Each scan has a scalar version and, where the CPU has one, a vector
//! version: SSE2 on every x86_64, AVX2 when the CPU reports it at run time,
//! NEON on aarch64. The vector versions are tested to give the same answer
//! as the scalar ones on every input of a randomised table.
#![allow(unsafe_code)]

/// The first position of `b` in `hay`.
#[inline]
pub fn memchr(b: u8, hay: &[u8]) -> Option<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        if hay.len() >= 16 {
            if avx2() {
                // SAFETY: AVX2 was detected at run time just above.
                return unsafe { x86::memchr_avx2(b, hay) };
            }
            // SAFETY: SSE2 is part of the x86_64 baseline.
            return unsafe { x86::memchr_sse2(b, hay) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if hay.len() >= 16 {
            // SAFETY: NEON is part of the aarch64 baseline.
            return unsafe { arm::memchr_neon(b, hay) };
        }
    }
    memchr_scalar(b, hay)
}

/// The scalar [`memchr`], the reference the vector versions are tested
/// against.
#[inline]
pub fn memchr_scalar(b: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&x| x == b)
}

/// The first position of `a` or `b` in `hay`.
#[inline]
pub fn memchr2(a: u8, b: u8, hay: &[u8]) -> Option<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        if hay.len() >= 16 {
            if avx2() {
                // SAFETY: AVX2 was detected at run time just above.
                return unsafe { x86::memchr2_avx2(a, b, hay) };
            }
            // SAFETY: SSE2 is part of the x86_64 baseline.
            return unsafe { x86::memchr2_sse2(a, b, hay) };
        }
    }
    memchr2_swar(a, b, hay)
}

/// The scalar [`memchr2`], the reference the others are tested against.
#[inline]
pub fn memchr2_scalar(a: u8, b: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&x| x == a || x == b)
}

/// [`memchr2`] eight bytes at a time in a `u64` (SWAR), for CPUs without
/// a vector path: a byte equal to `a` becomes zero after `xor`, and
/// `(x - 0x01..) & !x & 0x80..` flags the lowest zero byte exactly.
#[inline]
pub fn memchr2_swar(a: u8, b: u8, hay: &[u8]) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let zero = |x: u64| x.wrapping_sub(LO) & !x & HI;
    let (va, vb) = (LO * a as u64, LO * b as u64);
    let mut i = 0;
    while i + 8 <= hay.len() {
        let w = u64::from_le_bytes(hay[i..i + 8].try_into().expect("8 bytes"));
        // The lowest flagged byte of each half is exact; take the lower.
        let m = zero(w ^ va).trailing_zeros().min(zero(w ^ vb).trailing_zeros());
        if m < 64 {
            return Some(i + m as usize / 8);
        }
        i += 8;
    }
    memchr2_scalar(a, b, &hay[i..]).map(|k| i + k)
}

/// How many bytes of `hay` equal `b`: the line count of a transcript's
/// history before its newest boundary, which is hundreds of megabytes on a
/// long session.
#[inline]
pub fn count(b: u8, hay: &[u8]) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        if hay.len() >= 32 {
            if avx2() {
                // SAFETY: AVX2 was detected at run time just above.
                return unsafe { x86::count_avx2(b, hay) };
            }
            // SAFETY: SSE2 is part of the x86_64 baseline.
            return unsafe { x86::count_sse2(b, hay) };
        }
    }
    count_scalar(b, hay)
}

/// The scalar [`count`], the reference the vector versions are tested
/// against.
#[inline]
pub fn count_scalar(b: u8, hay: &[u8]) -> usize {
    hay.iter().filter(|&&x| x == b).count()
}

/// The first byte of `hay` a JSON string scan must stop at: a quote, a
/// backslash or a control byte; `hay.len()` when there is none.
#[inline]
pub fn json_stop(hay: &[u8]) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        if hay.len() >= 16 {
            // SAFETY: SSE2 is part of the x86_64 baseline.
            return unsafe { x86::json_stop_sse2(hay) };
        }
    }
    json_stop_scalar(hay)
}

/// The scalar [`json_stop`].
#[inline]
pub fn json_stop_scalar(hay: &[u8]) -> usize {
    hay.iter().position(|&c| c == b'"' || c == b'\\' || c < 0x20).unwrap_or(hay.len())
}

/// The first position of `needle` in `hay`; an empty needle is found at 0.
#[inline]
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let Some((&first, rest)) = needle.split_first() else { return Some(0) };
    if needle.len() > hay.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    let mut at = 0;
    while at <= last {
        let i = at + memchr(first, &hay[at..=last])?;
        if hay[i + 1..i + needle.len()] == *rest {
            return Some(i);
        }
        at = i + 1;
    }
    None
}

#[cfg(target_arch = "x86_64")]
fn avx2() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static HAS: AtomicU8 = AtomicU8::new(0);
    match HAS.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let yes = std::arch::is_x86_feature_detected!("avx2");
            HAS.store(if yes { 2 } else { 1 }, Ordering::Relaxed);
            yes
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    /// # Safety
    ///
    /// The CPU must support SSE2 (every x86_64 does). Loads are unaligned
    /// and never read past `hay`: the tail shorter than 16 bytes is scanned
    /// by overlapping the last full block.
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn memchr_sse2(b: u8, hay: &[u8]) -> Option<usize> {
        let n = hay.len();
        let p = hay.as_ptr();
        let v = _mm_set1_epi8(b as i8);
        let mut i = 0;
        while i + 16 <= n {
            // SAFETY: i + 16 <= n, so the 16 bytes at p + i are in `hay`.
            let m = _mm_movemask_epi8(_mm_cmpeq_epi8(unsafe { _mm_loadu_si128(p.add(i).cast()) }, v));
            if m != 0 {
                return Some(i + m.trailing_zeros() as usize);
            }
            i += 16;
        }
        if i < n {
            let j = n - 16;
            // SAFETY: n >= 16 (the caller checks), so p + n - 16 starts a
            // full block inside `hay`.
            let m = _mm_movemask_epi8(_mm_cmpeq_epi8(unsafe { _mm_loadu_si128(p.add(j).cast()) }, v));
            // Bytes before i were already scanned.
            let m = (m as u32) >> (i - j) << (i - j);
            if m != 0 {
                return Some(j + m.trailing_zeros() as usize);
            }
        }
        None
    }

    /// # Safety
    ///
    /// SSE2 (the x86_64 baseline); `hay` at least 16 bytes. Loads never
    /// read past `hay`.
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn memchr2_sse2(a: u8, b: u8, hay: &[u8]) -> Option<usize> {
        let n = hay.len();
        let p = hay.as_ptr();
        let (va, vb) = (_mm_set1_epi8(a as i8), _mm_set1_epi8(b as i8));
        let mask = |x: __m128i| _mm_movemask_epi8(_mm_or_si128(_mm_cmpeq_epi8(x, va), _mm_cmpeq_epi8(x, vb))) as u32;
        let mut i = 0;
        while i + 16 <= n {
            // SAFETY: i + 16 <= n.
            let m = mask(unsafe { _mm_loadu_si128(p.add(i).cast()) });
            if m != 0 {
                return Some(i + m.trailing_zeros() as usize);
            }
            i += 16;
        }
        if i < n {
            let j = n - 16;
            // SAFETY: n >= 16, so the last full block is inside `hay`.
            let m = mask(unsafe { _mm_loadu_si128(p.add(j).cast()) }) >> (i - j) << (i - j);
            if m != 0 {
                return Some(j + m.trailing_zeros() as usize);
            }
        }
        None
    }

    /// # Safety
    ///
    /// AVX2 (checked at run time by the caller); `hay` at least 16 bytes.
    /// Loads never read past `hay`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn memchr2_avx2(a: u8, b: u8, hay: &[u8]) -> Option<usize> {
        let n = hay.len();
        let p = hay.as_ptr();
        let (va, vb) = (_mm256_set1_epi8(a as i8), _mm256_set1_epi8(b as i8));
        let mut i = 0;
        while i + 32 <= n {
            // SAFETY: i + 32 <= n.
            let x = unsafe { _mm256_loadu_si256(p.add(i).cast()) };
            let m = _mm256_movemask_epi8(_mm256_or_si256(_mm256_cmpeq_epi8(x, va), _mm256_cmpeq_epi8(x, vb))) as u32;
            if m != 0 {
                return Some(i + m.trailing_zeros() as usize);
            }
            i += 32;
        }
        if i < n {
            let rest = &hay[i..];
            // SAFETY: AVX2 implies SSE2.
            let r = if rest.len() >= 16 { unsafe { memchr2_sse2(a, b, rest) } } else { super::memchr2_scalar(a, b, rest) };
            return r.map(|k| i + k);
        }
        None
    }

    /// # Safety
    ///
    /// SSE2 (the x86_64 baseline). Loads never read past `hay`. A match
    /// compares to all ones (-1), so subtracting it adds 1 to a byte lane;
    /// the lanes are summed (`psadbw`) before any can pass 255.
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn count_sse2(b: u8, hay: &[u8]) -> usize {
        let n = hay.len();
        let p = hay.as_ptr();
        let v = _mm_set1_epi8(b as i8);
        let zero = _mm_setzero_si128();
        let mut total = 0usize;
        let mut i = 0;
        while i + 16 <= n {
            let mut acc = _mm_setzero_si128();
            let mut k = 0;
            while k < 255 && i + 16 <= n {
                // SAFETY: i + 16 <= n, so the 16 bytes at p + i are in `hay`.
                acc = _mm_sub_epi8(acc, _mm_cmpeq_epi8(unsafe { _mm_loadu_si128(p.add(i).cast()) }, v));
                i += 16;
                k += 1;
            }
            let s = _mm_sad_epu8(acc, zero);
            total += (_mm_cvtsi128_si64(s) + _mm_cvtsi128_si64(_mm_unpackhi_epi64(s, s))) as usize;
        }
        total + super::count_scalar(b, &hay[i..])
    }

    /// # Safety
    ///
    /// AVX2 (checked at run time by the caller). Loads never read past
    /// `hay`. The [`count_sse2`] method on 32-byte lanes.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn count_avx2(b: u8, hay: &[u8]) -> usize {
        let n = hay.len();
        let p = hay.as_ptr();
        let v = _mm256_set1_epi8(b as i8);
        let zero = _mm256_setzero_si256();
        let mut total = 0usize;
        let mut i = 0;
        while i + 32 <= n {
            let mut acc = _mm256_setzero_si256();
            let mut k = 0;
            while k < 255 && i + 32 <= n {
                // SAFETY: i + 32 <= n, so the 32 bytes at p + i are in `hay`.
                acc = _mm256_sub_epi8(acc, _mm256_cmpeq_epi8(unsafe { _mm256_loadu_si256(p.add(i).cast()) }, v));
                i += 32;
                k += 1;
            }
            let s = _mm256_sad_epu8(acc, zero);
            total += (_mm256_extract_epi64::<0>(s) + _mm256_extract_epi64::<1>(s) + _mm256_extract_epi64::<2>(s) + _mm256_extract_epi64::<3>(s)) as usize;
        }
        total + super::count_scalar(b, &hay[i..])
    }

    /// # Safety
    ///
    /// SSE2 (the x86_64 baseline); `hay` at least 16 bytes. Loads never
    /// read past `hay`.
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn json_stop_sse2(hay: &[u8]) -> usize {
        let n = hay.len();
        let p = hay.as_ptr();
        let quote = _mm_set1_epi8(b'"' as i8);
        let slash = _mm_set1_epi8(b'\\' as i8);
        // Unsigned c < 0x20 as a signed compare after flipping the top bit.
        let flip = _mm_set1_epi8(i8::MIN);
        let limit = _mm_set1_epi8((0x20u8 ^ 0x80) as i8);
        let mask = |x: __m128i| -> u32 {
            let q = _mm_or_si128(_mm_cmpeq_epi8(x, quote), _mm_cmpeq_epi8(x, slash));
            let c = _mm_cmplt_epi8(_mm_xor_si128(x, flip), limit);
            _mm_movemask_epi8(_mm_or_si128(q, c)) as u32
        };
        let mut i = 0;
        while i + 16 <= n {
            // SAFETY: i + 16 <= n.
            let m = mask(unsafe { _mm_loadu_si128(p.add(i).cast()) });
            if m != 0 {
                return i + m.trailing_zeros() as usize;
            }
            i += 16;
        }
        if i < n {
            let j = n - 16;
            // SAFETY: n >= 16, so the last full block is inside `hay`.
            let m = mask(unsafe { _mm_loadu_si128(p.add(j).cast()) }) >> (i - j) << (i - j);
            if m != 0 {
                return j + m.trailing_zeros() as usize;
            }
        }
        n
    }

    /// # Safety
    ///
    /// The CPU must support AVX2 (checked at run time by the caller);
    /// `hay` must be at least 16 bytes. Loads never read past `hay`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn memchr_avx2(b: u8, hay: &[u8]) -> Option<usize> {
        let n = hay.len();
        let p = hay.as_ptr();
        let v = _mm256_set1_epi8(b as i8);
        let mut i = 0;
        while i + 32 <= n {
            // SAFETY: i + 32 <= n, so the 32 bytes at p + i are in `hay`.
            let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(unsafe { _mm256_loadu_si256(p.add(i).cast()) }, v));
            if m != 0 {
                return Some(i + m.trailing_zeros() as usize);
            }
            i += 32;
        }
        if i < n {
            // The tail: the SSE2 scan of the rest, which is at least 1 and
            // under 32 bytes; it handles fewer than 16 by itself.
            let rest = &hay[i..];
            // SAFETY: AVX2 implies SSE2.
            let r = if rest.len() >= 16 { unsafe { memchr_sse2(b, rest) } } else { super::memchr_scalar(b, rest) };
            return r.map(|k| i + k);
        }
        None
    }
}

#[cfg(target_arch = "aarch64")]
mod arm {
    use std::arch::aarch64::*;

    /// # Safety
    ///
    /// NEON is part of the aarch64 baseline; `hay` must be at least 16
    /// bytes. Loads never read past `hay`.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn memchr_neon(b: u8, hay: &[u8]) -> Option<usize> {
        let n = hay.len();
        let p = hay.as_ptr();
        let v = vdupq_n_u8(b);
        let mut i = 0;
        while i + 16 <= n {
            // SAFETY: i + 16 <= n, so the 16 bytes at p + i are in `hay`.
            let eq = vceqq_u8(unsafe { vld1q_u8(p.add(i)) }, v);
            if vmaxvq_u8(eq) != 0 {
                return hay[i..i + 16].iter().position(|&x| x == b).map(|k| i + k);
            }
            i += 16;
        }
        super::memchr_scalar(b, &hay[i..]).map(|k| i + k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator, so the table is the same each run.
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn vector_scans_agree_with_scalar() {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for len in 0..300 {
            for _ in 0..8 {
                let alphabet = 1 + (xorshift(&mut s) % 6) as u8;
                let hay: Vec<u8> = (0..len).map(|_| b'a' + (xorshift(&mut s) % alphabet as u64) as u8).collect();
                for b in [b'a', b'b', b'c', b'f', b'z', 0] {
                    assert_eq!(memchr(b, &hay), memchr_scalar(b, &hay), "len {len} byte {b}");
                    for c in [b'a', b'c', b'e', 0xff] {
                        let want = memchr2_scalar(b, c, &hay);
                        assert_eq!(memchr2(b, c, &hay), want, "len {len} bytes {b} {c}");
                        assert_eq!(memchr2_swar(b, c, &hay), want, "swar len {len} bytes {b} {c}");
                    }
                }
                let nl = 1 + (xorshift(&mut s) % 3) as usize;
                let needle: Vec<u8> = (0..nl).map(|_| b'a' + (xorshift(&mut s) % alphabet as u64) as u8).collect();
                let want = hay.windows(needle.len()).position(|w| w == needle.as_slice());
                assert_eq!(find(&hay, &needle), want, "{hay:?} {needle:?}");
            }
        }
        // Any byte values, so SWAR borrows across bytes are exercised.
        for len in 0..200 {
            let hay: Vec<u8> = (0..len).map(|_| xorshift(&mut s) as u8 % 5 * 63).collect();
            for (a, b) in [(0, 63), (126, 252), (189, 1), (0, 0)] {
                assert_eq!(memchr2(a, b, &hay), memchr2_scalar(a, b, &hay), "{hay:?}");
                assert_eq!(memchr2_swar(a, b, &hay), memchr2_scalar(a, b, &hay), "{hay:?}");
            }
        }
        assert_eq!(find(b"abc", b""), Some(0));
        let mut s = 7u64;
        for len in 0..200 {
            let hay: Vec<u8> = (0..len)
                .map(|_| match xorshift(&mut s) % 40 {
                    0 => b'"',
                    1 => b'\\',
                    2 => (xorshift(&mut s) % 0x20) as u8,
                    3 => 0x80 | (xorshift(&mut s) % 0x80) as u8,
                    _ => b'a',
                })
                .collect();
            assert_eq!(json_stop(&hay), json_stop_scalar(&hay), "{hay:?}");
        }
        assert_eq!(find(b"ab", b"abc"), None);
        // Counts, across the 255-block flush of the vector paths and with
        // any byte values (0xff is the compare's own all-ones).
        for len in (0..300).chain([255 * 16 - 1, 255 * 16, 255 * 32 + 17, 70_000]) {
            let hay: Vec<u8> = (0..len).map(|_| xorshift(&mut s) as u8 % 4 * 85).collect();
            for b in [0, 85, 255, b'\n'] {
                assert_eq!(count(b, &hay), count_scalar(b, &hay), "len {len} byte {b}");
            }
            let all = vec![b'\n'; len];
            assert_eq!(count(b'\n', &all), len);
        }
    }
}
