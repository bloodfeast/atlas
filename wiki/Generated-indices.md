```
<!-- vault-mcp:index NAME start {"sources":[...],...} -->
...generated, rewritten on every build or refresh...
<!-- vault-mcp:index NAME end -->
```

Only the bytes from the start marker line to the end marker line are ever rewritten; text outside is
byte-identical afterwards. A missing note is created holding just the markers; missing markers are appended
after a blank line. The spec is stored in the start marker as compact JSON (`>` written as `\u003e` so it
cannot close the comment), so `refresh_index` and `refresh_all_indices` need no arguments. Output is
deterministic (natural sort, ties by vault and path), so an unchanged vault refreshes to identical bytes and
no write. Several named indices in one note are independent.

Spec fields:

| Field | Meaning |
|---|---|
| `sources` | `[{vault, folder, glob}]`, 1-16, may span vaults (rows in other vaults get `obsidian://` links) |
| `filter` | `[{key, value, prefix}]`, all must match; `key` may be `folder` or `tag`; case-insensitive |
| `group_by` | `{key, order, link, heading_level, label}`: `label` is `raw` (default, the value or `order` entry as written) or `title_case` (each word capitalised); `key` is a frontmatter key, `folder` or `tag`; a value joins the first `order` entry it starts with; `link` renders labels as `[[label]]` |
| `sort` | a frontmatter key, `file` (the file name, the default), `title` or `path` |
| `columns` | keys or `{key, header, link}`; pseudo-keys `link` (the note link), `title` (frontmatter `title`, else the file name), `file` (the file name), `path`, `vault`, `folder` |
| `format` | `table` (default), `list`, `counts` (group sizes and a total; needs `group_by`), or `count_inline`: just the number, with both markers on one line, so it can sit inside a sentence (`We track <!-- ... start ... -->243<!-- ... end --> ideas.`) |
| `link_text` | what the link shows: `title` (frontmatter `title`, falling back to the file name), `file` (always the file name), or any frontmatter key; default the file name. Same-vault links drop the folder when the file name is unique in the vault |
| `count_header` | header of the count column in `counts` |
| `rows` | `notes` (default), `discussions` or `decision_queue` (see [[Discussions]]); thread rows may give `"sources": []` for every vault |

## An ideas vault's Index.md as specs

A hand-written `Index.md` maps onto three regions. Test `readme_index_specs_render_on_a_copy_of_a_live_vault`
builds the counts and the full table on a temp copy of the vault named by `ATLAS_LIVE_IDEAS_VAULT` (skipped
when it is unset) and checks a row per idea, a total that covers every idea and an idempotent refresh.

**Counts** (`## Counts`):

```json
{"sources":[{"vault":"perf","folder":"Ideas","glob":"PI-*.md"}],
 "group_by":{"key":"status","order":["open","queued","in flight","done","parked"]},
 "format":"counts","count_header":"Ideas"}
```

**Areas** (`## Areas`), in the hand order:

```json
{"sources":[{"vault":"perf","folder":"Ideas","glob":"PI-*.md"}],
 "group_by":{"key":"area","link":true,"order":["Storage","GPU passes","Networking",
   "CPU","Tooling"]},
 "format":"counts","count_header":"Ideas"}
```

**All ideas** (`## All ideas`):

```json
{"sources":[{"vault":"perf","folder":"Ideas","glob":"PI-*.md"}],"sort":"id","link_text":"title",
 "columns":[{"key":"id","header":"ID"},{"key":"link","header":"Idea"},{"key":"status","header":"Status"},
            {"key":"area","header":"Area","link":true},{"key":"saving_median","header":"Saving"}]}
```

The Saving column then shows the frontmatter `saving_median`; the hand table shows each note's one-line
summary, which is not in the frontmatter. Adding a `summary` key to the ideas (one `set_frontmatter` each)
would make the two identical.

**Each area note** (`Areas/GPU passes.md`), grouped by status as today:

```json
{"sources":[{"vault":"perf","folder":"Ideas","glob":"PI-*.md"}],
 "filter":[{"key":"area","value":"GPU passes"}],
 "group_by":{"key":"status","order":["open","queued","in flight","done","parked"],"heading_level":2},
 "sort":"id","columns":[{"key":"link","header":"Idea"},{"key":"status","header":"Status"},{"key":"saving_median","header":"Saving"}]}
```

A cross-vault "everything parked" index is the same with sources in several vaults and
`"filter":[{"key":"status","value":"parked"}]`.
