//! Transport-neutral conformance oracles. A driver supplies resource calls and
//! streams; record counts and imported history belong to its fixture builder.
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub type Bindings = BTreeMap<String, Value>;
pub trait Events {
    /// Return a validated producer frame. Dropping the reader aborts the follow.
    fn next_emitted(&self) -> Value;
    /// Encode fixture frames in the transport's wire format, then feed the
    /// exact decoder used by a live follow, before application type filtering.
    /// Future fields/types are client data; do not apply producer-only checks.
    fn decode_injected(&self, frames: &[Value]) -> Vec<Value>;
    /// Every consumed frame in every case also exercises the client decoder.
    fn next(&self) -> Value {
        let event = self.next_emitted();
        check_extensions(self, &event);
        event
    }
}
fn check_extensions(stream: &(impl Events + ?Sized), event: &Value) {
    let mut extended = event.clone();
    extended["future_field"] = json!({"state":"future_state"});
    if let Some(data) = extended["data"].as_object_mut() {
        data.insert("future_field".into(), true.into());
        if let Some(message) = data.get_mut("message").and_then(Value::as_object_mut) {
            message.insert("kind".into(), "future_kind".into());
            message.insert("body".into(), "future_body_state".into());
            message.insert("future_field".into(), true.into());
            if let Some(deliveries) = message.get_mut("deliveries").and_then(Value::as_array_mut) {
                for delivery in deliveries {
                    delivery["state"] = "future_delivery_state".into();
                }
            }
        }
        if let Some(changeset) = data.get_mut("changeset").and_then(Value::as_object_mut) {
            changeset.insert("state".into(), "future_state".into());
        }
    }
    let mut unknown = extended.clone();
    unknown["type"] = if event["type"] == "mdkg.future.changed" {
        "mdkg.future.extended"
    } else {
        "mdkg.future.changed"
    }
    .into();
    let parsed = stream.decode_injected(&[extended, unknown, event.clone()]);
    assert_eq!(
        parsed.len(),
        3,
        "the wire decoder must read all three frames"
    );
    // A client handles its known type, skips the future one, then continues.
    let known: Vec<_> = parsed
        .iter()
        .filter(|frame| frame["type"] == event["type"])
        .collect();
    assert_eq!(
        known.len(),
        2,
        "unknown event types must be skipped without losing the next frame"
    );
    for frame in known {
        for key in ["id", "cursor", "subjects"] {
            assert_eq!(
                frame[key], event[key],
                "known event identity after extension: {key}"
            );
        }
    }
}

pub trait Driver: Sized + Sync {
    type Stream: Events;
    fn new(spec: &Value) -> Self;
    /// Canonical resource operation. `cli` expectations are read by the CLI
    /// adapter only; normalize resource wrappers and remove legacy field names.
    fn invoke(&self, operation: &Value) -> Value;
    fn follow(&self, after: Option<&str>, filters: &Value) -> Self::Stream;
    /// Page tokens remain opaque: CLI uses its numeric position, HTTP its next.
    fn inbox(&self, limit: Option<u64>, page: Option<&Value>) -> Value;
    fn supports_aliases() -> bool {
        false
    }
    /// Fixture oracle, never a client operation or cursor parser.
    fn record_count(&self) -> usize;
    fn records(&self) -> Vec<(u64, String, String)>;
    fn store_path(&self) -> std::path::PathBuf;
    fn head(&self) -> String;
    /// A server driver can restart cached fixture handles after native setup.
    fn fixture_changed(&self) {}
    fn import_fixture(&self, history: &Value);
}

