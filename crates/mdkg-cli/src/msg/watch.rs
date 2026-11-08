//! Stdio transport for the API event view. A refusal is an ordinary
//! envelope; after the first frame, errors are final bare stream events.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::Args;
use mdkg_coord_sqlite::events::{self, CursorProblem, Event, ReadError, Reader};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::{ErrorKind, GraphError, config};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    flag,
};

use crate::coord::{Repo, now_ms, store_error_in};
use crate::output::{self, DiagnosticOut, Report};

#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Resume just after this opaque event cursor (`start` replays history).
    #[arg(long, conflicts_with = "from_start")]
    after: Option<String>,
    /// Replay from the first record, then follow new events.
    #[arg(long)]
    from_start: bool,
    /// Event types, repeated or comma-separated; a trailing `*` matches a prefix.
    #[arg(long, value_delimiter = ',')]
    types: Vec<String>,
    /// Events of these channels, including messages posted to them (repeatable).
    #[arg(long, value_name = "NAME")]
    channel: Vec<String>,
    /// Events for messages delivered to this mailbox.
    #[arg(long, value_name = "NAME")]
    mailbox: Option<String>,
    /// Required: print one bare API event per line.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WatchData {
    /// Current event-stream head, so a refused client can re-read and resume.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

/// `root` is where a writing command must run for the store
/// ([`Repo::command_root`]), which a recovery or upgrade hint names.
fn failure(error: ReadError, root: &Path) -> (GraphError, Option<String>) {
    match error {
        ReadError::Store(error) => (store_error_in(error, Some(root)), None),
        ReadError::Cursor { problem, head } => {
            let (code, message) = match problem {
                CursorProblem::Bad => (Code::ApiBadCursor, "the event cursor isn't well formed"),
                CursorProblem::Reset => (
                    Code::ApiCursorReset,
                    "the event cursor's history was restored or replaced",
                ),
                CursorProblem::Foreign => (
                    Code::ApiCursorForeign,
                    "the event cursor names another coordinator",
                ),
                CursorProblem::Expired => (
                    Code::ApiCursorExpired,
                    "the event cursor's history was pruned",
                ),
            };
            let kind = if problem == CursorProblem::Bad {
                ErrorKind::Usage
            } else {
                ErrorKind::Conflict
            };
            (
                GraphError::new(
                    kind,
                    Diagnostic::error(code, message)
                        .with_hint("re-read the resources and resume from their event_cursor"),
                ),
                Some(head),
            )
        }
    }
}

fn type_matches(pattern: &str, kind: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or_else(|| pattern == kind, |prefix| kind.starts_with(prefix))
}

impl WatchArgs {
    fn validate(&self) -> Result<(), GraphError> {
        if !self.json {
            return Err(super::usage(
                Code::UsageInvalidArguments,
                "msg watch requires --json",
            ));
        }
        for channel in &self.channel {
            super::check_room(channel)?;
        }
        if let Some(mailbox) = &self.mailbox {
            super::check_mailbox(mailbox)?;
        }
        if self
            .types
            .iter()
            .any(|p| p.is_empty() || p.strip_suffix('*').unwrap_or(p).contains('*'))
        {
            return Err(super::usage(
                Code::UsageInvalidArguments,
                "--types accepts event names or a prefix followed by one *",
            ));
        }
        Ok(())
    }

    fn matches(&self, event: &Event) -> bool {
        self.selects(&event.kind, &event.subjects)
    }

    fn selects(&self, kind: &str, subjects: &[String]) -> bool {
        (self.types.is_empty() || self.types.iter().any(|t| type_matches(t, kind)))
            && (self.channel.is_empty()
                || self
                    .channel
                    .iter()
                    .any(|name| subjects.contains(&events::path("channels", name))))
            && self.mailbox.as_ref().is_none_or(|name| {
                kind.starts_with("mdkg.message.")
                    && subjects.contains(&events::path("mailboxes", name))
            })
    }
}

