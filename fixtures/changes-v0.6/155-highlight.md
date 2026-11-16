---
bump: feature
section: highlight
---
- **Semantic search reads the whole of a long section.** A section longer than the encoder's 510 tokens was
  embedded by its head only; it is now embedded as overlapping windows (510 tokens, a stride of 384) and scores
  by its best window, so a question answered deep in a long section can find it. On 30 held-out questions
  answered past token 620 of a long section, semantic hit@5 rose from 7 to 18 and hybrid from 18 to 24; on the
  15 held-out control questions semantic went from 13 to 14 and hybrid from 14 to 15, and the memory eval is
  unchanged.
