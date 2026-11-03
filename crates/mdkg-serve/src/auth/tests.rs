use std::collections::HashMap;
use std::sync::Mutex;

use mdkg_ops::wire::MachineKey;

use super::*;
use crate::boot::test_clocks::ManualClocks;

pub(crate) const T0: i64 = 1_793_290_515_000;
pub(crate) const GRAPH: &str = "graph-1";

/// A stand-in for Ed25519: the "signature" is two SHA-256s over the key
/// and the base. It checks the pipeline, not the curve.
pub(crate) fn fake_sign(public_key: &[u8; 32], base: &[u8]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for (i, half) in out.chunks_mut(32).enumerate() {
        let mut h = Sha256::new();
        h.update([i as u8]);
        h.update(public_key);
        h.update(base);
        half.copy_from_slice(&h.finalize());
    }
    out
}

pub(crate) struct FakeVerifier;

impl Verifier for FakeVerifier {
    fn verify(&self, public_key: &[u8; 32], base: &[u8], signature: &[u8; 64]) -> bool {
        &fake_sign(public_key, base) == signature
    }
}

/// A person key: its human, public key, and whether it's revoked.
pub(crate) type PersonRow = (String, [u8; 32], bool);

#[derive(Default)]
pub(crate) struct FakePersons(pub Mutex<HashMap<String, PersonRow>>);

impl PersonVerifier for FakePersons {
    fn verify(&self, key_id: &str, alg: &str, base: &[u8], signature: &[u8]) -> PersonCheck {
        let keys = self.0.lock().expect("lock");
        let Some((human, pk, revoked)) = keys.get(key_id) else {
            return PersonCheck::Unknown;
        };
        if *revoked {
            return PersonCheck::Unknown;
        }
        if alg != "ed25519" || signature != fake_sign(pk, base).as_slice() {
            return PersonCheck::Invalid;
        }
        PersonCheck::Verified(PersonKey {
            key_id: key_id.into(),
            human: human.clone(),
            machine_grade: true,
        })
    }
}

#[derive(Default)]
pub(crate) struct FakeKeys(pub Mutex<HashMap<String, MachineKey>>);

impl KeyDirectory for FakeKeys {
    fn machine(&self, machine: &str) -> Option<MachineKey> {
        self.0.lock().expect("lock").get(machine).cloned()
    }

    fn person(&self, _key_id: &str) -> Option<mdkg_ops::wire::PersonKey> {
        None
    }
}

pub(crate) fn key_of(machine: &str) -> [u8; 32] {
    Sha256::digest(machine.as_bytes()).into()
}

pub(crate) struct Fixture {
    pub gate: Arc<Gate>,
    pub clocks: Arc<ManualClocks>,
    pub keys: Arc<FakeKeys>,
    pub persons: Arc<FakePersons>,
}

pub(crate) fn fixture() -> Fixture {
    let clocks = Arc::new(ManualClocks::at(T0));
    let keys = Arc::new(FakeKeys::default());
    let persons = Arc::new(FakePersons::default());
    for (m, state) in [
        ("m-ok", MachineState::Approved),
        ("m-revoked", MachineState::Revoked),
        ("m-pending", MachineState::Pending),
    ] {
        keys.0.lock().expect("lock").insert(
            m.into(),
            MachineKey {
                machine: m.into(),
                public_key: key_of(m),
                state,
            },
        );
    }
    persons.0.lock().expect("lock").insert(
        "pk-ann".into(),
        ("human:ann".into(), key_of("pk-ann"), false),
    );
    let boot = Boot::start(&*clocks).expect("boot");
    let gate = Gate::new(
        GRAPH.into(),
        boot,
        clocks.clone(),
        NonceCache::default(),
        keys.clone(),
        Arc::new(FakeVerifier),
        persons.clone(),
        vec!["https://allowed.example".into()],
        // The fixture's approved machine is the host, which has no rate.
        Arc::new(crate::limits::RateLimiter::new(
            crate::limits::Rate::default(),
            "m-ok".into(),
        )),
    );
    Fixture {
        gate: Arc::new(gate),
        clocks,
        keys,
        persons,
    }
}

/// A request a client builds, then signs.
#[derive(Clone)]
pub(crate) struct Req {
    pub method: &'static str,
    pub path: String,
    pub query: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub keyid: String,
    pub sign_with: [u8; 32],
    pub created: i64,
    pub nonce: String,
    /// Components to cover; `None` covers what the profile requires.
    pub cover: Option<Vec<&'static str>>,
}

impl Req {
    pub fn get(path: &str, boot: &str) -> Self {
        Self {
            method: "GET",
            path: path.into(),
            query: None,
            headers: vec![
                ("mdkg-graph".into(), GRAPH.into()),
                ("mdkg-boot".into(), boot.into()),
            ],
            body: Vec::new(),
            keyid: "m-ok".into(),
            sign_with: key_of("m-ok"),
            created: T0 / 1000,
            nonce: b64::encode_url(&crate::random::bytes::<16>().expect("urandom")),
            cover: None,
        }
    }

