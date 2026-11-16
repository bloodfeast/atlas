---
section: upgrade
---
- **The first search after upgrading embeds the windows of long sections in the background.** On the
  maintainers' store that is about 2,390 window vectors (about 1.22M tokens) beside the 8,521 section vectors
  already there, which are kept. Until it is done, answers note `windows pending: N windows, M sections not yet
  split` and each section scores by the windows it has. On a 7,315-section store, 1,970 windows took 408 s on 8
  encoder threads. The store's version does not change: an older atlas on the same store keeps working and never
  deletes the new window vectors.
