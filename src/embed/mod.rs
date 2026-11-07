//! Owned sentence embeddings for semantic search: a packed BERT model
//! (written once by `cargo xtask pack-embedder`), its WordPiece tokenizer
//! ([`wordpiece`]), the encoder ([`bert`]) and the arithmetic under it
//! ([`gemm`]: AVX2, AVX-512 VNNI and NEON kernels over a scalar reference).
//!
//! No machine-learning crate and no model format parser: the packed file
//! is a fixed little-endian layout (see the packing script's docstring),
//! read with [`std::fs::read`].

pub mod bert;
pub mod gemm;
pub mod wordpiece;

use std::path::Path;

use crate::error::{Error, Result};
use bert::{Bert, Layer, Linear, Matrix, Norm, Pooling, Scratch};
use gemm::Isa;
use wordpiece::Tokenizer;

/// A loaded model.
#[derive(Debug)]
pub struct Model {
    /// The tokenizer.
    pub tok: Tokenizer,
    /// The encoder.
    pub bert: Bert,
    /// The longest token sequence encoded (longer text is cut).
    pub max_seq: usize,
    /// The vector width.
    pub dim: usize,
    /// Text put before a query (some models are trained with one).
    pub query_prefix: String,
    /// True for int8 weights.
    pub int8: bool,
    /// Names the weights: the source file's hash and the precision. Cached
    /// vectors are keyed by it.
    pub id: String,
    /// The kernels used.
    pub isa: Isa,
}

struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(|| Error::InvalidArgument("model file is truncated".into()))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap_or([0; 4])))
    }
    fn usize(&mut self) -> Result<usize> {
        Ok(self.u32()? as usize)
    }
    fn f32s(&mut self, n: usize) -> Result<Vec<f32>> {
        let bytes = n.checked_mul(4).ok_or_else(|| Error::InvalidArgument("model file sizes overflow".into()))?;
        Ok(self.take(bytes)?.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
    }
    fn text16(&mut self) -> Result<String> {
        let n = u16::from_le_bytes(self.take(2)?.try_into().unwrap_or([0; 2])) as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| Error::InvalidArgument("model file text is not UTF-8".into()))
    }
    fn matrix(&mut self, rows: usize, cols: usize, int8: bool) -> Result<Matrix> {
        rows.checked_mul(cols).and_then(|n| n.checked_mul(4)).ok_or_else(|| Error::InvalidArgument("model file sizes overflow".into()))?;
        if !int8 {
            return Ok(Matrix::F32 { w: self.f32s(rows * cols)?, cols });
        }
        let scale = self.f32s(rows)?;
        let w: Vec<i8> = self.take(rows * cols)?.iter().map(|&b| b as i8).collect();
        self.take((4 - (rows * cols) % 4) % 4)?;
        let sum = gemm::row_sums(&w, cols);
        Ok(Matrix::I8 { w, scale, sum, cols })
    }
    fn linear(&mut self, out: usize, inp: usize, int8: bool) -> Result<Linear> {
        let w = self.matrix(out, inp, int8)?;
        Ok(Linear { w, b: self.f32s(out)? })
    }
    fn norm(&mut self, n: usize) -> Result<Norm> {
        Ok(Norm { g: self.f32s(n)?, b: self.f32s(n)? })
    }
    /// A capacity for `count` entries of at least `min` bytes each: never
    /// more than the rest of the file can hold, so a corrupt count is a
    /// truncation error and not a giant allocation.
    fn cap(&self, count: usize, min: usize) -> usize {
        count.min((self.b.len() - self.at) / min.max(1))
    }
}

impl Model {
    /// Loads a packed model file.
    ///
    /// # Arguments
    ///
    /// * `path` - the `.atm` file.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the file is missing, [`Error::InvalidArgument`]
    /// when it is not a packed model or is truncated.
    pub fn load(path: &Path) -> Result<Model> {
        let b = std::fs::read(path).map_err(|e| Error::NotFound(format!("model {}: {e}", path.display())))?;
        Model::parse(&b, Isa::detect())
    }