pub fn resolved(value: &Value, bindings: &Bindings) -> Value {
    match value {
        Value::String(name) if name.starts_with('$') => bindings
            .get(name)
            .unwrap_or_else(|| panic!("unbound {name}"))
            .clone(),
        Value::Array(values) => {
            Value::Array(values.iter().map(|v| resolved(v, bindings)).collect())
        }
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), resolved(v, bindings)))
                .collect(),
        ),
        other => other.clone(),
    }
}
pub fn expect_partial(expected: &Value, actual: &Value, bindings: &mut Bindings) {
    match expected {
        Value::String(name) if name.starts_with('$') => {
            if let Some(first) = bindings.get(name) {
                assert_eq!(actual, first, "binding {name}");
            } else {
                bindings.insert(name.clone(), actual.clone());
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                expect_partial(
                    value,
                    actual
                        .get(key)
                        .unwrap_or_else(|| panic!("missing {key} in {actual}")),
                    bindings,
                );
            }
        }
        Value::Array(values) => {
            let got = actual.as_array().expect("expected array");
            assert_eq!(values.len(), got.len(), "array shape");
            for (a, b) in values.iter().zip(got) {
                expect_partial(a, b, bindings);
            }
        }
        _ => assert_eq!(expected, actual),
    }
}
fn run_op<D: Driver>(driver: &D, op: &Value, bindings: &mut Bindings) -> Value {
    let mut request = op.clone();
    request["args"] = resolved(&op["args"], bindings);
    let result = driver.invoke(&request);
    expect_partial(&op["expect"], &result, bindings);
    result
}
fn operations<D: Driver>(case: &Value, driver: &D, bindings: &mut Bindings) {
    for operation in case["operations"].as_array().expect("operations") {
        run_op(driver, operation, bindings);
    }
}
fn all<D: Driver>(driver: &D) -> Vec<Value> {
    let stream = driver.follow(Some("start"), &json!({}));
    (0..driver.record_count()).map(|_| stream.next()).collect()
}
fn uris(messages: &Value) -> BTreeSet<String> {
    messages
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| m["uri"].as_str().expect("resource uri").to_string())
        .collect()
}

