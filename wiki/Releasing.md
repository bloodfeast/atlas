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
and the same with default targets (`cargo +1.99.0 clippy --workspace --locked -- -D warnings`). The public
repository's CI runs them in its `clippy` job on every pull request, the release command refuses to merge until
both pass, the development repository's `dev-pr` workflow runs them on its pull requests, and the local rehearsal
(below) runs them before its tests. The toolchain is pinned so that a new Rust release's lints cannot fail an
unrelated release; `rust-toolchain.toml` pins the same version, so the shipped binary is built by it too. To bump
it, change `channel` in `rust-toolchain.toml`, `CLIPPY_TOOLCHAIN` in `xtask/src/release.rs` and every `1.99.0` in
`.github/workflows/ci.yml` and `.github/workflows/dev-pr.yml`, and clear what the new lints find.

## The release

A release goes through three hands: the development repository opens a pull request, the public repository
builds, and the owner approves, signs and publishes. Nothing of the development history is exported, and the
release signing key never leaves the owner's machine. The owner drives it with one command, run twice at a
terminal, with his approval on GitHub between the two runs:

```sh
cargo xtask release ship vX.Y.Z   # run 1: opens the release pull request
# review and approve the pull request on GitHub, on its head commit
cargo xtask release ship vX.Y.Z   # run 2: merges, waits for the build, signs and publishes
```

Each run reads what GitHub holds for the version and takes the next step, so a run that stopped anywhere is
simply run again. It asks before each public act, at a terminal, for a typed answer that names the act and the
version: `open vX.Y.Z` before the export goes to the public repository, `merge vX.Y.Z` before the merge, and
`publish vX.Y.Z` once before the signature, the upload, the pushes and the publish. An answer for one act never
passes another. Without a terminal every one is refused and nothing changes, so no script or agent can open,
merge or publish a release; no environment variable or flag skips a prompt.

**Run 1** (no pull request yet):

1. The checkout guard (below) at the development repository's `main`, and its `origin` must be
   `github.com/bloodfeast/atlas-dev` as git really fetches and pushes (`insteadOf` and a push URL applied).
   `Cargo.toml` must be the version asked for. A draft release of the version already on the public repository
   stops it: the build would refuse to make another.
2. The binary leak check: unless a pass stamp stands for this commit, denylist and accepted-hits file, `release
   check` (below) runs first. The plan says so before it starts: three release builds, tens of minutes.
3. The plan, then `open vX.Y.Z`. The command records the commit, a random nonce and the run in its stage folder
   (`<stage>/vX.Y.Z/ship/state.json`), and starts the `release-pr` workflow with the version, that commit and the
   nonce. It finds the run by the nonce in its name, watches it to the end, and prints the pull request. A failed
   run is reported by its page only, never its log.

A run already in progress is watched, never started twice. After a failed run, the next run 1 starts it again
only after a new `open` answer; the release branch may already be on the public repository, and the new run
replaces it. The same holds once the pull request is open: when `main` has moved on since run 1, or the last
run that was meant to update the pull request failed or never showed, ship re-exports (run 2, step 2). A pull
request closed without a merge gives way to run 1 again once its branch is gone from the public repository, or
holds another commit than the closed pull request's head; while the branch still holds that head, ship refuses
and says to delete the branch by hand, then run ship again.

