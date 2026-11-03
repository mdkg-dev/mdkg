use super::*;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;

const LIMITS: HeadLimits = HeadLimits {
    request_line: 256,
    header_bytes: 512,
    header_count: 8,
};
const T: Duration = Duration::from_secs(5);

/// The lingering read when a connection ends, short here: most tests'
/// peers are on the test's own thread and close only after it.
const TEST_LINGER: Duration = Duration::from_millis(100);

fn conn() -> (Box<dyn HttpConnection>, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    let c = Connection::new(Box::new(a), LIMITS).with_linger(TEST_LINGER);
    (Box::new(c), b)
}

fn head_of(bytes: &[u8]) -> Result<Option<RequestHead>, ReadError> {
    let (mut c, mut peer) = conn();
    peer.write_all(bytes).unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    c.read_head(T, T)
}

fn malformed(bytes: &[u8]) -> &'static str {
    match head_of(bytes) {
        Err(ReadError::Malformed { reason: w, .. }) => w,
        other => panic!("expected malformed, got {other:?}"),
    }
}

#[test]
fn reads_heads_and_bodies_in_turn_then_a_clean_close() {
    let (mut c, mut peer) = conn();
    peer.write_all(
        b"POST /v1/leases?x=1 HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\nX-A:  pad  \r\n\r\nhelloGET /v1/status HTTP/1.1\r\nhost: h\r\n\r\n",
    )
    .unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    let h = c.read_head(T, T).unwrap().unwrap();
    assert_eq!(h.method, "POST");
    assert_eq!(h.path, "/v1/leases");
    assert_eq!(h.query.as_deref(), Some("x=1"));
    assert_eq!(
        h.headers,
        [
            ("host".into(), "h".into()),
            ("content-length".into(), "5".into()),
            ("x-a".into(), "pad".into()),
        ]
    );
    // The body bytes read with the head are kept for this body only.
    assert_eq!(c.read_body(5, T).unwrap(), b"hello");
    let h = c.read_head(T, T).unwrap().unwrap();
    assert_eq!((h.method.as_str(), h.query), ("GET", None));
    assert!(c.read_head(T, T).unwrap().is_none());
}

#[test]
fn a_body_read_with_its_head_may_hold_any_byte() {
    // Claude #9693: a body arriving in the same read as its head (a
    // pretty-printed JSON body, a binary one) is not checked as head
    // bytes, and a second request behind it reads normally.
    let (mut c, mut peer) = conn();
    let mut bytes = b"POST / HTTP/1.1\r\nContent-Length: 6\r\n\r\n".to_vec();
    bytes.extend_from_slice(b"a\nb\0\rc");
    bytes.extend_from_slice(b"POST /2 HTTP/1.1\r\nContent-Length: 3\r\n\r\n{\n}");
    peer.write_all(&bytes).unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/");
    assert_eq!(c.read_body(6, T).unwrap(), b"a\nb\0\rc");
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/2");
    assert_eq!(c.read_body(3, T).unwrap(), b"{\n}");
    assert!(c.read_head(T, T).unwrap().is_none());
}

#[test]
fn the_path_keeps_its_encoding_and_a_bare_query_is_empty() {
    let h = head_of(b"GET /a%2Fb%20c? HTTP/1.1\r\n\r\n")
        .unwrap()
        .unwrap();
    assert_eq!(h.path, "/a%2Fb%20c");
    assert_eq!(h.query.as_deref(), Some(""));
}

#[test]
fn bodies_are_never_dechunked_or_judged() {
    // serve judges Transfer-Encoding and Content-Length; the codec passes
    // the headers through and leaves the body bytes unread.
    let (mut c, mut peer) = conn();
    peer.write_all(
        b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 3\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
    )
    .unwrap();
    let h = c.read_head(T, T).unwrap().unwrap();
    assert!(
        h.headers
            .iter()
            .any(|(n, v)| n == "transfer-encoding" && v == "chunked")
    );
    assert_eq!(c.read_body(3, T).unwrap(), b"5\r\n");
}

#[test]
fn malformed_heads_are_refused() {
    assert_eq!(malformed(b"GET / HTTP/1.1\nHost: h\r\n\r\n"), "bare LF");
    assert_eq!(
        malformed(b"GET / HTTP/1.1\r\nHost: h\rX\r\n\r\n"),
        "bare CR"
    );
    assert_eq!(malformed(b"GET / HTTP/1.1\r\nHost: h\0\r\n\r\n"), "NUL");
    assert_eq!(
        malformed(b"GET / HTTP/1.1\r\nA: b\r\n c\r\n\r\n"),
        "obs-fold"
    );
    assert_eq!(malformed(b"GET / HTTP/1.0\r\n\r\n"), "version");
    assert_eq!(malformed(b"GET http://h/ HTTP/1.1\r\n\r\n"), "target");
    assert_eq!(malformed(b"GET * HTTP/1.1\r\n\r\n"), "target");
    assert_eq!(malformed(b"GET /a#b HTTP/1.1\r\n\r\n"), "target");
    assert_eq!(
        malformed(b"GET / HTTP/1.1\r\nA: \x7f\r\n\r\n"),
        "header value"
    );
    assert_eq!(
        malformed(b"GET / HTTP/1.1\r\nA: \xc3\xa9\r\n\r\n"),
        "header value"
    );
    assert!(matches!(head_of(b"GET /"), Err(ReadError::Truncated)));
}

