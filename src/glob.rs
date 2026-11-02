//! A small, case-insensitive glob: `*` (within one folder), `**` (across
//! folders), `?` (one character). Everything else is literal.
//!
//! Matching runs the pattern as a set of positions over the text, one
//! character at a time, so it costs pattern length times text length at
//! most, whatever the pattern (a backtracking matcher is exponential in the
//! number of stars), and it does not recurse.

/// Matches `text` against `pattern`.
///
/// # Arguments
///
/// * `pattern` - the glob. When it has no `/`, it is matched against the
///   last path component only, so `PI-*.md` matches `Ideas/PI-001 X.md`.
/// * `path` - a `/`-separated path.
pub fn matches(pattern: &str, path: &str) -> bool {
    let target = if pattern.contains('/') { path } else { path.rsplit('/').next().unwrap_or(path) };
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n = p.len();
    let role = roles(&p);
    // `cur[i]`: the text so far can be matched by `p[..i]`, the rest of the
    // text still to be matched by `p[i..]`.
    let mut cur = vec![false; n + 1];
    cur[0] = true;
    close(&p, &role, &mut cur);
    let mut next = vec![false; n + 1];
    for c in target.to_lowercase().chars() {
        next.fill(false);
        let mut any = false;
        for i in 0..n {
            if !cur[i] {
                continue;
            }
            let to = match role[i] {
                // `**` takes any character, `/` included, into its loop
                // state (its second star), which keeps taking them.
                Role::Double => Some(i + 1),
                Role::Loop => Some(i),
                // `*` takes any character but `/`, and stays.
                Role::Single => (c != '/').then_some(i),
                Role::Char if p[i] == '?' => (c != '/').then_some(i + 1),
                Role::Char => (p[i] == c).then_some(i + 1),
            };
            if let Some(j) = to {
                next[j] = true;
                any = true;
            }
        }
        if !any {
            return false;
        }
        close(&p, &role, &mut next);
        std::mem::swap(&mut cur, &mut next);
    }
    cur[n]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The first star of `**`.
    Double,
    /// The second star of `**`: where `**` is once it has taken a
    /// character (so `**/` can no longer match zero folders).
    Loop,
    /// A lone `*`.
    Single,
    /// `?` or a literal.
    Char,
}

/// The role of each pattern position, read left to right.
fn roles(p: &[char]) -> Vec<Role> {
    let mut r = Vec::with_capacity(p.len());
    let mut i = 0;
    while i < p.len() {
        if p[i] == '*' && p.get(i + 1) == Some(&'*') {
            r.push(Role::Double);
            r.push(Role::Loop);
            i += 2;
        } else {
            r.push(if p[i] == '*' { Role::Single } else { Role::Char });
            i += 1;
        }
    }
    r
}

/// Adds the positions reachable without taking a character: past a `*`
/// (it may take none), past a `**`, past `**/` when the `**` took nothing
/// (zero folders), and out of a `**` that took something.
fn close(p: &[char], role: &[Role], s: &mut [bool]) {
    for i in 0..p.len() {
        if !s[i] {
            continue;
        }
        match role[i] {
            Role::Double => {
                s[i + 2] = true;
                if p.get(i + 2) == Some(&'/') {
                    s[i + 3] = true;
                }
            }
            Role::Loop | Role::Single => s[i + 1] = true,
            Role::Char => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn globs() {
        assert!(matches("PI-*.md", "Ideas/PI-001 X.md"));
        assert!(matches("pi-0??*", "Ideas/PI-001 X.md"));
        assert!(!matches("Ideas/*.md", "Ideas/sub/x.md"));
        assert!(matches("Ideas/**/*.md", "Ideas/sub/x.md"));
        assert!(matches("Ideas/**/*.md", "Ideas/x.md"));
        assert!(matches("**", "a/b/c.md"));
        assert!(!matches("*.txt", "a.md"));
    }

    #[test]
    fn edge_patterns() {
        assert!(matches("", ""));
        assert!(!matches("", "a"));
        assert!(matches("*", ""));
        assert!(matches("**/", "/"));
        assert!(matches("a/**/", "a/"));
        assert!(matches("***", "a/b"));
        assert!(!matches("a?", "a/"));
        assert!(matches("**/x", "x"));
        assert!(matches("**/x", "a/b/x"));
        assert!(!matches("*/x", "a/b/x"));
        // `**/` skips zero folders only when the `**` took nothing.
        assert!(!matches("**.md**/", "?.md\u{e9}"));
        assert!(matches("**.md**/", "?.md/"));
    }

    /// Many stars against a long name that never matches: exponential for
    /// a backtracking matcher, and a deep recursion for a long pattern.
    #[test]
    fn pathological_patterns_are_linear() {
        let name = "a".repeat(250);
        let t = std::time::Instant::now();
        assert!(!matches("*a*a*a*a*a*a*a*b", &name));
        assert!(!matches("*a*a*a*a*a*a*a*a*a*a*a*a*a*a*b", &name));
        assert!(!matches(&("**a".repeat(20) + "/b"), &("a/".repeat(100) + &name)));
        assert!(matches(&"*".repeat(100_000), &name));
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    }
}