/// What one read of the graph's head finds: the commit it names, or `None`
/// while it names none (an unborn `HEAD`, a graph ref not made yet). A read
/// that fails is an error, which changes nothing.
pub(super) type HeadRead = std::io::Result<Option<String>>;

/// The graph's head as the stream last saw it, read again at most once a
/// poll interval: the graph ref's tip in ref mode, the checkout's `HEAD`
/// otherwise. `mdkg.graph.head_changed` (A9) is an ephemeral hint: the
/// stream sends one naming the head it starts from, and one each time the
/// head changes, with `head` null while it names no commit. It's never
/// replayed, and missed while no stream runs.
pub(super) struct GraphHead<'a> {
    graph: String,
    read: Box<dyn FnMut() -> HeadRead + 'a>,
    /// `None` until a read succeeds; then what it found.
    last: Option<Option<String>>,
    checked: Option<Instant>,
}

impl<'a> GraphHead<'a> {
    pub(super) fn new(graph: String, read: impl FnMut() -> HeadRead + 'a) -> Self {
        Self {
            graph,
            read: Box::new(read),
            last: None,
            checked: None,
        }
    }

    /// The head to announce (`None` inside: it names no commit): what the
    /// first read that succeeds finds, where the stream starts; after
    /// that, what differs from the last read. A read that fails changes
    /// nothing.
    fn moved(&mut self, every: Duration) -> Option<Option<String>> {
        if self.checked.is_some_and(|at| at.elapsed() < every) {
            return None;
        }
        self.checked = Some(Instant::now());
        let head = (self.read)().ok()?;
        let was = self.last.replace(head.clone());
        (was.as_ref() != Some(&head)).then_some(head)
    }
}

fn ephemeral(kind: &str, cursor: &str, actor: &str, data: Value) -> Value {
    json!({"schema":"mdkg.api.event.v1","id":uuid::Uuid::now_v7().to_string(),"cursor":cursor,
        "type":kind,"time":mdkg_log::record::format_ts(now_ms()),"actor":actor,"subjects":[],"durable":false,"data":data})
}

fn line(out: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    out.flush()
}

