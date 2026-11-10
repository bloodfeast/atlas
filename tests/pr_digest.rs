//! PR digests (`lean.pr_digest`): the `gh pr` command reader, the diff split
//! and its exactness through the handle store, rounds, collapsing, the view
//! digests and the PostToolUse route.

mod common;

use std::path::Path;

use atlas::handles::{HandleCfg, Handles, Meta, Sel};
use atlas::hook_rewrite::{set_routing, Router};
use atlas::hooks::HookEvent;
use atlas::json;
use atlas::pr_digest::{self, Collapse, Git, Local, PrCmd, Sub, Target, Where};
use common::Temp;

fn store(t: &Temp) -> Handles {
    Handles::open(&t.path().join("index.sqlite"), HandleCfg::default()).unwrap()
}

fn wh(key: &str) -> Where {
    Where { key: key.into(), attributes: String::new(), toplevel: String::new() }
}

fn meta(cmd: &str) -> Meta {
    Meta { kind: "bash".into(), label: cmd.into(), cmd: cmd.into(), ..Meta::default() }
}

/// The parts' texts through `Handles::text`, in manifest order.
fn reassemble(h: &Handles, ids: &[String]) -> String {
    ids.iter().map(|id| h.text(id).unwrap().1).collect()
}

// ---- the command ----------------------------------------------------------

#[test]
fn gh_pr_commands_are_read_and_others_are_not() {
    let p = |c: &str| pr_digest::parse_cmd(c);
    let diff = |patch, repo: Option<&str>, target| Some(PrCmd { sub: Sub::Diff { patch }, repo: repo.map(str::to_string), target });
    assert_eq!(p("gh pr diff 12"), diff(false, None, Target::Number(12)));
    assert_eq!(p("gh pr diff #12 --patch"), diff(true, None, Target::Number(12)));
    assert_eq!(p("cd /x && GH_PAGER= gh pr diff 12 -R Octo/Demo"), diff(false, Some("octo/demo"), Target::Number(12)));
    assert_eq!(p("gh pr diff --repo=octo/demo feature/x"), diff(false, Some("octo/demo"), Target::Branch("feature/x".into())));
    assert_eq!(p("gh pr diff https://github.com/Octo/Demo/pull/34/files"), diff(false, Some("octo/demo"), Target::Number(34)));
    assert_eq!(p("gh pr diff --color never"), diff(false, None, Target::Current));
    assert_eq!(p("gh pr diff -R ghe.example.com/a/b 3"), diff(false, Some("ghe.example.com/a/b"), Target::Number(3)));
    assert_eq!(p("gh pr view 5 --comments").map(|c| c.sub), Some(Sub::View { comments: true, json: false }));
    assert_eq!(p("gh pr view 5 --json title,body").map(|c| c.sub), Some(Sub::View { comments: false, json: true }));
    assert_eq!(p("gh pr view 5 --json reviews,number").map(|c| c.sub), Some(Sub::View { comments: false, json: true }));
    for no in [
        "gh pr diff 12 --name-only",
        "gh pr diff 12 --color always",
        "gh pr diff 12 | head",
        "gh pr diff 12 > a.diff",
        "gh pr diff 12; echo",
        "gh pr diff 12 && echo",
        "gh pr diff 1 2",
        "gh pr diff --web",
        "gh pr view 5 --json number,headRefOid",
        "gh pr view 5 --json body --jq .body",
        "gh pr view 5 -t '{{.body}}'",
        "gh pr view 5 --web",
        "gh pr checkout 5",
        "gh issue view 5",
        "git diff",
        "gh pr diff 'unclosed",
        "gh pr diff https://example.com/x",
        "gh pr diff 12>out.diff",
        "gh pr diff 12|head",
        "cd ../other && gh pr diff 5",
        "GH_REPO=o/other gh pr diff 5",
        "gh pr diff 5 --patch=false",
        "gh pr view 5 --comments=false",
    ] {
        assert_eq!(p(no), None, "{no}");
    }
}

#[test]
fn repositories_normalize_from_flags_urls_and_remotes() {
    let n = pr_digest::norm_repo;
    assert_eq!(n("Octo/Demo").as_deref(), Some("octo/demo"));
    assert_eq!(n("https://github.com/octo/demo.git").as_deref(), Some("octo/demo"));
    assert_eq!(n("git@github.com:octo/demo.git").as_deref(), Some("octo/demo"));
    assert_eq!(n("ssh://git@github.com/octo/demo").as_deref(), Some("octo/demo"));
    assert_eq!(n("https://ghe.example.com/octo/demo").as_deref(), Some("ghe.example.com/octo/demo"));
    assert_eq!(n("ssh://git@github.com:22/octo/demo.git").as_deref(), Some("octo/demo"), "a port is not an owner");
    assert_eq!(n("https://ghe.example.com:8443/octo/demo").as_deref(), Some("ghe.example.com/octo/demo"));
    assert_eq!(pr_digest::parse_cmd("cd ../other && gh pr diff 5 -R a/b").map(|c| c.repo), Some(Some("a/b".into())), "-R still places it");
    assert_eq!(n("demo"), None);
    assert_eq!(n("octo/de mo"), None);
    let remotes = "remote.origin.url git@github.com:me/demo.git\nremote.upstream.url https://github.com/octo/demo\n";
    assert_eq!(pr_digest::remote_repo(remotes).as_deref(), Some("octo/demo"), "upstream before origin");
    let resolved = format!("{remotes}remote.origin.gh-resolved base\n");
    assert_eq!(pr_digest::remote_repo(&resolved).as_deref(), Some("me/demo"), "gh-resolved base wins");
    let named = format!("{remotes}remote.origin.gh-resolved other/repo\n");
    assert_eq!(pr_digest::remote_repo(&named).as_deref(), Some("other/repo"));
    assert_eq!(pr_digest::remote_repo("remote.fork.url https://github.com/a/b\n").as_deref(), Some("a/b"));
    assert_eq!(pr_digest::remote_repo(""), None);
}

struct Stub(Option<Local>);

impl Git for Stub {
    fn local(&self, _cwd: &str) -> Option<Local> {
        self.0.clone()
    }
}

#[test]
fn the_pr_is_placed_without_the_network() {
    let t = Temp::new("prd-where");
    std::fs::write(t.path().join(".gitattributes"), "gen/** linguist-generated\n").unwrap();
    let local = Local { remotes: "remote.origin.url https://github.com/octo/demo\n".into(), branch: "topic".into(), toplevel: t.path().to_string_lossy().into() };
    let here = Stub(Some(local.clone()));
    let cmd = |c: &str| pr_digest::parse_cmd(c).unwrap();
    let w = pr_digest::resolve(&cmd("gh pr diff 9"), "x", &here).unwrap();
    assert_eq!(w.key, "octo/demo#9");
    assert!(w.attributes.contains("linguist-generated"), "the checkout is the PR's repository");
    assert_eq!(w.toplevel, t.path().to_string_lossy(), "its working tree is where file heads are read");
    assert_eq!(pr_digest::resolve(&cmd("gh pr diff"), "x", &here).unwrap().key, "octo/demo@topic");
    assert_eq!(pr_digest::resolve(&cmd("gh pr diff --patch 9"), "x", &here).unwrap().key, "octo/demo#9 patch");
    let other = pr_digest::resolve(&cmd("gh pr diff 9 -R other/repo"), "x", &here).unwrap();
    assert_eq!(other.key, "other/repo#9");
    assert_eq!(other.attributes, "", "another repository's attributes are not this checkout's");
    assert_eq!(other.toplevel, "", "nor its working tree");
    // Nothing to go on: the generic route.
    assert_eq!(pr_digest::resolve(&cmd("gh pr diff 9"), "x", &Stub(None)), None);
    let detached = Stub(Some(Local { branch: "HEAD".into(), ..local }));
    assert_eq!(pr_digest::resolve(&cmd("gh pr diff"), "x", &detached), None);
    assert_eq!(pr_digest::resolve(&cmd("gh pr diff 4 -R a/b"), "x", &Stub(None)).unwrap().key, "a/b#4");
}

// ---- splitting and exactness ---------------------------------------------

/// Every shape a file can take in a diff: two hunks (one with a removed `-- `
/// line), a rename with similarity and no final newline on either side, a
/// binary file, a mode change, an empty new file, a copy, and a deletion
/// whose removed line looks like a diff header.
const MIXED: &str = "diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@ mod a;
 fn a() {}
-fn b() {}
+fn b() { 1 }
+fn c() {}
 fn d() {}
@@ -10,2 +11,2 @@
 x
--
+-- y
diff --git a/old name.txt b/new name.txt
similarity index 90%
rename from old name.txt
rename to new name.txt
index 3333333..4444444 100644
--- a/old name.txt
+++ b/new name.txt
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
diff --git a/img.png b/img.png
index 5555555..6666666 100644
Binary files a/img.png and b/img.png differ
diff --git a/run.sh b/run.sh
old mode 100644
new mode 100755
diff --git a/empty.txt b/empty.txt
new file mode 100644
index 0000000..e69de29
diff --git a/copy.rs b/copy2.rs
similarity index 100%
copy from copy.rs
copy to copy2.rs
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
index 7777777..0000000
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-a
-diff --git a/fake b/fake
";

fn files(text: &str) -> Vec<pr_digest::FileDiff> {
    pr_digest::split_diff(text).into_iter().filter_map(|p| p.file).collect()
}

#[test]
fn a_diff_splits_into_files_that_cover_it_exactly() {
    let parts = pr_digest::split_diff(MIXED);
    let joined: String = parts.iter().map(|p| &MIXED[p.start..p.end]).collect();
    assert_eq!(joined, MIXED);
    assert!(parts.windows(2).all(|w| w[0].end == w[1].start), "contiguous");
    let f = files(MIXED);
    let row = |f: &pr_digest::FileDiff| (f.path.clone(), f.status, f.add, f.del, f.hunks.clone());
    assert_eq!(
        f.iter().map(row).collect::<Vec<_>>(),
        vec![
            ("src/lib.rs".into(), 'M', 3, 2, vec![(5, 10), (11, 14)]),
            ("new name.txt".into(), 'R', 1, 1, vec![(8, 12)]),
            ("img.png".into(), 'M', 0, 0, vec![]),
            ("run.sh".into(), 'M', 0, 0, vec![]),
            ("empty.txt".into(), 'A', 0, 0, vec![]),
            ("copy2.rs".into(), 'C', 0, 0, vec![]),
            ("gone.rs".into(), 'D', 0, 2, vec![(6, 8)]),
        ]
    );
    assert_eq!(f[1].old_path, "old name.txt");
    assert_eq!(f[5].old_path, "copy.rs");
    assert!(f[2].binary && !f[0].binary);
    assert!(f[3].mode && !f[0].mode);
    assert_eq!(parts.len(), 7, "no text between files");
}

#[test]
fn line_endings_and_odd_bytes_survive_the_split() {
    let crlf = MIXED.replace('\n', "\r\n");
    let no_final = MIXED.trim_end_matches('\n').to_string();
    let mut bytes = MIXED.as_bytes().to_vec();
    let at = MIXED.find("+fn c() {}").unwrap() + 4;
    bytes.splice(at..at, [0xff, 0xfe, b'z']);
    let lossy = String::from_utf8_lossy(&bytes).into_owned();
    for text in [&crlf, &no_final, &lossy] {
        let parts = pr_digest::split_diff(text);
        let joined: String = parts.iter().map(|p| &text[p.start..p.end]).collect();
        assert_eq!(&joined, text);
        let f = files(text);
        assert_eq!(f.len(), 7, "{text:?}");
        assert_eq!(f[0].hunks, vec![(5, 10), (11, 14)]);
        assert_eq!((f[6].path.as_str(), f[6].del), ("gone.rs", 2));
    }
}

#[test]
fn the_stored_parts_reassemble_the_output_byte_for_byte() {
    let t = Temp::new("prd-exact");
    let mut h = store(&t);
    // A hunk over the slice cap (8 KB) as well.
    let mut big = String::from("diff --git a/big.rs b/big.rs\nindex 1..2 100644\n--- a/big.rs\n+++ b/big.rs\n@@ -0,0 +1,400 @@\n");
    for i in 0..400 {
        big.push_str(&format!("+let value_{i:04} = compute_something_long({i});\n"));
    }
    let crlf = MIXED.replace('\n', "\r\n");
    let public = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/gh/ripgrep-3560.diff")).unwrap();
    for (n, text) in [format!("{MIXED}{big}"), crlf, MIXED.trim_end().to_string(), public].iter().enumerate() {
        let w = wh(&format!("octo/demo#{n}"));
        let d = pr_digest::diff(&mut h, &w, text, &meta("gh pr diff")).unwrap();
        assert_eq!(&reassemble(&h, &d.parts), text, "case {n}");
        assert_eq!(&h.text(&d.whole).unwrap().1, text, "the whole output is a handle too");
        let m = pr_digest::manifest(h.conn(), &w.key, d.round).unwrap();
        assert_eq!(m.iter().map(|e| e.handle.clone()).collect::<Vec<_>>(), d.parts, "the manifest keeps the order");
        // Every file row names its own handle.
        for e in m.iter().filter(|e| !e.path.is_empty()) {
            assert!(d.digest.contains(&format!(" {} ", e.path)) || d.digest.contains(&e.handle), "{}: {}", e.path, d.digest);
        }
    }
    // The model's view of a hunk over the cap is paged, never cut silently.
    let text = format!("{MIXED}{big}");
    let d = pr_digest::diff(&mut h, &wh("octo/demo#0"), &text, &meta("gh pr diff")).unwrap();
    let big_id = d.parts.last().unwrap();
    assert!(d.digest.contains(&format!("M big.rs +400 -0 {big_id} 1 hunk @ 5-405")), "{}", d.digest);
    let s = h.slice(big_id, &Sel::Lines(5, 405)).unwrap();
    assert!(s.starts_with("@@ -0,0 +1,400 @@\n") && s.ends_with(&format!("--lines {}-405\n", s.lines().count() + 4)), "{s}");
}

