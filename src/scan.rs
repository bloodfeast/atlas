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
                }
                let nl = 1 + (xorshift(&mut s) % 3) as usize;
                let needle: Vec<u8> = (0..nl).map(|_| b'a' + (xorshift(&mut s) % alphabet as u64) as u8).collect();
                let want = hay.windows(needle.len()).position(|w| w == needle.as_slice());
                assert_eq!(find(&hay, &needle), want, "{hay:?} {needle:?}");
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
    }
}
