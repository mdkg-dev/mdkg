//! `mdkg git-serve <machine_id>`: the receiving side of git between
//! machines (the serve edd's S38, R85, R125 and R126).
//!
//! sshd runs it as a key's forced command, as the repository owner. It
//! cleans its environment, accepts exactly `git-upload-pack '<path>'` or
//! `git-receive-pack '<path>'` for a listed repository, checks the
//! machine in the store (read-only), takes the machine's git fence
//! shared, checks again, installs and verifies its hook folder, and runs
//! git by absolute path with no shell. The receiving hooks are
//! `git_hook`'s.
//!
//! **The fence outlives this process.** The fence's file descriptor is
//! handed to git without close-on-exec, so git and every hook it runs hold
//! the same lock. `machine revoke` and `agents retire`, which take it
//! exclusively, wait for the process that moves the refs, even when
//! `git-serve` itself died first (Terra 2 #6271).
//!
//! The store's machine and placement rows, the graph-ref authority and
//! the secret scan belong to other lanes. They are behind [`Book`],
//! [`AuthoritySource`] and [`Scanner`]; [`production`] gives the real
//! ones.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mdkg_format::Code;
use mdkg_graph::graphref;
use mdkg_run::orchestration::Names;
use serde_json::Value;

/// The only variables `git-serve` keeps from the environment sshd and the
/// owner's shell gave it (S38 step 1).
pub const KEEP: [&str; 5] = [
    "HOME",
    "USER",
    "LOGNAME",
    "SSH_ORIGINAL_COMMAND",
    "SSH_CONNECTION",
];

/// The pushing machine, for the hooks.
pub const MACHINE_VAR: &str = "MDKG_GIT_SERVE_MACHINE";
/// The git the hooks run, the one `git-serve` ran.
pub const GIT_VAR: &str = "MDKG_GIT_SERVE_GIT";

/// `git_serve.push_timeout_s`'s default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// `git_serve.git`'s default.
pub const DEFAULT_GIT: &str = "/usr/bin/git";
/// The hooks `git-serve` installs: its folder holds exactly these.
pub const HOOKS: [&str; 2] = ["pre-receive", "update"];

/// Why a connection or a push is refused: a code and a message for the
/// client's stderr. Messages never carry what a scan matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: Code,
    pub message: String,
}

impl Refusal {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn io(what: &str, e: io::Error) -> Self {
        Self::new(Code::IoWriteFailed, format!("{what}: {e}"))
    }
}

/// The environment with only [`KEEP`]'s variables, in that order.
pub fn clean_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let all: Vec<(OsString, OsString)> = vars.into_iter().collect();
    KEEP.iter()
        .filter_map(|k| all.iter().find(|(n, _)| n == OsStr::new(k)).cloned())
        .collect()
}

fn var<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
    env.iter()
        .find(|(n, _)| n == OsStr::new(name))
        .map(|(_, v)| v.as_os_str())
}

/// The two git services `git-serve` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    UploadPack,
    ReceivePack,
}

impl Service {
    pub fn as_str(self) -> &'static str {
        match self {
            Service::UploadPack => "git-upload-pack",
            Service::ReceivePack => "git-receive-pack",
        }
    }
}

/// `SSH_ORIGINAL_COMMAND`, parsed strictly (S38 step 2): exactly
/// `git-upload-pack '<path>'` or `git-receive-pack '<path>'`, one space,
/// one single-quoted path holding no quote, backslash, `!` or control
/// character. Anything else is refused, never handed to a shell.
pub fn parse_command(command: &str) -> Result<(Service, String), Refusal> {
    let refuse = || {
        Refusal::new(
            Code::ApiActorNotAllowed,
            "git-serve runs only `git-upload-pack '<path>'` or `git-receive-pack '<path>'`",
        )
    };
    let (program, quoted) = command.split_once(' ').ok_or_else(refuse)?;
    let service = match program {
        "git-upload-pack" => Service::UploadPack,
        "git-receive-pack" => Service::ReceivePack,
        _ => return Err(refuse()),
    };
    let path = quoted
        .strip_prefix('\'')
        .and_then(|q| q.strip_suffix('\''))
        .ok_or_else(refuse)?;
    if path.is_empty() || path.chars().any(|c| c.is_control() || "'\\!".contains(c)) {
        return Err(refuse());
    }
    Ok((service, path.to_owned()))
}

/// A machine id: a UUID in lowercase hyphenated form, so it's safe as a
/// file name.
pub fn check_machine_id(id: &str) -> Result<(), Refusal> {
    match uuid::Uuid::parse_str(id) {
        Ok(u) if u.hyphenated().to_string() == id => Ok(()),
        _ => Err(Refusal::new(
            Code::ApiUnauthenticated,
            "the forced command names no valid machine id",
        )),
    }
}

/// `git_serve` in the owner's user config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub repositories: Vec<PathBuf>,
    pub push_timeout: Duration,
    pub git: PathBuf,
}

/// The user config file under a cleaned environment, which has no
/// `XDG_CONFIG_HOME`.
pub fn config_path(home: &Path) -> PathBuf {
    home.join(".config").join("mdkg").join("config.json")
}

/// Reads `git_serve` from the owner's user config.
pub fn host_config(home: &Path) -> Result<HostConfig, Refusal> {
    let path = config_path(home);
    let mut bytes = Vec::new();
    match File::open(&path) {
        Ok(f) => f
            .take(mdkg_format::settings::MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Refusal::io("can't read the host config", e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Refusal::new(
                Code::ApiUnknownResource,
                "this host serves no repositories (git_serve.repositories)",
            ));
        }
        Err(e) => return Err(Refusal::io("can't read the host config", e)),
    };
    if bytes.len() as u64 > mdkg_format::settings::MAX_BYTES {
        return Err(Refusal::new(
            Code::ConfigInvalid,
            "the host config is over 1 MiB",
        ));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
        Refusal::new(
            Code::ConfigInvalid,
            format!("the host config isn't JSON: {e}"),
        )
    })?;
    parse_host_config(&value)
}

/// `git_serve`'s keys: `repositories` (absolute paths), `push_timeout_s`
/// (1 to 86400) and `git` (an absolute path).
pub fn parse_host_config(value: &Value) -> Result<HostConfig, Refusal> {
    let bad = |what: &str| Refusal::new(Code::ConfigInvalid, format!("git_serve.{what}"));
    let section = match value.get("git_serve") {
        None | Some(Value::Null) => &Value::Null,
        Some(v @ Value::Object(_)) => v,
        Some(_) => return Err(bad("must be an object")),
    };
    let mut repositories = Vec::new();
    match section.get("repositories") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            for item in list {
                match item.as_str().map(PathBuf::from) {
                    Some(p) if p.is_absolute() => repositories.push(p),
                    _ => return Err(bad("repositories must list absolute paths")),
                }
            }
        }
        Some(_) => return Err(bad("repositories must be a list")),
    }
    let push_timeout = match section.get("push_timeout_s") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT,
        Some(v) => match v.as_u64() {
            Some(s @ 1..=86_400) => Duration::from_secs(s),
            _ => {
                return Err(bad(
                    "push_timeout_s must be a whole number of seconds, 1 to 86400",
                ));
            }
        },
    };
    let git = match section.get("git") {
        None | Some(Value::Null) => PathBuf::from(DEFAULT_GIT),
        Some(Value::String(s)) if Path::new(s).is_absolute() => PathBuf::from(s),
        Some(_) => return Err(bad("git must be an absolute path")),
    };
    Ok(HostConfig {
        repositories,
        push_timeout,
        git,
    })
}

/// The listed repository the client's path names, by canonical path (S38
/// step 3). A relative path or `~/...` is from `home`, as sshd's working
/// folder is.
pub fn resolve_repo(path: &str, home: &Path, listed: &[PathBuf]) -> Result<PathBuf, Refusal> {
    let refuse = || {
        Refusal::new(
            Code::ApiUnknownResource,
            "that path isn't a repository this host serves",
        )
    };
    let asked = match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => home.join(path),
    };
    if Path::new(path)
        .components()
        .any(|c| c == Component::ParentDir)
    {
        return Err(refuse());
    }
    let asked = fs::canonicalize(asked).map_err(|_| refuse())?;
    listed
        .iter()
        .filter_map(|l| fs::canonicalize(l).ok())
        .find(|l| *l == asked)
        .ok_or_else(refuse)
}

/// A listed repository's git folder: `<repo>/.git` for a checkout, or the
/// repository itself when bare. A linked worktree isn't served.
pub fn common_dir(repo: &Path) -> Result<PathBuf, Refusal> {
    let dot = repo.join(".git");
    match fs::symlink_metadata(&dot) {
        Ok(m) if m.is_dir() => return Ok(dot),
        Ok(_) => {
            return Err(Refusal::new(
                Code::ApiUnknownResource,
                "the listed repository is a linked worktree; list its main checkout",
            ));
        }
        Err(_) => {}
    }
    if repo.join("HEAD").is_file() && repo.join("objects").is_dir() {
        return Ok(repo.to_path_buf());
    }
    Err(Refusal::new(
        Code::ApiUnknownResource,
        "the listed repository isn't a git repository",
    ))
}

/// A machine's row in the graph's store (S6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineRow {
    Approved,
    Pending,
    Revoking,
    Revoked,
    Unknown,
}

/// An agent's placement row (S12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementState {
    Placed,
    Retiring,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub machine: String,
    pub state: PlacementState,
}