// ---- diff-native reads ----------------------------------------------------

/// A file over the slice cap with several hunks, and one hunk over it alone.
fn over_cap() -> String {
    let mut t = String::from("diff --git a/many.rs b/many.rs\nindex 1..2 100644\n--- a/many.rs\n+++ b/many.rs\n");
    for h in 0..6 {
        t.push_str(&format!("@@ -{0},3 +{0},103 @@ fn h{h}()\n ctx\n", h * 200 + 1));
        for i in 0..100 {
            t.push_str(&format!("+    let hunk_{h}_value_{i:03} = 1;\n"));
        }
        t.push_str(" ctx\n ctx\n");
    }
    t.push_str("diff --git a/huge.rs b/huge.rs\nnew file mode 100644\nindex 0000000..3333333\n--- /dev/null\n+++ b/huge.rs\n@@ -0,0 +1,500 @@\n");
    for i in 0..500 {
        t.push_str(&format!("+pub const C{i:04}: u32 = {i};\n"));
    }
    t
}

/// Every page of a file read: `atlas slice h:F`, then each `next page`
/// (`--page K`), without the continuation lines.
fn read_file(h: &Handles, id: &str) -> Vec<String> {
    let mut pages = Vec::new();
    let mut s = h.slice(id, &Sel::Whole).unwrap();
    loop {
        let (body, next) = match s.rfind("next page: atlas slice ") {
            Some(i) if s[i..].lines().count() == 1 => (s[..i].to_string(), Some(s[i..].to_string())),
            _ => (s.clone(), None),
        };
        pages.push(body);
        let Some(next) = next else { return pages };
        let k = next.trim_end().strip_prefix(&format!("next page: atlas slice {id} --page ")).unwrap_or_else(|| panic!("{next}"));
        s = h.slice(id, &Sel::Page(k.parse().unwrap())).unwrap();
    }
}

/// A default read put back into the stored bytes it stands for, without
/// the header lines the row reproduces: each `= N hidden --lines A-B`
/// marker becomes the N context lines of its one slice, and each change
/// block with a word diff (`~`) its `-` lines then
/// its `+` lines.
fn decode_read(h: &Handles, id: &str, read: &str) -> String {
    let lines: Vec<&str> = read.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if l.starts_with("@@ ") {
            out.push_str(l);
            i += 1;
            continue;
        }
        if let Some(m) = l.strip_prefix("= ").and_then(|m| m.strip_suffix('\n')) {
            let (n, range) = m.split_once(" hidden --lines ").unwrap_or_else(|| panic!("a marker: {l:?}"));
            let n: usize = n.parse().unwrap();
            let (a, b) = range.split_once('-').unwrap();
            let hidden = h.slice(id, &Sel::Lines(a.parse().unwrap(), b.parse().unwrap())).unwrap();
            let hl: Vec<&str> = hidden.split_inclusive('\n').collect();
            assert_eq!(hl.len(), n, "a marker's count is its slice's lines: {l:?}");
            for x in &hl {
                assert!(x.starts_with(' ') || x.trim_end_matches(['\r', '\n']).is_empty(), "only context is hidden: {x:?}");
                out.push_str(x);
            }
            i += 1;
            continue;
        }
        if l.starts_with(['-', '+', '~']) {
            let mut j = i;
            while j < lines.len() && lines[j].starts_with(['-', '+', '~']) {
                j += 1;
            }
            let run = &lines[i..j];
            if run.iter().any(|x| x.starts_with('~')) {
                let (mut old, mut new) = (String::new(), String::new());
                for x in run {
                    match pr_digest::wd_decode(x) {
                        Some((o, n)) => {
                            old.push_str(&o);
                            new.push_str(&n);
                        }
                        None if x.starts_with('-') => {
                            old.push_str(x);
                        }
                        None => {
                            assert!(x.starts_with('+'), "a word diff line decodes: {x:?}");
                            new.push_str(x);
                        }
                    }
                }
                out.push_str(&old);
                out.push_str(&new);
            } else {
                run.iter().for_each(|x| out.push_str(x));
            }
            i = j;
            continue;
        }
        out.push_str(l);
        i += 1;
    }
    out
}

/// A file read (every page, decoded) with the header lines its row stands
/// for put back where git prints them: the stored part, byte for byte.
fn rebuild_file(h: &Handles, row: &str, id: &str) -> (String, Vec<String>) {
    let pages = read_file(h, id);
    let joined = decode_read(h, id, &pages.concat());
    let (git, minus, plus) = row_lines(row);
    let mut rebuilt = String::new();
    if !joined.starts_with("diff --git ") {
        rebuilt.push_str(&git);
    }
    let first_hunk = joined.find("\n@@ ").map(|i| i + 1).or_else(|| joined.starts_with("@@ ").then_some(0));
    match first_hunk {
        Some(i) if !joined[..i].contains("\n+++ ") => {
            rebuilt.push_str(&joined[..i]);
            rebuilt.push_str(&minus);
            rebuilt.push_str(&plus);
            rebuilt.push_str(&joined[i..]);
        }
        _ => rebuilt.push_str(&joined),
    }
    (rebuilt, pages)
}

/// The header lines a file row (shown, seen hunks, lock, generated or
/// deleted) stands for, rebuilt from the row's own words: its status, path,
/// `(from OLD)` and `+a -d` before its first handle.
fn row_lines(row: &str) -> (String, String, String) {
    let row = ["lock ", "generated ", "deleted "].iter().find_map(|t| row.strip_prefix(t)).unwrap_or(row);
    let status = &row[..1];
    let before = &row[2..row.find(" h:").unwrap()];
    let named = before.rsplitn(3, ' ').nth(2).unwrap();
    let (path, old) = match named.split_once(" (from ") {
        Some((p, o)) => (p, o.trim_end_matches(')')),
        None => (named, named),
    };
    let (path, old) = (unnumbered(path), unnumbered(old));
    let minus = if status == "A" { "--- /dev/null\n".to_string() } else { format!("--- a/{old}\n") };
    let plus = if status == "D" { "+++ /dev/null\n".to_string() } else { format!("+++ b/{path}\n") };
    (format!("diff --git a/{old} b/{path}\n"), minus, plus)
}

/// A row's path without its ` (N)` (the N-th patch of a path under
/// `--patch`).
fn unnumbered(p: &str) -> &str {
    match p.rsplit_once(" (") {
        Some((base, n)) if n.strip_suffix(')').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) => base,
        _ => p,
    }
}

/// The first handle a digest line names.
fn first_handle(l: &str) -> Option<&str> {
    l.split(' ').find(|w| w.starts_with("h:"))
}

/// The handle the `unchanged:` line names for a path.
fn unchanged_handle(g: &str, path: &str) -> Option<String> {
    let l = g.lines().find(|l| l.starts_with("unchanged: "))?;
    let at = [": ", ", "].iter().find_map(|sep| l.find(&format!("{sep}{path} h:")).map(|i| i + sep.len() + path.len() + 1))?;
    Some(l[at..].split(' ').next()?.to_string())
}

/// A status row: `M path ...`.
fn status_row(l: &str) -> bool {
    l.chars().nth(1) == Some(' ') && l.chars().next().is_some_and(|c| "MADRC".contains(c))
}

/// A part read as stored: `--lines 1-N` and each `next page`.
fn read_raw(h: &Handles, id: &str) -> String {
    let mut out = String::new();
    let mut sel = Sel::Lines(1, usize::MAX);
    loop {
        let s = h.slice(id, &sel).unwrap();
        match s.rfind("next page: atlas slice ") {
            Some(i) if s[i..].lines().count() == 1 => {
                out.push_str(&s[..i]);
                let r = s[i..].trim_end().strip_prefix(&format!("next page: atlas slice {id} --lines ")).unwrap_or_else(|| panic!("{s}"));
                let (a, b) = r.split_once('-').unwrap();
                sel = Sel::Lines(a.parse().unwrap(), b.parse().unwrap());
            }
            _ => {
                out.push_str(&s);
                return out;
            }
        }
    }
}

/// How many parts a round rebuilt by each route.
type Kinds = std::collections::BTreeMap<&'static str, usize>;

/// A round's stored diff rebuilt from what its digest names, in part order
/// (the manifest gives the order, never the bytes):
/// * a shown file row: the header lines the row stands for, then every page
///   of its default read (`next page` lines dropped, `~` decoded);
/// * a row with seen hunks: the same from its `whole file h:X` handle;
/// * a lock, generated or deleted row: the same from its handle;
/// * an unchanged file: its listed handle's default read, with a plain
///   modification's header lines put back when the read leaves them out;
/// * a vendored file: its tree's handle (a one-file tree's default read is
///   checked to be its bytes);
/// * a commit row: its handle;
/// * text between files with no row, and files past the row limit (only when
///   the digest says more files were left out): the whole output's handle.
///
/// No default read may hold a context marker (`= N hidden`).
fn rebuild_round(h: &Handles, key: &str, d: &pr_digest::Digested) -> (String, Kinds) {
    let g = &d.digest;
    let whole = g.split_once(' ').unwrap().0;
    assert_eq!(whole, d.whole);
    assert!(g.lines().last().unwrap().ends_with(&format!("; whole output {whole}")), "the footer names the whole output: {g}");
    let whole_text = h.text(whole).unwrap().1;
    let whole_parts = pr_digest::split_diff(&whole_text);
    let entries = pr_digest::manifest(h.conn(), key, d.round).unwrap();
    assert_eq!(whole_parts.len(), entries.len());
    let more = g.lines().any(|l| l.contains(" more files: atlas slice ") || (l.starts_with("unchanged: ") && l.contains(" more")));
    let mut trees: Vec<(String, String)> = Vec::new();
    for l in g.lines().filter(|l| l.starts_with("vendored ")) {
        let id = first_handle(l).unwrap();
        let t = h.text(id).unwrap().1;
        if pr_digest::split_diff(&t).len() == 1 {
            // A one-file tree reads diff-native, its header kept: no row gives it.
            assert_eq!(decode_read(h, id, &read_file(h, id).concat()), t, "a one-file tree's read is its bytes: {l}");
        }
        for p in pr_digest::split_diff(&t) {
            trees.push((p.file.unwrap().path, t[p.start..p.end].to_string()));
        }
    }
    let read = |row: &str, id: &str| {
        let (rebuilt, pages) = rebuild_file(h, row, id);
        for p in &pages {
            assert!(!p.lines().any(|l| l.starts_with("= ")), "a context marker in the default read of {id}: {p}");
        }
        rebuilt
    };
    let mut kinds = Kinds::new();
    let mut out = String::new();
    for (i, e) in entries.iter().enumerate() {
        let names = |l: &&str| l.split(' ').any(|w| w == e.handle);
        let (kind, bytes): (&'static str, String) = if e.path.is_empty() {
            match g.lines().find(|l| l.starts_with("commit ") && names(l)) {
                Some(_) => ("commit", h.text(&e.handle).unwrap().1),
                None => ("text between files (whole output)", whole_text[whole_parts[i].start..whole_parts[i].end].to_string()),
            }
        } else if let Some(l) = g.lines().find(|l| status_row(l) && l.ends_with(&format!(", whole file {}", e.handle))) {
            ("seen hunks (whole file)", read(l, &e.handle))
        } else if let Some(l) = g.lines().find(|l| ["lock ", "generated ", "deleted "].iter().any(|t| l.starts_with(t)) && first_handle(l) == Some(e.handle.as_str())) {
            (["lock", "generated", "deleted"].into_iter().find(|t| l.starts_with(t)).unwrap(), read(l, &e.handle))
        } else if let Some(l) = g.lines().find(|l| status_row(l) && first_handle(l) == Some(e.handle.as_str())) {
            ("shown", read(l, &e.handle))
        } else if let Some(id) = unchanged_handle(g, &e.path) {
            // Its default read; the entry names only the path, so a header
            // the read leaves out is a plain modification's.
            ("unchanged", rebuild_file(h, &format!("M {} +0 -0 {id}", e.path), &id).0)
        } else if let Some(j) = trees.iter().position(|t| t.0 == unnumbered(&e.path)) {
            ("vendored", trees.remove(j).1)
        } else {
            assert!(more, "{} ({}) is reachable from no row: {g}", e.path, e.handle);
            ("past the row limit (whole output)", whole_text[whole_parts[i].start..whole_parts[i].end].to_string())
        };
        *kinds.entry(kind).or_default() += 1;
        out.push_str(&bytes);
    }
    (out, kinds)
}

