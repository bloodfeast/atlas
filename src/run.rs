//! `atlas run`: runs a command, stores its whole output as a handle and
//! prints the digest and the handle (or the output itself when it is under
//! the threshold), then exits with the command's exit code.
//!
//! The command is spawned without a shell; stdout and stderr are read on
//! two threads into one buffer in line-arrival order. A pipeline goes
//! through `atlas run -- sh -c '...'` (or `cmd /C`), which the user writes.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::digest::{self, Shape};
use crate::handles::{HandleCfg, Handles, Meta};

/// The usage line of `atlas run`.
pub const USAGE: &str = "usage: atlas run [--digest test|build|grep|listing|source|summary|generic] [--timeout S] [--label TEXT] -- CMD [ARGS...]";

pub(crate) fn forced(name: &str) -> Result<Shape, String> {
    Ok(match name {
        "test" | "cargo-test" | "nextest" => Shape::CargoTest,
        "build" | "cargo" | "cargo-build" | "check" | "clippy" => Shape::CargoBuild,
        "grep" => Shape::Grep,
        "listing" | "files" => Shape::Listing,
        "source" | "outline" => Shape::Source(digest::Lang::Auto),
        "summary" | "report" => Shape::Summary,
        "generic" => Shape::Generic,
        o => return Err(format!("--digest {o:?}: expected test, build, grep, listing, source, summary or generic")),
    })
}

/// Shell-style quoting of one word for the label.
fn quote(w: &str) -> String {
    if !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:+,@%".contains(c)) {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

fn pump<R: Read + Send + 'static>(r: R, tx: mpsc::Sender<Vec<u8>>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut br = BufReader::new(r);
        loop {
            let mut line = Vec::new();
            match br.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// The `atlas run` subcommand.
///
/// # Arguments
///
/// * `args` - the words after `run`.
/// * `db` - the index database; the handle store lives beside it.
/// * `lean` - the configuration's `lean` section, if any.
///
/// # Errors
///
/// A message for the user when the subcommand cannot run.
pub fn cli(args: Vec<String>, db: &std::path::Path, lean: Option<&crate::json::Value>) -> Result<ExitCode, String> {
    let mut it = args.into_iter();
    let mut shape: Option<Shape> = None;
    let mut timeout: Option<Duration> = None;
    let mut label: Option<String> = None;
    let mut cmd: Vec<String> = Vec::new();
    while let Some(w) = it.next() {
        match w.as_str() {
            "--" => {
                cmd = it.by_ref().collect();
                break;
            }
            "--digest" => shape = Some(forced(&it.next().ok_or("--digest needs a name")?)?),
            "--timeout" => {
                let s: f64 = it.next().and_then(|t| t.parse().ok()).filter(|t: &f64| *t > 0.0).ok_or("--timeout needs seconds")?;
                timeout = Some(Duration::from_secs_f64(s));
            }
            "--label" => label = Some(it.next().ok_or("--label needs text")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            _ => {
                // No `--`: the first unknown word starts the command.
                cmd.push(w);
                cmd.extend(it.by_ref());
                break;
            }
        }
    }
    let Some(program) = cmd.first().cloned() else { return Err(USAGE.into()) };
    let line = cmd.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ");
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let t0 = Instant::now();
    let mut child = Command::new(&program)
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("atlas run: {program}: {e}"))?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let a = pump(child.stdout.take().ok_or("no stdout")?, tx.clone());
    let b = pump(child.stderr.take().ok_or("no stderr")?, tx);
    let mut buf: Vec<u8> = Vec::new();
    let mut timed_out = false;
    loop {
        let wait = match timeout {
            Some(t) => match t.checked_sub(t0.elapsed()) {
                Some(left) => left,
                None => {
                    timed_out = true;
                    let _ = child.kill();
                    break;
                }
            },
            None => Duration::from_secs(3600),
        };
        match rx.recv_timeout(wait) {
            Ok(l) => {
                buf.extend_from_slice(&l);
                if !l.ends_with(b"\n") {
                    buf.push(b'\n');
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait().map_err(|e| format!("atlas run: wait: {e}"))?;
    for l in rx.try_iter() {
        buf.extend_from_slice(&l);
    }
    let _ = (a.join(), b.join());
    let ms = t0.elapsed().as_millis() as i64;
    let code = if timed_out { 124 } else { status.code().unwrap_or(1) };
    if timed_out {
        buf.extend_from_slice(format!("atlas run: timed out after {:.0} s; killed\n", timeout.map_or(0.0, |t| t.as_secs_f64())).as_bytes());
    }
    let cfg = HandleCfg::from_lean(lean);
    let text = String::from_utf8_lossy(&buf).into_owned();
    // A command naming a deny-listed file still runs and prints; only the
    // handle is left out.
    if buf.len() < cfg.threshold_bytes || cmd.iter().any(|w| crate::hook_rewrite::command_denied(w, &cwd)) {
        print!("{text}");
        return Ok(ExitCode::from(code.clamp(0, 255) as u8));
    }
    let meta = Meta {
        kind: "run".into(),
        label: label.unwrap_or_else(|| line.clone()),
        cmd: line,
        cwd,
        session: String::new(),
        exit_code: Some(code as i64),
        ms: Some(ms),
    };
    match Handles::open(db, cfg).and_then(|mut h| h.put(&buf, &meta)) {
        Ok(row) => {
            let shape = shape.unwrap_or_else(|| Handles::shape_of(&row, &text));
            let d = digest::digest(&row.id, &row.meta.label, shape, &text, Some(code), cfg.digest_bytes);
            print!("{d}");
            println!("({:.1} s)", ms as f64 / 1000.0);
        }
        Err(e) => {
            // Fail open: the output as it is.
            eprintln!("atlas run: handle store: {e}");
            print!("{text}");
        }
    }
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}