    pub fn post(path: &str, boot: &str, body: &[u8]) -> Self {
        let mut r = Self::get(path, boot);
        r.method = "POST";
        r.body = body.to_vec();
        r.headers.push(("mdkg-actor".into(), "agent-a".into()));
        r.headers.push((
            "idempotency-key".into(),
            "01a188cc-0000-7000-8000-000000000001".into(),
        ));
        if !body.is_empty() {
            r.headers.push((
                "content-digest".into(),
                format!("sha-256=:{}:", b64::encode(&Sha256::digest(body))),
            ));
            r.headers
                .push(("content-length".into(), body.len().to_string()));
        }
        r
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.retain(|(n, _)| n != name);
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn without(mut self, name: &str) -> Self {
        self.headers.retain(|(n, _)| n != name);
        self
    }

    fn unsigned_head(&self) -> RequestHead {
        RequestHead {
            method: self.method.into(),
            path: self.path.clone(),
            query: self.query.clone(),
            headers: self.headers.clone(),
        }
    }

    fn params(&self) -> Member {
        let comps: Vec<&str> = match &self.cover {
            Some(c) => c.clone(),
            None => {
                let head = self.unsigned_head();
                ALWAYS
                    .iter()
                    .copied()
                    .chain(IF_PRESENT.iter().copied().filter(|c| head.has(c)))
                    .collect()
            }
        };
        Member::InnerList(
            comps
                .iter()
                .map(|c| Entry {
                    item: Item::String((*c).into()),
                    params: vec![],
                })
                .collect(),
            vec![
                ("created".into(), Item::Integer(self.created)),
                ("keyid".into(), Item::String(self.keyid.clone())),
                ("nonce".into(), Item::String(self.nonce.clone())),
                ("alg".into(), Item::String("ed25519".into())),
                ("tag".into(), Item::String("mdkg-v1".into())),
            ],
        )
    }

    /// Signs as the machine: the head with its signature headers.
    pub fn signed(&self) -> RequestHead {
        let mut head = self.unsigned_head();
        let params = self.params();
        let comps: Vec<String> = match &params {
            Member::InnerList(l, _) => l
                .iter()
                .map(|e| match &e.item {
                    Item::String(s) => s.clone(),
                    _ => unreachable!(),
                })
                .collect(),
            Member::Item(_) => unreachable!(),
        };
        let base = machine_base(&head, &comps, &params);
        let sig = fake_sign(&self.sign_with, base.as_bytes());
        head.headers.push((
            "signature-input".into(),
            format!("mdkg={}", sf::serialize_member(&params)),
        ));
        head.headers
            .push(("signature".into(), format!("mdkg=:{}:", b64::encode(&sig))));
        head
    }

    /// Signs as the machine, then as the person key `key_id`.
    pub fn person_signed(&self, key_id: &str, person_key: [u8; 32]) -> RequestHead {
        let mut head = self.clone().header(PERSON_HEADER, key_id).signed();
        let input = head
            .one("signature-input")
            .ok()
            .flatten()
            .expect("input")
            .to_owned();
        let sig = head
            .one("signature")
            .ok()
            .flatten()
            .expect("sig")
            .to_owned();
        let pmember = Member::InnerList(
            ["signature", "signature-input"]
                .iter()
                .map(|n| Entry {
                    item: Item::String((*n).into()),
                    params: vec![("key".into(), Item::String("mdkg".into()))],
                })
                .collect(),
            vec![
                ("created".into(), Item::Integer(self.created)),
                ("keyid".into(), Item::String(key_id.into())),
                ("alg".into(), Item::String("ed25519".into())),
                ("tag".into(), Item::String("mdkg-v1".into())),
            ],
        );
        let base = format!(
            "\"signature\";key=\"mdkg\": {}\n\"signature-input\";key=\"mdkg\": {}\n\"@signature-params\": {}",
            sig.trim_start_matches("mdkg="),
            input.trim_start_matches("mdkg="),
            sf::serialize_member(&pmember)
        );
        let psig = fake_sign(&person_key, base.as_bytes());
        head.headers
            .retain(|(n, _)| n != "signature-input" && n != "signature");
        head.headers.push((
            "signature-input".into(),
            format!("{input}, mdkg-person={}", sf::serialize_member(&pmember)),
        ));
        head.headers.push((
            "signature".into(),
            format!("{sig}, mdkg-person=:{}:", b64::encode(&psig)),
        ));
        head
    }
}

fn boot(f: &Fixture) -> String {
    f.gate.boot_id().expect("boot")
}

fn reason(r: Result<Authenticated, Refusal>) -> (Code, &'static str) {
    let e = r.expect_err("refused");
    (e.code, e.test_reason())
}

#[test]
fn a_signed_read_passes_and_names_its_machine() {
    let f = fixture();
    let req = Req::get("/v1/leases", &boot(&f));
    let mut auth = f.gate.authenticate(&req.signed()).expect("ok");
    assert_eq!(auth.machine(), "m-ok");
    assert!(!auth.is_write());
    assert_eq!(auth.person(), None);
    f.gate.check_body(&mut auth, b"").expect("no body");
}

#[test]
fn a_signed_write_with_its_body_passes() {
    let f = fixture();
    let body = br#"{"scope":"a.rs"}"#;
    let req = Req::post("/v1/leases", &boot(&f), body).header("mdkg-turn", "turn-1");
    let mut auth = f.gate.authenticate(&req.signed()).expect("ok");
    assert!(auth.is_write());
    assert_eq!(auth.actor(), Some("agent-a"));
    assert_eq!(auth.turn(), Some("turn-1"));
    assert_eq!(
        auth.idempotency_key(),
        Some("01a188cc-0000-7000-8000-000000000001")
    );
    f.gate.check_body(&mut auth, body).expect("digest");
    assert_eq!(
        f.gate
            .check_body(&mut auth, b"{}")
            .expect_err("changed")
            .test_reason(),
        "digest_mismatch"
    );
}

#[test]
fn every_tampered_component_fails_verification() {
    let f = fixture();
    let b = boot(&f);
    let body = br#"{"scope":"a.rs"}"#;
    let base = Req::post("/v1/leases", &b, body);
    let mut q = base.clone();
    q.query = Some("after=5".into());
    let signed = q.signed();
    for tamper in [
        |h: &mut RequestHead| h.method = "PUT".into(),
        |h: &mut RequestHead| h.path = "/v1/leases/lease-1:release".into(),
        |h: &mut RequestHead| h.query = Some("after=1".into()),
        |h: &mut RequestHead| h.query = None,
        |h: &mut RequestHead| {
            for (n, v) in h.headers.iter_mut() {
                if n == "mdkg-actor" {
                    *v = "agent-b".into();
                }
            }
        },
        |h: &mut RequestHead| {
            for (n, v) in h.headers.iter_mut() {
                if n == "idempotency-key" {
                    *v = "other".into();
                }
            }
        },
    ] {
        let mut h = signed.clone();
        tamper(&mut h);
        let r = f.gate.authenticate(&h);
        assert_eq!(reason(r), (Code::ApiUnauthenticated, "bad_signature"));
    }
}

#[test]
fn the_profile_is_enforced() {
    let f = fixture();
    let b = boot(&f);
    // Unsigned.
    let unsigned = Req::get("/v1/leases", &b).unsigned_head();
    assert_eq!(
        reason(f.gate.authenticate(&unsigned)),
        (Code::ApiUnauthenticated, "unsigned")
    );
    // The graph isn't covered.
    let mut r = Req::get("/v1/leases", &b);
    r.cover = Some(vec!["@method", "@path", "@query", "mdkg-boot"]);
    assert_eq!(
        reason(f.gate.authenticate(&r.signed())),
        (Code::ApiUnauthenticated, "not_covered")
    );
    // A turn header the signature leaves out can't ride along.
    let mut r = Req::get("/v1/leases", &b).header("mdkg-turn", "t");
    r.cover = Some(ALWAYS.to_vec());
    assert_eq!(
        reason(f.gate.authenticate(&r.signed())),
        (Code::ApiUnauthenticated, "not_covered")
    );
    // Covering a header the request lacks.
    let mut r = Req::get("/v1/leases", &b);
    r.cover = Some(vec![
        "@method",
        "@path",
        "@query",
        "mdkg-graph",
        "mdkg-boot",
        "mdkg-turn",
    ]);
    assert_eq!(
        reason(f.gate.authenticate(&r.signed())),
        (Code::ApiUnauthenticated, "component_missing")
    );
    // Another application's tag, a foreign component, a short nonce.
    let signed = Req::get("/v1/leases", &b).signed();
    for (from, to) in [
        ("tag=\"mdkg-v1\"", "tag=\"other\""),
        ("\"@method\"", "\"@authority\""),
        ("alg=\"ed25519\"", "alg=\"hmac-sha256\""),
    ] {
        let mut h = signed.clone();
        for (n, v) in h.headers.iter_mut() {
            if n == "signature-input" {
                *v = v.replace(from, to);
            }
        }
        assert_eq!(
            reason(f.gate.authenticate(&h)),
            (Code::ApiUnauthenticated, "profile"),
            "{to}"
        );
    }
    let mut r = Req::get("/v1/leases", &b);
    r.nonce = "short".into();
    assert_eq!(
        reason(f.gate.authenticate(&r.signed())),
        (Code::ApiUnauthenticated, "profile")
    );
    // A repeated signed header.
    let mut h = Req::get("/v1/leases", &b).signed();
    h.headers.push(("mdkg-graph".into(), "graph-2".into()));
    assert_eq!(
        reason(f.gate.authenticate(&h)),
        (Code::ApiUnauthenticated, "header_repeated")
    );
}

#[test]
fn write_rules() {
    let f = fixture();
    let b = boot(&f);
    let r = Req::post("/v1/leases", &b, b"{}").without("idempotency-key");
    assert_eq!(
        reason(f.gate.authenticate(&r.signed())),
        (Code::ApiKeyRequired, "key_required")
    );
    let r = Req::post("/v1/leases", &b, b"{}").without("mdkg-actor");
    assert_eq!(
        reason(f.gate.authenticate(&r.signed())),
        (Code::ApiUnauthenticated, "actor_missing")
    );
    let r = Req::post("/v1/leases", &b, b"{}").without("content-digest");
    assert_eq!(
        reason(f.gate.authenticate(&r.signed())),
        (Code::ApiUnauthenticated, "digest_missing")
    );
    // A long read is a POST but not a write.
    let r = Req::post("/v1/leases:read", &b, b"{}")
        .without("idempotency-key")
        .without("mdkg-actor");
    let auth = f.gate.authenticate(&r.signed()).expect("read");
    assert!(!auth.is_write());
}

#[test]
fn keys_graph_boot_and_window() {
    let f = fixture();
    let b = boot(&f);
    let mut r = Req::get("/v1", &b);
    r.keyid = "m-unknown".into();
    assert_eq!(
        reason(f.gate.authenticate(&r.signed())),
        (Code::ApiUnauthenticated, "unknown_key")
    );
    for (m, code) in [
        ("m-revoked", Code::ApiMachineRevoked),
        ("m-pending", Code::ApiMachinePending),
    ] {
        let mut r = Req::get("/v1", &b);
        r.keyid = m.into();
        r.sign_with = key_of(m);
        assert_eq!(reason(f.gate.authenticate(&r.signed())).0, code);
    }
    // Signed by a key other than the one enrolled.
    let mut r = Req::get("/v1", &b);
    r.sign_with = key_of("m-other");
    assert_eq!(
        reason(f.gate.authenticate(&r.signed())),
        (Code::ApiUnauthenticated, "bad_signature")
    );
    let r = Req::get("/v1", &b).header("mdkg-graph", "graph-2");
    let e = f.gate.authenticate(&r.signed()).expect_err("graph");
    assert_eq!((e.code, e.status()), (Code::ApiGraphUnknown, 404));
    let r = Req::get("/v1", "00000000000000000000000000000000");
    let e = f.gate.authenticate(&r.signed()).expect_err("boot");
    assert_eq!(e.code, Code::ApiReplayed);
    assert_eq!(e.data.get("boot_id"), Some(&Value::String(b.clone())));
    for off in [-61, 61] {
        let mut r = Req::get("/v1", &b);
        r.created = T0 / 1000 + off;
        let e = f.gate.authenticate(&r.signed()).expect_err("skew");
        assert_eq!((e.code, e.status()), (Code::ApiClockSkew, 401));
        assert_eq!(e.data.get("server_time"), Some(&Value::from(T0 / 1000)));
    }
    let mut r = Req::get("/v1", &b);
    r.created = T0 / 1000 + 60;
    f.gate
        .authenticate(&r.signed())
        .expect("edge of the window");
}

#[test]
fn a_replay_is_refused_and_a_bad_signature_uses_no_nonce() {
    let f = fixture();
    let b = boot(&f);
    let r = Req::get("/v1", &b);
    // A forged copy first: it must not use up the nonce.
    let mut forged = r.clone();
    forged.sign_with = key_of("attacker");
    assert!(f.gate.authenticate(&forged.signed()).is_err());
    let signed = r.signed();
    f.gate.authenticate(&signed).expect("first");
    assert_eq!(
        reason(f.gate.authenticate(&signed)),
        (Code::ApiReplayed, "nonce_reused")
    );
}

#[test]
fn a_clock_step_back_rotates_the_boot_and_refuses_old_signatures() {
    let f = fixture();
    let b = boot(&f);
    let signed = Req::get("/v1", &b).signed();
    f.clocks.advance(130_000);
    f.gate.prune_nonces().expect("prune");
    // Step back so the captured request is inside the window again.
    f.clocks.step_wall(-130_000);
    let e = f.gate.authenticate(&signed).expect_err("rotated");
    assert_eq!(e.code, Code::ApiReplayed);
    assert_ne!(e.data.get("boot_id"), Some(&Value::String(b)));
}

#[test]
fn browser_headers_are_refused_unless_allowed() {
    let f = fixture();
    let b = boot(&f);
    for (n, v) in [
        ("origin", "https://evil.example"),
        ("sec-fetch-site", "cross-site"),
    ] {
        let r = Req::get("/v1", &b).header(n, v);
        let e = f.gate.authenticate(&r.signed()).expect_err("origin");
        assert_eq!(e.code, Code::ApiOriginRefused);
    }
    let r = Req::get("/v1", &b)
        .header("origin", "https://allowed.example")
        .header("sec-fetch-site", "cross-site");
    f.gate.authenticate(&r.signed()).expect("allowed");
}

#[test]
fn a_pending_key_signs_only_its_own_enrolment() {
    let f = fixture();
    let b = boot(&f);
    let pk = key_of("m-new");
    let mut r = Req::post("/enroll/m-new:reveal", &b, b"{}")
        .without("idempotency-key")
        .without("mdkg-actor");
    r.keyid = "m-new".into();
    r.sign_with = pk;
    let auth = f
        .gate
        .authenticate_pending(&r.signed(), "m-new", &pk, &|_| Ok(()))
        .expect("reveal");
    assert_eq!(
        auth.origin(),
        Origin::Pending {
            machine: "m-new".into()
        }
    );
    f.gate.check_pending_body(&auth, b"{}").expect("digest");
    let mut r2 = r.clone();
    r2.nonce = b64::encode_url(&[7u8; 16]);
    r2.keyid = "m-else".into();
    assert_eq!(
        reason_p(
            f.gate
                .authenticate_pending(&r2.signed(), "m-new", &pk, &|_| Ok(()))
        ),
        (Code::ApiUnauthenticated, "keyid_mismatch")
    );
}

#[test]
fn a_pending_key_is_bound_to_its_own_route() {
    // Terra 2 #6713: the identity a pending key proves must be the one the
    // route names, for the reveal's and the poll's method only.
    let f = fixture();
    let b = boot(&f);
    let pk = key_of("m-a");
    let pending = |path: &str, post: bool, id: &str, n: u8| {
        let mut r = if post {
            Req::post(path, &b, b"{}")
        } else {
            Req::get(path, &b)
        }
        .without("idempotency-key")
        .without("mdkg-actor");
        r.keyid = "m-a".into();
        r.sign_with = pk;
        r.nonce = b64::encode_url(&[n; 16]);
        f.gate
            .authenticate_pending(&r.signed(), id, &pk, &|_| Ok(()))
    };
    let refused = (Code::ApiUnauthenticated, "pending_route");
    // m-a's key, addressed to m-b, whichever id the caller supplies.
    assert_eq!(
        reason_p(pending("/enroll/m-b:reveal", true, "m-a", 1)),
        refused
    );
    assert_eq!(reason_p(pending("/enroll/m-b", false, "m-a", 2)), refused);
    assert_eq!(
        reason_p(pending("/enroll/m-a:reveal", true, "m-b", 3)),
        refused
    );
    // Not an enrolment route, or the wrong method for one.
    assert_eq!(reason_p(pending("/v1/nodes", false, "m-a", 4)), refused);
    assert_eq!(reason_p(pending("/enroll/m-a", true, "m-a", 5)), refused);
    assert_eq!(reason_p(pending("/enroll", true, "m-a", 6)), refused);
    // Its own reveal and poll pass.
    pending("/enroll/m-a:reveal", true, "m-a", 7).expect("reveal");
    pending("/enroll/m-a", false, "m-a", 8).expect("poll");

    // The first enrolment is only `POST /enroll`.
    let body = enroll_body("m-a", &pk);
    let mut r = Req::post("/enroll/m-a:reveal", &b, &body)
        .without("idempotency-key")
        .without("mdkg-actor");
    r.keyid = "m-a".into();
    r.sign_with = pk;
    assert_eq!(
        reason_e(f.gate.authenticate_enroll(&r.signed(), &body, &|_| Ok(()))),
        refused
    );
}

#[test]
fn a_pending_key_cannot_pass_as_an_approved_machine() {
    // Terra 6 #6667: a pending key that signs as an approved machine's id
    // still gets only a pending origin, which the kernel never takes; and
    // the normal path checks the directory's key, not the pending one.
    let f = fixture();
    let b = boot(&f);
    let attacker = key_of("attacker");
    let mut r = Req::get("/enroll/m-ok", &b);
    r.sign_with = attacker;
    let pending = f
        .gate
        .authenticate_pending(&r.signed(), "m-ok", &attacker, &|_| Ok(()))
        .expect("signed by the pending key it named");
    assert!(matches!(pending.origin(), Origin::Pending { .. }));
    let mut r2 = r.clone();
    r2.nonce = b64::encode_url(&[9u8; 16]);
    assert_eq!(
        reason(f.gate.authenticate(&r2.signed())),
        (Code::ApiUnauthenticated, "bad_signature")
    );
}

fn enroll_body(machine: &str, pk: &[u8; 32]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "machine_id": machine,
        "public_key": b64::encode_url(pk),
        "name": "box",
        "graph_id": GRAPH,
        "commitment": "c",
    }))
    .expect("json")
}

