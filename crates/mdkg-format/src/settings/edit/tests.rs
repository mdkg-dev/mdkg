use super::*;
use serde_json::json;

fn source(layer: Layer) -> Input {
    Input::new(
        Origin::value(layer),
        json!({"unknown":{"keep":true},"work":{"committed":"definitions","priority_max":9}}),
    )
    .expect("source")
}

#[test]
fn values_are_typed_and_malformed_json_is_not_a_string_fallback() {
    let origin = Origin::value(Layer::User);
    for (text, expected) in [
        ("local", json!("local")),
        ("true", json!(true)),
        ("30", json!(30)),
        ("null", Value::Null),
        ("\"123abc\"", json!("123abc")),
        ("[1,2]", json!([1, 2])),
        ("", json!("")),
    ] {
        assert_eq!(value(&origin, text).expect("value"), expected);
    }
    for text in ["{bad", "[", "\"unterminated", "1e400", "{\"a\":1,\"a\":2}"] {
        assert!(
            !value(&origin, text)
                .expect_err("JSON refused")
                .to_string()
                .contains(text)
        );
    }
    assert!(value(&origin, &"x".repeat(MAX_BYTES as usize + 1)).is_err());
}

#[test]
fn dotted_keys_and_json_pointers_preserve_unrelated_fields() {
    let original = source(Layer::Project);
    let updated = set(&original, "work.priority_min", json!(2)).expect("edit");
    assert_eq!(updated.value["work"]["priority_min"], 2);
    assert_eq!(updated.value["work"]["committed"], "definitions");
    assert_eq!(updated.value["unknown"], original.value["unknown"]);
    assert!(original.value["work"].get("priority_min").is_none());
    let escaped = set(&updated, "/literal~1name~0/has.dot", json!([1, 2])).expect("pointer");
    assert_eq!(escaped.value["literal/name~"]["has.dot"], json!([1, 2]));
    assert!(
        set(&escaped, "/literal~1name~0/has.dot/0", json!(3)).is_err(),
        "array values are replaced whole"
    );
    for key in ["", ".", "work..a", "/bad~2", "bad\0key"] {
        assert!(set(&original, key, json!(0)).is_err());
    }
}

#[test]
fn storage_choice_edits_update_the_alias_without_rewriting_other_keys() {
    for (key, value, expected) in [
        ("work.location", "local", "local"),
        ("work.committed", "none", "local"),
        ("work.committed", "definitions", "git"),
    ] {
        let changed = set(&source(Layer::Project), key, json!(value)).expect("storage edit");
        assert_eq!(changed.value["work"]["location"], expected);
        assert!(changed.value["work"].get("committed").is_none());
        assert_eq!(changed.value["work"]["priority_max"], 9);
    }
}

#[test]
fn identity_and_nonfile_layers_cannot_be_changed_by_set() {
    for layer in [Layer::Project, Layer::User, Layer::Local] {
        for key in [
            "graph_id",
            "schema_version",
            "format_version",
            "/graph_id/child",
        ] {
            let error =
                set(&source(layer), key, json!("VALUE_SENTINEL")).expect_err("identity protected");
            assert!(!error.to_string().contains("VALUE_SENTINEL"));
        }
    }
    for layer in [Layer::BuiltIn, Layer::Flags] {
        assert!(set(&source(layer), "tool", json!("other")).is_err());
    }
    let array = Input::new(Origin::value(Layer::User), json!({"work":[]})).expect("array key");
    assert!(set(&array, "work.child", json!(1)).is_err());
    let key = vec!["child"; MAX_DEPTH + 1].join(".");
    assert!(set(&source(Layer::Project), &key, json!(1)).is_err());
}