#[test]
fn limits_are_checked_while_reading_before_any_body() {
    let long_line = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(300));
    assert!(matches!(
        head_of(long_line.as_bytes()),
        Err(ReadError::TooLarge {
            reason: "request_line",
            ..
        })
    ));
    // Even with no line end yet: refused as soon as it passes the limit.
    let (mut c, mut peer) = conn();
    peer.write_all("G".repeat(300).as_bytes()).unwrap();
    assert!(matches!(
        c.read_head(T, T),
        Err(ReadError::TooLarge {
            reason: "request_line",
            ..
        })
    ));
    let big = format!("GET / HTTP/1.1\r\nA: {}\r\n\r\n", "b".repeat(600));
    assert!(matches!(
        head_of(big.as_bytes()),
        Err(ReadError::TooLarge {
            reason: "header_bytes",
            ..
        })
    ));
    let mut many = String::from("GET / HTTP/1.1\r\n");
    for i in 0..9 {
        many.push_str(&format!("h{i}: v\r\n"));
    }
    many.push_str("\r\n");
    assert!(matches!(
        head_of(many.as_bytes()),
        Err(ReadError::TooLarge {
            reason: "header_count",
            ..
        })
    ));
    // Exactly at the limits is fine.
    let mut eight = String::from("GET / HTTP/1.1\r\n");
    for i in 0..8 {
        eight.push_str(&format!("h{i}: v\r\n"));
    }
    eight.push_str("\r\n");
    assert_eq!(head_of(eight.as_bytes()).unwrap().unwrap().headers.len(), 8);
}

#[test]
fn a_head_never_reads_past_its_limits() {
    // A peer that sends a huge head without a blank line: the codec reads
    // at most the limits' worth before refusing.
    let (mut c, mut peer) = conn();
    let w = thread::spawn(move || {
        let _ = peer.write_all(b"GET / HTTP/1.1\r\n");
        let _ = peer.write_all(&b"a".repeat(1 << 20));
        peer
    });
    assert!(matches!(
        c.read_head(T, T),
        Err(ReadError::TooLarge {
            reason: "header_bytes",
            ..
        })
    ));
    drop(c);
    let _ = w.join();
}

#[test]
fn heads_and_bodies_must_arrive_in_time() {
    // Idle: no first byte.
    let (mut c, _peer) = conn();
    assert!(matches!(
        c.read_head(Duration::from_millis(100), Duration::from_millis(100)),
        Err(ReadError::TimedOut)
    ));
    // Trickling a byte at a time can't stretch the head past its timeout.
    let (mut c, mut peer) = conn();
    let w = thread::spawn(move || {
        for b in b"GET / HTTP/1.1\r\nHost: h\r\n" {
            if peer.write_all(&[*b]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(40));
        }
    });
    let started = Instant::now();
    assert!(matches!(
        c.read_head(Duration::from_millis(300), Duration::from_millis(300)),
        Err(ReadError::TimedOut)
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(c);
    w.join().unwrap();
    // A short body is truncated; a slow one times out.
    let (mut c, mut peer) = conn();
    peer.write_all(b"POST / HTTP/1.1\r\n\r\nab").unwrap();
    c.read_head(T, T).unwrap().unwrap();
    assert!(matches!(
        c.read_body(5, Duration::from_millis(100)),
        Err(ReadError::TimedOut)
    ));
    let (mut c, mut peer) = conn();
    peer.write_all(b"POST / HTTP/1.1\r\n\r\nab").unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    c.read_head(T, T).unwrap().unwrap();
    assert!(matches!(c.read_body(5, T), Err(ReadError::Truncated)));
}

#[test]
fn a_closer_on_another_thread_unblocks_a_read() {
    let (mut c, _peer) = conn();
    let closer = c.closer().unwrap();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        closer.close();
    });
    let started = Instant::now();
    let r = c.read_head(Duration::from_secs(30), Duration::from_secs(30));
    assert!(started.elapsed() < Duration::from_secs(5), "{r:?}");
    assert!(!matches!(r, Ok(Some(_))));
    t.join().unwrap();
}

fn written(f: impl FnOnce(&mut dyn HttpConnection) -> io::Result<()>) -> io::Result<String> {
    let (mut c, mut peer) = conn();
    f(c.as_mut())?;
    drop(c);
    let mut out = String::new();
    peer.read_to_string(&mut out).unwrap();
    Ok(out)
}

#[test]
fn responses_are_framed_and_checked() {
    let r = Response {
        status: 200,
        headers: vec![("Mdkg-Api".into(), "v1".into())],
        body: b"{}".to_vec(),
    };
    assert_eq!(
        written(|c| c.write_response(&r, false, T)).unwrap(),
        "HTTP/1.1 200 OK\r\nMdkg-Api: v1\r\ncontent-length: 2\r\n\r\n{}"
    );
    assert_eq!(
        written(|c| c.write_response(&r, true, T)).unwrap(),
        "HTTP/1.1 200 OK\r\nMdkg-Api: v1\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
    );
    for bad in [
        ("X-A", "v\r\nX-B: smuggled"),
        ("Content-Length", "9"),
        ("Transfer-Encoding", "chunked"),
        ("Bad Name", "v"),
        ("", "v"),
    ] {
        let r = Response {
            status: 200,
            headers: vec![(bad.0.into(), bad.1.into())],
            body: vec![],
        };
        assert_eq!(
            written(|c| c.write_response(&r, false, T))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput,
            "{bad:?}"
        );
    }
    // No Content-Length, and no body, on 1xx, 204 and 304.
    let empty = |status| Response {
        status,
        headers: vec![],
        body: vec![],
    };
    assert_eq!(
        written(|c| c.write_response(&empty(204), false, T)).unwrap(),
        "HTTP/1.1 204 No Content\r\n\r\n"
    );
    assert!(
        !written(|c| c.write_response(&empty(304), false, T))
            .unwrap()
            .contains("content-length")
    );
    let with_body = Response {
        status: 204,
        headers: vec![],
        body: b"x".to_vec(),
    };
    assert!(written(|c| c.write_response(&with_body, false, T)).is_err());
    let r = Response {
        status: 99,
        headers: vec![],
        body: vec![],
    };
    assert!(written(|c| c.write_response(&r, false, T)).is_err());
}

