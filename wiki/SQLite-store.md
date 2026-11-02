`state/atlas.sqlite` in the data folder, or the config's `db` (outside every vault so Obsidian never sees it).

| Table | Rebuildable | Contents |
|---|---|---|
| `notes` | yes | vault, path, mtime, size, sha256, title (file stem), frontmatter as JSON |
| `links` | yes | from note, line, kind (`wiki` `embed` `md` `uri`), raw text, target, heading, resolved vault and path |
| `tags` | yes | frontmatter `tags` and inline `#tags`, lowercased |
| `fts` | yes | FTS5 over title and body |
| `ids` | re-derived | last number per sequence; the folder's file names are always checked too |
| `indices` | re-derived | name, note, spec, hash of the last output |
| `writes` | **no** | the audit log (with each write's reason), mirrored line for line to `state/writes.jsonl`; `reason` and `discussion` were added by in-place migrations, so older databases keep their rows (old rows have them empty) |
| `meta` | yes | schema version and vault roots |

Deleting the database loses nothing: the next start rebuilds the derived tables (test
`deleting_the_database_rebuilds_the_same_rows` compares every row), and the audit history stays in
`state/writes.jsonl`: one JSON line per write (time, tool, vault, path, sha before and after, client, pid, reason),
appended and fsynced under the write lock in the same step as the `writes` row. The file is only ever
opened for append; no rescan or rebuild truncates or rewrites it (test
`audit_jsonl_survives_a_rebuild_and_keeps_growing`). A rebuilt database starts an empty `writes` table. WAL mode, `busy_timeout` 30 s,
`BEGIN IMMEDIATE` for every write; every server process opens the same file. A write takes the lock, writes
the file atomically, updates that note's rows and appends the audit row, then commits; a crash between the
file and the commit is healed by the next rescan.

**Freshness.** Obsidian and plain-file agents edit the vaults too, so every tool call starts with a rescan:
walk the vaults, compare each note's mtime and size with its row, read and hash only what differs, re-parse
only what hashes differently. Notes gone from disk leave `notes`, `links`, `tags` and `fts`, never `writes`.
When the set of note paths changes, every link is resolved again (a new note can satisfy a broken link).

Measured on the development machine (release build, six vaults, 351 notes; a GPU bench was running at
the time, so treat these as upper figures):

| Rescan | Time |
|---|---|
| full rebuild from an empty database | 147-172 ms (5 runs) |
| per-call rescan, nothing changed | 2.2-2.8 ms (5 runs) |

These per-call rescans are what the vault tools (`read_note`, `search` over notes, the write tools) run.
The atlas tools (`lookup`, `search` in sections mode, `section`, `grep`) check freshness against the change
watcher instead; see [[Identifiers and freshness]].
