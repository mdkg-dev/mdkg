use std::net::{Ipv4Addr, Ipv6Addr};

use super::*;
use crate::auth::Authenticated;

fn head(headers: usize, value: usize, len: Option<&str>) -> RequestHead {
    let mut h: Vec<(String, String)> = (0..headers)
        .map(|i| (format!("x-{i}"), "v".repeat(value)))
        .collect();
    if let Some(n) = len {
        h.push(("content-length".into(), n.into()));
    }
    RequestHead {
        method: "POST".into(),
        path: "/v1/leases".into(),
        query: None,
        headers: h,
    }
}

fn reason(r: Result<(), Refusal>) -> &'static str {
    r.expect_err("refused").test_reason()
}

#[test]
fn sizes() {
    let l = Limits::default();
    l.check_head(&head(10, 10, Some("10")), None)
        .expect("small");
    assert_eq!(
        reason(l.check_head(&head(65, 1, None), None)),
        "header_count"
    );
    assert_eq!(
        reason(l.check_head(&head(2, 9000, None), None)),
        "header_bytes"
    );
    let over = (l.body_bytes + 1).to_string();
    assert_eq!(reason(l.check_head(&head(1, 1, Some(&over)), None)), "body");
    assert_eq!(
        reason(l.check_head(&head(1, 1, Some("4097")), Some(l.enroll_body_bytes))),
        "body"
    );
    let mut long = head(1, 1, None);
    long.path = "/".repeat(9000);
    let e = l.check_head(&long, None).expect_err("line");
    assert_eq!((e.test_reason(), e.status()), ("request_line", 413));
}

#[test]
fn a_configured_pool_always_leaves_authenticated_work_its_share() {
    // Claude #10013, orchestrator #9931: the pre-auth workers take at most
    // half the pool, and a pool under 2 is refused.
    let with = |n| {
        Limits::from_config(&LimitsConfig {
            connections: Some(n),
            ..Default::default()
        })
    };
    for n in [0, 1] {
        assert!(with(n).is_err(), "{n}");
    }
    for (n, preauth) in [(2, 1), (48, 24), (64, 32), (128, 64), (1000, 64)] {
        let l = with(n).expect("valid");
        assert_eq!((l.connections, l.tcp_preauth), (n, preauth), "{n}");
        assert!(l.connections - l.tcp_preauth >= 1);
    }
    assert_eq!(
        Limits::from_config(&LimitsConfig::default())
            .expect("default")
            .tcp_preauth,
        64
    );
}

#[test]
fn caps_compose_and_lengths_are_checked() {
    // Terra 3 #6700: a global cap below an endpoint's wins.
    let l = Limits::from_config(&LimitsConfig {
        body_bytes: Some(1024),
        ..Default::default()
    })
    .expect("valid");
    assert_eq!(
        reason(l.check_head(&head(1, 1, Some("2048")), Some(4096))),
        "body"
    );
    // A length past u64 is too large; a malformed or chunked one is bad.
    let e = l
        .check_head(&head(1, 1, Some("99999999999999999999999")), None)
        .expect_err("huge");
    assert_eq!((e.code, e.status()), (Code::ApiTooLarge, 413));
    let e = l
        .check_head(&head(1, 1, Some("12x")), None)
        .expect_err("bad");
    assert_eq!((e.test_reason(), e.status()), ("body_length", 400));
    let mut chunked = head(1, 1, None);
    chunked
        .headers
        .push(("transfer-encoding".into(), "chunked".into()));
    assert_eq!(reason(l.check_head(&chunked, None)), "body_length");
}