#[test]
fn a_file_read_decodes_to_the_stored_bytes_paged_at_hunks() {
    let t = Temp::new("prd-native");
    let mut h = store(&t);
    let cap = HandleCfg::default().slice_bytes;
    let public = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/gh/ripgrep-3560.diff")).unwrap();
    let cases = [
        ("over the cap", over_cap()),
        ("crlf", format!("{MIXED}{}", over_cap()).replace('\n', "\r\n")),
        ("no final newline", MIXED.trim_end().to_string()),
        ("mixed", MIXED.to_string()),
        ("public", public),
        ("edits", edits()),
        ("edits crlf", edits().replace('\n', "\r\n")),
        ("edits no final newline", edits().trim_end().to_string()),
    ];
    for (n, (name, text)) in cases.iter().enumerate() {
        let key = format!("octo/native#{n}");
        let d = pr_digest::diff(&mut h, &wh(&key), text, &meta("gh pr diff")).unwrap();
        let m = pr_digest::manifest(h.conn(), &key, d.round).unwrap();
        for e in m.iter().filter(|e| !e.path.is_empty()) {
            let stored = h.text(&e.handle).unwrap().1;
            let row = d.digest.lines().find(|l| l.contains(&e.handle)).unwrap_or_else(|| panic!("{name}: no row for {}", e.path));
            let (rebuilt, pages) = rebuild_file(&h, row, &e.handle);
            assert_eq!(rebuilt, stored, "{name} {}: the row and the decoded pages are the stored part", e.path);
            let joined = pages.concat();
            if name.contains("crlf") {
                assert!(joined.starts_with("diff --git ") && joined.contains("\r\n"), "a CRLF header line is never hidden");
            } else {
                assert!(!joined.starts_with("diff --git "), "{name} {}: the header lines the row has are left out", e.path);
            }
            for p in &pages {
                assert!(p.len() <= cap, "{name} {}: a page over the cap", e.path);
            }
        }
        if *name == "over the cap" {
            let many = m.iter().find(|e| e.path == "many.rs").unwrap();
            let pages = read_file(&h, &many.handle);
            assert!(pages.len() >= 3 && pages[1..].iter().all(|p| p.starts_with("@@ ")), "pages break between hunks: {} pages", pages.len());
            assert!(pages[..pages.len() - 1].iter().all(|p| p.len() > cap / 2), "a page holds as many hunks as fit");
            let huge = m.iter().find(|e| e.path == "huge.rs").unwrap();
            let pages = read_file(&h, &huge.handle);
            assert!(pages.len() >= 2 && pages[1].starts_with("+pub const C"), "one hunk over the cap is paged between lines");
        }
    }
}

#[test]
fn a_hunk_read_is_its_stored_bytes() {
    let t = Temp::new("prd-hunk");
    let mut h = store(&t);
    for (n, text) in [MIXED.replace('\n', "\r\n"), MIXED.trim_end().to_string()].iter().enumerate() {
        let d = pr_digest::diff(&mut h, &wh(&format!("octo/hunk#{n}")), text, &meta("gh pr diff")).unwrap();
        let parts = pr_digest::split_diff(text);
        for (p, id) in parts.iter().zip(&d.parts) {
            let part = &text[p.start..p.end];
            let at: Vec<usize> = std::iter::once(0)
                .chain(part.split_inclusive('\n').scan(0, |s, l| {
                    *s += l.len();
                    Some(*s)
                }))
                .collect();
            for &(a, b) in &p.file.as_ref().unwrap().hunks {
                assert_eq!(h.slice(id, &Sel::Lines(a, b)).unwrap(), part[at[a - 1]..at[b]], "case {n} lines {a}-{b}");
            }
            let n_lines = at.len() - 1;
            assert_eq!(h.slice(id, &Sel::Lines(1, n_lines)).unwrap(), part, "case {n}: --lines keeps every header line");
        }
    }
    // Other selectors and other kinds keep their line numbers.
    let d = pr_digest::diff(&mut h, &wh("octo/hunk#9"), MIXED, &meta("gh pr diff")).unwrap();
    assert!(h.slice(&d.parts[0], &Sel::Grep { re: "^@@".into(), context: 0 }).unwrap().starts_with("5\t@@ "));
    assert!(h.slice(&d.whole, &Sel::Lines(1, 2)).unwrap().starts_with("1\tdiff --git "), "the whole output is not a file part");
    let b = h.put(b"one\ntwo\n", &meta("cat x")).unwrap();
    assert_eq!(h.slice(&b.id, &Sel::Whole).unwrap(), "1\tone\n2\ttwo\n");
    assert_eq!(h.slice(&b.id, &Sel::Lines(2, 2)).unwrap(), "2\ttwo\n");
}

/// Paired edits, long context, a line with every word-diff escape, an
/// unpaired line, a pair before `\ No newline at end of file`, and a
/// deleted file.
fn edits() -> String {
    let mut t = String::from("diff --git a/e.rs b/e.rs\nindex 1..2 100644\n--- a/e.rs\n+++ b/e.rs\n@@ -1,13 +1,12 @@ fn main()\n");
    t.push_str(" let context_one_is_long_enough_to_hide = 1;\n let context_two_is_long_enough_to_hide = 2;\n let near = 3;\n");
    t.push_str("-    let value = compute(alpha, beta);\n-    let other = thing();\n-    let gone = 0;\n");
    t.push_str("+    let value = compute(alpha, gamma);\n+    totally different words here\n");
    t.push_str(" let after = 4;\n let context_far_after_the_change_one = 5;\n let context_far_after_the_change_two = 6;\n");
    t.push_str("-    s = \"~ \\\\ [- -] {+ +} [-x-]\" + a;\n+    s = \"~ \\\\ [- -] {+ +} [-y-]\" + a;\n");
    t.push_str(" let tail_context_line_far_enough_one = 7;\n let tail_context_line_far_enough_two = 8;\n let tail_context_line_far_enough_three = 9;\n");
    t.push_str("@@ -40,1 +40,1 @@\n-let last = old_value;\n+let last = new_value;\n\\ No newline at end of file\n");
    t.push_str("diff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\nindex 3..0000000\n--- a/gone.rs\n+++ /dev/null\n@@ -1,3 +0,0 @@\n-a\n-b\n-c\n");
    t
}

#[test]
fn a_word_diff_line_round_trips_every_escape() {
    for (old, new) in [
        ("    let value = compute(alpha, beta);", "    let value = compute(alpha, gamma);"),
        ("s = \"~ \\ [- -] {+ +} [-x-] {+y+}\" + a;", "s = \"~ \\ [- -] {+ +} [-z-] {+y+}\" + b;"),
        ("x[-1] + y[-2]", "x[-1] + y[-3]"),
        ("a-] b+} c\\", "a-] b+} d\\"),
        ("ends with cr\r", "ends with CR\r"),
        ("only old has cr\r", "only old has cr"),
    ] {
        let w = pr_digest::wd_line(old, new).unwrap_or_else(|| panic!("{old:?} pairs"));
        assert!(w.starts_with('~') && w.ends_with('\n') && w.matches('\n').count() == 1, "{w:?}");
        assert_eq!(pr_digest::wd_decode(&w), Some((format!("-{old}\n"), format!("+{new}\n"))), "{w:?}");
    }
    let w = pr_digest::wd_line("let a = f(x);", "let a = f(y);").unwrap();
    assert_eq!(w, "~let a = f([-x-]{+y+});\n");
    assert_eq!(pr_digest::wd_line("let a = 1;", "totally different words here"), None, "under the similarity");
    assert_eq!(pr_digest::wd_decode("~[-x\n"), None, "an open segment is not a word diff");
    assert_eq!(pr_digest::wd_decode("-x\n"), None);
}

#[test]
fn similarity_is_difflib_ratio_over_word_tokens() {
    // Python: difflib.SequenceMatcher(None, ta, tb, autojunk=False).ratio(),
    // ta and tb from re.findall(r'\w+|\s+|[^\w\s]', line).
    for (a, b, want) in [
        ("let a = f(x);", "let a = f(y);", 0.9090909090909091),
        ("    let value = compute(alpha, beta);", "    let value = compute(alpha, gamma);", 0.9333333333333333),
        ("let a = 1;", "totally different words here", 0.4),
        ("a b a b a", "b a b a b", 0.7777777777777778),
        ("    let other = thing();", "    totally different words here", 0.42105263157894735),
        ("x.y(z)", "", 0.0),
    ] {
        assert!((pr_digest::similarity(a, b) - want).abs() < 1e-12, "{a:?} {b:?}: {} vs {want}", pr_digest::similarity(a, b));
    }
}

#[test]
fn the_shipped_read_shows_word_diffs_and_hides_no_context() {
    assert_eq!(pr_digest::levers(), pr_digest::SHIPPED, "the suite runs the shipped levers");
    assert_eq!(pr_digest::SHIPPED, pr_digest::Levers { wd: true, c1: false, del: true });
    let t = Temp::new("prd-shipped");
    let mut h = store(&t);
    let text = edits();
    let d = pr_digest::diff(&mut h, &wh("octo/shipped#1"), &text, &meta("gh pr diff 1")).unwrap();
    let id = &d.parts[0];
    let read = h.slice(id, &Sel::Whole).unwrap();
    let at: Vec<&str> = text.split_inclusive('\n').collect();
    let want = [
        at[1],
        &at[4..8].concat(),
        "~    let value = compute(alpha, [-beta-]{+gamma+});\n",
        "-    let other = thing();\n",
        "+    totally different words here\n",
        "-    let gone = 0;\n",
        &at[13..16].concat(),
        "~    s = \"\\~ \\\\\\\\ \\[- \\-] \\{+ \\+} \\[-[-x-]{+y+}\\-]\" + a;\n",
        &at[18..25].concat(),
    ]
    .concat();
    assert_eq!(read, want, "{read}");
    assert!(!read.lines().any(|l| l.starts_with("= ")), "no context marker");
    assert_eq!(decode_read(&h, id, &read), [at[1], &at[4..25].concat()].concat());
    let shown = pr_digest::shown_lines(&h.text(id).unwrap().1, pr_digest::levers(), HandleCfg::default().slice_bytes).unwrap();
    assert!(shown.iter().skip(1).all(|&s| s), "every line is in the read");
    let footer = d.digest.lines().last().unwrap();
    assert!(footer.contains("; in a file read ~ is an edited line as [-old-]{+new+}; whole output h:") && !footer.contains('='), "{footer}");
}

/// One file part with one hunk of `body` (which may end without a newline).
fn one_hunk(path: &str, body: &str) -> (String, pr_digest::FileDiff) {
    let count = |c: char| body.split_inclusive('\n').filter(|l| l.starts_with([' ', c])).count();
    let text = format!("diff --git a/{path} b/{path}\nindex 1..2 100644\n--- a/{path}\n+++ b/{path}\n@@ -1,{} +1,{} @@\n{body}", count('-'), count('+'));
    let f = pr_digest::split_diff(&text).into_iter().find_map(|p| p.file).unwrap();
    (text, f)
}

/// A part from its `@@` line: what a read shows as stored.
fn hunk_as_stored(text: &str) -> &str {
    &text[text.find("@@ ").unwrap()..]
}

/// The shipped default read of a part's hunks (the header unit left out).
fn shipped_read(text: &str, f: &pr_digest::FileDiff, cap: usize) -> String {
    pr_digest::view(text, f, pr_digest::SHIPPED, cap).iter().skip(1).flatten().map(|p| p.text.as_str()).collect()
}

/// An edited one-line JSON of about 500 KB: the word diff is quadratic in
/// tokens, so it is never tried, at any cap. Before the bound this hung.
#[test]
fn a_huge_edited_line_is_shown_as_stored() {
    let json = |v: u32| format!("{{\"data\":[{}{v}]}}", (0..70_000).map(|i| format!("{i},")).collect::<String>());
    let (old, new) = (json(1), json(2));
    assert!(old.len() > 400_000);
    let body = format!("-{old}\n+{new}\n");
    let (text, f) = one_hunk("d.json", &body);
    for cap in [usize::MAX, HandleCfg::default().slice_bytes] {
        let read = shipped_read(&text, &f, cap);
        assert_eq!(read, hunk_as_stored(&text), "cap {cap}: the - line, then the + line, as stored");
    }
}

/// A pair at [`pr_digest::WD_MAX_LINE`] bytes still pairs; one byte over on
/// either side, it does not.
#[test]
fn the_word_diff_bound_is_on_each_stored_line() {
    let line = |len: usize, last: &str| {
        let words = "word ".repeat((len - 2 - last.len()) / 5);
        let pad = "x".repeat(len - 2 - last.len() - words.len());
        format!("{words}{pad}{last}")
    };
    let max = pr_digest::WD_MAX_LINE;
    let cap = usize::MAX;
    let (a, b) = (line(max, "a"), line(max, "b"));
    assert_eq!(format!("-{a}\n").len(), max);
    let (text, f) = one_hunk("w.rs", &format!("-{a}\n+{b}\n"));
    assert!(shipped_read(&text, &f, cap).contains("\n~"), "at the bound it pairs");
    for (old, new) in [(line(max + 1, "aa"), b.clone()), (a.clone(), line(max + 1, "bb"))] {
        let body = format!("-{old}\n+{new}\n");
        let (text, f) = one_hunk("w.rs", &body);
        assert!(pr_digest::similarity(&old, &new) > 0.9);
        assert_eq!(shipped_read(&text, &f, cap), hunk_as_stored(&text), "one byte over: as stored");
    }
}

