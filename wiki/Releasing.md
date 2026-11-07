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

## Clippy

Warnings fail a release: `cargo +1.99.0 clippy --workspace --locked --all-targets --features atlas/dev -- -D warnings`
and the same with default targets (`cargo +1.99.0 clippy --workspace --locked -- -D warnings`). The manual `cargo xtask
release` runs both before the tests (`--skip-tests` skips them too), and the public repository's CI runs them in its `clippy` job. The toolchain is
pinned so that a new Rust release's lints cannot fail an unrelated release; `rust-toolchain.toml` stays `stable`. To
bump it, change `CLIPPY_TOOLCHAIN` in `xtask/src/release.rs` and every `1.99.0` in `.github/workflows/ci.yml`, and
clear what the new lints find.

## The release

A release goes through three hands: the development repository opens a pull request, the public repository
builds, and the owner checks, signs and publishes. Nothing of the development history is exported, and the
release signing key never leaves the owner's machine.

1. **The pull request** (development repository, workflow `release-pr`). Started by hand with the version, or by
   pushing a `vX.Y.Z` tag there. `cargo xtask release --pr` makes the early checks (the changelog section with
   its highlights, `Cargo.toml` and `plugin.json` on the same version, the links naming the public repository,
   the public-repository guard as far as the release app can see it), exports the tracked tree with `git
   archive`, and checks every exported file and every wiki page against the private denylist. The denylist
   comes from a secret, written to a temporary file that only the job can read and removed after it; every term
   is masked in the log and none is ever printed. It then pushes branch `release/vX.Y.Z` to the public
   repository, the exported tree on top of its `main` with `bin/` left as it is, and opens a pull request into
   `main` whose body is the release notes. No tests run and nothing is built there: the public repository's CI
   tests the pull request, and its `clippy` job is the clippy check for this path.
2. **Review and merge** (public repository). The pull request comes from the release GitHub App, so the owner
   can give it the code-owner approval the `main` ruleset requires. The CI checks run on it; squash merge it.
3. **The build** (public repository, workflow `release`). A push to `main` that changes `Cargo.toml` to a version
   with no tag builds the five binaries on GitHub's hosted runners: Windows x86_64 on Windows, Linux x86_64 and
   aarch64 inside `debian:bullseye` (glibc 2.31) on x86_64 and Arm runners, and both macOS targets on an Apple
   Silicon runner. Each Linux job checks that the binary needs no glibc symbol newer than 2.31. The builds use
   the release's environment: `--locked`, symbols stripped, `ATLAS_RELEASE_BUILD` set (only release builds check
   for updates), and every build-machine path remapped (`/PDBALTPATH` too on Windows). `cargo xtask release
   assets` then writes `manifest.json` and the notes with each binary's SHA-256, and the job creates a **draft**
   release `vX.Y.Z` on the merge commit with the notes, the binaries and the manifest. A draft makes no tag; the
   job checks that none appeared.
4. **Sign and publish** (the owner's machine). `cargo xtask release sign vX.Y.Z` downloads the draft's manifest
   and binaries, checks each file's size and SHA-256 against the manifest (and the manifest's version, targets,
   names and download URLs), checks every downloaded file against the local denylist, and makes the full
   public-repository guard as the owner. It then commits the binaries to `main` as `bin/` beside the launcher
   (the plugin install is a git clone of `main`), signs the manifest, uploads `manifest.json.sig`, pushes `main`
   and the wiki, and publishes the draft on the binaries commit, which creates the tag. It refuses on any
   mismatch and names each one: a hash, a size, an asset the manifest lists that the draft lacks, a file the
   manifest does not list, a leak, or a `main` that moved past the draft's commit. A run stopped after the
   binaries commit was pushed can be run again: it finds that commit and goes on.

```sh
ATLAS_LEAK_DENYLIST=... ATLAS_SIGN_KEY=... cargo xtask release sign vX.Y.Z --dry-run   # every check, nothing out
ATLAS_LEAK_DENYLIST=... ATLAS_SIGN_KEY=... ATLAS_PUBLISH=vX.Y.Z cargo xtask release sign vX.Y.Z
```

Pushing to anything that is not a local folder needs `ATLAS_PUBLISH=vX.Y.Z`, in `--pr` (the workflow sets it)
and in `sign`, so a publish is always a deliberate act. The dry path runs both against local bare repositories:
`cargo xtask release --pr --remote PATH/pub.git` pushes the branch there and opens no pull request, and `sign`
with `--remote PATH/pub.git` and `ATLAS_GITHUB_STUB` reads the draft from a folder. The doc comment of
`xtask/src/release_ci.rs` lists every option.

### Signing

`manifest.json` holds the version and, per target, the file, its download URL, SHA-256 and size. One writer
makes it for both paths below, in the format atlas's updater reads. The signature, `manifest.json.sig`, is made
with `openssl pkeyutl -sign -rawin` from the key named by `ATLAS_SIGN_KEY` (an ed25519 PEM kept offline; openssl
asks for its passphrase; nothing ever creates or prints a private key). Signing refuses a key whose public half is
not the one built into atlas (`TRUSTED_KEY_HEX` in `src/update.rs`), and refuses to publish while that constant is
still the test key.

### The manual release

`cargo xtask release` without `--pr` is the fallback when CI cannot be used: it builds the binaries this machine
can build (Windows natively and Linux x86_64 inside WSL on Windows; Linux natively on Linux; both macOS targets on
a Mac with `--build-only`, copied to the publishing machine's prebuilt folder), runs the tests, exports, checks the
export and the binaries against the denylist, signs, and commits one squashed commit per version straight to the
public `main` with its tag, under the owner's ruleset bypass. Upload `release/` (the binaries, the manifest and
its signature) to the GitHub release by hand afterwards.