#[test]
fn a_head_of_exactly_16_kib_on_the_wire_passes() {
    // Terra 3 #6700: three headers that are 16 KiB as `name:value\r\n`.
    let l = Limits::default();
    let each = 16 * 1024 / 3;
    let mut h = head(0, 0, None);
    for (i, extra) in [0usize, 0, 16 * 1024 - 3 * each].iter().enumerate() {
        let name = format!("x-{i}");
        let value = "v".repeat(each + extra - name.len() - 3);
        h.headers.push((name, value));
    }
    let wire: usize = h
        .headers
        .iter()
        .map(|(n, v)| n.len() + 1 + v.len() + 2)
        .sum();
    assert_eq!(wire, 16 * 1024);
    l.check_head(&h, None).expect("at the limit");
    h.headers[0].1.push('v');
    assert_eq!(reason(l.check_head(&h, None)), "header_bytes");
}

#[test]
fn buckets_refill_and_say_when() {
    let t = Duration::from_secs(1000);
    let mut b = Bucket::new(2.0, 3.0, t);
    for _ in 0..3 {
        b.take(t).expect("burst");
    }
    assert_eq!(b.take(t), Err(1));
    b.take(t + Duration::from_millis(500))
        .expect("refilled one");
}

#[test]
fn rates_are_validated() {
    Rate::default().validate().expect("defaults");
    for bad in [
        Rate {
            writes_per_s: 0.0,
            ..Rate::default()
        },
        Rate {
            reads_per_s: -1.0,
            ..Rate::default()
        },
        Rate {
            read_burst: 0.5,
            ..Rate::default()
        },
        Rate {
            write_burst: f64::NAN,
            ..Rate::default()
        },
        Rate {
            writes_per_s: f64::INFINITY,
            ..Rate::default()
        },
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
    }
}

#[test]
fn rate_limits_per_machine_but_not_the_host() {
    let t = Duration::from_secs(10);
    let r = RateLimiter::new(Rate::default(), "host".into());
    for _ in 0..100 {
        r.admit("m", true, t).expect("write burst");
    }
    let e = r.admit("m", true, t).expect_err("limited");
    assert_eq!(
        (e.code, e.status(), e.retry_after),
        (Code::ApiRateLimited, 429, Some(1))
    );
    r.admit("m", false, t).expect("read");
    r.admit("m2", true, t).expect("other machine");
    for _ in 0..1000 {
        r.admit("host", true, t).expect("host is unlimited");
    }
    assert_eq!(r.drop_idle(t), 0, "both machines used tokens");
    assert_eq!(r.drop_idle(t + Duration::from_secs(60)), 2, "refilled");
}

#[test]
fn enrolment_is_five_per_minute_per_address_in_a_fixed_window() {
    // Decided, #6704: a fixed window, so no burst refills inside it.
    let t = Duration::from_secs(10);
    let e = EnrollLimiter::default();
    let a = Peer::Tcp(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
    for _ in 0..5 {
        e.admit(&a, t).expect("five");
    }
    assert_eq!(reason(e.admit(&a, t)), "enroll");
    // 48 s later the window still holds: nine in 48 s can't happen.
    assert_eq!(reason(e.admit(&a, t + Duration::from_secs(48))), "enroll");
    e.admit(&Peer::Tcp(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2))), t)
        .expect("another address");
    e.admit(&a, t + Duration::from_secs(60))
        .expect("next window");
}

#[test]
fn sources_are_addresses_not_ports() {
    // Terra 3 #6700: the source is the IP, never the port; an IPv6 peer
    // counts by its /64, and a v4-mapped one as v4.
    let v6 = |s: &str| Peer::Tcp(IpAddr::V6(s.parse::<Ipv6Addr>().expect("v6")));
    assert_eq!(
        v6("2001:db8:1:2::5").source(),
        v6("2001:db8:1:2:ffff::9").source()
    );
    assert_ne!(
        v6("2001:db8:1:2::5").source(),
        v6("2001:db8:1:3::5").source()
    );
    assert_eq!(v6("::ffff:192.0.2.1").source(), "192.0.2.1");
    assert_eq!(Peer::Unix.source(), "unix");
}