/// Pairs at a similarity of 0.6 or more, each about a million matcher steps
/// (one punctuation token repeated, against the same with its last token
/// changed), as 40 blocks: the read spends at most
/// [`pr_digest::WD_READ_STEPS`] over all of them, so the first 16 pairs are
/// word diffs and every pair after the budget is spent is shown as stored.
/// Without the shared budget all 40 would pair.
#[test]
fn a_reads_pairs_share_one_step_budget() {
    let (old, new) = (",".repeat(1000), format!("{}x", ",".repeat(999)));
    assert!(pr_digest::similarity(&old, &new) >= 0.6);
    let block = format!("-{old}
+{new}
 ctx
");
    let (text, f) = one_hunk("p.min.js", &block.repeat(40));
    let read = shipped_read(&text, &f, usize::MAX);
    let wd = read.lines().filter(|l| l.starts_with('~')).count();
    assert_eq!(wd, 16, "pairs word-diffed before the budget ran out");
    assert!(read.ends_with(&block.repeat(40 - 16)), "every later pair as stored");
}

/// A block's word diff is shown when it fits the cap, its floor (the longer
/// stored line per position) under it or not.
#[test]
fn a_word_diff_block_is_shown_up_to_the_cap() {
    let body = "-let a = f(x);\n+let a = f(y);\n";
    let (text, f) = one_hunk("c.rs", body);
    let wd = "~let a = f([-x-]{+y+});\n";
    assert_eq!(shipped_read(&text, &f, wd.len()), format!("@@ -1,1 +1,1 @@\n{wd}"), "exactly the cap");
    assert_eq!(shipped_read(&text, &f, wd.len() - 1), hunk_as_stored(&text), "over the cap: as stored");
}

/// A pair whose `+` line has no newline and no `\ No newline` marker (a
/// diff cut at its end) is never paired: the read is as stored.
#[test]
fn a_pair_without_a_final_newline_is_not_paired() {
    let body = "-let x = f(a);\n+let x = f(b);";
    let (text, f) = one_hunk("n.rs", body);
    assert_eq!(shipped_read(&text, &f, usize::MAX), hunk_as_stored(&text));
}

/// Change lines interleaved (`-`, `+`, `-`, `+`, or a `-` run after a `+`)
/// are not a change block: no word diff, every line as stored.
#[test]
fn only_a_closed_change_block_pairs() {
    for body in ["-a b c d\n+a b c e\n-x y z w\n+x y z v\n", " c\n+p q r s\n-t u v w\n+t u v x\n"] {
        let (text, f) = one_hunk("i.rs", body);
        assert_eq!(shipped_read(&text, &f, usize::MAX), hunk_as_stored(&text), "{body}");
    }
    // The same pairs alone do pair.
    let (text, f) = one_hunk("i.rs", "-t u v w\n+t u v x\n");
    assert_eq!(shipped_read(&text, &f, usize::MAX), "@@ -1,1 +1,1 @@\n~t u v [-w-]{+x+}\n");
}

/// `--lines` over the cap pages between hunks: every `next page` starts at
/// a hunk's `@@` line.
#[test]
fn a_raw_read_pages_at_hunk_starts() {
    let t = Temp::new("prd-raw-pages");
    let mut h = store(&t);
    let text = over_cap();
    let d = pr_digest::diff(&mut h, &wh("octo/raw#1"), &text, &meta("gh pr diff")).unwrap();
    let id = &d.parts[0];
    let part = h.text(id).unwrap().1;
    let lines: Vec<&str> = part.split_inclusive('\n').collect();
    let mut sel = Sel::Lines(1, lines.len());
    let mut pages = 0;
    loop {
        let s = h.slice(id, &sel).unwrap();
        pages += 1;
        let Some(i) = s.rfind("next page: atlas slice ") else { break };
        let r = s[i..].trim_end().strip_prefix(&format!("next page: atlas slice {id} --lines ")).unwrap();
        let (a, b) = r.split_once('-').unwrap();
        let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
        assert!(lines[a - 1].starts_with("@@ "), "page {pages} continues at a hunk: line {a} {:?}", lines[a - 1]);
        sel = Sel::Lines(a, b);
    }
    assert!(pages >= 3, "{pages} pages");
    assert_eq!(read_raw(&h, id), part);
}

/// A file head is read only from a regular file: never through a symlink to
/// a device.
#[cfg(unix)]
#[test]
fn a_file_head_reads_only_a_regular_file() {
    let t = Temp::new("prd-head-dev");
    std::os::unix::fs::symlink("/dev/zero", t.path().join("zero")).unwrap();
    assert_eq!(pr_digest::file_head(&t.path().to_string_lossy(), "zero"), "");
}

/// A deleted file with no hunks (an empty file) keeps its status row: a
/// `deleted` row would save nothing.
#[test]
fn a_deleted_empty_file_is_a_status_row() {
    let t = Temp::new("prd-del-empty");
    let mut h = store(&t);
    let text = format!("{}diff --git a/e.txt b/e.txt\ndeleted file mode 100644\nindex e69de29..0000000\n", file("a.rs", 1, "1..2", " a\n+b\n"));
    let d = pr_digest::diff(&mut h, &wh("octo/del-empty#1"), &text, &meta("gh pr diff")).unwrap();
    assert!(d.digest.contains(&format!("\nD e.txt +0 -0 {} no hunks\n", d.parts[1])), "{}", d.digest);
    assert!(!d.digest.contains("\ndeleted "), "{}", d.digest);
}

/// A handle no row backs reads with its header: a one-file vendored tree,
/// and an unchanged entry that is not a plain modification (a plain one
/// keeps the handle its row named).
#[test]
fn a_read_no_row_backs_keeps_its_header() {
    let t = Temp::new("prd-unbacked");
    let mut h = store(&t);
    let w = wh("octo/unbacked#1");
    let renamed = "diff --git a/was.rs b/now.rs\nsimilarity index 90%\nrename from was.rs\nrename to now.rs\nindex 1..2 100644\n--- a/was.rs\n+++ b/now.rs\n@@ -1,1 +1,1 @@\n-w\n+n\n";
    let added = "diff --git a/add.rs b/add.rs\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/add.rs\n@@ -0,0 +1 @@\n+x\n";
    let keep = file("keep.rs", 1, "1..2", " k\n+k2\n");
    let r1 = [renamed, added, &keep, &file("vendor/z.go", 1, "1..2", " z\n+z2\n")].concat();
    let one = pr_digest::diff(&mut h, &w, &r1, &meta("gh pr diff 1")).unwrap();
    assert!(!h.slice(&one.parts[0], &Sel::Whole).unwrap().starts_with("diff --git"), "a row backs it: {}", one.digest);
    let tree = one.digest.lines().find(|l| l.starts_with("vendored vendor/ 1 files ")).unwrap_or_else(|| panic!("{}", one.digest));
    let tree_id = first_handle(tree).unwrap();
    assert_eq!(h.slice(tree_id, &Sel::Whole).unwrap(), file("vendor/z.go", 1, "1..2", " z\n+z2\n"), "a one-file tree reads whole");
    let r2 = [renamed, added, &keep, &file("other.rs", 1, "1..2", " o\n+o2\n")].concat();
    let two = pr_digest::diff(&mut h, &w, &r2, &meta("gh pr diff 1")).unwrap();
    let g = &two.digest;
    assert_eq!(unchanged_handle(g, "keep.rs").as_deref(), Some(two.parts[2].as_str()), "a plain modification keeps its handle: {g}");
    for (path, i, stored) in [("now.rs", 0, renamed), ("add.rs", 1, added)] {
        let id = unchanged_handle(g, path).unwrap_or_else(|| panic!("{path}: {g}"));
        assert_ne!(id, two.parts[i], "{path}");
        assert_eq!(h.slice(&id, &Sel::Whole).unwrap(), stored, "{path}: its read keeps the header");
        assert_eq!(h.slice(&id, &Sel::Lines(1, 2)).unwrap(), stored.split_inclusive('\n').take(2).collect::<String>());
    }
}

/// C1 (context collapse) is measurement-only: reached here through
/// [`pr_digest::view`] with the lever named, never through `levers()`.
#[test]
fn context_collapse_hides_far_context_behind_a_marker() {
    let t = Temp::new("prd-levers");
    let mut h = store(&t);
    let text = edits();
    let d = pr_digest::diff(&mut h, &wh("octo/levers#1"), &text, &meta("gh pr diff 1")).unwrap();
    let id = &d.parts[0];
    let part = h.text(id).unwrap().1;
    let f = pr_digest::split_diff(&part).into_iter().find_map(|p| p.file).unwrap();
    let cap = HandleCfg::default().slice_bytes;
    let c1 = pr_digest::Levers { wd: true, c1: true, del: true };
    let read: String = pr_digest::view(&part, &f, c1, cap).iter().flatten().map(|p| p.text.as_str()).collect();
    let want = [
        "index 1..2 100644\n",
        "@@ -1,13 +1,12 @@ fn main()\n",
        "= 2 hidden --lines 6-7\n",
        " let near = 3;\n",
        "~    let value = compute(alpha, [-beta-]{+gamma+});\n",
        "-    let other = thing();\n",
        "+    totally different words here\n",
        "-    let gone = 0;\n",
        " let after = 4;\n",
        "= 1 hidden --lines 15-15\n",
        " let context_far_after_the_change_two = 6;\n",
        "~    s = \"\\~ \\\\\\\\ \\[- \\-] \\{+ \\+} \\[-[-x-]{+y+}\\-]\" + a;\n",
        " let tail_context_line_far_enough_one = 7;\n",
        "= 2 hidden --lines 20-21\n",
        "@@ -40,1 +40,1 @@\n",
        "-let last = old_value;\n",
        "+let last = new_value;\n",
        "\\ No newline at end of file\n",
    ]
    .concat();
    assert_eq!(read, want, "{read}");
    // The hunk read is the stored bytes, and a marker's hunk holds its lines.
    let at: Vec<&str> = text.split_inclusive('\n').collect();
    assert_eq!(h.slice(id, &Sel::Lines(5, 21)).unwrap(), at[4..21].concat());
    assert_eq!(decode_read(&h, id, &read), [at[1], &at[4..25].concat()].concat());
    // What the proxy counts as shown.
    let shown = pr_digest::shown_lines(&part, c1, cap).unwrap();
    let hidden: Vec<usize> = (1..shown.len()).filter(|&k| !shown[k]).collect();
    assert_eq!(hidden, vec![6, 7, 15, 20, 21]);
    let no_c1 = pr_digest::shown_lines(&part, pr_digest::Levers { c1: false, ..c1 }, cap).unwrap();
    assert!(no_c1.iter().all(|&s| s));
    // A deleted file is a row; its handle still reads whole.
    let gone = &d.parts[1];
    assert!(d.digest.contains(&format!("\ndeleted D gone.rs +0 -3 {gone}\n")), "{}", d.digest);
    assert!(!d.digest.contains("\nD gone.rs"), "{}", d.digest);
    assert_eq!(h.slice(gone, &Sel::Whole).unwrap(), "deleted file mode 100644\nindex 3..0000000\n@@ -1,3 +0,0 @@\n-a\n-b\n-c\n");
    // --page on another kind is its lines from K, numbered.
    let b = h.put(b"one\ntwo\nthree\n", &meta("cat y")).unwrap();
    assert_eq!(h.slice(&b.id, &Sel::Page(2)).unwrap(), "2\ttwo\n3\tthree\n");
    assert_eq!(atlas::handles::parse_sel(&["--page".to_string(), "7".to_string()]), Ok(Sel::Page(7)));
}

/// A file of hunks, each `(old start, body)`.
fn hunks(path: &str, index: &str, hs: &[(u32, &str)]) -> String {
    let mut t = format!("diff --git a/{path} b/{path}\nindex {index} 100644\n--- a/{path}\n+++ b/{path}\n");
    for (at, body) in hs {
        let n = body.lines().count();
        t.push_str(&format!("@@ -{at},1 +{at},{n} @@ fn ctx()\n{body}"));
    }
    t
}

#[test]
fn a_changed_file_shows_only_hunks_no_earlier_round_had() {
    let t = Temp::new("prd-reuse");
    let mut h = store(&t);
    let w = wh("octo/reuse#1");
    let (ha, hb, hc) = (" a\n+a2\n", " b\n+b2\n", " c\n+c2\n");
    let r1 = hunks("x.rs", "1..2", &[(1, ha), (20, hb), (40, hc)]);
    pr_digest::diff(&mut h, &w, &r1, &meta("gh pr diff 1")).unwrap();
    // Round 2: the first hunk changed, one added, the rest moved, one gone.
    let hd = " d\n+d2\n+d3\n";
    let r2 = hunks("x.rs", "1..3", &[(1, " a\n+a9\n"), (10, hd), (22, hb)]);
    let two = pr_digest::diff(&mut h, &w, &r2, &meta("gh pr diff 1")).unwrap();
    let row = two.digest.lines().find(|l| l.starts_with("M x.rs ")).unwrap_or_else(|| panic!("{}", two.digest));
    let whole = &two.parts[0];
    assert!(row.ends_with(&format!("2 of 3 hunks new @ 5-7 8-11  changed, 1 hunk seen before, 2 gone, whole file {whole}")), "{row}");
    let fresh = row.split(' ').find(|w| w.starts_with("h:")).unwrap();
    assert_ne!(fresh, whole);
    let want = format!("diff --git a/x.rs b/x.rs\nindex 1..3 100644\n--- a/x.rs\n+++ b/x.rs\n@@ -1,1 +1,2 @@ fn ctx()\n a\n+a9\n@@ -10,1 +10,3 @@ fn ctx()\n{hd}");
    assert_eq!(h.text(fresh).unwrap().1, want, "the new hunks' bytes as stored");
    assert_eq!(h.text(whole).unwrap().1, r2, "the whole file is still a handle");
    assert_eq!(reassemble(&h, &two.parts), r2);
    // Round 3 only drops a hunk: nothing new, the row names the whole file.
    let r3 = hunks("x.rs", "1..4", &[(1, " a\n+a9\n"), (10, hd)]);
    let three = pr_digest::diff(&mut h, &w, &r3, &meta("gh pr diff 1")).unwrap();
    let row = three.digest.lines().find(|l| l.starts_with("M x.rs ")).unwrap();
    assert!(row.contains(&format!(" {} 0 of 2 hunks new  changed, 2 hunks seen before, 1 gone, whole file {}", three.parts[0], three.parts[0])), "{row}");
    // A hunk seen only two rounds back still counts as seen.
    let r4 = hunks("x.rs", "1..5", &[(1, ha), (10, hd)]);
    let four = pr_digest::diff(&mut h, &w, &r4, &meta("gh pr diff 1")).unwrap();
    let row = four.digest.lines().find(|l| l.starts_with("M x.rs ")).unwrap();
    assert!(row.contains(" 0 of 2 hunks new  changed, 2 hunks seen before, 1 gone, "), "{row}");
}

#[test]
fn the_public_fixture_digests_to_one_row_per_file() {
    let t = Temp::new("prd-public");
    let mut h = store(&t);
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/gh/ripgrep-3560.diff")).unwrap();
    let d = pr_digest::diff(&mut h, &wh("burntsushi/ripgrep#3560"), &text, &meta("gh pr diff 3560")).unwrap();
    let lines: Vec<&str> = d.digest.lines().collect();
    assert!(lines[0].ends_with(": burntsushi/ripgrep#3560 round 1, 2 files +46 -1, 3014 B"), "{}", d.digest);
    assert!(lines[1].starts_with("M crates/ignore/src/dir.rs +25 -0 h:") && lines[1].ends_with("1 hunk @ 5-36"), "{}", d.digest);
    assert!(lines[2].starts_with("M crates/ignore/src/incremental.rs +21 -1 h:") && lines[2].ends_with("2 hunks @ 5-13 14-38"), "{}", d.digest);
    assert!(lines[3].starts_with("a file: atlas slice h:F; a hunk: atlas slice h:F --lines A-B; in a file read ~ is an edited line as [-old-]{+new+}; whole output h:"), "{}", d.digest);
    assert_eq!(lines.len(), 4);
}

const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA2: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn patch() -> String {
    format!(
        "From {SHA1} Mon Sep 17 00:00:00 2001\nFrom: A <a@example.com>\nSubject: [PATCH 1/2] first change\n\n---\n a.rs | 1 +\n\n\
         diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n a\n+b\n-- \n2.43.0\n\n\
         From {SHA2} Mon Sep 17 00:00:00 2001\nFrom: A <a@example.com>\nSubject: [PATCH 2/2] second change\n\n---\n\n\
         diff --git a/a.rs b/a.rs\nindex 2..3 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,3 @@\n a\n b\n+c\n\
         diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n-- \n2.43.0\n\n"
    )
}

#[test]
fn a_patch_keeps_its_commits_and_records_its_head() {
    let t = Temp::new("prd-patch");
    let mut h = store(&t);
    let text = patch();
    let parts = pr_digest::split_diff(&text);
    let kinds: Vec<bool> = parts.iter().map(|p| p.file.is_some()).collect();
    assert_eq!(kinds, vec![false, true, false, false, true, true, false], "commit, file, signature, commit, file, a mode change, signature");
    let d = pr_digest::diff(&mut h, &wh("octo/demo#3 patch"), &text, &meta("gh pr diff 3 --patch")).unwrap();
    assert_eq!(reassemble(&h, &d.parts), text);
    assert!(d.digest.contains("commit 0123456 [PATCH 1/2] first change h:"), "{}", d.digest);
    assert!(d.digest.contains("commit 89abcde [PATCH 2/2] second change h:"), "{}", d.digest);
    assert!(d.digest.contains("M a.rs +1 -0 h:") && d.digest.contains("M a.rs (2) +1 -0 h:"), "{}", d.digest);
    assert!(d.digest.contains("M run.sh +0 -0 h:") && d.digest.contains(" mode only\n"), "{}", d.digest);
    let head: String = h.conn().query_row("SELECT head FROM pr_rounds WHERE pr = 'octo/demo#3 patch'", [], |r| r.get(0)).unwrap();
    assert_eq!(head, SHA2, "the last commit is the head");
}

// ---- rounds ---------------------------------------------------------------

fn file(path: &str, at: u32, index: &str, body: &str) -> String {
    let n = body.lines().count();
    format!("diff --git a/{path} b/{path}\nindex {index} 100644\n--- a/{path}\n+++ b/{path}\n@@ -{at},1 +{at},{n} @@ fn ctx()\n{body}")
}

#[test]
fn a_later_round_shows_only_what_changed() {
    let t = Temp::new("prd-rounds");
    let mut h = store(&t);
    let w = wh("octo/demo#7");
    let a = file("a.rs", 1, "1..2", " a\n+a2\n");
    let b1 = file("b.rs", 10, "3..4", " b\n+b2\n");
    let b2 = file("b.rs", 30, "5..6", " b\n+b2\n"); // a rebase moved it
    let c1 = file("c.rs", 1, "7..8", " c\n+c2\n");
    let c2 = file("c.rs", 1, "7..9", " c\n+c3\n");
    let d = file("d.rs", 1, "1..2", " d\n+d2\n");
    let e = file("e.rs", 1, "1..2", " e\n+e2\n");
    let r1 = format!("{a}{b1}{c1}{d}");
    let r2 = format!("{a}{b2}{c2}{e}");
    let one = pr_digest::diff(&mut h, &w, &r1, &meta("gh pr diff 7")).unwrap();
    assert_eq!(one.round, 1);
    assert!(!one.digest.contains("since round"), "{}", one.digest);
    let two = pr_digest::diff(&mut h, &w, &r2, &meta("gh pr diff 7")).unwrap();
    assert_eq!(two.round, 2);
    let g = &two.digest;
    assert!(g.contains("since round 1: 1 changed, 1 new, 1 removed, 2 unchanged (1 moved)\n"), "{g}");
    assert!(g.contains(&format!("unchanged: a.rs {} (since round 1), b.rs {} (moved, since round 1)\n", one.parts[0], two.parts[1])), "{g}");
    assert_eq!(one.parts[0], two.parts[0], "an unchanged patch keeps its handle");
    assert!(g.contains(&format!("M c.rs +1 -0 {} 1 hunk @ 5-7  changed\n", two.parts[2])), "{g}");
    assert!(g.contains(&format!("M e.rs +1 -0 {} 1 hunk @ 5-7  new\n", two.parts[3])), "{g}");
    assert!(g.contains(&format!("removed since round 1: d.rs (was {})\n", one.parts[3])), "{g}");
    assert!(!g.contains("M a.rs") && !g.contains("M b.rs"), "unchanged files get no row: {g}");
    // The same output again (another reviewer) is the same round, the same answer.
    let again = pr_digest::diff(&mut h, &w, &r2, &meta("gh pr diff 7")).unwrap();
    assert_eq!(again, two);
    // A third round: a.rs has not changed since round 1, c.rs since round 2.
    let r3 = format!("{a}{b2}{c2}{}", file("e.rs", 1, "1..3", " e\n+e3\n"));
    let three = pr_digest::diff(&mut h, &w, &r3, &meta("gh pr diff 7")).unwrap();
    assert_eq!(three.round, 3);
    assert!(three.digest.contains("since round 2: 1 changed, 0 new, 0 removed, 3 unchanged\n"), "{}", three.digest);
    assert!(three.digest.contains("a.rs ") && three.digest.contains("(since round 1)") && three.digest.contains(&format!("c.rs {} (since round 2)", two.parts[2])), "{}", three.digest);
    assert!(three.digest.contains(&format!("b.rs {} (since round 2)", two.parts[1])), "moved in round 2, the same since: {}", three.digest);
    // Another PR's rounds are its own.
    let other = pr_digest::diff(&mut h, &wh("octo/demo#8"), &r2, &meta("gh pr diff 8")).unwrap();
    assert_eq!(other.round, 1);
}

#[test]
fn a_round_whose_handles_were_evicted_stores_them_again() {
    let t = Temp::new("prd-evict");
    let mut h = store(&t);
    let w = wh("octo/demo#1");
    let text = format!("{}{}", file("a.rs", 1, "1..2", " a\n+a2\n"), file("b.rs", 1, "1..2", " b\n+b2\n"));
    let one = pr_digest::diff(&mut h, &w, &text, &meta("gh pr diff 1")).unwrap();
    h.conn().execute("DELETE FROM handles", []).unwrap();
    let two = pr_digest::diff(&mut h, &w, &text, &meta("gh pr diff 1")).unwrap();
    assert_eq!(two.round, one.round);
    assert_eq!(reassemble(&h, &two.parts), text, "every part is back");
}

// ---- collapsing -----------------------------------------------------------

#[test]
fn lock_generated_and_vendored_files_collapse() {
    let attrs = pr_digest::attributes("# c\n/gen/** linguist-generated\napi.ts linguist-generated=true\nkeep.pb.go -linguist-generated\nlib/** linguist-vendored\nvendor/** linguist-vendored=false\n");
    let c = |p: &str, part: &str| pr_digest::collapse(p, part, &attrs, "");
    assert_eq!(c("Cargo.lock", ""), Some((Collapse::Lock, "Cargo.lock".into())));
    assert_eq!(c("web/package-lock.json", ""), Some((Collapse::Lock, "web/package-lock.json".into())));
    assert_eq!(c("gen/x/y.rs", ""), Some((Collapse::Generated, "gen/x/y.rs".into())));
    assert_eq!(c("src/api.ts", ""), Some((Collapse::Generated, "src/api.ts".into())));
    assert_eq!(c("src/x.pb.go", ""), Some((Collapse::Generated, "src/x.pb.go".into())));
    assert_eq!(c("keep.pb.go", ""), None, "an attribute unsets the name rule");
    assert_eq!(c("src/a.rs", "@@ -0,0 +1,2 @@\n+// @generated by tool\n+fn x() {}\n"), Some((Collapse::Generated, "src/a.rs".into())));
    assert_eq!(c("src/a.rs", "@@ -0,0 +1,1 @@\n+fn x() {}\n"), None);
    // Only the file's first lines as the PR leaves it: new-side line 12 counts, 13 does not.
    let at = |new: u32, ctx: usize| format!("--- a/m.rs\n+++ b/m.rs\n@@ -{new},{ctx} +{new},{} @@ fn f()\n{}-gone\n+// DO NOT EDIT below\n", ctx + 1, " x\n".repeat(ctx));
    assert_eq!(c("src/m.rs", &at(10, 2)), Some((Collapse::Generated, "src/m.rs".into())), "an added marker at line 12");
    assert_eq!(c("src/m.rs", &at(11, 2)), None, "an added marker at line 13 is mid-file");
    assert_eq!(c("src/m.rs", &at(200, 0)), None, "an added marker far down");
    assert_eq!(c("src/m.rs", "--- a/m.rs\n+++ b/m.rs\n@@ -1,1 +1,1 @@\n-// DO NOT EDIT\n+x\n"), None, "a removed marker");
    assert_eq!(pr_digest::generated_rule("src/m.rs", &at(1, 0), &attrs, ""), Some("added marker"));
    assert_eq!(pr_digest::generated_rule("x.pb.go", "", &attrs, ""), Some("name"));
    assert_eq!(pr_digest::generated_rule("gen/a.rs", "", &attrs, ""), Some("attribute"));
    assert_eq!(pr_digest::generated_rule("a.rs", "", &attrs, "// @generated\n"), Some("head marker"));
    assert_eq!(pr_digest::generated_rule("keep.pb.go", "", &attrs, "// @generated\n"), None, "an attribute unsets every rule");
    // The word in prose is not a marker (ripgrep's hand-written zsh completions, line 7).
    let zsh = "/*!\nProvides completions for ripgrep's CLI for the zsh shell.\n\n2. Are much higher in quality than the ones below that are auto-generated.\n";
    assert_eq!(pr_digest::generated_rule("crates/core/flags/complete/zsh.rs", "", &[], zsh), None);
    for head in ["<!-- WARNING: This file is auto-generated (cargo dev generate-all). -->\n", "# This file is autogenerated by pip-compile\n", "// <auto-generated>\n", "# THIS FILE WAS AUTOMATICALLY GENERATED\n"] {
        assert_eq!(pr_digest::generated_rule("a.txt", "", &[], head), Some("head marker"), "{head}");
    }
    assert_eq!(c("third_party/z/a.c", ""), Some((Collapse::Vendored, "third_party/".into())));
    assert_eq!(c("lib/q/a.c", ""), Some((Collapse::Vendored, "lib/q/".into())));
    assert_eq!(c("vendor/x/a.go", ""), None, "linguist-vendored=false");
    assert_eq!(c("src/vendor.rs", ""), None);
}

#[test]
fn a_generated_marker_in_the_working_tree_head_collapses() {
    let t = Temp::new("prd-head");
    let top = t.path().join("repo");
    std::fs::create_dir_all(top.join("docs")).unwrap();
    let mut rules = String::from("<!-- WARNING: This file is auto-generated (cargo dev generate-all). -->\n");
    rules.push_str(&"line\n".repeat(30));
    std::fs::write(top.join("docs/rules.md"), &rules).unwrap();
    std::fs::write(t.path().join("outside.md"), "auto-generated\n").unwrap();
    let late = format!("{}// @generated\n", "x\n".repeat(12));
    std::fs::write(top.join("late.rs"), &late).unwrap();
    let tops = top.to_string_lossy().to_string();
    let head = pr_digest::file_head(&tops, "docs/rules.md");
    assert_eq!(head.lines().count(), 12, "only the first lines are read");
    assert!(rules.starts_with(&head));
    assert_eq!(pr_digest::file_head(&tops, "../outside.md"), "", "a path that leaves the tree is not read");
    assert_eq!(pr_digest::file_head(&tops, "missing.md"), "");
    assert_eq!(pr_digest::file_head("", "docs/rules.md"), "", "no toplevel, nothing read");
    // Its hunks start past the marker, so the added lines alone miss it.
    let part = file("docs/rules.md", 20, "1..2", " line\n+new\n");
    assert_eq!(pr_digest::collapse("docs/rules.md", &part, &[], ""), None);
    assert_eq!(pr_digest::collapse("docs/rules.md", &part, &[], &head), Some((Collapse::Generated, "docs/rules.md".into())));
    assert_eq!(pr_digest::collapse("late.rs", "", &[], &pr_digest::file_head(&tops, "late.rs")), None, "a marker past the first lines");
    let off = pr_digest::attributes("docs/** -linguist-generated\n");
    assert_eq!(pr_digest::collapse("docs/rules.md", &part, &off, &head), None, "an attribute unsets it");
    // Through the digest: a generated row only with the toplevel.
    let text = [part.clone(), file("src/a.rs", 1, "1..2", " a\n+b\n")].concat();
    let mut h = store(&t);
    let mut w = wh("octo/head#1");
    let d = pr_digest::diff(&mut h, &w, &text, &meta("gh pr diff 1")).unwrap();
    assert!(d.digest.contains("\nM docs/rules.md +1 -0 h:"), "{}", d.digest);
    w.key = "octo/head#2".into();
    w.toplevel = tops;
    let d = pr_digest::diff(&mut h, &w, &text, &meta("gh pr diff 2")).unwrap();
    assert!(d.digest.contains(&format!("generated M docs/rules.md +1 -0 {}\n", d.parts[0])), "{}", d.digest);
}

#[test]
fn collapsed_files_are_one_row_each_and_a_tree_is_one_row() {
    let t = Temp::new("prd-collapse");
    let mut h = store(&t);
    let text = [
        file("src/main.rs", 1, "1..2", " a\n+b\n"),
        file("Cargo.lock", 1, "1..2", " a\n+b\n+c\n"),
        file("third_party/x/a.c", 1, "1..2", " a\n+b\n"),
        file("third_party/y/b.c", 1, "1..2", " a\n+b\n+c\n"),
        file("api.gen.ts", 1, "1..2", " a\n+// Code generated by protoc. DO NOT EDIT.\n"),
    ]
    .concat();
    let d = pr_digest::diff(&mut h, &wh("octo/demo#2"), &text, &meta("gh pr diff 2")).unwrap();
    let g = &d.digest;
    assert!(g.contains("M src/main.rs +1 -0 h:"), "{g}");
    assert!(g.contains(&format!("lock M Cargo.lock +2 -0 {}\n", d.parts[1])), "{g}");
    assert!(g.contains(&format!("generated M api.gen.ts +1 -0 {}\n", d.parts[4])), "{g}");
    let tree = g.lines().find(|l| l.starts_with("vendored third_party/ 2 files +3 -0 h:")).unwrap_or_else(|| panic!("{g}"));
    let tree_id = tree.rsplit(' ').next().unwrap();
    let both = format!("{}{}", file("third_party/x/a.c", 1, "1..2", " a\n+b\n"), file("third_party/y/b.c", 1, "1..2", " a\n+b\n+c\n"));
    assert_eq!(h.text(tree_id).unwrap().1, both, "the tree's handle holds its files");
    assert!(!g.contains("M third_party/"), "{g}");
    assert_eq!(g.matches(" @ ").count(), 1, "only the source file lists hunks: {g}");
    assert_eq!(reassemble(&h, &d.parts), text);
}

// ---- gh pr view -----------------------------------------------------------

const VIEW: &str = "title:\tignore: add a thing\nstate:\tOPEN\nauthor:\tsomeone\nlabels:\t\nnumber:\t12\n--\nThis adds a thing.\n\nMore words.\n";
const COMMENTS: &str = "author:\talice\nassociation:\tmember\nedited:\tfalse\nstatus:\tapproved\n--\nLooks good.\n\nOne nit below.\n--\nauthor:\tbob\nassociation:\towner\nedited:\tfalse\nstatus:\tnone\n--\nShipped.\n--\n";

#[test]
fn view_text_splits_into_fields_body_and_comments() {
    let parts = pr_digest::split_view(VIEW, false);
    assert_eq!(parts.iter().map(|p| p.kind).collect::<Vec<_>>(), vec!["header", "body"]);
    assert_eq!(&VIEW[parts[1].start..parts[1].end], "This adds a thing.\n\nMore words.\n");
    let parts = pr_digest::split_view(COMMENTS, true);
    assert_eq!(parts.len(), 2);
    assert!(COMMENTS[parts[1].start..].starts_with("author:\tbob\n"));
    let t = Temp::new("prd-view");
    let mut h = store(&t);
    let d = pr_digest::view_text(&mut h, &wh("octo/demo#12"), VIEW, &meta("gh pr view 12"), false).unwrap();
    assert_eq!(reassemble(&h, &d.parts), VIEW);
    assert!(d.digest.contains("\ntitle: ignore: add a thing\nstate: OPEN\nauthor: someone\nnumber: 12\nbody 3 lines, 32 B h:"), "{}", d.digest);
    assert!(d.digest.contains(": This adds a thing.\n"), "{}", d.digest);
    let d = pr_digest::view_text(&mut h, &wh("octo/demo#12"), COMMENTS, &meta("gh pr view 12 --comments"), true).unwrap();
    assert_eq!(reassemble(&h, &d.parts), COMMENTS);
    assert!(d.digest.contains("comment 1/2 alice (approved) 3 lines, 28 B h:"), "{}", d.digest);
    assert!(d.digest.contains(": Looks good.\ncomment 2/2 bob (none) 1 lines, 9 B h:"), "{}", d.digest);
    let first = d.digest.lines().find(|l| l.starts_with("comment 1/2")).unwrap();
    let id = first.split(' ').find(|w| w.starts_with("h:")).unwrap().trim_end_matches(':');
    assert!(h.text(id).unwrap().1.contains("One nit below."));
}

#[test]
fn view_json_gives_each_text_a_handle_and_records_the_head() {
    let t = Temp::new("prd-json");
    let mut h = store(&t);
    let text = json!({
        "title": "A change",
        "headRefOid": SHA1,
        "body": "Line one\nline two\n",
        "comments": [{"author": {"login": "alice"}, "body": "Nice\nwork", "createdAt": "2026-01-01T00:00:00Z"}],
        "reviews": [
            {"author": {"login": "bob"}, "body": "", "state": "APPROVED", "submittedAt": "2026-01-02T00:00:00Z"},
            {"author": {"login": "carol"}, "body": "Please fix x", "state": "CHANGES_REQUESTED", "submittedAt": "2026-01-03T00:00:00Z"}
        ],
        "labels": [{"name": "a"}]
    })
    .to_string();
    let d = pr_digest::view_json(&mut h, &wh("octo/demo#5"), &text, &meta("gh pr view 5 --json title,body,comments,reviews")).unwrap().unwrap();
    let g = &d.digest;
    assert!(g.contains("\ntitle: A change\n"), "{g}");
    assert!(g.contains("body 2 lines, 18 B h:"), "{g}");
    assert!(g.contains("comment 1/1 alice 2026-01-01T00:00:00Z 2 lines, 9 B h:"), "{g}");
    assert!(g.contains("review 1/2 bob (approved) 2026-01-02T00:00:00Z no text\n"), "{g}");
    assert!(g.contains("review 2/2 carol (changes_requested) 2026-01-03T00:00:00Z 1 lines, 12 B h:"), "{g}");
    assert!(g.contains("labels: 1 items\n"), "{g}");
    let texts: Vec<String> = d.parts.iter().map(|id| h.text(id).unwrap().1).collect();
    assert_eq!(texts, vec!["Line one\nline two\n", "Nice\nwork", "Please fix x"]);
    assert_eq!(h.text(&d.whole).unwrap().1, text);
    let head: String = h.conn().query_row("SELECT head FROM pr_heads WHERE pr = 'octo/demo#5'", [], |r| r.get(0)).unwrap();
    assert_eq!(head, SHA1);
    assert_eq!(pr_digest::view_json(&mut h, &wh("octo/demo#5"), "not json", &meta("gh pr view")).unwrap(), None);
}

// ---- the route ------------------------------------------------------------

fn bash(cmd: &str, stdout: &str) -> HookEvent {
    HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Bash".into(),
        session_id: "s1".into(),
        tool_input: json!({"command": cmd}),
        tool_response: json!({"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}),
        ..HookEvent::default()
    }
}

fn big_diff(n: usize, tag: &str) -> String {
    (0..n).map(|i| file(&format!("src/f{i}.rs"), 1, "1..2", &format!(" ctx\n+{tag} line for file {i} with some padding to make it longer\n"))).collect()
}

#[test]
fn the_route_is_on_by_default_and_off_when_set_false() {
    let t = Temp::new("prd-route");
    let db = t.path().join("index.sqlite");
    let text = big_diff(120, "one");
    assert!(text.len() > 12_000);
    let cmd = "gh pr diff 7 -R octo/demo";
    let mut off = Router::with_db(&db, Some(json!({"pr_digest": false})));
    let generic = off.post_tool_use(&bash(cmd, &text)).unwrap().unwrap();
    assert!(!generic["stdout"].as_str().unwrap().contains("round 1"), "false: the generic digest");
    let rounds = || Handles::open(&db, HandleCfg::default()).unwrap().conn().query_row("SELECT COUNT(*) FROM pr_rounds", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(rounds(), 0, "false records no round");
    // No key, an empty `lean`, and true are all on.
    for lean in [Some(json!({"pr_digest": true})), Some(json!({}))] {
        let t2 = Temp::new("prd-route-on");
        let mut r = Router::with_db(&t2.path().join("index.sqlite"), lean.clone());
        let s = r.post_tool_use(&bash(cmd, &text)).unwrap().unwrap();
        assert!(s["stdout"].as_str().unwrap().contains(": octo/demo#7 round 1, 120 files"), "{lean:?}");
    }
    let mut on = Router::with_db(&db, None);
    let out = on.post_tool_use(&bash(cmd, &text)).unwrap().unwrap();
    let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["stdout", "stderr", "interrupted", "isImage", "noOutputExpected"]);
    let s = out["stdout"].as_str().unwrap();
    assert!(s.starts_with("h:") && s.contains(": octo/demo#7 round 1, 120 files +120 -0,"), "{s}");
    assert!(s.contains("M src/f0.rs +1 -0 h:"), "{s}");
    assert_eq!(s.split_once(' ').unwrap().0, generic["stdout"].as_str().unwrap().split_once(' ').unwrap().0, "the whole output is the same handle");
    // Logged apart from route:Bash.
    let n: i64 = Handles::open(&db, HandleCfg::default()).unwrap().conn().query_row("SELECT COUNT(*) FROM token_log WHERE tool = 'route:gh-pr'", [], |r| r.get(0)).unwrap_or(-1);
    assert_eq!(n, 1);
    // Another command is untouched by the switch: the same answer as with it off.
    let other = "x\n".repeat(7000);
    assert_eq!(on.post_tool_use(&bash("cat big.txt", &other)).unwrap(), off.post_tool_use(&bash("cat big.txt", &other)).unwrap());
    // An empty or failed output is never a round.
    assert_eq!(on.post_tool_use(&bash(cmd, "")).unwrap(), None);
    // A small diff passes whole, and is still a round.
    let small = big_diff(2, "two");
    assert_eq!(on.post_tool_use(&bash(cmd, &small)).unwrap(), None);
    let rounds: i64 = Handles::open(&db, HandleCfg::default()).unwrap().conn().query_row("SELECT MAX(round) FROM pr_rounds WHERE pr = 'octo/demo#7'", [], |r| r.get(0)).unwrap();
    assert_eq!(rounds, 2);
    // A failure, a long stderr, an unknown PR or routing off: not this route.
    let mut ev = bash(cmd, &text);
    ev.tool_response["stderr"] = json!("e".repeat(3000));
    let long_err = on.post_tool_use(&ev).unwrap().unwrap();
    assert!(!long_err["stdout"].as_str().unwrap().contains("round"), "generic");
    let unknown = on.post_tool_use(&bash("gh pr diff", &text)).unwrap().unwrap();
    assert!(!unknown["stdout"].as_str().unwrap().contains("round"), "no repository at the cwd: generic");
    set_routing(&db, false).unwrap();
    assert_eq!(on.post_tool_use(&bash(cmd, &text)).unwrap(), None);
}

#[test]
fn a_pr_digest_is_far_smaller_than_the_diff() {
    let t = Temp::new("prd-size");
    let mut h = store(&t);
    let text = big_diff(60, "x");
    let d = pr_digest::diff(&mut h, &wh("octo/demo#1"), &text, &meta("gh pr diff 1")).unwrap();
    assert!(d.digest.len() * 2 < text.len(), "{} vs {}", d.digest.len(), text.len());
    assert_eq!(d.digest.lines().filter(|l| l.starts_with("M src/f")).count(), 60);
}

// ---- measurement ----------------------------------------------------------

/// What both arms know of a PR's repository: its `.gitattributes` and its
/// working tree for file heads. The harness feeds each fixture's base
/// (`ATLAS_PRDIGEST_HEADS/STEM/`: `.gitattributes` and the first lines of
/// every modified or deleted file); production reads the checkout.
struct Repo {
    attributes: String,
    toplevel: String,
}

/// Today's skip set, frozen: #133's lock, generated and vendored rules plus
/// the generated marker in a file's head, with the same attributes and heads
/// as the on arm. Never a deleted file, whatever `collapse` grows into.
fn today_skips(path: &str, part: &str, repo: &Repo) -> bool {
    let attrs = pr_digest::attributes(&repo.attributes);
    match pr_digest::collapse(path, part, &attrs, &pr_digest::file_head(&repo.toplevel, path)) {
        None => false,
        Some((c, _)) => match c {
            Collapse::Lock | Collapse::Generated | Collapse::Vendored => true,
            Collapse::Deleted => false,
        },
    }
}

/// The 1-based line ranges of a diff's files in the whole output, and
/// whether today's frozen skip set skips each.
fn file_ranges(text: &str, repo: &Repo) -> Vec<(usize, usize, bool, String)> {
    pr_digest::split_diff(text)
        .into_iter()
        .filter_map(|p| {
            let f = p.file?;
            let first = text[..p.start].matches('\n').count() + 1;
            let last = first + text[p.start..p.end].lines().count() - 1;
            let c = today_skips(&f.path, &text[p.start..p.end], repo);
            Some((first, last, c, f.path))
        })
        .collect()
}

/// What a review pulls on two routes, as text. Today: the generic digest, a
/// grep for the file boundaries and line slices of the whole output. On: the
/// PR digest and a file read (`atlas slice h:F` and its next pages) of every
/// file row's handle. Each slice's command line counts too. `all` reads the
/// collapsed files as well (every hunk), else they are skipped (realistic).
fn read_today(h: &Handles, digest: &str, text: &str, repo: &Repo, all: bool) -> String {
    let whole = digest.split_once(' ').unwrap().0.to_string();
    let mut out = String::from(digest);
    out.push_str(&format!("atlas slice {whole} --grep \"^diff --git\"\n"));
    out.push_str(&h.slice(&whole, &Sel::Grep { re: "^diff --git".into(), context: 0 }).unwrap());
    for r in file_ranges(text, repo).iter().filter(|r| all || !r.2) {
        let (mut a, b) = (r.0, r.1);
        loop {
            let s = h.slice(&whole, &Sel::Lines(a, b)).unwrap();
            out.push_str(&format!("atlas slice {whole} --lines {a}-{b}\n"));
            out.push_str(&s);
            let last = s.lines().last().unwrap_or("");
            let Some(next) = last.strip_prefix("truncated, ").and_then(|l| l.split_once("--lines ")) else { break };
            a = next.1.split('-').next().unwrap().parse().unwrap();
        }
    }
    out
}

/// The collapsed row kinds: a realistic review does not open them.
const COLLAPSED_ROWS: &[&str] = &["lock ", "generated ", "vendored ", "deleted "];

fn read_on(h: &Handles, digest: &str, all: bool) -> String {
    let mut out = String::from(digest);
    for l in digest.lines() {
        let collapsed = COLLAPSED_ROWS.iter().any(|c| l.starts_with(c));
        let file_row = collapsed || l.chars().nth(1) == Some(' ') && l.chars().next().is_some_and(|c| "MADRC".contains(c));
        let Some(id) = l.split(' ').find(|w| w.starts_with("h:") && w.len() >= 12) else { continue };
        if !file_row || (collapsed && !all) {
            continue;
        }
        let mut s = h.slice(id, &Sel::Whole).unwrap();
        out.push_str(&format!("atlas slice {id}\n"));
        loop {
            out.push_str(&s);
            // A diff-native page ends `next page: atlas slice ID ...`; a
            // numbered one (other kinds) `truncated, N more lines (... --lines K-B)`.
            let last = s.lines().last().unwrap_or("");
            if let Some(next) = last.strip_prefix(&format!("next page: atlas slice {id} ")) {
                let sel = atlas::handles::parse_sel(&next.split(' ').map(String::from).collect::<Vec<_>>()).unwrap();
                out.push_str(&format!("atlas slice {id} {next}\n"));
                s = h.slice(id, &sel).unwrap();
                continue;
            }
            let Some(next) = last.strip_prefix("truncated, ").and_then(|l| l.split_once("--lines ")).map(|x| x.1.trim_end_matches(')')) else { break };
            let (a, b) = next.split_once('-').unwrap();
            out.push_str(&format!("atlas slice {id} --lines {a}-{b}\n"));
            s = h.slice(id, &Sel::Lines(a.parse().unwrap(), b.parse().unwrap())).unwrap();
        }
    }
    out
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    match v.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => v[n / 2],
        n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
    }
}

/// The 1-based line of a file part that a review comment sits on: `right`
/// for the new side (`+` or context), else the old side.
fn part_line(part: &str, f: &pr_digest::FileDiff, right: bool, line: u64) -> Option<usize> {
    let lines: Vec<&str> = part.split_inclusive('\n').collect();
    for &(s, e) in &f.hunks {
        let hdr = lines[s - 1];
        let rest = hdr.strip_prefix("@@ -")?;
        let (old, rest) = rest.split_once(" +")?;
        let (new, _) = rest.split_once(" @@")?;
        let start = |r: &str| r.split(',').next().and_then(|x| x.parse::<u64>().ok());
        let (mut o, mut n) = (start(old)?, start(new)?);
        for (k, l) in lines.iter().enumerate().take(e).skip(s) {
            match l.as_bytes().first() {
                Some(b'-') => {
                    if !right && o == line {
                        return Some(k + 1);
                    }
                    o += 1;
                }
                Some(b'+') => {
                    if right && n == line {
                        return Some(k + 1);
                    }
                    n += 1;
                }
                Some(b'\\') => {}
                _ => {
                    if (right && n == line) || (!right && o == line) {
                        return Some(k + 1);
                    }
                    o += 1;
                    n += 1;
                }
            }
        }
    }
    None
}

/// The proxy's tally over a set: commented lines (unique per round, path,
/// side and line), the PRs they are in, and where each sits on the on arm.
#[derive(Default)]
struct Proxy {
    lines: usize,
    prs: std::collections::BTreeSet<String>,
    shown: usize,
    behind_marker: usize,
    collapsed: usize,
    unmapped: usize,
}

/// The review comments of a PR round (by `original_commit_id`), as
/// (path, new side, original line).
fn round_comments(dir: &str, stem: &str, oid: &str) -> (Vec<(String, bool, u64)>, usize) {
    let Ok(text) = std::fs::read_to_string(Path::new(dir).join(format!("{stem}.comments.json"))) else { return (Vec::new(), 0) };
    let Ok(v) = json::parse(&text) else { return (Vec::new(), 0) };
    let mut out: Vec<(String, bool, u64)> = Vec::new();
    let mut skipped = 0;
    for c in v.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        if c.get("original_commit_id").and_then(|x| x.as_str()) != Some(oid) {
            continue;
        }
        let path = c.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let right = c.get("side").and_then(|x| x.as_str()) != Some("LEFT");
        let Some(line) = c.get("original_line").and_then(|x| x.as_u64()).or_else(|| c.get("line").and_then(|x| x.as_u64())) else {
            skipped += 1;
            continue;
        };
        if !out.contains(&(path.clone(), right, line)) {
            out.push((path, right, line));
        }
    }
    (out, skipped)
}