    /// Parses packed model bytes.
    ///
    /// # Arguments
    ///
    /// * `b` - the file's bytes.
    /// * `isa` - the kernels to run with.
    ///
    /// # Errors
    ///
    /// As [`Model::load`].
    pub fn parse(b: &[u8], isa: Isa) -> Result<Model> {
        let mut c = Cursor { b, at: 0 };
        if c.take(8)? != b"ATLASEMB" || c.u32()? != 1 {
            return Err(Error::InvalidArgument("not an atlas embedding model (version 1)".into()));
        }
        let (hidden, layers, heads, inter, vocab, max_pos, type_vocab, max_seq) =
            (c.usize()?, c.usize()?, c.usize()?, c.usize()?, c.usize()?, c.usize()?, c.usize()?, c.usize()?);
        let pooling = if c.u32()? == 1 { Pooling::Cls } else { Pooling::Mean };
        let int8 = c.u32()? == 1;
        let lowercase = c.u32()? == 1;
        let eps = f32::from_le_bytes(c.take(4)?.try_into().unwrap_or([0; 4]));
        let sha = c.take(32)?;
        let n = c.usize()?;
        let query_prefix = String::from_utf8(c.take(n)?.to_vec()).map_err(|_| Error::InvalidArgument("query prefix is not UTF-8".into()))?;
        if hidden == 0 || heads == 0 || hidden % heads != 0 || max_seq < 3 || max_seq > max_pos || type_vocab == 0 || !(eps.is_finite() && eps > 0.0) {
            return Err(Error::InvalidArgument("model shape is inconsistent".into()));
        }
        let count = c.usize()?;
        let mut words = Vec::with_capacity(c.cap(count, 2));
        for _ in 0..count {
            words.push(c.text16()?);
        }
        let count = c.usize()?;
        let mut map = Vec::with_capacity(c.cap(count, 6));
        for _ in 0..count {
            let cp = c.u32()?;
            map.push((cp, c.text16()?));
        }
        let count = c.usize()?;
        let mut classes = Vec::with_capacity(c.cap(count, 12));
        for _ in 0..count {
            classes.push((c.u32()?, c.u32()?, c.u32()?));
        }
        if words.len() > vocab {
            return Err(Error::InvalidArgument(format!("the vocabulary has {} words for {vocab} embedding rows", words.len())));
        }
        let tok = Tokenizer::new(words, map, classes, lowercase).ok_or_else(|| Error::InvalidArgument("vocabulary lacks [CLS], [SEP] or [UNK]".into()))?;
        let word = c.matrix(vocab, hidden, int8)?;
        let mul = |a: usize, b: usize| a.checked_mul(b).ok_or_else(|| Error::InvalidArgument("model file sizes overflow".into()));
        let pos = c.f32s(mul(max_pos, hidden)?)?;
        let typ_all = c.f32s(mul(type_vocab, hidden)?)?;
        let emb_norm = c.norm(hidden)?;
        let mut ls = Vec::with_capacity(c.cap(layers, 64));
        for _ in 0..layers {
            let q = c.linear(hidden, hidden, int8)?;
            let k = c.linear(hidden, hidden, int8)?;
            let v = c.linear(hidden, hidden, int8)?;
            let o = c.linear(hidden, hidden, int8)?;
            let n1 = c.norm(hidden)?;
            let up = c.linear(inter, hidden, int8)?;
            let down = c.linear(hidden, inter, int8)?;
            let n2 = c.norm(hidden)?;
            ls.push(Layer { q, k, v, o, n1, up, down, n2 });
        }
        if c.at != b.len() {
            return Err(Error::InvalidArgument(format!("model file has {} bytes past its tensors", b.len() - c.at)));
        }
        let id = format!("{}-{}", crate::sha256::hex(sha).get(..16).unwrap_or(""), if int8 { "int8" } else { "f32" });
        let bert = Bert { hidden, heads, inter, eps, word, pos, typ: typ_all[..hidden].to_vec(), emb_norm, layers: ls, pooling };
        Ok(Model { tok, bert, max_seq, dim: hidden, query_prefix, int8, id, isa })
    }

    /// Embeds a passage (a section): unit length, `dim` floats.
    ///
    /// # Arguments
    ///
    /// * `text` - the text; past `max_seq` tokens it is cut.
    /// * `s` - scratch buffers, reused across calls on one thread.
    pub fn embed(&self, text: &str, s: &mut Scratch) -> Vec<f32> {
        let ids = self.tok.encode(text, self.max_seq);
        let mut out = vec![0f32; self.dim];
        self.bert.encode(self.isa, &ids, s, &mut out);
        out
    }

    /// Embeds a query (with the model's query prefix, if any).
    pub fn embed_query(&self, q: &str, s: &mut Scratch) -> Vec<f32> {
        if self.query_prefix.is_empty() {
            self.embed(q, s)
        } else {
            self.embed(&format!("{}{q}", self.query_prefix), s)
        }
    }