#[test]
fn a_peers_address_drops_only_the_port() {
    let v6: IpAddr = "2001:db8:1:2:3:4:5:6".parse().expect("v6");
    assert_eq!(Peer::Tcp(v6).address(), "2001:db8:1:2:3:4:5:6");
    let mapped: IpAddr = "::ffff:192.0.2.7".parse().expect("mapped");
    assert_eq!(Peer::Tcp(mapped).address(), "192.0.2.7");
    assert_eq!(Peer::Unix.address(), "unix");
}

#[test]
fn link_local_peers_on_two_interfaces_stay_apart() {
    // #7066: the scope id is kept, so `machine list --pending` can tell
    // fe80::1 on one interface from fe80::1 on another.
    let on = |scope: u32| {
        Peer::of(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
            "fe80::1".parse().expect("v6"),
            4000,
            0,
            scope,
        )))
    };
    assert_eq!(on(2).address(), "fe80::1%2");
    assert_eq!(on(3).address(), "fe80::1%3");
    assert_eq!(on(2).source(), "fe80:0:0:0::/64%2", "keeps its interface");
    assert_ne!(on(2).source(), on(3).source(), "one interface per source");
    let plain: std::net::SocketAddr = "[2001:db8::1]:4000".parse().expect("v6");
    assert_eq!(
        Peer::of(plain),
        Peer::Tcp("2001:db8::1".parse().expect("v6"))
    );
    let v4: std::net::SocketAddr = "192.0.2.7:4000".parse().expect("v4");
    assert_eq!(Peer::of(v4).address(), "192.0.2.7");
}

#[test]
fn the_source_table_is_bounded() {
    let t = Duration::from_secs(10);
    let e = EnrollLimiter {
        capacity: 2,
        ..EnrollLimiter::default()
    };
    let p = |n: u8| Peer::Tcp(IpAddr::V4(Ipv4Addr::new(192, 0, 2, n)));
    e.admit(&p(1), t).expect("1");
    e.admit(&p(2), t).expect("2");
    assert_eq!(reason(e.admit(&p(3), t)), "enroll");
    // Known sources keep their windows.
    e.admit(&p(1), t).expect("1 again");
    // Once the windows end, room comes back.
    e.admit(&p(3), t + Duration::from_secs(60)).expect("3");
}

#[test]
fn the_unix_socket_has_its_own_pool() {
    // Decided, #6704: TCP peers can't use up the local socket.
    let limits = Limits {
        connections: 4,
        tcp_preauth: 2,
        unix_connections: 1,
        ..Limits::default()
    };
    let caps = Arc::new(Caps::default());
    let _a = caps
        .connection(Listener::Tcp, &limits, Duration::ZERO)
        .expect("a");
    let _b = caps
        .connection(Listener::Tcp, &limits, Duration::ZERO)
        .expect("b");
    assert_eq!(
        caps.connection(Listener::Tcp, &limits, Duration::ZERO)
            .expect_err("pre-auth sub-cap")
            .test_reason(),
        "preauth"
    );
    let local = caps
        .connection(Listener::Unix, &limits, Duration::ZERO)
        .expect("local");
    assert!(
        caps.connection(Listener::Unix, &limits, Duration::ZERO)
            .is_err()
    );
    drop(local);
    caps.connection(Listener::Unix, &limits, Duration::ZERO)
        .expect("a local slot came back");
}

