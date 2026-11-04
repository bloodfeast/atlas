//! Secret redaction for text atlas keeps from a session transcript: the
//! compaction checkpoint's tool targets (part (a) of
//! `docs/design/compaction.md`) and the opt-in transcript source (part (c)).
//!
//! The unit is the line: a line that looks like it carries a secret is
//! replaced whole by [`REDACTED`], so no fragment of a key survives. A line
//! is redacted when it
//!
//! * names a denied file ([`crate::sources::is_denied`]: `.env`, `*.pem`,
//!   `id_rsa`, ...), so a `cat .env` and its output go together;
//! * holds a key-shaped token: a known prefix (`sk-`, `ghp_`, `github_pat_`,
//!   `xoxb-`, `AKIA`, `AIza`, `glpat-`, ...), a PEM header, a JWT, or a
//!   `Bearer` credential;
//! * is an `.env`-style assignment whose name says key, token, secret,
//!   password, credential or auth (`export API_TOKEN=...`, `"apiKey": "..."`);
//! * holds a long base64 or hex run (24 or more characters) beside one of
//!   those words;
//! * holds a term of the private leak denylist (the file named by
//!   `ATLAS_LEAK_DENYLIST`, one term per line, `#` comments), matched as
//!   ASCII-lowercase substrings.
//!
//! An armored block (`-----BEGIN ...-----` through its `-----END ...-----`:
//! a PEM private key, a PGP key block, a certificate) is redacted whole,
//! from its BEGIN line through its END line, or to the end of the text when
//! the END never comes, so the body lines between, which match no rule by
//! themselves, go with it. The markers are found anywhere in a line, so a
//! block whose lines are JSON strings (`"-----BEGIN PRIVATE KEY-----\n" +`,
//! the body on the lines after) goes whole as well.
//!
//! Over-redaction is accepted: a line lost costs a re-read, a secret kept
//! costs the secret.

use std::sync::OnceLock;

/// What a redacted line reads as.
pub const REDACTED: &str = "[redacted]";

/// Prefixes of provider tokens (matched case-sensitively at a token start).
const PREFIXES: &[&str] = &["sk-", "sk_live_", "sk_test_", "rk_live_", "ghp_", "gho_", "ghs_", "ghu_", "ghr_", "github_pat_", "xoxb-", "xoxp-", "xoxa-", "xoxs-", "glpat-", "AIza", "hf_", "npm_", "pypi-", "dop_v1_", "SG.", "ya29."];

/// Words that make a nearby long run, or an assignment, a secret.
const WORDS: &[&str] = &["key", "token", "secret", "password", "passwd", "credential", "auth", "bearer", "private"];

/// The leak denylist's terms, lowercase: the file `ATLAS_LEAK_DENYLIST`
/// names, read once; empty when unset or unreadable.
pub fn denylist() -> &'static [String] {
    static LIST: OnceLock<Vec<String>> = OnceLock::new();
    LIST.get_or_init(|| {
        std::env::var_os("ATLAS_LEAK_DENYLIST")
            .filter(|v| !v.is_empty())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| parse_terms(&t))
            .unwrap_or_default()
    })
}

/// A denylist file's terms: one per line, lowercased, blank lines and `#`
/// comments skipped.
pub fn parse_terms(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_ascii_lowercase).collect()
}

fn is_b64(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=' | b'_' | b'-')
}

/// The longest run of base64-ish characters holding both a letter and a
/// digit (a word alone is not a key).
fn long_run(line: &str) -> usize {
    let mut best = 0;
    for run in line.as_bytes().split(|c| !is_b64(*c)) {
        if run.len() > best && run.iter().any(u8::is_ascii_digit) && run.iter().any(u8::is_ascii_alphabetic) {
            best = run.len();
        }
    }
    best
}

fn has_word(lower: &str) -> bool {
    WORDS.iter().any(|w| lower.contains(w))
}