#[test]
fn the_first_enrolment_is_signed_by_the_key_it_carries() {
    let f = fixture();
    let b = boot(&f);
    let pk = key_of("m-new");
    let body = enroll_body("m-new", &pk);
    let mut r = Req::post("/enroll", &b, &body)
        .without("idempotency-key")
        .without("mdkg-actor");
    r.keyid = "m-new".into();
    r.sign_with = pk;
    let (auth, key) = f
        .gate
        .authenticate_enroll(&r.signed(), &body, &|_| Ok(()))
        .expect("self-signed");
    assert_eq!(auth.machine(), "m-new");
    assert_eq!(key.public_key, pk);

    // Signed by another key than the body's: no proof of possession.
    let mut forged = r.clone();
    forged.sign_with = key_of("attacker");
    forged.nonce = b64::encode_url(&[5u8; 16]);
    assert_eq!(
        reason_e(
            f.gate
                .authenticate_enroll(&forged.signed(), &body, &|_| Ok(()))
        ),
        (Code::ApiUnauthenticated, "bad_signature")
    );
    // A body swapped after signing.
    let other = enroll_body("m-new", &key_of("attacker"));
    let mut r3 = r.clone();
    r3.nonce = b64::encode_url(&[6u8; 16]);
    assert_eq!(
        reason_e(
            f.gate
                .authenticate_enroll(&r3.signed(), &other, &|_| Ok(()))
        ),
        (Code::ApiUnauthenticated, "digest_mismatch")
    );
    // keyid other than the body's machine id.
    let mut r4 = r.clone();
    r4.keyid = "m-ok".into();
    r4.nonce = b64::encode_url(&[8u8; 16]);
    assert_eq!(
        reason_e(f.gate.authenticate_enroll(&r4.signed(), &body, &|_| Ok(()))),
        (Code::ApiUnauthenticated, "keyid_mismatch")
    );
    // The same enrolment again is a replay.
    assert_eq!(
        reason_e(f.gate.authenticate_enroll(&r.signed(), &body, &|_| Ok(()))),
        (Code::ApiReplayed, "nonce_reused")
    );
    // A body that names no key.
    let mut r5 = Req::post("/enroll", &b, b"{}")
        .without("idempotency-key")
        .without("mdkg-actor");
    r5.keyid = "m-new".into();
    assert_eq!(
        reason_e(f.gate.authenticate_enroll(&r5.signed(), b"{}", &|_| Ok(()))),
        (Code::ApiUnauthenticated, "enroll_key")
    );
}