#[test]
fn authenticating_frees_the_preauth_slot_and_binds_the_machine() {
    let limits = Limits {
        connections: 4,
        tcp_preauth: 1,
        connections_per_machine: 1,
        ..Limits::default()
    };
    let caps = Arc::new(Caps::default());
    let mut a = caps
        .connection(Listener::Tcp, &limits, Duration::ZERO)
        .expect("a");
    assert_eq!(
        a.header_deadline(&limits, Duration::ZERO),
        limits.preauth_header_read
    );
    assert!(
        caps.connection(Listener::Tcp, &limits, Duration::ZERO)
            .is_err()
    );
    a.charge(&Authenticated::for_tests("m"), &limits, Duration::ZERO)
        .expect("charged");
    assert_eq!(
        a.header_deadline(&limits, Duration::ZERO),
        limits.header_read
    );
    let mut b = caps
        .connection(Listener::Tcp, &limits, Duration::ZERO)
        .expect("a pre-auth slot came back");
    // The machine's cap holds.
    assert_eq!(
        b.charge(&Authenticated::for_tests("m"), &limits, Duration::ZERO)
            .expect_err("per machine")
            .test_reason(),
        "machine_connections"
    );
    // A keep-alive request from another machine on a's connection.
    a.charge(&Authenticated::for_tests("m"), &limits, Duration::ZERO)
        .expect("same machine");
    assert_eq!(
        a.charge(&Authenticated::for_tests("n"), &limits, Duration::ZERO)
            .expect_err("another machine")
            .test_reason(),
        "machine_changed"
    );
    b.charge(&Authenticated::for_tests("n"), &limits, Duration::ZERO)
        .expect("n on its own connection");
    assert_eq!(a.machine(), Some("m"));
}

#[test]
fn streams_are_capped_per_machine() {
    let limits = Limits {
        streams_per_machine: 1,
        ..Limits::default()
    };
    let caps = Arc::new(Caps::default());
    let s = caps.stream("m", &limits).expect("stream");
    assert_eq!(
        caps.stream("m", &limits).expect_err("second").test_reason(),
        "streams"
    );
    caps.stream("n", &limits).expect("another machine");
    drop(s);
    caps.stream("m", &limits).expect("stream again");
}

#[test]
fn a_connection_that_never_authenticates_has_a_lifetime_and_no_keep_alive() {
    // Orchestrator #6822, Terra 4 #6786: 64 clients sending unsigned
    // `GET /` can't keep the pre-auth slots by renewing a header deadline.
    let limits = Limits::default();
    let caps = Arc::new(Caps::default());
    let t0 = Duration::from_secs(100);
    let mut c = caps.connection(Listener::Tcp, &limits, t0).expect("tcp");
    assert!(!c.keep_alive(), "closed after a public response");
    assert_eq!(c.header_deadline(&limits, t0), Duration::from_secs(5));
    assert_eq!(
        c.header_deadline(&limits, t0 + Duration::from_secs(7)),
        Duration::from_secs(3),
        "never past 10 s from accept"
    );
    assert_eq!(
        c.header_deadline(&limits, t0 + Duration::from_secs(11)),
        Duration::ZERO
    );
    c.charge(
        &Authenticated::for_tests("m"),
        &limits,
        t0 + Duration::from_secs(1),
    )
    .expect("authenticated");
    assert!(c.keep_alive());
    assert_eq!(
        c.header_deadline(&limits, t0 + Duration::from_secs(60)),
        limits.header_read
    );
    let u = caps.connection(Listener::Unix, &limits, t0).expect("unix");
    assert!(u.keep_alive());
    assert_eq!(
        u.header_deadline(&limits, t0 + Duration::from_secs(60)),
        limits.header_read
    );
}

#[test]
fn tcp_sources_cannot_crowd_the_unix_socket_out_of_the_source_table() {
    // Terra 4 #6786: the socket keeps its own room in the table.
    let t = Duration::from_secs(10);
    let e = EnrollLimiter {
        capacity: 2,
        ..EnrollLimiter::default()
    };
    let p = |n: u8| Peer::Tcp(IpAddr::V4(Ipv4Addr::new(192, 0, 2, n)));
    e.admit(&p(1), t).expect("1");
    e.admit(&p(2), t).expect("2");
    assert_eq!(reason(e.admit(&p(3), t)), "enroll");
    e.admit(&Peer::Unix, t).expect("the socket still gets in");
    // And the socket's entry doesn't take a TCP source's room.
    let e = EnrollLimiter {
        capacity: 2,
        ..EnrollLimiter::default()
    };
    e.admit(&Peer::Unix, t).expect("socket");
    e.admit(&p(1), t).expect("1");
    e.admit(&p(2), t).expect("2");
    assert_eq!(reason(e.admit(&p(3), t)), "enroll");
}

