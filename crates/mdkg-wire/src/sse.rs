//! Reading an SSE stream (WHATWG HTML's `text/event-stream`), the client
//! half of [`codec::sse_event`](crate::codec::sse_event): `GET /v1/events`,
//! a turn's output and `msg wait` (A15).
//!
//! **Parsing** follows WHATWG's rules: lines end in LF, CRLF or a lone CR;
//! a leading BOM is skipped; `field: value` loses one space after the
//! colon; `data` lines join with LF; `id` sets the id buffer, unless it
//! holds a NUL, and a blank line makes it the last event id, which
//! carries on to later events; `event` names the
//! type (`message` when absent); a blank line dispatches an event that has
//! data. Comment lines, `serve`'s keep-alives, are read and dropped, so
//! they never reach the caller (#7910); so are `retry` and unknown fields.
//!
//! **Bounds.** A line longer than `max_line`, or an event's data longer
//! than `max_event`, is refused (`InvalidData`), so a host can't make the
//! client buffer without end. `serve` never buffers more than 1 MiB per
//! stream; [`MAX_EVENT`] matches it.
//!
//! **Ending.** `Ok(None)` only at the end of the stream on an event
//! boundary, `serve`'s normal close, after which the client resumes with
//! its signed `?after=` cursor. An end part way through a line or an
//! event is `UnexpectedEof`, never taken for a normal close; WHATWG would
//! drop the partial event silently. Either way an `id` read for an event
//! that never ended isn't the last event id, so resuming from
//! [`SseReader::last_id`] never skips it (Terra 2 #10460). Read timeouts
//! are the caller's, on its stream: heartbeats come every 15 s.
//!
//! **Cost.** Each byte is scanned for a line ending once, and unread
//! bytes move down only once more than half the buffer is consumed, so a
//! long line costs linear work, not a rescan per read.

use std::io::{self, Read};

/// The longest line read.
pub const MAX_LINE: usize = 1 << 20;
/// The most data one event may carry, the LFs joining its lines included.
pub const MAX_EVENT: usize = 1 << 20;

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The last event id at dispatch: this event's `id:`, or an earlier
    /// event's when it carried none.
    pub id: Option<String>,
    /// The `event:` type, `message` when it carried none.
    pub event: String,
    /// The `data:` lines joined with LF.
    pub data: String,
}

/// Reads events from a byte stream.
pub struct SseReader<R> {
    inner: R,
    max_line: usize,
    max_event: usize,
    buf: Vec<u8>,
    /// Where unread bytes in `buf` start.
    pos: usize,
    /// How far past `pos` has been scanned without finding a line ending.
    scanned: usize,
    /// The last line ended in CR: an LF starting the next read is its.
    after_cr: bool,
    started: bool,
    eof: bool,
    /// The last event id: the id buffer as of the last blank line.
    last_id: Option<String>,
    /// WHATWG's last event ID buffer: what `id` lines set.
    id_buf: Option<String>,
    event: Option<String>,
    data: String,
    /// Fields read since the last dispatch or blank line.
    pending: bool,
}

impl<R> std::fmt::Debug for SseReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseReader")
            .field("last_id", &self.last_id)
            .finish_non_exhaustive()
    }
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_owned())
}

impl<R: Read> SseReader<R> {
    pub fn new(inner: R) -> Self {
        Self::with_limits(inner, MAX_LINE, MAX_EVENT)
    }

    pub fn with_limits(inner: R, max_line: usize, max_event: usize) -> Self {
        Self {
            inner,
            max_line,
            max_event,
            buf: Vec::new(),
            pos: 0,
            scanned: 0,
            after_cr: false,
            started: false,
            eof: false,
            last_id: None,
            id_buf: None,
            event: None,
            data: String::new(),
            pending: false,
        }
    }

    /// The last event id, for resuming: the id as of the last event
    /// boundary, never one read for an event that didn't end.
    pub fn last_id(&self) -> Option<&str> {
        self.last_id.as_deref()
    }

    /// The next event; `Ok(None)` at a normal close.
    pub fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        loop {
            let Some(line) = self.next_line()? else {
                if self.pending {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the stream ended part way through an event",
                    ));
                }
                return Ok(None);
            };
            if line.is_empty() {
                self.pending = false;
                self.last_id.clone_from(&self.id_buf);
                let event = self.event.take();
                if self.data.is_empty() {
                    continue;
                }
                let mut data = std::mem::take(&mut self.data);
                data.pop(); // the LF after the last data line
                return Ok(Some(SseEvent {
                    id: self.last_id.clone(),
                    event: event.unwrap_or_else(|| "message".to_owned()),
                    data,
                }));
            }
            self.field(&line)?;
        }
    }

    fn field(&mut self, line: &str) -> io::Result<()> {
        if line.starts_with(':') {
            return Ok(());
        }
        self.pending = true;
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match name {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                // The LF this line adds counts only once another follows.
                if self.data.len() + value.len() > self.max_event {
                    return Err(invalid("an SSE event is too large"));
                }
                self.data.push_str(value);
                self.data.push('\n');
            }
            "id" if !value.contains('\0') => self.id_buf = Some(value.to_owned()),
            _ => {}
        }
        Ok(())
    }

    /// The next line without its ending; `None` at the end of the stream
    /// on a line boundary.
    fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            if self.after_cr && self.pos < self.buf.len() {
                if self.buf[self.pos] == b'\n' {
                    self.pos += 1;
                }
                self.after_cr = false;
            }
            let rest = &self.buf[self.pos..];
            let found = rest[self.scanned..]
                .iter()
                .position(|b| *b == b'\n' || *b == b'\r')
                .map(|i| self.scanned + i);
            if let Some(i) = found {
                if i > self.max_line {
                    return Err(invalid("an SSE line is too long"));
                }
                self.after_cr = rest[i] == b'\r';
                let line = rest[..i].to_vec();
                self.pos += i + 1;
                self.scanned = 0;
                return self.text(line).map(Some);
            }
            self.scanned = rest.len();
            if rest.len() > self.max_line {
                return Err(invalid("an SSE line is too long"));
            }
            if self.eof {
                if rest.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the stream ended part way through a line",
                ));
            }
            self.fill()?;
        }
    }

    fn fill(&mut self) -> io::Result<()> {
        // Moves the unread bytes down only once most of the buffer is
        // read, so a long line isn't moved once per read (Terra 2 #10460).
        if self.pos > self.buf.len() / 2 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        let mut chunk = [0u8; 8 * 1024];
        let n = loop {
            match self.inner.read(&mut chunk) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        };
        if n == 0 {
            self.eof = true;
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    fn text(&mut self, mut line: Vec<u8>) -> io::Result<String> {
        if !self.started {
            self.started = true;
            if line.starts_with(b"\xEF\xBB\xBF") {
                line.drain(..3);
            }
        }
        String::from_utf8(line).map_err(|_| invalid("an SSE line isn't UTF-8"))
    }
}

#[cfg(test)]
mod tests;
