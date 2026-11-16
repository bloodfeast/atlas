Closes #

What it does, and why:

Tests (red before green):

Checklist:

- [ ] A fragment in `changes/` (`changes/<pr>.md`, or `changes/<pr>-<slug>.md` per section; see `changes/README.md`), or `changes/<pr>.none` for no user-facing change.
- [ ] Changed behaviour? Update the atlas skill's prose (`skills/atlas/SKILL.md`) and run `cargo xtask skill`.
- [ ] A token cut? The memory fidelity eval before and after (`cargo xtask memory-eval`), if you can run it: it needs a memory corpus of your own.