#[test]
fn a_preauth_body_is_bounded_by_the_lifetime_too() {
    // Opus #7109: a pre-auth body read slowly can't outlast 10 s from
    // accept; after authentication the body gets its full 30 s.
    let limits = Limits::default();
    let caps = Arc::new(Caps::default());
    let t0 = Duration::from_secs(100);
    let mut c = caps.connection(Listener::Tcp, &limits, t0).expect("tcp");
    assert_eq!(
        c.body_deadline(&limits, t0 + Duration::from_secs(4)),
        Duration::from_secs(6)
    );
    assert_eq!(
        c.body_deadline(&limits, t0 + Duration::from_secs(10)),
        Duration::ZERO
    );
    assert_eq!(c.preauth_deadline(), Some(t0 + Duration::from_secs(10)));
    c.charge(
        &Authenticated::for_tests("m"),
        &limits,
        t0 + Duration::from_secs(1),
    )
    .expect("authenticated");
    assert_eq!(c.preauth_deadline(), None);
    assert_eq!(
        c.body_deadline(&limits, t0 + Duration::from_secs(60)),
        limits.body_read
    );
    let u = caps.connection(Listener::Unix, &limits, t0).expect("unix");
    assert_eq!(
        u.body_deadline(&limits, t0 + Duration::from_secs(60)),
        limits.body_read
    );
}

#[test]
fn an_authentication_finished_after_the_lifetime_is_refused() {
    // Terra 2 #7123: a request that verified past 10 s from accept can't
    // promote its connection; it stays pre-auth and is closed.
    let limits = Limits::default();
    let caps = Arc::new(Caps::default());
    let t0 = Duration::from_secs(100);
    let mut late = caps.connection(Listener::Tcp, &limits, t0).expect("tcp");
    assert_eq!(
        late.charge(
            &Authenticated::for_tests("m"),
            &limits,
            t0 + Duration::from_secs(10)
        )
        .expect_err("expired")
        .test_reason(),
        "preauth_expired"
    );
    assert!(!late.keep_alive(), "still pre-auth");
    assert_eq!(late.machine(), None);
    let mut on_time = caps.connection(Listener::Tcp, &limits, t0).expect("tcp");
    on_time
        .charge(
            &Authenticated::for_tests("m"),
            &limits,
            t0 + Duration::from_secs(9),
        )
        .expect("within the lifetime");
    assert!(on_time.keep_alive());
}

#[test]
fn one_preauth_deadline_covers_tls_head_body_and_verification() {
    // Orchestrator #7129: one absolute deadline, 10 s from accept, for
    // every pre-auth step; each step's own limit within it.
    let limits = Limits::default();
    let caps = Arc::new(Caps::default());
    let t0 = Duration::from_secs(100);
    let c = caps.connection(Listener::Tcp, &limits, t0).expect("tcp");
    assert_eq!(c.preauth_deadline(), Some(t0 + Duration::from_secs(10)));
    // A TLS handshake with its own 10 s limit, started at 3 s: 7 s left.
    let tls = Duration::from_secs(10);
    assert_eq!(
        c.deadline(tls, t0 + Duration::from_secs(3)),
        Duration::from_secs(7)
    );
    // The head after it, at 6 s: the 5 s pre-auth read, cut to 4 s.
    assert_eq!(
        c.header_deadline(&limits, t0 + Duration::from_secs(6)),
        Duration::from_secs(4)
    );
    // The body at 8 s: 2 s left of its 30 s.
    assert_eq!(
        c.body_deadline(&limits, t0 + Duration::from_secs(8)),
        Duration::from_secs(2)
    );
    // Past the deadline every step is zero: close.
    assert_eq!(
        c.deadline(tls, t0 + Duration::from_secs(11)),
        Duration::ZERO
    );
    let u = caps.connection(Listener::Unix, &limits, t0).expect("unix");
    assert_eq!(u.preauth_deadline(), None);
    assert_eq!(u.deadline(tls, t0 + Duration::from_secs(60)), tls);
}

