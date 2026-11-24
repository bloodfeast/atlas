How a release of atlas is made. Users need none of this; it is here so the process is public and checkable.

## The changelog rule

Every release gets a user-facing section in `CHANGELOG.md`, `## vX.Y.Z`, written for users: what changed for
them, not how the work was organised. Its parts, in order, each left out when empty:

- `### Highlights`: what a user notices, with measured numbers where there are any. Required.
- `### Breaking changes` and `### Upgrade notes`: what a user must do when updating.
- `### Features`, `### Fixes`, `### Security hardening`, `### Known limits`: everything else.

`cargo xtask release` refuses a version whose section is missing, empty (blank lines, headings and HTML comments
only) or has no `### Highlights` with text, before anything is built. It also refuses a section that, outside a
fenced block, has a line starting with `<` (HTML comments included), a control character but a tab, a heading
after an indent or a list marker, or a setext underline, since each can change or hide what the notes show. The section is the source of three things:
the release notes, the release commit's message, and this wiki's [[Changelog]] page.

## Versioning

Releases are batched, and the version comes from changelog fragments. Every pull request that changes what a
user gets adds `changes/<pr>.md`: front matter naming its bump (`patch`, `feature`, `breaking`, or `none` for an
upgrade note) and its section
(`highlight`, `breaking`, `upgrade`, `feature`, `fix`, `security` or `known-limit`), then its lines for users.
A docs, CI or test-only pull request that touches a build path adds an empty `changes/<pr>.none` instead; the
development repository's `dev-pr` check fails one with neither (`changes/README.md` has the format).

When a release is wanted, `cargo xtask version` (`--dry-run` to preview) lists the pending fragments in merge
order, folds them into one section for the release at the top of `CHANGELOG.md` (refusing a malformed
fragment, a stray or untracked file, an empty set, a set of upgrade notes with no bump, or a section with no
Highlights), writes the version into
`Cargo.toml`, `Cargo.lock` and `.claude-plugin/plugin.json`, and deletes the fragments, all in one commit,
merged like any other; a failed step puts every file back. The version is Release.Feature.Patch, the same
above and below 1.0.0: a release takes the largest pending bump, once per release. A `patch` raises the last
number, a `feature` the middle one and a `breaking` the first, each resetting the numbers below it, so 0.6.0
with patch, patch, feature, patch is 0.7.0. A `fix` or `security` fragment is `bump: patch`, and an upgrade note
carries no bump of its own (`bump: none` or none written); the checks refuse a fragment that says otherwise.
Otherwise the release number goes up only by hand, with `--release` ((X+1).0.0). The one-off override,
`--set X.Y.Z`, folds every pending fragment into one section at that version and ignores the bumps; it must
be strictly above the current version and cannot go with `--release` (first use: v0.6.0 for the batch since
v0.5.0, the largest-bump rule counting from there). Merge order is each
fragment's adding commit on the first-parent history. `release ship` refuses to start while fragments are
still pending. Neither reads a tag.

## The README gate

The README is public, and its figures say which version measured them. `cargo xtask release check` and
`cargo xtask release ship` (before it dispatches the export) refuse a `README.md` that is out of date for the
version in `Cargo.toml`, and name each README line at fault:

- a figure stamped `at vX.Y.Z` (or `measured at vX.Y.Z`; `X.Y` alone counts too) from a minor more than one
  before the release: releasing 0.6.1, a figure at v0.5.0 passes and one at v0.4.0 is refused. A figure from an
  older release number is refused (releasing 1.0.0, a figure at v0.9.0). A version not after the word `at`,
  such as a `(v0.1)` baseline, is not a stamp;