pub fn run<D: Driver>(case: &Value) {
    match case["scenario"].as_str().expect("scenario") {
        "concurrent_list_watch" => concurrent_list_watch::<D>(case),
        "cursor_errors" => cursor_errors::<D>(case),
        "ordering_history" => ordering_history::<D>(case),
        "resume" => resume::<D>(case),
        "idempotency" => idempotency::<D>(case),
        "aliases" => aliases::<D>(case),
        "forward_compatibility" => forward::<D>(case),
        "pages" => pages::<D>(case),
        "canonical_paths" => canonical::<D>(case),
        "imported_history" => imported::<D>(case),
        "reserved_kinds" => reserved::<D>(case),
        other => panic!("unknown shared scenario {other}"),
    }
}
fn resume<D: Driver>(case: &Value) {
    let driver = D::new(&case["fixture"]);
    operations(case, &driver, &mut Bindings::new());
    let expected = all(&driver);
    assert_eq!(
        expected.len() as u64,
        case["expect"]["records"].as_u64().expect("record count")
    );
    let split = case["expect"]["split_after"].as_u64().expect("split") as usize;
    let first = driver.follow(Some("start"), &json!({}));
    let mut merged: Vec<_> = (0..split).map(|_| first.next()).collect();
    drop(first);
    let last = merged.last().expect("prefix")["cursor"]
        .as_str()
        .expect("cursor");
    let resumed = driver.follow(Some(last), &json!({}));
    for _ in split..expected.len() {
        merged.push(resumed.next());
    }
    assert_eq!(
        merged, expected,
        "reconnecting must preserve the complete sequence"
    );
}
fn idempotency<D: Driver>(case: &Value) {
    let driver = D::new(&case["fixture"]);
    let mut bindings = Bindings::new();
    for operation in case["operations"].as_array().expect("operations") {
        let before = driver.record_count();
        run_op(&driver, operation, &mut bindings);
        assert_eq!(
            driver.record_count() - before,
            operation["expect_records"].as_u64().expect("record delta") as usize
        );
    }
    let events = all(&driver);
    let sent = events
        .iter()
        .filter(|e| e["type"] == case["expect"]["event_type"])
        .count();
    assert_eq!(
        sent as u64,
        case["expect"]["sent"].as_u64().expect("sent count")
    );
}
fn aliases<D: Driver>(case: &Value) {
    let driver = D::new(&case["fixture"]);
    let legacy = D::supports_aliases().then(|| D::new(&case["fixture"]));
    let mut canonical_bindings = Bindings::new();
    let mut legacy_bindings = Bindings::new();
    for operation in case["operations"].as_array().expect("operations") {
        let current = run_op(&driver, operation, &mut canonical_bindings);
        if let Some(legacy) = &legacy {
            let mut old = operation.clone();
            old["cli"] = operation["cli"]["alias"].clone();
            old["cli"]["legacy"] = true.into();
            let old = run_op(legacy, &old, &mut legacy_bindings);
            let path = operation["cli"]["compare"]
                .as_str()
                .expect("resource comparison path");
            assert_eq!(
                current.pointer(path).expect("canonical resource"),
                old.pointer(path).expect("alias resource")
            );
        }
    }
    all(&driver);
    if let Some(legacy) = &legacy {
        all(legacy);
    }
}
fn mutate(value: &mut Value, mutations: &Value) {
    for (pointer, replacement) in mutations.as_object().expect("mutations") {
        let (parent, field) = pointer.rsplit_once('/').expect("JSON pointer");
        let field = field.replace("~1", "/").replace("~0", "~");
        value
            .pointer_mut(parent)
            .expect("mutation parent")
            .as_object_mut()
            .expect("object mutation")
            .insert(field, replacement.clone());
    }
}
fn forward<D: Driver>(case: &Value) {
    let driver = D::new(&case["fixture"]);
    operations(case, &driver, &mut Bindings::new());
    let stream = driver.follow(Some("start"), &json!({}));
    let events: Vec<_> = (0..driver.record_count()).map(|_| stream.next()).collect();
    let event = events
        .iter()
        .find(|event| event["type"] == case["expect"]["known_type"])
        .expect("known event");
    // This sample client only handles its known type and fields. Future values
    // inside a known message remain data; unknown event types are skipped.
    let client = |event: &Value| {
        if event["type"] == case["expect"]["known_type"] {
            event
                .pointer(case["expect"]["id_pointer"].as_str().expect("id path"))
                .cloned()
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    };
    for injection in case["injections"].as_array().expect("injections") {
        let mut future = event.clone();
        mutate(&mut future, &injection["set"]);
        let parsed = stream.decode_injected(&[future, event.clone()]);
        assert_eq!(
            parsed.len(),
            2,
            "both injected and subsequent frames decode"
        );
        assert_eq!(client(&parsed[0]), injection["expect_client_id"]);
        assert_eq!(
            client(&parsed[1]),
            client(event),
            "client continues after a future frame"
        );
    }
}
fn pages<D: Driver>(case: &Value) {
    let driver = D::new(&case["fixture"]);
    let ops = case["operations"].as_array().expect("operations");
    let split = case["expect"]["before_first_page"]
        .as_u64()
        .expect("page boundary") as usize;
    let mut bindings = Bindings::new();
    for operation in &ops[..split] {
        run_op(&driver, operation, &mut bindings);
    }
    let limit = case["expect"]["page_limit"].as_u64().expect("page limit");
    let first = driver.inbox(Some(limit), None);
    let cursor = first["data"]["event_cursor"]
        .as_str()
        .expect("snapshot cursor")
        .to_string();
    let mut combined = uris(&first["data"]["messages"]);
    let boundary = driver.record_count();
    for operation in &ops[split..] {
        run_op(&driver, operation, &mut bindings);
    }
    let mut page = first;
    while !page["data"]["next"].is_null() {
        page = driver.inbox(Some(limit), Some(&page["data"]["next"]));
        combined.extend(uris(&page["data"]["messages"]));
    }
    let event_type = case["expect"]["event_type"].as_str().expect("event type");
    let late: Vec<_> = all(&driver)
        .into_iter()
        .skip(boundary)
        .filter(|event| event["type"] == event_type)
        .collect();
    assert_eq!(
        late.len() as u64,
        case["expect"]["late_events"].as_u64().expect("late events")
    );
    let stream = driver.follow(Some(&cursor), &json!({"types":[event_type]}));
    for expected in late {
        let event = stream.next();
        assert_eq!(
            event, expected,
            "post-snapshot event identity, order and payload"
        );
        combined.insert(
            event["data"]["message"]["uri"]
                .as_str()
                .expect("message uri")
                .to_string(),
        );
    }
    assert_eq!(
        combined,
        uris(&driver.inbox(None, None)["data"]["messages"])
    );
}
fn canonical<D: Driver>(case: &Value) {
    let driver = D::new(&case["fixture"]);
    let operation = &case["operations"][0];
    let response = run_op(&driver, operation, &mut Bindings::new());
    let event = all(&driver).pop().expect("sent event");
    assert_eq!(response["data"]["message"]["uri"], case["expect"]["uri"]);
    assert_eq!(
        event["data"]["message"]["uri"],
        response["data"]["message"]["uri"]
    );
    for subject in case["expect"]["subjects"].as_array().expect("subjects") {
        assert!(
            event["subjects"]
                .as_array()
                .expect("event subjects")
                .contains(subject)
        );
    }
}
fn imported<D: Driver>(case: &Value) {
    let driver = D::new(&case["fixture"]);
    driver.import_fixture(&case["fixture"]["history"]);
    let events = all(&driver);
    assert_eq!(
        events.len() as u64,
        case["expect"]["records"].as_u64().expect("record count")
    );
    expect_partial(
        &case["expect"]["last"],
        events.last().expect("last event"),
        &mut Bindings::new(),
    );
}

/// Only mdkg sends `receipt` and `fault`, and only its own senders take a
/// `fault.` id (a-flow A26, F2): each refusal is one envelope and records
/// nothing, so the stream's only send is the ordinary one.
fn reserved<D: Driver>(case: &Value) {
    let driver = D::new(&case["fixture"]);
    operations(case, &driver, &mut Bindings::new());
    let events = all(&driver);
    let sent: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "mdkg.message.sent")
        .collect();
    assert_eq!(sent.len(), 1, "{events:?}");
    expect_partial(&case["expect"]["sent"], sent[0], &mut Bindings::new());
    for refused in case["expect"]["refused_ids"]
        .as_array()
        .expect("refused ids")
    {
        let id = refused.as_str().expect("refused id");
        assert!(
            events.iter().all(|e| !e.to_string().contains(id)),
            "{id} left a record"
        );
    }
}

