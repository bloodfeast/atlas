//! Runtime settings of the memory store, the recall hook and the
//! context-cap hook, resolved from the configuration.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::Result;

use super::export::{ExportSpec, Shape};

/// Recall budgets and the relevance gate.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallSettings {
    /// Tokens one prompt's recall may inject (pointers: title and key).
    pub budget_tokens: usize,
    /// Tokens a lane recall may take.
    pub lane_budget_tokens: usize,
    /// The least FTS5 score (negated bm25) a record needs.
    pub fts_floor: f64,
    /// The least number of distinct query words a record must contain.
    pub min_terms: usize,
    /// The cosine a lexical match also needs, when a model is loaded.
    pub cos_floor: f32,
    /// The cosine at which a record passes on meaning alone.
    pub cos_alone: f32,
    /// Tokens of the prompt that are embedded (its head).
    pub query_tokens: usize,
    /// The bm25 floor of a lexical match when a model is loaded (the
    /// cosine floor then guards it, so it is calibrated lower).
    pub hybrid_fts_floor: f64,
    /// With a model, how far below the prompt's best cosine a recalled
    /// record may be.
    pub cos_margin: f32,
    /// The cosine below which a hook injects a record not at all (records
    /// without a vector are not held to it).
    pub inject_floor: f32,
}

impl RecallSettings {
    /// The gate these settings describe.
    pub fn gate(&self, hybrid: bool) -> super::recall::Gate {
        let fts_floor = if hybrid { self.hybrid_fts_floor } else { self.fts_floor };
        super::recall::Gate { fts_floor, min_terms: self.min_terms, cos_floor: self.cos_floor, cos_alone: self.cos_alone }
    }
}

/// Context-cap thresholds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapSettings {
    /// Warn at this many context tokens.
    pub warn_tokens: u64,
    /// Block every tool but `handoff` at this many.
    pub block_tokens: u64,
    /// Warn again after this many more tokens.
    pub warn_every: u64,
    /// Apply to the main session too.
    pub main_session: bool,
}

/// Everything the memory tools and hooks need.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The store file.
    pub store: PathBuf,
    /// The private export folder, if exports are on.
    pub export_dir: Option<PathBuf>,
    /// The exports.
    pub exports: Vec<ExportSpec>,
    /// Seconds between automatic snapshots (0: off).
    pub snapshot_secs: u64,
    /// The snapshot folder.
    pub snapshot_dir: PathBuf,
    /// Snapshot size cap in bytes.
    pub snapshot_cap: u64,
    /// Recall settings.
    pub recall: RecallSettings,
    /// The packed embedding model the gate scores meaning with (see
    /// [`crate::models::semantic_model`]). A missing file leaves the gate
    /// lexical until it appears.
    pub model: PathBuf,
    /// Encoder threads for embedding records.
    pub embed_threads: usize,
    /// Context-cap settings.
    pub cap: CapSettings,
    /// Bytes per token (the ledger's ratio).
    pub bytes_per_token: f64,
    /// The index database, for the token log.
    pub index_db: PathBuf,
    /// The rulebook whose size at each subagent start is logged as the
    /// "brief without atlas" (see [`crate::tokens::log_brief`]): the
    /// configured `memory.brief_reference`, else the first `items` export.
    pub brief_reference: Option<PathBuf>,
    /// The configuration's `lean` section (see [`crate::config::Config::lean`]).
    pub lean: Option<crate::json::Value>,
    /// Memory folders whose memories marked `scope: user` the user export
    /// also imports (see [`crate::project`]).
    pub user_scope_from: Vec<PathBuf>,
    /// The user layer's memory folder: `remember` with `scope: user` writes
    /// there (see [`crate::project`]).
    pub user_memory: Option<PathBuf>,
    /// The user-level state folder of a configuration with projects: the
    /// asked-once offer state and the routing switch live there, shared by
    /// every project (else both stay beside the index database).
    pub user_state: Option<PathBuf>,
    /// The active project's repositories and links, when there is one.
    pub graph: Option<std::sync::Arc<crate::project::Graph>>,
    /// The memory index as a tree (see [`super::tree`]).
    pub tree: Option<super::tree::TreeSpec>,
}

