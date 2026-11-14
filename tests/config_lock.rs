//! Concurrent `atlas settings` changes: no acknowledged write is lost, no
//! backup is overwritten, and undo walks back through every change. On a
//! scratch home, through the binary.

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use atlas::fsio::FileLock;
use atlas::json;
use common::*;

/// A scratch home holding an `atlas.json` with no sources yet, and `extra`
/// folders to add.
struct Home {
    t: Temp,
}

impl Home {
    fn new(tag: &str) -> Home {
        let h = Home { t: Temp::new(tag) };
        std::fs::create_dir_all(h.atlas().join("notes")).unwrap();
        std::fs::create_dir_all(h.claude()).unwrap();
        std::fs::write(h.config(), ORIGINAL).unwrap();
        h
    }
    fn atlas(&self) -> PathBuf {
        self.t.path().join("atlas")
    }
    fn claude(&self) -> PathBuf {
        self.t.path().join("claude")
    }
    fn config(&self) -> PathBuf {
        self.atlas().join("atlas.json")
    }
    fn folder(&self, name: &str) -> PathBuf {
        let d = self.t.path().join("work").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("README.md"), "# readme\n").unwrap();
        d
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.arg("settings")
            .args(args)
            .env("ATLAS_HOME", self.atlas())
            .env("CLAUDE_CONFIG_DIR", self.claude())
            .env("HOME", self.t.path())
            .env("USERPROFILE", self.t.path())
            .env_remove("ATLAS_CONFIG")
            .stdin(Stdio::null());
        c
    }
    fn add(&self, dir: &Path) -> Child {
        self.command(&["add-project", "--yes"]).arg(dir).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    }
    fn sources(&self) -> Vec<String> {
        let v = json::parse(&std::fs::read_to_string(self.config()).unwrap()).unwrap();
        v.get("sources").and_then(json::Value::as_array).map(|a| a.iter().filter_map(|s| s.get("name").and_then(json::Value::as_str).map(str::to_string)).collect()).unwrap_or_default()
    }
    fn backups(&self) -> Vec<String> {
        std::fs::read_dir(self.atlas().join("atlas-backups")).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".json")).collect()).unwrap_or_default()
    }
}

const ORIGINAL: &str = "{\n  \"vaults\": [{\"name\": \"notes\", \"path\": \"notes\"}],\n  \"models\": {\"auto_download\": false, \"files\": []},\n  \"sources\": []\n}\n";

/// A change that has read the file before another process wrote it would lose
/// that write, even with a lock around its own write alone: this holds the
/// lock, sees the child reach it, writes, and only then lets the child go.
#[test]
fn a_change_reads_the_file_inside_the_lock() {
    let h = Home::new("lock-inside");
    let (one, other) = (h.folder("one"), h.folder("other"));
    let held = FileLock::acquire(&h.config()).unwrap();
    let mut child = h.add(&one);
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let first = lines.next().expect("the child says it waits").unwrap();
    assert!(first.contains("waiting for") && first.contains("atlas.json.lock"), "{first}");
    // What another process would have written while the child waits.
    let text = std::fs::read_to_string(h.config()).unwrap();
    let source = format!("\"sources\": [{{\"name\": \"other\", \"root\": {:?}, \"include\": [\"**/*.md\"]}}]", other.to_string_lossy().replace('\\', "/"));
    std::fs::write(h.config(), text.replace("\"sources\": []", &source)).unwrap();
    drop(held);
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}\n{rest:?}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(h.sources(), ["other", "one"], "both sources are kept");
    assert!(!rest.iter().any(|l| l.contains("waiting for")), "the wait is said once: {rest:?}");
}