/// SQLite fixture builder shared by the CLI and HTTP drivers. No cursor is
/// constructed here; imported rows deliberately collide with journal numbers.
pub fn seed_import_history(path: &std::path::Path, history: &Value) {
    let mut store =
        mdkg_coord_sqlite::Store::open_with(path, 60_000, mdkg_coord_sqlite::Upgrade::Refuse)
            .expect("fixture store");
    store
        .execute(&mdkg_coord::Command::Claim {
            owner: history["claim"]["owner"].as_str().expect("owner").into(),
            scope: mdkg_coord::Scope::new(history["claim"]["scope"].as_str().expect("scope")),
            mode: mdkg_coord::Mode::Exclusive,
            now: 1000,
            request: None,
            node: None,
        })
        .expect("fixture journal seed");
    let record = &history["import"]["record"];
    let imported = mdkg_coord_sqlite::log::Imported {
        ts_ms: record["ts_ms"].as_u64().expect("record time"),
        actor: record["actor"].as_str().expect("record actor").into(),
        kind: record["kind"].as_str().expect("record kind").into(),
        subject: record["subject"]
            .as_array()
            .expect("subjects")
            .iter()
            .map(|v| v.as_str().expect("subject").to_string())
            .collect(),
        body: record["body"].as_object().expect("record body").clone(),
    };
    store
        .import_records(
            history["import"]["source"].as_str().expect("import source"),
            &[imported],
        )
        .expect("fixture import");
}

fn concurrent_list_watch<D: Driver>(case: &Value) {
    for seed in 1..=case["expect"]["seeds"].as_u64().expect("fixture count") {
        let fixture = D::new(&case["fixture"]);
        let mut bindings = Bindings::new();
        run_op(&fixture, &case["operations"][0], &mut bindings);
        let operations = &case["operations"].as_array().expect("fixture array")[1..];
        let prefix = (seed as usize * 7) % operations.len();
        for operation in &operations[..prefix] {
            run_op(&fixture, operation, &mut bindings);
        }
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let ops = &operations[prefix..];
            let fixture = &fixture;
            let barrier = &barrier;
            let writer = scope.spawn(move || {
                let mut state = seed;
                let mut bindings = Bindings::new();
                barrier.wait();
                for op in ops {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    for _ in 0..state % 13 {
                        std::thread::yield_now();
                    }
                    run_op(fixture, op, &mut bindings);
                }
            });
            barrier.wait();
            for _ in 0..seed % 11 {
                std::thread::yield_now();
            }
            let list = fixture.inbox(None, None);
            let cursor = list["data"]["event_cursor"]
                .as_str()
                .expect("fixture string")
                .to_string();
            let stream = fixture.follow(
                Some(&cursor),
                &json!({"types":["mdkg.message.sent"],"mailbox":"b"}),
            );
            writer.join().expect("fixture value");
            let fresh = fixture.inbox(None, None);
            let listed = ids(&list["data"]["messages"]);
            let expected = ids(&fresh["data"]["messages"]);
            let remaining = expected.len() - listed.len();
            let mut combined = listed.clone();
            for _ in 0..remaining {
                let event = stream.next();
                assert_eq!(event["type"], "mdkg.message.sent");
                let id = event["data"]["message"]["messageId"]
                    .as_str()
                    .expect("fixture value")
                    .to_string();
                assert!(combined.insert(id), "list/watch overlap in seed {seed}");
            }
            assert_eq!(combined, expected, "list/watch gap in seed {seed}");
        });
        all(&fixture);
    }
}