/// Whether a token (split on anything but key characters) is key-shaped.
fn key_token(t: &str) -> bool {
    if PREFIXES.iter().any(|p| t.starts_with(p) && t.len() >= p.len() + 12) {
        return true;
    }
    // AWS access key ids.
    if (t.starts_with("AKIA") || t.starts_with("ASIA")) && t.len() == 20 && t[4..].bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
        return true;
    }
    // A JWT: three base64url parts, the first a JSON header.
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() == 3 && t.starts_with("eyJ") && parts.iter().all(|p| p.len() >= 8)
}

/// An `.env`-style or JSON assignment of a secret-named value:
/// `NAME=value`, `NAME: value`, `"name": "value"`, `export NAME=value`.
fn secret_assignment(line: &str) -> bool {
    let l = line.trim_start().trim_start_matches("export ").trim_start_matches(['"', '\'']);
    let Some(i) = l.find(['=', ':']) else { return false };
    let name = l[..i].trim_end().trim_end_matches(['"', '\'']);
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) {
        return false;
    }
    let value = l[i + 1..].trim().trim_matches(['"', '\'', ',']);
    let lower = name.to_ascii_lowercase();
    has_word(&lower) && value.len() >= 6 && !value.contains(' ')
}

/// A URL with a password in it: `scheme://user:password@host`.
fn url_credentials(line: &str) -> bool {
    line.match_indices("://").any(|(i, _)| {
        let rest = &line[i + 3..];
        let auth = rest.split(['/', ' ', '"']).next().unwrap_or("");
        auth.split_once('@').is_some_and(|(cred, host)| cred.contains(':') && !host.is_empty())
    })
}

/// Whether one line must be redacted, against `deny` (lowercase terms).
pub fn is_secret_line(line: &str, deny: &[String]) -> bool {
    let lower = line.to_ascii_lowercase();
    if deny.iter().any(|t| !t.is_empty() && lower.contains(t.as_str())) {
        return true;
    }
    if lower.contains("-----begin") && lower.contains("private key") {
        return true;
    }
    if lower.contains("bearer ") && long_run(line) >= 16 {
        return true;
    }
    for t in line.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '\\' | '+' | '='))) {
        let name = t.rsplit(['/', '\\']).next().unwrap_or(t);
        if !name.is_empty() && crate::sources::is_denied(name) {
            return true;
        }
        if key_token(t) {
            return true;
        }
    }
    if url_credentials(line) || secret_assignment(line) {
        return true;
    }
    has_word(&lower) && long_run(line) >= 24
}

/// The byte offset just past an armor marker `-----BEGIN <LABEL>-----` (or
/// `END`, as `word`) in `line`, the label being capitals, digits and spaces as RFC 7468 writes it;
/// None when the line has none.
fn armor(line: &str, word: &str) -> Option<usize> {
    let pat = format!("-----{word} ");
    let mut from = 0;
    while let Some(i) = line[from..].find(&pat) {
        let label = from + i + pat.len();
        let rest = &line[label..];
        let n = rest.bytes().take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b' ').count();
        if n > 0 && rest[n..].starts_with("-----") {
            return Some(label + n + 5);
        }
        from = label;
    }
    None
}

/// Whether each line of `text` (split on `\n`) must be redacted: the lines
/// [`is_secret_line`] finds, and every line of an armored block.
fn secret_lines(text: &str, deny: &[String]) -> Vec<bool> {
    let mut block = false;
    text.split('\n')
        .map(|l| {
            if block {
                // The block goes on to its END line, which goes with it.
                block = armor(l, "END").is_none();
                return true;
            }
            if let Some(b) = armor(l, "BEGIN") {
                // An END later on the same line (a JSON string with `\n`
                // escapes) closes it there.
                block = armor(&l[b..], "END").is_none();
                return true;
            }
            is_secret_line(l, deny)
        })
        .collect()
}

/// `text` with every secret-looking line, and every line of an armored
/// block, replaced by [`REDACTED`], against the leak denylist ([`denylist`]).
pub fn redact(text: &str) -> String {
    redact_with(text, denylist())
}

