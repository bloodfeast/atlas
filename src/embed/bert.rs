//! The BERT encoder: embeddings and LayerNorm, then per layer
//! multi-head self-attention and a GELU feed-forward block, each with a
//! residual and LayerNorm (post-norm, as BERT); then mean or `[CLS]`
//! pooling and L2 normalisation, as sentence-transformers does.
//!
//! One sequence at a time, unpadded, so attention needs no mask. Weights are
//! f32, or int8 with one f32 scale per output row (matrices only; biases,
//! LayerNorm and position embeddings stay f32), from the packed file read by
//! [`super::Model::load`].

use super::gemm::{self, Isa};

/// A weight matrix, `rows` x `cols`, in PyTorch's `[out, in]` layout.
#[derive(Debug, Clone)]
pub enum Matrix {
    /// f32 weights.
    F32 {
        /// Row-major weights.
        w: Vec<f32>,
        /// Columns (the input width).
        cols: usize,
    },
    /// int8 weights with a scale per row.
    I8 {
        /// Row-major weights.
        w: Vec<i8>,
        /// One scale per row.
        scale: Vec<f32>,
        /// The sum of each row's int8 weights (for the VNNI kernel).
        sum: Vec<i32>,
        /// Columns.
        cols: usize,
    },
}

impl Matrix {
    /// Row `r` as f32 (an embedding lookup).
    pub fn row(&self, r: usize, out: &mut [f32]) {
        match self {
            Matrix::F32 { w, cols } => out.copy_from_slice(&w[r * cols..(r + 1) * cols]),
            Matrix::I8 { w, scale, cols, .. } => {
                for (d, &q) in out.iter_mut().zip(&w[r * cols..(r + 1) * cols]) {
                    *d = f32::from(q) * scale[r];
                }
            }
        }
    }

    /// `y = x W^T + b` for `x` of `n` rows.
    fn apply(&self, isa: Isa, x: &[f32], b: &[f32], y: &mut [f32], scratch: &mut Vec<i8>) {
        match self {
            Matrix::F32 { w, cols } => gemm::linear(isa, x, *cols, w, b, y),
            Matrix::I8 { w, scale, sum, cols } => gemm::linear_i8(isa, x, *cols, w, scale, sum, b, y, scratch),
        }
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        match self {
            Matrix::F32 { w, .. } => w.len() * 4,
            Matrix::I8 { w, scale, sum, .. } => w.len() + scale.len() * 4 + sum.len() * 4,
        }
    }
}

/// A linear layer.
#[derive(Debug, Clone)]
pub struct Linear {
    /// Weights.
    pub w: Matrix,
    /// Biases.
    pub b: Vec<f32>,
}

/// LayerNorm parameters.
#[derive(Debug, Clone)]
pub struct Norm {
    /// Gain.
    pub g: Vec<f32>,
    /// Bias.
    pub b: Vec<f32>,
}

/// One encoder layer.
#[derive(Debug, Clone)]
pub struct Layer {
    /// Query projection.
    pub q: Linear,
    /// Key projection.
    pub k: Linear,
    /// Value projection.
    pub v: Linear,
    /// Attention output projection.
    pub o: Linear,
    /// LayerNorm after attention.
    pub n1: Norm,
    /// Feed-forward up projection.
    pub up: Linear,
    /// Feed-forward down projection.
    pub down: Linear,
    /// LayerNorm after the feed-forward block.
    pub n2: Norm,
}

/// Pooling of token vectors into one sentence vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Mean over every token (special tokens included, as the attention
    /// mask covers them).
    Mean,
    /// The `[CLS]` token's vector.
    Cls,
}

/// The encoder's weights and shape.
#[derive(Debug, Clone)]
pub struct Bert {
    /// Hidden width.
    pub hidden: usize,
    /// Attention heads.
    pub heads: usize,
    /// Feed-forward width.
    pub inter: usize,
    /// LayerNorm epsilon.
    pub eps: f32,
    /// Word embeddings.
    pub word: Matrix,
    /// Position embeddings, `max_pos` rows of `hidden`.
    pub pos: Vec<f32>,
    /// Token type 0's embedding.
    pub typ: Vec<f32>,
    /// Embedding LayerNorm.
    pub emb_norm: Norm,
    /// The layers.
    pub layers: Vec<Layer>,
    /// Pooling.
    pub pooling: Pooling,
}

/// Reusable buffers for one forward pass.
#[derive(Debug, Default)]
pub struct Scratch {
    x: Vec<f32>,
    q: Vec<f32>,
    k: Vec<f32>,
    v: Vec<f32>,
    vt: Vec<f32>,
    ctx: Vec<f32>,
    tmp: Vec<f32>,
    ff: Vec<f32>,
    p: Vec<f32>,
    qi8: Vec<i8>,
}

fn layer_norm(x: &mut [f32], width: usize, n: &Norm, eps: f32) {
    for row in x.chunks_exact_mut(width) {
        let mean = row.iter().sum::<f32>() / width as f32;
        let var = row.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / width as f32;
        let inv = 1.0 / (var + eps).sqrt();
        for (j, v) in row.iter_mut().enumerate() {
            *v = (*v - mean) * inv * n.g[j] + n.b[j];
        }
    }
}

