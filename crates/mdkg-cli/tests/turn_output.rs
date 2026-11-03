//! `mdkg.api.turn_output.v1` types `data` per `type`: every event the
//! launcher's journal writer and each harness's live parser emit conforms,
//! fields declared, and data that breaks its type doesn't. `agent.state`
//! and a gone turn's `turn.ended` come from `agents logs --follow`, whose
//! frames tests/agents.rs checks against the same schema.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use mdkg_run::Harness;
use mdkg_run::turn::journal;
use mdkg_run::turn::live::{Parser, Stream};
use serde_json::{Value, json};

fn schema() -> Value {
    common::api_schema::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api/mdkg.api.turn_output.v1.json"),
    )
}

fn errors(schema: &Value, event: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    common::api_schema::check_emitted(schema, schema, event, "$", &mut errors);
    errors
}

/// Conformance case 09's native output, for each harness, through the
/// live parser into a real journal, read back.
#[test]
fn every_type_a_journal_holds_conforms() {
    let schema = schema();
    let case: Value = serde_json::from_slice(
        &std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../conformance/api-v1/cases/09-turn-output.json"),
        )
        .expect("case 09"),
    )
    .expect("case JSON");
    let dir = common::temp::dir("turn-output");
    let mut seen = BTreeSet::new();
    for (n, harness) in Harness::ALL.into_iter().enumerate() {
        let id = mdkg_format::Uuid7::now().to_string();
        let mut w = journal::Writer::create(&dir, "cx", &id, n as u64 + 1).expect("journal");
        w.harness_starting(harness).expect("harness.starting");
        let mut parser = Parser::new(harness);
        for line in case["harnesses"][harness.as_str()]["native"]
            .as_array()
            .expect("native lines")
        {
            let stream = match line["stream"].as_str() {
                Some("stdout") => Stream::Stdout,
                _ => Stream::Stderr,
            };
            for (kind, data) in parser.line(stream, line["line"].as_str().expect("line")) {
                w.write(kind, data, false).expect("event");
            }
        }
        w.ended("replied").expect("turn.ended");
        let events = journal::read(&dir, "cx", &id)
            .expect("read")
            .expect("journal");
        for event in events {
            let value = serde_json::to_value(&event).expect("event JSON");
            let found = errors(&schema, &value);
            assert!(found.is_empty(), "{harness:?} {value}: {found:?}");
            seen.insert(event.kind);
        }
    }
    for kind in [
        "turn.started",
        "harness.starting",
        "text",
        "status",
        "error",
        "tool.started",
        "tool.ended",
        "usage",
        "native",
        "turn.ended",
    ] {
        assert!(seen.contains(kind), "no {kind} in {seen:?}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn data_that_breaks_its_type_does_not_conform() {
    let schema = schema();
    let event = |kind: &str, data: Value| {
        json!({
            "schema": "mdkg.api.turn_output.v1",
            "turn": "/v1/agents/cx/turns/1",
            "seq": 3,
            "time": "2026-10-30T00:00:00.000Z",
            "type": kind,
            "data": data,
        })
    };
    let usage = json!({"input_tokens": 1, "output_tokens": null, "cache_read_tokens": null,
        "total_tokens": null, "cost_usd": -0.5});
    for (kind, data) in [
        ("text", json!({"text": "hi"})),
        (
            "tool.started",
            json!({"class": "edit", "name": "Edit", "summary": ""}),
        ),
        (
            "tool.ended",
            json!({"class": "a-new-class", "name": "", "ok": false}),
        ),
        ("usage", usage.clone()),
        ("turn.started", json!({"turn_id": "t", "number": 1})),
        (
            "turn.ended",
            json!({"outcome": "replied", "output_gone": true}),
        ),
        // `agents logs` can't read every ended turn's outcome.
        ("turn.ended", json!({"outcome": null})),
        ("harness.starting", json!({"harness": "a-new-harness"})),
        ("agent.state", json!({"state": "running"})),
        (
            "native",
            json!({"harness": "claude", "event": [1, {"any": "shape"}]}),
        ),
        // An open set of types: one not listed carries any object.
        ("a.new.type", json!({"anything": true})),
    ] {
        let found = errors(&schema, &event(kind, data.clone()));
        assert!(found.is_empty(), "{kind} {data}: {found:?}");
    }
    let mut no_cost = usage;
    no_cost.as_object_mut().expect("usage").remove("cost_usd");
    for (kind, data) in [
        ("text", json!({})),
        ("status", json!({"text": 3})),
        ("error", json!({"text": "x", "extra": 1})),
        ("tool.started", json!({"class": "edit", "name": "Edit"})),
        (
            "tool.ended",
            json!({"class": "edit", "name": "Edit", "ok": "yes"}),
        ),
        ("usage", no_cost),
        ("turn.started", json!({"turn_id": "t", "number": -1})),
        (
            "turn.ended",
            json!({"outcome": "replied", "output_gone": false}),
        ),
        ("harness.starting", json!({})),
        ("agent.state", json!({"state": null})),
        ("native", json!({"event": {}})),
    ] {
        let found = errors(&schema, &event(kind, data.clone()));
        assert!(!found.is_empty(), "{kind} {data} conformed");
    }
}
