//! The measured baseline, as the token report reads it: what agents
//! actually spent on rule and handoff lookups before atlas, measured from
//! real Claude Code transcripts by [`crate::calibrate`] (in the background
//! at a server start, or `atlas --calibrate`; the method is documented
//! there) and saved to `baseline_log`.
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
    total INTEGER NOT NULL,
    rules INTEGER,
    fingerprint TEXT,
    cutoff_source TEXT,
    skips TEXT,
    too_few INTEGER NOT NULL DEFAULT 0
);
";

/// The columns added after the first release of the table, for a store
/// that has the old one: (name, type).
const ADDED: &[(&str, &str)] = &[("rules", "INTEGER"), ("fingerprint", "TEXT"), ("cutoff_source", "TEXT"), ("skips", "TEXT"), ("too_few", "INTEGER NOT NULL DEFAULT 0")];

/// Creates the table, or adds the columns an older one lacks. Rows saved
/// before them read as unstamped (`rules` empty): stale.
///
/// # Errors
///
/// [`crate::Error::StoreTooNew`] when the table or a column is missing and a
/// newer atlas upgraded the store (nothing is made or altered), or
/// [`crate::Error::Db`].
pub fn ensure(conn: &Connection) -> Result<()> {
    let columns = || -> Result<Vec<String>> { Ok(conn.prepare("SELECT name FROM pragma_table_info('baseline_log')")?.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?) };
    let complete = |have: &[String]| !have.is_empty() && ADDED.iter().all(|(name, _)| have.iter().any(|h| h == name));
    // The usual case reads only: the table is there whole.
    if complete(&columns()?) {
        return Ok(());
    }
    // The table and its columns are made under the write lock, after the
    // version check (an ALTER is a real schema change on a store a newer
    // atlas may have upgraded), and the columns are read again under it.
    crate::store::guarded(conn, || {
        conn.execute_batch(TABLE)?;
        let have = columns()?;
        for (name, ty) in ADDED {
            if !have.iter().any(|h| h == name) {
                conn.execute_batch(&format!("ALTER TABLE baseline_log ADD COLUMN {name} {ty}"))?;
            }
        }
        Ok(())
    })
}

/// What a calibration measured on the pre-atlas side.
#[derive(Debug, Clone, PartialEq, Default)]
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
    /// The lookup rules' version it was measured with
    /// ([`crate::calibrate::RULES_VERSION`]); `None` for a row saved before
    /// rules were versioned, or by hand with explicit inputs.
    pub rules: Option<u32>,
    /// The configured targets it was measured against
    /// ([`crate::calibrate::Plan::fingerprint`]).
    pub fingerprint: Option<String>,
    /// Where the cutoff came from.
    pub cutoff_source: Option<String>,
    /// The skips it applied, one per line.
    pub skips: Option<String>,
    /// Too little pre-atlas history for a measured ratio
    /// ([`crate::calibrate::MIN_EPISODES`], [`crate::calibrate::MIN_TRANSCRIPTS`]).
    pub too_few: bool,
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
/// [`crate::Error::StoreTooNew`] (nothing written), or [`crate::Error::Db`].
pub fn save(conn: &Connection, b: &Baseline) -> Result<()> {
    crate::store::guarded(conn, || {
        ensure(conn)?;
        conn.execute(
            "INSERT INTO baseline_log(time, before, transcripts, used, episodes, mean, median, p90, total, rules, fingerprint, cutoff_source, skips, too_few) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                crate::store::utc(SystemTime::now()),
                b.before,
                b.transcripts as i64,
                b.used as i64,
                b.episodes as i64,
                b.mean,
                b.median,
                b.p90,
                b.total as i64,
                b.rules.map(i64::from),
                b.fingerprint,
                b.cutoff_source,
                b.skips,
                i64::from(b.too_few)
            ],
        )?;
        Ok(())
    })
}

/// The latest saved calibration, with its time.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn latest(conn: &Connection) -> Result<Option<(String, Baseline)>> {
    ensure(conn)?;
    let mut st = conn.prepare("SELECT time, before, transcripts, used, episodes, mean, median, p90, total, rules, fingerprint, cutoff_source, skips, too_few FROM baseline_log ORDER BY id DESC LIMIT 1")?;
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
                rules: r.get::<_, Option<i64>>(9)?.and_then(|v| u32::try_from(v).ok()),
                fingerprint: r.get(10)?,
                cutoff_source: r.get(11)?,
                skips: r.get(12)?,
                too_few: r.get::<_, i64>(13)? != 0,
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_table_gains_the_stamps_and_its_rows_read_unstamped() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE baseline_log(id INTEGER PRIMARY KEY, time TEXT NOT NULL, before TEXT NOT NULL, transcripts INTEGER NOT NULL, used INTEGER NOT NULL, episodes INTEGER NOT NULL, mean REAL NOT NULL, median REAL NOT NULL, p90 REAL NOT NULL, total INTEGER NOT NULL);
             INSERT INTO baseline_log(time, before, transcripts, used, episodes, mean, median, p90, total) VALUES('2026-11-08T11:40:52.532Z', '2026-10-08T19:20:49Z', 1089, 81, 878, 5120.1, 2817.0, 12907.0, 4495497);",
        )
        .unwrap();
        let (when, b) = latest(&conn).unwrap().unwrap();
        assert_eq!(when, "2026-11-08T11:40:52.532Z");
        assert_eq!((b.episodes, b.median, b.rules, b.fingerprint, b.too_few), (878, 2817.0, None, None, false));
        ensure(&conn).unwrap();
        let s = Baseline { before: "t".into(), episodes: 3, rules: Some(2), fingerprint: Some("f".into()), cutoff_source: Some("c".into()), skips: Some("a\nb".into()), too_few: true, ..Default::default() };
        save(&conn, &s).unwrap();
        assert_eq!(latest(&conn).unwrap().unwrap().1, s);
        assert_eq!(conn.query_row("SELECT count(*) FROM baseline_log", [], |r| r.get::<_, i64>(0)).unwrap(), 2, "the old row is kept");
    }
}

#[cfg(test)]
mod storeguard_tests {
    use super::*;
    use crate::store::skew_support::{dump, make_index_newer};

    /// On a store a newer atlas upgraded since this one opened it, a save
    /// writes nothing and the table is neither made nor altered; a whole
    /// table is only read.
    #[test]
    fn a_save_and_its_table_check_the_version_under_the_lock() {
        let c = Connection::open_in_memory().unwrap();
        make_index_newer(&c);
        let before = dump(&c);
        assert!(save(&c, &Baseline::default()).expect_err("refused").is_store_too_new());
        assert!(ensure(&c).expect_err("refused").is_store_too_new());
        assert_eq!(dump(&c), before, "no table made");
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE baseline_log(id INTEGER PRIMARY KEY, time TEXT NOT NULL);").unwrap();
        make_index_newer(&c);
        let before = dump(&c);
        assert!(ensure(&c).expect_err("refused").is_store_too_new());
        assert_eq!(dump(&c), before, "no column added");
        let c = Connection::open_in_memory().unwrap();
        ensure(&c).unwrap();
        make_index_newer(&c);
        let before = dump(&c);
        assert!(save(&c, &Baseline::default()).expect_err("refused").is_store_too_new());
        ensure(&c).unwrap();
        assert!(c.is_autocommit(), "rolled back");
        assert_eq!(dump(&c), before);
    }
}