fn reason_p(r: Result<PendingAuth, Refusal>) -> (Code, &'static str) {
    let e = r.expect_err("refused");
    (e.code, e.test_reason())
}

fn reason_e(r: Result<(PendingAuth, EnrollKey), Refusal>) -> (Code, &'static str) {
    let e = r.expect_err("refused");
    (e.code, e.test_reason())
}

#[test]
fn person_signatures() {
    let f = fixture();
    let b = boot(&f);
    let ann = key_of("pk-ann");
    let r = Req::post("/v1/agents/a:retire", &b, b"{}").header("mdkg-actor", "human:ann");
    let auth = f
        .gate
        .authenticate(&r.person_signed("pk-ann", ann))
        .expect("person");
    assert_eq!(auth.person().map(|p| p.human.as_str()), Some("human:ann"));
    assert_eq!(auth.machine(), "m-ok");
    assert!(matches!(
        auth.origin(),
        Origin::Remote {
            person: Some(_),
            ..
        }
    ));

    // Another person's actor.
    let mut r2 = r.clone().header("mdkg-actor", "human:bob");
    r2.nonce = b64::encode_url(&[1u8; 16]);
    assert_eq!(
        reason(f.gate.authenticate(&r2.person_signed("pk-ann", ann))),
        (Code::ApiActorNotAllowed, "person_actor_mismatch")
    );
    // A wrong person key.
    let mut r3 = r.clone();
    r3.nonce = b64::encode_url(&[2u8; 16]);
    assert_eq!(
        reason(
            f.gate
                .authenticate(&r3.person_signed("pk-ann", key_of("x")))
        ),
        (Code::ApiUnauthenticated, "bad_person_signature")
    );
    // A revoked person key: the transport refuses it (Terra #6271's race is
    // closed by the kernel's recheck inside the write; this is layer 1).
    f.persons
        .0
        .lock()
        .expect("lock")
        .get_mut("pk-ann")
        .expect("ann")
        .2 = true;
    let mut r4 = r.clone();
    r4.nonce = b64::encode_url(&[3u8; 16]);
    assert_eq!(
        reason(f.gate.authenticate(&r4.person_signed("pk-ann", ann))),
        (Code::ApiPersonUnknown, "person_unknown")
    );
    // A person header with no person signature.
    let mut r5 = r.clone().header(PERSON_HEADER, "pk-ann");
    r5.nonce = b64::encode_url(&[4u8; 16]);
    assert_eq!(
        reason(f.gate.authenticate(&r5.signed())),
        (Code::ApiUnauthenticated, "profile")
    );
}