/// The rounds of a PR as the set's manifest names them (`manifest.json`),
/// else every `NAME.rK.diff` in K order then `NAME.diff`; with each round's
/// commit when the manifest gives it.
fn pr_rounds(dir: &str, stem: &str, manifest: Option<&json::Value>) -> Vec<(String, String)> {
    if let Some(picks) = manifest.and_then(|m| m.get("picks")).and_then(|p| p.as_array()) {
        if let Some(p) = picks.iter().find(|p| p.get("name").and_then(|n| n.as_str()) == Some(stem)) {
            return p
                .get("rounds")
                .and_then(|r| r.as_array())
                .unwrap()
                .iter()
                .map(|r| (r.get("file").and_then(|x| x.as_str()).unwrap().to_string(), r.get("oid").and_then(|x| x.as_str()).unwrap_or("").to_string()))
                .collect();
        }
    }
    let mut earlier: Vec<(usize, String)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|f| f.strip_prefix(&format!("{stem}.r"))?.strip_suffix(".diff")?.parse().ok().map(|k| (k, f)))
        .collect();
    earlier.sort();
    let mut rounds: Vec<(String, String)> = earlier.into_iter().map(|e| (e.1, String::new())).collect();
    rounds.push((format!("{stem}.diff"), String::new()));
    rounds
}