/// The store's machine and placement rows, opened read-only (R125). Every
/// call reads the store again: nothing is cached between calls.
pub trait Book {
    fn machine(&self, id: &str) -> Result<MachineRow, Refusal>;
    fn placement(&self, agent: &str) -> Result<Option<Placement>, Refusal>;
}

/// Refuses unless the machine is approved (S38 step 5).
pub fn check_approved(book: &dyn Book, machine: &str) -> Result<(), Refusal> {
    match book.machine(machine)? {
        MachineRow::Approved => Ok(()),
        MachineRow::Revoking | MachineRow::Revoked => Err(Refusal::new(
            Code::ApiMachineRevoked,
            "this machine's key was revoked for this graph",
        )),
        MachineRow::Pending => Err(Refusal::new(
            Code::ApiMachinePending,
            "this machine's enrolment waits for a person to approve it",
        )),
        MachineRow::Unknown => Err(Refusal::new(
            Code::ApiUnauthenticated,
            "this machine isn't enrolled in this graph",
        )),
    }
}

/// The main ref when `mdkg.mainRef` is unset.
pub const DEFAULT_MAIN_REF: &str = "refs/heads/main";

/// Every ref the main ref stands for, all protected (Terra #11168): the
/// resolved ref, and when it resolves to a remote-tracking
/// `refs/remotes/<remote>/<name>`, the local branches it could mean,
/// `refs/heads/<name>` and `refs/heads/<remote>/<name>`. Its protection
/// doesn't depend on how it resolves.
pub fn protected_main(main_ref: &str) -> Vec<String> {
    let mut refs = vec![main_ref.to_owned()];
    if let Some(remote) = main_ref.strip_prefix("refs/remotes/") {
        refs.push(format!("refs/heads/{remote}"));
        if let Some((_, name)) = remote.split_once('/') {
            refs.push(format!("refs/heads/{name}"));
        }
    }
    refs.dedup();
    refs
}

/// The graph's agents at the graph ref's tip, as the hooks need them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agents {
    pub names: Names,
    /// Each defined agent and whether it's retired.
    pub defined: BTreeMap<String, bool>,
    /// Refs no push may move whatever the pattern says: the main ref and
    /// the graph ref.
    pub protected: Vec<String>,
    /// The main ref, full: a pushed branch's net change is taken against
    /// it.
    pub main_ref: String,
    /// The graph-owned folders, repository-relative (`graphref::
    /// graph_paths_dir`): no pushed commit may change a path in one. A
    /// part named `.mdkg` anywhere is refused besides (S42).
    pub graph_folders: Vec<String>,
}

impl Agents {
    /// From the committed config and agent definitions, as
    /// `mdkg_graph::graphref::Authority` holds them, with the root graph
    /// folder `.mdkg` alone until the caller sets the folders.
    pub fn from_authority<'a>(
        config: &Value,
        definitions: impl IntoIterator<Item = (&'a String, &'a Value)>,
        protected: Vec<String>,
    ) -> Result<Self, Refusal> {
        let names = Names::from_config(config)
            .map_err(|e| Refusal::new(Code::ConfigInvalid, e.to_string()))?;
        let defined = definitions
            .into_iter()
            .map(|(name, def)| {
                let retired = def.get("retired").and_then(Value::as_bool) == Some(true);
                (name.clone(), retired)
            })
            .collect();
        Ok(Self {
            names,
            defined,
            protected,
            main_ref: DEFAULT_MAIN_REF.to_owned(),
            graph_folders: vec![mdkg_graph::committed::DEFAULT_DIR.to_owned()],
        })
    }
}

/// The graph's agents, read through `graphref::authority()` (S39).
pub trait AuthoritySource {
    fn agents(&self, git_dir: &Path) -> Result<Agents, Refusal>;
}

/// What the secret scan found: never the matched text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub commit: String,
    pub path: String,
}

/// The secret.scan lane's scan over the commits a push adds (S34).
pub trait Scanner {
    fn scan(
        &self,
        git: &Path,
        git_dir: &Path,
        commits: &[String],
    ) -> Result<Option<Finding>, Refusal>;
}

/// Opens the store of the repository whose git folder it's given.
pub type OpenBook = Box<dyn Fn(&Path) -> Result<Box<dyn Book>, Refusal>>;

/// Where `git-serve` and its hooks get the store, the authority and the
/// scan.
pub struct Deps {
    pub book: OpenBook,
    pub authority: Box<dyn AuthoritySource>,
    pub scanner: Box<dyn Scanner>,
}

/// The deps this build has: the store's rows, the graph ref's authority
/// and `secret.scan`'s scan.
pub fn production() -> Deps {
    Deps {
        book: Box::new(|common| {
            Ok(Box::new(StoreBook {
                conn: check_store(common)?,
            }) as Box<dyn Book>)
        }),
        authority: Box::new(GraphAuthority),
        scanner: Box::new(SecretScan),
    }
}

/// g-core's `graphref::authority()` at the graph ref's tip (S39: never a
/// working tree's or main's), read in the repository's main checkout with
/// the pins naming that checkout and its git folder outright. A bare
/// repository has no checkout for it, and a clone with no graph ref no
/// tip, so both are refused.
struct GraphAuthority;

impl AuthoritySource for GraphAuthority {
    fn agents(&self, git_dir: &Path) -> Result<Agents, Refusal> {
        let top = git_dir
            .parent()
            .filter(|_| git_dir.file_name() == Some(OsStr::new(".git")))
            .ok_or_else(|| {
                Refusal::new(
                    Code::ApiUnsupported,
                    "a bare repository has no checkout to read the graph's agents in; list the repository's main checkout",
                )
            })?
            .to_path_buf();
        graphref::with_git_trust(Some(git_dir.to_path_buf()), Some(top.clone()), || {
            let graph = graphref::resolve(&top)
                .map_err(graph_refusal)?
                .ok_or_else(|| {
                    Refusal::new(
                        Code::GraphRefMissing,
                        "the repository has no graph ref; git-serve reads the agents only from its tip",
                    )
                })?;
            let authority = graphref::authority(&top).map_err(graph_refusal)?;
            let graph_folders = graph_folders(&top, &graph, &authority)?;
            // `mdkg.mainRef` unset is `main`, as mdkg-git's main_ref has it,
            // so an agent named `main` under an `{agent}` pattern still
            // can't move it (#11064).
            let main_ref = graph
                .main_ref
                .clone()
                .unwrap_or_else(|| DEFAULT_MAIN_REF.to_owned());
            let mut agents = Agents::from_authority(
                &authority.config,
                authority.definitions.iter().map(|(n, d)| (n, &d.value)),
                [vec![graph.name], protected_main(&main_ref)].concat(),
            )?;
            agents.main_ref = main_ref;
            agents.graph_folders = graph_folders;
            Ok(agents)
        })
    }
}

/// Every folder a graph recorded on this graph ref owns, repository-
/// relative (Terra #10862): for each graph root, its folder (the `.dir`
/// record, else `.mdkg`) and its committed workspaces' folders, as
/// `graphref::graph_paths_dir` gives them. A nested graph's config is read
/// at the same tip; one that can't be read refuses the push, since its
/// folders would be unknown. Graphs recorded on another graph ref have no
/// tip here: only a `.mdkg` part covers theirs.
fn graph_folders(
    top: &Path,
    graph: &graphref::GraphRef,
    authority: &graphref::Authority,
) -> Result<Vec<String>, Refusal> {
    let folder_of = |id: &str| {
        graph
            .graph_dirs
            .get(id)
            .cloned()
            .unwrap_or_else(|| mdkg_graph::committed::DEFAULT_DIR.to_owned())
    };
    let mut roots: BTreeMap<String, String> = graph
        .graph_ids
        .iter()
        .map(|id| {
            let root =
                graphref::normalize_root(graph.graph_roots.get(id).map_or("", String::as_str));
            (root, folder_of(id))
        })
        .collect();
    // The authority's own graph, at the repository root, recorded or not.
    roots.insert(String::new(), folder_of(&authority.graph_id));
    let mut folders = Vec::new();
    for (root, dir) in roots {
        let config = if root.is_empty() {
            authority.config.clone()
        } else {
            graphref::authority_in(top, &root)
                .map_err(graph_refusal)?
                .config
        };
        let config = mdkg_graph::config::from_snapshot(config)
            .map_err(|e| Refusal::new(Code::GraphAuthorityInvalid, e.to_string()))?;
        for path in graphref::graph_paths_dir(&config, &dir).map_err(graph_refusal)? {
            let path = if root.is_empty() {
                path
            } else {
                format!("{root}/{path}")
            };
            if !folders.contains(&path) {
                folders.push(path);
            }
        }
    }
    folders.sort();
    Ok(folders)
}

fn graph_refusal(e: graphref::Error) -> Refusal {
    Refusal::new(
        Code::parse(e.code).unwrap_or(Code::GraphAuthorityInvalid),
        e.message,
    )
}

/// serve-store's machine and placement rows, through one read-only
/// connection to the store `check_store` opened. Each call is its own
/// query, so the reads before and after the fence see what's committed
/// then (S38 steps 5 and 6).
struct StoreBook {
    conn: rusqlite::Connection,
}

fn store_refusal(e: mdkg_coord_sqlite::serve::ServeError) -> Refusal {
    Refusal::new(Code::CoordStoreFailed, format!("can't read the store: {e}"))
}

