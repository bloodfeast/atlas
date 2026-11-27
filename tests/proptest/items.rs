//! `digest::item_end` and `brace_body_end` stay inside the file and never end
//! an item before it starts, whatever the braces look like.
use atlas::digest;
use proptest::prelude::*;

proptest! {
    #[test]
    fn an_item_ends_inside_its_file(lines in prop::collection::vec("[ a-z{}();\"'/*]{0,12}", 1..30), start in 0usize..30) {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let start = start % refs.len();
        let end = digest::item_end(&refs, start);
        prop_assert!(start <= end && end <= refs.len(), "start {} end {} of {}", start, end, refs.len());
        if let Some(e) = digest::brace_body_end(&refs, start) {
            prop_assert!(start <= e && e <= refs.len(), "start {} body end {} of {}", start, e, refs.len());
        }
    }
}