/// Every process exits 0, keeps its source and has its own backup, and undo
/// walks all the way back to the original bytes.
#[test]
fn many_processes_keep_every_write_and_every_backup() {
    const PROCESSES: usize = 8;
    let h = Home::new("lock-many");
    for round in 0..3 {
        let before = h.sources().len();
        let folders: Vec<PathBuf> = (0..PROCESSES).map(|i| h.folder(&format!("r{round}p{i}"))).collect();
        let kids: Vec<Child> = folders.iter().map(|d| h.add(d)).collect();
        for k in kids {
            let o = k.wait_with_output().unwrap();
            assert!(o.status.success(), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        }
        assert_eq!(h.sources().len(), before + PROCESSES, "round {round}: {:?}", h.sources());
        assert_eq!(h.backups().len(), (round + 1) * PROCESSES, "one backup per process: {:?}", h.backups());
    }
    for _ in 0..3 * PROCESSES {
        let o = h.command(&["undo", "--yes"]).output().unwrap();
        assert!(o.status.success(), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }
    assert_eq!(std::fs::read_to_string(h.config()).unwrap(), ORIGINAL, "every undo restored one change");
    assert!(h.backups().is_empty(), "{:?}", h.backups());
}

/// Backups taken by processes holding different locks are never one change.
#[test]
fn backups_of_the_two_files_never_share_a_stamp() {
    let h = Home::new("lock-two");
    let mut kids: Vec<Child> = Vec::new();
    for i in 0..6 {
        kids.push(h.command(&["set", "goal-checkin-120", &(100 + i).to_string()]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap());
        kids.push(h.add(&h.folder(&format!("two{i}"))));
    }
    for k in kids {
        let o = k.wait_with_output().unwrap();
        assert!(o.status.success(), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }
    let stamps = |dir: PathBuf| -> Vec<String> {
        std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".json")).map(|n| n.split_once('.').unwrap().1.rsplit_once('.').unwrap().0.to_string()).collect()
    };
    let (a, s) = (stamps(h.atlas().join("atlas-backups")), stamps(h.claude().join("atlas-backups")));
    assert_eq!(a.len(), 6);
    // The first settings write finds no file: an `.absent` record, not counted.
    assert_eq!(s.len(), 5);
    assert!(!a.iter().any(|x| s.contains(x)), "{a:?} {s:?}");
}

/// A lock past its deadline is an error naming the lock file, and nothing is
/// written. The operating system frees a dead process's file lock, which is why this is
/// a lock and not a lock file; that is not exercised here.
#[test]
fn the_deadline_names_the_lock_and_a_second_handle_waits() {
    let h = Home::new("lock-deadline");
    let held = FileLock::acquire(&h.config()).unwrap();
    let e = FileLock::acquire_within(&h.config(), Duration::from_millis(100)).unwrap_err();
    assert!(e.contains("atlas.json.lock") && e.contains("nothing written"), "{e}");
    drop(held);
    FileLock::acquire_within(&h.config(), Duration::from_millis(100)).expect("free again");
    // The guarded file is replaced by renames, so its lock is its own file.
    assert!(h.atlas().join("atlas.json.lock").is_file());
}

#[test]
fn a_killed_waiter_writes_nothing_and_the_lock_stays_usable() {
    let h = Home::new("lock-kill");
    let dir = h.folder("one");
    let held = FileLock::acquire(&h.config()).unwrap();
    let mut waiting = h.add(&dir);
    let stderr = waiting.stderr.take().unwrap();
    assert!(BufReader::new(stderr).lines().next().unwrap().unwrap().contains("waiting for"));
    let _ = waiting.kill();
    let _ = waiting.wait();
    drop(held);
    assert_eq!(h.sources().len(), 0, "the killed process wrote nothing");
    let o = h.add(&dir).wait_with_output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(h.sources(), ["one"]);
}

/// Every writer of the file passes through the lock: each waits for it.
#[test]
fn every_atlas_json_writer_waits_for_the_lock() {
    let h = Home::new("lock-writers");
    let repo = h.t.path().join("repos");
    std::fs::create_dir_all(repo.join("r1").join(".git")).unwrap();
    std::fs::write(repo.join("r1").join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let cases: Vec<Vec<String>> = vec![
        vec!["set".into(), "update-pin".into(), "0.1.0".into()],
        vec!["off".into(), "update".into()],
        vec!["orchestrate".into(), "--yes".into()],
        vec!["migrate-projects".into(), "alpha".into()],
        vec!["add-project".into(), "--new".into(), repo.to_string_lossy().to_string()],
    ];
    for args in cases {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let held = FileLock::acquire(&h.config()).unwrap();
        let mut c = h.command(&refs).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let stderr = c.stderr.take().unwrap();
        let line = BufReader::new(stderr).lines().next().map(|l| l.unwrap()).unwrap_or_default();
        assert!(line.contains("waiting for"), "{args:?}: {line}");
        drop(held);
        let o = c.wait_with_output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stdout));
        // Each round starts from the original, so each writer has its case.
        std::fs::write(h.config(), ORIGINAL).unwrap();
        let _ = std::fs::remove_file(repo.join(atlas::project::PROJECT_FILE));
    }
    // Writers that need a state first.
    let waits = |args: &[&str]| {
        let held = FileLock::acquire(&h.config()).unwrap();
        let mut c = h.command(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let line = BufReader::new(c.stderr.take().unwrap()).lines().next().map(|l| l.unwrap()).unwrap_or_default();
        assert!(line.contains("waiting for"), "{args:?}: {line}");
        drop(held);
        let o = c.wait_with_output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stdout));
    };
    waits(&["on", "update"]);
    assert!(h.command(&["set", "update-pin", "0.1.0"]).output().unwrap().status.success());
    waits(&["undo", "--yes"]);
    assert!(h.command(&["migrate-projects", "alpha"]).output().unwrap().status.success());
    waits(&["add-project", "--to", "alpha", &repo.join("r1").to_string_lossy()]);
}