- a line that says something is `planned` or `coming` and names, in backticks, a term that a released
  `## vX.Y.Z` section of `CHANGELOG.md` names in backticks. A promise in prose alone ("more languages are
  planned") is not caught, so read the README for those when cutting a release.

Fix it by measuring the figure again on this version and stamping it, or taking it out; never by restamping a
figure that was not measured again. `cargo test -p xtask` runs the gate on the repository's README too
(`the_repository_readme_passes_for_this_version`), so the version commit that leaves a figure two minors behind
fails its tests until the README is refreshed.

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

## CI caches

The third-party actions a workflow may use are a short list a test holds, each at one commit, so a bump is a
deliberate edit in the workflow and the test (`third_party_allowed` in `xtask/src/release_ci.rs`); a tag, another
commit or any other action fails it. The rule is Brandon's (2026-11-19: "Allow cache, not leak job", then "Replace
with GitHub/inline"): the public `ci.yml` uses GitHub's own actions only, because the public repository's Actions
policy refuses any other. Its cache is `actions/cache` (restore, and save from main), cargo-nextest is a prebuilt
release whose sha256 the workflow pins and checks (never built from source), and the Windows test job's ReFS Dev
Drive is an inline PowerShell step. `dev-pr.yml` may use `Swatinem/rust-cache`; `release-pr.yml`, whose leak job
holds the denylist, and `release.yml` none.

In the public `ci.yml` a push to main saves the caches and every pull request, a fork's too, restores main's and
saves nothing (the save step runs on main only), so no pull request can plant what main or another pull request
builds from. Each holds the cargo registry and the target folder. The target folder is never pruned: a
`Cargo.lock` change restores the last cache and saves a new entry with the old dependencies' artifacts still in
it, so the entries grow until GitHub's 10 GB repository limit evicts the oldest; that is accepted (the next save
after an eviction starts small again).

`dev-pr`'s `release-check` caches compiled dependencies with `Swatinem/rust-cache` too. It saves the registry crates only (the action removes the
workspace's own crates before it saves), keyed on the runner, rustc's version, `Cargo.lock`, the `Cargo.toml` files
and `rust-toolchain.toml`, so a toolchain or lockfile change starts cold; `zstd` is installed in the Debian 11
container for the archive. The save setting is the action's default: the workflow runs only on pull requests, and
GitHub scopes a cache a pull request saves to that pull request, so main and other pull requests never read it. The
cache therefore helps only a pull request's later pushes and re-runs: every pull request's first run is cold (3m47s
measured, plus a 36 MB save), a re-run warm (2m39s). `release-pr`'s leak build and the release workflow
(`release.yml`) cache nothing: a published binary, and the one checked against the denylist, are always built cold.

## Before a pull request

Every pull request passes, on the machine that made it and on its committed head: `cargo fmt` on the files it
touches; both clippy runs above; `cargo xtask test-gate`, the tests on nextest and then the doc tests; and `cargo
xtask linux-test`, the same tests in release (going on past a failure) and both clippy runs in a Docker Linux
container (see [[Testing and coverage]]).
The last is there because a Windows or macOS machine never compiles the `cfg(target_os = "linux")` code and the
development repository's CI builds and lints but runs no tests, so before it a Linux-only failure first showed on
the public repository's release pull request. Commit first, since it tests the working tree as it is; its last
line is the result to quote, for example `linux-test: PASS in 250s; test ok (1715 passed, 0 failed) 239s; doctest
ok (2 passed, 0 failed) 4s; clippy all-targets dev ok 5s; clippy default ok 1s`.

## The release

A release goes through three hands: the development repository opens a pull request, the public repository
builds, and the owner's machine approves, signs and publishes. Nothing of the development history is exported,
and the release signing key never leaves the owner's machine. The whole release is one unattended run, which an
agent on the owner's machine makes from start to end:

```sh
cargo xtask version                # folds changes/ into CHANGELOG.md and bumps the version
# open that as a pull request on the development repository, merge it once its checks pass
cargo xtask release ship vX.Y.Z    # preflight, release-pr, approval, merge, build, sign, publish
```