/// [`redact`] against the given terms.
pub fn redact_with(text: &str, deny: &[String]) -> String {
    let marks = secret_lines(text, deny);
    if !marks.contains(&true) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for (i, (l, secret)) in text.split('\n').zip(marks).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(if secret { REDACTED } else { l });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_key_shaped_lines_and_keeps_the_rest() {
        let deny = parse_terms("# private\nQuillfeather\n\n");
        // Synthesized secrets, none real.
        let cases = [
            "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwx",
            "export GITHUB_TOKEN=ghp_0123456789abcdefABCDEF0123456789abcd",
            "  \"apiKey\": \"zq81kd93jdk2\",",
            "aws AKIAABCDEFGHIJKLMNOP done",
            "Authorization: Bearer abcdef0123456789abcdef",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "cat H:/proj/.env",
            "type C:\\keys\\id_ed25519",
            "the token is 4f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.c2lnbmF0dXJlMTIz",
            "met quillfeather at noon",
            "DATABASE_URL=postgres://app:hunter2pass@db.internal/main",
        ];
        for c in cases {
            assert!(is_secret_line(c, &deny), "{c}");
        }
        let keep = [
            "cargo test --no-fail-fast",
            "see rules:r00412@3f9c2a1b7e40",
            "commit 2393075 merged",
            "the key insight is caching",
            "src/compact.rs:120 error[E0308]: mismatched types",
            "password reset flow is in auth.rs",
        ];
        for c in keep {
            assert!(!is_secret_line(c, &deny), "{c}");
        }
        assert_eq!(redact_with("a\nTOKEN=abcdef012345\nb", &deny), "a\n[redacted]\nb");
    }

    /// Review finding 2: only a block's BEGIN line was redacted; its body
    /// lines match no rule alone and were kept. Synthesized blocks, none real.
    #[test]
    fn armored_blocks_go_whole() {
        let body = ["MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj", "MkPxNZoBX8n2N8nwZ5Uu4Tj0r6dL5mT8p2qQy7wX3rNcLsVbEeKoPq1aHzD9fG2", "q7Hn3T5w=="];
        for (begin, end) in [
            ("-----BEGIN PRIVATE KEY-----", "-----END PRIVATE KEY-----"),
            ("-----BEGIN OPENSSH PRIVATE KEY-----", "-----END OPENSSH PRIVATE KEY-----"),
            ("-----BEGIN PGP PRIVATE KEY BLOCK-----", "-----END PGP PRIVATE KEY BLOCK-----"),
        ] {
            let text = format!("$ cat deploy_key\n{begin}\nVersion: Test\n\n{}\n{end}\nafter the key", body.join("\n"));
            let out = redact_with(&text, &[]);
            for b in body {
                assert!(!out.contains(b), "{begin}: body line kept:\n{out}");
            }
            assert!(out.ends_with("[redacted]\nafter the key"), "{out}");
            assert!(out.starts_with("$ cat deploy_key\n"), "{out}");
        }
        // No END (output cut short): redacted to the end.
        let cut = format!("-----BEGIN RSA PRIVATE KEY-----\n{}", body.join("\n"));
        assert!(redact_with(&cut, &[]).split('\n').all(|l| l == REDACTED));
        // A key split across JSON string lines.
        let json = format!("{{\n  \"private_key\": \"-----BEGIN PRIVATE KEY-----\\n\" +\n  \"{}\\n\" +\n  \"{}\\n\" +\n  \"-----END PRIVATE KEY-----\\n\",\n  \"client_email\": \"bot@example.iam\"\n}}", body[0], body[1]);
        let out = redact_with(&json, &[]);
        assert!(!out.contains(body[0]) && !out.contains(body[1]), "{out}");
        assert!(out.contains("client_email"), "{out}");
        // BEGIN and END on one line (escaped newlines) close there.
        let one = format!("\"private_key\": \"-----BEGIN PRIVATE KEY-----\\n{}\\n-----END PRIVATE KEY-----\\n\",\nnext line", body[0]);
        assert_eq!(redact_with(&one, &[]), "[redacted]\nnext line");
        // A dash rule in prose is not a block.
        let prose = "-----BEGIN here-----x\n----- begin -----\nplain";
        assert_eq!(redact_with(prose, &[]), prose);
    }
}
