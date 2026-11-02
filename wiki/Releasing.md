How a release of atlas is made. Users need none of this; it is here so the process is public and checkable.

## The changelog rule

Every release gets a user-facing section in `CHANGELOG.md`, `## vX.Y.Z`, written for users: what changed for
them, not how the work was organised. Its parts, in order, each left out when empty:

- `### Highlights`: what a user notices, with measured numbers where there are any. Required.
- `### Breaking changes` and `### Upgrade notes`: what a user must do when updating.
- `### Features`, `### Security hardening`, `### Known limits`: everything else.

`cargo xtask release` refuses a version whose section is missing, empty (blank lines, headings and HTML comments
only) or has no `### Highlights` with text, before anything is built. The section is the source of three things:
the release notes, the release commit's message, and this wiki's [[Changelog]] page.

## Release notes

`scripts/release-notes.md.in` is filled from the section and the build, in this order: the version and the
release date; the highlights; the note that Obsidian is not required; breaking changes and upgrade notes when
there are any; install; the prebuilt binaries with their SHA-256; the model files with theirs; everything else
from the section in a collapsed block; and a link to the full changelog on the wiki. A placeholder alone on its
line (`@HIGHLIGHTS@`, `@UPGRADE@`, `@DETAILS@`, `@BINARIES@`, `@MISSING@`, `@MODELS@`, `@CHANGES@` for the whole
section) is replaced by its block, and an empty block takes its line with it; `@VERSION@`, `@DATE@` and `@REPO@`
are replaced inline. `ATLAS_RELEASE_DATE` sets the date (default today, UTC).

## The release

`cargo xtask release` makes a release of the public repository: it builds the binaries this machine can
build (Windows natively and Linux x86_64 inside WSL on Windows; Linux natively on Linux; both macOS targets
on a Mac with `--build-only`, copied to the publishing machine's prebuilt folder), runs the tests, exports the
tracked tree with `git archive`, checks every exported file and binary against the private denylist named by
`ATLAS_LEAK_DENYLIST`, and commits it as one squashed commit per version, tagged, with the release notes as its
message. Nothing of the development history is exported. The export must carry `.github/workflows/ci.yml`,
`.github/CODEOWNERS`, `SECURITY.md`, `LICENSE`, `NOTICE`, `README.md`, `CHANGELOG.md` and `wiki/Home.md`, or the
release stops. A dry run points `--remote` at a local bare repository, and `--dry-run` does everything but push
and sign (it ignores `ATLAS_SIGN_KEY` and refuses `--push`); the doc comment of `xtask/src/release.rs` lists
every option.

```sh
cargo xtask release --build-only                                     # binaries only (on a Mac, for macOS)
cargo xtask release --remote PATH/pub.git --dry-run                  # everything but push and sign
ATLAS_PUBLISH=vX.Y.Z cargo xtask release --remote URL --push         # the release
```

Every release also stages `release/`: `manifest.json` (the version and, per target, the file, its download URL,
SHA-256 and size), `manifest.json.sig` and the binaries, uploaded as the release's assets. The signature is made
with `openssl pkeyutl -sign -rawin` from the key named by `ATLAS_SIGN_KEY` (an ed25519 PEM kept offline; the
release never creates or prints a private key). It refuses to publish without it, with a key whose public
half is not the one built into atlas (`TRUSTED_KEY_HEX` in `src/update.rs`), or while that constant is still the
test key. Release builds are compiled with `ATLAS_RELEASE_BUILD` set; only they check for updates.

## The wiki

This wiki is written in the repository's `wiki/` folder and published by the release, one commit per release, to
`<public repo>.wiki.git` (the `--remote` URL with `.git` replaced by `.wiki.git`):

1. The pages are taken from the exported tree (never the working tree), and the [[Changelog]] page is generated
   from `CHANGELOG.md`. A `Changelog.md` committed under `wiki/` is refused, as is a subfolder (GitHub wikis are
   flat) or a missing `Home.md`.
2. The pages get the same leak check as the export.
3. The wiki repository is cloned. GitHub creates it only when the first page is made in the web UI; until then
   the release stops and says so. Create any page once at `https://github.com/<public repo>/wiki` and run the
   release again. The release never creates the wiki repository any other way.
4. The wiki's pages are replaced by the release's and committed under the public identity as `atlas vX.Y.Z
   wiki`. A release that changes no page makes no commit.
5. With `--push` the commit is pushed after the release commit and tag, to the branch the wiki's `HEAD` names
   (`master` on GitHub), read with `git ls-remote --symref` rather than assumed. Without it, and always with
   `--dry-run`, the release prints which pages would change.

`--build-only` skips the wiki. `tests/docs.rs` keeps the pages honest: every `[[link]]` and relative link
resolves to a page, every page is in the sidebar, the README's documentation links name existing pages, and
every tool and configuration key in the code is documented on some page.

## The public-repository guard

GitHub keeps a redirect from a repository's old name to its new one, so a public name can still lead to a
private repository. Before any push, of the release commit, its tag or the wiki, the release asks GitHub (`gh api`,
as the logged-in owner) about `ATLAS_PUBLIC_REPO` and refuses unless:

- the `--remote` URL and the wiki remote name that repository;
- its `full_name` is that name exactly (case aside), so it is not a redirect, and it is not the development
  repository (this checkout's `origin`);
- it is public, and its owner is a user account, not an organization;
- every ruleset's bypass list is empty or exactly the repository admin role;
- no collaborator but the owner has admin permission;
- an active branch ruleset covers `main` with the `deletion`, `non_fast_forward`, `required_linear_history`
  and `pull_request` rules, and an active tag ruleset covers `refs/tags/v*` with `deletion`, `update` and
  `creation` (the two rulesets below).

Before that, and before anything is built, the release checks that the README, `SECURITY.md`, the CI
workflow's gate and every wiki page name `ATLAS_PUBLIC_REPO` wherever they name the public repository (wiki
and security links, the marketplace command), and refuses a mismatch.

A refusal names what it found, for example `bloodfeast/atlas resolves to bloodfeast/atlas-dev (private): create
the public repo first`. A push to GitHub stops on it; `--dry-run` and a local `--remote` report it and go on.
The tests use saved answers (`ATLAS_GITHUB_STUB`) and never reach the network.

## The public repository's setup

Applied by hand with `gh` before the first release; the guard checks the parts that decide who can bypass
what.

**Ruleset `main`** (the default branch):

- no deletion, no force-push, linear history;
- pull requests required, with 1 approving review from a code owner (`.github/CODEOWNERS`), stale approvals
  dismissed on a new push, approval of the most recent push, and every conversation resolved;
- squash merge only;
- the CI checks required: `test (ubuntu-latest)`, `test (windows-latest)` and `test (macos-latest)`;
- bypass: the admin role only.

**Ruleset `tags`** (`v*`):

- no update, no deletion, creation restricted;
- bypass: the admin role only, so only the owner's release can make a tag.

**Settings:**

- secret scanning with push protection;
- private vulnerability reporting (see `SECURITY.md`);
- Dependabot alerts;
- Actions: the default token read-only, and approval required for workflows from outside contributors;
- the wiki editable only by people with push access;
- squash merge the only merge method.

## macOS

On macOS, `cargo xtask verify-macos` runs the whole suite in release with the `dev` feature (the FSEvents
watcher test included), then the recall check and the bench on `fixtures/generic`, a small corpus shipped for
that purpose (its database goes to a temporary folder). Paste its whole output back.