fn ordering_history<D: Driver>(case: &Value) {
    let fixture = D::new(&case["fixture"]);
    let mut bindings = Bindings::new();
    for op in case["operations"].as_array().expect("fixture array") {
        run_op(&fixture, op, &mut bindings);
    }
    let events = all(&fixture);
    let records = fixture.records();
    assert_eq!(events.len(), records.len());
    for (event, (seq, hash, _)) in events.iter().zip(&records) {
        assert_eq!(event["id"], *hash, "event order at log sequence {seq}");
    }
    let mut from = 0;
    for expected in case["expect"]["events"].as_array().expect("fixture array") {
        let (offset, event) = events[from..]
            .iter()
            .enumerate()
            .find(|(_, event)| {
                event["type"] == expected["type"]
                    && expected["data"]
                        .get("kind")
                        .is_none_or(|kind| event["data"]["kind"] == *kind)
            })
            .unwrap_or_else(|| panic!("missing {expected}: {events:?}"));
        expect_partial(expected, event, &mut bindings);
        from += offset + 1;
    }
    assert_eq!(events, all(&fixture), "replay must be byte-for-byte stable");
}

fn cursor_errors<D: Driver>(case: &Value) {
    let fixture = D::new(&case["fixture"]);
    let mut bindings = Bindings::new();
    run_op(&fixture, &case["operations"][0], &mut bindings);
    let older = mdkg_coord_sqlite::snapshot(&fixture.store_path(), "older").expect("fixture value");
    run_op(&fixture, &case["operations"][1], &mut bindings);
    let original = fixture.head();
    let original_seq = fixture.records().last().expect("fixture value").0;
    all(&fixture);
    cursor_refusal_shared(&fixture, "not-a-cursor", case, "bad_cursor");
    let other = D::new(&case["fixture"]);
    cursor_refusal_shared(&fixture, &other.head(), case, "foreign_store");
    // Give a genuinely distinct fixture a valid genesis whose full id shares
    // the target's first eight digits. No invalid or mismatched metadata.
    let source = rusqlite::Connection::open(fixture.store_path()).expect("fixture value");
    let target_id: String = source
        .query_row("SELECT value FROM meta WHERE key='store_id'", [], |r| {
            r.get(0)
        })
        .expect("fixture value");
    drop(source);
    let tail = if target_id[8..] == "a".repeat(24) {
        "b".repeat(24)
    } else {
        "a".repeat(24)
    };
    let store_id = format!("{}{tail}", &target_id[..8]);
    let connection = rusqlite::Connection::open(other.store_path()).expect("fixture value");
    let text: String = connection
        .query_row("SELECT record FROM log WHERE seq=1", [], |r| r.get(0))
        .expect("fixture value");
    let (mut genesis, _) = mdkg_log::record::Record::parse(&text).expect("fixture value");
    genesis.body.insert("store".into(), store_id.clone().into());
    genesis.subject = vec![format!("store:{store_id}")];
    let (text, hash) = genesis.seal().expect("fixture value");
    connection
        .execute(
            "UPDATE log SET hash=?1,record=?2 WHERE seq=1",
            rusqlite::params![hash, text],
        )
        .expect("fixture value");
    connection
        .execute("UPDATE meta SET value=?1 WHERE key='store_id'", [store_id])
        .expect("fixture value");
    connection
        .execute(
            "UPDATE meta SET value=?1 WHERE key='log_head'",
            [format!("1 {hash}")],
        )
        .expect("fixture value");
    drop(connection);
    other.fixture_changed();
    cursor_refusal_shared(&fixture, &other.head(), case, "foreign_same_prefix");
    let same =
        mdkg_coord_sqlite::snapshot(&fixture.store_path(), "same-anchor").expect("fixture value");
    mdkg_coord_sqlite::restore(&fixture.store_path(), &same).expect("fixture value");
    fixture.fixture_changed();
    cursor_refusal_shared(&fixture, &original, case, "restored");
    std::fs::copy(&older, fixture.store_path()).expect("fixture value");
    fixture.fixture_changed();
    cursor_refusal_shared(&fixture, &original, case, "manual_copy_over");
    let mut fork = case["operations"][1].clone();
    fork["args"]["id"] = "forked-message".into();
    run_op(&fixture, &fork, &mut bindings);
    assert_eq!(
        fixture.records().last().expect("fixture value").0,
        original_seq
    );
    cursor_refusal_shared(&fixture, &original, case, "forked_anchor");
}

