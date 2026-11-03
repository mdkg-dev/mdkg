use super::*;
use crate::codec::sse_event;

/// A stream handing out at most `step` bytes per read, so lines and
/// their endings split across reads.
struct Trickle {
    bytes: Vec<u8>,
    at: usize,
    step: usize,
}

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.step.min(buf.len()).min(self.bytes.len() - self.at);
        buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Every event in `bytes`, read `step` bytes at a time, and how it ended.
fn events(bytes: &[u8], step: usize) -> (Vec<SseEvent>, io::Result<()>) {
    let mut r = SseReader::new(Trickle {
        bytes: bytes.to_vec(),
        at: 0,
        step,
    });
    let mut out = Vec::new();
    loop {
        match r.next_event() {
            Ok(Some(e)) => out.push(e),
            Ok(None) => return (out, Ok(())),
            Err(e) => return (out, Err(e)),
        }
    }
}

fn ev(id: Option<&str>, event: &str, data: &str) -> SseEvent {
    SseEvent {
        id: id.map(str::to_owned),
        event: event.to_owned(),
        data: data.to_owned(),
    }
}

#[test]
fn reads_what_the_codec_writes() {
    let mut stream = Vec::new();
    stream.extend(sse_event(Some("c1.7"), "mdkg.event", "{\"a\":1}").unwrap());
    stream.extend(b": keep-alive\n\n");
    stream.extend(sse_event(None, "mdkg.event", "line one\nline two\n").unwrap());
    stream.extend(sse_event(Some("c1.9"), "mdkg.stream.error", "").unwrap());
    for step in [1, 2, 3, 7, 4096] {
        let (got, end) = events(&stream, step);
        end.unwrap();
        assert_eq!(
            got,
            [
                ev(Some("c1.7"), "mdkg.event", "{\"a\":1}"),
                // No id of its own: the last one carries on.
                ev(Some("c1.7"), "mdkg.event", "line one\nline two\n"),
                ev(Some("c1.9"), "mdkg.stream.error", ""),
            ],
            "step {step}"
        );
    }
}

#[test]
fn every_line_ending_and_a_leading_bom_are_read() {
    for stream in [
        &b"\xEF\xBB\xBFid: 1\ndata: x\n\n"[..],
        b"id: 1\r\ndata: x\r\n\r\n",
        b"id: 1\rdata: x\r\r",
        b"id: 1\r\ndata: x\r\n\n",
    ] {
        for step in [1, 2, 64] {
            let (got, end) = events(stream, step);
            end.unwrap();
            assert_eq!(got, [ev(Some("1"), "message", "x")], "{stream:?} {step}");
        }
    }
}

#[test]
fn fields_follow_whatwg() {
    let stream = b"retry: 10\nunknown: y\ndata:no space\ndata:  two spaces\ndata\n\n\
event: named\n\n\
id: a\0b\ndata: nul id ignored\n\n\
event: reset\ndata: typed\n\ndata: default type\n\n\
id\ndata: empty id\n\n";
    let (got, end) = events(stream, 5);
    end.unwrap();
    assert_eq!(
        got,
        [
            ev(None, "message", "no space\n two spaces\n"),
            // A blank line after only `event:` dispatches nothing.
            ev(None, "message", "nul id ignored"),
            ev(None, "reset", "typed"),
            ev(None, "message", "default type"),
            ev(Some(""), "message", "empty id"),
        ]
    );
}

#[test]
fn an_end_part_way_through_is_never_a_normal_close() {
    // Only an end on an event boundary is `Ok(None)`.
    let (got, end) = events(b"data: a\n\n: heartbeat\n", 3);
    assert_eq!(got, [ev(None, "message", "a")]);
    end.unwrap();
    for cut in [
        &b"data: a\n\ndata: b\n"[..],
        b"data: a\n\ndata: b",
        b"id: 2\n",
    ] {
        let (_, end) = events(cut, 3);
        assert_eq!(
            end.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof,
            "{cut:?}"
        );
    }
}

