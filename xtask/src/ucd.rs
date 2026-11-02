//! The Unicode Character Database, read from `UnicodeData.txt` and
//! `SpecialCasing.txt` (the 15.1.0 copies committed in `xtask/ucd/15.1.0/`,
//! Unicode License V3 beside them): general category, combining class,
//! canonical decomposition and lowercase per code point.
//!
//! Used by `pack-embedder` for the tokenizer tables it packs, and by the
//! leak check (`tests/leak_check.rs` includes this file by path) to
//! normalise text before matching. Self-contained: no `crate::` paths, so
//! both can include it.

use std::collections::HashMap;
use std::path::Path;

/// What the Unicode Character Database says of one code point.
#[derive(Clone, Default)]
pub struct Ucd {
    /// The general category, two letters (`Lu`, `Mn`, `Cf`, ...).
    pub cat: [u8; 2],
    /// The canonical combining class.
    pub ccc: u8,
    /// The canonical decomposition mapping (not a compatibility one).
    pub decomp: Vec<u32>,
    /// The simple lowercase mapping.
    pub lower: Option<u32>,
}

/// The UCD, indexed by code point.
pub struct Db {
    /// Per code point, 0 to 0x10FFFF.
    pub chars: Vec<Ucd>,
    /// SpecialCasing.txt's unconditional lowercase mappings.
    pub special_lower: HashMap<u32, Vec<u32>>,
}

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = 21 * T_COUNT;
const S_COUNT: u32 = 19 * N_COUNT;

fn hex(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim(), 16).map_err(|_| format!("UCD: bad code point {s:?}"))
}

impl Db {
    /// Reads `UnicodeData.txt` and `SpecialCasing.txt` from `dir`.
    ///
    /// # Errors
    ///
    /// A missing file or a malformed line, named.
    pub fn load(dir: &Path) -> Result<Db, String> {
        let read = |n: &str| std::fs::read_to_string(dir.join(n)).map_err(|e| format!("{}: {e}", dir.join(n).display()));
        let data = read("UnicodeData.txt")?;
        let unassigned = Ucd { cat: *b"Cn", ..Ucd::default() };
        let mut chars = vec![unassigned; 0x110000];
        let mut first: Option<(u32, Ucd)> = None;
        for line in data.lines().filter(|l| !l.trim().is_empty()) {
            let f: Vec<&str> = line.split(';').collect();
            if f.len() < 15 {
                return Err(format!("UnicodeData.txt: short line {line:?}"));
            }
            let cp = hex(f[0])?;
            let cat: [u8; 2] = f[2].as_bytes().try_into().map_err(|_| format!("UnicodeData.txt: bad category in {line:?}"))?;
            let ccc = f[3].parse().map_err(|_| format!("UnicodeData.txt: bad combining class in {line:?}"))?;
            let decomp = if f[5].is_empty() || f[5].starts_with('<') {
                Vec::new()
            } else {
                f[5].split_whitespace().map(hex).collect::<Result<_, _>>()?
            };
            let lower = if f[13].is_empty() { None } else { Some(hex(f[13])?) };
            let u = Ucd { cat, ccc, decomp, lower };
            if f[1].ends_with(", First>") {
                first = Some((cp, u));
            } else if f[1].ends_with(", Last>") {
                let (start, u) = first.take().ok_or_else(|| format!("UnicodeData.txt: {line:?} has no First"))?;
                for c in start..=cp {
                    chars[c as usize] = u.clone();
                }
            } else {
                chars[cp as usize] = u;
            }
        }
        let mut special_lower = HashMap::new();
        for line in read("SpecialCasing.txt")?.lines() {
            let line = line.split('#').next().unwrap_or("");
            if line.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split(';').map(str::trim).collect();
            // A fifth field names a condition: those mappings are skipped,
            // as Python's tables skip them.
            if f.len() < 5 || !f[4].is_empty() {
                continue;
            }
            special_lower.insert(hex(f[0])?, f[1].split_whitespace().map(hex).collect::<Result<_, _>>()?);
        }
        Ok(Db { chars, special_lower })
    }

    /// What the database says of `cp`.
    pub fn get(&self, cp: u32) -> &Ucd {
        &self.chars[cp as usize]
    }

    /// `str.lower()` of one character: the unconditional special casing,
    /// else the simple lowercase mapping, else the character.
    pub fn lower(&self, cp: u32) -> Vec<u32> {
        if let Some(m) = self.special_lower.get(&cp) {
            return m.clone();
        }
        vec![self.get(cp).lower.unwrap_or(cp)]
    }

    /// The full canonical decomposition of `cp`, appended to `out`
    /// (not yet ordered: see [`Db::nfd`]).
    pub fn decompose(&self, cp: u32, out: &mut Vec<u32>) {
        if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
            let s = cp - S_BASE;
            out.push(L_BASE + s / N_COUNT);
            out.push(V_BASE + (s % N_COUNT) / T_COUNT);
            if s % T_COUNT != 0 {
                out.push(T_BASE + s % T_COUNT);
            }
            return;
        }
        let d = &self.get(cp).decomp;
        if d.is_empty() {
            out.push(cp);
        } else {
            for &c in d {
                self.decompose(c, out);
            }
        }
    }

    /// `unicodedata.normalize("NFD", s)`: full canonical decomposition, then
    /// each run of non-starters stably sorted by combining class.
    pub fn nfd(&self, s: &[u32]) -> Vec<u32> {
        let mut out = Vec::new();
        for &c in s {
            self.decompose(c, &mut out);
        }
        let mut i = 0;
        while i < out.len() {
            if self.get(out[i]).ccc == 0 {
                i += 1;
                continue;
            }
            let start = i;
            while i < out.len() && self.get(out[i]).ccc != 0 {
                i += 1;
            }
            out[start..i].sort_by_key(|&c| self.get(c).ccc);
        }
        out
    }
}
