# Contributing to atlas

Pull requests are welcome: a fix, a new source, a sharper doc, a test that catches a bug. A small one with a clear
reason merges fastest.

## How a pull request goes in

1. **Open it against `main` here.** The public CI tests it: the test suite on Linux, Windows and macOS and both
   clippy runs, the same checks a release must pass.
2. **Review and merge.** The maintainer reviews it and, once the checks pass, it is merged on GitHub as one squash
   commit, with you as its author.
3. **Import.** atlas is developed in a private repository and published here one release at a time. After the
   merge, the maintainer runs `cargo xtask release import`, which brings your commit into the development
   repository with its author kept, through a pull request there. A release refuses to go out while a merged
   change here is missing from the development repository, so nothing merged here is lost or reverted by the next
   export.
4. **Ship.** Your change goes out in the next release, and its CHANGELOG line credits you.

## Before you open it

- Run `cargo fmt` on the files you touched, both clippy runs the CI makes
  (`cargo +1.99.0 clippy --workspace --locked --all-targets --features atlas/dev -- -D warnings` and
  `cargo +1.99.0 clippy --workspace --locked -- -D warnings`) and `cargo test --workspace`. The wiki's Testing and
  coverage page has the details.
- If your change is one a user would notice, add a changelog fragment, `changes/<number>.md`, named for your pull
  request's number (open the pull request first to get it). `changes/README.md` has the format: front matter
  naming the bump and the section, then the lines for users. When it is imported it is renamed
  `changes/public-<number>.md` and your GitHub login is added to it, so the CHANGELOG line names you.
- Leave `Co-authored-by:` lines out: the public history may carry only your own address, so the maintainer
  deletes any such line when merging.
- Commit under an address you are happy to see in the public history: the squash commit carries your GitHub
  account's address as GitHub sets it (your `users.noreply.github.com` address if you keep yours private).

## Security issues

Do not open a pull request or an issue for a vulnerability: see `SECURITY.md`.

## License

By contributing you agree that your contribution is licensed under the project's license, Apache 2.0 (`LICENSE`).
