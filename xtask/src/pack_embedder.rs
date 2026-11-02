//! `cargo xtask pack-embedder MODEL_DIR OUT.atm [--int8] [--query-prefix
//! TEXT] [--ucd UCD_DIR]`: pack a BERT sentence-embedding model for atlas.
//! A one-time tool step; atlas itself never reads safetensors, JSON configs
//! or Unicode tables.
//!
//! Reads a Hugging Face model folder (model.safetensors, config.json,
//! vocab.txt, tokenizer_config.json, 1_Pooling/config.json,
//! sentence_bert_config.json) and writes atlas's packed little-endian model
//! file (format below). The packed file also carries the tokenizer's Unicode
//! tables, so the tokenizer in atlas needs none: per code point, the text
//! BERT's basic tokenizer maps it to (lowercase, then NFD with combining
//! marks dropped), and the ranges of whitespace, control (any C category, as
//! BERT's tokenizer has it) and punctuation characters.
//!
//! Those tables come from the Unicode Character Database files
//! `UnicodeData.txt` and `SpecialCasing.txt`: by default the 15.1.0 copies
//! committed in `xtask/ucd/15.1.0/` (Unicode License V3, beside them), which
//! the models atlas pins were packed against; `--ucd UCD_DIR` names another
//! version's (from `https://www.unicode.org/Public/<version>/ucd/`). The
//! version decides the tables, so the same model packed against the same
//! version is the same file, byte for byte.
//!
//! Format (all integers u32 LE, floats f32 LE):
//!   magic b"ATLASEMB", version 1
//!   hidden, layers, heads, intermediate, vocab, max_pos, type_vocab, max_seq,
//!   pooling (0 mean, 1 cls), quant (0 f32, 1 int8), lowercase (0/1)
//!   layer_norm_eps (f32)
//!   source sha256 (32 bytes), query prefix (u32 len + UTF-8)
//!   vocab: count, then per token u16 len + UTF-8
//!   map: count, then per entry u32 code point, u16 len + UTF-8 replacement
//!   classes: count, then per range u32 start, u32 end (inclusive), u32 flags
//!            (1 whitespace, 2 control, 4 punctuation)
//!   tensors in a fixed order (see `tensors`); a matrix [rows, cols] is f32
//!   rows, or with int8: rows f32 scales then rows*cols int8 (padded to 4
//!   bytes); vectors are always f32.

use crate::py;
use crate::ucd::Db;
#[cfg(test)]
use crate::ucd::Ucd;
use atlas::json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Usage.
pub const USAGE: &str = "usage: cargo xtask pack-embedder MODEL_DIR OUT.atm [--int8] [--query-prefix TEXT] [--ucd UCD_DIR]";

/// The committed Unicode Character Database files, the default for `--ucd`.
pub fn default_ucd() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("ucd").join("15.1.0")
}

/// Parsed arguments.
pub struct Args {
    /// The Hugging Face model folder.
    pub src: PathBuf,
    /// The packed file to write.
    pub out: String,
    /// The folder holding UnicodeData.txt and SpecialCasing.txt (default:
    /// [`default_ucd`]).
    pub ucd: PathBuf,
    /// Quantise the matrices to int8.
    pub int8: bool,
    /// The query prefix stored in the file.
    pub prefix: String,
}

impl Args {
    /// Parses the command's arguments.
    pub fn parse(args: Vec<String>) -> Result<Args, String> {
        let (mut pos, mut ucd, mut int8, mut prefix) = (Vec::new(), None, false, String::new());
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--int8" => int8 = true,
                "--query-prefix" => prefix = it.next().ok_or_else(|| format!("--query-prefix needs a value\n{USAGE}"))?,
                "--ucd" => ucd = Some(PathBuf::from(it.next().ok_or_else(|| format!("--ucd needs a value\n{USAGE}"))?)),
                s if s.starts_with("--") => return Err(format!("unknown option {s}\n{USAGE}")),
                _ => pos.push(a),
            }
        }
        let ucd = ucd.unwrap_or_else(default_ucd);
        match <[String; 2]>::try_from(pos) {
            Ok([src, out]) => Ok(Args { src: src.into(), out, ucd, int8, prefix }),
            Err(_) => Err(USAGE.into()),
        }
    }
}

