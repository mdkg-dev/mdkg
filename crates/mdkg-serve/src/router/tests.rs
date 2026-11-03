use mdkg_ops::registry::{Keyed, Kind, Person, Route};

use super::*;

const fn op(name: &'static str, method: Method, path: &'static str, class: Class) -> OpSpec {
    OpSpec {
        name,
        kind: Kind::Write,
        capability: "persons",
        cli: &["x"],
        mcp: &[],
        http: Some(Route { method, path }),
        input_schema: "s",
        output_schema: "s",
        keyed: Keyed::Accepted,
        class,
        person: Person::Signed,
        remote_plan: false,
    }
}

static OPS: &[OpSpec] = &[
    op(
        "agents.retire",
        Method::Post,
        "/v1/agents/{name}:retire",
        Class::Operator,
    ),
    op("nodes.list", Method::Get, "/v1/nodes", Class::Any),
    op("actors.show", Method::Get, "/v1/actors/{id}", Class::Any),
    op(
        "store.migrate",
        Method::Post,
        "/v1/store:migrate",
        Class::Host,
    ),
    op(
        PERSON_APPROVE,
        Method::Post,
        "/v1/persons/{id}:approve",
        Class::Operator,
    ),
];

fn head(method: &str, path: &str) -> RequestHead {
    RequestHead {
        method: method.into(),
        path: path.into(),
        query: None,
        headers: vec![],
    }
}

fn reason(r: Result<Routed, Refusal>) -> (Code, &'static str) {
    let e = r.expect_err("refused");
    (e.code, e.test_reason())
}