/// Bytes a review pulls per PR-round, today against on, over the recorded
/// public diffs in `ATLAS_PRDIGEST_MEASURE`: a PR's rounds are those its
/// `manifest.json` names, else `NAME.rK.diff` in K order then `NAME.diff`.
/// The ratio is today over on (realistic: collapsed files unread; every
/// hunk: all read), and raw over on.
///
/// The on arm (realistic) is charged one hunk read (`atlas slice h:F
/// --lines A-B` and the hunk) for every hunk where a review comment of that
/// round sits on a line the default read does not show: behind a C1 marker,
/// or in a collapsed or deleted file. The same comments are the proxy: how
/// many commented lines are shown. Every file read is decoded back to its
/// stored part, byte for byte.
///
/// * `ATLAS_PRDIGEST_HEADS`: per PR a folder `STEM/` with the base's
///   `.gitattributes` and file heads, fed to both arms (harness parity).
/// * `ATLAS_PRDIGEST_TODAY=bare`: the today arm gets neither (the design's
///   reading; reported, never judged).
/// * `ATLAS_PRDIGEST_DUMP`: each round's two reads are written there for a
///   token count (the on read with its charge).
#[test]
#[ignore]
fn measure_public_prs() {
    let Ok(dir) = std::env::var("ATLAS_PRDIGEST_MEASURE") else { return };
    let dump = std::env::var("ATLAS_PRDIGEST_DUMP").ok();
    let heads = std::env::var("ATLAS_PRDIGEST_HEADS").ok();
    let bare_today = std::env::var("ATLAS_PRDIGEST_TODAY").as_deref() == Ok("bare");
    let manifest = std::fs::read_to_string(Path::new(&dir).join("manifest.json")).ok().map(|t| json::parse(&t).unwrap());
    let cap = HandleCfg::default().slice_bytes;
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().filter(|e| e.path().is_file())?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".diff") && !n.contains(".r"))
        .collect();
    names.sort();
    println!("| PR-round | raw | today realistic | on realistic | charge | x | x uncharged | raw x | today every hunk | on every hunk | x | files |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    let (mut all, mut first, mut every, mut plain) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut proxy = Proxy::default();
    let mut decoded = 0usize;
    let mut slowest: Option<(f64, String)> = None;
    let mut audited: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    println!("levers {:?}", pr_digest::levers());
    for n in names {
        let stem = n.trim_end_matches(".diff").to_string();
        let rounds = pr_rounds(&dir, &stem, manifest.as_ref());
        let on_repo = match &heads {
            Some(hd) => {
                let top = Path::new(hd).join(&stem);
                assert!(top.is_dir(), "no heads for {stem} under {hd}");
                Repo { attributes: std::fs::read_to_string(top.join(".gitattributes")).unwrap_or_default(), toplevel: top.to_string_lossy().into() }
            }
            None => Repo { attributes: String::new(), toplevel: String::new() },
        };
        let today_repo = if bare_today { Repo { attributes: String::new(), toplevel: String::new() } } else { Repo { attributes: on_repo.attributes.clone(), toplevel: on_repo.toplevel.clone() } };
        let t = Temp::new("prd-measure");
        let db = t.path().join("index.sqlite");
        let cmd = format!("gh pr diff 1 -R measure/{stem}");
        let pc = pr_digest::parse_cmd(&cmd).unwrap();
        let key = format!("measure/{stem}#1");
        let w = Where { key: key.clone(), attributes: on_repo.attributes.clone(), toplevel: on_repo.toplevel.clone() };
        let m = Meta { kind: "bash".into(), label: cmd.clone(), cmd: cmd.clone(), ..Meta::default() };
        let mut today = Router::with_db(&db, None);
        for (k, (file, oid)) in rounds.iter().enumerate() {
            let text = std::fs::read_to_string(Path::new(&dir).join(file)).unwrap();
            let g = today.post_tool_use(&bash(&cmd, &text)).unwrap().map(|v| v["stdout"].as_str().unwrap().to_string());
            // The on arm as the route runs it, with the repository's attributes
            // and working tree given rather than found at a cwd.
            let mut h = Handles::open(&db, HandleCfg::default()).unwrap();
            let small = text.len() < HandleCfg::default().threshold_bytes;
            let started = std::time::Instant::now();
            let d = pr_digest::route(&mut h, &pc, &w, &text, &m, small).unwrap();
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            if slowest.as_ref().is_none_or(|s: &(f64, String)| ms > s.0) {
                slowest = Some((ms, format!("{stem} r{} ({} bytes)", k + 1, text.len())));
            }
            // The generated rules' audit: every file they collapse, and why.
            let attrs = pr_digest::attributes(&on_repo.attributes);
            for p in pr_digest::split_diff(&text) {
                let Some(f) = p.file else { continue };
                if let Some(rule) = pr_digest::generated_rule(&f.path, &text[p.start..p.end], &attrs, &pr_digest::file_head(&on_repo.toplevel, &f.path)) {
                    if matches!(pr_digest::collapse(&f.path, &text[p.start..p.end], &attrs, &pr_digest::file_head(&on_repo.toplevel, &f.path)), Some((Collapse::Generated, _))) && audited.insert((stem.clone(), f.path.clone())) {
                        println!("GENERATED {stem} r{} {} ({rule})", k + 1, f.path);
                    }
                }
            }
            let p = d.as_ref().map(|d| d.digest.clone());
            let (tr, mut pr, ta, pa) = match (&g, &p) {
                (Some(g), Some(p)) => (read_today(&h, g, &text, &today_repo, false), read_on(&h, p, false), read_today(&h, g, &text, &today_repo, true), read_on(&h, p, true)),
                // Under the threshold both pass whole.
                _ => (text.clone(), text.clone(), text.clone(), text.clone()),
            };
            let uncharged = pr.len();
            if let (Some(p), Some(d)) = (&p, &d) {
                // Every file row read decodes to its stored part.
                for l in p.lines() {
                    let collapsed = COLLAPSED_ROWS.iter().any(|c| l.starts_with(c));
                    let file_row = l.chars().nth(1) == Some(' ') && l.chars().next().is_some_and(|c| "MADRC".contains(c));
                    let Some(id) = l.split(' ').find(|w| w.starts_with("h:") && w.len() >= 12) else { continue };
                    if collapsed || !file_row || l.contains(" binary") {
                        continue;
                    }
                    let (rebuilt, _) = rebuild_file(&h, l, id);
                    assert_eq!(rebuilt, h.text(id).unwrap().1, "{stem} r{}: {l}: the decoded read is the stored part", k + 1);
                    decoded += 1;
                }
                // The proxy and the expansion charge.
                let (comments, _) = round_comments(&dir, &stem, oid);
                let entries = pr_digest::manifest(h.conn(), &key, d.round).unwrap();
                let mut charged: std::collections::BTreeSet<(String, usize)> = std::collections::BTreeSet::new();
                for (path, right, line) in comments {
                    proxy.lines += 1;
                    proxy.prs.insert(stem.clone());
                    let Some(e) = entries.iter().find(|e| e.path == path) else {
                        proxy.unmapped += 1;
                        continue;
                    };
                    let part = h.text(&e.handle).unwrap().1;
                    let Some(f) = pr_digest::split_diff(&part).into_iter().find_map(|x| x.file) else {
                        proxy.unmapped += 1;
                        continue;
                    };
                    let Some(at) = part_line(&part, &f, right, line) else {
                        proxy.unmapped += 1;
                        continue;
                    };
                    let row = p.lines().find(|l| l.contains(&e.handle)).unwrap_or("");
                    let collapsed = COLLAPSED_ROWS.iter().any(|c| row.starts_with(c));
                    let shown = !collapsed && pr_digest::shown_lines(&part, pr_digest::levers(), cap).unwrap()[at];
                    if shown {
                        proxy.shown += 1;
                        continue;
                    }
                    if collapsed {
                        proxy.collapsed += 1;
                    } else {
                        proxy.behind_marker += 1;
                    }
                    let (a, b) = *f.hunks.iter().find(|hk| hk.0 <= at && at <= hk.1).unwrap();
                    if charged.insert((e.handle.clone(), a)) {
                        pr.push_str(&format!("atlas slice {} --lines {a}-{b}\n", e.handle));
                        pr.push_str(&h.slice(&e.handle, &Sel::Lines(a, b)).unwrap());
                    }
                }
            }
            if let Some(dd) = &dump {
                std::fs::write(Path::new(dd).join(format!("{stem}.{}.today.txt", k + 1)), &tr).unwrap();
                std::fs::write(Path::new(dd).join(format!("{stem}.{}.on.txt", k + 1)), &pr).unwrap();
            }
            let x = tr.len() as f64 / pr.len() as f64;
            let xu = tr.len() as f64 / uncharged as f64;
            let xa = ta.len() as f64 / pa.len() as f64;
            all.push(x);
            plain.push(xu);
            every.push(xa);
            if k == 0 {
                first.push(x);
            }
            println!(
                "| {stem} r{} | {} | {} | {} | {} | {x:.2}x | {xu:.2}x | {:.2}x | {} | {} | {xa:.2}x | {} |",
                k + 1,
                text.len(),
                tr.len(),
                pr.len(),
                pr.len() - uncharged,
                text.len() as f64 / pr.len() as f64,
                ta.len(),
                pa.len(),
                file_ranges(&text, &today_repo).len()
            );
        }
    }
    let worst = all.iter().copied().fold(f64::INFINITY, f64::min);
    if let Some((ms, at)) = &slowest {
        println!("ROUTE slowest {ms:.1} ms: {at}; {} generated files", audited.len());
    }
    println!(
        "PROXY {} commented lines over {} PRs: {} shown, {} behind a marker, {} in a collapsed or deleted file, {} not placed; {} file reads decoded",
        proxy.lines,
        proxy.prs.len(),
        proxy.shown,
        proxy.behind_marker,
        proxy.collapsed,
        proxy.unmapped,
        decoded
    );
    println!(
        "BAR median {:.3}x over {} PR-rounds, round-1 median {:.3}x over {}, worst {worst:.3}x; uncharged median {:.3}x; every-hunk median {:.3}x",
        median(&mut all.clone()),
        all.len(),
        median(&mut first.clone()),
        first.len(),
        median(&mut plain.clone()),
        median(&mut every.clone())
    );
}

