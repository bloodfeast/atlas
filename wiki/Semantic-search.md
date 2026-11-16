`search` mode `semantic` ranks every section by the cosine of its best window's embedding with the query's; `hybrid` fuses
that ranking with FTS5's (the query's words ORed) by reciprocal rank. The answer is the sections answer.

- **Model.** A BERT sentence embedder, packed once by `cargo xtask pack-embedder` (a tool step outside atlas,
  reading the Unicode Character Database 15.1.0 files committed in `xtask/ucd/` for the tables) from the public Hugging Face
  files into one little-endian file that also carries the tokenizer's Unicode tables. Atlas reads it with `std::fs::read`; no model format, ML or protobuf crate.
  Users never run the packing step: the packed file is a release asset that `atlas --setup` downloads
  (`src/models.rs`), resuming an interrupted transfer with a range request and refusing (and deleting) a
  file whose SHA-256 does not match its pin.
- **Inference is owned** (`src/embed/`): the WordPiece tokenizer (Hugging Face `BertTokenizer` rules), the
  encoder, mean or `[CLS]` pooling and L2 normalisation. Matrices are f32 or int8 with a scale per row
  (activations quantised per row at run time, integer dot products). The kernels (`embed/gemm.rs`) are AVX2+FMA
  (detected at run time) and NEON over a scalar reference that emulates their exact summation order, so SIMD
  and scalar results are tested bit-identical, not merely close.
- **Windows.** The encoder reads 512 tokens, so a longer section used to be embedded by its first 510 only
  (about a third of the indexed text was never embedded). A section longer than that is now embedded as
  overlapping windows and scores by its best window (max-sim). Window 0 is the section's own text, cut as
  before; windows 1..n are the heading path, a newline and a stretch of the body cut on word boundaries, at
  most 510 tokens with the heading (the heading kept to its first 128 tokens), each starting 384 body tokens
  after the one before, the last moved back to end at the body's end. Tokens are the model's own WordPiece
  pieces. The rules are in `src/embed/window.rs`, versioned by `WINDOW_RULES_VERSION` (a test hashes the file
  against `WINDOW_RULES_HISTORY`); a new version re-splits every section, and only windows whose text changed
  are embedded again.
- **Vectors** are cached by the SHA-256 of the text they embed, so a re-index re-embeds only changed text and
  a rebuilt database keeps its vectors. Window 0 is in `vec_cache(model, sha, vec)`, keyed by `sections.sha`,
  exactly as before. Windows 1..n are in two tables beside it: `sec_window(model, sha, ix, wsha, rule)` lists a
  section's windows (row 0 marks the section split, with `wsha = sha`) and `win_vec(model, wsha, vec)` holds
  their vectors. Both are per model, since windows depend on the model's tokenizer. Splitting needs the
  tokenizer, so it runs in the embedder, not the indexer. A few pending windows are embedded on the query's
  thread; more go to a background thread, and answers say `vectors missing for N of M sections` (no window-0
  vector) or `windows pending: N windows, M sections not yet split` until it is done; meanwhile a section scores
  by the windows it has. `--embed` embeds everything now. Pruning drops `sec_window` rows of sections that are
  gone, window-0 vectors no section has, and window vectors no `sec_window` row names.
- **Older binaries.** The two new tables are additive caches (`CREATE TABLE IF NOT EXISTS`): the store's version
  does not change, so an older atlas keeps opening the same store. It reads, writes and prunes `vec_cache` only,
  which still holds window 0 of every section by `sections.sha`, so its searches are what they were and its
  prune never deletes a window vector.
- **Upgrading** costs one background pass over the long sections: on the maintainers' store, about 2,390 window
  vectors (about 1.22M tokens) beside the 8,521 window-0 vectors it already has. Measured on the 7,315-section eval
  snapshot (one machine, 8 encoder threads): 1,970 windows in 408 s on top of an embedded store; a cold
  `--embed` takes 825 s against 422 s before (window 0 as before, then the windows). The index holds 1.27x
  the rows (13.6 MB of vectors resident, from 10.7 MB; `win_vec` adds 3.0 MB to the store), and a reload of
  the vectors after a change takes about 615 ms against 465 ms. Search latency is unchanged within noise
  (`--judge` semantic median 80.5 ms against 81.7 ms, most of it the query's own encoding).
- **Search** is brute force over the in-memory matrix (one dot product per window with the same kernels, then
  each section's best), and `k` counts sections, not windows; at this corpus size an approximate index would cost
  more than it saves. The query side is unchanged: one query vector.