impl Book for StoreBook {
    fn machine(&self, id: &str) -> Result<MachineRow, Refusal> {
        use mdkg_coord_sqlite::serve::{self, MachineState};
        if let Some(m) = serve::machine(&self.conn, id).map_err(store_refusal)? {
            return Ok(match m.state {
                MachineState::Approved => MachineRow::Approved,
                MachineState::Revoking => MachineRow::Revoking,
                MachineState::Revoked => MachineRow::Revoked,
            });
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let pending = serve::pending(&self.conn, now).map_err(store_refusal)?;
        Ok(if pending.iter().any(|p| p.machine_id == id) {
            MachineRow::Pending
        } else {
            MachineRow::Unknown
        })
    }

    fn placement(&self, agent: &str) -> Result<Option<Placement>, Refusal> {
        use mdkg_coord_sqlite::serve::{self, PlacementState as Stored};
        Ok(serve::placement(&self.conn, agent)
            .map_err(store_refusal)?
            .map(|p| Placement {
                machine: p.machine_id,
                state: match p.state {
                    Stored::Placed => PlacementState::Placed,
                    Stored::Retiring => PlacementState::Retiring,
                    Stored::Retired => PlacementState::Released,
                },
            }))
    }
}

/// `secret.scan`'s scan (`crate::hook::secret::scan`), the same one git's
/// own hooks run, with `credentials::looks_secret` its only detector. A
/// scan that can't run refuses the push (orchestrator, 2026-10-30), and a
/// finding never carries what matched. A blob over the scan's 16 MiB cap
/// isn't read, and the pusher is warned which.
struct SecretScan;

impl Scanner for SecretScan {
    fn scan(
        &self,
        git: &Path,
        git_dir: &Path,
        commits: &[String],
    ) -> Result<Option<Finding>, Refusal> {
        crate::hook::secret::scan(git, git_dir, commits)
            .map(|scanned| {
                // Blobs too large to read pass with a warning, which the
                // hook's stderr carries to the pusher (orchestrator #11600).
                if !scanned.skipped.is_empty() {
                    eprintln!(
                        "mdkg git-hook: {}",
                        crate::hook::git::unscanned_line(&scanned.skipped)
                    );
                }
                scanned.finding.map(|f| Finding {
                    commit: f.commit,
                    path: f.path,
                })
            })
            .map_err(|e| {
                Refusal::new(
                    Code::IoGitFailed,
                    format!(
                        "the secret scan couldn't run ({}); nothing in this push lands",
                        e.0
                    ),
                )
            })
    }
}

/// Opens the graph's store read-only and refuses one whose versions
/// aren't this build's (R125): `git-serve` never upgrades a store, and
/// can't read a newer one.
pub fn check_store(common: &Path) -> Result<rusqlite::Connection, Refusal> {
    use rusqlite::OptionalExtension;
    let path = common.join("mdkg").join("coord.sqlite");
    let failed = |e: rusqlite::Error| {
        Refusal::new(Code::CoordStoreFailed, format!("can't read the store: {e}"))
    };
    let conn = mdkg_coord_sqlite::Store::open_raw_read_only(&path)
        .map_err(|e| Refusal::new(Code::CoordStoreFailed, format!("can't read the store: {e}")))?
        .ok_or_else(|| {
            Refusal::new(
                Code::ApiUnauthenticated,
                "this graph has no store, so no machine is enrolled",
            )
        })?;
    let read = |key: &str| -> Result<Option<String>, rusqlite::Error> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    };
    let schema = read("schema").map_err(failed)?.unwrap_or_default();
    let label = match read("msg_schema").map_err(failed)? {
        Some(m) => format!("{schema}+msg{m}"),
        None => schema,
    };
    let build = mdkg_coord_sqlite::build_label();
    if label != build {
        return Err(Refusal::new(
            Code::CoordStoreFailed,
            format!(
                "the store is at {label} and this build reads {build}; git-serve never upgrades it"
            ),
        ));
    }
    Ok(conn)
}

/// The machine's fence lock (S38 step 6).
pub fn fence_path(common: &Path, machine: &str) -> PathBuf {
    common
        .join("mdkg")
        .join("git-fence")
        .join(format!("{machine}.lock"))
}

/// Takes the fence shared, waiting up to `timeout` for a revoke or a
/// retirement that holds it.
pub fn take_fence(path: &Path, timeout: Duration) -> Result<File, Refusal> {
    let fail = |e| Refusal::io("can't take the git fence", e);
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(fail)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(fail)?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock_shared() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Refusal::new(
                    Code::ApiDraining,
                    "the machine's git fence is held by a revoke or a retirement; retry",
                ));
            }
            Err(fs::TryLockError::Error(e)) => return Err(fail(e)),
        }
    }
}

/// Whether some process holds the fence: an exclusive try-lock on a new
/// open file description fails while any shared holder lives. The hooks
/// refuse a push without it.
pub fn fence_held(path: &Path) -> Result<bool, Refusal> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Refusal::io("can't read the git fence", e)),
    };
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(e)) => Err(Refusal::io("can't read the git fence", e)),
    }
}

/// Lets a child inherit the fence: the lock is on the open file
/// description, so it lasts until every process holding it exits.
pub fn inherit_fence(file: &File) -> Result<(), Refusal> {
    rustix::io::fcntl_setfd(file, rustix::io::FdFlags::empty())
        .map_err(|e| Refusal::io("can't hand the git fence to git", e.into()))
}

/// The hook folder for the running build (S38 step 7):
/// `<owner state>/mdkg/git-hooks/<version>-<hash of the mdkg path>/`.
pub fn hook_dir(home: &Path, exe: &Path) -> PathBuf {
    // FNV-1a: a stable name per binary path, not a security boundary;
    // the folder's contents are verified on every connection.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in exe.as_os_str().as_encoded_bytes() {
        hash = (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
    }
    home.join(".local")
        .join("state")
        .join("mdkg")
        .join("git-hooks")
        .join(format!("{}-{hash:016x}", env!("CARGO_PKG_VERSION")))
}

/// A path as one single-quoted shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The hook scripts this build generates, naming `exe` by absolute path.
/// A hook whose `mdkg` is gone fails `exec`, so sh exits non-zero and git
/// refuses the push.
pub fn hook_scripts(exe: &Path) -> Vec<(&'static str, String)> {
    let exe = sh_quote(&exe.to_string_lossy());
    HOOKS
        .iter()
        .map(|name| {
            (
                *name,
                format!(
                    "#!/bin/sh\n# Generated by mdkg {}. git-serve rewrites this file if it changes.\nexec {exe} git-hook {name} \"$@\"\n",
                    env!("CARGO_PKG_VERSION")
                ),
            )
        })
        .collect()
}

/// Makes the hook folder hold exactly the generated scripts, executable
/// and owner-only, rewriting any that differ by temp file and rename and
/// removing every other entry (Opus r5 m6: git runs any hook it finds).
/// Refuses if the folder can't be made to match (R126), or if another
/// install still holds the lock at `deadline` (#11064: this connection
/// already holds the fence, so it never waits unbounded).
pub fn install_hooks(dir: &Path, exe: &Path, deadline: Instant) -> Result<(), Refusal> {
    let refuse = |what: String| {
        Refusal::new(
            Code::IoWriteFailed,
            format!("can't install git-serve's hooks: {what}"),
        )
    };
    let scripts = hook_scripts(exe);
    // One install at a time, so a connection never removes another's
    // temp file or sees it in its check (Terra #10408). The lock sits
    // beside the folder, never in it.
    let lock_path =
        install_lock_path(dir).ok_or_else(|| refuse(format!("{} has no parent", dir.display())))?;
    if let Some(parent) = lock_path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| refuse(e.to_string()))?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| refuse(e.to_string()))?;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Refusal::new(
                    Code::ApiDraining,
                    "another connection is still installing git-serve's hooks; retry",
                ));
            }
            Err(fs::TryLockError::Error(e)) => return Err(refuse(e.to_string())),
        }
    }
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(refuse(format!("{} isn't a folder", dir.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| refuse(e.to_string()))?;
        }
        Err(e) => return Err(refuse(e.to_string())),
    }
    let entries = fs::read_dir(dir).map_err(|e| refuse(e.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|e| refuse(e.to_string()))?;
        let name = entry.file_name();
        if HOOKS.iter().any(|h| name == OsStr::new(h)) {
            continue;
        }
        let path = entry.path();
        let gone = match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&path),
            _ => fs::remove_file(&path),
        };
        gone.map_err(|e| refuse(format!("{}: {e}", path.display())))?;
    }
    for (name, text) in &scripts {
        let path = dir.join(name);
        if hook_matches(&path, text) {
            continue;
        }
        let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
        let written = (|| -> io::Result<()> {
            let _ = fs::remove_file(&tmp);
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o700))?;
            if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                fs::remove_dir_all(&path)?;
            }
            fs::rename(&tmp, &path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(refuse(format!("{}: {e}", path.display())));
        }
    }
    // Verify what's there now, not what was written.
    let extra = fs::read_dir(dir)
        .map_err(|e| refuse(e.to_string()))?
        .filter_map(Result::ok)
        .any(|e| !HOOKS.iter().any(|h| e.file_name() == OsStr::new(h)));
    if extra || !scripts.iter().all(|(n, t)| hook_matches(&dir.join(n), t)) {
        return Err(refuse(format!(
            "{} doesn't hold exactly this build's hooks",
            dir.display()
        )));
    }
    Ok(())
}

