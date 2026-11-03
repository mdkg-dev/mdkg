//! S3's start, for serve-local (dec-01a18dd9): the settings; the `serve`
//! lock; the local socket; `serve.json`. A failure before the socket is
//! bound releases the lock, by dropping it, and binds nothing (R4); one
//! after removes what it bound before the lock goes.
//!
//! serve-local opens no store and needs no lease mode: each operation is
//! the CLI's, which opens the store, upgrades it and keeps its own leases
//! and transactions as it always does.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::GraphError;
use mdkg_serve::boot::{BootId, Clocks, SystemClocks};
use mdkg_serve::state::{LockError, STATE_SCHEMA, ServeLock, ServeState};

use super::binary::Binary;
use super::config::{self, Settings};
use super::local::{Dispatch, Identity, Routes, Token, TurnCheck, turn_check};
use super::{ServeData, rfc3339_ms, socket, token};

/// How long a start waits while a lease-mode switch holds the `serve`
/// lock; any other holder is refused at once.
pub const SWITCH_WAIT: Duration = Duration::from_secs(30);

/// What the command line gave.
#[derive(Debug, Clone)]
pub struct Options<'a> {
    pub root: &'a Path,
    /// `--listen`: the loopback address and port.
    pub listen: Option<&'a str>,
    /// Ref mode: the graph id committed on the graph ref. `root` is then
    /// the checkout, whose `.mdkg` (if any) isn't the graph.
    pub graph_id: Option<&'a str>,
    /// Ref mode: the graph ref's committed `.mdkg/config.json`, for the
    /// ignored-`serve` warning.
    pub graph_config: Option<&'a serde_json::Value>,
}

/// Where the start reads its environment from, so tests never touch the
/// operator's home.
pub struct Env {
    pub config: config::Env,
    /// Where a socket whose path is too long goes.
    pub runtime: PathBuf,
    /// `<state>`, for the loopback token; `None` reads it as mdkg-wire
    /// does (`$XDG_STATE_HOME`, else `$HOME/.local/state`).
    pub state: Option<PathBuf>,
    pub clocks: Arc<dyn Clocks>,
    /// The program serve spawns and watches: `None` for its own; a test
    /// names a copy it can replace.
    pub program: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        let var = |k| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Self {
            config: config::Env::from_process(),
            runtime: socket::runtime_dir(
                var("XDG_RUNTIME_DIR").as_deref(),
                var("TMPDIR").as_deref(),
            ),
            state: None,
            clocks: Arc::new(SystemClocks::new()),
            program: None,
        }
    }
}

/// What answers requests, and the turn check: production's, or a
/// test's.
pub enum Answer {
    Production,
    #[cfg_attr(not(test), expect(dead_code, reason = "tests inject their own"))]
    With(Arc<dyn Dispatch>),
    #[cfg_attr(not(test), expect(dead_code, reason = "tests inject their own"))]
    WithTurns(Arc<dyn Dispatch>, TurnCheck),
}

/// A started `serve`, listening, before its threads run.
pub struct Ready {
    pub(super) lock: ServeLock,
    pub(super) dispatch: Arc<dyn Dispatch>,
    pub(super) unix: UnixListener,
    pub(super) tcp: Option<TcpListener>,
    pub(super) token: Option<Token>,
    pub(super) turn: TurnCheck,
    pub(super) binary: Arc<Binary>,
    pub(super) socket: PathBuf,
    pub(super) common: PathBuf,
    pub(super) state: ServeState,
    pub(super) settings: Settings,
    data: ServeData,
    warnings: Vec<Diagnostic>,
}

impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ready")
            .field("data", &self.data)
            .finish_non_exhaustive()
    }
}

impl Ready {
    pub fn data(&self) -> &ServeData {
        &self.data
    }

    pub fn warnings(&self) -> &[Diagnostic] {
        &self.warnings
    }
}

/// A usage refusal for an argument, naming the flag, never what was typed.
fn bad_arg(message: &str, hint: &str) -> GraphError {
    GraphError::usage(Code::UsageInvalidArguments, message).with_hint(hint)
}

