//! The encoder's arithmetic: f32 and int8 dot products and the linear
//! layer `y = x W^T + b`, with AVX2+FMA (x86-64, detected at run time) and
//! NEON (aarch64) kernels over a scalar reference, and an AVX-512 VNNI
//! int8 kernel where the CPU has it.
//!
//! Every kernel computes an f32 dot product in the same order: eight lane
//! accumulators, lane `l` taking elements `l, l + 8, l + 16, ...` with a
//! fused multiply-add, then the fixed reduction `((a0+a4) + (a2+a6)) +
//! ((a1+a5) + (a3+a7))`. The scalar reference emulates that order with
//! `f32::mul_add`, so the SIMD and scalar results are bit-identical (tested),
//! not merely close. Integer dot products are exact in any order.

/// Which kernels run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isa {
    /// Portable Rust.
    Scalar,
    /// AVX2 with FMA.
    Avx2,
    /// AVX2 with FMA for f32, and AVX-512 VNNI (with F and BW) for the int8
    /// linear layer.
    Avx512Vnni,
    /// NEON (always present on aarch64).
    Neon,
}

impl Isa {
    /// The best kernels this CPU runs.
    pub fn detect() -> Isa {
        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma") {
                if std::arch::is_x86_feature_detected!("avx512f")
                    && std::arch::is_x86_feature_detected!("avx512bw")
                    && std::arch::is_x86_feature_detected!("avx512vnni")
                {
                    return Isa::Avx512Vnni;
                }
                return Isa::Avx2;
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            return Isa::Neon;
        }
        #[allow(unreachable_code)]
        Isa::Scalar
    }

    /// True when the AVX2 and FMA kernels may run (`Avx2` and
    /// `Avx512Vnni` are only produced after checking both).
    #[inline]
    pub fn avx2(self) -> bool {
        matches!(self, Isa::Avx2 | Isa::Avx512Vnni)
    }
}

#[inline]
fn reduce8(a: [f32; 8]) -> f32 {
    ((a[0] + a[4]) + (a[2] + a[6])) + ((a[1] + a[5]) + (a[3] + a[7]))
}

/// The scalar reference dot product (see the module docs for its order).
pub fn dot_scalar(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut acc = [0f32; 8];
    for i in 0..n {
        acc[i & 7] = a[i].mul_add(b[i], acc[i & 7]);
    }
    reduce8(acc)
}

/// The scalar reference int8 dot product.
pub fn dot_i8_scalar(a: &[i8], b: &[i8]) -> i32 {
    a.iter().zip(b).map(|(&x, &y)| i32::from(x) * i32::from(y)).sum()
}