/// The lock one install holds: `<folder>.lock` beside the hook folder.
pub fn install_lock_path(dir: &Path) -> Option<PathBuf> {
    let mut name = dir.file_name()?.to_owned();
    name.push(".lock");
    Some(dir.parent()?.join(name))
}

/// Whether `path` is a regular file (not a link) holding exactly `text`,
/// with mode 0700.
fn hook_matches(path: &Path, text: &str) -> bool {
    let Ok(m) = fs::symlink_metadata(path) else {
        return false;
    };
    m.is_file()
        && m.permissions().mode() & 0o7777 == 0o700
        && fs::read(path).is_ok_and(|b| b == text.as_bytes())
}

/// What one connection asked for, for the log.
struct Asked {
    service: Option<Service>,
    repository: Option<PathBuf>,
}

/// Runs one connection: `Ok(exit)` with git's exit code, or the refusal.
/// `env` is the process environment; only [`KEEP`]'s variables are read
/// or passed on.
pub fn serve(
    machine: &str,
    env: impl IntoIterator<Item = (OsString, OsString)>,
    exe: &Path,
    deps: &Deps,
) -> u8 {
    serve_to(machine, env, exe, deps, Stdio::inherit())
}

/// [`serve`], with git's stdout given: the protocol's channel to the
/// client.
pub fn serve_to(
    machine: &str,
    env: impl IntoIterator<Item = (OsString, OsString)>,
    exe: &Path,
    deps: &Deps,
    stdout: Stdio,
) -> u8 {
    let env = clean_env(env);
    let home = var(&env, "HOME").map(PathBuf::from);
    let mut asked = Asked {
        service: None,
        repository: None,
    };
    let result = connect(machine, &env, exe, deps, &mut asked, stdout);
    let (exit, outcome) = match &result {
        Ok(0) => (0, "ok".to_owned()),
        Ok(code) => (*code, format!("git exited {code}")),
        Err(r) => {
            eprintln!(
                "mdkg git-serve: {} ({})",
                crate::hook::printable(&r.message),
                r.code.as_str()
            );
            (1, r.code.as_str().to_owned())
        }
    };
    if let Some(home) = home.filter(|h| h.is_absolute()) {
        let service = asked.service.map(Service::as_str);
        log(
            &home,
            machine,
            service,
            asked.repository.as_deref(),
            &[],
            &outcome,
        );
    }
    exit
}

fn connect(
    machine: &str,
    env: &[(OsString, OsString)],
    exe: &Path,
    deps: &Deps,
    asked: &mut Asked,
    stdout: Stdio,
) -> Result<u8, Refusal> {
    let home = var(env, "HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .ok_or_else(|| Refusal::new(Code::UsageInvalidArguments, "HOME isn't an absolute path"))?;
    check_machine_id(machine)?;
    let command = var(env, "SSH_ORIGINAL_COMMAND")
        .and_then(OsStr::to_str)
        .ok_or_else(|| {
            Refusal::new(
                Code::ApiActorNotAllowed,
                "git-serve runs only as an ssh forced command, for git",
            )
        })?;
    let (service, path) = parse_command(command)?;
    asked.service = Some(service);
    let config = host_config(&home)?;
    // One limit for the whole connection, the fence's wait and git's run
    // together (#11064).
    let deadline = Instant::now() + config.push_timeout;
    let repo = resolve_repo(&path, &home, &config.repositories)?;
    asked.repository = Some(repo.clone());
    let common = common_dir(&repo)?;

    let book = (deps.book)(&common)?;
    check_approved(book.as_ref(), machine)?;
    let fence = take_fence(
        &fence_path(&common, machine),
        deadline.saturating_duration_since(Instant::now()),
    )?;
    check_approved(book.as_ref(), machine)?;
    let hooks = hook_dir(&home, exe);
    install_hooks(&hooks, exe, deadline)?;

    let mut cmd = Command::new(&config.git);
    match service {
        Service::UploadPack => {
            cmd.arg("upload-pack");
        }
        Service::ReceivePack => {
            cmd.arg("-c")
                .arg(format!("core.hooksPath={}", hooks.display()))
                .args([
                    "-c",
                    "receive.denyNonFastForwards=true",
                    "-c",
                    "receive.denyDeletes=true",
                    "receive-pack",
                ]);
        }
    }
    cmd.arg(&repo)
        .current_dir(&repo)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env(MACHINE_VAR, machine)
        .env(GIT_VAR, &config.git)
        .stdin(Stdio::inherit())
        .stdout(stdout)
        .stderr(Stdio::inherit())
        // Its own group, so the time limit stops its hooks with it.
        .process_group(0);
    inherit_fence(&fence)?;
    let spawned = cmd.spawn();
    // git has its copy; keep any later child of this process from taking one.
    let _ = rustix::io::fcntl_setfd(&fence, rustix::io::FdFlags::CLOEXEC);
    let mut child = spawned.map_err(|e| Refusal::io("can't run git", e))?;
    // From here git holds the fence too, whatever happens to this process.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                drop(fence);
                return Ok(status.code().map_or(1, |c| u8::try_from(c).unwrap_or(1)));
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = kill_group(&mut child);
                let _ = child.wait();
                return Err(Refusal::new(
                    Code::IoGitFailed,
                    "git ran past git_serve.push_timeout_s and was stopped; a ref it was updating may keep a stale refs/heads/<branch>.lock for the repository's owner to remove",
                ));
            }
            Err(e) => return Err(Refusal::io("can't wait for git", e)),
        }
    }
}

/// Stops git and every process in its group, the hooks included, so none
/// keeps the inherited fence and stalls a revoke (Terra #10408). The
/// workspace's rustix has no `process` feature, so this runs `kill` by
/// absolute path; if that can't run, git alone is stopped. Returns
/// whether the whole group was signalled.
fn kill_group(child: &mut std::process::Child) -> bool {
    let group = format!("-{}", child.id());
    let killed = ["/bin/kill", "/usr/bin/kill"].iter().any(|kill| {
        Command::new(kill)
            .args(["-KILL", "--", &group])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    });
    if !killed {
        let _ = child.kill();
    }
    killed
}