/// `--listen`'s address: `127.0.0.1:<port>` or `[::1]:<port>`, with an
/// explicit port. Nothing else is ever bound (dec-01a18dd9): no other
/// address, no unspecified one, no port the system picks. Messages name
/// the flag, never what was typed.
pub fn listen_addr(listen: Option<&str>) -> Result<Option<SocketAddr>, GraphError> {
    let Some(text) = listen else {
        return Ok(None);
    };
    let addr: SocketAddr = text.parse().map_err(|_| {
        bad_arg(
            "--listen (or serve.listen) isn't an IP address and port",
            "give the loopback address and a port: 127.0.0.1:7461 or [::1]:7461",
        )
    })?;
    let loopback = matches!(addr.ip(), IpAddr::V4(ip) if ip == Ipv4Addr::LOCALHOST)
        || matches!(addr.ip(), IpAddr::V6(ip) if ip == Ipv6Addr::LOCALHOST);
    if !loopback {
        return Err(bad_arg(
            "--listen (or serve.listen) names an address other than the loopback one",
            "serve listens only on 127.0.0.1 or [::1]; reach it from elsewhere by your own means",
        ));
    }
    if addr.port() == 0 {
        return Err(bad_arg(
            "--listen (or serve.listen) needs an explicit port",
            "name the port, such as 127.0.0.1:7461",
        ));
    }
    Ok(Some(addr))
}

/// Loopback with no `run.wrapper`: the token stands for "outside a turn"
/// only while a sandbox keeps turns from reading it, and without the
/// wrapper none does (a stated residual, #12103).
fn unsandboxed() -> Diagnostic {
    Diagnostic::warning(
        Code::RunSandboxMismatch,
        "loopback is on and no run.wrapper is configured, so an agent turn can read the token",
    )
    .with_hint(
        "configure run.wrapper in your user config to sandbox turns, \
         or serve only the local socket (no --listen)",
    )
}

fn io_failed(what: &str, e: impl std::fmt::Display) -> GraphError {
    GraphError::failure(Code::IoWriteFailed, format!("{what}: {e}"))
}

fn lock_error(e: LockError) -> GraphError {
    match e {
        LockError::Held => GraphError::conflict(
            Code::ApiServeRunning,
            "another serve is already running for this repository",
        )
        .with_hint(format!(
            "`{} serve status` names it; stop it before starting another",
            mdkg_format::program::BIN
        )),
        LockError::Switching => GraphError::conflict(
            Code::ApiRemoteConflict,
            "a lease-mode switch still holds the serve lock",
        )
        .with_hint("start serve again once the switch has finished"),
        LockError::Io(e) => GraphError::failure(
            Code::IoWriteFailed,
            format!("can't take the serve lock: {e}"),
        ),
    }
}