fn ids(messages: &Value) -> BTreeSet<String> {
    messages
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| m["messageId"].as_str().expect("message id").to_string())
        .collect()
}
fn cursor_refusal_shared<D: Driver>(fixture: &D, cursor: &str, case: &Value, scenario: &str) {
    let expected = case["expect"]["refusals"]
        .as_array()
        .expect("refusals")
        .iter()
        .find(|v| v["case"] == scenario)
        .expect("scenario");
    let request = json!({"op":"events.watch","actor":"a","args":{"after":cursor},"expect":{"ok":false,"diagnostics":[{"code":expected["code"]}],"data":{"head":fixture.head()}},"cli":{"exit":expected["cli"]["exit"],"envelope":{"schema":case["cli"]["schema"]}},"http":expected["http"]});
    run_op(fixture, &request, &mut Bindings::new());
}

/// One transport of the envelope cases (`same_envelope`): a registry
/// operation, named as `mdkg_ops` names it, with its arguments by their
/// HTTP names, run as `actor` on the transport's own copy of the fixture.
pub trait Transport {
    /// The case's name for it: `cli-local`, `mcp` or `http-socket`.
    fn name(&self) -> &'static str;
    /// The envelope, with the fixture's own folder written `<fixture>`,
    /// and the transport's outcome: the CLI's exit code, MCP's `isError`,
    /// or the HTTP status.
    fn call(&self, op: &str, actor: &str, args: &Value) -> (Value, Value);
}

/// The same operations give the same envelope on every transport the case
/// names (dec-01a18dd9: serve and MCP answer what the CLI prints), and
/// each transport its own outcome for it.
pub fn same_envelope(case: &Value, transports: &[&dyn Transport]) {
    let names: Vec<&str> = transports.iter().map(|t| t.name()).collect();
    let named: Vec<&str> = case["transports"]
        .as_array()
        .expect("transports")
        .iter()
        .map(|t| t.as_str().expect("transport name"))
        .collect();
    assert_eq!(names, named, "the case's transports, in its order");
    let mut first: Option<(&str, Vec<Value>)> = None;
    for transport in transports {
        let mut bindings = Bindings::new();
        let mut envelopes = Vec::new();
        for operation in case["operations"].as_array().expect("operations") {
            let op = operation["op"].as_str().expect("op");
            let actor = operation["actor"].as_str().expect("actor");
            let args = resolved(&operation["args"], &bindings);
            let (envelope, outcome) = transport.call(op, actor, &args);
            let at = format!("{} {op}: {envelope}", transport.name());
            assert_eq!(outcome, operation["outcomes"][transport.name()], "{at}");
            expect_partial(&operation["expect"], &envelope, &mut bindings);
            // A value each copy of the fixture has its own of, such as a
            // cursor that names its store, is compared by presence only.
            let mut envelope = envelope;
            for pointer in operation["per_copy"].as_array().into_iter().flatten() {
                let pointer = pointer.as_str().expect("JSON pointer");
                let value = envelope
                    .pointer_mut(pointer)
                    .unwrap_or_else(|| panic!("{at}: no {pointer}"));
                *value = "<per copy>".into();
            }
            envelopes.push(envelope);
        }
        match &first {
            None => first = Some((transport.name(), envelopes)),
            Some((name, expected)) => {
                for ((operation, want), got) in case["operations"]
                    .as_array()
                    .expect("operations")
                    .iter()
                    .zip(expected)
                    .zip(&envelopes)
                {
                    assert_eq!(
                        got,
                        want,
                        "{} {}: the envelope {name} gave",
                        transport.name(),
                        operation["op"]
                    );
                }
            }
        }
    }
}
