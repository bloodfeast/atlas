//! Public #15: the shell routing check never recurses without bound. An
//! unclosed quote left by the split at `|`, `;`, `&`, `(`, `)`, `$` or a
//! backtick (`c" file`, a piece of `grep -n "a\|b\|c" file`) is one word
//! holding the whole piece; the recursive scan took it up again forever and
//! overflowed the hook thread's stack, which aborted the server. Run on a
//! 64 KiB stack, so any recursion that grows with the input fails here too.

use atlas::hook_rewrite::routable;

/// `levels` quote layers, each inside the last (`"a 'a "a ... x" ...' "`),
/// so each scan of a word finds the next layer as one word of its own.
fn nested(levels: usize) -> String {
    let mut s = "x y".to_string();
    for i in 0..levels {
        let q = if i % 2 == 0 { '\'' } else { '"' };
        s = format!("a {q}{s}{q}");
    }
    s
}

#[test]
fn unclosed_and_nested_quotes_never_overflow() {
    let deep = format!("grep {}", nested(2_000));
    let t = std::thread::Builder::new()
        .name("stack-test".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            assert!(routable(r#"grep -n "a\|b\|c" src/x.rs"#));
            assert!(routable(r#"grep -n "a\|b\|c" f; grep -n "d\|e" g; grep "x\|y" h"#));
            assert!(routable(r#"grep "a(b" f"#));
            assert!(routable(r#"grep "\$x y" f"#));
            assert!(routable(r#"git log --format="%h|%s" -5"#));
            assert!(routable(r#"grep x 'unclosed  "#));
            assert!(routable(&deep));
            // Still caught: git behind a quoted command, at any depth, and
            // after an unclosed quote the split left.
            assert!(!routable(r#"grep a f; bash -c "git push""#));
            assert!(!routable(r#"grep a f; sh -c "bash -c 'git push origin'" x"#));
            assert!(!routable(r#"grep "a\|b" f; bash -c "git push"#));
            assert!(!routable(r#"find . -name "a|b" -exec git push \;"#));
        })
        .unwrap();
    t.join().unwrap();
}
