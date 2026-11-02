Obsidian wikilinks resolve only inside one vault. Between vaults the link is an Obsidian URI, in the form
the Obsidian docs give (`help.obsidian.md`, "Obsidian URI", action `open`):

```
[PI-002 Throttle](obsidian://open?vault=Research%20Log&file=Ideas%2FPI-002%20Throttle)
[PI-002 Throttle > History](obsidian://open?vault=Research%20Log&file=Ideas%2FPI-002%20Throttle%23History)
```

- `vault` is the vault's **folder name** (Obsidian's vault name); it is read from the root at launch, never
  assumed (test `vault_name_is_the_folder_name_not_an_assumed_one` serves a renamed copy).
- `file` is the path from the vault root without `.md`, percent-encoded (`/` is `%2F`, space `%20`); a
  heading rides on it as `%23Heading`. Everything but `A-Z a-z 0-9 - _ . ~` is encoded, parentheses
  included, so the URI is safe inside a Markdown link.
- The parser also reads `obsidian://vault/<vault>/<file>` and `obsidian://open?path=<absolute path>`.

Resolution follows Obsidian: case-insensitive; a bare `[[Name]]` matches by file name, same folder first,
then the shortest path; `[[a/b]]` from the root, then relative, then as a path suffix; Markdown links
relative to the note, then to the root; image, PDF and other attachment targets are recorded but not checked.
Links inside fenced code and inline code are ignored.
