# Changelog fragments

Every pull request that changes what a user gets adds a fragment here. Merged fragments wait in this folder
until a release is cut: `cargo xtask version` folds them into a new `## vX.Y.Z` section of `CHANGELOG.md`,
computes the version from them, and deletes them in the same commit. No pull request title or branch carries
a version.

## A fragment

`changes/<pr>.md`, named for its own pull request's number. A pull request with lines for more than one section
adds one file per section: `changes/<pr>-<slug>.md`, the slug in lowercase letters, digits and `-`.

```markdown
---
bump: feature
section: feature
---
- **What a user notices, in bold.** Then what it does for them, with measured numbers where there are any,
  in the CHANGELOG's voice: written for users, not about how the work was organised.
```

The file is UTF-8 with no byte order mark. The front matter holds two keys, each once, and nothing else:

- `bump`: which part of the version, Release.Feature.Patch, the change raises.
  - `breaking`: a user must change something (a removed or renamed tool, setting or flag, a changed default
    they relied on, a store they must rebuild). Forces a release bump.
  - `feature`: feature work.
  - `patch`: a change to an existing feature: a fix, performance or correctness.
- `section`: where the lines go in the release's section.

| `section:`    | CHANGELOG heading         |
|---------------|---------------------------|
| `highlight`   | `### Highlights`          |
| `breaking`    | `### Breaking changes`    |
| `upgrade`     | `### Upgrade notes`       |
| `feature`     | `### Features`            |
| `fix`         | `### Fixes`               |
| `security`    | `### Security hardening`  |
| `known-limit` | `### Known limits`        |

`section: breaking` needs `bump: breaking`.

The body is a Markdown list: its first line that is not blank starts with `- `. It holds no headings outside a
fenced code block: no line starting with `#`, no `#` heading after spaces, tabs or a list or quote marker (`- ## x`,
`  # x`, `1. # x`), and no setext underline (a line of only `=` or only `-`, after any spaces or tabs). A file
holds no control character but a tab and its line endings (a carriage return only directly before a line feed):
GitHub reads a lone carriage return as a line break. A fenced block opens on a line of up to three spaces and three or
more backticks or tildes, closes on a line of the same character at least as long, and must be closed (four
spaces or a tab before the backticks make no fence). Every line of a block, its closing fence included, is
indented at least as far as its opening fence (blank lines aside): on GitHub a shorter line ends the list item
the block sits in, and the block with it. No line outside a fenced block starts with `<` (after any spaces or
tabs), HTML comments included: GitHub reads it as HTML, which can hide the rest of the release notes.
Continuation lines are indented two spaces, and list items follow each other with no blank line between them, as
in `CHANGELOG.md` (the fold drops any).

## No user-facing change

A pull request that changes only docs, CI or tests, but touches a path the build reads, adds an empty
`changes/<pr>.none` instead. A pull request that touches no build path needs neither.

## The checks

- **dev-pr** (`.github/workflows/dev-pr.yml`, its `changes` job, through `scripts/changes-check.sh`): a pull
  request that touches a build path (`Cargo.*`, `rust-toolchain.toml`, `build.rs`, `src/`, `xtask/`,
  `tests/`, `benches/`, `examples/`, `config/`, `hooks/`, `vocab/`, `.cargo/`, `.github/`) fails without
  `changes/<pr>.md`, `changes/<pr>-<slug>.md` or `changes/<pr>.none`; having both a fragment and a marker
  fails too. Every fragment a pull request adds or changes is checked against the format above, and every
  file it adds or changes here must be named in one of those forms or be this README. The `cargo xtask
  version` commit, which removes other pull requests' fragments and writes `CHANGELOG.md` and `Cargo.toml`,
  needs none of its own; removing fragments alone does not count. The check checks out only this folder and
  the script, and builds nothing. The dev-pr workflow is development-only and is not exported.
- **`cargo xtask version`** parses every fragment by the same rules (`xtask/src/version.rs`; its tests run
  both on the same cases), and refuses a malformed one, a stray file, a fragment git does not track (an
  ignored one would be folded and left behind) or a folder with no fragments.

## Cutting a release

```sh
cargo xtask version --dry-run   # the steps, the next version and its CHANGELOG section; changes nothing
cargo xtask version             # one commit: the section, the version, the fragments deleted
cargo xtask version --release   # the same, with the release number raised by hand: X.Y.Z to (X+1).0.0
cargo xtask version --set 0.6.0 # the same, at the version named: the one-off override
```

On a clean checkout, `cargo xtask version`:

1. replays the fragments in merge order from `Cargo.toml`'s version, one step each (the version rule below);
   with `--release` the version is (X+1).0.0 instead, whatever the fragments say, and with `--set X.Y.Z` it is
   X.Y.Z (see the override below);
2. writes one section for the release at the top of `CHANGELOG.md`, however many steps it took, its parts in
   the header's order, each part's lines by pull request number, and refuses it when no fragment is
   `section: highlight` (the release notes lead with the Highlights; write one for the release in its own
   fragment if no pull request had one);
3. writes the version into `Cargo.toml`, `Cargo.lock` (atlas's own `[[package]]` entry) and
   `.claude-plugin/plugin.json` (Claude Code refreshes an installed plugin only when its version changes);
4. deletes every fragment and marker in this folder (not this README) and commits all of it. If any step
   fails, every file is put back as it was and nothing is committed.

Merge that commit through a pull request, then `cargo xtask release ship vX.Y.Z` as before
(`wiki/Releasing.md`). `release ship` refuses to start while fragments are pending: any file here named as a
fragment counts, a stray file does not. Neither command reads a tag.

### The version rule

The version is Release.Feature.Patch, by the same rule above and below 1.0.0. Each fragment is one step, in
merge order, and each part it raises resets the parts below it:

| Fragment's `bump:` | 0.5.3 becomes | 1.2.3 becomes |
|--------------------|---------------|---------------|
| `patch`            | 0.5.4         | 1.2.4         |
| `feature`          | 0.6.0         | 1.3.0         |
| `breaking`         | 1.0.0         | 2.0.0         |

So 0.6.0 with patch, patch, feature, patch is 0.7.1, and 0.5.0 with fifteen patches and then a feature is
0.6.0: the patches before the feature leave no trace in the number, though their lines are in the 0.6.0
section. Otherwise the release number goes up only by hand, with `--release`. The tag is `vX.Y.Z`.

Merge order is the order of the commits on the first-parent history of the checkout that added each
fragment: the merge commit of a merged pull request, or the commit itself for a squash or a rebase. Fragments
one commit added go by pull request number, then name.

### The one-off override

`cargo xtask version --set X.Y.Z` folds every pending fragment into one `## vX.Y.Z` section and writes X.Y.Z
into `Cargo.toml`, `Cargo.lock` and `plugin.json`, ignoring the replay. It refuses a version that is not
strictly above `Cargo.toml`'s, and refuses `--set` together with `--release`; everything else is as above (the
Highlights refusal, the untracked-fragment refusal, the rollback). It is for a release whose number is
decided by hand, not the usual way to cut one. Its first use is v0.6.0, for the batch since v0.5.0; the
replay counts from there.