/// A folder added while another change moves the setup into projects does not
/// become a top-level entry, which would join every project.
#[test]
fn an_add_that_waited_through_a_move_into_projects_writes_nothing() {
    let h = Home::new("lock-moved");
    let dir = h.folder("one");
    let held = FileLock::acquire(&h.config()).unwrap();
    let mut c = h.add(&dir);
    let line = BufReader::new(c.stderr.take().unwrap()).lines().next().unwrap().unwrap();
    assert!(line.contains("waiting for"), "{line}");
    let text = std::fs::read_to_string(h.config()).unwrap();
    let moved = text.replacen('{', "{\"projects\": [],", 1);
    std::fs::write(h.config(), &moved).unwrap();
    drop(held);
    let o = c.wait_with_output().unwrap();
    assert!(!o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(std::fs::read_to_string(h.config()).unwrap(), moved, "nothing written");
}

/// A dry run writes nothing, a lock file included.
#[test]
fn a_dry_run_makes_no_lock_file() {
    let h = Home::new("lock-dry");
    let d = h.folder("one");
    let o = h.command(&["add-project", "--dry-run"]).arg(&d).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!h.atlas().join("atlas.json.lock").exists());
}

/// `undo` with a question: the list is read again after the answer, and a
/// newer change refuses it. Scripted through the library's prompt path by a
/// change made between the summary and the answer is not reachable from a
/// binary without a terminal, so it is driven through the library.
#[test]
fn undo_refuses_when_a_newer_change_came_after_the_question() {
    use atlas::claude_settings::{run, Io, Paths};
    let h = Home::new("lock-undo");
    let d = h.folder("one");
    assert!(h.add(&d).wait_with_output().unwrap().status.success());
    let p = Paths { claude_dir: h.claude(), atlas_config: Some(h.config()), state: None, db: Some(h.atlas().join("state").join("atlas.sqlite")), user_state: None, project: None };
    // A reader that makes a newer change when it is asked for the answer.
    struct Answer<'a>(&'a Home, bool);
    impl std::io::Read for Answer<'_> {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }
    impl std::io::BufRead for Answer<'_> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            if !self.1 {
                self.1 = true;
                let other = self.0.folder("newer");
                assert!(self.0.add(&other).wait_with_output().unwrap().status.success());
            }
            Ok(b"y\n")
        }
        fn consume(&mut self, _: usize) {}
    }
    let before_undo = {
        let mut out = Vec::new();
        let mut input = Answer(&h, false);
        let code = run(&["undo".to_string()], &p, &mut Io { out: &mut out, input: &mut input, interactive: true, version: None, dry_run: false, machine: None }).unwrap();
        (code, String::from_utf8(out).unwrap())
    };
    assert_eq!(before_undo.0, std::process::ExitCode::FAILURE, "{}", before_undo.1);
    assert!(before_undo.1.contains("Nothing reverted: another atlas change came after"), "{}", before_undo.1);
    assert_eq!(h.sources(), ["one", "newer"], "nothing was reverted");
}