**The pull request** (development repository, workflow `release-pr`). It checks out the commit ship sent (the
tag's commit for a tag push) and refuses one that is not on `main`. `cargo xtask release --pr` makes the early
checks (the changelog section with its highlights, `Cargo.toml` and `plugin.json` on the same version, the links
naming the public repository, the public-repository guard as far as the release app can see it), exports the
tracked tree with `git archive`, and checks every exported file and every wiki page against the private denylist.
Before that job runs, job `linux-leak` builds the Linux x86_64 binary exactly as the public `release` workflow's
`linux` job does (in `debian:bullseye`, in the release environment, with the same remaps), and runs the binary
leak check (accepted hits honoured) and the secret scan on it; it uploads nothing, and a hit stops the release
before anything reaches the public repository. The denylist comes from a secret, written to a temporary file that
only the job can read and removed after it; every term is masked in the log and none is ever printed. It then
pushes branch `release/vX.Y.Z` to the public repository, the exported tree on top of its `main` with `bin/` left
as it is, and opens a pull request into `main` whose body is the release notes. No tests run and nothing else is
built there: the public repository's CI tests the pull request.

**Review** (public repository). The pull request comes from the release GitHub App, so the owner can give it
the code-owner approval the `main` ruleset requires. Approve it on its head commit once the CI checks pass; do
not merge it on GitHub: run 2 merges it.

**Run 2** (the pull request open):

1. The pull request must be the release's: open, from `release/vX.Y.Z` of the public repository itself (not a
   fork) into `main`, opened by the release app, titled `atlas vX.Y.Z`.
2. `HEAD` must be the commit run 1 sent, and that run must have passed (when this checkout holds run 1's
   record; without it the checks below stand alone, and the command says so). If not, ship **re-exports**
   instead of merging: run 1's steps again for `HEAD`, which must be the tip of `main` (a re-export never goes
   back to an older commit), with the binary leak check unless a stamp stands, the plan naming the pull request,
   and a new `open vX.Y.Z` whose prompt names the pull request's number, its current head and `HEAD`. The new
   `release-pr` run replaces the release branch and updates the open pull request, so its head moves; ship
   watches the run and prints the pull request. An approval of the old head does not count: review the pull
   request again, approve it on its new head, and run ship again. A run still in progress is watched, never
   dispatched again; a failed one is dispatched again only after another `open` answer. Without an answer nothing
   is dispatched and the pull request does not change (a `release check` run before the prompt may have left its
   stamp). Otherwise the checkout guard with `HEAD` on `main`.
3. The owner's latest review must be an approval of the pull request's head commit (an approval of an earlier
   push does not count), and no one else's latest review may ask for changes. Each reviewer's latest is read from
   the pull request's `reviews` by time, comments aside, as GitHub counts it (gh's `latestReviews` leaves the
   commit out, so it cannot tell which head was approved), and every check must have passed, these five among
   them: `test (ubuntu-latest)`, `test (windows-latest)`, `test (macos-latest)`, `clippy (windows-latest)`,
   `clippy (ubuntu-latest)`. Anything missing is listed, nothing changes, and the run ends; run it again later.
   The command checks this itself because the owner, as an admin, could merge past the ruleset on GitHub.
4. The full public-repository guard, then a fresh clone of the public repository: the checkout's `HEAD` must hold
   exactly what the pull request's head holds (export-ignored files and `bin/` aside), so the approval was of
   this code.
5. `merge vX.Y.Z`, then a squash merge pinned to the approved head commit (GitHub refuses it if the branch moved),
   with the release commit's own subject and message, so GitHub's default text never decides what enters the public
   history. GitHub picks the squash commit's author (it refused the public address given as `--author-email`), and
   ship checks every address after the merge. Never with the admin bypass. The branch is kept.
6. Every author, committer and `Co-authored-by:` address of the public history, merge commit included, must be
   the public one or GitHub's own (the address check, below); a hit stops everything and names the merge commit and
   how many addresses were refused, never the addresses. The same check runs again whenever a later run finds the
   pull request already merged.
7. The release workflow's run on the merge commit is watched to its end (a run 2 that stopped after the merge
   finds it, and never merges again). A failed build stops it with the run's page: a rebuild is the owner's (delete
   the draft if one was made, then start the workflow by hand).
8. The draft must target the merge commit; a stale or foreign draft is refused before anything is downloaded.
   Then the sign step (below), with its one `publish vX.Y.Z`.

