use super::*;
use crate::codec::{Codec, HeadLimits, Http1, Response};
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::thread;

const T: Duration = Duration::from_secs(5);
const LIMITS: HeadLimits = HeadLimits {
    request_line: 1024,
    header_bytes: 4096,
    header_count: 32,
};

fn pair() -> (ClientConnection, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    (ClientConnection::new(Box::new(a)), b)
}

/// What serve's codec reads of `req`: the head and, by its length, the body.
fn served(req: Outgoing) -> (crate::codec::RequestHead, Vec<u8>) {
    let (mut client, peer) = pair();
    let server = thread::spawn(move || {
        let mut c = Http1.connection(Box::new(peer), LIMITS);
        let head = c.read_head(T, T).unwrap().unwrap();
        let len = head
            .headers
            .iter()
            .find(|(n, _)| n == "content-length")
            .map_or(0, |(_, v)| v.parse().unwrap());
        let body = c.read_body(len, T).unwrap();
        (head, body)
    });
    client.send(&req, T).unwrap();
    server.join().unwrap()
}

fn header<'a>(h: &'a crate::codec::RequestHead, name: &str) -> Option<&'a str> {
    h.headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn a_request_reads_back_through_the_server_codec() {
    let mut req = Outgoing::new("POST", "/v1/leases:claim", Some("a=1".into()));
    req.header("Mdkg-Graph", "g1").unwrap();
    req.body(br#"{"scope":"x"}"#.to_vec()).unwrap();
    let (head, body) = served(req);
    assert_eq!(
        (
            head.method.as_str(),
            head.path.as_str(),
            head.query.as_deref()
        ),
        ("POST", "/v1/leases:claim", Some("a=1"))
    );
    assert_eq!(header(&head, "host"), Some(HOST));
    assert_eq!(header(&head, "mdkg-graph"), Some("g1"));
    // S4: every body is JSON.
    assert_eq!(header(&head, "content-type"), Some("application/json"));
    assert_eq!(header(&head, "content-length"), Some("13"));
    assert!(header(&head, "content-digest").is_some());
    assert_eq!(body, br#"{"scope":"x"}"#);
    // A GET has no body and no framing for one; a bodiless POST says 0.
    let (head, _) = served(Outgoing::new("GET", "/v1/events", None));
    assert_eq!(header(&head, "content-length"), None);
    assert_eq!(header(&head, "content-type"), None);
    let (head, _) = served(Outgoing::new("POST", "/v1/x", None));
    assert_eq!(header(&head, "content-length"), Some("0"));
    // A JSON type with parameters, set by the caller, stays its own.
    let mut req = Outgoing::new("POST", "/v1/x", None);
    req.header("Content-Type", "application/json; charset=utf-8")
        .unwrap();
    req.body(b"{}".to_vec()).unwrap();
    let (head, _) = served(req);
    assert_eq!(
        header(&head, "content-type"),
        Some("application/json; charset=utf-8")
    );
}

#[test]
fn a_request_the_client_shouldnt_send_is_refused_before_a_byte() {
    let refused = |req: Outgoing| {
        let (mut client, mut peer) = pair();
        let err = client.send(&req, T).unwrap_err();
        drop(client);
        let mut got = Vec::new();
        peer.read_to_end(&mut got).unwrap();
        assert!(got.is_empty());
        err.kind()
    };
    let with = |name: &str, value: &str| {
        let mut r = Outgoing::new("POST", "/v1/x", None);
        r.headers.push((name.into(), value.into()));
        r
    };
    for (name, value) in [
        ("Host", "elsewhere"),
        ("Content-Length", "5"),
        ("Transfer-Encoding", "chunked"),
        ("Connection", "keep-alive"),
        ("Content-Type", "text/plain"),
        ("X-Split", "a\r\nInjected: 1"),
    ] {
        assert_eq!(
            refused(with(name, value)),
            io::ErrorKind::InvalidInput,
            "{name}"
        );
    }
    for (method, path, query) in [
        ("BREW", "/v1/x", None),
        // Terra 2 #10561: a HEAD response's length has no body after it.
        ("HEAD", "/v1/x", None),
        ("GET", "v1/x", None),
        ("GET", "/v1/a b", None),
        ("GET", "/v1/x#frag", None),
        ("GET", "/v1/x?q", None),
        ("GET", "/v1/x", Some("a b")),
    ] {
        let r = Outgoing::new(method, path, query.map(str::to_owned));
        assert_eq!(refused(r), io::ErrorKind::InvalidInput, "{method} {path}");
    }
    let mut get = Outgoing::new("GET", "/v1/x", None);
    get.body = b"{}".to_vec();
    assert_eq!(refused(get), io::ErrorKind::InvalidInput);
}

/// What the client reads of `response`, written raw by a peer that
/// then closes.
fn read(response: &[u8], max: usize) -> io::Result<(ResponseHead, Vec<u8>)> {
    let (mut client, mut peer) = pair();
    // From a thread: a response larger than the socket's buffer would
    // block a writer on the test's own thread. The client may refuse and
    // close before reading it all, so the write may fail.
    let response = response.to_vec();
    thread::spawn(move || {
        let _ = peer.write_all(&response);
        let _ = peer.shutdown(Shutdown::Write);
    });
    let head = client.read_head(T)?;
    let body = client.read_body(&head, max, T)?;
    Ok((head, body))
}

#[test]
fn a_response_is_read_by_its_length() {
    let (mut client, peer) = pair();
    let server = thread::spawn(move || {
        let mut c = Http1.connection(Box::new(peer), LIMITS);
        c.read_head(T, T).unwrap().unwrap();
        let r = Response {
            status: 200,
            headers: vec![("Mdkg-Boot".into(), "b1".into())],
            body: br#"{"ok":true}"#.to_vec(),
        };
        c.write_response(&r, true, T).unwrap();
    });
    client
        .send(&Outgoing::new("GET", "/v1/status", None), T)
        .unwrap();
    let head = client.read_head(T).unwrap();
    assert_eq!(head.status, 200);
    assert_eq!(head.get("mdkg-boot"), Some("b1"));
    assert!(head.closes());
    assert_eq!(client.read_body(&head, 1024, T).unwrap(), br#"{"ok":true}"#);
    drop(client);
    server.join().unwrap();
    // An interim 100 is skipped; 204 has no body.
    let (head, body) = read(
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n",
        16,
    )
    .unwrap();
    assert_eq!((head.status, body.len()), (204, 0));
}

#[test]
fn a_response_outside_the_rules_is_refused() {
    let bad = |raw: &[u8]| read(raw, 16).unwrap_err().kind();
    for raw in [
        &b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n"[..],
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\ncontent-length: 3\r\n\r\nhi",
        b"HTTP/1.1 200 OK\r\ncontent-length: +2\r\n\r\nhi",
        b"HTTP/1.1 200 OK\r\n\r\nno length",
        b"HTTP/1.1 200 OK\r\ncontent-length: 17\r\n\r\n",
        b"HTTP/1.0 200 OK\r\ncontent-length: 0\r\n\r\n",
        b"HTTP/1.1 200 OK\ncontent-length: 0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nA: b\r\n folded\r\ncontent-length: 0\r\n\r\n",
        b"HTTP/1.1 101 Switching Protocols\r\n\r\n",
    ] {
        assert_eq!(
            bad(raw),
            io::ErrorKind::InvalidData,
            "{}",
            String::from_utf8_lossy(raw)
        );
    }
    let huge = [
        b"HTTP/1.1 200 OK\r\nX: ".as_slice(),
        &vec![b'a'; MAX_RESPONSE_HEAD],
        b"\r\n\r\n",
    ]
    .concat();
    assert_eq!(bad(&huge), io::ErrorKind::InvalidData);
    // Cut part way: never a whole response.
    assert_eq!(
        bad(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhi"),
        io::ErrorKind::UnexpectedEof
    );
    assert_eq!(bad(b"HTTP/1.1 200 OK\r\n"), io::ErrorKind::UnexpectedEof);
}

#[test]
fn a_silent_host_times_out() {
    let (mut client, _peer) = pair();
    let started = Instant::now();
    let err = client.read_head(Duration::from_millis(200)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// A stream served by the codec: `frames` are each event's JSON `data`,
/// with its id; `end` closes cleanly when true, else cuts mid-event.
fn stream(frames: Vec<(Option<&'static str>, String)>, clean: bool) -> Events {
    let (mut client, peer) = pair();
    thread::spawn(move || {
        let mut c = Http1.connection(Box::new(peer.try_clone().unwrap()), LIMITS);
        c.read_head(T, T).unwrap().unwrap();
        c.start_stream(&[]).unwrap();
        for (id, data) in frames {
            c.write_event(id, "mdkg.event", &data).unwrap();
        }
        if clean {
            let _ = c.close_gracefully();
        } else {
            let mut raw = peer;
            raw.write_all(b"id: c1.99\ndata: {\"type\"").unwrap();
            raw.shutdown(Shutdown::Both).unwrap();
        }
    });
    client
        .send(&Outgoing::new("GET", "/v1/events", None), T)
        .unwrap();
    let head = client.read_head(T).unwrap();
    client.into_events(&head, T).unwrap()
}

#[test]
fn a_stream_drops_heartbeats_and_ends_with_its_error() {
    let event = r#"{"type":"mdkg.lease.claimed","cursor":"c1.1"}"#.to_owned();
    let beat = r#"{"type":"mdkg.stream.heartbeat","data":{"head":"c1.1"}}"#.to_owned();
    let error = r#"{"type":"mdkg.stream.error","data":{"head":"c1.1","diagnostics":[{"code":"api.cursor_reset","severity":"error","message":"restored","hint":"re-read"},{"code":"api.from_the_future","message":"new"}]}}"#.to_owned();
    let mut s = stream(
        vec![
            (Some("c1.1"), event.clone()),
            (Some("c1.1"), beat),
            (Some("c1.1"), error),
        ],
        true,
    );
    assert_eq!(
        s.next_event().unwrap(),
        Some(Event {
            id: "c1.1".into(),
            data: event
        })
    );
    let diags = s.next_event().unwrap_err();
    assert_eq!(diags[0].code, Code::ApiCursorReset);
    assert_eq!(diags[0].hint.as_deref(), Some("re-read"));
    // A code this build doesn't know keeps its name in the message, and
    // isn't taken for a network fault (Terra 2 #10561).
    assert_eq!(diags[1].code, Code::ApiUnsupported);
    assert!(diags[1].message.contains("api.from_the_future"));
}

#[test]
fn a_stream_closes_normally_only_on_a_boundary() {
    let event = r#"{"type":"mdkg.message.sent","cursor":"c1.2"}"#.to_owned();
    let mut s = stream(vec![(Some("c1.2"), event.clone())], true);
    assert_eq!(s.next_event().unwrap().unwrap().data, event);
    assert_eq!(s.next_event().unwrap(), None);
    assert_eq!(s.last_id(), Some("c1.2"));
    // Cut part way through an event: diagnostics, and the cursor stays
    // at the last whole event, so resuming misses nothing.
    let mut s = stream(vec![(Some("c1.2"), event)], false);
    s.next_event().unwrap().unwrap();
    let diags = s.next_event().unwrap_err();
    assert_eq!(diags[0].code, Code::ApiUnreachable);
    assert_eq!(s.last_id(), Some("c1.2"));
}

#[test]
fn only_an_event_stream_becomes_events() {
    let (mut client, mut peer) = pair();
    peer.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
    )
    .unwrap();
    let head = client.read_head(T).unwrap();
    assert_eq!(
        client.into_events(&head, T).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    // An event that isn't JSON, or has no type, breaks the stream.
    for data in ["not json", r#"{"cursor":"c1"}"#] {
        let mut s = stream(vec![(Some("c1"), data.to_owned())], true);
        assert_eq!(s.next_event().unwrap_err()[0].code, Code::ApiUnreachable);
    }
}

#[test]
fn a_silent_stream_times_out() {
    let (mut client, mut peer) = pair();
    peer.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
    )
    .unwrap();
    let head = client.read_head(T).unwrap();
    let mut s = client
        .into_events(&head, Duration::from_millis(200))
        .unwrap();
    let started = Instant::now();
    assert!(s.next_event().unwrap_err()[0].message.contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(peer);
}
