//! `atlas::json`: what it prints, it parses back, and no input makes it panic.
use atlas::json::{self, Map, Value};
use proptest::prelude::*;

fn value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![Just(Value::Null), any::<bool>().prop_map(Value::from), any::<i64>().prop_map(Value::from), any::<String>().prop_map(Value::from)];
    leaf.prop_recursive(3, 32, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::vec((any::<String>(), inner), 0..4).prop_map(|kv| {
                let mut m = Map::new();
                for (k, v) in kv {
                    m.insert(k, v);
                }
                Value::from(m)
            }),
        ]
    })
}

proptest! {
    #[test]
    fn printed_json_parses_back_to_the_same_value(v in value()) {
        let text = v.to_string();
        prop_assert_eq!(json::parse(&text).unwrap(), v);
    }

    #[test]
    fn printing_a_parsed_print_gives_the_same_text(v in value()) {
        let text = v.to_string();
        prop_assert_eq!(json::parse(&text).unwrap().to_string(), text);
    }

    #[test]
    fn no_text_makes_the_parser_panic(s in any::<String>(), b in prop::collection::vec(any::<u8>(), 0..64)) {
        let _ = json::parse(&s);
        let _ = json::parse_bytes(&b);
    }
}
