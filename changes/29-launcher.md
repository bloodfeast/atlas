---
bump: patch
section: fix
---
- **`atlas` started through a link now finds its binary.** A link to the plugin's `bin/atlas`, such as
  `~/.local/bin/atlas`, said the release had no prebuilt binary for the machine: the launcher looked beside the
  link. It now follows the link first.
