//! `hook_rewrite::annotate_grep` only ever appends to a match line: with
//! nothing indexed the output is the input, and with every file indexed each
//! line keeps its text and the line count stays.
use atlas::code::Item;
use atlas::hook_rewrite::annotate_grep;
use proptest::prelude::*;

fn grep_output() -> impl Strategy<Value = String> {
    prop::collection::vec("[a-z]{1,8}\\.rs[:-][0-9]{1,3}[:-][ -~]{0,20}", 0..12).prop_map(|l| l.join("\n"))
}

proptest! {
    #[test]
    fn with_nothing_indexed_a_grep_passes_unchanged(content in grep_output()) {
        let out = annotate_grep(&content, "/nowhere", None, &mut |_| Ok(None)).unwrap();
        prop_assert_eq!(out, content);
    }

    #[test]
    fn an_annotation_only_appends(content in grep_output()) {
        let item = Item { name: "x".into(), kind: "fn".into(), line: 1, end: 10_000 };
        let out = annotate_grep(&content, "/nowhere", None, &mut |_| Ok(Some(vec![item.clone()]))).unwrap();
        let (a, b): (Vec<&str>, Vec<&str>) = (content.lines().collect(), out.lines().collect());
        prop_assert_eq!(a.len(), b.len(), "{:?}", out);
        for (x, y) in a.iter().zip(&b) {
            prop_assert!(y.starts_with(x), "{:?} became {:?}", x, y);
        }
    }
}