/// The default relevance floor: the negated bm25 a record needs. Derived by
/// `cargo xtask calibrate-recall` from `--recall-scores` over 40 prompts unrelated
/// to the corpus (the floor clears every one of them) and 80 real prompts
/// (64 of 80 then recall something); see `docs/design/atlas-c.md`. bm25
/// scales with the corpus, so a different corpus recalibrates it.
pub const DEFAULT_FTS_FLOOR: f64 = 9.72;
/// The number of records in the store the bm25 floors were calibrated on.
pub const CALIBRATION_RECORDS: i64 = 1400;

/// The factor a calibrated bm25 floor is scaled by for a store of
/// `records` records. bm25 sums each matched term's IDF, and FTS5's IDF of
/// a term found in one record is `ln((N - 0.5) / 1.5)`: on a small store
/// every score shrinks with it, so an absolute floor tuned on a large store
/// recalls nothing on a new one. The floor follows the same curve (1.0 at
/// the calibration size, 0.27 at 10 records, 0 below 3); the matched-words
/// rule still guards an unrelated prompt.
pub fn floor_scale(records: i64) -> f64 {
    let idf = |n: i64| ((n as f64 - 0.5) / 1.5).ln().max(0.0);
    idf(records) / idf(CALIBRATION_RECORDS)
}

/// The default [`RecallSettings::cos_margin`]. Measured on a replay of 24
/// orchestrator prompts against a 1,400-record store with the bge int8
/// model: the records labelled off-topic passed the gate at cosines 0.47 to
/// 0.56 while each prompt's best was 0.58 to 0.75; 0.10 below the best
/// drops most of them and keeps every top record.
pub const DEFAULT_COS_MARGIN: f32 = 0.10;

/// The default [`RecallSettings::inject_floor`], from
/// `atlas --hook-report --tune-floor 1` ([`crate::hook_hits::tune`]): the
/// cosine that maximises the tokens hits save less the tokens injected (see
/// the script for the objective and the data it was run on).
pub const DEFAULT_INJECT_FLOOR: f32 = 0.0;

/// The default least number of distinct query words in a recalled record.
pub const DEFAULT_MIN_TERMS: usize = 3;
/// The default floors of the hybrid gate (a model loaded), derived together
/// by `cargo xtask calibrate-recall` over the same 120 prompts with the bge
/// int8 model: 0 of 40 unrelated prompts recall anything, 69 of 80 real ones
/// do (64 with the lexical gate). A different model or corpus recalibrates.
pub const DEFAULT_HYBRID_FTS_FLOOR: f64 = 8.89;
/// The default cosine a lexical match also needs (with a model). Raised
/// from the calibrated 0.43 (Brandon, 2026-10-31): on a replay against the
/// live store, records labelled off-topic passed at cosines up to 0.56.
pub const DEFAULT_COS_FLOOR: f32 = 0.57;
/// The default cosine at which a record passes on meaning alone: above the
/// highest cosine of any unrelated prompt's candidate (0.70).
pub const DEFAULT_COS_ALONE: f32 = 0.72;
/// The default number of prompt tokens the gate embeds: the same verdicts
/// as 64, 128 and 512 tokens on the calibration prompts, at half the p99
/// of 64 (the encoder's cost grows with the tokens).
pub const DEFAULT_QUERY_TOKENS: usize = 32;