/// The normalisation map and the character class ranges, from 0x80 up.
fn unicode_tables(db: &Db, lowercase: bool) -> (Vec<(u32, String)>, Vec<(u32, u32, u32)>) {
    let mut mapping = Vec::new();
    let mut flags = Vec::with_capacity(0x110000 - 0x80);
    for cp in 0x80u32..0x110000 {
        if (0xD800..=0xDFFF).contains(&cp) {
            flags.push(0);
            continue;
        }
        let cat = db.get(cp).cat;
        let mut f = 0;
        if &cat == b"Zs" {
            f |= 1;
        } else if cat[0] == b'C' {
            f |= 2;
        }
        if cat[0] == b'P' {
            f |= 4;
        }
        flags.push(f);
        if lowercase {
            let m: Vec<u32> = db.nfd(&db.lower(cp)).into_iter().filter(|&x| &db.get(x).cat != b"Mn").collect();
            if m != [cp] {
                let s: String = m.iter().map(|&x| char::from_u32(x).expect("decompositions are scalar values")).collect();
                mapping.push((cp, s));
            }
        }
    }
    let mut ranges = Vec::new();
    let mut run: Option<(u32, u32)> = None;
    for (i, &f) in flags.iter().enumerate() {
        let cp = 0x80 + i as u32;
        if let Some((start, cur)) = run {
            if f != cur {
                if cur != 0 {
                    ranges.push((start, cp - 1, cur));
                }
                run = None;
            }
        }
        if run.is_none() {
            run = Some((cp, f));
        }
    }
    if let Some((start, cur)) = run {
        if cur != 0 {
            ranges.push((start, 0x10FFFF, cur));
        }
    }
    (mapping, ranges)
}

/// The tensors in file order, `true` for a matrix (quantised with --int8).
fn tensors(layers: usize) -> Vec<(String, bool)> {
    let mut t: Vec<(String, bool)> = [
        ("embeddings.word_embeddings.weight", true),
        ("embeddings.position_embeddings.weight", false),
        ("embeddings.token_type_embeddings.weight", false),
        ("embeddings.LayerNorm.weight", false),
        ("embeddings.LayerNorm.bias", false),
    ]
    .iter()
    .map(|(n, m)| (n.to_string(), *m))
    .collect();
    for i in 0..layers {
        let p = format!("encoder.layer.{i}.");
        for lin in ["attention.self.query", "attention.self.key", "attention.self.value", "attention.output.dense"] {
            t.push((format!("{p}{lin}.weight"), true));
            t.push((format!("{p}{lin}.bias"), false));
        }
        for (n, m) in [
            ("attention.output.LayerNorm.weight", false),
            ("attention.output.LayerNorm.bias", false),
            ("intermediate.dense.weight", true),
            ("intermediate.dense.bias", false),
            ("output.dense.weight", true),
            ("output.dense.bias", false),
            ("output.LayerNorm.weight", false),
            ("output.LayerNorm.bias", false),
        ] {
            t.push((format!("{p}{n}"), m));
        }
    }
    t
}

/// An F32 tensor from the safetensors file: its shape and raw LE bytes.
struct Tensor<'a> {
    shape: Vec<usize>,
    bytes: &'a [u8],
}

/// The F32 tensors (integer buffers such as position_ids are not weights)
/// and the file's SHA-256.
fn read_safetensors(data: &[u8]) -> Result<HashMap<String, Tensor<'_>>, String> {
    let bad = |m: &str| format!("model.safetensors: {m}");
    let n = u64::from_le_bytes(data.get(..8).ok_or_else(|| bad("too short"))?.try_into().expect("8 bytes")) as usize;
    let header =
        atlas::json::parse_bytes(data.get(8..8 + n).ok_or_else(|| bad("header past the end"))?).map_err(|e| bad(&e.to_string()))?;
    let base = 8 + n;
    let mut out = HashMap::new();
    for (name, meta) in header.as_object().ok_or_else(|| bad("the header is not an object"))?.iter() {
        if name == "__metadata__" || meta.get("dtype").and_then(Value::as_str) != Some("F32") {
            continue;
        }
        let offs: Vec<usize> = meta
            .get("data_offsets")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_u64).map(|x| x as usize).collect())
            .unwrap_or_default();
        let shape: Vec<usize> = meta
            .get("shape")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_u64).map(|x| x as usize).collect())
            .unwrap_or_default();
        let [a, b] = offs[..] else { return Err(bad(&format!("{name}: bad data_offsets"))) };
        let bytes = data.get(base + a..base + b).ok_or_else(|| bad(&format!("{name}: data past the end")))?;
        if bytes.len() != shape.iter().product::<usize>() * 4 {
            return Err(bad(&format!("{name}: {} bytes do not hold shape {shape:?}", bytes.len())));
        }
        out.insert(name.clone(), Tensor { shape, bytes });
    }
    Ok(out)
}

