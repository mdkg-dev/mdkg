use super::*;
use std::net::TcpListener;
use std::thread;

const T: Duration = Duration::from_secs(5);

/// Joins a server thread within `d`, failing the test instead of hanging
/// if a deadline regresses (Terra 2 #10025).
fn join_within<R>(h: thread::JoinHandle<R>, d: Duration) -> R {
    let until = Instant::now() + d;
    while !h.is_finished() {
        assert!(
            Instant::now() < until,
            "the host didn't finish within {d:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    h.join().unwrap()
}

fn host() -> (ServeKey, HostAcceptor) {
    let key = ServeKey::from_seed([3u8; 32]);
    let acceptor = HostAcceptor::new(&key).unwrap();
    (key, acceptor)
}

/// A listener on localhost, and a thread accepting one connection with
/// `acceptor` and handing back what `serve` did with it.
fn serve_one<R: Send + 'static>(
    acceptor: HostAcceptor,
    timeout: Duration,
    then: impl FnOnce(io::Result<Box<dyn Io>>) -> R + Send + 'static,
) -> (std::net::SocketAddr, thread::JoinHandle<R>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let t = thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        then(acceptor.accept(tcp, timeout))
    });
    (addr, t)
}

#[test]
fn a_client_pinned_to_the_host_key_talks_tls13() {
    let (key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| {
        let mut io = io.unwrap();
        let mut buf = [0u8; 4];
        io.read_exact(&mut buf).unwrap();
        io.write_all(&buf).unwrap();
        io.flush().unwrap();
        buf
    });
    let tcp = TcpStream::connect(addr).unwrap();
    let (mut stream, presented) = connect_stream(tcp, Some(&key.fingerprint()), T).unwrap();
    assert_eq!(presented, key.fingerprint());
    assert_eq!(
        stream.conn.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_3)
    );
    stream.write_all(b"ping").unwrap();
    let mut back = [0u8; 4];
    stream.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"ping");
    assert_eq!(&server.join().unwrap(), b"ping");
}