/// S3's start, up to listening; [`super::run::run`] starts the threads
/// and waits for the stop. `on_locked` runs once the `serve` lock is
/// held, before anything is bound: the command installs its signal
/// handlers there.
pub fn start(
    opts: &Options<'_>,
    env: &Env,
    answer: Answer,
    on_locked: &mut dyn FnMut(),
) -> Result<Ready, GraphError> {
    // 1. serve's own program, fingerprinted first; the settings and the
    // graph.
    let binary = Arc::new(
        match &env.program {
            None => Binary::at_start(),
            Some(p) => Binary::new(p, p),
        }
        .map_err(|e| io_failed("can't fingerprint serve's own program", e))?,
    );
    // A committed config from the gate means ref mode: the checkout's
    // config file isn't the graph's, so its project layer isn't read
    // (#16620); the graph ref's is checked for `serve` instead (#16656).
    let (settings, mut warnings) = config::read(opts.root, &env.config, opts.graph_config)?;
    let listen = listen_addr(opts.listen.or(settings.listen.as_deref()))?;
    if listen.is_some() && !settings.wrapper {
        warnings.push(unsandboxed());
    }
    let graph_id = match opts.graph_id {
        Some(g) => Some(g.to_owned()),
        None => mdkg_graph::config::load(opts.root)?
            .graph_id
            .map(|g| g.to_string()),
    }
    .ok_or_else(|| {
        GraphError::usage(
            Code::ConfigInvalid,
            "serve needs a format 0.7 graph with a graph_id",
        )
        .with_hint(format!("run `{} migrate` first", mdkg_format::program::BIN))
    })?;

    // 2. The lock: any failure from here to the bind drops it.
    let common = super::common_dir(opts.root)?;
    let lock = ServeLock::acquire_after_switch(&common, SWITCH_WAIT).map_err(lock_error)?;
    on_locked();
    let now = env.clocks.wall_ms();
    let identity = Identity {
        serve_id: lock.serve_id().to_owned(),
        graph_id: graph_id.clone(),
    };
    let (dispatch, turn): (Arc<dyn Dispatch>, TurnCheck) = match answer {
        Answer::Production => {
            let watch = super::events::Watch {
                binary: binary.clone(),
                root: opts.root.to_path_buf(),
            };
            let ops = super::ops::Ops::new(opts.root, &graph_id, binary.clone())
                .map_err(|e| io_failed("can't find serve's own program", e))?;
            (
                Arc::new(Routes {
                    identity,
                    watch,
                    ops: Some(ops),
                }) as Arc<dyn Dispatch>,
                turn_check(common.clone()),
            )
        }
        Answer::With(d) => (d, turn_check(common.clone())),
        Answer::WithTurns(d, t) => (d, t),
    };

    // 3. The socket, then serve.json.
    let uid = mdkg_serve::state::own_uid();
    let socket_path = socket::path_for(lock.dir().path(), &env.runtime, uid);
    let fallback = socket_path.parent() != Some(lock.dir().path());
    let unix = socket::bind(&socket_path, uid, fallback)
        .map_err(|e| io_failed("can't listen on the graph's local socket", e))?;
    let unbind = |e: GraphError| {
        let _ = std::fs::remove_file(&socket_path);
        e
    };
    let (tcp, token, token_file) = match listen {
        None => (None, None, None),
        Some(addr) => {
            let state = match &env.state {
                Some(s) => s.clone(),
                None => mdkg_wire::keys::state_dir()
                    .map_err(|e| unbind(io_failed("can't find the state folder", e)))?,
            };
            let path = token::path_for(&state, &common);
            let token = token::load_or_mint(&path, uid)
                .map_err(|e| unbind(io_failed("can't read or mint the loopback token", e)))?;
            // A port that's taken fails the start visibly: no other port
            // is tried.
            let listener = TcpListener::bind(addr)
                .map_err(|e| unbind(io_failed("can't listen on --listen's address", e)))?;
            (Some(listener), Some(token), Some(path))
        }
    };
    let tcp_addr = match &tcp {
        Some(l) => Some(
            l.local_addr()
                .map_err(|e| unbind(io_failed("can't read the loopback address", e)))?
                .to_string(),
        ),
        None => None,
    };
    let boot = BootId::mint()
        .map_err(|e| unbind(io_failed("can't mint a boot id", e)))?
        .as_hex();
    let started_at = rfc3339_ms(now);
    let state = ServeState {
        schema: STATE_SCHEMA.to_owned(),
        serve_id: lock.serve_id().to_owned(),
        boot_id: boot,
        pid: std::process::id(),
        started_at: started_at.clone(),
        socket: socket_path.to_string_lossy().into_owned(),
        build: crate::version::LINE.to_owned(),
        tcp: tcp_addr.clone(),
        token_file: token_file
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
    };
    state
        .write(&lock)
        .map_err(|e| unbind(io_failed("can't write serve.json", e)))?;
    let data = ServeData {
        serve_id: lock.serve_id().to_owned(),
        pid: std::process::id(),
        graph_id,
        socket: state.socket.clone(),
        tcp: tcp_addr,
        token_file: state.token_file.clone(),
        capabilities: vec![super::local::CAPABILITY.to_owned()],
        drain_grace_s: settings.drain_grace.as_secs(),
        started_at,
        build: crate::version::LINE.to_owned(),
    };
    Ok(Ready {
        lock,
        dispatch,
        unix,
        tcp,
        token,
        turn,
        binary,
        socket: socket_path,
        common,
        state,
        settings,
        data,
        warnings,
    })
}
