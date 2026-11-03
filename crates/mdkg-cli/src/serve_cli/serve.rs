//! `mdkg serve`, serve-local (dec-01a18dd9; API v1 A15, A16): the CLI's
//! operations over HTTP/1.1 on the graph's local socket, whose file mode
//! is the authentication, as the store file's is for the CLI.
//!
//! - **The start** ([`start`]): the settings; the `serve` lock; the local
//!   socket (`local.sock`, 0600 in a 0700 folder); `serve.json`; one
//!   `mdkg.serve.v1` envelope, then JSON lines on stderr. A failure before
//!   the socket releases the lock and binds nothing (R4).
//! - **The listener** ([`local`]): no signature Gate and no served lease
//!   mode; a peer of another user is refused. It answers `GET /`, and the
//!   CLI's refusal for what it doesn't serve yet.
//! - **Loopback**, opt-in (`--listen 127.0.0.1:<port>` or `[::1]`, from
//!   the user layer or the command line only): a bearer token from a 0600
//!   file outside the repository ([`token`]); any `Origin` and any `Host`
//!   but localhost refused; nothing but loopback ever bound.
//! - **The stop** ([`run`]): SIGTERM or SIGINT drains (R5); a second
//!   signal ends the grace. Exit 0 after a clean stop, non-zero after a
//!   failed start or when the 10 s limit cut a request off.
//! - **`mdkg serve status`**: what `serve.json` and `serve.live` say about
//!   this repository's `serve` (S3's discovery), never probing a pid.
//!
//! `serve` runs no harness and never re-execs itself: an upgrade is a
//! restart (S3). The networked design (signed requests, TLS, enrolment,
//! person keys) is the later one; its code in mdkg-serve and mdkg-wire
//! stays unwired.

mod binary;
mod config;
mod events;
mod local;
mod ops;
mod run;
mod socket;
mod start;
#[cfg(test)]
mod tests;
mod token;

use std::path::Path;

use clap::{Args, Subcommand};
use mdkg_format::Code;
use mdkg_graph::GraphError;
use mdkg_serve::state::{Found, ServeState};
use schemars::JsonSchema;
use serde::Serialize;

use crate::output;

/// `mdkg serve`'s arguments. `--root` and `--allow-store-upgrade` are the
/// global options, which `serve` takes as every command does.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ServeArgs {
    #[command(subcommand)]
    pub action: Option<ServeAction>,
    /// Also listen on loopback TCP, 127.0.0.1:<port> or [::1]:<port>,
    /// with a bearer token (overrides `serve.listen`).
    #[arg(long, value_name = "ADDR:PORT")]
    pub listen: Option<String>,
    /// Print the start as one JSON envelope.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum ServeAction {
    /// Whether this repository's serve is running, from serve.json.
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

impl ServeArgs {
    pub fn json(&self) -> bool {
        match &self.action {
            Some(ServeAction::Status { json }) => *json,
            None => self.json,
        }
    }
}

/// `mdkg.serve.v1`: what a started `serve` serves.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ServeData {
    /// Fresh at every start.
    pub serve_id: String,
    pub pid: u32,
    pub graph_id: String,
    /// The graph's local socket: `<common>/mdkg/local.sock`, or its short
    /// fallback.
    pub socket: String,
    /// The loopback address, when `--listen` or `serve.listen` gave one.
    pub tcp: Option<String>,
    /// The loopback listener's token file (0600): its path, never the
    /// token.
    pub token_file: Option<String>,
    /// What this serve offers: `serve.local` for the unsigned local
    /// socket.
    pub capabilities: Vec<String>,
    pub drain_grace_s: u64,
    pub started_at: String,
    pub build: String,
}

/// `mdkg.serve.status.v1`: what `serve.json` says, checked against
/// `serve.live`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ServeStatusData {
    /// `running`; `starting` (`serve.live` is held by a start that hasn't
    /// written its file yet); `stale` (no live holder: a crashed serve's
    /// file, replaced at the next start); `absent`; or `unreadable`.
    pub state: String,
    pub serve: Option<StateOut>,
}

/// `serve.json` (`mdkg.serve.state.v1`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StateOut {
    pub serve_id: String,
    pub boot_id: String,
    pub pid: u32,
    pub started_at: String,
    pub socket: String,
    pub build: String,
    pub tcp: Option<String>,
    /// The loopback token file's path, never the token.
    pub token_file: Option<String>,
}

impl From<ServeState> for StateOut {
    fn from(s: ServeState) -> Self {
        Self {
            serve_id: s.serve_id,
            boot_id: s.boot_id,
            pid: s.pid,
            started_at: s.started_at,
            socket: s.socket,
            build: s.build,
            tcp: s.tcp,
            token_file: s.token_file,
        }
    }
}

/// Runs `mdkg serve` or `mdkg serve status` for the graph at `root`.
/// `gate` is the ref-mode graph's committed authority.
pub fn run(root: &Path, gate: Option<&mdkg_graph::graphref::Authority>, args: &ServeArgs) -> u8 {
    match &args.action {
        Some(ServeAction::Status { json }) => status(root, *json),
        None => serve(root, gate, args),
    }
}