#[test]
fn a_person_signature_cannot_be_lifted_onto_another_request() {
    let f = fixture();
    let b = boot(&f);
    let ann = key_of("pk-ann");
    let r = Req::post("/v1/agents/a:retire", &b, b"{}").header("mdkg-actor", "human:ann");
    let first = r.person_signed("pk-ann", ann);
    // A second machine-signed request, with the first's person signature.
    let mut other = Req::post("/v1/agents/b:retire", &b, b"{}")
        .header("mdkg-actor", "human:ann")
        .header(PERSON_HEADER, "pk-ann")
        .signed();
    let pin = first
        .one("signature-input")
        .ok()
        .flatten()
        .expect("in")
        .to_owned();
    let psig = first
        .one("signature")
        .ok()
        .flatten()
        .expect("sig")
        .to_owned();
    let p_in = &pin[pin.find(", mdkg-person=").expect("person input")..];
    let p_sig = &psig[psig.find(", mdkg-person=").expect("person sig")..];
    for (n, v) in other.headers.iter_mut() {
        if n == "signature-input" {
            v.push_str(p_in);
        } else if n == "signature" {
            v.push_str(p_sig);
        }
    }
    assert_eq!(
        reason(f.gate.authenticate(&other)),
        (Code::ApiUnauthenticated, "bad_person_signature")
    );
}

#[test]
fn a_revoked_machine_is_refused_at_the_next_request() {
    let f = fixture();
    let b = boot(&f);
    f.gate
        .authenticate(&Req::get("/v1", &b).signed())
        .expect("approved");
    f.keys
        .0
        .lock()
        .expect("lock")
        .get_mut("m-ok")
        .expect("m-ok")
        .state = MachineState::Revoked;
    assert_eq!(
        reason(f.gate.authenticate(&Req::get("/v1", &b).signed())).0,
        Code::ApiMachineRevoked
    );
}

#[test]
fn a_transient_forward_jump_seen_by_a_prune_cannot_reopen_a_replay() {
    // Terra 6 #6612: after 120 monotonic seconds, a prune that sees the
    // wall clock jumped ahead drops the nonce; the clock is then corrected
    // before the next request. The prune's observation raised the mark, so
    // the correction rotates the boot id and the captured copy is refused.
    let f = fixture();
    let b = boot(&f);
    let mut r = Req::get("/v1", &b);
    r.created = T0 / 1000 + 60;
    let signed = r.signed();
    f.gate.authenticate(&signed).expect("first");
    // 120 s of monotonic time pass while the wall clock stands still, so
    // `created` is still ahead of the wall clock.
    f.clocks
        .mono
        .fetch_add(120_000, std::sync::atomic::Ordering::SeqCst);
    f.clocks.step_wall(3_600_000);
    assert_eq!(f.gate.prune_nonces().expect("prune"), 1);
    f.clocks.step_wall(-3_600_000);
    let e = f.gate.authenticate(&signed).expect_err("replay");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::ApiReplayed, "boot_changed")
    );
}

/// A verifier that, once armed, blocks inside `verify` until released:
/// a request that verifies slowly.
struct BlockingVerifier {
    armed: std::sync::atomic::AtomicBool,
    entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Verifier for BlockingVerifier {
    fn verify(&self, public_key: &[u8; 32], base: &[u8], signature: &[u8; 64]) -> bool {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            if let Some(tx) = self.entered.lock().expect("lock").take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.release.lock().expect("lock").take() {
                let _ = rx.recv();
            }
        }
        &fake_sign(public_key, base) == signature
    }
}

#[test]
fn a_slow_verification_cannot_insert_after_a_prune_dropped_its_nonce() {
    // Terra 1 #6697: a copy passes the window check just before 120 s,
    // stalls in verification, a prune at 125 s drops the original's nonce,
    // and the copy resumes. Its insert re-reads the clocks under the nonce
    // lock and finds `created` outside the window.
    let base = fixture();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let verifier = Arc::new(BlockingVerifier {
        armed: std::sync::atomic::AtomicBool::new(false),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
    });
    let clocks = base.clocks.clone();
    let gate = Arc::new(Gate::new(
        GRAPH.into(),
        Boot::start(&*clocks).expect("boot"),
        clocks.clone(),
        NonceCache::default(),
        base.keys.clone(),
        verifier.clone(),
        base.persons.clone(),
        vec![],
        Arc::new(crate::limits::RateLimiter::new(
            crate::limits::Rate::default(),
            "m-ok".into(),
        )),
    ));
    let b = gate.boot_id().expect("boot");
    let mut r = Req::get("/v1", &b);
    r.created = T0 / 1000 + 60;
    let signed = r.signed();
    gate.authenticate(&signed).expect("the original");
    clocks.advance(119_900);
    verifier
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (g, h) = (gate.clone(), signed.clone());
    let copy = std::thread::spawn(move || g.authenticate(&h));
    // Bounded, so a regression that refuses before verifying fails the
    // test instead of hanging it (Terra 3 #6773).
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the copy is verifying");
    clocks.advance(6_000);
    assert_eq!(gate.prune_nonces().expect("prune"), 1);
    release_tx.send(()).expect("release");
    let e = copy
        .join()
        .expect("thread")
        .expect_err("the copy is refused");
    assert_eq!(e.code, Code::ApiClockSkew);
}

#[test]
fn a_rate_limited_flood_takes_no_nonces() {
    // Terra 3 #6700: requests over the machine's rate are refused before
    // their nonce is inserted, and so is a malformed digest.
    let base = fixture();
    let rate = Arc::new(crate::limits::RateLimiter::new(
        crate::limits::Rate {
            reads_per_s: 1.0,
            read_burst: 2.0,
            ..crate::limits::Rate::default()
        },
        "host".into(),
    ));
    let gate = Gate::new(
        GRAPH.into(),
        Boot::start(&*base.clocks).expect("boot"),
        base.clocks.clone(),
        NonceCache::default(),
        base.keys.clone(),
        Arc::new(FakeVerifier),
        base.persons.clone(),
        vec![],
        rate,
    );
    let b = gate.boot_id().expect("boot");
    for i in 0..10u8 {
        let mut r = Req::get("/v1", &b);
        r.nonce = b64::encode_url(&[i; 16]);
        let res = gate.authenticate(&r.signed());
        if i < 2 {
            res.expect("within the burst");
        } else {
            assert_eq!(res.expect_err("limited").test_reason(), "reads");
        }
    }
    assert_eq!(gate.nonces.len(), 2);
    let mut r = Req::post("/v1/leases", &b, b"{}").header("content-digest", "sha-256=:AA==:");
    r.nonce = b64::encode_url(&[42; 16]);
    assert_eq!(
        gate.authenticate(&r.signed())
            .expect_err("digest")
            .test_reason(),
        "bad_digest"
    );
    assert_eq!(gate.nonces.len(), 2);
}

#[test]
fn a_pending_key_cannot_reserve_an_approved_machines_nonce() {
    // Terra 2 #6713: a pending request that carries an approved machine's
    // in-flight nonce, even naming that machine's id, uses its own cache,
    // under the key it proved; the genuine request stays fresh.
    let f = fixture();
    let b = boot(&f);
    let attacker = key_of("attacker");
    let seen = b64::encode_url(&[0x5a; 16]);

    // A poll and a reveal for the approved id, signed by another key: the
    // second is a replay of the first's nonce, under the key that signed.
    let mut results = Vec::new();
    for (post, path) in [(false, "/enroll/m-ok"), (true, "/enroll/m-ok:reveal")] {
        let mut r = if post {
            Req::post(path, &b, b"{}")
        } else {
            Req::get(path, &b)
        }
        .without("idempotency-key")
        .without("mdkg-actor");
        r.sign_with = attacker;
        r.nonce = seen.clone();
        results.push(
            f.gate
                .authenticate_pending(&r.signed(), "m-ok", &attacker, &|_| Ok(())),
        );
    }
    assert!(results[0].is_ok());
    assert_eq!(
        reason_p(results.remove(1)),
        (Code::ApiReplayed, "nonce_reused")
    );
    // A first enrolment for a new id, by another key, with the same nonce.
    let other = key_of("attacker-2");
    let body = enroll_body("m-new", &other);
    let mut e = Req::post("/enroll", &b, &body)
        .without("idempotency-key")
        .without("mdkg-actor");
    e.keyid = "m-new".into();
    e.sign_with = other;
    e.nonce = seen.clone();
    f.gate
        .authenticate_enroll(&e.signed(), &body, &|_| Ok(()))
        .expect("enrol");
    assert_eq!(f.gate.nonces.len(), 0, "nothing in the approved cache");
    assert_eq!(f.gate.pending_nonces.len(), 2, "one per proven key");

    let mut genuine = Req::get("/v1/nodes", &b);
    genuine.nonce = seen;
    f.gate.authenticate(&genuine.signed()).expect("fresh");
}