/// The stop flag is the signal-handler seam; unit tests drive the same loop
/// without delivering process-wide signals to a parallel test runner.
fn follow(
    reader: &mut Reader,
    root: &Path,
    args: &WatchArgs,
    stop: &AtomicBool,
    out: &mut impl Write,
    heartbeat: Duration,
    graph_head: &mut GraphHead<'_>,
) -> u8 {
    let mut last_sent = Instant::now();
    let mut started = false;
    let mut last_head = reader.cursor();
    let head_subjects = vec![events::path("graphs", &graph_head.graph)];
    let heads_selected = args.selects("mdkg.graph.head_changed", &head_subjects);
    loop {
        if stop.load(Ordering::Relaxed) {
            return 0;
        }
        let page = match reader.poll(256) {
            Ok(page) => page,
            Err(error) => {
                let (error, head) = failure(error, root);
                if !started {
                    let mut report = Report::failed(error);
                    report.data = Some(WatchData { head });
                    return output::emit("msg.watch", true, report, |_| {});
                }
                let code = error.kind.exit_code();
                let frame = ephemeral(
                    "mdkg.stream.error",
                    &reader.cursor().unwrap_or_default(),
                    "mdkg",
                    json!({"diagnostics":[DiagnosticOut::from(error.diagnostic.as_ref())],"head":head.or_else(||last_head.clone())}),
                );
                let _ = line(out, &frame);
                return code;
            }
        };
        // A successful initial read establishes the stream, even if filters
        // select no events. Cursor failures at that boundary stay envelopes.
        started = true;
        last_head = Some(page.head.clone());
        for event in page.events {
            if args.matches(&event) {
                if let Err(error) = line(out, &event.value()) {
                    return if error.kind() == io::ErrorKind::BrokenPipe {
                        0
                    } else {
                        4
                    };
                }
                last_sent = Instant::now();
            }
            if stop.load(Ordering::Relaxed) {
                return 0;
            }
        }
        // Filters that leave the event out never read the head (each read
        // is a git process).
        if heads_selected && let Some(head) = graph_head.moved(super::poll_interval()) {
            let cursor = reader.cursor().unwrap_or_else(|| page.head.clone());
            let mut frame = ephemeral(
                "mdkg.graph.head_changed",
                &cursor,
                "mdkg",
                json!({"graph":graph_head.graph,"head":head}),
            );
            frame["subjects"] = json!(head_subjects);
            if let Err(error) = line(out, &frame) {
                return if error.kind() == io::ErrorKind::BrokenPipe {
                    0
                } else {
                    4
                };
            }
            last_sent = Instant::now();
        }
        if last_sent.elapsed() >= heartbeat {
            let cursor = reader.cursor().unwrap_or_else(|| page.head.clone());
            let frame = ephemeral(
                "mdkg.stream.heartbeat",
                &cursor,
                "mdkg",
                json!({"head":page.head}),
            );
            if let Err(error) = line(out, &frame) {
                return if error.kind() == io::ErrorKind::BrokenPipe {
                    0
                } else {
                    4
                };
            }
            last_sent = Instant::now();
        }
        if !page.more {
            // Bound shutdown latency even when a caller asks for a long poll.
            let pause = super::poll_interval().min(heartbeat.saturating_sub(last_sent.elapsed()));
            let paused = Instant::now();
            while !stop.load(Ordering::Relaxed) && paused.elapsed() < pause {
                std::thread::sleep(
                    Duration::from_millis(50).min(pause.saturating_sub(paused.elapsed())),
                );
            }
        }
    }
}

pub(super) fn run(root: &Path, args: &WatchArgs) -> u8 {
    type Prepared = (Reader, PathBuf, String, PathBuf, String);
    let prepared = (|| -> Result<Prepared, (GraphError, Option<String>)> {
        args.validate().map_err(|e| (e, None))?;
        let repo = Repo::open(root).map_err(|e| (e, None))?;
        let config = config::load(&repo.root).map_err(|e| (e, None))?;
        let graph = config.graph_id.map(|id| id.to_string()).ok_or_else(|| {
            (
                super::usage(
                    Code::ConfigInvalid,
                    "the event stream requires a format 0.7 graph",
                ),
                None,
            )
        })?;
        let path = mdkg_coord_sqlite::git::store_path(&repo.top)
            .map_err(|e| (crate::coord::git_error(e), None))?;
        Reader::open(
            &path,
            &graph,
            if args.from_start {
                Some("start")
            } else {
                args.after.as_deref()
            },
        )
        .map_err(|e| failure(e, repo.command_root()))
        .map(|reader| {
            // The graph's head: the graph ref in ref mode, else HEAD.
            let head_ref = repo
                .mode
                .as_ref()
                .map_or_else(|| "HEAD".to_owned(), |m| m.graph.name.clone());
            (
                reader,
                repo.command_root().to_path_buf(),
                graph,
                repo.top.clone(),
                head_ref,
            )
        })
    })();
    let (mut reader, command_root, graph, top, head_ref) = match prepared {
        Ok(prepared) => prepared,
        Err((error, head)) => {
            let mut report = Report::failed(error);
            if head.is_some() {
                report.data = Some(WatchData { head });
            }
            return output::emit("msg.watch", args.json, report, |_| {});
        }
    };
    // In ref mode the command was bound to the graph cache, whose shared
    // lock would otherwise last as long as the stream and keep every graph
    // write (and a `status` that refreshes the cache) waiting. The stream
    // reads only the store and the graph ref's name, both taken above.
    crate::coord::graph_git::unbind();
    // SIGINT and SIGTERM close the stream at its next loop boundary with
    // exit 0 (A10). A second one takes the default action at once, for a
    // stream blocked writing to a reader that stopped reading.
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        if let Err(e) = close_on(signal, &stop) {
            eprintln!("warning: signal {signal} will end msg watch without a clean close: {e}");
        }
    }
    let mut graph_head = GraphHead::new(graph, || crate::coord::commit_named(&top, &head_ref));
    follow(
        &mut reader,
        &command_root,
        args,
        &stop,
        &mut io::stdout().lock(),
        Duration::from_secs(15),
        &mut graph_head,
    )
}