```sh
cargo xtask release --build-only                                     # binaries only (on a Mac, for macOS)
cargo xtask release --remote PATH/pub.git --dry-run                  # everything but push and sign
ATLAS_PUBLISH=vX.Y.Z cargo xtask release --remote URL --push         # the release
```

Both paths refuse an export without `.github/workflows/ci.yml`, `.github/workflows/release.yml`,
`.github/CODEOWNERS`, `SECURITY.md`, `LICENSE`, `NOTICE`, `README.md`, `CHANGELOG.md` and `wiki/Home.md`. Every
address in the public history must be the public one, or GitHub's own on the commits a merged pull request makes.

## The wiki

This wiki is written in the repository's `wiki/` folder and published once per release to `<public
repo>.wiki.git`: by `release sign` from the public `main`, or by the manual release from its export.

1. The pages are taken from the exported tree (never the working tree), and the [[Changelog]] page is generated
   from `CHANGELOG.md`. A `Changelog.md` committed under `wiki/` is refused, as is a subfolder (GitHub wikis are
   flat) or a missing `Home.md`.
2. The pages get the same leak check as the export.
3. The wiki repository is cloned. GitHub creates it only when the first page is made in the web UI; until then
   the release stops and says so. Create any page once at `https://github.com/<public repo>/wiki` and run the
   release again. The release never creates the wiki repository any other way.
4. The wiki's pages are replaced by the release's and committed under the public identity as `atlas vX.Y.Z
   wiki`. A release that changes no page makes no commit.
5. The commit is pushed to the branch the wiki's `HEAD` names (`master` on GitHub), read with `git ls-remote
   --symref` rather than assumed. A dry run prints which pages would change.

`tests/docs.rs` keeps the pages honest: every `[[link]]` and relative link resolves to a page, every page is in
the sidebar, the README's documentation links name existing pages, and every tool and configuration key in the
code is documented on some page.

## The public-repository guard

GitHub keeps a redirect from a repository's old name to its new one, so a public name can still lead to a
private repository. Before anything is pushed the release asks GitHub (`gh api`) about `ATLAS_PUBLIC_REPO` and
refuses unless:

- the remote and the wiki remote name that repository;
- its `full_name` is that name exactly (case aside), so it is not a redirect, and it is not the development
  repository (this checkout's `origin`);
- it is public, and its owner is a user account, not an organization;
- every ruleset's bypass list is empty or exactly the repository admin role;
- no collaborator but the owner has admin permission;
- an active branch ruleset covers `main` with the `deletion`, `non_fast_forward`, `required_linear_history`
  and `pull_request` rules, and an active tag ruleset covers `refs/tags/v*` with `deletion`, `update` and
  `creation` (the two rulesets below).

GitHub shows a ruleset's bypass list, and may show the collaborator list, only to an admin. The release app is
not one, so `--pr` reports those two parts as unseen and checks the rest; `release sign` and the manual release
run as the owner and check everything before anything is published.

Before that, and before anything is built, the release checks that the README, `SECURITY.md`, the workflows'
gates and every wiki page name `ATLAS_PUBLIC_REPO` wherever they name the public repository (wiki and security
links, the marketplace command), and refuses a mismatch.

A refusal names what it found, for example `bloodfeast/atlas resolves to bloodfeast/atlas-dev (private): create
the public repo first`. A push to GitHub stops on it; a dry run and a local remote report it and go on. The tests
use saved answers (`ATLAS_GITHUB_STUB`) and never reach the network.

## The public repository's setup

Applied by hand before the first release; the guard checks the parts that decide who can bypass what.

**Ruleset `main`** (the default branch):

- no deletion, no force-push, linear history;
- pull requests required, with 1 approving review from a code owner (`.github/CODEOWNERS`), stale approvals
  dismissed on a new push, approval of the most recent push, and every conversation resolved;
- squash merge only;
- the CI checks required: `test (ubuntu-latest)`, `test (windows-latest)` and `test (macos-latest)` (GitHub
  offers a check for this list once it has run, so add them after the first release pull request);
- bypass: the admin role only.

**Ruleset `tags`** (`v*`):

- no update, no deletion, creation restricted;
- bypass: the admin role only, so only the owner's publish can make a tag.

**The release app** (a GitHub App owned by the owner's account):

- repository permissions: Contents read and write, Pull requests read and write, Workflows read and write
  (GitHub refuses an app's push that changes `.github/workflows/` without it), Metadata read; nothing else, no
  webhook;
- installed on the public repository only;
- in no ruleset's bypass list: it pushes only `release/*` branches, which no ruleset covers, and its pull
  requests need the owner's approval like anyone's;
- its Client ID and a private key are the development repository's secrets `ATLAS_RELEASE_APP_ID` and
  `ATLAS_RELEASE_APP_KEY`, beside `ATLAS_LEAK_DENYLIST` (the denylist's text).

**Settings:**

- secret scanning with push protection;
- private vulnerability reporting (see `SECURITY.md`);
- Dependabot alerts;
- Actions: the default token read-only (the release workflow's draft job asks for `contents: write` itself), and
  approval required for workflows from outside contributors;
- the wiki editable only by people with push access;
- squash merge the only merge method.

## macOS

On macOS, `cargo xtask verify-macos` runs the whole suite in release with the `dev` feature (the FSEvents
watcher test included), then the recall check and the bench on `fixtures/generic`, a small corpus shipped for
that purpose (its database goes to a temporary folder). Paste its whole output back.
