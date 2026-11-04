//! The release's secret scan: provider-key shapes in raw bytes, the kind
//! GitHub push protection (and other secret scanners) refuse a push for.
//! `cargo xtask release` runs it over every release asset before anything
//! is pushed, and `tests/secret_scan.rs` runs it over the built atlas
//! binary; the test includes this file by path, so it holds no `crate::`
//! path.
//!
//! v0.2.0's sign step was refused by push protection over a false positive:
//! the linker had packed `src/redact.rs`'s prefix literals end to end, so
//! the binary held `sk_test_` followed by thirty-odd token characters. The
//! scan exists so that a release can never meet that refusal at the push;
//! it stops at the step before, naming the file, the byte offset and the
//! rule.
//!
//! # Matching
//!
//! A rule is a prefix at a token start (the byte before it is not an ASCII
//! letter or digit) followed by at least `min` bytes of its body class. The
//! thresholds are looser than the providers' exact formats (no checksums,
//! no fixed lengths), so a near miss of a real format is still caught: a
//! false positive here costs a look, at the push it costs the release. A
//! PEM private key header counts when a base64 body follows it.
//!
//! A hit is reported by file, byte offset, rule and run length. The bytes
//! after the prefix are never printed, so the report cannot leak a key.

/// What may follow a prefix.
#[derive(Clone, Copy)]
enum Body {
    /// ASCII letters, digits, `_` and `-`.
    Token,
    /// ASCII letters and digits.
    Alnum,
    /// ASCII capitals and digits.
    Upper,
}

impl Body {
    fn admits(self, b: u8) -> bool {
        match self {
            Body::Token => b.is_ascii_alphanumeric() || b == b'_' || b == b'-',
            Body::Alnum => b.is_ascii_alphanumeric(),
            Body::Upper => b.is_ascii_uppercase() || b.is_ascii_digit(),
        }
    }
}

/// One provider-key shape.
struct Rule {
    name: &'static str,
    prefix: &'static [u8],
    body: Body,
    min: usize,
}

const fn rule(name: &'static str, prefix: &'static [u8], body: Body, min: usize) -> Rule {
    Rule { name, prefix, body, min }
}

/// The shapes, after the common secret-scanning patterns (GitHub's
/// supported patterns and gitleaks' defaults), loosened as the module docs
/// say.
const RULES: &[Rule] = &[
    rule("GitHub personal access token", b"ghp_", Body::Token, 30),
    rule("GitHub OAuth token", b"gho_", Body::Token, 30),
    rule("GitHub user-to-server token", b"ghu_", Body::Token, 30),
    rule("GitHub server-to-server token", b"ghs_", Body::Token, 30),
    rule("GitHub refresh token", b"ghr_", Body::Token, 30),
    rule("GitHub fine-grained token", b"github_pat_", Body::Token, 40),
    rule("live secret key (Stripe, Highnote)", b"sk_live_", Body::Token, 16),
    rule("test secret key (Stripe, Highnote)", b"sk_test_", Body::Token, 16),
    rule("live restricted key (Stripe, Highnote)", b"rk_live_", Body::Token, 16),
    rule("test restricted key (Stripe, Highnote)", b"rk_test_", Body::Token, 16),
    rule("OpenAI or Anthropic key", b"sk-", Body::Token, 32),
    rule("Slack bot token", b"xoxb-", Body::Token, 10),
    rule("Slack user token", b"xoxp-", Body::Token, 10),
    rule("Slack app token", b"xoxa-", Body::Token, 10),
    rule("Slack refresh token", b"xoxr-", Body::Token, 10),
    rule("Slack session token", b"xoxs-", Body::Token, 10),
    rule("Slack webhook", b"hooks.slack.com/services/", Body::Token, 8),
    rule("GitLab personal access token", b"glpat-", Body::Token, 20),
    rule("Google API key", b"AIza", Body::Token, 30),
    rule("Google OAuth access token", b"ya29.", Body::Token, 30),
    rule("Hugging Face token", b"hf_", Body::Alnum, 30),
    rule("npm token", b"npm_", Body::Alnum, 30),
    rule("PyPI token", b"pypi-", Body::Token, 50),
    rule("DigitalOcean token", b"dop_v1_", Body::Alnum, 40),
    rule("SendGrid key", b"SG.", Body::Token, 20),
    rule("AWS access key id", b"AKIA", Body::Upper, 16),
    rule("AWS temporary access key id", b"ASIA", Body::Upper, 16),
];

/// One secret-shaped run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Byte offset of the prefix.
    pub offset: usize,
    /// The rule's name.
    pub rule: &'static str,
    /// The rule's prefix (safe to print: it is no secret).
    pub prefix: String,
    /// Bytes of the body class after the prefix.
    pub run: usize,
}

impl std::fmt::Display for Hit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "byte {}: {} (`{}` then {} key characters)", self.offset, self.rule, self.prefix, self.run)
    }
}

fn token_start(bytes: &[u8], i: usize) -> bool {
    i == 0 || !bytes[i - 1].is_ascii_alphanumeric()
}

/// The PEM private key header at `i` (`-----BEGIN [LABEL ]PRIVATE
/// KEY-----`, a label of capitals and spaces) when at least 40 base64 bytes
/// follow it after one line break: the length of that body and the offset
/// just past it, else None.
fn pem_key(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    const BEGIN: &[u8] = b"-----BEGIN ";
    let rest = bytes.get(i..)?.strip_prefix(BEGIN)?;
    let label = rest.iter().take(64).take_while(|b| b.is_ascii_uppercase() || **b == b' ').count();
    if !rest[..label].ends_with(b"PRIVATE KEY") {
        return None;
    }
    let after = rest[label..].strip_prefix(b"-----")?;
    let after = after.strip_prefix(b"\r").unwrap_or(after);
    let after = after.strip_prefix(b"\n").or_else(|| after.strip_prefix(b"\\n"))?;
    let n = after.iter().take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')).count();
    (n >= 40).then(|| (n, bytes.len() - after.len() + n))
}

/// Every secret-shaped run in `bytes`, in offset order. A run is reported
/// once, by the first rule whose prefix starts it.
pub fn scan(bytes: &[u8]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let mut next = i + 1;
        if token_start(bytes, i) {
            if bytes[i] == b'-' {
                if let Some((n, end)) = pem_key(bytes, i) {
                    hits.push(Hit { offset: i, rule: "PEM private key", prefix: "-----BEGIN ... PRIVATE KEY-----".into(), run: n });
                    next = end;
                }
            }
            for r in RULES {
                if bytes[i..].starts_with(r.prefix) {
                    let body = &bytes[i + r.prefix.len()..];
                    let n = body.iter().take_while(|b| r.body.admits(**b)).count();
                    if n >= r.min {
                        hits.push(Hit { offset: i, rule: r.name, prefix: String::from_utf8_lossy(r.prefix).into_owned(), run: n });
                        next = i + r.prefix.len() + n;
                        break;
                    }
                }
            }
        }
        i = next.max(i + 1);
    }
    hits
}

/// The scan of every file under a folder, as `(relative path, hit)` lines
/// sorted by path: what the release prints before it refuses.
pub fn scan_files(files: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut out = Vec::new();
    for (name, bytes) in files {
        for h in scan(bytes) {
            out.push(format!("{name} ({h})"));
        }
    }
    out
}