/// Registers `signal` to set `stop`. signal-hook runs a signal's actions in
/// registration order, so the conditional default goes first: it sees the
/// flag a previous signal set, and only then does this signal set it.
fn close_on(signal: std::ffi::c_int, stop: &Arc<AtomicBool>) -> io::Result<()> {
    let conditional = flag::register_conditional_default(signal, stop.clone())?;
    if let Err(e) = flag::register(signal, stop.clone()) {
        // Without the flag, the conditional action would swallow every such
        // signal, and unregistering it leaves the handler ignoring them
        // (signal-hook never restores the default). Take the default action
        // on the first signal instead, as without a handler.
        signal_hook::low_level::unregister(conditional);
        flag::register_conditional_default(signal, Arc::new(AtomicBool::new(true)))?;
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("mdkg-watch-loop-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir(&dir).unwrap();
            let fixture = Self(dir);
            drop(
                mdkg_coord_sqlite::Store::open_with(
                    &fixture.0.join("coord.sqlite"),
                    1000,
                    mdkg_coord_sqlite::Upgrade::Allow,
                )
                .unwrap(),
            );
            fixture
        }
        fn reader(&self) -> Reader {
            Reader::open(
                &self.0.join("coord.sqlite"),
                "01a185ac-560e-76e0-91af-2c49dfc29b81",
                None,
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }
    fn args() -> WatchArgs {
        WatchArgs {
            after: None,
            from_start: false,
            types: Vec::new(),
            channel: Vec::new(),
            mailbox: None,
            json: true,
        }
    }

    struct StopAfterFrame<'a> {
        bytes: Vec<u8>,
        stop: &'a AtomicBool,
    }
    impl Write for StopAfterFrame<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.bytes.last() == Some(&b'\n') {
                self.stop.store(true, Ordering::Relaxed);
            }
            Ok(())
        }
    }
    #[test]
    fn an_idle_heartbeat_repeats_the_durable_cursor_without_writing_a_record() {
        let fixture = Fixture::new();
        let mut reader = fixture.reader();
        let cursor = reader.cursor().unwrap();
        let stop = AtomicBool::new(false);
        let mut out = StopAfterFrame {
            bytes: Vec::new(),
            stop: &stop,
        };
        assert_eq!(
            follow(
                &mut reader,
                Path::new("/repo"),
                &args(),
                &stop,
                &mut out,
                Duration::from_millis(1),
                &mut GraphHead::new(String::new(), || Err(io::Error::other("no head")))
            ),
            0
        );
        let event: Value = serde_json::from_slice(&out.bytes).unwrap();
        assert_eq!(event["type"], "mdkg.stream.heartbeat");
        assert_eq!(event["cursor"], cursor);
        assert_eq!(event["data"]["head"], cursor);
        assert_eq!(event["durable"], false);
        assert_eq!(
            events::head(&fixture.0.join("coord.sqlite")).unwrap(),
            cursor
        );
    }
    #[test]
    fn filtered_backlog_heartbeats_before_catching_up_to_the_head() {
        let fixture = Fixture::new();
        let path = fixture.0.join("coord.sqlite");
        let mut store =
            mdkg_coord_sqlite::Store::open_with(&path, 1000, mdkg_coord_sqlite::Upgrade::Allow)
                .unwrap();
        let lease = match store
            .execute(&mdkg_coord::Command::Claim {
                owner: "writer".into(),
                scope: mdkg_coord::Scope::new("work"),
                mode: mdkg_coord::Mode::Exclusive,
                now: 1,
                request: None,
                node: None,
                branch: None,
            })
            .unwrap()
        {
            mdkg_coord::Outcome::Lease { lease } => lease,
            other => panic!("expected lease: {other:?}"),
        };
        for _ in 0..300 {
            store
                .execute(&mdkg_coord::Command::Heartbeat {
                    owner: "writer".into(),
                    lease: lease.clone(),
                    now: 1,
                })
                .unwrap();
        }
        let head = events::head(&path).unwrap();
        let mut reader =
            Reader::open(&path, "01a185ac-560e-76e0-91af-2c49dfc29b81", Some("start")).unwrap();
        let stop = AtomicBool::new(false);
        let mut out = StopAfterFrame {
            bytes: Vec::new(),
            stop: &stop,
        };
        let mut selected = args();
        selected.types = vec!["mdkg.message.*".into()];
        assert_eq!(
            follow(
                &mut reader,
                Path::new("/repo"),
                &selected,
                &stop,
                &mut out,
                Duration::ZERO,
                &mut GraphHead::new(String::new(), || Err(io::Error::other("no head")))
            ),
            0
        );
        let event: Value = serde_json::from_slice(&out.bytes).unwrap();
        assert_eq!(event["type"], "mdkg.stream.heartbeat");
        assert_eq!(event["data"]["head"], head);
        assert_ne!(
            event["cursor"], head,
            "heartbeat must not wait for the backlog to drain"
        );
        assert_eq!(
            events::head(&path).unwrap(),
            head,
            "heartbeats never write records"
        );
    }
    #[test]
    fn a_set_stop_flag_closes_without_emitting_or_waiting() {
        let fixture = Fixture::new();
        let mut reader = fixture.reader();
        let stop = AtomicBool::new(true);
        let mut out = Vec::new();
        assert_eq!(
            follow(
                &mut reader,
                Path::new("/repo"),
                &args(),
                &stop,
                &mut out,
                Duration::from_secs(15),
                &mut GraphHead::new(String::new(), || Err(io::Error::other("no head")))
            ),
            0
        );
        assert!(out.is_empty());
    }
    #[test]
    fn the_start_is_one_ephemeral_event_and_each_move_another() {
        let fixture = Fixture::new();
        let mut reader = fixture.reader();
        let cursor = reader.cursor().unwrap();
        let stop = AtomicBool::new(false);
        let mut out = StopAfterFrame {
            bytes: Vec::new(),
            stop: &stop,
        };
        // A failed read first: the start is the first read that succeeds.
        let mut reads = vec![Ok(Some("c1".to_owned())), Err(io::Error::other("no git"))];
        let mut head = GraphHead::new("g1".into(), move || reads.pop().unwrap());
        let mut selected = args();
        selected.types = vec!["mdkg.graph.*".into()];
        assert_eq!(
            follow(
                &mut reader,
                Path::new("/repo"),
                &selected,
                &stop,
                &mut out,
                Duration::from_secs(15),
                &mut head
            ),
            0
        );
        let event: Value = serde_json::from_slice(&out.bytes).unwrap();
        assert_eq!(event["type"], "mdkg.graph.head_changed");
        assert_eq!(event["data"], json!({"graph":"g1","head":"c1"}));
        assert_eq!(event["subjects"], json!([events::path("graphs", "g1")]));
        assert_eq!(event["durable"], false);
        assert_eq!(event["cursor"], cursor);
        // A channel filter doesn't select it.
        let mut channel = args();
        channel.channel = vec!["dev".into()];
        assert!(!channel.selects("mdkg.graph.head_changed", &[events::path("graphs", "g1")]));
        assert!(args().selects("mdkg.graph.head_changed", &[events::path("graphs", "g1")]));
    }
    #[test]
    fn a_head_is_announced_at_the_start_and_on_each_change_naming_none_too() {
        let mut reads: Vec<HeadRead> = vec![
            Ok(Some("c3".to_owned())),
            Ok(None),
            Ok(None),
            Ok(Some("c2".to_owned())),
            Err(io::Error::other("no git")),
            Ok(Some("c1".to_owned())),
            Ok(Some("c1".to_owned())),
        ];
        let mut head = GraphHead::new("g1".into(), move || reads.pop().unwrap());
        let seen: Vec<_> = (0..7).map(|_| head.moved(Duration::ZERO)).collect();
        let c = |h: &str| Some(Some(h.to_owned()));
        // Start c1, the same c1, a failed read, a move to c2, no commit
        // (announced once), c3.
        assert_eq!(
            seen,
            [c("c1"), None, None, c("c2"), Some(None), None, c("c3")]
        );
        // A stream that starts unborn says so, then announces the first
        // commit.
        let mut reads: Vec<HeadRead> = vec![Ok(Some("c1".to_owned())), Ok(None)];
        let mut head = GraphHead::new("g1".into(), move || reads.pop().unwrap());
        assert_eq!(head.moved(Duration::ZERO), Some(None));
        assert_eq!(head.moved(Duration::ZERO), c("c1"));
    }
    #[test]
    fn an_unborn_head_reads_as_none_and_a_failed_read_as_an_error() {
        let fixture = Fixture::new();
        let repo = fixture.0.join("repo");
        let git = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(git.success());
        assert!(matches!(
            crate::coord::commit_named(&repo, "HEAD"),
            Ok(None)
        ));
        // Not a repository at all: git fails, which isn't "no commit".
        let outside = fixture.0.join("none");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join(".git"), "gitdir: missing\n").unwrap();
        assert!(crate::coord::commit_named(&outside, "HEAD").is_err());
    }
    #[test]
    fn a_filter_that_leaves_the_head_out_never_reads_it() {
        let fixture = Fixture::new();
        let mut reader = fixture.reader();
        let stop = AtomicBool::new(false);
        let mut out = StopAfterFrame {
            bytes: Vec::new(),
            stop: &stop,
        };
        let mut channel = args();
        channel.channel = vec!["dev".into()];
        let mut head = GraphHead::new("g1".into(), || panic!("read the head"));
        // A zero heartbeat ends the first pass with a frame, which stops it.
        assert_eq!(
            follow(
                &mut reader,
                Path::new("/repo"),
                &channel,
                &stop,
                &mut out,
                Duration::ZERO,
                &mut head
            ),
            0
        );
        let event: Value = serde_json::from_slice(&out.bytes).unwrap();
        assert_eq!(event["type"], "mdkg.stream.heartbeat");
    }
    #[test]
    fn type_and_resource_filters_accept_unknown_types_and_match_exact_channel_names() {
        let mut args = args();
        args.types = vec!["mdkg.message.*".into()];
        args.channel = vec!["dev".into()];
        args.mailbox = Some("b".into());
        assert!(args.validate().is_ok());
        let mut event = Event {
            schema: "mdkg.api.event.v1",
            id: "future".into(),
            cursor: "future".into(),
            kind: "mdkg.message.future".into(),
            time: "future".into(),
            actor: "a".into(),
            subjects: vec![
                events::path("channels", "dev-other"),
                events::path("mailboxes", "b"),
            ],
            durable: true,
            data: json!({}),
        };
        assert!(!args.matches(&event));
        event.subjects[0] = events::path("channels", "dev");
        assert!(args.matches(&event));
        event.subjects[1] = events::path("mailboxes", "c");
        assert!(!args.matches(&event));
        args.types = vec!["mdkg.*.sent".into()];
        assert!(args.validate().is_err());
    }
}