#[test]
fn sse_frames() {
    let out = written(|c| {
        c.start_stream(&[("Mdkg-Api".into(), "v1".into())])?;
        c.write_event(Some("42"), "mdkg.event", "{\"a\":1}")?;
        c.write_event(None, "mdkg.event", "line one\nline two")?;
        c.write_comment("keep-alive")
    })
    .unwrap();
    assert_eq!(
        out,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-store\r\n\
         Mdkg-Api: v1\r\nconnection: close\r\n\r\n\
         id: 42\nevent: mdkg.event\ndata: {\"a\":1}\n\n\
         event: mdkg.event\ndata: line one\ndata: line two\n\n\
         : keep-alive\n\n"
    );
    // Nothing can end a frame early.
    assert!(sse_event(Some("4\n2"), "e", "d").is_err());
    assert!(sse_event(None, "e\nid: 9", "d").is_err());
    assert!(sse_event(None, "", "d").is_err());
    assert!(sse_event(None, "e", "a\rb").is_err());
    assert!(written(|c| c.write_comment("a\nb")).is_err());
}

/// An `Io` returning exactly the chunks it was given, one per read, so
/// split and coalesced reads are deterministic.
struct Scripted(std::collections::VecDeque<Vec<u8>>);

struct NoClose;
impl Closer for NoClose {
    fn close(&self) {}
}

impl Read for Scripted {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(mut chunk) = self.0.pop_front() else {
            return Ok(0);
        };
        let n = chunk.len().min(buf.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        if n < chunk.len() {
            chunk.drain(..n);
            self.0.push_front(chunk);
        }
        Ok(n)
    }
}

impl Write for Scripted {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Io for Scripted {
    fn write_by(&mut self, buf: &[u8], _: Instant) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
    fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
    fn closer(&self) -> io::Result<Box<dyn Closer>> {
        Ok(Box::new(NoClose))
    }
}

fn scripted(chunks: &[&[u8]], limits: HeadLimits) -> Box<dyn HttpConnection> {
    let q = chunks.iter().map(|c| c.to_vec()).collect();
    Http1.connection(Box::new(Scripted(q)), limits)
}

#[test]
fn a_head_exactly_at_its_limits_passes_however_its_crlfs_split() {
    // Terra 4 #9744, Terra 3 #9769 #3: a CR waiting for its LF isn't
    // content.
    let limits = HeadLimits {
        request_line: 16,
        header_bytes: 10,
        header_count: 2,
    };
    let line = b"GET /aaaa HTTP/1.1"; // 18: over 16
    assert!(matches!(
        scripted(&[line, b"\r\n\r\n"], limits).read_head(T, T),
        Err(ReadError::TooLarge {
            reason: "request_line",
            ..
        })
    ));
    // A request line of exactly 16 and a header block of exactly 10
    // bytes ("a: b\r\n" and "c:\r\n"), each CRLF split across reads.
    let line = b"GET /aa HTTP/1.1";
    let mut c = scripted(
        &[line, b"\r", b"\na: b\r", b"\nc:\r\n", b"\r", b"\n"],
        limits,
    );
    let h = c.read_head(T, T).unwrap().unwrap();
    assert_eq!(h.headers.len(), 2);
    // One byte more in the block is refused.
    let mut c = scripted(&[line, b"\r\na: bc\r\n", b"d:\r\n", b"\r\n"], limits);
    assert!(matches!(
        c.read_head(T, T),
        Err(ReadError::TooLarge {
            reason: "header_bytes",
            ..
        })
    ));
}

#[test]
fn a_headerless_head_isnt_charged_for_the_next_request() {
    // Terra 3 #9769 #2: a second request read in the same chunk doesn't
    // count against the first head's limits.
    let limits = HeadLimits {
        request_line: 64,
        header_bytes: 32,
        header_count: 2,
    };
    let mut both = b"GET /1 HTTP/1.1\r\n\r\nGET /2 HTTP/1.1\r\n".to_vec();
    for i in 0..2 {
        both.extend_from_slice(format!("h{i}: v\r\n").as_bytes());
    }
    both.extend_from_slice(b"\r\n");
    let mut c = scripted(&[&both], limits);
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/1");
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/2");
}

#[test]
fn read_ahead_is_bounded_and_buffered_body_bytes_are_never_parsed() {
    // Orchestrator's call on Terra 4 #9744: a head and its body in one
    // read yield the head; the body (here shaped like a request) is
    // returned by read_body exactly, and never parsed as a head.
    let body = b"GET /evil HTTP/1.1\r\n\r\n";
    let mut one = format!("POST / HTTP/1.1\r\ncontent-length: {}\r\n\r\n", body.len()).into_bytes();
    one.extend_from_slice(body);
    let mut c = scripted(&[&one], LIMITS);
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/");
    assert_eq!(c.read_body(body.len() as u64, T).unwrap(), body);
    assert!(c.read_head(T, T).unwrap().is_none());
    // A read asks for no more than the head limits allow: a peer offering
    // 1 MiB in one read gets at most request_line + header_bytes + 4
    // bytes taken.
    let (mut c, mut peer) = conn();
    let mut big = b"POST / HTTP/1.1\r\n\r\n".to_vec();
    big.extend_from_slice(&vec![b'x'; 1 << 20]);
    let w = thread::spawn(move || {
        let _ = peer.write_all(&big);
        peer
    });
    c.read_head(T, T).unwrap().unwrap();
    // What was read ahead is all body; asking for more than that reads on.
    let max = LIMITS.request_line + LIMITS.header_bytes + 4;
    let got = c.read_body(max as u64, T).unwrap();
    assert_eq!(got, vec![b'x'; max]);
    drop(c);
    let _ = w.join();
}

#[test]
fn a_head_trickled_a_byte_at_a_time_is_scanned_once() {
    // Terra 4 #9744: work stays proportional to the bytes received.
    let limits = HeadLimits {
        request_line: 64,
        header_bytes: 64 * 1024,
        header_count: 4096,
    };
    let mut head = b"GET / HTTP/1.1\r\n".to_vec();
    while head.len() < 60 * 1024 {
        head.extend_from_slice(b"x-pad: 0123456789abcdef\r\n");
    }
    head.extend_from_slice(b"\r\n");
    let chunks: Vec<&[u8]> = head.chunks(1).collect();
    let started = Instant::now();
    let h = scripted(&chunks, limits).read_head(T, T).unwrap().unwrap();
    assert!(h.headers.len() > 2000);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_peer_that_stops_reading_fails_the_write_in_time() {
    // Terra 4 #9744, Terra 1 #9908: an SSE write to a peer that never
    // reads fails within the frame's timeout; the writer runs under a
    // bounded wait so a hang fails the test instead of stalling it.
    let (a, peer) = UnixStream::pair().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    thread::spawn(move || {
        let mut c =
            Connection::new(Box::new(a), LIMITS).with_write_timeout(Duration::from_millis(200));
        c.start_stream(&[]).unwrap();
        let data = "x".repeat(64 * 1024);
        let err = loop {
            if let Err(e) = c.write_event(None, "e", &data) {
                break e;
            }
        };
        let _ = tx.send(err.kind());
    });
    let kind = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the write returned");
    assert!(
        matches!(kind, io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut),
        "{kind:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(WRITE_TIMEOUT, Duration::from_secs(30));
    drop(peer);
}

#[test]
fn a_head_request_gets_a_bodiless_405_and_the_connection_closes() {
    // Orchestrator's call on Terra 3 #9769: serve doesn't support HEAD;
    // the listener answers 405 with no body and closes.
    let (mut c, mut peer) = conn();
    peer.write_all(b"HEAD /v1/status HTTP/1.1\r\nHost: h\r\n\r\n")
        .unwrap();
    let h = c.read_head(T, T).unwrap().unwrap();
    assert_eq!(h.method, "HEAD");
    let r = Response {
        status: 405,
        headers: vec![("Allow".into(), "GET".into())],
        body: vec![],
    };
    c.write_response(&r, true, T).unwrap();
    drop(c);
    let mut out = String::new();
    peer.read_to_string(&mut out).unwrap();
    assert_eq!(
        out,
        "HTTP/1.1 405 Method Not Allowed\r\nAllow: GET\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
}

#[test]
fn stream_headers_and_205_framing() {
    // Terra 4 #9744: the stream's own headers can't be overridden.
    for h in ["Content-Type", "cache-control"] {
        assert!(written(|c| c.start_stream(&[(h.into(), "x".into())])).is_err());
    }
    // Terra 3 #9769 #4: 205 carries content-length: 0 and no body.
    let r = Response {
        status: 205,
        headers: vec![],
        body: vec![],
    };
    assert_eq!(
        written(|c| c.write_response(&r, false, T)).unwrap(),
        "HTTP/1.1 205 Reset Content\r\ncontent-length: 0\r\n\r\n"
    );
    let r = Response {
        status: 205,
        headers: vec![],
        body: b"x".to_vec(),
    };
    assert!(written(|c| c.write_response(&r, false, T)).is_err());
}

#[test]
fn a_response_to_a_peer_reading_a_little_at_a_time_fails_in_time() {
    // Claude #9884, Terra 1 #9908: the timeout is the whole response's,
    // however the peer's reads let partial writes through.
    let (a, mut peer) = UnixStream::pair().unwrap();
    let reader = thread::spawn(move || {
        let mut b = [0u8; 64];
        for _ in 0..200 {
            if peer.read(&mut b).unwrap_or(0) == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let mut c = Http1.connection(Box::new(a), LIMITS);
    let r = Response {
        status: 200,
        headers: vec![],
        body: vec![b'x'; 4 << 20],
    };
    let started = Instant::now();
    let err = c
        .write_response(&r, false, Duration::from_millis(300))
        .unwrap_err();
    assert!(
        matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ),
        "{err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    drop(c);
    reader.join().unwrap();
}

#[test]
fn the_first_byte_and_the_whole_head_have_their_own_timeouts() {
    // serve-core's read_head(first_byte, whole).
    let (mut c, _peer) = conn();
    let started = Instant::now();
    assert!(matches!(
        c.read_head(Duration::from_millis(100), Duration::from_secs(10)),
        Err(ReadError::TimedOut)
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    // A first byte in time, the rest trickled past `whole`.
    let (mut c, mut peer) = conn();
    let w = thread::spawn(move || {
        for b in b"GET / HTTP/1.1\r\nHost: h\r\n" {
            if peer.write_all(&[*b]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(40));
        }
    });
    let started = Instant::now();
    assert!(matches!(
        c.read_head(Duration::from_secs(5), Duration::from_millis(300)),
        Err(ReadError::TimedOut)
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(c);
    w.join().unwrap();
}

#[test]
fn a_refused_heads_request_is_marked() {
    // serve-core's ReadError { reason, head }: a HEAD's refusal carries
    // no body.
    let m = |bytes: &[u8]| match head_of(bytes) {
        Err(ReadError::Malformed { reason, head }) => (reason, head),
        Err(ReadError::TooLarge { reason, head }) => (reason, head),
        other => panic!("{other:?}"),
    };
    assert_eq!(m(b"HEAD / HTTP/1.0\r\n\r\n"), ("version", true));
    assert_eq!(m(b"GET / HTTP/1.0\r\n\r\n"), ("version", false));
    let long = format!("HEAD /{} HTTP/1.1\r\n\r\n", "a".repeat(300));
    assert_eq!(m(long.as_bytes()), ("request_line", true));
}

#[test]
fn a_body_with_stray_bytes_in_the_heads_own_read_is_left_alone() {
    // Claude #9884 #2: the #9693 failure, pinned with one deterministic
    // read carrying the head and a body holding LF, NUL and CR.
    let mut one = b"POST / HTTP/1.1\r\ncontent-length: 6\r\n\r\n".to_vec();
    one.extend_from_slice(b"a\nb\0\rc");
    let mut c = scripted(&[&one], LIMITS);
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/");
    assert_eq!(c.read_body(6, T).unwrap(), b"a\nb\0\rc");
}

/// A scripted `Io` that counts the bytes it has handed out.
struct Counted(Scripted, std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(buf)?;
        self.1.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
        Ok(n)
    }
}
impl Write for Counted {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Io for Counted {
    fn write_by(&mut self, buf: &[u8], _: Instant) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
    fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
    fn closer(&self) -> io::Result<Box<dyn Closer>> {
        Ok(Box::new(NoClose))
    }
}

#[test]
fn read_ahead_takes_no_more_than_the_head_limits() {
    // Terra 1 #9908 #4: measured where it happens, before read_head
    // returns: offered a head and 1 MiB in one read, the codec takes at
    // most request_line + header_bytes + 4 bytes.
    let mut big = b"POST / HTTP/1.1\r\n\r\n".to_vec();
    big.extend_from_slice(&vec![b'x'; 1 << 20]);
    let taken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let io = Counted(Scripted(vec![big].into()), taken.clone());
    let mut c = Http1.connection(Box::new(io), LIMITS);
    c.read_head(T, T).unwrap().unwrap();
    let max = LIMITS.request_line + LIMITS.header_bytes + 4;
    let n = taken.load(std::sync::atomic::Ordering::SeqCst);
    assert!(n <= max, "{n} > {max}");
}

#[test]
fn a_keep_alive_head_gets_all_of_whole_from_its_first_byte() {
    // Terra 2 #10025: idling before a head doesn't eat into the head's
    // own `whole`. Margins that hold on a loaded CI runner: the idle alone is longer
    // than `whole`, so `whole` counted from the call would time out
    // before the first byte, and the head (about 180 ms) has five times
    // that from its first byte.
    let (first_byte, whole) = (Duration::from_secs(10), Duration::from_secs(1));
    let (mut c, mut peer) = conn();
    let w = thread::spawn(move || {
        thread::sleep(Duration::from_millis(1500));
        for b in b"GET / HTTP/1.1\r\n\r\n" {
            peer.write_all(&[*b]).unwrap();
            thread::sleep(Duration::from_millis(10));
        }
        peer
    });
    let h = c.read_head(first_byte, whole);
    assert_eq!(h.unwrap().unwrap().path, "/");
    // And a head that has started, then stalls, times out at `whole`
    // from its first byte, well before `first_byte` (Claude #10166).
    let mut peer = w.join().unwrap();
    peer.write_all(b"GET /2 HT").unwrap();
    let started = Instant::now();
    assert!(matches!(
        c.read_head(first_byte, whole),
        Err(ReadError::TimedOut)
    ));
    assert!(started.elapsed() < first_byte / 2);
}

#[test]
fn an_empty_line_before_the_request_line_is_refused() {
    // Terra 2 #10025: so a HEAD is always seen at byte 0.
    match head_of(b"\r\nHEAD / HTTP/1.1\r\nA\r\n\r\n") {
        Err(ReadError::Malformed { reason, .. }) => {
            assert_eq!(reason, "empty line before the request line")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_expired_deadline_writes_nothing() {
    // Terra 2 #10025: the deadline is checked before the first attempt.
    let (mut a, mut peer) = UnixStream::pair().unwrap();
    let past = Instant::now() - Duration::from_millis(1);
    let err = a.write_by(b"x", past).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    drop(a);
    let mut got = Vec::new();
    peer.read_to_end(&mut got).unwrap();
    assert!(got.is_empty());
}

#[test]
fn a_pipelined_request_gets_no_answer_the_connection_ends_after_the_first() {
    // Orchestrator's call on Terra 3 #10083: two requests in one packet
    // get one response, which says connection: close, then the end; the
    // second is never parsed.
    let (mut c, mut peer) = conn();
    peer.write_all(b"GET /1 HTTP/1.1\r\n\r\nGET /2 HTTP/1.1\r\n\r\n")
        .unwrap();
    // Let both arrive, so the first read takes them together.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/1");
    let r = Response {
        status: 204,
        headers: vec![],
        body: vec![],
    };
    c.write_response(&r, false, T).unwrap();
    assert!(c.read_head(T, T).unwrap().is_none(), "/2 is never parsed");
    let mut out = String::new();
    peer.read_to_string(&mut out).unwrap();
    assert_eq!(out, "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n");
}

#[test]
fn an_interim_response_doesnt_end_an_exchange() {
    // #10083: a 1xx isn't a final response, so it neither closes for
    // leftover bytes nor marks the connection between exchanges.
    let (mut c, mut peer) = conn();
    peer.write_all(b"POST / HTTP/1.1\r\ncontent-length: 2\r\n\r\nhi")
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    c.read_head(T, T).unwrap().unwrap();
    let interim = Response {
        status: 100,
        headers: vec![],
        body: vec![],
    };
    c.write_response(&interim, false, T).unwrap();
    // The body is still there for read_body.
    assert_eq!(c.read_body(2, T).unwrap(), b"hi");
}

#[test]
fn a_refusal_before_a_method_is_established_is_bodiless() {
    // Orchestrator's call on Terra 3 #10182: refused before a method
    // other than HEAD is known, the answer carries no body, so the
    // no-body-for-HEAD rule can't be evaded.
    let flag = |bytes: &[u8]| match head_of(bytes) {
        Err(ReadError::Malformed { reason, head }) => (reason, head),
        Err(ReadError::TooLarge { reason, head }) => (reason, head),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        flag(b"\r\nHEAD / HTTP/1.1\r\n\r\n"),
        ("empty line before the request line", true)
    );
    assert_eq!(flag(b"head / HTTP/1.1\r\n\r\n"), ("method", true));
    assert_eq!(flag(b"HEADX / HTTP/1.1\r\n\r\n"), ("method", true));
    assert_eq!(flag(b"G\0T / HTTP/1.1\r\n\r\n"), ("NUL", true));
    let overlong = format!("{} / HTTP/1.1\r\n\r\n", "A".repeat(300));
    assert_eq!(flag(overlong.as_bytes()), ("request_line", true));
    // A method serve knows, then a space: the answer may have a body.
    assert_eq!(flag(b"GET / HTTP/1.0\r\n\r\n"), ("version", false));
    assert_eq!(flag(b"POST /\0 HTTP/1.1\r\n\r\n"), ("NUL", false));
    // Every method the codec reads still parses.
    for m in METHODS {
        let h = head_of(format!("{m} / HTTP/1.1\r\n\r\n").as_bytes());
        assert_eq!(h.unwrap().unwrap().method, m);
    }
}

/// What the peer gets when serve answers 400 with a body after a refused
/// head `bytes`, and what the next `read_head` returns.
fn answer_to_refused(bytes: &[u8]) -> (String, bool) {
    let (mut c, mut peer) = conn();
    peer.write_all(bytes).unwrap();
    assert!(c.read_head(T, T).is_err());
    let r = Response {
        status: 400,
        headers: vec![],
        body: b"bad".to_vec(),
    };
    c.write_response(&r, false, T).unwrap();
    let next_is_none = c.read_head(T, T).unwrap().is_none();
    let mut out = String::new();
    peer.read_to_string(&mut out).unwrap();
    (out, next_is_none)
}

#[test]
fn a_refused_head_is_answered_then_the_connection_ends() {
    // #10182: a bodiless refusal's body is dropped whatever serve gave,
    // and after any refusal the answer says connection: close and the
    // connection ends.
    assert_eq!(
        answer_to_refused(b"head / HTTP/1.1\r\n\r\n"),
        (
            "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
            true
        )
    );
    assert_eq!(
        answer_to_refused(b"GET / HTTP/1.0\r\n\r\n"),
        (
            "HTTP/1.1 400 Bad Request\r\ncontent-length: 3\r\nconnection: close\r\n\r\nbad".into(),
            true
        )
    );
}

#[test]
fn a_failed_body_read_ends_the_connection_after_its_answer() {
    // The rest of a body that timed out or was cut can't start a request.
    let (mut c, mut peer) = conn();
    peer.write_all(b"POST / HTTP/1.1\r\ncontent-length: 4\r\n\r\nhi")
        .unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    c.read_head(T, T).unwrap().unwrap();
    assert!(matches!(c.read_body(4, T), Err(ReadError::Truncated)));
    let r = Response {
        status: 400,
        headers: vec![],
        body: vec![],
    };
    // An interim answer to a failed read is refused.
    let interim = Response {
        status: 100,
        headers: vec![],
        body: vec![],
    };
    assert!(c.write_response(&interim, false, T).is_err());
    c.write_response(&r, false, T).unwrap();
    assert!(c.read_head(T, T).unwrap().is_none());
    let mut out = String::new();
    peer.read_to_string(&mut out).unwrap();
    assert_eq!(
        out,
        "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
}

/// [`Scripted`], ended without close_notify (an unclean TLS close), or
/// by the error its second field names.
struct Cut(Scripted, io::ErrorKind);

impl Read for Cut {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.0.0.is_empty() {
            return Err(self.1.into());
        }
        self.0.read(buf)
    }
}

impl Write for Cut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Io for Cut {
    fn write_by(&mut self, buf: &[u8], t: Instant) -> io::Result<usize> {
        self.0.write_by(buf, t)
    }
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(t)
    }
    fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.0.set_write_timeout(t)
    }
    fn closer(&self) -> io::Result<Box<dyn Closer>> {
        self.0.closer()
    }
}

fn cut(chunks: &[&[u8]]) -> Box<dyn HttpConnection> {
    let q = chunks.iter().map(|c| c.to_vec()).collect();
    Http1.connection(
        Box::new(Cut(Scripted(q), io::ErrorKind::UnexpectedEof)),
        LIMITS,
    )
}

/// [`Scripted`], ended by a reset (`ECONNRESET`).
fn reset(chunks: &[&[u8]]) -> Box<dyn HttpConnection> {
    let q = chunks.iter().map(|c| c.to_vec()).collect();
    Http1.connection(
        Box::new(Cut(Scripted(q), io::ErrorKind::ConnectionReset)),
        LIMITS,
    )
}

fn status(code: u16) -> Response {
    Response {
        status: code,
        headers: vec![],
        body: vec![],
    }
}

#[test]
fn an_unclean_close_is_clean_only_with_no_request_outstanding() {
    // Orchestrator's call on #10296: requests are counted, up for a head
    // read, down for a final response; only at zero is an unclean close
    // before a head a clean end.
    // A 100 Continue doesn't answer the request.
    let mut c = cut(&[b"POST / HTTP/1.1\r\n\r\n"]);
    c.read_head(T, T).unwrap().unwrap();
    c.write_response(&status(100), false, T).unwrap();
    assert!(matches!(c.read_head(T, T), Err(ReadError::Truncated)));
    // Two heads, one answer: one is still outstanding.
    let mut c = cut(&[b"GET /1 HTTP/1.1\r\n\r\n", b"GET /2 HTTP/1.1\r\n\r\n"]);
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/1");
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/2");
    c.write_response(&status(204), false, T).unwrap();
    assert!(matches!(c.read_head(T, T), Err(ReadError::Truncated)));
    // Both answered: clean.
    let mut c = cut(&[b"GET /1 HTTP/1.1\r\n\r\n", b"GET /2 HTTP/1.1\r\n\r\n"]);
    c.read_head(T, T).unwrap().unwrap();
    c.read_head(T, T).unwrap().unwrap();
    c.write_response(&status(204), false, T).unwrap();
    c.write_response(&status(204), false, T).unwrap();
    assert!(c.read_head(T, T).unwrap().is_none());
    // Before any request: clean.
    assert!(cut(&[]).read_head(T, T).unwrap().is_none());
}

#[test]
fn a_reset_between_exchanges_is_a_close_and_an_error_elsewhere() {
    // A peer that reset the connection is gone: between exchanges that
    // ends it as cleanly as a close; with a request outstanding or a head
    // half read it's still the failure it was.
    assert!(reset(&[]).read_head(T, T).unwrap().is_none());
    let mut c = reset(&[b"GET /1 HTTP/1.1\r\n\r\n"]);
    c.read_head(T, T).unwrap().unwrap();
    c.write_response(&status(204), false, T).unwrap();
    assert!(c.read_head(T, T).unwrap().is_none());
    let mut c = reset(&[b"GET /1 HTTP/1.1\r\n\r\n"]);
    c.read_head(T, T).unwrap().unwrap();
    assert!(
        matches!(c.read_head(T, T), Err(ReadError::Io(e)) if e.kind() == io::ErrorKind::ConnectionReset)
    );
    let mut c = reset(&[b"GET /1 HT"]);
    assert!(
        matches!(c.read_head(T, T), Err(ReadError::Io(e)) if e.kind() == io::ErrorKind::ConnectionReset)
    );
}

#[test]
fn an_answer_before_the_declared_body_is_read_ends_the_connection() {
    // Terra 2 #10352: the body arrives after serve answered; it must not
    // be read as the next head.
    let (mut c, mut peer) = conn();
    peer.write_all(b"POST / HTTP/1.1\r\ncontent-length: 25\r\n\r\n")
        .unwrap();
    c.read_head(T, T).unwrap().unwrap();
    c.write_response(&status(401), false, T).unwrap();
    // The body, which reads as a request if parsed.
    peer.write_all(b"GET /smuggled HTTP/1.1\r\n\r\n").unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    assert!(c.read_head(T, T).unwrap().is_none());
    let mut out = String::new();
    peer.read_to_string(&mut out).unwrap();
    assert_eq!(
        out,
        "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    // A transfer-encoding declares a body too; content-length 0 doesn't,
    // and neither does a body read in full.
    for (head, read, ends) in [
        (
            &b"POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n"[..],
            0,
            true,
        ),
        (b"POST / HTTP/1.1\r\ncontent-length: 0\r\n\r\n", 0, false),
        (b"POST / HTTP/1.1\r\ncontent-length: 2\r\n\r\nhi", 2, false),
    ] {
        let (mut c, mut peer) = conn();
        peer.write_all(head).unwrap();
        thread::sleep(Duration::from_millis(20));
        c.read_head(T, T).unwrap().unwrap();
        if read > 0 {
            c.read_body(read, T).unwrap();
        }
        c.write_response(&status(204), false, T).unwrap();
        peer.write_all(b"GET /next HTTP/1.1\r\n\r\n").unwrap();
        let next = c.read_head(T, T).unwrap();
        assert_eq!(next.is_none(), ends, "{}", String::from_utf8_lossy(head));
    }
}

#[test]
fn a_request_arriving_after_the_answer_is_the_next_one() {
    // Terra 2 #10352 #4: only bytes already read when the answer is
    // written are a pipelined request; a later one is kept-alive's next.
    let (mut c, mut peer) = conn();
    peer.write_all(b"GET /1 HTTP/1.1\r\n\r\n").unwrap();
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/1");
    c.write_response(&status(204), false, T).unwrap();
    peer.write_all(b"GET /2 HTTP/1.1\r\n\r\n").unwrap();
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/2");
}

/// Over TCP: the client sends `request`, then, after the codec has
/// written its answer, 4 KiB more, then closes 50 ms on. Returns what the
/// client got and how long `write_response` took.
fn lingered_answer(request: &'static [u8], close: bool) -> (String, Duration) {
    use std::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let mut c = Http1.connection(Box::new(tcp), LIMITS);
        assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/1");
        let r = Response {
            status: 200,
            headers: vec![],
            body: b"answer".to_vec(),
        };
        let started = Instant::now();
        c.write_response(&r, close, T).unwrap();
        let took = started.elapsed();
        assert!(c.read_head(T, T).unwrap().is_none());
        took
    });
    let mut client = TcpStream::connect(addr).unwrap();
    client.write_all(request).unwrap();
    // The answer's head arriving shows the codec has ended the connection.
    let mut first = [0u8; 1];
    client.read_exact(&mut first).unwrap();
    client.write_all(&[b'x'; 4096]).unwrap();
    thread::sleep(Duration::from_millis(50));
    client.shutdown(Shutdown::Write).unwrap();
    let mut out = String::from_utf8(first.to_vec()).unwrap();
    client.read_to_string(&mut out).unwrap();
    (out, server.join().unwrap())
}

#[test]
fn the_codec_lingers_when_it_ends_a_connection_so_the_answer_isnt_reset() {
    // Terra 2 #10352 #3, #10386 #3 (RFC 9112 section 9.6): over TCP,
    // closing with the peer's later bytes unread resets the connection;
    // the codec reads and drops them before write_response returns, so
    // the client gets the whole answer whether or not serve calls
    // read_head again: when it ends a connection itself (pipelined
    // bytes), and when serve asks it to (close).
    let want = "HTTP/1.1 200 OK\r\ncontent-length: 6\r\nconnection: close\r\n\r\nanswer";
    for (request, close) in [
        (
            &b"GET /1 HTTP/1.1\r\n\r\nGET /2 HTTP/1.1\r\n\r\n"[..],
            false,
        ),
        (b"GET /1 HTTP/1.1\r\n\r\n", true),
    ] {
        let (out, took) = lingered_answer(request, close);
        assert_eq!(out, want);
        // It read until the client's close, 50 ms on, and no longer.
        assert!(
            took >= Duration::from_millis(40) && took < LINGER,
            "{took:?}"
        );
    }
}

#[test]
fn the_lingering_read_is_bounded() {
    // A peer that neither sends nor closes holds it for the linger bound
    // only, at an end the codec makes and at close_gracefully, once.
    let (a, mut peer) = UnixStream::pair().unwrap();
    let mut c = Connection::new(Box::new(a), LIMITS).with_linger(Duration::from_millis(200));
    peer.write_all(b"GET /1 HTTP/1.1\r\n\r\nGET /2 HTTP/1.1\r\n\r\n")
        .unwrap();
    thread::sleep(Duration::from_millis(20));
    c.read_head(T, T).unwrap().unwrap();
    let started = Instant::now();
    c.write_response(&status(204), false, T).unwrap();
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(150) && took < Duration::from_secs(1),
        "{took:?}"
    );
    let started = Instant::now();
    assert!(c.read_head(T, T).unwrap().is_none());
    c.close_gracefully().unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "it ends once"
    );
    // An SSE stream's end lingers the same way.
    let (a, mut peer) = UnixStream::pair().unwrap();
    let mut c = Connection::new(Box::new(a), LIMITS).with_linger(Duration::from_millis(200));
    peer.write_all(b"GET /events HTTP/1.1\r\n\r\n").unwrap();
    c.read_head(T, T).unwrap().unwrap();
    c.start_stream(&[]).unwrap();
    let started = Instant::now();
    c.close_gracefully().unwrap();
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(150) && took < Duration::from_secs(1),
        "{took:?}"
    );
    assert!(c.read_head(T, T).unwrap().is_none());
    drop(peer);
}

#[test]
fn a_body_read_short_of_its_declared_length_ends_the_connection() {
    // Terra 2 #10386 #2: read_body's lengths are counted against the
    // declared content-length; the rest can't start a request.
    let answer = |head: &[u8], reads: &[u64]| {
        let (mut c, mut peer) = conn();
        peer.write_all(head).unwrap();
        thread::sleep(Duration::from_millis(20));
        c.read_head(T, T).unwrap().unwrap();
        for n in reads {
            c.read_body(*n, T).unwrap();
        }
        c.write_response(&status(204), false, T).unwrap();
        peer.write_all(b"GET /next HTTP/1.1\r\n\r\n").unwrap();
        c.read_head(T, T).unwrap().is_none()
    };
    let twenty = b"POST / HTTP/1.1\r\ncontent-length: 20\r\n\r\n0123456789abcdefghij";
    // Only 10 of the 20 have arrived when serve answers; the rest, shaped
    // as a request, comes after (Terra 2 #10418), so nothing is left
    // buffered to end the connection but the count.
    let ten_of_twenty = b"POST / HTTP/1.1\r\ncontent-length: 20\r\n\r\n0123456789";
    assert!(answer(ten_of_twenty, &[10]), "10 of 20 read");
    assert!(!answer(twenty, &[10, 10]), "all 20, in two reads");
    assert!(!answer(twenty, &[20]));
    // A content-length that isn't one number: the end can't be found.
    let two = b"POST / HTTP/1.1\r\ncontent-length: 2\r\ncontent-length: 3\r\n\r\nhi";
    assert!(answer(two, &[2]));
    let signed = b"POST / HTTP/1.1\r\ncontent-length: +2\r\n\r\nhi";
    assert!(answer(signed, &[2]));
    // A transfer-encoding stays unknown whatever is read.
    let chunked = b"POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\n\r\n";
    assert!(answer(chunked, &[12]));
}

#[test]
fn a_head_that_times_out_before_its_method_is_known_gets_a_bodiless_answer() {
    // Terra 2 #10361: a stalled `HEAD /x HT` gets no body in its 408; a
    // stalled GET keeps it. Either way the connection ends.
    let answer = |partial: &[u8]| {
        let (mut c, mut peer) = conn();
        peer.write_all(partial).unwrap();
        assert!(matches!(
            c.read_head(T, Duration::from_millis(100)),
            Err(ReadError::TimedOut)
        ));
        let r = Response {
            status: 408,
            headers: vec![],
            body: b"slow".to_vec(),
        };
        c.write_response(&r, false, T).unwrap();
        drop(c);
        let mut out = String::new();
        peer.read_to_string(&mut out).unwrap();
        out
    };
    assert_eq!(
        answer(b"HEAD /x HT"),
        "HTTP/1.1 408 Request Timeout\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    assert_eq!(
        answer(b"HEA"),
        "HTTP/1.1 408 Request Timeout\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    assert_eq!(
        answer(b"GET /x HT"),
        "HTTP/1.1 408 Request Timeout\r\ncontent-length: 4\r\nconnection: close\r\n\r\nslow"
    );
}

#[test]
fn a_peer_that_closed_its_socket_reads_as_a_close() {
    // On macOS, setting a timeout on a Unix socket whose peer has closed
    // fails with EINVAL; the codec still reads what was sent, then the
    // close, rather than an error.
    let (mut c, mut peer) = conn();
    peer.write_all(b"GET /1 HTTP/1.1\r\n\r\n").unwrap();
    drop(peer);
    thread::sleep(Duration::from_millis(20));
    assert_eq!(c.read_head(T, T).unwrap().unwrap().path, "/1");
    c.write_response(&status(204), false, T).unwrap_err();
    assert!(c.read_head(T, T).unwrap().is_none());
    // A kept-alive client that leaves after its answer: a clean close,
    // even where the kernel reports it as a reset (Linux, when the peer
    // never read the answer).
    let (mut c, mut peer) = conn();
    peer.write_all(b"GET /1 HTTP/1.1\r\n\r\n").unwrap();
    c.read_head(T, T).unwrap().unwrap();
    c.write_response(&status(204), false, T).unwrap();
    drop(peer);
    thread::sleep(Duration::from_millis(20));
    assert!(c.read_head(T, T).unwrap().is_none());
    // Zero is still refused.
    let (a, b) = UnixStream::pair().unwrap();
    drop(b);
    assert_eq!(
        Io::set_read_timeout(&a, Some(Duration::ZERO))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}