    /// Embeds a query cut to its first `max_tokens` tokens (special tokens
    /// and the query prefix included; never past `max_seq`). The encoder's
    /// cost grows with the token count, so a hook embeds the head of a long
    /// prompt rather than all of it.
    pub fn embed_query_cut(&self, q: &str, max_tokens: usize, s: &mut Scratch) -> Vec<f32> {
        let text = if self.query_prefix.is_empty() { q.to_string() } else { format!("{}{q}", self.query_prefix) };
        let ids = self.tok.encode(&text, max_tokens.clamp(3, self.max_seq));
        let mut out = vec![0f32; self.dim];
        self.bert.encode(self.isa, &ids, s, &mut out);
        out
    }

    /// Embeds many passages on `threads` threads (scoped; each with its
    /// own scratch), in input order.
    pub fn embed_many(&self, texts: &[String], threads: usize) -> Vec<Vec<f32>> {
        let threads = threads.clamp(1, texts.len().max(1));
        let next = std::sync::atomic::AtomicUsize::new(0);
        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        let slots: Vec<std::sync::Mutex<&mut Vec<f32>>> = out.iter_mut().map(std::sync::Mutex::new).collect();
        std::thread::scope(|sc| {
            for _ in 0..threads {
                sc.spawn(|| {
                    let mut s = Scratch::default();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= texts.len() {
                            break;
                        }
                        let v = self.embed(&texts[i], &mut s);
                        if let Ok(mut slot) = slots[i].lock() {
                            **slot = v;
                        }
                    }
                });
            }
        });
        drop(slots);
        out
    }

    /// Bytes the weights hold in memory.
    pub fn weight_bytes(&self) -> usize {
        let b = &self.bert;
        let mut n = b.word.bytes() + (b.pos.len() + b.typ.len()) * 4;
        for l in &b.layers {
            for lin in [&l.q, &l.k, &l.v, &l.o, &l.up, &l.down] {
                n += lin.w.bytes() + lin.b.len() * 4;
            }
        }
        n
    }
}