impl Settings {
    /// Resolves settings from an optional configuration and the index
    /// database path.
    ///
    /// # Errors
    ///
    /// [`crate::Error::InvalidArgument`] for an unknown export shape.
    pub fn from_config(c: Option<&Config>, db: &Path) -> Result<Settings> {
        let beside = db.parent().unwrap_or(Path::new(".")).to_path_buf();
        let m = c.and_then(|c| c.memory.clone()).unwrap_or_default();
        let r = c.and_then(|c| c.recall.clone()).unwrap_or_default();
        let k = c.and_then(|c| c.cap.clone()).unwrap_or_default();
        let store = m.store.clone().unwrap_or_else(|| beside.join("memory.sqlite"));
        let mut exports = Vec::new();
        for e in &m.exports {
            exports.push(ExportSpec {
                name: e.name.clone(),
                shape: Shape::parse(&e.shape)?,
                path: PathBuf::from(&e.path),
                index: e.index.clone().unwrap_or_else(|| "MEMORY.md".into()),
                handoffs: e.handoffs,
                index_cap: e.index_cap,
                index_topics: e.index_topics.as_ref().map(|ts| ts.iter().map(|t| (t.name.clone(), t.words.clone())).collect()),
            });
        }
        let snapshot_dir = m.snapshot_dir.clone().unwrap_or_else(|| store.parent().unwrap_or(Path::new(".")).join("snapshots"));
        let brief_reference = m.brief_reference.clone().or_else(|| {
            let dir = m.export_dir.as_ref()?;
            exports.iter().find(|e| e.shape == Shape::Items).map(|e| dir.join(&e.path))
        });
        Ok(Settings {
            store,
            export_dir: m.export_dir.clone(),
            exports,
            snapshot_secs: m.snapshot_hours.unwrap_or(24) as u64 * 3600,
            snapshot_dir,
            snapshot_cap: m.snapshot_cap_mb.unwrap_or(1024) as u64 * 1024 * 1024,
            recall: RecallSettings {
                // Pointers, not bodies: a title and key is about 30 tokens.
                budget_tokens: r.budget_tokens.unwrap_or(150),
                lane_budget_tokens: r.lane_budget_tokens.unwrap_or(2000),
                fts_floor: r.fts_floor.unwrap_or(DEFAULT_FTS_FLOOR),
                min_terms: r.min_terms.unwrap_or(DEFAULT_MIN_TERMS),
                cos_floor: r.cos_floor.map_or(DEFAULT_COS_FLOOR, |x| x as f32),
                cos_alone: r.cos_alone.map_or(DEFAULT_COS_ALONE, |x| x as f32),
                query_tokens: r.query_tokens.unwrap_or(DEFAULT_QUERY_TOKENS),
                hybrid_fts_floor: r.hybrid_fts_floor.unwrap_or(DEFAULT_HYBRID_FTS_FLOOR),
                cos_margin: DEFAULT_COS_MARGIN,
                inject_floor: r.inject_floor.map_or(DEFAULT_INJECT_FLOOR, |x| x as f32),
            },
            model: crate::models::semantic_model(c, db),
            embed_threads: c.and_then(|c| c.semantic.as_ref()).and_then(|s| s.threads).unwrap_or(crate::config::DEFAULT_EMBED_THREADS).max(1),
            cap: CapSettings {
                warn_tokens: k.warn_tokens.unwrap_or(450_000) as u64,
                block_tokens: k.block_tokens.unwrap_or(500_000) as u64,
                warn_every: k.warn_every.unwrap_or(25_000) as u64,
                main_session: k.main_session.unwrap_or(false),
            },
            bytes_per_token: c.and_then(|c| c.bytes_per_token).unwrap_or(crate::tokens::DEFAULT_BYTES_PER_TOKEN),
            index_db: db.to_path_buf(),
            brief_reference,
            lean: c.and_then(|c| c.lean.clone()),
            user_scope_from: m.user_scope_from.clone(),
            user_memory: m.user_memory.clone(),
            user_state: m.user_state.clone(),
            graph: None,
            tree: m.tree.clone(),
        })
    }

    /// Bytes allowed for a token budget.
    pub fn bytes_for(&self, tokens: usize) -> usize {
        (tokens as f64 * self.bytes_per_token) as usize
    }

    /// The export memory records are listed in (the first memory-shaped one).
    pub fn memory_export(&self) -> Option<&ExportSpec> {
        self.exports.iter().find(|e| e.shape == Shape::MemoryDir)
    }

    /// The export new handoff records are added to.
    pub fn handoff_export(&self) -> Option<&ExportSpec> {
        self.exports.iter().find(|e| e.handoffs)
    }
}