`release ship` reads what GitHub holds for the version and takes each next step until the release is out: it
opens the release pull request (run 1), waits for the public repository's checks and for the owner's approval on GitHub, merges it, watches the build,
signs and publishes (run 2). It asks nothing at the terminal. A run
that stopped anywhere is simply run again, and goes on from where GitHub stands. It stops, with the line that
failed, on anything a person must decide: a gate that fails, a failed check, a request for changes, a new leak
hit (below), or more than a day of waiting.

**The one human gate** is the owner's approval of the release pull request on GitHub (from any device, his
phone included), plus a new leak hit, which always stops for him. Ship never approves the pull request itself: it
looks at the pull request's reviews once a minute, with no prompt, and the moment his latest review approves its
head commit and every check on that head has passed, it merges, signs with the stored passphrase and publishes,
with no further step. An approval of an earlier head does not count.

`cargo xtask release ship vX.Y.Z --attended` is the older two-run form for the owner at a terminal: run 1 opens the
pull request, the owner approves it on GitHub, and run 2 asks for a typed `merge vX.Y.Z` before the merge and
`publish vX.Y.Z` before the signature, each refused without a terminal.

**What the owner sets up once.**

1. The private configuration (below), naming the denylist, the accepted-hits file and the signing key.
2. `cargo xtask release store-passphrase`, at a terminal: it asks for the signing key's passphrase twice, without
   echo, checks that it opens the key, and keeps it encrypted with Windows DPAPI under his account on this machine
   (`%LOCALAPPDATA%/atlas-release/sign-passphrase.dpapi`, or the file `ATLAS_SIGN_PASSPHRASE_STORE` names). It
   decrypts on that machine under that account only, but there any process running as the account can decrypt
   it, an agent's included: the store keeps the passphrase off disk in the clear and away from other accounts,
   not away from programs the owner runs. Ship refuses to start without it, and checks that it opens the key
   before anything else, so a run never stops at the signature after the merge. Ship hands it to openssl on
   openssl's standard input (`-passin stdin`), never in an argument, the environment or any output. Run it again after changing the passphrase. Off Windows there is no store, and openssl asks for
   the passphrase when signing.
3. `gh` logged in as the owner, with the rights to merge the public pull request, set the
   development repository's environment secrets, and dispatch its workflows.

**Approval.** The public `main` ruleset wants the code owner's approval of the release pull request, and ship
wants it too: the owner's latest review must approve the head commit. Ship never gives it; it waits for it.