#[test]
fn an_enrolment_for_a_known_id_is_refused_and_its_replay_is_a_replay() {
    let f = fixture();
    let b = boot(&f);
    let attacker = key_of("attacker");
    let seen = b64::encode_url(&[0x6b; 16]);
    // The store, as `known`: it holds another key for each id.
    let asked = std::sync::atomic::AtomicUsize::new(0);
    let store = |_: &EnrollKey| {
        asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(Refusal::new(
            Code::ApiEnrollExists,
            ErrorKind::Conflict,
            "exists",
        ))
    };
    for (n, (known, store_asked)) in [("m-ok", 0), ("m-revoked", 0), ("m-pending", 1)]
        .into_iter()
        .enumerate()
    {
        let body = enroll_body(known, &attacker);
        let mut e = Req::post("/enroll", &b, &body)
            .without("idempotency-key")
            .without("mdkg-actor");
        e.keyid = known.into();
        e.sign_with = attacker;
        e.nonce = b64::encode_url(&[0x6b + n as u8; 16]);
        // Another key learns only that the id exists, whatever its state
        // (#7853). An enrolled or revoked machine is the directory's
        // answer; a pending entry, whatever its key, is the store's, as
        // the entry may be stale (Terra 3 #8179).
        asked.store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            reason_e(f.gate.authenticate_enroll(&e.signed(), &body, &store)),
            (Code::ApiEnrollExists, "exists"),
            "{known}"
        );
        assert_eq!(
            asked.load(std::sync::atomic::Ordering::Relaxed),
            store_asked,
            "{known}"
        );
        // The refusal used the nonce: sent again, the request is a replay,
        // before any lookup (Terra 7 #8463, orchestrator #8480).
        asked.store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            reason_e(f.gate.authenticate_enroll(&e.signed(), &body, &store)),
            (Code::ApiReplayed, "nonce_reused"),
            "{known}"
        );
        assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
    // A stale pending entry whose row the store no longer holds lets a
    // new key's request through.
    let body = enroll_body("m-pending", &attacker);
    let mut e = Req::post("/enroll", &b, &body)
        .without("idempotency-key")
        .without("mdkg-actor");
    e.keyid = "m-pending".into();
    e.sign_with = attacker;
    e.nonce = b64::encode_url(&[0x7f; 16]);
    f.gate
        .authenticate_enroll(&e.signed(), &body, &|_| Ok(()))
        .expect("the store holds no row");
    // Each request used its nonce in the pending keys' cache.
    assert_eq!(f.gate.nonces.len(), 0);
    assert_eq!(f.gate.pending_nonces.len(), 4);
    let mut genuine = Req::get("/v1/nodes", &b);
    genuine.nonce = seen;
    f.gate.authenticate(&genuine.signed()).expect("fresh");
}

#[test]
fn a_full_pending_cache_refuses_enrolment_requests_never_api_requests() {
    let f = fixture();
    let b = boot(&f);
    let gate = Arc::try_unwrap(f.gate)
        .expect("one owner")
        .with_pending_nonces(NonceCache::new(1));
    let enrol = |id: &str| {
        let pk = key_of(id);
        let body = enroll_body(id, &pk);
        let mut e = Req::post("/enroll", &b, &body)
            .without("idempotency-key")
            .without("mdkg-actor");
        e.keyid = id.into();
        e.sign_with = pk;
        gate.authenticate_enroll(&e.signed(), &body, &|_| Ok(()))
    };
    enrol("m-new-1").expect("first");
    assert_eq!(
        reason_e(enrol("m-new-2")),
        (Code::ApiRateLimited, "nonce_cache_full")
    );
    // A reveal and a poll of the machine already pending are refused too
    // (#6911): every pending request shares the bounded cache.
    let pk = key_of("m-new-1");
    for (post, path) in [(true, "/enroll/m-new-1:reveal"), (false, "/enroll/m-new-1")] {
        let mut r = if post {
            Req::post(path, &b, b"{}")
        } else {
            Req::get(path, &b)
        }
        .without("idempotency-key")
        .without("mdkg-actor");
        r.keyid = "m-new-1".into();
        r.sign_with = pk;
        assert_eq!(
            reason_p(gate.authenticate_pending(&r.signed(), "m-new-1", &pk, &|_| Ok(()))),
            (Code::ApiRateLimited, "nonce_cache_full"),
            "{path}"
        );
    }
    gate.authenticate(&Req::get("/v1/nodes", &b).signed())
        .expect("approved machines keep their own cache");
    // But an approved machine polling its own enrolment goes through the
    // pending cache, and is refused while it is full (#7147).
    let ok = key_of("m-ok");
    let poll = Req::get("/enroll/m-ok", &b).signed();
    assert_eq!(
        reason_p(gate.authenticate_pending(&poll, "m-ok", &ok, &|_| Ok(()))),
        (Code::ApiRateLimited, "nonce_cache_full")
    );
    // The timer prunes both caches.
    f.clocks.advance(200_000);
    assert_eq!(gate.prune_nonces(), Ok(2));
}

#[test]
fn replays_spend_none_of_the_machines_rate() {
    // Terra 4 #6786: a copy of a signed request, repeated, is refused as a
    // replay before the bucket is charged, so the machine keeps its budget.
    let base = fixture();
    let gate = Gate::new(
        GRAPH.into(),
        Boot::start(&*base.clocks).expect("boot"),
        base.clocks.clone(),
        NonceCache::default(),
        base.keys.clone(),
        Arc::new(FakeVerifier),
        base.persons.clone(),
        vec![],
        Arc::new(crate::limits::RateLimiter::new(
            crate::limits::Rate {
                reads_per_s: 1.0,
                read_burst: 2.0,
                ..crate::limits::Rate::default()
            },
            "host".into(),
        )),
    );
    let b = gate.boot_id().expect("boot");
    let first = Req::get("/v1", &b).signed();
    gate.authenticate(&first).expect("the original");
    for _ in 0..5 {
        assert_eq!(
            reason(gate.authenticate(&first)),
            (Code::ApiReplayed, "nonce_reused")
        );
    }
    gate.authenticate(&Req::get("/v1", &b).signed())
        .expect("the burst's second token is still there");
    assert_eq!(
        gate.authenticate(&Req::get("/v1", &b).signed())
            .expect_err("now over the rate")
            .test_reason(),
        "reads"
    );
}

