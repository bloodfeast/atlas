//! The embedder against truncated, mutated and foreign model files, and the
//! tokenizer and encoder against empty, enormous and hostile text.

use std::time::{Duration, Instant};

use crate::gen::{each, Rng, DEEP, HOSTILE};
use atlas::embed::bert::Scratch;
use atlas::embed::gemm::Isa;
use atlas::embed::{tiny_model_bytes, Model};

fn unit(v: &[f32]) -> bool {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().all(|x| x.is_finite()) && (n - 1.0).abs() < 1e-3
}

#[test]
fn every_truncation_of_a_model_file_is_refused() {
    for int8 in [false, true] {
        let b = tiny_model_bytes(int8, 3);
        for n in 0..b.len() {
            assert!(Model::parse(&b[..n], Isa::Scalar).is_err(), "a {n}-byte prefix of a {}-byte model parsed", b.len());
        }
    }
    for foreign in [&b""[..], b"ATLASEMB", b"ATLASEMB\x02\0\0\0", b"\x89PNG\r\n\x1a\n", b"GGUF\x03\0\0\0", &[0u8; 4096][..]] {
        assert!(Model::parse(foreign, Isa::Scalar).is_err());
    }
}

/// Mutated model files: refused, or parsed into a model that embeds any
/// text into a finite unit vector. Never a panic or a giant allocation.
fn mutated_models(n: usize) {
    let seeds = [tiny_model_bytes(false, 5), tiny_model_bytes(true, 6)];
    each(60, n, |r| {
        let mut b = r.pick(&seeds).clone();
        // Header fields are where counts and shapes live: hit them often.
        if r.chance(1, 2) {
            let at = 12 + 4 * r.below(12);
            let v = match r.below(4) {
                0 => 0u32,
                1 => u32::MAX,
                2 => 1 << r.below(32),
                _ => r.next() as u32,
            };
            if at + 4 <= b.len() {
                b[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }
        } else {
            r.mutate(&mut b);
        }
        b
    }, |b| {
        let t = Instant::now();
        if let Ok(m) = Model::parse(b, Isa::Scalar) {
            let mut s = Scratch::default();
            for text in ["", "a", "the widget cache", "\u{1f980}\u{0}"] {
                let v = m.embed(text, &mut s);
                // Mutated weights may hold NaN; the shape still holds.
                assert_eq!(v.len(), m.dim);
            }
        }
        assert!(t.elapsed() < Duration::from_secs(5));
    });
}

#[test]
fn fuzz_model_files() {
    mutated_models(1500);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_fuzz_model_files() {
    mutated_models(1500 * DEEP);
}

#[test]
fn hostile_text_embeds_to_unit_vectors() {
    let m = Model::parse(&tiny_model_bytes(true, 7), Isa::detect()).unwrap();
    let mut s = Scratch::default();
    let long = "widget ".repeat(2_000_000);
    let one_word = "w".repeat(1_000_000);
    let replacement = "\u{fffd}".repeat(10_000);
    let controls: String = (0u8..32).map(char::from).collect::<String>().repeat(1000);
    for text in [String::new(), " ".into(), "\0".into(), long, one_word, replacement, controls, "\u{e9}\u{301}\u{130}\u{212a}".repeat(1000)] {
        let t = Instant::now();
        let v = m.embed(&text, &mut s);
        assert!(v.iter().all(|x| x.is_finite()), "non-finite");
        assert!(unit(&v) || v.iter().all(|x| *x == 0.0), "not a unit vector");
        let q = m.embed_query_cut(&text, 0, &mut s);
        assert!(q.iter().all(|x| x.is_finite()));
        let _ = m.embed_query_cut(&text, usize::MAX, &mut s);
        assert!(t.elapsed() < Duration::from_secs(10), "{} bytes took {:?}", text.len(), t.elapsed());
    }
    // Lossy UTF-8 is what the tokenizer sees for invalid bytes.
    let lossy = String::from_utf8_lossy(b"\xff\xfe\xc0\x80widget\xed\xa0\x80").into_owned();
    assert!(unit(&m.embed(&lossy, &mut s)));
}

fn tokenizer_never_panics(n: usize) {
    let m = Model::parse(&tiny_model_bytes(false, 8), Isa::Scalar).unwrap();
    each(61, n, |r: &mut Rng| (r.text_from(HOSTILE, 40), r.below(600)), |(t, max)| {
        let words = m.tok.words(t);
        assert!(words.iter().all(|w| !w.is_empty()), "an empty word");
        let ids = m.tok.encode(t, *max);
        assert!(ids.len() <= (*max).max(2), "{} ids for max {max}", ids.len());
        let all = m.tok.encode_all(t);
        let _ = all;
    });
}

#[test]
fn fuzz_tokenizer() {
    tokenizer_never_panics(4000);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_fuzz_tokenizer() {
    tokenizer_never_panics(4000 * DEEP);
}

#[test]
fn a_tokenizer_without_its_special_tokens_is_refused() {
    use atlas::embed::wordpiece::Tokenizer;
    assert!(Tokenizer::new(vec![], vec![], vec![], true).is_none());
    assert!(Tokenizer::new(vec!["[CLS]".into(), "[SEP]".into()], vec![], vec![], true).is_none());
    let t = Tokenizer::new(vec!["[PAD]".into(), "[UNK]".into(), "[CLS]".into(), "[SEP]".into(), "".into(), "##".into()], vec![(0x130, String::new())], vec![(0, u32::MAX, 0)], true);
    if let Some(t) = t {
        let _ = t.encode("\u{130} a ## b", 10);
        let _ = t.encode("", 0);
    }
}