#[test]
fn lines_and_events_are_bounded() {
    let read = |bytes: Vec<u8>| {
        let mut r = SseReader::with_limits(
            Trickle {
                bytes,
                at: 0,
                step: 100,
            },
            64,
            128,
        );
        r.next_event()
    };
    // A line past the limit is refused before its end arrives.
    let long = [b"data: ".as_slice(), &[b'x'; 10_000]].concat();
    assert_eq!(read(long).unwrap_err().kind(), io::ErrorKind::InvalidData);
    // Many short data lines can't add up past the event limit.
    let many = b"data: 0123456789abcdef\n".repeat(20);
    assert_eq!(read(many).unwrap_err().kind(), io::ErrorKind::InvalidData);
    // Within both, it reads.
    let fits = b"data: 0123456789abcdef\n".repeat(5);
    let fits = [fits.as_slice(), b"\n"].concat();
    assert_eq!(read(fits).unwrap().unwrap().data.len(), 5 * 17 - 1);
    // Not UTF-8.
    assert_eq!(
        read(b"data: \xff\n\n".to_vec()).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn a_read_error_reaches_the_caller() {
    struct Silent;
    impl Read for Silent {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::TimedOut.into())
        }
    }
    let mut r = SseReader::new(Silent);
    assert_eq!(r.next_event().unwrap_err().kind(), io::ErrorKind::TimedOut);
}

#[test]
fn the_last_id_is_kept_for_resuming() {
    let mut r = SseReader::new(&b"id: c1.3\ndata: x\n\ndata: y\n\n"[..]);
    r.next_event().unwrap().unwrap();
    r.next_event().unwrap().unwrap();
    assert_eq!(r.last_id(), Some("c1.3"));
    assert!(r.next_event().unwrap().is_none());
}

#[test]
fn an_id_for_an_event_that_never_ended_isnt_resumed_from() {
    // Terra 2 #10460: the id commits at the event's blank line, so a cut
    // after `id: 2` resumes from 1 and event 2 is sent again.
    for cut in [
        &b"id: 1\ndata: a\n\nid: 2\n"[..],
        b"id: 1\ndata: a\n\nid: 2\nevent: x\ndata: b\n",
        b"id: 1\ndata: a\n\nid: 2\ndata: b",
    ] {
        let mut r = SseReader::new(Trickle {
            bytes: cut.to_vec(),
            at: 0,
            step: 4,
        });
        assert_eq!(r.next_event().unwrap().unwrap().id.as_deref(), Some("1"));
        assert_eq!(
            r.next_event().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(r.last_id(), Some("1"), "{cut:?}");
    }
    // An id ended by a blank line with no data still commits (WHATWG).
    let mut r = SseReader::new(&b"id: 1\ndata: a\n\nid: 2\n\n"[..]);
    r.next_event().unwrap().unwrap();
    assert!(r.next_event().unwrap().is_none());
    assert_eq!(r.last_id(), Some("2"));
}

#[test]
fn an_event_may_carry_exactly_its_limit() {
    // Terra 2 #10460 #2: the LF after the last data line isn't data.
    let read = |data: &[u8]| {
        let bytes = [b"data: ", data, b"\n\n"].concat();
        SseReader::with_limits(&bytes[..], 64, 16).next_event()
    };
    assert_eq!(read(&[b'x'; 16]).unwrap().unwrap().data.len(), 16);
    assert_eq!(
        read(&[b'x'; 17]).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    // Two lines: 7 + LF + 8 = 16 fits; 7 + LF + 9 doesn't.
    let two = |second: usize| {
        let bytes = [
            b"data: 0123456\ndata: ".as_slice(),
            &vec![b'y'; second],
            b"\n\n",
        ]
        .concat();
        SseReader::with_limits(&bytes[..], 64, 16).next_event()
    };
    assert_eq!(two(8).unwrap().unwrap().data.len(), 16);
    assert_eq!(two(9).unwrap_err().kind(), io::ErrorKind::InvalidData);
}

#[test]
fn a_long_line_read_in_small_pieces_is_whole() {
    // Terra 2 #10460 #3: scanned once, moved rarely, and still correct
    // across many reads, with lines either side of it.
    let long = vec![b'z'; MAX_LINE - 6];
    let bytes = [
        b"data: a\n\ndata: ".as_slice(),
        &long,
        b"\r\n\r\ndata: b\n\n",
    ]
    .concat();
    let (got, end) = events(&bytes, 8 * 1024);
    end.unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[1].data.len(), MAX_LINE - 6);
    assert_eq!((got[0].data.as_str(), got[2].data.as_str()), ("a", "b"));
}
