//! `atlas::protobuf`: the reader gives back what the writer wrote, in order,
//! and no byte string makes it panic.
use atlas::protobuf::{Reader, WireType, Writer};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Field {
    Int(u32, i64),
    Str(u32, String),
    Bytes(u32, Vec<u8>),
}

fn field() -> impl Strategy<Value = Field> {
    let n = 1u32..(1 << 29);
    prop_oneof![
        (n.clone(), any::<i64>()).prop_map(|(f, v)| Field::Int(f, v)),
        (n.clone(), any::<String>()).prop_map(|(f, s)| Field::Str(f, s)),
        (n, prop::collection::vec(any::<u8>(), 0..32)).prop_map(|(f, b)| Field::Bytes(f, b)),
    ]
}

proptest! {
    #[test]
    fn written_fields_read_back_in_order(fields in prop::collection::vec(field(), 0..12)) {
        let mut w = Writer::default();
        for f in &fields {
            match f {
                Field::Int(n, v) => { w.int(*n, *v); }
                Field::Str(n, s) => { w.string(*n, s); }
                Field::Bytes(n, b) => { w.bytes(*n, b); }
            }
        }
        let mut r = Reader::new(&w.buf);
        for f in &fields {
            let (n, wt) = r.field().unwrap().expect("a field");
            match f {
                Field::Int(m, v) => {
                    prop_assert!(n == *m && matches!(wt, WireType::Varint));
                    prop_assert_eq!(r.varint().unwrap(), *v as u64);
                }
                Field::Str(m, s) => {
                    prop_assert!(n == *m && matches!(wt, WireType::Len));
                    prop_assert_eq!(r.string().unwrap(), s.as_str());
                }
                Field::Bytes(m, b) => {
                    prop_assert!(n == *m && matches!(wt, WireType::Len));
                    prop_assert_eq!(r.bytes().unwrap(), b.as_slice());
                }
            }
        }
        prop_assert!(r.field().unwrap().is_none());
    }

    #[test]
    fn no_bytes_make_the_reader_panic(b in prop::collection::vec(any::<u8>(), 0..64)) {
        let mut r = Reader::new(&b);
        for _ in 0..64 {
            match r.field() {
                Ok(Some((_, wt))) => if r.skip(wt).is_err() { break },
                _ => break,
            }
        }
    }
}
