//! `project::checkout` against git itself, on the layouts the real `git`
//! writes. The hand-made layouts in tests/code.rs copy one git version's
//! files; this keeps them honest when git changes its formats. The oracle is
//! `git rev-parse --path-format=absolute --show-toplevel --git-common-dir`.
//! Skipped when git is missing; `--relative-paths` needs git 2.48 and
//! `--orphan` 2.42.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use atlas::project::{checkout, path_key};
use common::*;

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main", "-c", "protocol.file.allow=always", "-C"])
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(o.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// git's version as (major, minor), or `None` without git.
fn version() -> Option<(u32, u32)> {
    let o = Command::new("git").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let mut n = text.split_whitespace().nth(2)?.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    Some((n.next()?, n.next()?))
}

fn oracle(dir: &Path) -> (String, String) {
    let out = git(dir, &["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-common-dir"]);
    let mut l = out.lines();
    (path_key(Path::new(l.next().unwrap())), path_key(Path::new(l.next().unwrap())))
}

fn ours(dir: &Path) -> Option<(String, String)> {
    checkout(dir).map(|c| (path_key(&c.top), c.common))
}

fn repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    write(&dir.join("a.rs"), "pub fn a() {}\n");
    git(dir, &["add", "a.rs"]);
    git(dir, &["commit", "-qm", "one"]);
}

#[test]
fn checkout_agrees_with_git_on_the_layouts_git_writes() {
    let Some(v) = version() else {
        eprintln!("skipped: no git");
        return;
    };
    let t = Temp::new("wt-vs-git");
    let p = t.path();
    let n = p.join("n");
    repo(&n);
    let mut agree: Vec<(&str, std::path::PathBuf)> = vec![("a main checkout", n.clone())];
    git(&n, &["worktree", "add", "-q", ".claude/worktrees/a"]);
    agree.push(("a worktree under a dot folder", n.join(".claude/worktrees/a")));
    if v >= (2, 48) {
        git(&n, &["worktree", "add", "-q", "--relative-paths", ".claude/worktrees/rel"]);
        agree.push(("--relative-paths", n.join(".claude/worktrees/rel")));
    }
    if v >= (2, 42) {
        git(&n, &["worktree", "add", "-q", "--orphan", "-b", "fresh", &p.join("orphan").to_string_lossy()]);
        agree.push(("--orphan", p.join("orphan")));
    }
    git(&n, &["worktree", "add", "-q", "-b", "moving", &p.join("before").to_string_lossy()]);
    git(&n, &["worktree", "move", &p.join("before").to_string_lossy(), &p.join("moved").to_string_lossy()]);
    agree.push(("after git worktree move", p.join("moved")));
    git(p, &["clone", "-q", "--bare", &n.to_string_lossy(), &p.join("b/.bare").to_string_lossy()]);
    git(&p.join("b/.bare"), &["worktree", "add", "-q", &p.join("b/main").to_string_lossy(), "main"]);
    git(&p.join("b/.bare"), &["worktree", "add", "-q", "-b", "feat", &p.join("b/feat").to_string_lossy()]);
    agree.push(("a bare repository's worktree", p.join("b/main")));
    agree.push(("a bare repository's other worktree", p.join("b/feat")));
    fs::create_dir_all(p.join("s")).unwrap();
    git(&p.join("s"), &["init", "-q", "--separate-git-dir", &p.join("s-gitdir").to_string_lossy()]);
    write(&p.join("s/s.rs"), "pub fn s() {}\n");
    git(&p.join("s"), &["add", "s.rs"]);
    git(&p.join("s"), &["commit", "-qm", "one"]);
    git(&p.join("s"), &["worktree", "add", "-q", "-b", "w", &p.join("s-wt").to_string_lossy()]);
    agree.push(("a --separate-git-dir checkout", p.join("s")));
    agree.push(("its worktree", p.join("s-wt")));
    let sub = p.join("sub");
    repo(&sub);
    git(&n, &["submodule", "add", "-q", &sub.to_string_lossy(), "vendor/sub"]);
    agree.push(("a submodule", n.join("vendor/sub")));
    git(p, &["clone", "-q", "--shared", &n.to_string_lossy(), &p.join("shared").to_string_lossy()]);
    agree.push(("a clone --shared", p.join("shared")));
    for (what, dir) in &agree {
        assert_eq!(ours(dir), Some(oracle(dir)), "{what}: {}", dir.display());
    }
    // A clone --shared borrows objects but is another repository: it never
    // shares the source's common folder, so it never maps to its roots.
    assert_ne!(ours(&p.join("shared")).unwrap().1, ours(&n).unwrap().1);
    // A subfolder answers as its checkout.
    fs::create_dir_all(n.join(".claude/worktrees/a/deep/er")).unwrap();
    assert_eq!(ours(&n.join(".claude/worktrees/a/deep/er")), Some(oracle(&n.join(".claude/worktrees/a"))));
    // A plain mv leaves the admin folder naming the old place: atlas refuses
    // it (git still works there) until git worktree repair.
    fs::rename(p.join("moved"), p.join("by-hand")).unwrap();
    assert_eq!(ours(&p.join("by-hand")), None, "a worktree moved by hand");
    git(&p.join("by-hand"), &["worktree", "repair"]);
    assert_eq!(ours(&p.join("by-hand")), Some(oracle(&p.join("by-hand"))), "after git worktree repair");
}