**The build** (public repository, workflow `release`). A push to `main` that changes `Cargo.toml` to a version
with no tag builds the five binaries on GitHub's hosted runners: Windows x86_64 on Windows, Linux x86_64 and
aarch64 inside `debian:bullseye` (glibc 2.31) on x86_64 and Arm runners, and both macOS targets on an Apple Silicon
runner. Each Linux job checks that the binary needs no glibc symbol newer than 2.31. The builds use the release's
environment: `--locked`, symbols stripped, `ATLAS_RELEASE_BUILD` set (only release builds check for updates), and
every build-machine path remapped (`/PDBALTPATH` too on Windows). `cargo xtask release assets` then writes
`manifest.json` and the notes with each binary's SHA-256, and the job creates a **draft** release `vX.Y.Z` on the
merge commit with the notes, the binaries and the manifest. A draft makes no tag; the job checks that none
appeared.

**The sign step** (`cargo xtask release sign vX.Y.Z`, run by ship, or alone to resume; alone, it reads the denylist, the accepted-hits file and the key from
`ATLAS_LEAK_DENYLIST`, `ATLAS_LEAK_ACCEPTED` and `ATLAS_SIGN_KEY` only, not from the private config). It downloads the draft's
manifest and binaries, checks each file's size and SHA-256 against the manifest (and the manifest's version,
targets, names and download URLs), checks every downloaded file against the local denylist (accepted hits pass
in the binaries only) and for secret-shaped runs, and makes the full public-repository guard as the owner. It then
commits the binaries to `main` as `bin/` beside the launcher (the plugin install is a git clone of `main`) and
prepares the wiki commit, both locally. Only then does it check the signing key: openssl on `PATH` and a file at
the key's path, which is not opened; openssl alone opens it, to sign. Then `publish vX.Y.Z`, and only after it: it
signs the manifest, uploads `manifest.json.sig`, pushes `main` and the wiki, and publishes the draft on the
binaries commit, which creates the tag. It refuses on any mismatch and names each one: a hash, a size, an asset
the manifest lists that the draft lacks, a file the manifest does not list, a leak, or a `main` that moved past
the draft's commit. A run stopped after the binaries commit was pushed can be run again: it finds that commit and
goes on, behind the same prompt. `--dry-run` makes every check and stops before the key.

## The private configuration

The paths ship works from are kept in a private file, `%USERPROFILE%/.atlas-release.json` (`~/.atlas-release.json`
elsewhere), or the file `ATLAS_RELEASE_CONFIG` names:

```json
{
  "denylist": "PATH/TO/leak-denylist.txt",
  "accepted": "PATH/TO/leak-accepted.txt",
  "key": "PATH/TO/release-signing-key.pem",
  "wsl_distro": "Debian"
}
```

Paths only, and the WSL distribution; any other key is refused, and every one may be left out.
`ATLAS_LEAK_DENYLIST`, `ATLAS_LEAK_ACCEPTED`, `ATLAS_SIGN_KEY` and `ATLAS_WSL_DISTRO` win over the file. Each run
prints where each setting came from (the path, never the contents). Without `accepted`, `leak-accepted.txt` beside
the denylist is used when it is there. The file and every path it names are refused inside the atlas checkout,
inside any clone of the development or public repository, and inside any other git work tree unless git ignores
them there (and does not track them).

## The binary leak check

Ship runs it in run 1 when no current stamp stands; it can also be run on its own on the publishing machine,
from the development repository's `main`:

```sh
cargo xtask release check   # ATLAS_LEAK_DENYLIST=PATH/TO/leak-denylist.txt
```

It builds the binaries this machine can build from `git archive HEAD`, one after another and with at most 16
jobs (`CARGO_BUILD_JOBS`, capped at 16): on Windows `x86_64-pc-windows-msvc`, then `x86_64-unknown-linux-gnu`
and `aarch64-unknown-linux-gnu` inside WSL (the aarch64 build needs `gcc-aarch64-linux-gnu` and the rustup
target there). Three release builds: expect tens of minutes. It then runs the leak check and the secret scan on
them, and refuses when a target was not built. A pass writes a stamp beside the binaries, keyed by the commit,
the denylist's SHA-256 and the accepted-hits file's SHA-256: a second run with the same three says so and builds
nothing (`--force` builds anyway), and a change to any of them runs it again. WSL's glibc and cross compiler are
not the public workflow's, so these bytes are close to the shipped ones, not equal; the `linux-leak` job checks
the Linux build CI makes, and `sign` checks the downloaded assets; the stamp never stands in for either. It prints
the denylist's term count and SHA-256, both of the file and of its text with carriage returns removed and one
final newline; the `linux-leak` job prints the second for the secret copy, so the two can be compared.