#[test]
fn another_pin_is_refused() {
    let (_key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| io.is_err());
    let other = ServeKey::from_seed([4u8; 32]).fingerprint();
    let tcp = TcpStream::connect(addr).unwrap();
    let err = connect(tcp, &other, T).err().expect("refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
    assert!(server.join().unwrap(), "the host's handshake fails too");
}

#[test]
fn a_stalled_or_plaintext_peer_fails_the_handshake_in_time() {
    // Connects and sends nothing: the handshake times out.
    let (_key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, Duration::from_millis(300), |io| {
        io.err().map(|e| e.kind())
    });
    let _tcp = TcpStream::connect(addr).unwrap();
    let started = Instant::now();
    assert_eq!(server.join().unwrap(), Some(io::ErrorKind::TimedOut));
    assert!(started.elapsed() < Duration::from_secs(3));
    // Plain HTTP instead of a ClientHello is refused.
    let (_key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| io.is_err());
    let mut tcp = TcpStream::connect(addr).unwrap();
    tcp.write_all(b"GET / HTTP/1.1\r\nHost: h\r\n\r\n").unwrap();
    assert!(server.join().unwrap());
}

#[test]
fn the_codec_runs_over_tls() {
    use crate::codec::{Codec, HeadLimits, Http1};
    let (key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| {
        let limits = HeadLimits {
            request_line: 256,
            header_bytes: 512,
            header_count: 8,
        };
        let mut c = Http1.connection(io.unwrap(), limits);
        let head = c.read_head(T, T).unwrap().unwrap();
        let body = c.read_body(2, T).unwrap();
        (head.path, body)
    });
    let tcp = TcpStream::connect(addr).unwrap();
    let mut io = connect(tcp, &key.fingerprint(), T).unwrap();
    io.write_all(b"POST /v1/x HTTP/1.1\r\nContent-Length: 2\r\n\r\nhi")
        .unwrap();
    io.flush().unwrap();
    let (path, body) = server.join().unwrap();
    assert_eq!((path.as_str(), body.as_slice()), ("/v1/x", &b"hi"[..]));
}

#[test]
fn spki_parsing_takes_only_an_ed25519_key() {
    let mut spki = ED25519_SPKI_PREFIX.to_vec();
    spki.extend_from_slice(&[7u8; 32]);
    assert_eq!(ed25519_key(&spki), Some(&[7u8; 32][..]));
    assert_eq!(ed25519_key(&spki[..43]), None);
    let mut long = spki.clone();
    long.push(0);
    assert_eq!(ed25519_key(&long), None);
    spki[8] = 0x71; // another curve's OID
    assert_eq!(ed25519_key(&spki), None);
}

#[test]
fn the_pinned_key_without_its_signature_is_refused() {
    // Claude #9799, Terra 1 #9853: a host presenting the pinned key but
    // signing the handshake with another fails the TLS signature check.
    let pinned = ServeKey::from_seed([3u8; 32]);
    let impostor = ServeKey::from_seed([5u8; 32]);
    let config =
        server_config(spki_of(&pinned.public_key()), signer_of(&impostor).unwrap()).unwrap();
    let acceptor = HostAcceptor { config };
    let (addr, server) = serve_one(acceptor, T, |io| io.is_err());
    let tcp = TcpStream::connect(addr).unwrap();
    let err = connect(tcp, &pinned.fingerprint(), T)
        .err()
        .expect("refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
    assert!(server.join().unwrap());
}

#[test]
fn pairing_learns_the_presented_key_and_still_checks_its_signature() {
    let (key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| io.is_ok());
    let tcp = TcpStream::connect(addr).unwrap();
    let (_io, fp) = connect_for_pairing(tcp, T).unwrap();
    assert_eq!(fp, key.fingerprint());
    assert!(server.join().unwrap());
    // An impostor's signature fails pairing too.
    let pinned = ServeKey::from_seed([3u8; 32]);
    let impostor = ServeKey::from_seed([5u8; 32]);
    let config =
        server_config(spki_of(&pinned.public_key()), signer_of(&impostor).unwrap()).unwrap();
    let (addr, server) = serve_one(HostAcceptor { config }, T, |io| io.is_err());
    let tcp = TcpStream::connect(addr).unwrap();
    assert!(connect_for_pairing(tcp, T).is_err());
    assert!(server.join().unwrap());
}

/// A real ClientHello's bytes, to trickle.
fn client_hello() -> Vec<u8> {
    let config = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(HostKey {
            pinned: None,
            algorithms: provider().signature_verification_algorithms,
        }))
        .with_no_client_auth();
    let mut conn =
        ClientConnection::new(Arc::new(config), ServerName::try_from("mdkg").unwrap()).unwrap();
    let mut out = Vec::new();
    while conn.wants_write() {
        conn.write_tls(&mut out).unwrap();
    }
    out
}

#[test]
fn a_trickled_handshake_cant_outlast_its_timeout() {
    // Claude #9799, Terra 1 #9853: each byte arriving just inside the
    // socket timeout doesn't stretch the handshake.
    let (_key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, Duration::from_millis(300), |io| {
        io.err().map(|e| e.kind())
    });
    let mut tcp = TcpStream::connect(addr).unwrap();
    let hello = client_hello();
    let started = Instant::now();
    let w = thread::spawn(move || {
        for b in hello {
            if tcp.write_all(&[b]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    let kind = join_within(server, Duration::from_secs(5));
    assert!(
        matches!(
            kind,
            Some(io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
        ),
        "{kind:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    w.join().unwrap();
}

#[test]
fn a_trickled_record_cant_outlast_a_codec_deadline() {
    // Terra 1 #9853: after the handshake, a TLS record trickled a byte at
    // a time doesn't keep a head read open past its timeout.
    use crate::codec::{Codec, HeadLimits, Http1, ReadError};
    let (key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| {
        let limits = HeadLimits {
            request_line: 256,
            header_bytes: 512,
            header_count: 8,
        };
        let mut c = Http1.connection(io.unwrap(), limits);
        let started = Instant::now();
        let r = c.read_head(Duration::from_millis(300), Duration::from_millis(300));
        (matches!(r, Err(ReadError::TimedOut)), started.elapsed())
    });
    let tcp = TcpStream::connect(addr).unwrap();
    let mut raw = tcp.try_clone().unwrap();
    let _io = connect(tcp, &key.fingerprint(), T).unwrap();
    // An application-data record header claiming 64 bytes, then a byte
    // every 50 ms.
    raw.write_all(&[0x17, 0x03, 0x03, 0x00, 0x40]).unwrap();
    let w = thread::spawn(move || {
        for _ in 0..40 {
            if raw.write_all(&[0]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    let (timed_out, took) = join_within(server, Duration::from_secs(5));
    assert!(timed_out);
    assert!(took < Duration::from_millis(1500), "{took:?}");
    w.join().unwrap();
}

/// How the client ends: with close_notify, or by half-closing the TCP
/// stream without one.
#[derive(Clone, Copy, Debug)]
enum End {
    Graceful,
    Unclean,
}

/// Connects, does `before` with the client stream, ends it as `end`, and
/// returns what the host's codec saw for each `read_head` it made, after
/// answering every head it read in full if `answer`.
fn session(end: End, answer: bool, before: impl FnOnce(&mut Box<dyn Io>)) -> Vec<String> {
    use crate::codec::{Codec, HeadLimits, Http1, Response};
    let limits = HeadLimits {
        request_line: 256,
        header_bytes: 512,
        header_count: 8,
    };
    let (key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, move |io| {
        let mut c = Http1.connection(io.unwrap(), limits);
        let mut seen = Vec::new();
        loop {
            match c.read_head(T, T) {
                Ok(Some(h)) => {
                    seen.push(format!("head {}", h.path));
                    if answer {
                        let r = Response {
                            status: 204,
                            headers: vec![],
                            body: vec![],
                        };
                        c.write_response(&r, false, T).unwrap();
                    }
                }
                Ok(None) => {
                    seen.push("clean".into());
                    break;
                }
                Err(e) => {
                    seen.push(format!("{e:?}"));
                    break;
                }
            }
        }
        seen
    });
    let tcp = TcpStream::connect(addr).unwrap();
    let raw = tcp.try_clone().unwrap();
    let mut io = connect(tcp, &key.fingerprint(), T).unwrap();
    before(&mut io);
    match end {
        End::Graceful => io.close_gracefully().unwrap(),
        // Half-close, keeping the socket until the host is done: dropping
        // it with the host's session ticket unread would reset it.
        End::Unclean => raw.shutdown(std::net::Shutdown::Write).unwrap(),
    }
    let seen = server.join().unwrap();
    drop(io);
    seen
}

#[test]
fn only_close_notify_or_a_close_between_exchanges_is_clean() {
    // Orchestrator's call on Terra 1 #9945: an unclean TLS close is
    // UnexpectedEof; the codec takes it as a clean close only before a
    // head's first byte, between exchanges.
    let send = |io: &mut Box<dyn Io>| {
        io.write_all(b"GET /1 HTTP/1.1\r\n\r\n").unwrap();
        io.flush().unwrap();
    };
    // With close_notify: always clean.
    assert_eq!(session(End::Graceful, false, |_| {}), ["clean"]);
    // Unclean, before any request: between exchanges, so clean.
    assert_eq!(session(End::Unclean, false, |_| {}), ["clean"]);
    // Unclean, after a request answered in full: clean.
    let seen = session(End::Unclean, true, |io| {
        send(io);
        let mut b = [0u8; 64];
        let _ = io.read(&mut b);
    });
    assert_eq!(seen, ["head /1", "clean"]);
    // Unclean, after a request that wasn't answered: Truncated.
    assert_eq!(session(End::Unclean, false, send), ["head /1", "Truncated"]);
    // Unclean, part way through a head: Truncated.
    let seen = session(End::Unclean, false, |io| {
        io.write_all(b"GET / HT").unwrap();
        io.flush().unwrap();
    });
    assert_eq!(seen, ["Truncated"]);
    // close_notify part way through a head: still Truncated by framing.
    let seen = session(End::Graceful, false, |io| {
        io.write_all(b"GET / HT").unwrap();
        io.flush().unwrap();
    });
    assert_eq!(seen, ["Truncated"]);
}

#[test]
fn a_tls_timeout_is_per_read_as_on_a_plain_socket() {
    // Orchestrator's call on #9945: a client that sets a read timeout once
    // and gets data more often than that never times out.
    let (key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| {
        let mut io = io.unwrap();
        for _ in 0..10 {
            io.write_all(b"x").unwrap();
            io.flush().unwrap();
            thread::sleep(Duration::from_millis(60));
        }
        io.close_gracefully().unwrap();
    });
    let tcp = TcpStream::connect(addr).unwrap();
    let mut io = connect(tcp, &key.fingerprint(), T).unwrap();
    io.set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    let mut got = 0;
    let mut b = [0u8; 16];
    loop {
        match io.read(&mut b) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) => panic!("after {got} bytes: {e:?}"),
        }
    }
    assert_eq!(got, 10);
    server.join().unwrap();
}

#[test]
fn a_tls_response_to_a_silent_peer_fails_in_time() {
    // Terra 1 #9908 #1: over TLS a write can need a read; a peer that
    // neither reads nor sends can't hold a response past its timeout.
    use crate::codec::{Codec, HeadLimits, Http1, Response};
    let (key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| {
        let limits = HeadLimits {
            request_line: 256,
            header_bytes: 512,
            header_count: 8,
        };
        let mut c = Http1.connection(io.unwrap(), limits);
        let r = Response {
            status: 200,
            headers: vec![],
            // Far more than loopback's kernel buffers hold, Linux's
            // autotuned maximums included (Terra 2 #10352).
            body: vec![b'x'; 64 << 20],
        };
        let started = Instant::now();
        let res = c.write_response(&r, false, Duration::from_millis(300));
        (res.err().map(|e| e.kind()), started.elapsed())
    });
    let tcp = TcpStream::connect(addr).unwrap();
    let _io = connect(tcp, &key.fingerprint(), T).unwrap();
    let (failed, took) = join_within(server, Duration::from_secs(5));
    assert_eq!(failed, Some(io::ErrorKind::TimedOut));
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

#[test]
fn a_tls_response_to_a_trickle_reader_fails_in_time() {
    // Terra 1 #9908 #2: a peer draining a little at a time can't stretch
    // a TLS write past its deadline either.
    use crate::codec::{Codec, HeadLimits, Http1, Response};
    let (key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| {
        let limits = HeadLimits {
            request_line: 256,
            header_bytes: 512,
            header_count: 8,
        };
        let mut c = Http1.connection(io.unwrap(), limits);
        let r = Response {
            status: 200,
            headers: vec![],
            // Far more than loopback's kernel buffers hold, Linux's
            // autotuned maximums included (Terra 2 #10352).
            body: vec![b'x'; 64 << 20],
        };
        let started = Instant::now();
        let res = c.write_response(&r, false, Duration::from_millis(300));
        (res.err().map(|e| e.kind()), started.elapsed())
    });
    let tcp = TcpStream::connect(addr).unwrap();
    let mut raw = tcp.try_clone().unwrap();
    let io = connect(tcp, &key.fingerprint(), T).unwrap();
    let reader = thread::spawn(move || {
        let mut b = [0u8; 64];
        for _ in 0..300 {
            if raw.read(&mut b).unwrap_or(0) == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    });
    let (failed, took) = join_within(server, Duration::from_secs(5));
    assert_eq!(failed, Some(io::ErrorKind::TimedOut));
    assert!(took < Duration::from_millis(1500), "{took:?}");
    drop(io);
    reader.join().unwrap();
}

#[test]
fn a_zero_timeout_is_refused_on_tls_as_on_a_plain_socket() {
    // Terra 3 #10083: the same answer on every transport.
    let (key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| io.map(drop));
    let tcp = TcpStream::connect(addr).unwrap();
    let plain = tcp.try_clone().unwrap();
    let io = connect(tcp, &key.fingerprint(), T).unwrap();
    for (tls, std) in [
        (
            io.set_read_timeout(Some(Duration::ZERO)),
            plain.set_read_timeout(Some(Duration::ZERO)),
        ),
        (
            io.set_write_timeout(Some(Duration::ZERO)),
            plain.set_write_timeout(Some(Duration::ZERO)),
        ),
    ] {
        assert_eq!(tls.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
    // Only zero is refused, on both sides (Terra 1 #10307).
    io.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
    io.set_read_timeout(None).unwrap();
    io.set_write_timeout(Some(Duration::from_millis(1)))
        .unwrap();
    io.set_write_timeout(None).unwrap();
    join_within(server, Duration::from_secs(5)).unwrap();
}

#[test]
fn a_tls_write_by_sends_its_records_by_its_own_deadline() {
    // Terra 3 #10182, Terra 2 #10361: `write_by` flushes its records
    // under the caller's deadline. Against a peer that has stopped
    // reading, every write that returns Ok has left nothing buffered in
    // rustls (rustls's own write would return Ok with records unsent,
    // its flush error swallowed), and the first that can't finish is
    // TimedOut by its deadline.
    let (key, acceptor) = host();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (addr, server) = serve_one(acceptor, T, move |io| {
        let _io = io.unwrap();
        // Read nothing until the client is done.
        let _ = release_rx.recv();
    });
    let tcp = TcpStream::connect(addr).unwrap();
    let (stream, _) = connect_stream(tcp, Some(&key.fingerprint()), T).unwrap();
    let mut io = TlsIo::Client(stream);
    let chunk = [0u8; 16 << 10];
    let mut sent = 0usize;
    let (err, took) = loop {
        assert!(sent < 256 << 20, "the peer's buffers never filled");
        let wait = Duration::from_millis(200);
        let started = Instant::now();
        match io.write_by(&chunk, started + wait) {
            Ok(n) => {
                sent += n;
                let TlsIo::Client(s) = &io else {
                    unreachable!()
                };
                assert!(!s.conn.wants_write(), "Ok with records unsent");
            }
            Err(e) => break (e, started.elapsed()),
        }
    };
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(took < Duration::from_millis(800), "{took:?}");
    // And an expired deadline sends nothing.
    let past = Instant::now();
    thread::sleep(Duration::from_millis(5));
    assert_eq!(
        io.write_by(b"never", past).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    release_tx.send(()).unwrap();
    join_within(server, Duration::from_secs(5));
}

#[test]
fn a_tls_response_to_a_peer_that_stops_reading_mid_response_fails_at_the_deadline() {
    // Orchestrator's call on Terra 3 #10083: the peer reads the start of
    // the response, then stops; the write fails at its deadline.
    use crate::codec::{Codec, HeadLimits, Http1, Response};
    let (key, acceptor) = host();
    let (addr, server) = serve_one(acceptor, T, |io| {
        let limits = HeadLimits {
            request_line: 256,
            header_bytes: 512,
            header_count: 8,
        };
        let mut c = Http1.connection(io.unwrap(), limits);
        let r = Response {
            status: 200,
            headers: vec![],
            // Far more than loopback's kernel buffers hold, Linux's
            // autotuned maximums included (Terra 2 #10352).
            body: vec![b'x'; 64 << 20],
        };
        let started = Instant::now();
        let res = c.write_response(&r, false, Duration::from_millis(400));
        (res.err().map(|e| e.kind()), started.elapsed())
    });
    let tcp = TcpStream::connect(addr).unwrap();
    let mut io = connect(tcp, &key.fingerprint(), T).unwrap();
    let mut got = 0;
    let mut b = [0u8; 4096];
    while got < 64 * 1024 {
        got += io.read(&mut b).unwrap();
    }
    // Then stop reading, keeping the connection open.
    let (failed, took) = join_within(server, Duration::from_secs(5));
    assert_eq!(failed, Some(io::ErrorKind::TimedOut));
    assert!(took < Duration::from_millis(1500), "{took:?}");
    drop(io);
}
