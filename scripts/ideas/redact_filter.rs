//! The freeze's redaction step: one escaped text per stdin line, the same text
//! through `redact::redact_with` and the leak denylist on stdout. Built with one
//! `rustc -O`, so the corpus is redacted by atlas's own code, no copy of its rules.

#[path = "../../src/redact.rs"]
#[allow(dead_code)]
mod redact;

/// What `redact.rs` asks of the crate: the name rules of `sources::is_denied`.
mod sources {
    pub fn is_denied(name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        let ext = n.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        if n == ".env" || n.starts_with(".env.") || matches!(ext, "env" | "pem" | "key" | "p12" | "pfx" | "jks" | "keystore" | "kdbx" | "asc" | "gpg" | "credentials" | "secret") {
            return true;
        }
        if n.ends_with(".md") {
            return false;
        }
        ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"].iter().any(|k| n.starts_with(k))
            || n == "credentials"
            || n.starts_with("credentials.")
            || matches!(n.as_str(), ".netrc" | "_netrc" | ".npmrc" | ".pypirc")
            || n.starts_with("secrets.")
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

/// One row: the secret-pattern rules on the original text first, so a term cut out
/// cannot break up a key those rules would have caught; then the terms by span.
fn redact_row(text: &str, deny: &[String]) -> (String, bool, bool) {
    let ruled = redact::redact_with(text, &[]);
    let by_rule = ruled != text;
    let (kept, by_term) = redact::redact_terms(&ruled, deny);
    (kept, by_rule, by_term)
}

/// `--scan`: how many stdin values still hold a term. Counts only, and a failure
/// exits nonzero; no term is ever printed.
fn scan(deny: &[String]) -> std::process::ExitCode {
    use std::io::BufRead;
    let (mut values, mut hits) = (0u64, 0u64);
    for line in std::io::stdin().lock().lines() {
        let text = unescape(&line.expect("stdin is utf-8"));
        values += 1;
        if redact::term_marks(text.to_ascii_lowercase().as_bytes(), deny).contains(&true) {
            hits += 1;
        }
    }
    println!("scan: terms={} values={values} values_with_a_term={hits}", deny.len());
    if deny.is_empty() || hits > 0 { std::process::ExitCode::FAILURE } else { std::process::ExitCode::SUCCESS }
}

fn main() -> std::process::ExitCode {
    use std::io::{BufRead, Write};
    let deny = redact::denylist();
    if std::env::args().any(|a| a == "--scan") {
        return scan(deny);
    }
    let stdout = std::io::stdout();
    let mut w = std::io::BufWriter::new(stdout.lock());
    for line in std::io::stdin().lock().lines() {
        let text = unescape(&line.expect("stdin is utf-8"));
        let (kept, by_rule, by_term) = redact_row(&text, deny);
        let esc = kept.replace('\\', "\\\\").replace('\r', "\\r").replace('\n', "\\n");
        writeln!(w, "{}{}\t{esc}", u8::from(by_rule), u8::from(by_term)).expect("stdout");
    }
    eprintln!("denylist terms: {}", deny.len());
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny(terms: &[&str]) -> Vec<String> {
        redact::parse_terms(&terms.join("\n"))
    }

    #[test]
    fn only_the_term_goes_and_case_does_not_save_it() {
        let d = deny(&["zebra"]);
        assert_eq!(redact_row("keep the ZeBrA pen here", &d).0, "keep the [redacted] pen here");
        assert_eq!(redact_row("no match here", &d), ("no match here".to_string(), false, false));
    }

    #[test]
    fn a_term_inside_a_word_matches_as_the_live_rule_does() {
        let d = deny(&["zebra"]);
        assert!(redact::is_secret_line("zebras", &d));
        assert_eq!(redact_row("zebras run", &d).0, "[redacted]s run");
    }

    #[test]
    fn overlapping_terms_and_repeats_leave_nothing() {
        let d = deny(&["abc", "bcd", "aa"]);
        let (kept, _, by_term) = redact_row("x abcd y aaa z abc", &d);
        assert!(by_term);
        assert_eq!(kept, "x [redacted] y [redacted] z [redacted]");
    }

    #[test]
    fn secret_rules_still_blank_the_line_and_run_on_the_original() {
        let d = deny(&["ghp"]);
        let token = format!("{}{}", "ghp_", "a".repeat(36));
        let (kept, by_rule, _) = redact_row(&format!("use {token} now\nand keep this"), &d);
        assert!(by_rule);
        assert_eq!(kept, "[redacted]\nand keep this");
    }

    #[test]
    fn non_ascii_around_a_term_survives() {
        let d = deny(&["zebra"]);
        assert_eq!(redact_row("caf\u{e9} zebra \u{2192} ok", &d).0, "caf\u{e9} [redacted] \u{2192} ok");
    }

    #[test]
    fn the_scan_matcher_sees_what_redaction_removes() {
        let d = deny(&["zebra"]);
        assert!(redact::term_marks(b"a zebra b", &d).contains(&true));
        let (kept, _, _) = redact_row("a ZEBRA b", &d);
        assert!(!redact::term_marks(kept.to_ascii_lowercase().as_bytes(), &d).contains(&true));
    }
}