#[test]
fn a_known_id_is_answered_only_after_the_origin_graph_and_signature() {
    // Terra 6 #6939: a request for another graph, from a refused origin,
    // or with a bad signature learns nothing of this graph's ids and uses
    // no nonce; a known id's refusal uses its nonce (orchestrator #8480).
    let f = fixture();
    let b = boot(&f);
    let attacker = key_of("attacker");
    let body = enroll_body("m-ok", &attacker);
    let enrol = |edit: &dyn Fn(&mut Req)| {
        let mut e = Req::post("/enroll", &b, &body)
            .without("idempotency-key")
            .without("mdkg-actor");
        e.sign_with = attacker;
        edit(&mut e);
        reason_e(f.gate.authenticate_enroll(&e.signed(), &body, &|_| Ok(())))
    };
    assert_eq!(
        enrol(&|e| *e = e.clone().header("mdkg-graph", "graph-other")),
        (Code::ApiGraphUnknown, "graph_unknown")
    );
    assert_eq!(
        enrol(&|e| *e = e.clone().header("origin", "https://evil.example")),
        (Code::ApiOriginRefused, "origin")
    );
    assert_eq!(
        enrol(&|e| e.sign_with = key_of("someone-else")),
        (Code::ApiUnauthenticated, "bad_signature")
    );
    assert_eq!(f.gate.nonces_held(), 0);
    assert_eq!(enrol(&|_| {}), (Code::ApiEnrollExists, "exists"));
    assert_eq!(f.gate.nonces_held(), 1);
}

#[test]
fn a_replay_refused_after_a_rotation_names_the_new_boot() {
    // Terra 3 #7115: a copy stalls in verification while a backward clock
    // step rotates the boot id; its replay refusal must carry the new id,
    // which a client needs to sign its retry.
    let base = fixture();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let verifier = Arc::new(BlockingVerifier {
        armed: std::sync::atomic::AtomicBool::new(false),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
    });
    let clocks = base.clocks.clone();
    let gate = Arc::new(Gate::new(
        GRAPH.into(),
        Boot::start(&*clocks).expect("boot"),
        clocks.clone(),
        NonceCache::default(),
        base.keys.clone(),
        verifier.clone(),
        base.persons.clone(),
        vec![],
        Arc::new(crate::limits::RateLimiter::new(
            crate::limits::Rate::default(),
            "m-ok".into(),
        )),
    ));
    let old = gate.boot_id().expect("boot");
    let signed = Req::get("/v1", &old).signed();
    gate.authenticate(&signed).expect("the original");
    verifier
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (g, h) = (gate.clone(), signed.clone());
    let copy = std::thread::spawn(move || g.authenticate(&h));
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the copy is verifying");
    clocks.step_wall(-5_000);
    let new = gate.boot_id().expect("rotated");
    assert_ne!(new, old);
    release_tx.send(()).expect("release");
    let e = copy.join().expect("thread").expect_err("refused");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::ApiReplayed, "boot_changed")
    );
    assert_eq!(e.data["boot_id"], serde_json::Value::String(new));
}