/// Builds a tiny random model in the packed format (2 layers, width 16,
/// a 40-token vocabulary), for tests: no real weights ship with the crate.
///
/// # Arguments
///
/// * `int8` - pack the matrices as int8.
/// * `seed` - the generator seed.
pub fn tiny_model_bytes(int8: bool, seed: u32) -> Vec<u8> {
    let (hidden, layers, heads, inter, max_pos, type_vocab, max_seq) = (16usize, 2usize, 4usize, 32usize, 64usize, 2usize, 32usize);
    let vocab: Vec<String> = ["[PAD]", "[UNK]", "[CLS]", "[SEP]", "the", "cat", "sat", "on", "mat", "dog", "ran", "##s", ",", ".", "fast", "slow"]
        .iter()
        .map(|s| s.to_string())
        .chain((0..24).map(|i| format!("w{i}")))
        .collect();
    let mut s = seed;
    let mut rnd = move || {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((s >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.4
    };
    let mut b: Vec<u8> = b"ATLASEMB".to_vec();
    for x in [1, hidden, layers, heads, inter, vocab.len(), max_pos, type_vocab, max_seq, 0, usize::from(int8), 1] {
        b.extend_from_slice(&(x as u32).to_le_bytes());
    }
    b.extend_from_slice(&1e-12f32.to_le_bytes());
    b.extend_from_slice(&[7u8; 32]);
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&(vocab.len() as u32).to_le_bytes());
    for t in &vocab {
        b.extend_from_slice(&(t.len() as u16).to_le_bytes());
        b.extend_from_slice(t.as_bytes());
    }
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    let mat = |b: &mut Vec<u8>, rows: usize, cols: usize, rnd: &mut dyn FnMut() -> f32| {
        let w: Vec<f32> = (0..rows * cols).map(|_| rnd()).collect();
        if int8 {
            let mut q = vec![0i8; rows * cols];
            for r in 0..rows {
                let sc = gemm::quantize_row(&w[r * cols..(r + 1) * cols], &mut q[r * cols..(r + 1) * cols]);
                b.extend_from_slice(&sc.to_le_bytes());
            }
            b.extend(q.iter().map(|&x| x as u8));
            b.extend(std::iter::repeat_n(0u8, (4 - (rows * cols) % 4) % 4));
        } else {
            w.iter().for_each(|v| b.extend_from_slice(&v.to_le_bytes()));
        }
    };
    let vec = |b: &mut Vec<u8>, n: usize, base: f32, rnd: &mut dyn FnMut() -> f32| {
        for _ in 0..n {
            b.extend_from_slice(&(base + rnd()).to_le_bytes());
        }
    };
    mat(&mut b, vocab.len(), hidden, &mut rnd);
    vec(&mut b, max_pos * hidden, 0.0, &mut rnd);
    vec(&mut b, type_vocab * hidden, 0.0, &mut rnd);
    vec(&mut b, hidden, 1.0, &mut rnd);
    vec(&mut b, hidden, 0.0, &mut rnd);
    for _ in 0..layers {
        for _ in 0..4 {
            mat(&mut b, hidden, hidden, &mut rnd);
            vec(&mut b, hidden, 0.0, &mut rnd);
        }
        vec(&mut b, hidden, 1.0, &mut rnd);
        vec(&mut b, hidden, 0.0, &mut rnd);
        mat(&mut b, inter, hidden, &mut rnd);
        vec(&mut b, inter, 0.0, &mut rnd);
        mat(&mut b, hidden, inter, &mut rnd);
        vec(&mut b, hidden, 0.0, &mut rnd);
        vec(&mut b, hidden, 1.0, &mut rnd);
        vec(&mut b, hidden, 0.0, &mut rnd);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_model_loads_and_embeds_unit_vectors() {
        for int8 in [false, true] {
            let m = Model::parse(&tiny_model_bytes(int8, 9), Isa::detect()).unwrap();
            let mut s = Scratch::default();
            let a = m.embed("The cat sat on the mat.", &mut s);
            let b = m.embed("the cat sat on the mat", &mut s);
            assert_eq!(a.len(), 16);
            let n: f32 = a.iter().map(|v| v * v).sum();
            assert!((n - 1.0).abs() < 1e-5);
            assert_ne!(a, b, "punctuation is a token");
            assert_eq!(m.embed("THE CAT sat on the mat.", &mut s), a, "lowercased");
            assert!(m.id.ends_with(if int8 { "int8" } else { "f32" }));
        }
    }

    #[test]
    fn simd_and_scalar_encoders_agree_exactly() {
        let bytes = tiny_model_bytes(false, 3);
        let simd = Model::parse(&bytes, Isa::detect()).unwrap();
        let scalar = Model::parse(&bytes, Isa::Scalar).unwrap();
        let mut s = Scratch::default();
        for t in ["the dog ran fast", "w1 w2 w3 w4 w5 w6 w7 w8 w9 w10 w11 w12 w13 cats", ""] {
            let a = simd.embed(t, &mut s);
            let b = scalar.embed(t, &mut s);
            assert!(a.iter().zip(&b).all(|(x, y)| x.to_bits() == y.to_bits()), "{t:?}");
        }
        let many = simd.embed_many(&["the cat".to_string(), "the dog".to_string(), "slow".to_string()], 2);
        assert_eq!(many[1], simd.embed("the dog", &mut s));
    }

    #[test]
    fn corrupt_headers_are_refused_without_huge_allocations() {
        let b = tiny_model_bytes(false, 2);
        // Header words after the magic and version: hidden, layers, heads,
        // inter, vocab, max_pos, type_vocab, max_seq, pooling, int8,
        // lowercase, eps.
        let set = |word: usize, v: u32| {
            let mut c = b.clone();
            c[12 + 4 * word..16 + 4 * word].copy_from_slice(&v.to_le_bytes());
            c
        };
        for (word, v) in [(1, u32::MAX), (4, u32::MAX), (5, u32::MAX), (6, 0), (6, u32::MAX), (11, f32::NAN.to_bits()), (11, 0), (11, f32::INFINITY.to_bits()), (4, 1)] {
            assert!(Model::parse(&set(word, v), Isa::Scalar).is_err(), "word {word} = {v:#x} parsed");
        }
    }

    #[test]
    fn truncated_or_foreign_files_are_refused() {
        let b = tiny_model_bytes(true, 1);
        assert!(Model::parse(&b[..b.len() - 1], Isa::Scalar).is_err());
        assert!(Model::parse(b"NOTAMODEL", Isa::Scalar).is_err());
        let mut extra = b.clone();
        extra.push(0);
        assert!(Model::parse(&extra, Isa::Scalar).is_err());
    }
}
