//! The measured baseline, as the token report reads it: what agents
//! actually spent on rule and handoff lookups before atlas, measured from
//! real Claude Code transcripts by `cargo xtask calibrate-baseline` (the
//! method is documented there, in `xtask/src/calibrate.rs`) and saved to
//! `baseline_log`.
//!
//! The token ledger's counterfactual ([`crate::tokens`]) is an upper bound.
//! `--token-report` heads its output with the **measured** ratio per answer:
//! the median bytes of a pre-atlas lookup episode over the median bytes atlas
//! returned per answer of the comparable read tools ([`COMPARABLE`]), the
//! means beside it; and the **upper bound**: the sum of the counterfactual
//! over the sum returned, over answers that have one.

use std::time::SystemTime;

use rusqlite::{params, Connection};

use crate::error::Result;

/// The read tools the measured ratio compares against.
pub const COMPARABLE: &[&str] = &["lookup", "search", "recall", "section", "grep", "read_note"];

/// The table calibration runs are saved to. Durable.
pub const TABLE: &str = "
CREATE TABLE IF NOT EXISTS baseline_log(
    id INTEGER PRIMARY KEY,
    time TEXT NOT NULL,
    before TEXT NOT NULL,
    transcripts INTEGER NOT NULL,
    used INTEGER NOT NULL,
    episodes INTEGER NOT NULL,
    mean REAL NOT NULL,
    median REAL NOT NULL,
    p90 REAL NOT NULL,
    total INTEGER NOT NULL
);
";

/// What a calibration measured on the pre-atlas side.
#[derive(Debug, Clone, PartialEq)]
pub struct Baseline {
    /// Only calls before this time counted.
    pub before: String,
    /// Transcripts read.
    pub transcripts: usize,
    /// Of them, transcripts with at least one lookup.
    pub used: usize,
    /// Lookup episodes.
    pub episodes: usize,
    /// Bytes per episode: mean, median, 90th percentile.
    pub mean: f64,
    /// Median bytes per episode.
    pub median: f64,
    /// 90th percentile bytes per episode.
    pub p90: f64,
    /// Bytes over every episode.
    pub total: u64,
}

/// The value at quantile `q` of sorted values (the element at
/// `floor(q * n)`, the last for `q = 1`), 0 when empty.
pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((q * sorted.len() as f64) as usize).min(sorted.len() - 1)]
}

/// The median: the middle value, or the mean of the two middle values.
pub fn median(sorted: &[f64]) -> f64 {
    match sorted.len() {
        0 => 0.0,
        n if n % 2 == 1 => sorted[n / 2],
        n => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
    }
}

/// Saves a calibration run.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn save(conn: &Connection, b: &Baseline) -> Result<()> {
    conn.execute_batch(TABLE)?;
    conn.execute(
        "INSERT INTO baseline_log(time, before, transcripts, used, episodes, mean, median, p90, total) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![crate::store::utc(SystemTime::now()), b.before, b.transcripts as i64, b.used as i64, b.episodes as i64, b.mean, b.median, b.p90, b.total as i64],
    )?;
    Ok(())
}

/// The latest saved calibration, with its time.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn latest(conn: &Connection) -> Result<Option<(String, Baseline)>> {
    conn.execute_batch(TABLE)?;
    let mut st = conn.prepare("SELECT time, before, transcripts, used, episodes, mean, median, p90, total FROM baseline_log ORDER BY id DESC LIMIT 1")?;
    let mut rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Baseline {
                before: r.get(1)?,
                transcripts: r.get::<_, i64>(2)? as usize,
                used: r.get::<_, i64>(3)? as usize,
                episodes: r.get::<_, i64>(4)? as usize,
                mean: r.get(5)?,
                median: r.get(6)?,
                p90: r.get(7)?,
                total: r.get::<_, i64>(8)? as u64,
            },
        ))
    })?;
    Ok(rows.next().transpose()?)
}

/// The atlas side, from the token ledger: the comparable answers' returned
/// bytes (sorted), and the counterfactual and returned sums over those that
/// have a counterfactual.
#[derive(Debug, Clone, PartialEq)]
pub struct AtlasSide {
    /// Returned bytes of every comparable answer, sorted.
    pub returned: Vec<f64>,
    /// Sum of the counterfactual over answers that have one.
    pub counterfactual: f64,
    /// Sum returned over the same answers.
    pub returned_comparable: f64,
}

impl AtlasSide {
    /// Reads the ledger.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn read(conn: &Connection) -> Result<AtlasSide> {
        conn.execute_batch(crate::tokens::TOKEN_TABLE)?;
        let marks = COMPARABLE.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(",");
        let mut st = conn.prepare(&format!("SELECT returned, grep, read FROM token_log WHERE tool IN ({marks})"))?;
        let rows: Vec<(i64, Option<i64>, Option<i64>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
        let mut returned: Vec<f64> = rows.iter().map(|r| r.0 as f64).collect();
        returned.sort_by(f64::total_cmp);
        let with: Vec<&(i64, Option<i64>, Option<i64>)> = rows.iter().filter(|r| r.1.is_some()).collect();
        Ok(AtlasSide {
            returned,
            counterfactual: with.iter().map(|r| (r.1.unwrap_or(0) + r.2.unwrap_or(0)) as f64).sum(),
            returned_comparable: with.iter().map(|r| r.0 as f64).sum(),
        })
    }

    /// The upper-bound ratio (0 with no comparable answers).
    pub fn upper(&self) -> f64 {
        if self.returned_comparable > 0.0 {
            self.counterfactual / self.returned_comparable
        } else {
            0.0
        }
    }

    /// The measured ratios (median, mean) against a baseline (0 when either
    /// side is empty).
    pub fn measured(&self, b: &Baseline) -> (f64, f64) {
        let med = median(&self.returned);
        let mean = if self.returned.is_empty() { 0.0 } else { self.returned.iter().sum::<f64>() / self.returned.len() as f64 };
        (if med > 0.0 { b.median / med } else { 0.0 }, if mean > 0.0 { b.mean / mean } else { 0.0 })
    }
}