#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod avx2 {
    use std::arch::x86_64::*;

    #[inline]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn reduce(v: __m256) -> f32 {
        // lanes l + (l + 4), then (0 + 2) and (1 + 3), then their sum: the
        // order of `reduce8`.
        let s = _mm_add_ps(_mm256_castps256_ps128(v), _mm256_extractf128_ps(v, 1));
        let t = _mm_add_ps(s, _mm_movehl_ps(s, s));
        let r = _mm_add_ss(t, _mm_shuffle_ps(t, t, 1));
        _mm_cvtss_f32(r)
    }

    /// A tail of fewer than 8 elements, folded into the lanes as the scalar
    /// reference does.
    #[inline]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn tail(acc: __m256, a: &[f32], b: &[f32]) -> __m256 {
        let mut x = [0f32; 8];
        let mut y = [0f32; 8];
        x[..a.len()].copy_from_slice(a);
        y[..b.len()].copy_from_slice(b);
        // Zero lanes add 0 * 0 to an accumulator: fma(0, 0, acc) == acc
        // exactly, so the padding changes nothing.
        _mm256_fmadd_ps(_mm256_loadu_ps(x.as_ptr()), _mm256_loadu_ps(y.as_ptr()), acc)
    }

    /// # Safety
    ///
    /// The CPU must support AVX2 and FMA; `a` and `b` have equal length.
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let full = n & !7;
        let mut acc = _mm256_setzero_ps();
        let mut i = 0;
        while i < full {
            // SAFETY: i + 8 <= full <= n for both slices.
            acc = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i)), _mm256_loadu_ps(b.as_ptr().add(i)), acc);
            i += 8;
        }
        if full < n {
            acc = tail(acc, &a[full..], &b[full..]);
        }
        reduce(acc)
    }

    /// Four dot products of one `x` row with four `w` rows (each `k` long),
    /// written to `out[0..4]`.
    ///
    /// # Safety
    ///
    /// AVX2 and FMA; `x` has `k` elements and `w` has `4 * k`.
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot4(x: &[f32], w: &[f32], k: usize, out: &mut [f32; 4]) {
        let full = k & !7;
        let (w0, w1, w2, w3) = (w.as_ptr(), w.as_ptr().add(k), w.as_ptr().add(2 * k), w.as_ptr().add(3 * k));
        let (mut a0, mut a1, mut a2, mut a3) = (_mm256_setzero_ps(), _mm256_setzero_ps(), _mm256_setzero_ps(), _mm256_setzero_ps());
        let mut i = 0;
        while i < full {
            // SAFETY: i + 8 <= k for x and each of the four w rows.
            let xv = _mm256_loadu_ps(x.as_ptr().add(i));
            a0 = _mm256_fmadd_ps(xv, _mm256_loadu_ps(w0.add(i)), a0);
            a1 = _mm256_fmadd_ps(xv, _mm256_loadu_ps(w1.add(i)), a1);
            a2 = _mm256_fmadd_ps(xv, _mm256_loadu_ps(w2.add(i)), a2);
            a3 = _mm256_fmadd_ps(xv, _mm256_loadu_ps(w3.add(i)), a3);
            i += 8;
        }
        if full < k {
            let xt = &x[full..k];
            a0 = tail(a0, xt, &w[full..k]);
            a1 = tail(a1, xt, &w[k + full..2 * k]);
            a2 = tail(a2, xt, &w[2 * k + full..3 * k]);
            a3 = tail(a3, xt, &w[3 * k + full..4 * k]);
        }
        *out = [reduce(a0), reduce(a1), reduce(a2), reduce(a3)];
    }

    /// Eight dot products: two `x` rows against four `w` rows (each `k`
    /// long), so every weight load serves two rows. `out[r][j]` is row `r`
    /// against weight row `j`, each in the eight-lane order.
    ///
    /// # Safety
    ///
    /// AVX2 and FMA; `x` has `2 * k` elements and `w` has `4 * k`.
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot4x2(x: &[f32], w: &[f32], k: usize, out: &mut [[f32; 4]; 2]) {
        let full = k & !7;
        let (x0, x1) = (x.as_ptr(), x.as_ptr().add(k));
        let (w0, w1, w2, w3) = (w.as_ptr(), w.as_ptr().add(k), w.as_ptr().add(2 * k), w.as_ptr().add(3 * k));
        let z = _mm256_setzero_ps();
        let (mut a0, mut a1, mut a2, mut a3, mut b0, mut b1, mut b2, mut b3) = (z, z, z, z, z, z, z, z);
        let mut i = 0;
        while i < full {
            // SAFETY: i + 8 <= k for both x rows and the four w rows.
            let (u, v) = (_mm256_loadu_ps(x0.add(i)), _mm256_loadu_ps(x1.add(i)));
            let p = _mm256_loadu_ps(w0.add(i));
            a0 = _mm256_fmadd_ps(u, p, a0);
            b0 = _mm256_fmadd_ps(v, p, b0);
            let p = _mm256_loadu_ps(w1.add(i));
            a1 = _mm256_fmadd_ps(u, p, a1);
            b1 = _mm256_fmadd_ps(v, p, b1);
            let p = _mm256_loadu_ps(w2.add(i));
            a2 = _mm256_fmadd_ps(u, p, a2);
            b2 = _mm256_fmadd_ps(v, p, b2);
            let p = _mm256_loadu_ps(w3.add(i));
            a3 = _mm256_fmadd_ps(u, p, a3);
            b3 = _mm256_fmadd_ps(v, p, b3);
            i += 8;
        }
        if full < k {
            let (xa, xb) = (&x[full..k], &x[k + full..2 * k]);
            let wt = |j: usize| &w[j * k + full..(j + 1) * k];
            a0 = tail(a0, xa, wt(0));
            a1 = tail(a1, xa, wt(1));
            a2 = tail(a2, xa, wt(2));
            a3 = tail(a3, xa, wt(3));
            b0 = tail(b0, xb, wt(0));
            b1 = tail(b1, xb, wt(1));
            b2 = tail(b2, xb, wt(2));
            b3 = tail(b3, xb, wt(3));
        }
        *out = [[reduce(a0), reduce(a1), reduce(a2), reduce(a3)], [reduce(b0), reduce(b1), reduce(b2), reduce(b3)]];
    }

    /// # Safety
    ///
    /// AVX2; `a` and `b` have equal length.
    #[target_feature(enable = "avx2")]
    pub unsafe fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
        let n = a.len();
        let full = n & !15;
        let mut acc = _mm256_setzero_si256();
        let mut i = 0;
        while i < full {
            // SAFETY: i + 16 <= n for both slices; 16 bytes are loaded.
            let x = _mm256_cvtepi8_epi16(_mm_loadu_si128(a.as_ptr().add(i) as *const __m128i));
            let y = _mm256_cvtepi8_epi16(_mm_loadu_si128(b.as_ptr().add(i) as *const __m128i));
            acc = _mm256_add_epi32(acc, _mm256_madd_epi16(x, y));
            i += 16;
        }
        let mut lanes = [0i32; 8];
        _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, acc);
        let mut s: i32 = lanes.iter().sum();
        for j in full..n {
            s += i32::from(a[j]) * i32::from(b[j]);
        }
        s
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn hsum_i32(v: __m256i) -> i32 {
        let mut lanes = [0i32; 8];
        _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, v);
        lanes.iter().sum()
    }

    /// Eight int8 dot products: two `x` rows against four `w` rows (each
    /// `k` long), so each widened weight load serves two rows and each
    /// widened row load four weights. Exact (integer), so equal to eight
    /// [`dot_i8`] calls.
    ///
    /// # Safety
    ///
    /// AVX2; `x` has `2 * k` elements and `w` has `4 * k`.
    #[target_feature(enable = "avx2")]
    pub unsafe fn dot_i8_2x4(x: &[i8], w: &[i8], k: usize, out: &mut [[i32; 4]; 2]) {
        let full = k & !15;
        let (x0, x1) = (x.as_ptr(), x.as_ptr().add(k));
        let wp = [w.as_ptr(), w.as_ptr().add(k), w.as_ptr().add(2 * k), w.as_ptr().add(3 * k)];
        let z = _mm256_setzero_si256();
        let mut acc = [[z; 4]; 2];
        let mut i = 0;
        while i < full {
            // SAFETY: i + 16 <= k for both x rows and the four w rows.
            let u = _mm256_cvtepi8_epi16(_mm_loadu_si128(x0.add(i) as *const __m128i));
            let v = _mm256_cvtepi8_epi16(_mm_loadu_si128(x1.add(i) as *const __m128i));
            for j in 0..4 {
                let p = _mm256_cvtepi8_epi16(_mm_loadu_si128(wp[j].add(i) as *const __m128i));
                acc[0][j] = _mm256_add_epi32(acc[0][j], _mm256_madd_epi16(u, p));
                acc[1][j] = _mm256_add_epi32(acc[1][j], _mm256_madd_epi16(v, p));
            }
            i += 16;
        }
        for r in 0..2 {
            for j in 0..4 {
                let mut s = hsum_i32(acc[r][j]);
                for t in full..k {
                    s += i32::from(x[r * k + t]) * i32::from(w[j * k + t]);
                }
                out[r][j] = s;
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod vnni {
    use std::arch::x86_64::*;

    /// `R` int8 rows of `x` against four int8 weight rows, each `k` long,
    /// with `vpdpbusd`: 64 bytes per step, four products summed into each
    /// 32-bit lane.
    ///
    /// `vpdpbusd` multiplies unsigned bytes by signed bytes, and both sides
    /// here are signed. Each `x` byte is flipped to `x + 128` (an exclusive
    /// or with 0x80), which is in `0..=255`, so the kernel computes
    /// `dot(x + 128, w) = dot(x, w) + 128 * sum(w)` and subtracts
    /// `128 * wsum[j]`. Each product fits 16 bits and the four-product sums
    /// are added into 32-bit lanes without saturation, so the result is the
    /// exact integer dot product: equal to [`super::dot_i8_scalar`].
    ///
    /// The tail (`k % 64` bytes) is read with a zeroing masked load: a
    /// masked-off weight byte is 0, so its product is 0 whatever the
    /// flipped `x` byte is.
    ///
    /// # Safety
    ///
    /// The CPU must support AVX-512 F, BW and VNNI; `x` has `R * k`
    /// elements, `w` has `4 * k` and `wsum` four (the sums of the four
    /// weight rows). Overflow cannot occur for `k` below 2^31 / (255 * 128).
    #[inline]
    #[target_feature(enable = "avx512f,avx512bw,avx512vnni")]
    pub unsafe fn dot_rx4<const R: usize>(x: &[i8], w: &[i8], k: usize, wsum: &[i32], out: &mut [[i32; 4]; R]) {
        let full = k & !63;
        let xp = x.as_ptr();
        let wp = [w.as_ptr(), w.as_ptr().add(k), w.as_ptr().add(2 * k), w.as_ptr().add(3 * k)];
        let flip = _mm512_set1_epi8(i8::MIN);
        let z = _mm512_setzero_si512();
        let mut acc = [[z; 4]; R];
        let mut i = 0;
        while i < full {
            // SAFETY: i + 64 <= k for every x row and the four w rows.
            let wv = [
                _mm512_loadu_si512(wp[0].add(i) as *const __m512i),
                _mm512_loadu_si512(wp[1].add(i) as *const __m512i),
                _mm512_loadu_si512(wp[2].add(i) as *const __m512i),
                _mm512_loadu_si512(wp[3].add(i) as *const __m512i),
            ];
            for (r, ar) in acc.iter_mut().enumerate() {
                let xv = _mm512_xor_si512(_mm512_loadu_si512(xp.add(r * k + i) as *const __m512i), flip);
                for (a, &p) in ar.iter_mut().zip(&wv) {
                    *a = _mm512_dpbusd_epi32(*a, xv, p);
                }
            }
            i += 64;
        }
        if full < k {
            // 1..=63 bytes remain.
            let m: __mmask64 = (1u64 << (k - full)) - 1;
            // SAFETY: the masked loads touch only bytes full..k of each row.
            let wv = [
                _mm512_maskz_loadu_epi8(m, wp[0].add(full)),
                _mm512_maskz_loadu_epi8(m, wp[1].add(full)),
                _mm512_maskz_loadu_epi8(m, wp[2].add(full)),
                _mm512_maskz_loadu_epi8(m, wp[3].add(full)),
            ];
            for (r, ar) in acc.iter_mut().enumerate() {
                let xv = _mm512_xor_si512(_mm512_maskz_loadu_epi8(m, xp.add(r * k + full)), flip);
                for (a, &p) in ar.iter_mut().zip(&wv) {
                    *a = _mm512_dpbusd_epi32(*a, xv, p);
                }
            }
        }
        for (o, ar) in out.iter_mut().zip(&acc) {
            for j in 0..4 {
                o[j] = _mm512_reduce_add_epi32(ar[j]).wrapping_sub(wsum[j].wrapping_mul(128));
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[allow(unsafe_code)]
mod neon {
    use std::arch::aarch64::*;

    /// # Safety
    ///
    /// `a` and `b` have equal length (NEON is always present on aarch64).
    pub unsafe fn dot(a: &[f32], b: &[f32]) -> f32 {
        // Lanes 0..4 and 4..8 of the eight-lane order in two registers.
        let n = a.len();
        let full = n & !7;
        let (mut lo, mut hi) = (vdupq_n_f32(0.0), vdupq_n_f32(0.0));
        let mut i = 0;
        while i < full {
            // SAFETY: i + 8 <= n for both slices.
            lo = vfmaq_f32(lo, vld1q_f32(a.as_ptr().add(i)), vld1q_f32(b.as_ptr().add(i)));
            hi = vfmaq_f32(hi, vld1q_f32(a.as_ptr().add(i + 4)), vld1q_f32(b.as_ptr().add(i + 4)));
            i += 8;
        }
        let mut acc = [0f32; 8];
        vst1q_f32(acc.as_mut_ptr(), lo);
        vst1q_f32(acc.as_mut_ptr().add(4), hi);
        for j in full..n {
            acc[j & 7] = a[j].mul_add(b[j], acc[j & 7]);
        }
        super::reduce8(acc)
    }

    /// # Safety
    ///
    /// `a` and `b` have equal length.
    pub unsafe fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
        let n = a.len();
        let full = n & !15;
        let mut acc = vdupq_n_s32(0);
        let mut i = 0;
        while i < full {
            // SAFETY: i + 16 <= n for both slices.
            let x = vld1q_s8(a.as_ptr().add(i));
            let y = vld1q_s8(b.as_ptr().add(i));
            let lo = vmull_s8(vget_low_s8(x), vget_low_s8(y));
            let hi = vmull_high_s8(x, y);
            acc = vpadalq_s16(acc, lo);
            acc = vpadalq_s16(acc, hi);
            i += 16;
        }
        let mut s = vaddvq_s32(acc);
        for j in full..n {
            s += i32::from(a[j]) * i32::from(b[j]);
        }
        s
    }
}

/// An f32 dot product with the given kernels.
#[inline]
#[allow(unsafe_code)]
pub fn dot(isa: Isa, a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `Isa::Avx2` and `Isa::Avx512Vnni` are only produced by
        // `detect` after checking AVX2 and FMA (or by tests that checked the
        // same).
        Isa::Avx2 | Isa::Avx512Vnni => unsafe { avx2::dot(a, b) },
        #[cfg(target_arch = "aarch64")]
        // SAFETY: NEON is part of the aarch64 baseline.
        Isa::Neon => unsafe { neon::dot(a, b) },
        _ => dot_scalar(a, b),
    }
}

/// An int8 dot product with the given kernels (exact).
#[inline]
#[allow(unsafe_code)]
pub fn dot_i8(isa: Isa, a: &[i8], b: &[i8]) -> i32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as in `dot`.
        Isa::Avx2 | Isa::Avx512Vnni => unsafe { avx2::dot_i8(a, b) },
        #[cfg(target_arch = "aarch64")]
        // SAFETY: as in `dot`.
        Isa::Neon => unsafe { neon::dot_i8(a, b) },
        _ => dot_i8_scalar(a, b),
    }
}

/// `y[n][o] = dot(x[n], w[o]) + b[o]` for an f32 weight matrix
/// (`w` is `out` rows of `k`, PyTorch's `Linear` layout).
///
/// # Arguments
///
/// * `isa` - the kernels.
/// * `x` - `n` rows of `k`.
/// * `k` - the input width.
/// * `w` - `out` rows of `k`.
/// * `b` - `out` biases.
/// * `y` - `n` rows of `out`, overwritten.
#[allow(unsafe_code)]
pub fn linear(isa: Isa, x: &[f32], k: usize, w: &[f32], b: &[f32], y: &mut [f32]) {
    let out = b.len();
    let n = x.len() / k;
    debug_assert_eq!(w.len(), out * k);
    debug_assert_eq!(y.len(), n * out);
    #[allow(unused_mut)]
    let mut i = 0;
    #[cfg(target_arch = "x86_64")]
    if isa.avx2() {
        // Two rows at a time, four weight rows per step.
        let mut q = [[0f32; 4]; 2];
        while i + 2 <= n {
            let xr = &x[i * k..(i + 2) * k];
            let mut o = 0;
            while o + 4 <= out {
                // SAFETY: AVX2+FMA checked by `Isa::detect`; `xr` holds two
                // rows of `k` and `w[o * k..]` at least four.
                unsafe { avx2::dot4x2(xr, &w[o * k..(o + 4) * k], k, &mut q) };
                for r in 0..2 {
                    for j in 0..4 {
                        y[(i + r) * out + o + j] = q[r][j] + b[o + j];
                    }
                }
                o += 4;
            }
            while o < out {
                for r in 0..2 {
                    y[(i + r) * out + o] = dot(isa, &xr[r * k..(r + 1) * k], &w[o * k..(o + 1) * k]) + b[o];
                }
                o += 1;
            }
            i += 2;
        }
    }
    for i in i..n {
        let xr = &x[i * k..(i + 1) * k];
        let yr = &mut y[i * out..(i + 1) * out];
        let mut o = 0;
        #[cfg(target_arch = "x86_64")]
        if isa.avx2() {
            let mut q = [0f32; 4];
            while o + 4 <= out {
                // SAFETY: AVX2+FMA checked by `Isa::detect`; `w[o * k..]`
                // holds at least four rows of `k`.
                unsafe { avx2::dot4(xr, &w[o * k..(o + 4) * k], k, &mut q) };
                for j in 0..4 {
                    yr[o + j] = q[j] + b[o + j];
                }
                o += 4;
            }
        }
        while o < out {
            yr[o] = dot(isa, xr, &w[o * k..(o + 1) * k]) + b[o];
            o += 1;
        }
    }
}

/// Quantises one row to int8 with a symmetric scale: `x ~ q * scale`.
///
/// # Returns
///
/// The scale (1 for an all-zero row).
pub fn quantize_row(x: &[f32], q: &mut [i8]) -> f32 {
    let m = x.iter().fold(0f32, |m, v| m.max(v.abs()));
    let s = if m > 0.0 { m / 127.0 } else { 1.0 };
    let inv = 1.0 / s;
    for (d, v) in q.iter_mut().zip(x) {
        *d = (v * inv).round().clamp(-127.0, 127.0) as i8;
    }
    s
}

/// The int8 linear layer: each `x` row quantised on the fly, integer dot
/// products with the int8 weight rows, then rescaled:
/// `y[n][o] = dot_i8(xq[n], wq[o]) * xs[n] * ws[o] + b[o]`.
///
/// # Arguments
///
/// * `isa` - the kernels.
/// * `x` - `n` rows of `k`.
/// * `k` - the input width.
/// * `wq` - `out` rows of `k` int8 weights.
/// * `ws` - `out` weight scales.
/// * `wsum` - `out` sums of the int8 weight rows ([`row_sums`]), used by
///   the VNNI kernel.
/// * `b` - `out` biases.
/// * `y` - `n` rows of `out`, overwritten.
/// * `scratch` - reused buffer for the quantised rows.
///
/// The rows are quantised first, in blocks of [`I8_BLOCK`], and each weight
/// row is then applied to every row of the block while it is in L1: walking
/// the weights once per input row read the whole matrix `n` times (a
/// 32-token query streamed ~21 MB of bge weights 32 times). Each output is
/// the same dot product as before, so the result is bit-identical.
///
/// With [`Isa::Avx512Vnni`] each step is four input rows against four
/// weight rows with `vpdpbusd`; the integer dot products are exact, so the
/// output is bit-identical to the AVX2 and scalar paths (tested).
#[allow(clippy::too_many_arguments)]
#[allow(unsafe_code)]
pub fn linear_i8(isa: Isa, x: &[f32], k: usize, wq: &[i8], ws: &[f32], wsum: &[i32], b: &[f32], y: &mut [f32], scratch: &mut Vec<i8>) {
    let out = b.len();
    let n = x.len() / k;
    assert!(wq.len() == out * k && ws.len() == out && wsum.len() == out && y.len() == n * out, "linear_i8: mismatched shapes");
    let mut xs = [0f32; I8_BLOCK];
    let mut start = 0;
    while start < n {
        let rows = (n - start).min(I8_BLOCK);
        scratch.resize(rows * k, 0);
        for r in 0..rows {
            let i = start + r;
            xs[r] = quantize_row(&x[i * k..(i + 1) * k], &mut scratch[r * k..(r + 1) * k]);
        }
        // Only the x86-64 AVX-512 block below advances `o`; elsewhere it stays 0.
        #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
        let mut o = 0;
        #[cfg(target_arch = "x86_64")]
        if isa == Isa::Avx512Vnni {
            // Four weight rows against four input rows per step.
            let mut q4 = [[0i32; 4]; 4];
            let mut q1 = [[0i32; 4]; 1];
            while o + 4 <= out {
                let (wr, s4) = (&wq[o * k..(o + 4) * k], &wsum[o..o + 4]);
                let mut r = 0;
                while r < rows {
                    let take = if r + 4 <= rows { 4 } else { 1 };
                    let xr = &scratch[r * k..(r + take) * k];
                    // SAFETY: AVX-512 F, BW and VNNI were detected; `xr` is
                    // `take * k`, `wr` is `4 * k` and `s4` four long.
                    let q: &[[i32; 4]] = unsafe {
                        if take == 4 {
                            vnni::dot_rx4::<4>(xr, wr, k, s4, &mut q4);
                            &q4
                        } else {
                            vnni::dot_rx4::<1>(xr, wr, k, s4, &mut q1);
                            &q1
                        }
                    };
                    for (dr, qr) in q.iter().enumerate() {
                        for (dj, &d) in qr.iter().enumerate() {
                            y[(start + r + dr) * out + o + dj] = (d as f32) * (xs[r + dr] * ws[o + dj]) + b[o + dj];
                        }
                    }
                    r += take;
                }
                o += 4;
            }
        } else if isa.avx2() {
            // Four weight rows against two input rows per step.
            let mut q = [[0i32; 4]; 2];
            while o + 4 <= out {
                let wr = &wq[o * k..(o + 4) * k];
                let mut r = 0;
                while r + 2 <= rows {
                    // SAFETY: AVX2 was detected; the slices are 2 * k and 4 * k long.
                    unsafe { avx2::dot_i8_2x4(&scratch[r * k..(r + 2) * k], wr, k, &mut q) };
                    for (dr, qr) in q.iter().enumerate() {
                        for (dj, &d) in qr.iter().enumerate() {
                            y[(start + r + dr) * out + o + dj] = (d as f32) * (xs[r + dr] * ws[o + dj]) + b[o + dj];
                        }
                    }
                    r += 2;
                }
                for r in r..rows {
                    for j in o..o + 4 {
                        let d = dot_i8(isa, &scratch[r * k..(r + 1) * k], &wq[j * k..(j + 1) * k]);
                        y[(start + r) * out + j] = (d as f32) * (xs[r] * ws[j]) + b[j];
                    }
                }
                o += 4;
            }
        }
        for o in o..out {
            let wr = &wq[o * k..(o + 1) * k];
            for r in 0..rows {
                let d = dot_i8(isa, &scratch[r * k..(r + 1) * k], wr);
                y[(start + r) * out + o] = (d as f32) * (xs[r] * ws[o]) + b[o];
            }
        }
        start += rows;
    }
}

/// Input rows quantised and multiplied per weight-row pass in [`linear_i8`].
pub const I8_BLOCK: usize = 64;

/// The sum of each int8 weight row (`out` rows of `k`), which the VNNI
/// kernel subtracts (times 128) to undo its unsigned offset.
pub fn row_sums(wq: &[i8], k: usize) -> Vec<i32> {
    if k == 0 {
        return Vec::new();
    }
    wq.chunks_exact(k).map(|r| r.iter().map(|&v| i32::from(v)).sum()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((s >> 8) as f32 / (1u32 << 24) as f32) - 0.5
            })
            .collect()
    }

    #[test]
    fn simd_dot_is_bit_identical_to_scalar() {
        let isa = Isa::detect();
        for n in [0, 1, 7, 8, 9, 31, 32, 33, 384, 1536, 1537] {
            let (a, b) = (data(n, 1), data(n, 2));
            assert_eq!(dot(isa, &a, &b).to_bits(), dot_scalar(&a, &b).to_bits(), "n={n} isa={isa:?}");
            let qa: Vec<i8> = a.iter().map(|v| (v * 250.0) as i8).collect();
            let qb: Vec<i8> = b.iter().map(|v| (v * 250.0) as i8).collect();
            assert_eq!(dot_i8(isa, &qa, &qb), dot_i8_scalar(&qa, &qb), "n={n}");
        }
    }

    #[test]
    fn simd_linear_is_bit_identical_to_scalar() {
        let isa = Isa::detect();
        for (n, k, out) in [(3, 384, 384), (2, 37, 13), (1, 1536, 5)] {
            let (x, w, b) = (data(n * k, 3), data(out * k, 4), data(out, 5));
            let (mut y1, mut y2) = (vec![0f32; n * out], vec![0f32; n * out]);
            linear(isa, &x, k, &w, &b, &mut y1);
            linear(Isa::Scalar, &x, k, &w, &b, &mut y2);
            assert!(y1.iter().zip(&y2).all(|(p, q)| p.to_bits() == q.to_bits()), "{n}x{k}x{out}");
            let mut wq = vec![0i8; out * k];
            let ws: Vec<f32> = (0..out).map(|o| quantize_row(&w[o * k..(o + 1) * k], &mut wq[o * k..(o + 1) * k])).collect();
            let mut s = Vec::new();
            linear_i8(isa, &x, k, &wq, &ws, &row_sums(&wq, k), &b, &mut y1, &mut s);
            linear_i8(Isa::Scalar, &x, k, &wq, &ws, &row_sums(&wq, k), &b, &mut y2, &mut s);
            assert_eq!(y1, y2);
        }
    }

    /// The blocked int8 linear equals the row-at-a-time order it replaced,
    /// bit for bit, across a block boundary.
    #[test]
    fn blocked_i8_linear_equals_row_at_a_time() {
        let isa = Isa::detect();
        for (n, k, out) in [(1, 384, 7), (I8_BLOCK + 7, 48, 9), (2 * I8_BLOCK, 16, 3), (5, 37, 13), (32, 384, 384)] {
            let (x, w, b) = (data(n * k, 6), data(out * k, 7), data(out, 8));
            let mut wq = vec![0i8; out * k];
            let ws: Vec<f32> = (0..out).map(|o| quantize_row(&w[o * k..(o + 1) * k], &mut wq[o * k..(o + 1) * k])).collect();
            let mut y = vec![0f32; n * out];
            linear_i8(isa, &x, k, &wq, &ws, &row_sums(&wq, k), &b, &mut y, &mut Vec::new());
            let mut q = vec![0i8; k];
            for i in 0..n {
                let xs = quantize_row(&x[i * k..(i + 1) * k], &mut q);
                for o in 0..out {
                    let want = (dot_i8(isa, &q, &wq[o * k..(o + 1) * k]) as f32) * (xs * ws[o]) + b[o];
                    assert_eq!(y[i * out + o].to_bits(), want.to_bits(), "{n}x{k}x{out} at {i},{o}");
                }
            }
        }
    }

    /// The VNNI int8 linear equals the scalar reference and the AVX2 path
    /// bit for bit: full and masked 64-byte steps, four-row tiles and the
    /// single-row remainder, an output count that is not a multiple of four,
    /// and extreme bytes (every product at its largest magnitude).
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn vnni_linear_is_bit_identical() {
        if Isa::detect() != Isa::Avx512Vnni {
            eprintln!("vnni_linear_is_bit_identical: no AVX-512 VNNI on this CPU; the path is not exercised");
            return;
        }
        let shapes = [(1, 1, 4), (3, 63, 5), (4, 64, 4), (5, 65, 9), (7, 127, 8), (9, 384, 384), (I8_BLOCK + 3, 1536, 12), (2, 384, 1536), (32, 37, 13)];
        for (si, &(n, k, out)) in shapes.iter().enumerate() {
            let seed = si as u32 * 10;
            let (x, w, b) = (data(n * k, seed + 1), data(out * k, seed + 2), data(out, seed + 3));
            let mut wq = vec![0i8; out * k];
            let ws: Vec<f32> = (0..out).map(|o| quantize_row(&w[o * k..(o + 1) * k], &mut wq[o * k..(o + 1) * k])).collect();
            let sums = row_sums(&wq, k);
            let mut ys: Vec<Vec<f32>> = Vec::new();
            for isa in [Isa::Avx512Vnni, Isa::Avx2, Isa::Scalar] {
                let mut y = vec![0f32; n * out];
                linear_i8(isa, &x, k, &wq, &ws, &sums, &b, &mut y, &mut Vec::new());
                ys.push(y);
            }
            let bits = |v: &[f32]| v.iter().map(|f| f.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&ys[0]), bits(&ys[2]), "vnni vs scalar {n}x{k}x{out}");
            assert_eq!(bits(&ys[1]), bits(&ys[2]), "avx2 vs scalar {n}x{k}x{out}");
        }
        // Extremes: the kernel's integer dot products against the scalar
        // reference with every byte at -127 or 127 (and -128, which the
        // quantiser never makes but the kernel must still get right).
        for k in [1, 64, 100, 1536] {
            for (xv, wv) in [(-127i8, -127i8), (127, 127), (-127, 127), (-128, -128), (-128, 127)] {
                let x = vec![xv; 4 * k];
                let w = vec![wv; 4 * k];
                let sums = row_sums(&w, k);
                let mut q = [[0i32; 4]; 4];
                // SAFETY: VNNI detected above; the slices are 4 * k long.
                #[allow(unsafe_code)]
                unsafe {
                    vnni::dot_rx4::<4>(&x, &w, k, &sums, &mut q)
                };
                let want = dot_i8_scalar(&x[..k], &w[..k]);
                assert!(q.iter().flatten().all(|&d| d == want), "k={k} x={xv} w={wv}: {q:?} vs {want}");
            }
        }
    }

    #[test]
    fn quantised_linear_is_close_to_f32() {
        let (n, k, out) = (4, 384, 64);
        let (x, w, b) = (data(n * k, 6), data(out * k, 7), data(out, 8));
        let mut y = vec![0f32; n * out];
        linear(Isa::Scalar, &x, k, &w, &b, &mut y);
        let mut wq = vec![0i8; out * k];
        let ws: Vec<f32> = (0..out).map(|o| quantize_row(&w[o * k..(o + 1) * k], &mut wq[o * k..(o + 1) * k])).collect();
        let mut yq = vec![0f32; n * out];
        linear_i8(Isa::Scalar, &x, k, &wq, &ws, &row_sums(&wq, k), &b, &mut yq, &mut Vec::new());
        let err = y.iter().zip(&yq).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        assert!(err < 0.05, "max error {err}");
    }
}