fn serve(root: &Path, gate: Option<&mdkg_graph::graphref::Authority>, args: &ServeArgs) -> u8 {
    // Every stderr line is JSON from here, panics included.
    log::json_panics();
    let opts = start::Options {
        root,
        listen: args.listen.as_deref(),
        graph_id: gate.map(|g| g.graph_id.as_str()),
        graph_config: gate.map(|g| &g.config),
    };
    // SIGTERM and SIGINT are taken over once the lock is held, before
    // anything is bound: one that comes while serve binds drains it once
    // the start is done; one before the lock (a start waiting on a
    // lease-mode switch) stops it as usual, with nothing to leave behind
    // (#11393 item 2, #11588 item 2).
    let mut signals = None;
    let ready = match start::start(
        &opts,
        &start::Env::from_process(),
        start::Answer::Production,
        &mut || signals = Some(run::Signals::install()),
    ) {
        Ok(r) => r,
        Err(e) => {
            log::line(
                "error",
                "serve.start_failed",
                serde_json::json!({
                    "code": e.diagnostic.code.as_str(),
                }),
            );
            return output::emit("serve", args.json, output::Report::<()>::failed(e), |_| {});
        }
    };
    let data = ready.data().clone();
    let warnings = ready.warnings().to_vec();
    output::emit(
        "serve",
        args.json,
        output::Report::ok(data.clone()).with(warnings),
        |d| {
            println!(
                "serve: serving graph {} on {}{}",
                d.graph_id,
                d.socket,
                d.tcp
                    .as_deref()
                    .map(|t| format!(" and {t}"))
                    .unwrap_or_default()
            );
        },
    );
    let signals = signals.unwrap_or_else(run::Signals::install);
    run::run(ready, &signals)
}

fn status(root: &Path, json: bool) -> u8 {
    let report = match status_data(root) {
        Ok(d) => output::Report::ok(d),
        Err(e) => output::Report::failed(e),
    };
    output::emit("serve.status", json, report, |d| match &d.serve {
        Some(s) => println!(
            "serve {}: {} (pid {}, since {}) on {}",
            d.state, s.serve_id, s.pid, s.started_at, s.socket
        ),
        None => println!("serve {}", d.state),
    })
}

fn status_data(root: &Path) -> Result<ServeStatusData, GraphError> {
    let common = common_dir(root)?;
    let found = ServeState::find(&common).map_err(|e| {
        GraphError::failure(Code::IoReadFailed, format!("can't read serve.json: {e}"))
    })?;
    let (state, serve) = match found {
        Found::None => ("absent", None),
        Found::Running(s) => ("running", Some(s.into())),
        Found::Stale(s) => ("stale", Some(s.into())),
        Found::HeldByOther(s) => ("starting", Some(s.into())),
        Found::Unreadable => ("unreadable", None),
    };
    Ok(ServeStatusData {
        state: state.to_owned(),
        serve,
    })
}

/// JSON lines on stderr, one per event, after the start's envelope.
pub mod log {
    use serde_json::{Value, json};

    /// Writes `{"time","level","event",...fields}` as one line.
    pub fn line(level: &str, event: &str, fields: Value) {
        let mut v = json!({
            "time": super::rfc3339_ms(now_ms()),
            "level": level,
            "event": event,
        });
        if let (Some(obj), Value::Object(f)) = (v.as_object_mut(), fields) {
            obj.extend(f);
        }
        // A stderr that's gone (its reader exited) loses the line, never
        // panics: a panic here, inside the panic hook, would abort serve
        // before its orderly stop (#11748).
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "{v}");
    }

    /// The fields a panic is logged with: the thread's name and the
    /// panic's location, never its payload, which may carry whatever the
    /// panicking code formatted.
    pub fn panic_fields(thread: Option<&str>, location: Option<(&str, u32)>) -> Value {
        json!({
            "thread": thread,
            "file": location.map(|(f, _)| f),
            "line": location.map(|(_, l)| l),
        })
    }

    /// Replaces std's panic hook for this process, so a panic is one JSON
    /// line on stderr like every other log line, not std's plain text
    /// (#11685). The panic still unwinds as before.
    pub fn json_panics() {
        std::panic::set_hook(Box::new(|info| {
            let thread = std::thread::current();
            let location = info.location().map(|l| (l.file(), l.line()));
            line(
                "error",
                "serve.panicked",
                panic_fields(thread.name(), location),
            );
        }));
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn a_panic_is_logged_by_place_never_by_payload() {
            let v = super::panic_fields(Some("main"), Some(("src/x.rs", 7)));
            assert_eq!(
                v,
                serde_json::json!({"thread": "main", "file": "src/x.rs", "line": 7})
            );
            let v = super::panic_fields(None, None);
            assert_eq!(
                v,
                serde_json::json!({"thread": null, "file": null, "line": null})
            );
        }
    }
}

/// The repository's git common dir, as the store finds it.
fn common_dir(root: &Path) -> Result<std::path::PathBuf, GraphError> {
    let common = mdkg_coord_sqlite::git::common_dir(root).map_err(|e| {
        GraphError::failure(
            Code::IoReadFailed,
            format!("can't find the git common dir: {e}"),
        )
    })?;
    Ok(std::fs::canonicalize(&common).unwrap_or(common))
}

/// RFC 3339 UTC with milliseconds.
pub fn rfc3339_ms(ms: u64) -> String {
    let secs = i64::try_from(ms / 1000).unwrap_or(i64::MAX);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

/// Days since 1970-01-01 as a proleptic Gregorian date (Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