// ---- the default read is lossless -------------------------------------------

/// Every part of every round, rebuilt from what its digest names, is the
/// stored diff byte for byte: CRLF, no final newline, over the cap, every
/// word-diff escape, deleted, lock, vendored and generated files (by the
/// working tree's head, by attribute, by name with a rename), hunks seen in
/// an earlier round, unchanged files, rows past the limit, and `--patch`
/// commits with the text between them.
#[test]
fn the_default_read_reassembles_every_round_of_synthetic_prs() {
    assert_eq!(pr_digest::levers(), pr_digest::SHIPPED, "the shipped levers");
    let t = Temp::new("prd-rebuild");
    let top = t.path().join("repo");
    std::fs::create_dir_all(top.join("docs")).unwrap();
    std::fs::write(top.join("docs/gen.md"), "<!-- This file is auto-generated. -->\nline\n").unwrap();
    let mut h = store(&t);
    let renamed = "diff --git a/old.pb.go b/new.pb.go\nsimilarity index 90%\nrename from old.pb.go\nrename to new.pb.go\nindex 1..2 100644\n--- a/old.pb.go\n+++ b/new.pb.go\n@@ -1,1 +1,1 @@\n-a\n+b\n";
    let mid = file("src/mid.rs", 40, "1..2", " x\n+// DO NOT EDIT below this line\n");
    let collapsed = [
        file("src/main.rs", 1, "1..2", " a\n+b\n"),
        file("Cargo.lock", 1, "1..2", " a\n+b\n+c\n"),
        file("third_party/x/a.c", 1, "1..2", " a\n+b\n"),
        file("third_party/y/b.c", 1, "1..2", " a\n+b\n+c\n"),
        file("vendor/z.go", 1, "1..2", " z\n+z2\n"),
        file("docs/gen.md", 20, "1..2", " line\n+new\n"),
        file("gen/api.rs", 1, "1..2", " a\n+b\n"),
        renamed.to_string(),
        mid,
    ]
    .concat();
    let (ha, hb, hc) = (" a\n+a2\n", " b\n+b2\n", " c\n+c2\n");
    let keep = [
        file("keep.rs", 1, "1..2", " k\n+k2\n"),
        "diff --git a/was.rs b/now.rs\nsimilarity index 90%\nrename from was.rs\nrename to now.rs\nindex 1..2 100644\n--- a/was.rs\n+++ b/now.rs\n@@ -1,1 +1,1 @@\n-w\n+n\n".to_string(),
        "diff --git a/add.rs b/add.rs\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/add.rs\n@@ -0,0 +1 @@\n+x\n".to_string(),
    ]
    .concat();
    let r1 = [hunks("x.rs", "1..2", &[(1, ha), (20, hb), (40, hc)]), keep.clone()].concat();
    let r2 = [hunks("x.rs", "1..3", &[(1, " a\n+a9\n"), (10, " d\n+d2\n"), (22, hb)]), keep].concat();
    let prs: Vec<(&str, Vec<String>)> = vec![
        ("mixed", vec![MIXED.to_string()]),
        ("crlf", vec![format!("{MIXED}{}", over_cap()).replace('\n', "\r\n")]),
        ("no final newline", vec![MIXED.trim_end().to_string()]),
        ("over the cap", vec![over_cap()]),
        ("edits", vec![edits()]),
        ("edits crlf", vec![edits().replace('\n', "\r\n")]),
        ("edits no final newline", vec![edits().trim_end().to_string()]),
        ("collapsed", vec![collapsed]),
        ("rounds", vec![r1, r2]),
        ("many", vec![big_diff(pr_digest::MAX_ROWS + 10, "many")]),
        ("patch", vec![patch()]),
    ];
    let mut kinds = Kinds::new();
    for (n, (name, rounds)) in prs.iter().enumerate() {
        let key = format!("octo/rebuild#{n}");
        let mut w = wh(&key);
        if *name == "collapsed" {
            w.toplevel = top.to_string_lossy().into();
            w.attributes = "gen/** linguist-generated\n".into();
        }
        for (k, text) in rounds.iter().enumerate() {
            let d = pr_digest::diff(&mut h, &w, text, &meta("gh pr diff")).unwrap();
            let (rebuilt, ks) = rebuild_round(&h, &key, &d);
            assert!(rebuilt == *text, "{name} round {}: the digest's reads are not the stored diff\n{}", k + 1, d.digest);
            for (kind, c) in ks {
                *kinds.entry(kind).or_default() += c;
            }
            if *name == "collapsed" {
                let g = &d.digest;
                assert!(g.contains("\ngenerated M docs/gen.md +1 -0 h:"), "by the working tree's head: {g}");
                assert!(g.contains("\ngenerated M gen/api.rs +1 -0 h:"), "by attribute: {g}");
                assert!(g.contains("\ngenerated R new.pb.go (from old.pb.go) +1 -1 h:"), "by name, renamed: {g}");
                assert!(g.contains("\nM src/mid.rs +1 -0 h:"), "a marker added mid-file is not a generated file: {g}");
            }
        }
    }
    for want in ["shown", "seen hunks (whole file)", "lock", "generated", "deleted", "unchanged", "vendored", "commit", "text between files (whole output)", "past the row limit (whole output)"] {
        assert!(kinds.get(want).is_some_and(|&c| c > 0), "no {want} part: {kinds:?}");
    }
}