**The preflight.** Before run 1 dispatches anything, ship runs here every check the `release-pr` and `release`
workflows make, so a failure stops with its own line instead of on GitHub: the hidden-text guard (`cargo xtask
hidden`); `release --pr` as a dry run against a fresh clone of the public repository (the changelog section and
its highlights, the versions, the links, the export, every exported file and wiki page against the denylist, the
release notes, and the export guard); the xtask tests (`cargo test -p xtask --locked`, which also checks the
lockfile the workflows build with `--locked`); the public CI's required checks on the exported tree, its
clippy lines from the exported `ci.yml` run as written (the pinned toolchain, `--all-targets` with
`--features atlas/dev`, and `-D warnings`) and its workspace tests (`cargo test --workspace --profile ci --locked
--no-fail-fast`, which covers nextest's run and the doc tests in one), with the exported tree first made a git
checkout of its own whose index tracks the export as the release commit does (the release commit also keeps main's previous
`bin/`, which the export never carries and no workspace test depends on), since tests that ask git what is tracked
would otherwise ask whatever repository holds the stage folder; then `release check`, the binary leak gate
and secret scan on locally built release binaries. A pass is stamped in `<stage>/vX.Y.Z/preflight` for the commit, the denylist and
the accepted-hits file.

**The leak secrets.** Just before each dispatch, ship sets the `linux-leak` job's
`ATLAS_LEAK_DENYLIST` and `ATLAS_LEAK_ACCEPTED` secrets of the development repository from the local files
(the bytes piped to `gh`, never printed; a comment line when there are no accepted hits), so the
`linux-leak` job always checks with exactly what the local gates used, and a change to either file never needs a
hand copy to GitHub.

**Run 1** (no pull request yet):

1. The checkout guard (below) at the development repository's `main`, and its `origin` must be
   `github.com/bloodfeast/atlas-dev` as git really fetches and pushes (`insteadOf` and a push URL applied).
   `Cargo.toml` must be the version asked for. A draft release of the version already on the public repository
   stops it: the build would refuse to make another.
2. The export guard (see [Outside pull requests](#outside-pull-requests)): a change on the public `main` that
   `HEAD` does not hold, which the export would revert, stops it before anything is checked or dispatched.
   Then the binary leak check: unless a pass stamp stands for this commit, denylist and accepted-hits file, `release
   check` (below) runs first. The plan says so before it starts: three release builds, tens of minutes.
3. The plan, the preflight (above), the binary leak check when no stamp stands, the leak secrets set from the
   local files, then the dispatch, with no prompt. The command records the commit, a random nonce and the run in its stage folder
   (`<stage>/vX.Y.Z/ship/state.json`), and starts the `release-pr` workflow with the version, that commit and the
   nonce. It finds the run by the nonce in its name, watches it to the end, and prints the pull request. A failed
   run is reported by its page only, never its log.

A run already in progress is watched, never started twice. After a failed run, the next run 1 starts it again;
the release branch may already be on the public repository, and the new run
replaces it. The same holds once the pull request is open: when `main` has moved on since run 1, or the last
run that was meant to update the pull request failed or never showed, ship re-exports (run 2, step 2). A pull
request closed without a merge gives way to run 1 again once its branch is gone from the public repository, or
holds another commit than the closed pull request's head; while the branch still holds that head, ship refuses
and says to delete the branch by hand, then run ship again.

**The pull request** (development repository, workflow `release-pr`, which only ship's dispatch starts). Each of
its two jobs checks out the commit ship sent and refuses one that is not on `main`. Job `linux-leak` builds the
Linux x86_64 binary exactly as the public `release` workflow's `linux` job does (in `debian:bullseye`, in the
release environment, with the same remaps), runs the binary leak check (accepted hits honoured) and the secret scan
on it, exports the tracked tree with `git archive` and checks every exported file and every wiki page against the
private denylist; it uploads nothing, a hit stops the release before anything reaches the public repository, and
only when every check passed does it name the commit it checked. The denylist comes from a secret, written to a
temporary file that only the job can read and removed after it; every term is masked in the log and none is ever
printed. Job `pr` never holds the denylist: `cargo xtask release --pr` makes the early checks (the changelog
section with its highlights, `Cargo.toml` and `plugin.json` on the same version, the links naming the public
repository, the public-repository guard as far as the release app can see it), exports the same commit again and
refuses any other than the one `linux-leak` checked, makes the export guard again on the public `main` it
clones, then pushes branch `release/vX.Y.Z` to the public repository, the exported tree on top of its `main` with `bin/` left as it is, and opens a pull request into `main`
whose body is the release notes. Each job's secrets are its own, in an Environment that only `main` may use. No tests
run and nothing else is built there: the public repository's CI tests the pull request.

**Review** (public repository). The pull request comes from the release GitHub App, so the owner can give it
the code-owner approval the `main` ruleset requires. The owner approves it on its head commit once the CI
checks pass; unattended ship is waiting for exactly that. Never merge it on GitHub: run 2 merges
it.

**Run 2** (the pull request open):

1. The pull request must be the release's: open, from `release/vX.Y.Z` of the public repository itself (not a
   fork) into `main`, opened by the release app, titled `atlas vX.Y.Z`.
2. `HEAD` must be the commit run 1 sent, and that run must have passed (when this checkout holds run 1's
   record; without it the checks below stand alone, and the command says so). If not, ship **re-exports**
   instead of merging: run 1's steps again for `HEAD`, which must be the tip of `main` (a re-export never goes
   back to an older commit), with the binary leak check unless a stamp stands, the plan naming the pull request,
   and, with no prompt, a line naming the pull request's number, its current head and `HEAD` before the
   dispatch. The new `release-pr` run replaces the release branch and updates the open pull request, so its head
   moves; ship watches the run and prints the pull request. An approval of the old head does not count: review
   the pull request again, approve it on its new head, and run ship again. A run still in progress is watched,
   never dispatched again; a failed one is dispatched again on the next run. Otherwise the checkout guard with
   `HEAD` on `main`.
3. The owner's latest review must be an approval of the pull request's head commit (an approval of an earlier
   push does not count), and no one else's latest review may ask for changes. Each reviewer's latest is read from
   the pull request's `reviews` by time, comments aside, as GitHub counts it (gh's `latestReviews` leaves the
   commit out, so it cannot tell which head was approved), and every check must have passed, these five among
   them: `test (ubuntu-latest)`, `test (windows-latest)`, `test (macos-latest)`, `clippy (windows-latest)`,
   `clippy (ubuntu-latest)`. Anything missing is listed and nothing changes. Unattended, a check still running
   or not yet started is waited on (a look a minute), and so is the owner's missing approval; anything
   else ends the run. With `--attended` the run ends; run it again later.
   The command checks this itself because the owner, as an admin, could merge past the ruleset on GitHub.