**Accepted hits.** A linker packs unrelated string literals side by side, and the leak check's folding of white
space and string concatenation can join two of them into a denylist term (v0.3.0's flag strings did). Such a
place is accepted in the private accepted-hits file instead of cutting the term. Each line is a term number and
the SHA-256 of that term and the normalised text around the place (48 characters each side); the term itself is
never written. An entry passes that one place in a release binary (ELF, PE or Mach-O) and nothing else: every
place of every term is checked on its own, a text file, a path, the exported tree and the wiki pages are never
accepted, and a malformed line or an unknown term number is refused. The binary checks (`check`, the `linux-leak`
job and `sign`'s assets) print how many places were accepted and how many entries went unused. To add the places
of a hit after checking that it is a false positive, at a terminal:

```sh
cargo xtask release accept-hit PATH/TO/BINARY...           # type `accept N` to append
cargo xtask release accept-hit PATH/TO/BINARY... --print   # the entries, nothing written
```

Without a terminal it refuses. Then copy the file to the `ATLAS_LEAK_ACCEPTED` secret (the owner's steps, at the
end). An entry holds only while the same literals sit side by side again, on every target, so a new release may
need new entries.

## The checkout guard

Every step that can reach GitHub from a local checkout (ship's runs, `sign` in a dry run as well, `--pr` outside
the release-pr job) first refuses unless the checkout has no uncommitted change to a tracked file, no untracked or
ignored file where cargo builds xtask from (`xtask/`, `src/`, `Cargo.toml`, `Cargo.lock`, `.cargo/`,
`build.rs`), no file marked assume-unchanged or skip-worktree, `git fetch` of the development repository's `main`
succeeded, and `HEAD` is that `main` (run 1, `--pr`) or a commit on it (run 2 and `sign`, which run after the
release pull request, when `main` may have moved on). Run 2 then compares the checkout with the pull request's
head, and `sign` with the draft's commit, path by path (export-ignored files and the public `bin/` aside): the
code that merges, checks and signs the release is the release's own. Sign from the development commit the release
was made from. A `sign` with a local folder as `--remote` is the dry path and needs `ATLAS_GITHUB_STUB`, so no
unguarded run reaches the real draft.

`--pr` pushes to GitHub only from the release-pr job, where `ATLAS_PUBLISH=vX.Y.Z` (set by the workflow) stands
for the prompt; from anywhere else a push to GitHub asks `open vX.Y.Z` at a terminal. The dry paths run against
local bare repositories: `cargo xtask release --pr --remote PATH/pub.git` pushes the branch there and opens no
pull request, and `sign` with `--remote PATH/pub.git` and `ATLAS_GITHUB_STUB` reads the draft from a folder. Ship
has no dry path: it talks to GitHub and refuses `ATLAS_GITHUB_STUB`. The doc comments of
`xtask/src/release_ci.rs` and `xtask/src/release_ship.rs` list every option.

## Signing

`manifest.json` holds the version and, per target, the file, its download URL, SHA-256 and size; one writer
makes it, in the format atlas's updater reads. The signature, `manifest.json.sig`, is made with `openssl pkeyutl
-sign -rawin` from the release signing key (an ed25519 PEM kept offline; openssl asks for its passphrase; nothing
ever creates or prints a private key). Signing refuses a key whose public half is not the one built into atlas
(`TRUSTED_KEY_HEX` in `src/update.rs`), and refuses to publish while that constant is still the test key.

## The local rehearsal

`cargo xtask release --remote URL` builds the binaries this machine can build (Windows natively and Linux x86_64
inside WSL on Windows; Linux natively on Linux; both macOS targets on a Mac with `--build-only`), runs clippy and
the tests, exports, checks the export and the binaries against the denylist, and makes the release commit, its tag,
the unsigned manifest and the wiki commit in the stage folder, from a clone of URL. It never pushes and never
signs. The `--push` that once published from here is gone: it skipped the pull request, the approval and the
public build, and needed the ruleset bypass. Every release goes through ship.

```sh
cargo xtask release --build-only                       # binaries only (on a Mac, for macOS)
cargo xtask release --remote PATH/pub.git              # everything but push and sign
```

Both this and the pull request refuse an export without `.github/workflows/ci.yml`, `.github/workflows/release.yml`,
`.github/CODEOWNERS`, `SECURITY.md`, `LICENSE`, `NOTICE`, `README.md`, `CHANGELOG.md` and `wiki/Home.md`.

**The address check.** Every author, committer and `Co-authored-by:` address in the public history must be the
public one, or GitHub's own on the commits a merged pull request makes (`noreply@github.com`, an app's `[bot]`
address). A refused address is counted in the error, never printed. The two
v0.2.1 merges whose author address GitHub's web merge took from the merger's account are accepted by name; no
other commit is.

## The wiki

This wiki is written in the repository's `wiki/` folder and published once per release to `<public
repo>.wiki.git` by the sign step, from the public `main`.

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
private repository. Before anything is merged or pushed the release asks GitHub (`gh api`) about
`ATLAS_PUBLIC_REPO` and refuses unless:

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
not one, so `--pr` reports those two parts as unseen and checks the rest; run 2 and `release sign` run as the
owner and check everything before anything is merged or published.

Before that, and before anything is built, the release checks that the README, `SECURITY.md`, the workflows'
gates and every wiki page name `ATLAS_PUBLIC_REPO` wherever they name the public repository (wiki and security
links, the marketplace command), and refuses a mismatch.

A refusal names what it found, for example `bloodfeast/atlas resolves to bloodfeast/atlas-dev (private): create
the public repo first`. A step that reaches GitHub stops on it; a dry run and a local remote report it and go on.
The tests use saved answers (`ATLAS_GITHUB_STUB`) and stubbed GitHub calls, and never reach the network.

## The public repository's setup

Applied by hand before the first release; the guard checks the parts that decide who can bypass what.

**Ruleset `main`** (the default branch):

- no deletion, no force-push, linear history;
- pull requests required, with 1 approving review from a code owner (`.github/CODEOWNERS`), stale approvals
  dismissed on a new push, approval of the most recent push, and every conversation resolved;
- squash merge only;
- the CI checks required (see the owner's steps);
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
  `ATLAS_RELEASE_APP_KEY`, beside `ATLAS_LEAK_DENYLIST` (the denylist's text) and `ATLAS_LEAK_ACCEPTED`.

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

## The owner's steps

Applied by hand by the owner; nothing in the release changes a ruleset, a required check or a secret. Rulesets on
a private repository need a paid GitHub plan; if the ruleset page says rulesets are not enforced, they are not.

**1. Ruleset `v-tags` on the development repository** (a tag ruleset). A `v*` tag pushed there starts the
release-pr workflow, which opens the public pull request, so no one may make one, the owner included: agents push
with the owner's identity, and a bypass for the admin role would let theirs through too.

1. `bloodfeast/atlas-dev`: Settings, Rules, Rulesets, New ruleset, New tag ruleset.
2. Name `v-tags`; Enforcement status Active; Bypass list empty.
3. Target tags: Add target, Include by pattern, `v*`.
4. Rules: Restrict creations, Restrict updates, Restrict deletions. Create.

Or, as one command:

```sh
gh api -X POST repos/bloodfeast/atlas-dev/rulesets --input - <<'JSON'
{"name": "v-tags", "target": "tag", "enforcement": "active", "bypass_actors": [],
 "conditions": {"ref_name": {"include": ["refs/tags/v*"], "exclude": []}},
 "rules": [{"type": "creation"}, {"type": "update"}, {"type": "deletion"}]}
JSON
```

To push a tag on purpose, set the ruleset's Enforcement status to Disabled, push the tag, and set it back to
Active. Ship starts the workflow with the version and needs no tag.

**2. Ruleset `main checks` on the development repository** (a branch ruleset). The `dev-pr` workflow's
`release-check` job (a release-target build of the Linux x86_64 binary in `debian:bullseye`, nothing uploaded,
and both clippy runs) must pass before a pull request merges. It runs when a pull request touches what the build
or clippy reads (`Cargo.*`, `rust-toolchain.toml`, `src/`, `xtask/`, `tests/`, `config/`, `hooks/`, `vocab/`,
`.cargo/`, `.github/`); on any other pull request it is skipped, which counts as passed. It costs about 12
private Actions minutes a run.

1. Open a pull request that touches `src/` once, so GitHub has seen the check.
2. `bloodfeast/atlas-dev`: Settings, Rules, Rulesets, New ruleset, New branch ruleset.
3. Name `main checks`; Enforcement status Active; Bypass list empty.
4. Target branches: Add target, Include default branch.
5. Rules: Require status checks to pass; Add checks, `release-check`. Create.

Or:

```sh
gh api -X POST repos/bloodfeast/atlas-dev/rulesets --input - <<'JSON'
{"name": "main checks", "target": "branch", "enforcement": "active", "bypass_actors": [],
 "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
 "rules": [{"type": "required_status_checks", "parameters": {"strict_required_status_checks_policy": false,
   "do_not_enforce_on_create": false, "required_status_checks": [{"context": "release-check"}]}}]}
JSON
```

With no bypass, a commit pushed straight to `main` without the check is refused too: everything reaches `main`
through a pull request.

**3. The required checks of the public `main` ruleset.** Ship refuses to merge until these five have passed on
the pull request's head; the ruleset should require the same, so a merge on GitHub's page cannot skip them either.
GitHub offers a check for this list once it has run, so add them after the first release pull request:

1. `bloodfeast/atlas`: Settings, Rules, Rulesets, `main`.
2. Require status checks to pass: Add checks, `test (ubuntu-latest)`, `test (windows-latest)`,
   `test (macos-latest)`, `clippy (windows-latest)`, `clippy (ubuntu-latest)`. Save.

**4. The denylist secret.** `ATLAS_LEAK_DENYLIST` is a copy of the local denylist file; after any change to the
file, replace the copy with the file's whole text (gh reads it from standard input and never prints it):

```sh
gh secret set ATLAS_LEAK_DENYLIST --repo bloodfeast/atlas-dev < PATH/TO/leak-denylist.txt
gh secret list --repo bloodfeast/atlas-dev        # ATLAS_LEAK_DENYLIST shows the new date
```

GitHub never shows a secret's value, but the release-pr job's `linux-leak` step "Leak and secret gates" prints
the copy's term count and SHA-256 (carriage returns removed, one final newline); `cargo xtask release check`
prints the same for the local file. If they differ, set the secret again. The release-pr job drops carriage
returns, so a file with Windows line endings is fine.

The file holds one term per line; blank lines and lines starting with `#` are skipped. A term matches anywhere,
inside longer words too, unless the line right before it is exactly `#[word]`: then it matches only as a whole word,
with no letter, digit or `_` right before or after it (`-` is not a word character, so `x-term` still matches). The
marker is a comment to everything else that reads the list (the log masking above, idea-capture redaction), which
still treat the term below it as a plain term, and term numbers do not change. `xtask/src/leak.rs` has the details.

**5. The accepted-hits secret.** `ATLAS_LEAK_ACCEPTED` is a copy of the accepted-hits file (term numbers and
digests only), read by the `linux-leak` job; unset means no accepted hits there. After any change to the file:

```sh
gh secret set ATLAS_LEAK_ACCEPTED --repo bloodfeast/atlas-dev < PATH/TO/leak-accepted.txt
```

**6. The private configuration.** Write `%USERPROFILE%/.atlas-release.json` as shown above, with the paths of the
denylist, the accepted-hits file and the signing key, outside every repository (or in one that ignores them).