#[test]
fn the_fixed_routes_and_the_registry() {
    let r = Router::new(OPS);
    assert_eq!(r.route(&head("GET", "/")), Ok(Routed::Root));
    assert_eq!(
        r.route(&head("POST", "/enroll")),
        Ok(Routed::Enroll(enroll::Route::Request))
    );
    assert_eq!(
        r.route(&head("POST", "/persons:enroll")),
        Ok(Routed::PersonEnroll)
    );
    assert_eq!(
        r.route(&head(
            "POST",
            "/persons/01a18800-0000-7000-8000-000000000003:prove"
        )),
        Ok(Routed::PersonProve(
            "01a18800-0000-7000-8000-000000000003".into()
        ))
    );
    match r.route(&head("POST", "/v1/agents/a-api:retire")) {
        Ok(Routed::Op { op, params }) => {
            assert_eq!(op.name, "agents.retire");
            assert_eq!(params, vec!["a-api".to_owned()]);
        }
        other => panic!("{other:?}"),
    }
    match r.route(&head("GET", "/v1/nodes")) {
        Ok(Routed::Op { op, params }) => {
            assert_eq!(op.name, "nodes.list");
            assert!(params.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn anything_else_is_unsupported() {
    // R42: no other route, method or shape is served.
    let r = Router::new(OPS);
    for (m, p) in [
        ("DELETE", "/v1/nodes"),
        ("POST", "/v1/nodes"),
        ("GET", "/v1/agents/a-api:retire"),
        ("POST", "/v1/agents/:retire"),
        ("POST", "/v1/agents/a:b:retire"),
        ("POST", "/v1/agents/a/b:retire"),
        ("GET", "/v1/nope"),
        ("POST", "/persons/../x:prove"),
    ] {
        assert_eq!(
            reason(r.route(&head(m, p))),
            (
                Code::ApiUnsupported,
                if m == "DELETE" { "method" } else { "route" }
            ),
            "{m} {p}"
        );
    }
}

#[test]
fn a_host_class_operation_is_refused_on_every_listener() {
    // Orchestrator #8984: host-class operations run only in-process; the
    // router refuses them before anything else is read, whoever signed.
    let r = Router::new(OPS);
    assert_eq!(
        reason(r.route(&head("POST", "/v1/store:migrate"))),
        (Code::ApiUnsupported, "host_class")
    );
}

#[test]
fn person_approve_needs_a_present_signature() {
    // #8901, #8835, Terra 3 #9112: a machine key alone, the host's
    // included, is refused; a machine-grade person signature is refused;
    // a Present one is the remote approval path.
    let r = Router::new(OPS);
    let routed = r
        .route(&head("POST", "/v1/persons/p1:approve"))
        .expect("routed");
    let machine_only = Authenticated::for_tests("m-ok");
    assert_eq!(
        r.admit(&routed, &machine_only)
            .expect_err("no person")
            .test_reason(),
        "person_signature"
    );
    let grade = |machine_grade| {
        Authenticated::for_tests_with_person(
            "m-ok",
            crate::auth::PersonKey {
                key_id: "pk".into(),
                human: "human:ann".into(),
                machine_grade,
            },
        )
    };
    assert_eq!(
        r.admit(&routed, &grade(true))
            .expect_err("machine grade")
            .test_reason(),
        "present_required"
    );
    r.admit(&routed, &grade(false)).expect("Present");
    // Other operations are the kernel's to judge.
    let other = r
        .route(&head("POST", "/v1/agents/a:retire"))
        .expect("routed");
    r.admit(&other, &machine_only)
        .expect("not the router's call");
}

fn op_name(r: &Router, path: &str) -> &'static str {
    match r.route(&head("GET", path)) {
        Ok(Routed::Op { op, .. }) => op.name,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_path_two_routes_match_is_refused_whatever_the_order() {
    // Orchestrator #9206, #9318: overlap is a bug `Router::new` catches;
    // should one reach a release build, which skips that check, a path
    // both routes match is refused and the registry's order never
    // decides. Built directly here, as such a build would.
    static SHOW_FIRST: &[OpSpec] = &[
        op("agents.show", Method::Get, "/v1/agents/{name}", Class::Any),
        op(
            "agents.search",
            Method::Get,
            "/v1/agents/search",
            Class::Any,
        ),
    ];
    static SEARCH_FIRST: &[OpSpec] = &[
        op(
            "agents.search",
            Method::Get,
            "/v1/agents/search",
            Class::Any,
        ),
        op("agents.show", Method::Get, "/v1/agents/{name}", Class::Any),
    ];
    for ops in [SHOW_FIRST, SEARCH_FIRST] {
        let r = Router { ops };
        assert_eq!(
            reason(r.route(&head("GET", "/v1/agents/search"))),
            (Code::ApiUnsupported, "ambiguous_route")
        );
        assert_eq!(op_name(&r, "/v1/agents/a1"), "agents.show");
        // The literal shadows an agent named `search`: an overlap.
        assert_eq!(overlaps(ops).len(), 1);
    }
    // A collection operation on the collection's `:verb` overlaps nothing.
    static ON_COLLECTION: &[OpSpec] = &[
        op("agents.show", Method::Get, "/v1/agents/{name}", Class::Any),
        op(
            "agents.search",
            Method::Get,
            "/v1/agents:search",
            Class::Any,
        ),
    ];
    assert!(overlaps(ON_COLLECTION).is_empty());
    let r = Router::new(ON_COLLECTION);
    assert_eq!(op_name(&r, "/v1/agents:search"), "agents.search");
    assert_eq!(op_name(&r, "/v1/agents/search"), "agents.show");
    // Literal against placeholder at any position (Opus #9256's pair).
    static CROSSED: &[OpSpec] = &[
        op("x.a", Method::Get, "/v1/x/{a}/y", Class::Any),
        op("x.b", Method::Get, "/v1/x/z/{b}", Class::Any),
    ];
    assert_eq!(overlaps(CROSSED), vec![("/v1/x/{a}/y", "/v1/x/z/{b}")]);
    assert_eq!(
        reason(Router { ops: CROSSED }.route(&head("GET", "/v1/x/z/y"))),
        (Code::ApiUnsupported, "ambiguous_route")
    );
    // Two literals that differ at some position share no path.
    static DISJOINT: &[OpSpec] = &[
        op("x.a", Method::Get, "/v1/x/{a}/y", Class::Any),
        op("x.b", Method::Get, "/v1/x/z/w", Class::Any),
    ];
    assert!(overlaps(DISJOINT).is_empty());
    static TIED: &[OpSpec] = &[
        op("a.one", Method::Get, "/v1/x/{a}", Class::Any),
        op("a.two", Method::Get, "/v1/x/{b}", Class::Any),
    ];
    assert_eq!(
        reason(Router { ops: TIED }.route(&head("GET", "/v1/x/1"))),
        (Code::ApiUnsupported, "ambiguous_route")
    );
    assert_eq!(overlaps(TIED).len(), 1);
}

#[test]
fn the_overlap_check_covers_the_fixed_routes() {
    static ON_PROVE: &[OpSpec] = &[op("p.x", Method::Post, "/persons/{id}:prove", Class::Any)];
    static ON_POLL: &[OpSpec] = &[op("e.x", Method::Get, "/enroll/{x}", Class::Any)];
    static ON_ROOT: &[OpSpec] = &[op("r.x", Method::Get, "/", Class::Any)];
    static DIFFERENT_VERB: &[OpSpec] =
        &[op("p.y", Method::Post, "/persons/{id}:revoke", Class::Any)];
    assert_eq!(overlaps(ON_PROVE).len(), 1);
    assert_eq!(overlaps(ON_POLL).len(), 1);
    assert_eq!(overlaps(ON_ROOT).len(), 1);
    assert!(overlaps(DIFFERENT_VERB).is_empty());
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "routes that match one path")]
fn building_a_router_over_overlapping_routes_panics() {
    static OVERLAP: &[OpSpec] = &[
        op(
            "a.one",
            Method::Post,
            "/v1/agents/{name}:retire",
            Class::Any,
        ),
        op(
            "a.two",
            Method::Post,
            "/v1/agents/special:retire",
            Class::Any,
        ),
    ];
    Router::new(OVERLAP);
}

#[test]
fn each_capture_is_decoded_once() {
    // Orchestrator #9262, #9318; Terra 1 #9192.
    let r = Router::new(OPS);
    let param = |path: &str| match r.route(&head("GET", path)) {
        Ok(Routed::Op { params, .. }) => params,
        other => panic!("{other:?}"),
    };
    assert_eq!(param("/v1/actors/human%3Anick"), vec!["human:nick"]);
    // Terra 5 #9293, orchestrator #9318: canonical paths encode `:` and
    // `/` inside identifiers; the resource's own rules judge the value.
    assert_eq!(param("/v1/actors/codex%3Apr19"), vec!["codex:pr19"]);
    assert_eq!(
        param("/v1/actors/agent%2Fserve-core"),
        vec!["agent/serve-core"]
    );
    assert_eq!(param("/v1/actors/a%2fb"), vec!["a/b"]);
    assert_eq!(param("/v1/actors/human%3anick"), vec!["human:nick"]);
    assert_eq!(param("/v1/actors/caf%C3%A9"), vec!["caf\u{e9}"]);
    // A second decode never happens: the kernel sees the literal `%3A`.
    assert_eq!(param("/v1/actors/human%253Anick"), vec!["human%3Anick"]);
    let refused = |path: &str| {
        let e = r.route(&head("GET", path)).expect_err(path);
        assert_eq!(e.code, Code::UsageInvalidArguments, "{path}");
        e.test_reason()
    };
    assert_eq!(refused("/v1/actors/a%00b"), "path_control");
    assert_eq!(refused("/v1/actors/a%0Ab"), "path_control");
    assert_eq!(refused("/v1/actors/a%7Fb"), "path_control");
    assert_eq!(refused("/v1/actors/a%2"), "path_escape");
    assert_eq!(refused("/v1/actors/a%"), "path_escape");
    assert_eq!(refused("/v1/actors/a%zzb"), "path_escape");
    assert_eq!(refused("/v1/actors/a%FFb"), "path_utf8");
    assert_eq!(refused("/v1/actors/%C3"), "path_utf8");
    // The `:verb` is split off the raw segment: an encoded colon is part
    // of the value, never a verb, and a raw one in a value is no route.
    match r.route(&head("POST", "/v1/agents/a%3Aretire:retire")) {
        Ok(Routed::Op { op, params }) => {
            assert_eq!(op.name, "agents.retire");
            assert_eq!(params, vec!["a:retire"]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        reason(r.route(&head("POST", "/v1/agents/a%3Aretire"))),
        (Code::ApiUnsupported, "route")
    );
}

#[test]
fn a_fixed_route_s_id_is_decoded_as_any_capture() {
    // Terra 4 #9403: `%30` is `0` on the fixed routes too, and auth's
    // pending-key route check agrees with the router.
    let r = Router::new(OPS);
    let id = "01a18800-0000-7000-8000-000000000003";
    let encoded = "%301a18800-0000-7000-8000-000000000003";
    assert_eq!(
        r.route(&head("POST", &format!("/persons/{encoded}:prove"))),
        Ok(Routed::PersonProve(id.into()))
    );
    for (m, p, want) in [
        (
            "POST",
            format!("/enroll/{encoded}:reveal"),
            enroll::Route::Reveal(id.into()),
        ),
        (
            "GET",
            format!("/enroll/{encoded}"),
            enroll::Route::Poll(id.into()),
        ),
    ] {
        assert_eq!(r.route(&head(m, &p)), Ok(Routed::Enroll(want.clone())));
        assert_eq!(enroll::Route::of(&head(m, &p)), Some(want));
    }
    // A malformed escape is the decoder's refusal; an id that decodes to
    // something the id rules refuse is no route.
    let e = r.route(&head("GET", "/enroll/%zz")).expect_err("malformed");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::UsageInvalidArguments, "path_escape")
    );
    assert_eq!(enroll::Route::of(&head("GET", "/enroll/%zz")), None);
    for p in ["/enroll/a%2Fb", "/enroll/a%3Ab", "/enroll/a%20b"] {
        assert_eq!(
            reason(r.route(&head("GET", p))),
            (Code::ApiUnsupported, "route"),
            "{p}"
        );
        assert_eq!(enroll::Route::of(&head("GET", p)), None, "{p}");
    }
}

#[test]
fn a_registry_route_over_a_fixed_one_is_ambiguous_past_a_release_build() {
    // Terra 4 #9403, Terra 1 #9429: fixed and registry routes are matched
    // together, so the fixed one is never taken first.
    static ON_ENROLL: &[OpSpec] = &[op("p.e", Method::Post, "/persons:enroll", Class::Any)];
    static ON_POLL: &[OpSpec] = &[op("e.f", Method::Get, "/enroll/foo", Class::Any)];
    assert_eq!(
        reason(Router { ops: ON_ENROLL }.route(&head("POST", "/persons:enroll"))),
        (Code::ApiUnsupported, "ambiguous_route")
    );
    assert_eq!(
        reason(Router { ops: ON_POLL }.route(&head("GET", "/enroll/foo"))),
        (Code::ApiUnsupported, "ambiguous_route")
    );
    assert_eq!(overlaps(ON_ENROLL).len(), 1);
    assert_eq!(overlaps(ON_POLL).len(), 1);
}

#[test]
fn served_templates_never_overlap() {
    // Orchestrator #9206: no two routes of the real registry, or one and
    // a fixed route, match one path. Empty today, it keeps any entry
    // that fills it honest.
    assert_eq!(
        overlaps(mdkg_ops::registry::OPS),
        Vec::<(&str, &str)>::new()
    );
    Router::new(mdkg_ops::registry::OPS);
    assert!(overlaps(OPS).is_empty(), "the test registry");
}