#[test]
fn a_connection_is_promoted_only_after_its_first_body_is_checked() {
    // Orchestrator #7678, Terra 5 #8175: the request the gate
    // authenticated is the one charged. Its head alone leaves the
    // connection pre-auth, with what is left of its 10 s for the body; a
    // body that fails its digest leaves it so; the body that matches
    // promotes it.
    let f = crate::auth::tests::fixture();
    let boot = f.gate.boot_id().expect("boot");
    let head = crate::auth::tests::Req::post("/v1/channels", &boot, b"{\"ok\":1}").signed();
    let limits = Limits::default();
    let caps = Arc::new(Caps::default());
    let t0 = Duration::from_secs(100);
    let mut c = caps.connection(Listener::Tcp, &limits, t0).expect("tcp");
    let at = t0 + Duration::from_secs(9);
    let mut auth = f.gate.authenticate(&head).expect("head");
    let unchecked = |c: &mut Conn, auth: &Authenticated| {
        c.charge(auth, &limits, at)
            .expect_err("body not checked")
            .test_reason()
    };
    assert_eq!(unchecked(&mut c, &auth), "body_unchecked");
    assert!(!c.keep_alive(), "still pre-auth");
    assert_eq!(c.body_deadline(&limits, at), Duration::from_secs(1));
    f.gate
        .check_body(&mut auth, b"{\"ok\":2}")
        .expect_err("another body");
    assert_eq!(unchecked(&mut c, &auth), "body_unchecked");
    f.gate
        .check_body(&mut auth, b"{\"ok\":1}")
        .expect("the signed body");
    c.charge(&auth, &limits, at + Duration::from_millis(500))
        .expect("promoted once the body checked out");
    assert!(c.keep_alive());
}

#[test]
fn the_sockets_first_request_needs_its_body_checked_too() {
    // Opus #8420: a Unix connection is never pre-auth, but its first
    // request binds it to a machine only once its body checked out.
    let limits = Limits::default();
    let caps = Arc::new(Caps::default());
    let at = Duration::from_secs(100);
    let mut c = caps.connection(Listener::Unix, &limits, at).expect("unix");
    assert_eq!(
        c.charge(&Authenticated::for_tests_head_only("m"), &limits, at)
            .expect_err("body not checked")
            .test_reason(),
        "body_unchecked"
    );
    c.charge(&Authenticated::for_tests("m"), &limits, at)
        .expect("bound");
    c.charge(&Authenticated::for_tests_head_only("m"), &limits, at)
        .expect("a later head");
}

#[test]
fn a_promoted_connection_charges_later_requests_on_their_heads() {
    // Opus #8152: after promotion a request is charged before its body is
    // read; another machine's is still refused as such.
    let limits = Limits::default();
    let caps = Arc::new(Caps::default());
    let t0 = Duration::from_secs(100);
    let mut c = caps.connection(Listener::Tcp, &limits, t0).expect("tcp");
    let at = t0 + Duration::from_secs(1);
    c.charge(&Authenticated::for_tests("m"), &limits, at)
        .expect("promoted");
    c.charge(&Authenticated::for_tests_head_only("m"), &limits, at)
        .expect("a later head");
    assert_eq!(
        c.charge(&Authenticated::for_tests_head_only("m2"), &limits, at)
            .expect_err("another machine")
            .test_reason(),
        "machine_changed"
    );
}