4. The full public-repository guard, then a fresh clone of the public repository: the checkout's `HEAD` must hold
   exactly what the pull request's head holds (export-ignored files and `bin/` aside), so the approval was of
   this code.
5. `merge vX.Y.Z` (typed with `--attended`, said unattended), then a squash merge pinned to the approved head commit (GitHub refuses it if the branch moved),
   with the release commit's own subject and message, so GitHub's default text never decides what enters the public
   history. GitHub picks the squash commit's author (it refused the public address given as `--author-email`), and
   ship checks every address after the merge. Never with the admin bypass. The branch is kept.
6. Every author, committer and `Co-authored-by:` address of the public history, merge commit included, must be
   the public one or GitHub's own, or an outside pull request's author on its own squash commit (the address
   check, below); a hit stops everything and names the merge commit and
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
every build-machine path remapped (`/PDBALTPATH` too on Windows). Each build job then attests its binaries'
build provenance (GitHub's `attest-build-provenance`: a signed statement that this workflow built these bytes from
this commit). A job with a read-only token runs `cargo xtask release assets`, which writes `manifest.json` and the
notes with each binary's SHA-256, and a last job, which runs none of the repository's code, creates a **draft**
release `vX.Y.Z` on the merge commit with the notes, the binaries and the manifest. A draft makes no tag; the job
checks that none appeared. A build started by hand builds `main` only.