#[test]
fn concurrent_copies_of_one_request_charge_one_token() {
    // Orchestrator #7118, Terra 3 #7115: the replay check, the charge and
    // the insert are one step, so of eight copies sent at once exactly one
    // passes and is charged; the machine keeps the rest of its burst.
    let base = fixture();
    let gate = Arc::new(Gate::new(
        GRAPH.into(),
        Boot::start(&*base.clocks).expect("boot"),
        base.clocks.clone(),
        NonceCache::default(),
        base.keys.clone(),
        Arc::new(FakeVerifier),
        base.persons.clone(),
        vec![],
        Arc::new(crate::limits::RateLimiter::new(
            crate::limits::Rate {
                reads_per_s: 0.001,
                read_burst: 2.0,
                ..crate::limits::Rate::default()
            },
            "host".into(),
        )),
    ));
    let b = gate.boot_id().expect("boot");
    let signed = Req::get("/v1", &b).signed();
    let start = Arc::new(std::sync::Barrier::new(8));
    let results: Vec<_> = (0..8)
        .map(|_| {
            let (g, h, s) = (gate.clone(), signed.clone(), start.clone());
            std::thread::spawn(move || {
                s.wait();
                g.authenticate(&h).map(|_| ()).map_err(|e| e.test_reason())
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().expect("thread"))
        .collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .all(|r| r.is_ok() || *r == Err("nonce_reused")),
        "{results:?}"
    );
    gate.authenticate(&Req::get("/v1", &b).signed())
        .expect("the second token is still there");
    assert_eq!(
        reason(gate.authenticate(&Req::get("/v1", &b).signed())),
        (Code::ApiRateLimited, "reads")
    );
}

#[test]
fn a_refusing_store_hook_runs_only_after_the_origin_graph_and_signature() {
    // #7182: with a store hook that refuses (an id already pending), the
    // refusal still comes only after the origin, graph and signature
    // checks; those refusals use no nonce, and the hook's own refusal does
    // (orchestrator #8480).
    let f = fixture();
    let b = boot(&f);
    let pk = key_of("m-new-7");
    let body = enroll_body("m-new-7", &pk);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let pending = || {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(Refusal::new(
            Code::ApiEnrollPending,
            ErrorKind::Conflict,
            "pending",
        ))
    };
    let enrol = |edit: &dyn Fn(&mut Req)| {
        let mut e = Req::post("/enroll", &b, &body)
            .without("idempotency-key")
            .without("mdkg-actor");
        e.keyid = "m-new-7".into();
        e.sign_with = pk;
        edit(&mut e);
        reason_e(
            f.gate
                .authenticate_enroll(&e.signed(), &body, &|_| pending()),
        )
    };
    assert_eq!(
        enrol(&|e| *e = e.clone().header("mdkg-graph", "graph-other")),
        (Code::ApiGraphUnknown, "graph_unknown")
    );
    assert_eq!(
        enrol(&|e| *e = e.clone().header("origin", "https://evil.example")),
        (Code::ApiOriginRefused, "origin")
    );
    assert_eq!(
        enrol(&|e| e.sign_with = key_of("someone-else")),
        (Code::ApiUnauthenticated, "bad_signature")
    );
    // The hook never ran for those (#7800): not run and discarded, but not
    // run at all.
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(f.gate.nonces_held(), 0, "none of those used a nonce");
    assert_eq!(enrol(&|_| {}), (Code::ApiEnrollPending, "pending"));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(f.gate.nonces_held(), 1, "the refused one did (#8480)");
}

#[test]
fn a_pending_entry_for_its_own_key_asks_the_store_and_a_wrong_graph_wins() {
    // Terra 3 #7835: a pending directory entry for the key that signed is
    // checked with the store, which may know it expired; and a directory
    // hit still answers graph_unknown for a body naming another graph.
    let f = fixture();
    let b = boot(&f);
    let own = key_of("m-pending");
    let body = enroll_body("m-pending", &own);
    let mut e = Req::post("/enroll", &b, &body)
        .without("idempotency-key")
        .without("mdkg-actor");
    e.keyid = "m-pending".into();
    e.sign_with = own;
    // The store says the request expired: the enrolment proceeds.
    let asked = std::sync::atomic::AtomicBool::new(false);
    f.gate
        .authenticate_enroll(&e.signed(), &body, &|_| {
            asked.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .expect("a stale entry defers to the store");
    assert!(asked.load(std::sync::atomic::Ordering::SeqCst));
    // A directory hit with another graph in the body.
    let mut other: serde_json::Value =
        serde_json::from_slice(&enroll_body("m-ok", &own)).expect("j");
    other["graph_id"] = "graph-other".into();
    let other = serde_json::to_vec(&other).expect("json");
    let mut g = Req::post("/enroll", &b, &other)
        .without("idempotency-key")
        .without("mdkg-actor");
    g.keyid = "m-ok".into();
    g.sign_with = own;
    assert_eq!(
        reason_e(f.gate.authenticate_enroll(&g.signed(), &other, &|_| Ok(()))),
        (Code::ApiGraphUnknown, "graph_unknown")
    );
}

#[test]
fn of_concurrent_copies_of_a_first_enrolment_only_one_reaches_the_store() {
    // Terra 7 #8463, orchestrator #8480: the replay check and the insert
    // are one critical section, and the known-id check runs after it, so
    // copies released together reach the store once; the rest are replays.
    let f = fixture();
    let b = boot(&f);
    let pk = key_of("m-copies");
    let body = enroll_body("m-copies", &pk);
    let mut e = Req::post("/enroll", &b, &body)
        .without("idempotency-key")
        .without("mdkg-actor");
    e.keyid = "m-copies".into();
    e.sign_with = pk;
    let head = e.signed();
    let asked = std::sync::atomic::AtomicUsize::new(0);
    let start = std::sync::Barrier::new(8);
    let results: Vec<_> = std::thread::scope(|s| {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                s.spawn(|| {
                    start.wait();
                    f.gate
                        .authenticate_enroll(&head, &body, &|_| {
                            asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            Err(Refusal::new(
                                Code::ApiEnrollPending,
                                ErrorKind::Conflict,
                                "pending",
                            ))
                        })
                        .map(|_| ())
                        .map_err(|e| e.test_reason())
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|t| t.join().expect("thread"))
            .collect()
    });
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(results.iter().filter(|r| **r == Err("pending")).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == Err("nonce_reused"))
            .count(),
        7
    );
}

#[test]
fn a_host_key_request_over_serve_is_an_approved_machines() {
    // Orchestrator #8984 (revising #8901 item 5), Terra 2 #8979: a serve
    // listener never produces Local. The host key's request, on any
    // listener, is the host machine acting as an approved machine, its
    // verified person key kept.
    let f = fixture();
    let b = boot(&f);
    let gate = Arc::try_unwrap(f.gate)
        .expect("one owner")
        .with_host("m-ok", key_of("m-ok"))
        .expect("the host row matches");
    assert_eq!(gate.host(), Some("m-ok"));
    let host = gate
        .authenticate(&Req::get("/v1/nodes", &b).signed())
        .expect("host");
    assert_eq!(
        host.origin(),
        Origin::Remote {
            machine: "m-ok".into(),
            person: None,
        }
    );
    let signed = Req::get("/v1/nodes", &b).header("mdkg-actor", "human:ann");
    let with_person = gate
        .authenticate(&signed.person_signed("pk-ann", key_of("pk-ann")))
        .expect("person");
    // The whole verified key, its id and grade too (Terra 1 #9025).
    assert_eq!(
        with_person.origin(),
        Origin::Remote {
            machine: "m-ok".into(),
            person: Some(PersonKey {
                key_id: "pk-ann".into(),
                human: "human:ann".into(),
                machine_grade: true,
            }),
        }
    );
}

#[test]
fn a_host_row_with_another_key_refuses_the_start() {
    // #8901 item 5, kept by #8984: a start whose host row holds another
    // key, isn't approved, or is missing is refused.
    let mk = || Arc::try_unwrap(fixture().gate).expect("one owner");
    assert!(mk().with_host("m-ok", key_of("someone-else")).is_err());
    assert!(mk().with_host("m-revoked", key_of("m-revoked")).is_err());
    assert!(mk().with_host("m-unknown", key_of("m-unknown")).is_err());
    assert!(mk().with_host("m-ok", key_of("m-ok")).is_ok());
}

/// A backend for building contexts: nothing here applies an operation.
struct NoBackend;

impl mdkg_ops::Backend for NoBackend {
    fn kind(&self) -> mdkg_ops::BackendKind {
        mdkg_ops::BackendKind::Local
    }
    fn apply(
        &self,
        _: &mdkg_ops::Ctx,
        _: &'static mdkg_ops::registry::OpSpec,
        _: &mdkg_ops::Request,
    ) -> mdkg_ops::Report {
        mdkg_ops::Report::ok(serde_json::Value::Null)
    }
    fn stream(
        &self,
        _: &mdkg_ops::Ctx,
        _: &'static mdkg_ops::registry::OpSpec,
        _: &mdkg_ops::Request,
    ) -> Result<Box<dyn mdkg_ops::wire::EventStream>, mdkg_ops::Report> {
        Err(mdkg_ops::Report::ok(serde_json::Value::Null))
    }
}

fn parts(actor: &str) -> mdkg_ops::Parts {
    mdkg_ops::Parts {
        actor: actor.into(),
        agent: None,
        turn: None,
        transport: mdkg_ops::Transport::Http,
        idempotency_key: None,
        clock: mdkg_ops::Clock {
            now_ms: 0,
            lease_ttl_ms: 0,
        },
        trust: Default::default(),
        backend: Arc::new(NoBackend),
    }
}

#[test]
fn a_request_reaches_the_kernel_s_context_only_through_the_gate_s_verified() {
    // #10448, orchestrator #10750: the gate's whole S5 pass is the one way
    // into `Verified`, and `Ctx::remote` binds a person signature to the
    // actor, so a person-signed request acting as anyone else is refused.
    let f = fixture();
    let b = boot(&f);
    let body = b"{}";
    let r = Req::post("/v1/agents/a:retire", &b, body).header("mdkg-actor", "human:ann");
    let mut auth = f
        .gate
        .authenticate(&r.person_signed("pk-ann", key_of("pk-ann")))
        .expect("person");
    assert_eq!(auth.verified(), None, "not before the body is checked");
    f.gate.check_body(&mut auth, body).expect("digest");
    let verified = auth.verified().expect("verified");
    assert_eq!(verified.machine(), "m-ok");
    assert_eq!(verified.nonce(), r.nonce, "the nonce the gate used");
    assert_eq!(verified.person(), Some(("pk-ann", "human:ann")));
    let ctx = mdkg_ops::Ctx::remote(&verified, parts("human:ann")).expect("ann acts as ann");
    assert_eq!(ctx.machine(), "m-ok");
    assert_eq!(ctx.person(), Some("pk-ann"));
    assert_eq!(
        ctx.origin(),
        &mdkg_ops::Origin::Remote {
            machine: "m-ok".into()
        }
    );
    for actor in ["human:bob", "agent-a", "Human:ann"] {
        let refused = mdkg_ops::Ctx::remote(&verified, parts(actor)).err();
        assert_eq!(
            refused.map(|d| d.code),
            Some(Code::ApiActorNotAllowed),
            "{actor}"
        );
    }
    // A request without a person signature names any actor.
    let r = Req::post("/v1/agents/a:retire", &b, body);
    let mut auth = f.gate.authenticate(&r.signed()).expect("machine");
    f.gate.check_body(&mut auth, body).expect("digest");
    let verified = auth.verified().expect("verified");
    assert_eq!(verified.person(), None);
    assert!(mdkg_ops::Ctx::remote(&verified, parts("agent-a")).is_ok());
}