/// Appends `{time, machine, service, repository, refs, result}` to
/// `<owner state>/mdkg/serve/git-serve.log`. Best effort: a log that
/// can't be written never changes the outcome.
pub fn log(
    home: &Path,
    machine: &str,
    service: Option<&str>,
    repository: Option<&Path>,
    refs: &[String],
    result: &str,
) {
    let dir = home.join(".local").join("state").join("mdkg").join("serve");
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let line = serde_json::json!({
        "time": crate::output::rfc3339(ms),
        "machine": machine,
        "service": service,
        "repository": repository.map(|r| r.display().to_string()),
        "refs": refs,
        "result": result,
    });
    let _ = fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir);
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join("git-serve.log"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Fakes of the other lanes' parts, from a fixture file, for this module's
/// tests and `git_hook`'s.
#[cfg(test)]
pub(crate) mod fakes {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde_json::{Value, json};

    use super::*;

    /// The fixture: machine rows, placements, the authority and the scan's
    /// marker. `machine_after` replaces a machine's row from its second read
    /// on, to stand for a revoke landing between git-serve's two reads.
    #[derive(Debug, Clone, Default)]
    pub struct Fixture {
        pub machines: BTreeMap<String, MachineRow>,
        pub machine_after: BTreeMap<String, MachineRow>,
        pub placements: BTreeMap<String, Placement>,
        pub config: Value,
        pub definitions: BTreeMap<String, Value>,
        pub protected: Vec<String>,
        pub marker: Option<String>,
        /// Empty: `from_authority`'s default.
        pub graph_folders: Vec<String>,
    }

    fn row(s: &str) -> MachineRow {
        match s {
            "approved" => MachineRow::Approved,
            "pending" => MachineRow::Pending,
            "revoking" => MachineRow::Revoking,
            "revoked" => MachineRow::Revoked,
            _ => MachineRow::Unknown,
        }
    }

    fn row_str(r: MachineRow) -> &'static str {
        match r {
            MachineRow::Approved => "approved",
            MachineRow::Pending => "pending",
            MachineRow::Revoking => "revoking",
            MachineRow::Revoked => "revoked",
            MachineRow::Unknown => "unknown",
        }
    }

    fn state(s: &str) -> PlacementState {
        match s {
            "placed" => PlacementState::Placed,
            "retiring" => PlacementState::Retiring,
            _ => PlacementState::Released,
        }
    }

    fn state_str(s: PlacementState) -> &'static str {
        match s {
            PlacementState::Placed => "placed",
            PlacementState::Retiring => "retiring",
            PlacementState::Released => "released",
        }
    }

    impl Fixture {
        pub fn to_json(&self) -> Value {
            let rows = |m: &BTreeMap<String, MachineRow>| -> Value {
                m.iter()
                    .map(|(k, v)| (k.clone(), json!(row_str(*v))))
                    .collect()
            };
            json!({
                "machines": rows(&self.machines),
                "machine_after": rows(&self.machine_after),
                "placements": self.placements.iter().map(|(k, p)| (k.clone(), json!([p.machine, state_str(p.state)]))).collect::<serde_json::Map<_, _>>(),
                "config": self.config,
                "definitions": self.definitions,
                "protected": self.protected,
                "marker": self.marker,
                "graph_folders": self.graph_folders,
            })
        }

        pub fn from_json(v: &Value) -> Self {
            let rows = |key: &str| -> BTreeMap<String, MachineRow> {
                v[key]
                    .as_object()
                    .map(|o| {
                        o.iter()
                            .map(|(k, s)| (k.clone(), row(s.as_str().unwrap_or(""))))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            Self {
                machines: rows("machines"),
                machine_after: rows("machine_after"),
                placements: v["placements"]
                    .as_object()
                    .map(|o| {
                        o.iter()
                            .map(|(k, p)| {
                                (
                                    k.clone(),
                                    Placement {
                                        machine: p[0].as_str().unwrap_or("").to_owned(),
                                        state: state(p[1].as_str().unwrap_or("")),
                                    },
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                config: v["config"].clone(),
                definitions: v["definitions"]
                    .as_object()
                    .map(|o| o.iter().map(|(k, d)| (k.clone(), d.clone())).collect())
                    .unwrap_or_default(),
                protected: v["protected"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
                marker: v["marker"].as_str().map(str::to_owned),
                graph_folders: v["graph_folders"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        }

        pub fn deps(&self) -> Deps {
            let book = self.clone();
            let authority = self.clone();
            let scanner = self.clone();
            Deps {
                book: Box::new(move |_| {
                    Ok(Box::new(FakeBook {
                        fixture: book.clone(),
                        reads: RefCell::new(0),
                    }) as Box<dyn Book>)
                }),
                authority: Box::new(authority),
                scanner: Box::new(scanner),
            }
        }
    }

    pub struct FakeBook {
        fixture: Fixture,
        reads: RefCell<usize>,
    }

    impl Book for FakeBook {
        fn machine(&self, id: &str) -> Result<MachineRow, Refusal> {
            let mut reads = self.reads.borrow_mut();
            *reads += 1;
            let later = (*reads > 1)
                .then(|| self.fixture.machine_after.get(id))
                .flatten();
            Ok(later
                .or_else(|| self.fixture.machines.get(id))
                .copied()
                .unwrap_or(MachineRow::Unknown))
        }

        fn placement(&self, agent: &str) -> Result<Option<Placement>, Refusal> {
            Ok(self.fixture.placements.get(agent).cloned())
        }
    }

    impl AuthoritySource for Fixture {
        fn agents(&self, _: &Path) -> Result<Agents, Refusal> {
            let mut agents = Agents::from_authority(
                &self.config,
                self.definitions.iter(),
                self.protected.clone(),
            )?;
            if !self.graph_folders.is_empty() {
                agents.graph_folders = self.graph_folders.clone();
            }
            Ok(agents)
        }
    }

    impl Scanner for Fixture {
        /// Finds the marker in any file of any pushed commit.
        fn scan(
            &self,
            git: &Path,
            git_dir: &Path,
            commits: &[String],
        ) -> Result<Option<Finding>, Refusal> {
            let Some(marker) = &self.marker else {
                return Ok(None);
            };
            for commit in commits {
                let out = Command::new(git)
                    .args(["grep", "-l", "-F", "-e", marker, commit])
                    .current_dir(git_dir)
                    .output()
                    .expect("git grep");
                if let Some(line) = String::from_utf8_lossy(&out.stdout).lines().next() {
                    let path = line.split_once(':').map_or(line, |(_, p)| p);
                    return Ok(Some(Finding {
                        commit: commit.clone(),
                        path: path.to_owned(),
                    }));
                }
            }
            Ok(None)
        }
    }

    /// A temporary folder, removed on drop, by its canonical path.
    pub struct Tmp(PathBuf);

    impl Tmp {
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub fn tempdir() -> Tmp {
        let dir = std::env::temp_dir().join(format!("mdkg-git-serve-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("tmp");
        Tmp(std::fs::canonicalize(&dir).expect("canon"))
    }

    /// The git tests run.
    pub fn git() -> PathBuf {
        let out = Command::new("/bin/sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("sh");
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
    }

    /// Runs git with no system or global config, in `dir`.
    pub fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new(git())
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Commits `files` on the current branch of `dir`, returning the SHA.
    pub fn commit(dir: &Path, files: &[(&str, &str)], message: &str) -> String {
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(&p, text).expect("write");
            run_git(dir, &["add", path]);
        }
        run_git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
        run_git(dir, &["rev-parse", "HEAD"])
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::fakes::{Fixture, Tmp, commit, git, run_git, tempdir};
    use super::*;

    pub(crate) const M: &str = "01a188d0-835c-77be-b7fc-5bd4eb5d46d7";

    /// A deadline no test reaches unless an install is held.
    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    /// The production scanner is `secret.scan`'s: it finds a commit that
    /// adds a credential by commit and path, never the text, passes a clean
    /// one, and refuses when it can't run.
    #[test]
    fn the_production_scanner_is_the_secret_scan() {
        let tmp = tempdir();
        let top = tmp.path().join("r");
        std::fs::create_dir_all(&top).expect("repo");
        run_git(&top, &["init", "-q", "-b", "main"]);
        let clean = commit(&top, &[("a.txt", "ordinary\n")], "clean");
        let token = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");
        let keyed = commit(&top, &[("k.txt", &format!("x {token}\n"))], "key");
        let git_dir = top.join(".git");
        let scanner = production().scanner;
        assert_eq!(
            scanner.scan(&git(), &git_dir, std::slice::from_ref(&clean)),
            Ok(None)
        );
        assert_eq!(
            scanner.scan(&git(), &git_dir, &[clean, keyed.clone()]),
            Ok(Some(Finding {
                commit: keyed,
                path: "k.txt".into()
            }))
        );
        let r = scanner
            .scan(&git(), &git_dir, &["0".repeat(40)])
            .expect_err("no such commit");
        assert_eq!(r.code, Code::IoGitFailed);
        assert!(!r.message.contains(token), "{}", r.message);
    }

    #[test]
    fn the_environment_keeps_only_five_variables() {
        let env = os(&[
            ("GIT_DIR", "/evil"),
            ("MDKG_ROOT", "/evil"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("BASH_ENV", "/evil.sh"),
            ("PATH", "/evil/bin"),
            ("HOME", "/h"),
            ("LOGNAME", "o"),
            ("SSH_ORIGINAL_COMMAND", "git-upload-pack '/r'"),
            ("USER", "o"),
            ("SSH_CONNECTION", "10.0.0.2 5000 10.0.0.1 22"),
        ]);
        let kept: Vec<String> = clean_env(env)
            .into_iter()
            .map(|(k, _)| k.into_string().expect("utf-8"))
            .collect();
        assert_eq!(kept, KEEP);
    }

    #[test]
    fn the_command_is_parsed_strictly() {
        assert_eq!(
            parse_command("git-upload-pack '/r/x'").expect("ok"),
            (Service::UploadPack, "/r/x".into())
        );
        assert_eq!(
            parse_command("git-receive-pack 'r.git'").expect("ok"),
            (Service::ReceivePack, "r.git".into())
        );
        for bad in [
            "",
            "git-upload-pack",
            "git-upload-pack /r",
            "git-upload-pack  '/r'",
            "git upload-pack '/r'",
            "git-upload-archive '/r'",
            "sh -c 'id'",
            "git-receive-pack ''",
            "git-receive-pack '/r'\\''; id'",
            "git-receive-pack '/r' '/s'",
            "git-receive-pack '/r\n'",
            "git-receive-pack '/r!'",
            "git-receive-pack \"/r\"",
        ] {
            let r = parse_command(bad).expect_err(bad);
            assert_eq!(r.code, Code::ApiActorNotAllowed, "{bad:?}");
        }
    }

    #[test]
    fn machine_ids_are_lowercase_uuids() {
        check_machine_id(M).expect("ok");
        for bad in [
            "",
            "../x",
            "01A188D0-835C-77BE-B7FC-5BD4EB5D46D7",
            "01a188d0835c77beb7fc5bd4eb5d46d7",
            "m1",
        ] {
            assert_eq!(
                check_machine_id(bad).expect_err(bad).code,
                Code::ApiUnauthenticated
            );
        }
    }

    #[test]
    fn the_host_config_is_read_from_the_user_layer() {
        let c = parse_host_config(&json!({"git_serve": {
            "repositories": ["/a", "/b"], "push_timeout_s": 5, "git": "/opt/git/bin/git"
        }}))
        .expect("ok");
        assert_eq!(
            c.repositories,
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert_eq!(c.push_timeout, Duration::from_secs(5));
        assert_eq!(c.git, PathBuf::from("/opt/git/bin/git"));
        let d = parse_host_config(&json!({})).expect("ok");
        assert!(d.repositories.is_empty());
        assert_eq!(
            (d.push_timeout, d.git),
            (DEFAULT_TIMEOUT, PathBuf::from(DEFAULT_GIT))
        );
        for bad in [
            json!({"git_serve": []}),
            json!({"git_serve": {"repositories": ["rel"]}}),
            json!({"git_serve": {"push_timeout_s": 0}}),
            json!({"git_serve": {"git": "git"}}),
        ] {
            assert_eq!(
                parse_host_config(&bad).expect_err("bad").code,
                Code::ConfigInvalid
            );
        }
        let home = tempdir();
        assert_eq!(
            host_config(home.path()).expect_err("none").code,
            Code::ApiUnknownResource
        );
    }

    #[test]
    fn only_listed_repositories_resolve() {
        let home = tempdir();
        let repo = home.path().join("repo");
        let other = home.path().join("other");
        std::fs::create_dir_all(&repo).expect("mkdir");
        std::fs::create_dir_all(&other).expect("mkdir");
        std::os::unix::fs::symlink(&repo, home.path().join("link")).expect("link");
        let listed = [repo.clone()];
        let want = std::fs::canonicalize(&repo).expect("canon");
        for asked in [repo.to_str().expect("utf-8"), "repo", "~/repo", "link"] {
            assert_eq!(
                resolve_repo(asked, home.path(), &listed).expect(asked),
                want
            );
        }
        for asked in ["other", "repo/../other", "/", "missing"] {
            let r = resolve_repo(asked, home.path(), &listed).expect_err(asked);
            assert_eq!(r.code, Code::ApiUnknownResource);
        }
    }

    #[test]
    fn approval_maps_each_row_to_its_code() {
        for (row, code) in [
            (MachineRow::Revoking, Some(Code::ApiMachineRevoked)),
            (MachineRow::Revoked, Some(Code::ApiMachineRevoked)),
            (MachineRow::Pending, Some(Code::ApiMachinePending)),
            (MachineRow::Unknown, Some(Code::ApiUnauthenticated)),
            (MachineRow::Approved, None),
        ] {
            let f = Fixture {
                machines: BTreeMap::from([(M.to_owned(), row)]),
                ..Fixture::default()
            };
            let book = (f.deps().book)(Path::new("/")).expect("book");
            assert_eq!(
                check_approved(book.as_ref(), M).err().map(|r| r.code),
                code,
                "{row:?}"
            );
        }
    }

    #[test]
    fn the_store_is_read_only_and_must_be_this_builds() {
        let dir = tempdir();
        assert_eq!(
            check_store(dir.path()).expect_err("none").code,
            Code::ApiUnauthenticated
        );
        std::fs::create_dir_all(dir.path().join("mdkg")).expect("mkdir");
        let db = dir.path().join("mdkg").join("coord.sqlite");
        let build = mdkg_coord_sqlite::build_label();
        let (schema, mail) = build.split_once("+msg").expect("label");
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .expect("meta");
        conn.execute(
            "INSERT INTO meta VALUES ('schema', ?1), ('msg_schema', ?2)",
            [schema, mail],
        )
        .expect("rows");
        check_store(dir.path()).expect("this build's store");
        conn.execute("UPDATE meta SET value = '999' WHERE key = 'schema'", [])
            .expect("newer");
        let r = check_store(dir.path()).expect_err("newer");
        assert_eq!(r.code, Code::CoordStoreFailed);
        assert!(r.message.contains("never upgrades"), "{}", r.message);
        let v: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .expect("read");
        assert_eq!(v, "999", "git-serve left the store as it was");
    }

    #[test]
    fn the_hook_folder_holds_exactly_this_builds_hooks() {
        let home = tempdir();
        let exe = Path::new("/opt/it's/mdkg");
        let dir = hook_dir(home.path(), exe);
        assert!(dir.starts_with(home.path().join(".local/state/mdkg/git-hooks")));
        assert_ne!(dir, hook_dir(home.path(), Path::new("/other/mdkg")));
        install_hooks(&dir, exe, soon()).expect("fresh");
        let scripts = hook_scripts(exe);
        assert!(
            scripts[0]
                .1
                .contains("exec '/opt/it'\\''s/mdkg' git-hook pre-receive \"$@\"")
        );
        let read = |n: &str| std::fs::read_to_string(dir.join(n)).expect("hook");
        assert_eq!(read("update"), scripts[1].1);

        // A changed hook, an extra hook, a link and a wrong mode are all
        // put right.
        std::fs::write(dir.join("update"), "#!/bin/sh\nexit 0\n").expect("tamper");
        std::fs::write(dir.join("post-receive"), "#!/bin/sh\n").expect("extra");
        std::fs::create_dir(dir.join("reference-transaction")).expect("extra dir");
        std::fs::remove_file(dir.join("pre-receive")).expect("rm");
        std::os::unix::fs::symlink("/bin/true", dir.join("pre-receive")).expect("link");
        install_hooks(&dir, exe, soon()).expect("repaired");
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("ls")
            .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(names, ["pre-receive", "update"]);
        assert_eq!(read("update"), scripts[1].1);
        assert!(
            !std::fs::symlink_metadata(dir.join("pre-receive"))
                .expect("meta")
                .is_symlink()
        );
        std::fs::set_permissions(dir.join("update"), std::fs::Permissions::from_mode(0o777))
            .expect("chmod");
        install_hooks(&dir, exe, soon()).expect("mode");
        assert_eq!(
            std::fs::metadata(dir.join("update"))
                .expect("meta")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        // The folder deleted: reinstalled.
        std::fs::remove_dir_all(&dir).expect("rm");
        install_hooks(&dir, exe, soon()).expect("reinstalled");
        assert_eq!(read("pre-receive"), scripts[0].1);
    }

    /// Terra #10408: connections installing at once never refuse each
    /// other. Each one rewrites the scripts, since the two builds' texts
    /// differ; the lock is on the open file description, so threads
    /// contend as processes would.
    #[test]
    fn concurrent_installs_never_refuse_each_other() {
        let home = tempdir();
        let dir = hook_dir(home.path(), Path::new("/opt/mdkg"));
        let results: Vec<Result<(), Refusal>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let dir = &dir;
                    s.spawn(move || {
                        let exe = if i % 2 == 0 {
                            "/opt/mdkg"
                        } else {
                            "/opt/other/mdkg"
                        };
                        (0..10).try_for_each(|_| install_hooks(dir, Path::new(exe), soon()))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("join"))
                .collect()
        });
        for r in results {
            r.expect("no install refused another");
        }
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("ls")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[test]
    fn an_unwritable_hook_folder_that_differs_is_refused() {
        let home = tempdir();
        let exe = Path::new("/opt/mdkg");
        let dir = hook_dir(home.path(), exe);
        install_hooks(&dir, exe, soon()).expect("fresh");
        std::fs::write(dir.join("update"), "#!/bin/sh\nexit 0\n").expect("tamper");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        let r = install_hooks(&dir, exe, soon());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod back");
        assert_eq!(r.expect_err("unwritable").code, Code::IoWriteFailed);
        // A matching unwritable folder is fine: it's verified, not written.
        install_hooks(&dir, exe, soon()).expect("repaired");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        let r = install_hooks(&dir, exe, soon());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod back");
        r.expect("verified");
    }

    #[test]
    fn a_hook_whose_mdkg_is_gone_exits_non_zero() {
        let home = tempdir();
        let exe = home.path().join("gone/mdkg");
        let dir = hook_dir(home.path(), &exe);
        install_hooks(&dir, &exe, soon()).expect("install");
        let status = Command::new(dir.join("update"))
            .args(["refs/heads/x", "0", "1"])
            .stderr(Stdio::null())
            .status()
            .expect("run hook");
        assert!(!status.success());
    }

    #[test]
    fn a_held_fence_makes_git_serve_wait_then_refuse() {
        let dir = tempdir();
        let path = fence_path(dir.path(), M);
        let first = take_fence(&path, Duration::from_millis(10)).expect("shared");
        let second = take_fence(&path, Duration::from_millis(10)).expect("shared too");
        assert!(fence_held(&path).expect("held"));
        drop((first, second));
        // Another test thread's fork holds a copy of every open file until
        // its exec, so "free" may take a moment to show.
        let deadline = Instant::now() + Duration::from_secs(5);
        while fence_held(&path).expect("fence") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!fence_held(&path).expect("free"));
        let revoke = File::open(&path).expect("open");
        revoke.lock().expect("exclusive");
        let r = take_fence(&path, Duration::from_millis(200)).expect_err("held exclusively");
        assert_eq!(r.code, Code::ApiDraining);
    }

    /// Terra 2 #6271: git-serve dies after the hooks pass, git lives on.
    /// The fence stays held until git exits, so a revoke can't commit
    /// before git moves the refs.
    #[test]
    fn the_fence_outlives_git_serve_until_git_exits() {
        let dir = tempdir();
        let path = fence_path(dir.path(), M);
        let fence = take_fence(&path, Duration::from_millis(10)).expect("shared");
        inherit_fence(&fence).expect("inherit");
        let mut child = Command::new("/bin/sleep").arg("30").spawn().expect("child");
        // As connect does: no other test thread's child takes a copy.
        rustix::io::fcntl_setfd(&fence, rustix::io::FdFlags::CLOEXEC).expect("cloexec");
        drop(fence);
        let revoke = File::open(&path).expect("open");
        assert!(
            matches!(revoke.try_lock(), Err(fs::TryLockError::WouldBlock)),
            "the child still holds the fence"
        );
        assert!(fence_held(&path).expect("held"));
        child.kill().expect("kill");
        child.wait().expect("wait");
        // Another thread's fork holds a copy of every open file until its
        // exec, so "free" may take a moment to show.
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(revoke.try_lock(), Err(fs::TryLockError::WouldBlock))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        revoke.try_lock().expect("free once git is gone");
    }

    /// A stand-in git that records its arguments and environment.
    fn stand_in_git(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("git");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    struct Host {
        home: Tmp,
        repo: PathBuf,
    }

    fn host(git_path: &Path, timeout_s: u64) -> Host {
        let home = tempdir();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("repo");
        let config = config_path(home.path());
        std::fs::create_dir_all(config.parent().expect("dir")).expect("mkdir");
        std::fs::write(
            &config,
            json!({"git_serve": {"repositories": [repo], "push_timeout_s": timeout_s, "git": git_path}}).to_string(),
        )
        .expect("config");
        Host { home, repo }
    }

    fn env_for(h: &Host, command: &str) -> Vec<(OsString, OsString)> {
        let home = h.home.path().to_str().expect("utf-8").to_owned();
        os(&[
            ("HOME", &home),
            ("USER", "o"),
            ("SSH_ORIGINAL_COMMAND", command),
            // What a shell startup file might set: none reaches git.
            ("GIT_DIR", "/evil"),
            ("MDKG_ROOT", "/evil"),
            ("GIT_SSH_COMMAND", "evil"),
            ("PATH", "/evil"),
        ])
    }

    fn approved() -> Fixture {
        Fixture {
            machines: BTreeMap::from([(M.to_owned(), MachineRow::Approved)]),
            ..Fixture::default()
        }
    }

    #[test]
    fn git_runs_by_absolute_path_with_a_clean_environment() {
        let bin = tempdir();
        let record = bin.path().join("record");
        let git_path = stand_in_git(
            bin.path(),
            &format!("{{ echo \"$@\"; env; }} > '{}'", record.display()),
        );
        let h = host(&git_path, 30);
        let exe = Path::new("/opt/mdkg");
        let command = format!("git-receive-pack '{}'", h.repo.display());
        assert_eq!(serve(M, env_for(&h, &command), exe, &approved().deps()), 0);
        let seen = std::fs::read_to_string(&record).expect("git ran");
        let (args, env) = seen.split_once('\n').expect("lines");
        let hooks = hook_dir(h.home.path(), exe);
        assert_eq!(
            args,
            format!(
                "-c core.hooksPath={} -c receive.denyNonFastForwards=true -c receive.denyDeletes=true receive-pack {}",
                hooks.display(),
                std::fs::canonicalize(&h.repo).expect("canon").display()
            )
        );
        for line in env.lines() {
            let name = line.split_once('=').map_or(line, |(n, _)| n);
            assert!(
                KEEP.contains(&name)
                    || [MACHINE_VAR, GIT_VAR, "PWD", "SHLVL", "_", "OLDPWD"].contains(&name),
                "{name} reached git"
            );
        }
        assert!(env.contains(&format!("{MACHINE_VAR}={M}")));
        assert!(hooks.join("pre-receive").is_file(), "hooks installed");
        let log =
            std::fs::read_to_string(h.home.path().join(".local/state/mdkg/serve/git-serve.log"))
                .expect("log");
        let line: Value = serde_json::from_str(log.lines().last().expect("line")).expect("json");
        assert_eq!(
            (line["machine"].as_str(), line["result"].as_str()),
            (Some(M), Some("ok"))
        );

        let fetch = format!("git-upload-pack '{}'", h.repo.display());
        assert_eq!(serve(M, env_for(&h, &fetch), exe, &approved().deps()), 0);
        let seen = std::fs::read_to_string(&record).expect("git ran");
        assert!(seen.starts_with("upload-pack /"), "{seen}");
    }

    #[test]
    fn a_machine_not_approved_never_reaches_git() {
        let bin = tempdir();
        let record = bin.path().join("record");
        let git_path = stand_in_git(bin.path(), &format!("touch '{}'", record.display()));
        let h = host(&git_path, 30);
        let command = format!("git-receive-pack '{}'", h.repo.display());
        let exe = Path::new("/opt/mdkg");
        for row in [
            MachineRow::Revoking,
            MachineRow::Revoked,
            MachineRow::Pending,
            MachineRow::Unknown,
        ] {
            let f = Fixture {
                machines: BTreeMap::from([(M.to_owned(), row)]),
                ..Fixture::default()
            };
            assert_eq!(
                serve(M, env_for(&h, &command), exe, &f.deps()),
                1,
                "{row:?}"
            );
        }
        // A revoke that commits between the first read and the fence.
        let racing = Fixture {
            machine_after: BTreeMap::from([(M.to_owned(), MachineRow::Revoking)]),
            ..approved()
        };
        assert_eq!(serve(M, env_for(&h, &command), exe, &racing.deps()), 1);
        // A path not listed, and a command that isn't git's.
        assert_eq!(
            serve(
                M,
                env_for(&h, "git-receive-pack '/etc'"),
                exe,
                &approved().deps()
            ),
            1
        );
        assert_eq!(serve(M, env_for(&h, "sh"), exe, &approved().deps()), 1);
        assert_eq!(
            serve("../x", env_for(&h, &command), exe, &approved().deps()),
            1
        );
        assert!(!record.exists(), "git never ran");
    }

    #[test]
    fn git_past_its_time_limit_is_stopped() {
        let bin = tempdir();
        let git_path = stand_in_git(bin.path(), "exec /bin/sleep 30");
        let h = host(&git_path, 1);
        let command = format!("git-upload-pack '{}'", h.repo.display());
        let start = Instant::now();
        assert_eq!(
            serve(
                M,
                env_for(&h, &command),
                Path::new("/opt/mdkg"),
                &approved().deps()
            ),
            1
        );
        assert!(start.elapsed() < Duration::from_secs(10));
        let _ = git();
    }

    /// The production authority reads g-core's graph ref tip (S39): the
    /// agents defined there, the graph and main refs as protected, and
    /// the graph's folders, a workspace's included. A clone with no graph
    /// ref, and a bare repository, are refused.
    #[test]
    fn the_agents_come_from_the_graph_refs_tip() {
        const ID: &str = "01a18a09-0804-7e91-b5a9-7c454dd43ccb";
        // A nested graph at `docs`, with its own folder name (Terra #10862).
        const NESTED: &str = "01a18a09-0804-7e91-b5a9-7c454dd43ccd";
        let tmp = tempdir();
        let top = tmp.path().join("host");
        std::fs::create_dir_all(&top).expect("mkdir");
        run_git(&top, &["init", "-q", "-b", "trunk"]);
        commit(&top, &[("src/a.rs", "1\n")], "code");
        run_git(&top, &["checkout", "-q", "--orphan", "seed"]);
        run_git(&top, &["rm", "-rfq", "--cached", "."]);
        std::fs::remove_dir_all(top.join("src")).expect("clear");
        let config = json!({
            "schema_version": 3, "format_version": "0.7", "graph_id": ID,
            "orchestration": {"branch": "{agent}"},
            "workspaces": {
                "root": {"path": ".", "mdkg_dir": ".mdkg"},
                "web": {"path": "apps/web", "mdkg_dir": "graph"},
            },
        });
        commit(
            &top,
            &[
                (".mdkg/config.json", &config.to_string()),
                (".mdkg/agents/astra.json", r#"{"role":"worker"}"#),
                (".mdkg/agents/main.json", r#"{"role":"worker"}"#),
                (
                    ".mdkg/agents/old.json",
                    r#"{"role":"worker","retired":true}"#,
                ),
                (
                    "docs/kb/config.json",
                    &json!({
                        "schema_version": 3, "format_version": "0.7", "graph_id": NESTED,
                        "workspaces": {
                            "root": {"path": ".", "mdkg_dir": ".mdkg"},
                            "api": {"path": "api", "mdkg_dir": "graph"},
                        },
                    })
                    .to_string(),
                ),
            ],
            "graph",
        );
        run_git(&top, &["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
        run_git(&top, &["checkout", "-qf", "trunk"]);
        run_git(&top, &["branch", "-D", "-q", "seed"]);
        run_git(&top, &["clean", "-fdxq"]);
        let git_dir = std::fs::canonicalize(top.join(".git")).expect("canon");

        // No graph ref yet: checkout mode is never an authority here.
        let r = GraphAuthority.agents(&git_dir).expect_err("checkout mode");
        assert_eq!(r.code, Code::GraphRefMissing, "{}", r.message);

        run_git(&top, &["config", "mdkg.graph.ref", "mdkg-graph"]);
        run_git(
            &top,
            &["config", &format!("mdkg.graph.{ID}.ref"), "mdkg-graph"],
        );
        // mdkg.mainRef unset is `main`: under the `{agent}` pattern the
        // agent named `main` still can't move it (#11064).
        let unset = GraphAuthority.agents(&git_dir).expect("authority");
        assert_eq!(
            unset.protected,
            ["refs/heads/mdkg-graph", "refs/heads/main"]
        );
        assert_eq!(unset.main_ref, "refs/heads/main");
        let r = super::super::git_hook::owner(&unset, "refs/heads/main").expect_err("main");
        assert!(r.message.contains("never pushed"), "{}", r.message);
        run_git(&top, &["config", "mdkg.mainRef", "trunk"]);
        for (key, value) in [("ref", "mdkg-graph"), ("root", "docs"), ("dir", "kb")] {
            run_git(
                &top,
                &["config", &format!("mdkg.graph.{NESTED}.{key}"), value],
            );
        }
        // A checkout's own files change nothing.
        std::fs::create_dir_all(top.join(".mdkg/agents")).expect("mkdir");
        std::fs::write(top.join(".mdkg/agents/rogue.json"), "{}").expect("rogue");
        let agents = GraphAuthority.agents(&git_dir).expect("authority");
        assert_eq!(
            agents.defined,
            BTreeMap::from([
                ("astra".to_owned(), false),
                ("main".to_owned(), false),
                ("old".to_owned(), true)
            ])
        );
        assert_eq!(
            agents.protected,
            ["refs/heads/mdkg-graph", "refs/heads/trunk"]
        );
        // A main ref that resolves to a remote-tracking ref still protects
        // the local branch it stands for (Terra #11168).
        let trunk = run_git(&top, &["rev-parse", "trunk"]);
        run_git(&top, &["update-ref", "refs/remotes/origin/main", &trunk]);
        run_git(&top, &["config", "mdkg.mainRef", "origin/main"]);
        let remote = GraphAuthority.agents(&git_dir).expect("authority");
        assert_eq!(remote.main_ref, "refs/remotes/origin/main");
        assert_eq!(
            remote.protected,
            [
                "refs/heads/mdkg-graph",
                "refs/remotes/origin/main",
                "refs/heads/origin/main",
                "refs/heads/main"
            ]
        );
        let r = super::super::git_hook::owner(&remote, "refs/heads/main").expect_err("main");
        assert!(r.message.contains("never pushed"), "{}", r.message);
        run_git(&top, &["config", "mdkg.mainRef", "trunk"]);
        assert_eq!(
            agents.graph_folders,
            [".mdkg", "apps/web/graph", "docs/api/graph", "docs/kb"]
        );

        run_git(tmp.path(), &["clone", "-q", "--bare", "host", "bare.git"]);
        let bare = std::fs::canonicalize(tmp.path().join("bare.git")).expect("canon");
        let r = GraphAuthority.agents(&bare).expect_err("bare");
        assert_eq!(r.code, Code::ApiUnsupported);
    }

    /// #11064: `kill -KILL -- -<pgid>`, as kill_group runs it, stops a
    /// whole process group on this platform (macOS's BSD kill and Linux's
    /// both take `--`; RC CI covers Linux).
    #[test]
    fn kill_group_stops_the_whole_group() {
        use std::io::BufRead;
        let mut child = Command::new("/bin/sh")
            .args(["-c", "/bin/sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("group");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("pid");
        let grandchild = line.trim().to_owned();
        assert!(kill_group(&mut child), "kill took the group");
        child.wait().expect("reaped");
        let alive = || {
            Command::new("/bin/kill")
                .args(["-0", &grandchild])
                .stderr(Stdio::null())
                .status()
                .expect("kill -0")
                .success()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "the group's other process is gone");
    }

    /// #11064: an install that never lets go of the lock can't hold this
    /// connection, which already has the fence, past its deadline.
    #[test]
    fn a_held_install_lock_is_waited_on_only_until_the_deadline() {
        let home = tempdir();
        let exe = Path::new("/opt/mdkg");
        let dir = hook_dir(home.path(), exe);
        install_hooks(&dir, exe, soon()).expect("fresh");
        let held = File::open(install_lock_path(&dir).expect("lock")).expect("open");
        held.lock().expect("another install");
        let start = Instant::now();
        let r = install_hooks(&dir, exe, Instant::now() + Duration::from_millis(200))
            .expect_err("held");
        assert_eq!(r.code, Code::ApiDraining, "{}", r.message);
        assert!(start.elapsed() < Duration::from_secs(5));
        drop(held);
        install_hooks(&dir, exe, soon()).expect("free again");
    }

    /// #11064: the fence's wait and git's run share one limit, so a revoke
    /// that holds the fence for most of it leaves git only the rest.
    #[test]
    fn the_fence_wait_and_git_share_one_time_limit() {
        let bin = tempdir();
        let git_path = stand_in_git(bin.path(), "exec /bin/sleep 30");
        let h = host(&git_path, 4);
        let fence = fence_path(&h.repo.join(".git"), M);
        std::fs::create_dir_all(fence.parent().expect("dir")).expect("mkdir");
        let revoke = File::create(&fence).expect("fence");
        revoke.lock().expect("exclusive");
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            drop(revoke);
        });
        let command = format!("git-upload-pack '{}'", h.repo.display());
        let start = Instant::now();
        assert_eq!(
            serve(
                M,
                env_for(&h, &command),
                Path::new("/opt/mdkg"),
                &approved().deps()
            ),
            1
        );
        let took = start.elapsed();
        release.join().expect("release");
        // One limit: about 4 s. Two, as before, would be about 7.
        assert!(
            took >= Duration::from_secs(3) && took < Duration::from_millis(5500),
            "{took:?}"
        );
    }

    /// The production book reads serve-store's rows read-only: each
    /// machine state, a pending enrolment, an unknown id, and each
    /// placement state; a row that changes between reads is seen (S38
    /// steps 5 and 6).
    #[test]
    fn the_book_reads_serve_stores_rows() {
        use mdkg_coord_sqlite::serve;
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join("mdkg")).expect("mkdir");
        let db = dir.path().join("mdkg").join("coord.sqlite");
        let build = mdkg_coord_sqlite::build_label();
        let (schema, mail) = build.split_once("+msg").expect("label");
        let mut conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .expect("meta");
        conn.execute(
            "INSERT INTO meta VALUES ('schema', ?1), ('msg_schema', ?2)",
            [schema, mail],
        )
        .expect("rows");
        let tx = conn.transaction().expect("tx");
        serve::create(&tx).expect("serve tables");
        tx.commit().expect("commit");
        let machine = |id: &str, name: &str, state: &str| {
            conn.execute(
                "INSERT INTO machines (machine_id, name, public_key, fingerprint, state, groups, launcher)
                 VALUES (?1, ?2, 'k', 'f', ?3, '[]', ?2)",
                [id, name, state],
            )
            .expect("machine");
        };
        const R: &str = "01a18a09-0804-7e91-b5a9-7c454dd43cc1";
        const P: &str = "01a18a09-0804-7e91-b5a9-7c454dd43cc2";
        machine(M, "m", "approved");
        machine(R, "r", "revoked");
        conn.execute(
            "INSERT INTO pending (pending_id, machine_id, public_key, fingerprint, name, commitment, ns, requested_ms, expires_ms)
             VALUES ('p1', ?1, 'k', 'f', 'p', 'c', 'n', 0, 9999999999999)",
            [P],
        )
        .expect("pending");
        for (agent, state) in [("a", "placed"), ("b", "retiring"), ("c", "retired")] {
            conn.execute(
                "INSERT INTO placements (agent, machine_id, source, state, since_ms)
                 VALUES (?1, ?2, 'claimed', ?3, 0)",
                [agent, M, state],
            )
            .expect("placement");
        }

        let book = (production().book)(dir.path()).expect("book");
        assert_eq!(book.machine(M).expect("m"), MachineRow::Approved);
        assert_eq!(book.machine(R).expect("r"), MachineRow::Revoked);
        assert_eq!(book.machine(P).expect("p"), MachineRow::Pending);
        assert_eq!(
            book.machine("01a18a09-0804-7e91-b5a9-7c454dd43cc3")
                .expect("u"),
            MachineRow::Unknown
        );
        let state = |agent: &str| book.placement(agent).expect("placement").map(|p| p.state);
        assert_eq!(state("a"), Some(PlacementState::Placed));
        assert_eq!(state("b"), Some(PlacementState::Retiring));
        assert_eq!(state("c"), Some(PlacementState::Released));
        assert_eq!(state("d"), None);
        assert_eq!(book.placement("a").expect("a").expect("row").machine, M);

        // Revocation's first phase commits between two reads: the second sees it.
        conn.execute(
            "UPDATE machines SET state = 'revoking' WHERE machine_id = ?1",
            [M],
        )
        .expect("revoke");
        assert_eq!(book.machine(M).expect("m"), MachineRow::Revoking);
        assert_eq!(
            check_approved(book.as_ref(), M).expect_err("revoking").code,
            Code::ApiMachineRevoked
        );
    }

    /// Terra #10408: the time limit stops git's whole group, so a hook
    /// still running never keeps the inherited fence and stalls a revoke.
    #[test]
    fn the_time_limit_stops_the_hooks_with_git() {
        // A loaded test run can keep the stand-in from starting its hook
        // before the limit, which proves nothing: try again then.
        let mut seen = Vec::new();
        let (h, hook) = loop {
            let bin = tempdir();
            let started = bin.path().join("started");
            let pid = bin.path().join("pid");
            let git_path = stand_in_git(
                bin.path(),
                &format!(
                    ": > '{}'\n/bin/sleep 30 &\necho $! > '{}'\nwait",
                    started.display(),
                    pid.display()
                ),
            );
            let h = host(&git_path, 2);
            let command = format!("git-receive-pack '{}'", h.repo.display());
            assert_eq!(
                serve(
                    M,
                    env_for(&h, &command),
                    Path::new("/opt/mdkg"),
                    &approved().deps()
                ),
                1
            );
            if let Ok(hook) = std::fs::read_to_string(&pid) {
                break (h, hook);
            }
            seen.push(started.exists());
            assert!(
                seen.len() < 3,
                "the hook never started (git started: {seen:?})"
            );
        };
        // The hook's process is gone (killed, then reaped once orphaned).
        let alive = || {
            Command::new("/bin/kill")
                .args(["-0", hook.trim()])
                .stderr(Stdio::null())
                .status()
                .expect("kill -0")
                .success()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "the hook was stopped with git");
        // So nothing of this push holds the fence. (Another test's child
        // spawned in the instant this one's fence was inheritable could;
        // git-serve itself spawns nothing else.)
        let fence = fence_path(&h.repo.join(".git"), M);
        while fence_held(&fence).expect("fence") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!fence_held(&fence).expect("fence"), "the fence is free");
    }
}