/// The error function, to within 1.5e-7 (Abramowitz and Stegun 7.1.26),
/// computed in f64: well below f32 resolution of the activations it feeds.
fn erf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let y = 1.0 - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t + 0.254_829_592) * t * (-x * x).exp();
    if x >= 0.0 {
        y
    } else {
        -y
    }
}

/// BERT's exact GELU, `x * (1 + erf(x / sqrt 2)) / 2`.
fn gelu(x: f32) -> f32 {
    let x = f64::from(x);
    (0.5 * x * (1.0 + erf(x / std::f64::consts::SQRT_2))) as f32
}

impl Bert {
    /// Encodes one token sequence into a unit-length sentence vector.
    ///
    /// # Arguments
    ///
    /// * `isa` - the kernels.
    /// * `ids` - token ids (`[CLS] ... [SEP]`), at most the position table.
    /// * `s` - scratch buffers.
    /// * `out` - `hidden` floats, overwritten.
    pub fn encode(&self, isa: Isa, ids: &[u32], s: &mut Scratch, out: &mut [f32]) {
        let (h, n) = (self.hidden, ids.len());
        let dh = h / self.heads;
        s.x.resize(n * h, 0.0);
        for (t, &id) in ids.iter().enumerate() {
            let row = &mut s.x[t * h..(t + 1) * h];
            self.word.row(id as usize, row);
            for j in 0..h {
                row[j] += self.pos[t * h + j] + self.typ[j];
            }
        }
        layer_norm(&mut s.x, h, &self.emb_norm, self.eps);
        let scale = 1.0 / (dh as f32).sqrt();
        for l in &self.layers {
            for (lin, buf) in [(&l.q, &mut s.q), (&l.k, &mut s.k), (&l.v, &mut s.v)] {
                buf.resize(n * h, 0.0);
                lin.w.apply(isa, &s.x, &lin.b, buf, &mut s.qi8);
            }
            s.ctx.resize(n * h, 0.0);
            s.p.resize(n, 0.0);
            s.vt.resize(n * dh, 0.0);
            for hd in 0..self.heads {
                let off = hd * dh;
                // This head's values, transposed: one contiguous row per
                // dimension, so each context value is one dot product.
                for j in 0..n {
                    for d in 0..dh {
                        s.vt[d * n + j] = s.v[j * h + off + d];
                    }
                }
                for i in 0..n {
                    let qh = &s.q[i * h + off..i * h + off + dh];
                    let mut mx = f32::NEG_INFINITY;
                    for j in 0..n {
                        let sc = gemm::dot(isa, qh, &s.k[j * h + off..j * h + off + dh]) * scale;
                        s.p[j] = sc;
                        mx = mx.max(sc);
                    }
                    let mut sum = 0f32;
                    for p in s.p.iter_mut() {
                        *p = (*p - mx).exp();
                        sum += *p;
                    }
                    let inv = 1.0 / sum;
                    for p in s.p.iter_mut() {
                        *p *= inv;
                    }
                    for d in 0..dh {
                        s.ctx[i * h + off + d] = gemm::dot(isa, &s.p, &s.vt[d * n..(d + 1) * n]);
                    }
                }
            }
            s.tmp.resize(n * h, 0.0);
            l.o.w.apply(isa, &s.ctx, &l.o.b, &mut s.tmp, &mut s.qi8);
            for (x, a) in s.x.iter_mut().zip(&s.tmp) {
                *x += a;
            }
            layer_norm(&mut s.x, h, &l.n1, self.eps);
            s.ff.resize(n * self.inter, 0.0);
            l.up.w.apply(isa, &s.x, &l.up.b, &mut s.ff, &mut s.qi8);
            for v in s.ff.iter_mut() {
                *v = gelu(*v);
            }
            l.down.w.apply(isa, &s.ff, &l.down.b, &mut s.tmp, &mut s.qi8);
            for (x, a) in s.x.iter_mut().zip(&s.tmp) {
                *x += a;
            }
            layer_norm(&mut s.x, h, &l.n2, self.eps);
        }
        match self.pooling {
            Pooling::Cls => out.copy_from_slice(&s.x[..h]),
            Pooling::Mean => {
                out.iter_mut().for_each(|v| *v = 0.0);
                for row in s.x.chunks_exact(h) {
                    for (o, v) in out.iter_mut().zip(row) {
                        *o += v;
                    }
                }
                let inv = 1.0 / n as f32;
                out.iter_mut().for_each(|v| *v *= inv);
            }
        }
        let norm = out.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
        out.iter_mut().for_each(|v| *v /= norm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gelu_matches_known_values() {
        // erf-based GELU at a few points (values from the closed form).
        for (x, want) in [(0.0f32, 0.0f32), (1.0, 0.841_344_7), (-1.0, -0.158_655_3), (2.5, 2.484_475_8)] {
            assert!((gelu(x) - want).abs() < 5e-6, "gelu({x}) = {}", gelu(x));
        }
    }
}
