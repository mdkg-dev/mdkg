//! Property: writing any valid frontmatter and parsing it back is lossless,
//! including keys no template knows (edd-1 invariant 20).

use mdkg_format::frontmatter::{self, Frontmatter, Value};
use proptest::prelude::*;

fn key() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,12}"
}

fn scalar() -> impl Strategy<Value = String> {
    // Non-empty, no surrounding whitespace, no line breaks; not a list or a
    // boolean literal (those would parse as other kinds).
    "[A-Za-z0-9]([A-Za-z0-9 :/._#\"'()-]{0,30}[A-Za-z0-9.])?"
        .prop_filter("not a boolean literal", |s| s != "true" && s != "false")
}

fn item() -> impl Strategy<Value = String> {
    "[a-z0-9]([a-z0-9 ._:/-]{0,12}[a-z0-9])?"
}

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        scalar().prop_map(Value::String),
        any::<bool>().prop_map(Value::Bool),
        prop::collection::vec(item(), 0..5).prop_map(Value::List),
    ]
}

fn body() -> impl Strategy<Value = String> {
    // Any body without carriage returns (CRLF is normalized on parse).
    "[^\r]{0,200}"
}

proptest! {
    #[test]
    fn write_then_parse_is_lossless(
        entries in prop::collection::vec((key(), value()), 0..12),
        body in body(),
    ) {
        let mut fm = Frontmatter::default();
        for (k, v) in entries {
            fm.insert(k, v);
        }
        let text = frontmatter::write(&fm, &body);
        let parsed = frontmatter::parse(&text, "p.md").expect("written frontmatter parses");
        prop_assert_eq!(&parsed.body, &body);
        prop_assert_eq!(parsed.frontmatter.entries().len(), fm.entries().len());
        for (k, v) in fm.entries() {
            prop_assert_eq!(parsed.frontmatter.get(k), Some(v));
        }
        // Writing again is stable.
        let again = frontmatter::write(&parsed.frontmatter, &parsed.body);
        prop_assert_eq!(again, text);
    }
}
