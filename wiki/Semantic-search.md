`search` mode `semantic` ranks every section by the cosine of its embedding with the query's; `hybrid` fuses
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
- **Vectors** are cached in `vec_cache` keyed by the model and the SHA-256 of the section's text
  (`sections.sha`), so a re-index re-embeds only changed sections and a rebuilt database keeps its vectors.
  A few missing sections are embedded on the query's thread; a cold corpus goes to a background thread and
  answers say `vectors missing for N of M sections` until it is done. `--embed` embeds everything now.
- **Search** is brute force over the in-memory matrix (one dot product per section with the same kernels);
  at this corpus size an approximate index would cost more than it saves.