**The sign step** (`cargo xtask release sign vX.Y.Z`, run by ship, or alone to resume; alone, it reads the denylist, the accepted-hits file and the key from
`ATLAS_LEAK_DENYLIST`, `ATLAS_LEAK_ACCEPTED` and `ATLAS_SIGN_KEY` only, not from the private config). It downloads the draft's
manifest and binaries, checks each file's size and SHA-256 against the manifest (and the manifest's version,
targets, names and download URLs), checks every downloaded file against the local denylist (accepted hits pass
in the binaries only) and for secret-shaped runs, and checks each binary's build provenance with `gh attestation
verify`: it must have been built by the public repository's `release.yml` on `main` at the draft's commit (the
release pull request's merge commit), on a GitHub-hosted runner, so binaries swapped into the draft after the
build, manifest and all, are refused. It makes the full public-repository guard as the owner. It then
commits the binaries to `main` as `bin/` beside the launcher (the plugin install is a git clone of `main`) and
prepares the wiki commit, both locally. Only then does it check the signing key: openssl on `PATH` and a file at
the key's path, which is not opened; openssl alone opens it, to sign. Then `publish vX.Y.Z`, and only after it: it
signs the manifest, uploads `manifest.json.sig`, pushes `main` and the wiki, and publishes the draft on the
binaries commit, which creates the tag. No attestation covers the draft's notes, so the publish replaces them with
notes the sign step makes again from the changelog at the draft's commit and the verified binaries (a dry run
writes them to its stage folder to read). It refuses on any mismatch and names each one: a hash, a size, an asset
the manifest lists that the draft lacks, a file the manifest does not list, a leak, a binary without that provenance, or a `main` that moved past
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

**A place in a binary is judged by the source, not its bytes.** A linker packs unrelated string literals side by
side with nothing between them, in an order that changes per build and per target, so a term can show up across
two literals that each hold only part of it (v0.3.0 to v0.9.0 had such places, and the earlier rules, which keyed
an accepted place on the bytes around it, needed a new entry for most releases). So every place of a term in a
release binary (ELF, PE or Mach-O) is explained against the source the binary was built from:

- the tree: for `check`, the stage's `git archive`; for `gate` and `sign`, the checkout's HEAD (its files that are
  not export-ignored, as HEAD holds them, whatever the working files say), or `--src TREE`;
- the dependencies: every registry package `cargo metadata --locked` names outside the tree, by crate, version
  and file; a path or git package outside the tree is the owner's code, judged as the tree is;
- the string literals of the toolchain's library source, when `rust-src` is installed: always read, so what
  explains one place never depends on whether another was explained.

Two kinds of known strings come from those files. Literal values, what a compiler lays into a binary whole: the
string literals of Rust and C files (escapes decoded, a format string also cut at its `{}`), the literals a
`concat!` or C's adjacency joins, as one value, and every other text file whole (`include_str!`). And, only to
report a place one of them holds, the identifiers and comment lines of Rust and C files and the lines of other
text files (none from the toolchain). A string of fewer than 3 bytes is not one. A linker lays whole literal values
end to end, so, by the bytes of the place:

- **literals packed side by side**: two or more whole literal values, each starting where the one before ends, of
  different files, make it, and reach past the term on both sides. It passes with no entry, and the binary checks
  list each such place with its pieces' files and lines;
- **one string of the tree** holds it: it stops. The export gate refuses the same term in the source, which is
  where to fix it;
- **one literal of a dependency or of the toolchain** holds it, with at least 4 bytes beyond the term: third-party
  text, reported by crate, version, file and line. It stops until an entry accepts it;
- **a join**: whole known strings end to end make it, but not as packing does: a piece is an identifier, a comment
  or a text line, two pieces side by side come from one file (a macro may join them in the source), or the pieces
  start or end exactly where the term does (as a word in a path would). It may be packing, it may not: it stops,
  and the owner checks the place; only an entry keyed on its pieces passes it;
- **anything else stops, and no entry accepts it**: no such chain makes the place, so the build put it there (a
  path with a home folder in a panic location, a user or host name, an environment value, generated code); or a
  dependency's string that is not a literal value, or holds the term with less than 4 bytes around it (an entry
  for it would accept the term wherever a build puts it). Fix the build (the release builds remap the workspace,
  cargo home and sysroot paths).

A place in a text file, in a path, in the exported tree or in the wiki pages is never explained or accepted, nor a
place found only in a UTF-16 reading. Every place is judged on its own, so an accepted one cannot hide another
place of the same term.

**Accepted hits.** Each line of the private accepted-hits file is a term number and the SHA-256 of the domain
`atlas-accept-src-v1`, that term, the crate's name and the dependency's string that holds the place. Source content
only: no offset, no neighbouring bytes, no version or file path, so an entry holds on every rebuild and every
target, and voids when a new version of the crate changes that string. A join's entry is the SHA-256 of the domain
`atlas-accept-join-v1`, that term, and each piece's crate and string in order. The term itself is never written.
Entries of the earlier forms (digests of a binary's bytes around a place) match nothing: the checks say once how
many entries matched no place, so delete the ones no release needs. A malformed line or an unknown term number is
refused. The binary checks (`check`, the `linux-leak` job and `sign`'s assets) print each place of literals side
by side and each accepted place, with the files and lines of their source, never a path that holds a term. To list
every place of a binary with its verdict, and add the entries for its dependency and join places after checking
them, at a terminal:

```sh
cargo xtask release accept-hit PATH/TO/BINARY... [--src TREE]           # type `accept N` to append
cargo xtask release accept-hit PATH/TO/BINARY... [--src TREE] --print   # the entries, nothing written
```

Without a terminal it refuses: a new hit is always the owner's call, never accepted by an agent. A leak report
names the file, line, column, byte offset, term number and verdict of each place, never the term, so an agent can
locate the place and ask. Ship copies the file to the `ATLAS_LEAK_ACCEPTED` secret itself before its next dispatch.
`check`, `gate` and `accept-hit` read the denylist and the accepted-hits file from the private configuration
(above) when `ATLAS_LEAK_DENYLIST` is not set.


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
-sign -rawin` from the release signing key (an ed25519 PEM kept on the owner's machine; nothing ever creates or
prints a private key). Its passphrase comes from the DPAPI store on Windows (see the owner's one-time setup under
[The release](#the-release)), given to openssl on its standard input; elsewhere openssl asks for it. Signing refuses a key whose public half is not the one built into atlas
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
address), or, as the author of its own squash commit only, the address of a merged outside pull request's author
(the address rule under [Outside pull requests](#outside-pull-requests)). A refused address is counted in the
error, never printed. The two v0.2.1 merges whose author address GitHub's web merge took from the merger's account
are accepted by name; no other commit is.

## Outside pull requests

Anyone may open a pull request on the public repository (`CONTRIBUTING.md`, exported, tells them how). The public
CI tests it, and the owner merges it on GitHub as one squash commit, its author kept. Three parts of the release
keep such a change and its author:

**Before merging one**, in GitHub's squash dialog delete every `Co-authored-by:` line of anyone but the pull
request's author (an AI tool's address is a common one). The address rule (below) passes only the author's own
address, so import and run 1 refuse otherwise, naming the commit and the refused slot, never the address. The
public history cannot change after the merge: the only remedy then is to check the address by hand and add the
commit to `ACCEPTED_MERGES` in `xtask/src/release_ci.rs`.

**Import.** The development repository holds the code, and every release exports its tree on top of the public
`main`, so a change merged only on the public repository would be reverted by the next export. After the merge,
in a clean checkout of the development repository:

```sh
cargo xtask release import             # one development pull request per public change
cargo xtask release import --dry-run   # the commits only: nothing pushed, nothing opened
```

It clones the public repository (and never pushes to it), and lists every commit of its `main` that is not a
release's (`atlas vX.Y.Z`, `atlas vX.Y.Z (#N)` or `atlas vX.Y.Z binaries`, every address the public one or
GitHub's) and that the development `main` does not hold yet. A change is held when a commit in the development
history carries a `Public-commit: <sha>` trailer naming it (an import's commit does), or when the change,
`changes/` aside, applies in reverse to the development tree (it was made there by hand); a held change is skipped
and named. For each other change, which GitHub must tie to a merged outside pull request (below), it makes branch
`import/public-<n>` on the development `main` with the change applied, its author, address and date kept and its
subject ending `(public #<n>)`, pushes the branch to the development repository and opens a pull request there
whose body names the public pull request and commit. A changelog fragment the outside pull request carries,
`changes/<n>.md`, is renamed `changes/public-<n>.md` (a slug kept), with `author: <login>` added to its front
matter, so it never collides with a development pull request's fragment; `cargo xtask version` folds it like any
other, and each of its lines in the CHANGELOG ends `Thanks to @<login> (#<n>).`. A marker, `changes/<n>.none`
for a change with nothing user-facing, becomes `changes/public-<n>.none`. The development `dev-pr` check takes
an imported fragment or marker as the pull request's own. A change that does not apply to the development `main`, or
that GitHub ties to no merged outside pull request, is named and left for the owner to bring in by hand, with a
`Public-commit:` trailer naming it.

Merge an import pull request with **Rebase and merge**, never squash: a squash makes whoever merges the commit's
author (the outside author only a co-author), and once the branch has a second commit, GitHub's squash message
moves the `Public-commit:` line out of the commit's trailer block. The guard still finds that line anywhere in a
commit's message, but the author is lost. Only an import writes these lines: a contributor's own `Public-commit:`
or `Public-pr:` lines are stripped from the message it copies, so no public change is marked held by its
contributor. Before it opens anything, the import runs the address check (below) over the whole public `main` and
refuses a commit the release would refuse. Imports are not stacked: each one is built on the development `main`,
so a public change that depends on another one fails "does not apply" until the first one's import is merged; run
the import again then.

**The export guard.** Release run 1 (`release ship`, before the leak check and the dispatch) and `release --pr`
(before the release branch is pushed) clone the public repository and refuse while its `main` holds a change
that is not a release's and that `HEAD` does not hold, naming each commit and saying to run `cargo xtask release
import` first. Nothing is pushed or dispatched; merge the import's pull request, then run ship again. Run 1 then
runs the address check (below) over every commit of the public `main`, the check run 2 makes after its merge,
and refuses before the leak check and the dispatch, so a commit it refuses never stops a release halfway.

**The address rule.** The address check (run 2 step 6, every later run, `sign` and the local rehearsal) passes a
commit's author address when GitHub says the commit is an outside pull request's squash commit: `repos/<repo>/
commits/<sha>/pulls` names a pull request merged into `main` of the public repository with that commit as its merge
commit, opened by a user (not a bot) other than the repository's owner, and `repos/<repo>/commits/<sha>` names that
same account as the commit's author and `web-flow` (GitHub itself) as its committer. No address is kept in a list:
an owner address links to the owner's account, so it still fails as an outside commit's author, and every
committer and `Co-authored-by:` address of an outside commit is checked as before. A commit GitHub ties to no
merged outside pull request fails on an unknown address, and the check still prints counts only, never an
address. Both endpoints answer for a public repository with any token, or none, so the owner's `gh` reads them.
The rule trusts any GitHub user but the owner, so the owner contributes as the owner, never from another account.
When GitHub does not answer (an error or a rate limit), the check says it "could not verify" the commit, which is
not a refused address; nothing passes unverified, so run again once GitHub answers.

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
  `creation`.

GitHub shows a ruleset's bypass list, and may show the collaborator list, only to an admin. The release app is
not one, so `--pr` reports those two parts as unseen and checks the rest; run 2 and `release sign` run as the
owner and check everything before anything is merged or published.

Before that, and before anything is built, the release checks that the README, `SECURITY.md`, the workflows'
gates and every wiki page name `ATLAS_PUBLIC_REPO` wherever they name the public repository (wiki and security
links, the marketplace command), and refuses a mismatch.

A refusal names what it found, for example `bloodfeast/atlas resolves to bloodfeast/atlas-dev (private): create
the public repo first`. A step that reaches GitHub stops on it; a dry run and a local remote report it and go on.
The tests use saved answers (`ATLAS_GITHUB_STUB`) and stubbed GitHub calls, and never reach the network.

## The repositories' setup

The rulesets, settings, release app and secrets the release depends on are set by the owner by hand; nothing in
the release changes them, and the guard above checks the parts that decide who can bypass what.

## macOS

On macOS, `cargo xtask verify-macos` runs the whole suite in release with the `dev` feature (the FSEvents
watcher test included), then the recall check and the bench on `fixtures/generic`, a small corpus shipped for
that purpose (its database goes to a temporary folder). Paste its whole output back.