fn read_json(p: &Path) -> Result<Value, String> {
    let t = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    atlas::json::parse(&t).map_err(|e| format!("{}: {e}", p.display()))
}

fn u32_of(v: &Value, k: &str) -> Result<u32, String> {
    v.get(k).and_then(Value::as_u64).and_then(|x| u32::try_from(x).ok()).ok_or_else(|| format!("{k} is not a u32 integer"))
}

fn text(b: &mut Vec<u8>, s: &str) -> Result<(), String> {
    let n = u16::try_from(s.len()).map_err(|_| format!("{s:?} is longer than 65535 bytes"))?;
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Packs the model and returns the summary line.
pub fn run(a: &Args) -> Result<String, String> {
    let cfg = read_json(&a.src.join("config.json"))?;
    let pool = read_json(&a.src.join("1_Pooling").join("config.json"))?;
    let st = read_json(&a.src.join("sentence_bert_config.json"))?;
    let tok = read_json(&a.src.join("tokenizer_config.json"))?;
    let lowercase = match tok.get("do_lower_case") {
        None => true,
        v => py::truthy(v),
    };
    // Python: `strip_accents not in (None,) and strip_accents != lowercase`.
    let same = |v: &Value| match v {
        Value::Bool(b) => *b == lowercase,
        Value::Number(n) => n.as_f64() == Some(if lowercase { 1.0 } else { 0.0 }),
        _ => false,
    };
    if let Some(sa) = tok.get("strip_accents").filter(|v| !v.is_null()) {
        if !same(sa) {
            return Err("strip_accents differing from do_lower_case is not supported".into());
        }
    }
    let pooling = u32::from(py::truthy(pool.get("pooling_mode_cls_token")));
    if pooling == 0 && !py::truthy(pool.get("pooling_mode_mean_tokens")) {
        return Err("only mean or CLS pooling".into());
    }
    let data = std::fs::read(a.src.join("model.safetensors")).map_err(|e| format!("model.safetensors: {e}"))?;
    let sha = atlas::sha256::hex(&data);
    let raw = read_safetensors(&data)?;
    let mut w: HashMap<String, Tensor> = HashMap::new();
    for (k, v) in raw {
        w.insert(k.strip_prefix("bert.").map(str::to_string).unwrap_or(k), v);
    }
    let vocab_text = std::fs::read_to_string(a.src.join("vocab.txt")).map_err(|e| format!("vocab.txt: {e}"))?;
    let vocab: Vec<String> = py::text_lines(&vocab_text).into_iter().map(|l| l.trim_end_matches('\n').to_string()).collect();
    let vocab_size = u32_of(&cfg, "vocab_size")?;
    if vocab.len() != vocab_size as usize {
        return Err(format!("vocab.txt holds {} tokens, config.json says {vocab_size}", vocab.len()));
    }
    let db = Db::load(&a.ucd)?;
    let (mapping, ranges) = unicode_tables(&db, lowercase);
    let layers = u32_of(&cfg, "num_hidden_layers")?;
    let mut b: Vec<u8> = b"ATLASEMB".to_vec();
    for x in [
        1,
        u32_of(&cfg, "hidden_size")?,
        layers,
        u32_of(&cfg, "num_attention_heads")?,
        u32_of(&cfg, "intermediate_size")?,
        vocab_size,
        u32_of(&cfg, "max_position_embeddings")?,
        u32_of(&cfg, "type_vocab_size")?,
        u32_of(&st, "max_seq_length")?,
        pooling,
        u32::from(a.int8),
        u32::from(lowercase),
    ] {
        b.extend_from_slice(&x.to_le_bytes());
    }
    let eps = cfg.get("layer_norm_eps").and_then(Value::as_f64).ok_or("layer_norm_eps is not a number")?;
    b.extend_from_slice(&(eps as f32).to_le_bytes());
    for i in 0..32 {
        b.push(u8::from_str_radix(&sha[2 * i..2 * i + 2], 16).expect("sha256 hex"));
    }
    b.extend_from_slice(&(a.prefix.len() as u32).to_le_bytes());
    b.extend_from_slice(a.prefix.as_bytes());
    b.extend_from_slice(&(vocab.len() as u32).to_le_bytes());
    for t in &vocab {
        text(&mut b, t)?;
    }
    b.extend_from_slice(&(mapping.len() as u32).to_le_bytes());
    for (cp, m) in &mapping {
        b.extend_from_slice(&cp.to_le_bytes());
        text(&mut b, m)?;
    }
    b.extend_from_slice(&(ranges.len() as u32).to_le_bytes());
    for (s, e, f) in &ranges {
        for x in [s, e, f] {
            b.extend_from_slice(&x.to_le_bytes());
        }
    }
    for (name, matrix) in tensors(layers as usize) {
        let t = w.get(&name).ok_or_else(|| format!("model.safetensors has no {name}"))?;
        if matrix && a.int8 {
            let [rows, cols] = t.shape[..] else { return Err(format!("{name} is not a matrix")) };
            let vals: Vec<f32> = t.bytes.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().expect("4 bytes"))).collect();
            let mut q = Vec::with_capacity(rows * cols);
            for r in 0..rows {
                let row = &vals[r * cols..(r + 1) * cols];
                // numpy: abs().max(axis=1) / 127.0 in f32, a zero scale made 1.
                let mut scale = row.iter().fold(f32::NEG_INFINITY, |m, x| m.max(x.abs())) / 127.0f32;
                if scale == 0.0 {
                    scale = 1.0;
                }
                b.extend_from_slice(&scale.to_le_bytes());
                q.extend(row.iter().map(|x| (x / scale).round_ties_even().clamp(-127.0, 127.0) as i8 as u8));
            }
            b.extend_from_slice(&q);
            b.resize(b.len() + (4 - q.len() % 4) % 4, 0);
        } else {
            b.extend_from_slice(t.bytes);
        }
    }
    std::fs::write(&a.out, &b).map_err(|e| format!("{}: {e}", a.out))?;
    Ok(format!(
        "{}: {} bytes, {}, {} map entries, {} class ranges, source sha256 {sha}",
        a.out,
        b.len(),
        if a.int8 { "int8" } else { "f32" },
        mapping.len(),
        ranges.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mini_db() -> Db {
        let mut chars = vec![Ucd { cat: *b"Cn", ..Ucd::default() }; 0x110000];
        let set = |chars: &mut Vec<Ucd>, cp: u32, cat: &[u8; 2], ccc: u8, decomp: Vec<u32>, lower: Option<u32>| {
            chars[cp as usize] = Ucd { cat: *cat, ccc, decomp, lower };
        };
        set(&mut chars, 0xC9, b"Lu", 0, vec![0x45, 0x301], Some(0xE9));
        set(&mut chars, 0xE9, b"Ll", 0, vec![0x65, 0x301], None);
        set(&mut chars, 0x65, b"Ll", 0, vec![], None);
        set(&mut chars, 0x301, b"Mn", 230, vec![], None);
        set(&mut chars, 0x316, b"Mn", 220, vec![], None);
        Db { chars, special_lower: HashMap::new() }
    }

    #[test]
    fn nfd_decomposes_and_orders_marks() {
        let db = mini_db();
        assert_eq!(db.nfd(&db.lower(0xC9)), [0x65, 0x301]);
        assert_eq!(db.nfd(&[0x65, 0x301, 0x316]), [0x65, 0x316, 0x301]);
        // Hangul: GA (AC00) is L + V; GAG (AC01) adds a T.
        assert_eq!(db.nfd(&[0xAC01]), [0x1100, 0x1161, 0x11A8]);
    }
}