/// The same check over the recorded public PRs (M4): every round of every
/// fixture in `ATLAS_PRDIGEST_SETS` (`NAME=DIR[=HEADS]`, `;` between sets;
/// DIR as `measure_public_prs` reads it, HEADS its per-PR `.gitattributes`
/// and file heads), replayed in order. It never passes on nothing: it panics
/// when the variable is unset or names no fixtures, and prints what it
/// checked per set.
#[test]
#[ignore]
fn the_default_read_reassembles_every_fixture() {
    let sets = std::env::var("ATLAS_PRDIGEST_SETS").expect("ATLAS_PRDIGEST_SETS names the fixture sets (NAME=DIR[=HEADS];...)");
    assert_eq!(pr_digest::levers(), pr_digest::SHIPPED, "the shipped levers: ATLAS_PRDIGEST_LEVERS must be unset");
    let mut total = 0usize;
    for set in sets.split(';').filter(|s| !s.trim().is_empty()) {
        let mut it = set.splitn(3, '=');
        let (name, dir, heads) = (it.next().unwrap(), it.next().expect("NAME=DIR"), it.next());
        let manifest = std::fs::read_to_string(Path::new(dir).join("manifest.json")).ok().map(|t| json::parse(&t).unwrap());
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{dir}: {e}"))
            .filter_map(|e| e.ok().filter(|e| e.path().is_file())?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".diff") && !n.contains(".r"))
            .collect();
        names.sort();
        assert!(!names.is_empty(), "set {name}: no fixtures in {dir}");
        let (mut rounds_n, mut kinds) = (0usize, Kinds::new());
        for n in &names {
            let stem = n.trim_end_matches(".diff");
            let mut w = wh(&format!("fixture/{stem}#1"));
            if let Some(hd) = heads {
                let top = Path::new(hd).join(stem);
                assert!(top.is_dir(), "no heads for {stem} under {hd}");
                w.attributes = std::fs::read_to_string(top.join(".gitattributes")).unwrap_or_default();
                w.toplevel = top.to_string_lossy().into();
            }
            let t = Temp::new("prd-fixture");
            let mut h = store(&t);
            for (k, (file, _)) in pr_rounds(dir, stem, manifest.as_ref()).iter().enumerate() {
                let text = std::fs::read_to_string(Path::new(dir).join(file)).unwrap();
                let d = pr_digest::diff(&mut h, &w, &text, &meta("gh pr diff 1")).unwrap();
                let (rebuilt, ks) = rebuild_round(&h, &w.key, &d);
                assert!(rebuilt == text, "set {name} {stem} round {}: the digest's reads are not the stored diff", k + 1);
                rounds_n += 1;
                for (kind, c) in ks {
                    *kinds.entry(kind).or_default() += c;
                }
            }
        }
        let parts: usize = kinds.values().sum();
        println!("SET {name}: {} PRs, {rounds_n} rounds, {parts} parts rebuilt: {kinds:?}", names.len());
        total += rounds_n;
    }
    assert!(total > 0, "no rounds checked");
}
