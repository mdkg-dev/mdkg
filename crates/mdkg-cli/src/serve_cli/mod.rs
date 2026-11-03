//! serve's commands: `serve`, `machine`, `person`, `remote`, `git-serve`
//! and `git-hook` (the serve edd). main.rs flattens [`ServeCommand`] into its
//! command list and hands every one to [`early`], before any graph is
//! found, then to [`run`] if `early` passes. So each command is added here
//! and in its own module, and main.rs never changes for one.
//!
//! A command not built yet refuses with `api.unsupported`, naming the
//! command and nothing that was typed. It's hidden from `--help`, and so
//! from docs/commands.md, until built: the lane that builds one drops its
//! `hide` and regenerates the doc. `git-serve` and `git-hook` are built
//! (lane L4); `git-hook` stays hidden, since only its generated hooks run it.
//! `serve` is built (lane serve-run).

mod git_hook;
mod git_serve;
mod serve;

pub(crate) use serve::{ServeData, ServeStatusData};

use std::ffi::OsString;
use std::path::Path;

use clap::{Args, Subcommand};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::ErrorKind;

use crate::output;

#[derive(Debug, Subcommand)]
pub enum ServeCommand {
    /// Serve this graph's coordinator over its socket and, when asked,
    /// TLS; `serve status` tells whether it runs.
    Serve(serve::ServeArgs),
    /// This machine's identity and the graph's enrolled machines (not
    /// built yet).
    #[command(hide = true)]
    Machine(Stub),
    /// People's keys for acting over the network: enroll, approve, list,
    /// revoke (not built yet).
    #[command(hide = true)]
    Person(Stub),
    /// Bind this clone to a coordinator on another machine (not built
    /// yet).
    #[command(hide = true)]
    Remote(Stub),
    /// The receiving side of git between machines: the ssh forced command
    /// `authorized_keys` names for a machine's key, run by sshd as the
    /// repository owner.
    #[command(name = "git-serve")]
    GitServe(GitServeArgs),
    /// The receiving hooks `git-serve` installs for its pushes; git runs
    /// them, never a person, so it stays out of --help and the command
    /// docs (orchestrator, #11064).
    #[command(name = "git-hook", hide = true)]
    GitHook(GitHookArgs),
}

#[derive(Debug, Args)]
pub struct GitServeArgs {
    /// The machine whose key this is, as approved for the graph.
    #[arg(value_name = "MACHINE_ID")]
    machine: String,
}

#[derive(Debug, Args)]
pub struct GitHookArgs {
    #[command(subcommand)]
    hook: HookCommand,
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// git's pre-receive hook: every ref of the push, on stdin.
    PreReceive,
    /// git's update hook: one ref.
    Update {
        #[arg(value_name = "REF")]
        refname: String,
        old: String,
        new: String,
    },
}

/// A command not built yet: it takes any arguments, so it refuses the
/// same way however it's called.
#[derive(Debug, Args)]
pub struct Stub {
    /// Print JSON.
    #[arg(long)]
    json: bool,
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        hide = true,
        value_name = "ARGS"
    )]
    rest: Vec<OsString>,
}

impl Stub {
    fn json(&self) -> bool {
        self.json || self.rest.iter().any(|a| a == "--json")
    }
}

impl ServeCommand {
    fn stub(&self) -> Option<&Stub> {
        match self {
            ServeCommand::Machine(s) | ServeCommand::Person(s) | ServeCommand::Remote(s) => Some(s),
            ServeCommand::Serve(_) | ServeCommand::GitServe(_) | ServeCommand::GitHook(_) => None,
        }
    }

    /// The command's name: its envelope's schema once it's built.
    pub fn name(&self) -> &'static str {
        match self {
            // `serve status` answers as `mdkg.serve.status.v1`, early
            // failures included (#11581 item 1).
            ServeCommand::Serve(serve::ServeArgs {
                action: Some(serve::ServeAction::Status { .. }),
                ..
            }) => "serve.status",
            ServeCommand::Serve(_) => "serve",
            ServeCommand::Machine(_) => "machine",
            ServeCommand::Person(_) => "person",
            ServeCommand::Remote(_) => "remote",
            ServeCommand::GitServe(_) => "git-serve",
            ServeCommand::GitHook(_) => "git-hook",
        }
    }

    /// `git-serve` and `git-hook` write git's protocol and stderr only.
    pub fn json(&self) -> bool {
        match self {
            ServeCommand::Serve(a) => a.json(),
            _ => self.stub().is_some_and(Stub::json),
        }
    }
}

/// Runs a command that needs no graph, before main.rs looks for one:
/// `Some(exit)` when it ran, `None` to go on to [`run`] with the graph.
pub fn early(_root: Option<&Path>, c: &ServeCommand) -> Option<u8> {
    match c {
        ServeCommand::GitServe(a) => Some(match this_mdkg() {
            Ok(exe) => git_serve::serve(
                &a.machine,
                std::env::vars_os(),
                &exe,
                &git_serve::production(),
            ),
            Err(e) => {
                eprintln!("mdkg git-serve: can't find its own program: {e}");
                1
            }
        }),
        ServeCommand::GitHook(a) => {
            let (hook, args) = match &a.hook {
                HookCommand::PreReceive => (git_hook::Hook::PreReceive, vec![]),
                HookCommand::Update { refname, old, new } => (
                    git_hook::Hook::Update,
                    vec![refname.clone(), old.clone(), new.clone()],
                ),
            };
            let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
            // git runs hooks in the repository's git folder.
            let git_dir = std::env::current_dir().unwrap_or_default();
            Some(git_hook::run(
                hook,
                &args,
                std::io::stdin().lock(),
                &env,
                &git_dir,
                &git_serve::production(),
            ))
        }
        // serve needs the graph and its store.
        ServeCommand::Serve(_) => None,
        _ => Some(not_built(c)),
    }
}

/// This program's absolute path, links resolved: the one the hooks name.
fn this_mdkg() -> std::io::Result<std::path::PathBuf> {
    std::fs::canonicalize(std::env::current_exe()?)
}

/// Runs a command with the graph at `root` and the checked actor. `gate` is
/// the committed authority main read in ref mode, where `root` is the
/// checkout and holds no graph config.
pub fn run(
    root: &Path,
    _actor: Option<String>,
    gate: Option<&mdkg_graph::graphref::Authority>,
    c: &ServeCommand,
) -> u8 {
    match c {
        ServeCommand::Serve(a) => {
            if let Some(gate) = gate
                && a.action.is_none()
            {
                match nested_root(root, &gate.graph_id) {
                    Ok(None) => (),
                    Ok(Some(at)) => {
                        return refused(
                            a.json(),
                            format!("serve can't serve the nested graph at {at} in ref mode yet"),
                        );
                    }
                    // Unknown is refused: started under this graph's id,
                    // served calls could reach the root graph (#15491).
                    Err(why) => {
                        return refused(
                            a.json(),
                            format!(
                                "serve can't tell whether {} is the checkout top: {why}",
                                root.display()
                            ),
                        );
                    }
                }
            }
            serve::run(root, gate, a)
        }
        _ => not_built(c),
    }
}

/// The ref-mode graph `graph_id`'s root below the checkout top, when it
/// isn't the top: its recorded root, as the authority reader normalizes
/// it, or `root` itself when that isn't the top. An error when neither the
/// records nor git can say.
fn nested_root(root: &Path, graph_id: &str) -> Result<Option<String>, String> {
    use mdkg_graph::graphref;
    let recorded = graphref::resolve(root)
        .map_err(|e| e.message)?
        .and_then(|g| g.graph_roots.get(graph_id).cloned())
        .map(|r| graphref::normalize_root(&r))
        .filter(|r| !r.is_empty());
    if recorded.is_some() {
        return Ok(recorded);
    }
    let at = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let top = crate::coord::graph_git::git::toplevel(&at)
        .and_then(std::fs::canonicalize)
        .map_err(|e| e.to_string())?;
    Ok((at != top).then(|| at.display().to_string()))
}

/// Served calls run the CLI, which binds a ref-mode clone's root graph:
/// a nested graph's requests would reach that one instead.
fn refused(json: bool, message: String) -> u8 {
    let d = Diagnostic::error(Code::ApiUnsupported, message)
        .with_hint("serve the repository's root graph: run serve without --root");
    output::emit(
        "serve",
        json,
        output::Report::<()> {
            data: None,
            diagnostics: vec![d],
            failure: Some(ErrorKind::Usage),
        },
        |_| {},
    )
}

fn not_built(c: &ServeCommand) -> u8 {
    let name = c.name();
    let d = Diagnostic::error(
        Code::ApiUnsupported,
        format!("`{name}` isn't built in this version of mdkg"),
    )
    .with_hint("serve and its commands arrive in a later 0.7.1 build");
    let report: output::Report<()> = output::Report {
        data: None,
        diagnostics: vec![d],
        failure: Some(ErrorKind::Usage),
    };
    // `mdkg.cli.v1`, as for a parse failure: an unbuilt command has no
    // schema of its own yet, and `mdkg schema cli` has this one.
    output::emit("cli", c.json(), report, |_| {})
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    /// Review #15621: when git can't name the checkout top (here, a
    /// repository marked bare, where `--show-toplevel` fails), the nested
    /// check is an error, never "not nested". The authority reader runs
    /// the same lookup first, so this fails there; the later lookup can
    /// only fail on a transient fault after it.
    #[test]
    fn an_unknown_checkout_top_is_an_error() {
        let dir = std::env::temp_dir().join(format!("mdkg-serve-top-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).expect("dir");
        // The fixture's git reads no inherited repository or config
        // variable (GIT_DIR, GIT_CONFIG and the rest): every GIT_* goes.
        let git = |args: &[&str]| {
            let mut cmd = std::process::Command::new("git");
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("GIT_") {
                    cmd.env_remove(key);
                }
            }
            cmd.args(args)
                .current_dir(&dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .expect("git")
        };
        let init = git(&["init", "-q"]);
        assert!(init.success());
        let bare = git(&["config", "core.bare", "true"]);
        assert!(bare.success());
        let found = super::nested_root(&dir, "01a19fa0-0000-7000-8000-000000000003");
        std::fs::remove_dir_all(&dir).ok();
        assert!(found.is_err(), "{found:?}");
    }

    use crate::{Cli, Command};

    fn parse(args: &[&str]) -> super::ServeCommand {
        let cli = Cli::try_parse_from(std::iter::once("mdkg").chain(args.iter().copied()))
            .expect("parses");
        match cli.command {
            Command::Serve(c) => c,
            other => panic!("not a serve command: {other:?}"),
        }
    }

    #[test]
    fn every_stub_parses_with_any_arguments() {
        for (args, name, json) in [
            (&["serve"][..], "serve", false),
            (&["serve", "--json"], "serve", true),
            (&["serve", "status", "--json"], "serve.status", true),
            (
                &["serve", "--root", ".", "--listen", "127.0.0.1:7000"],
                "serve",
                false,
            ),
            (&["machine", "show", "--json"], "machine", true),
            (
                &["machine", "approve", "m1", "--code", "12345678"],
                "machine",
                false,
            ),
            (
                &["remote", "attach", "https://h", "--fingerprint", "mdkg:x"],
                "remote",
                false,
            ),
            (
                &["person", "approve", "human:a", "--pending", "p1"],
                "person",
                false,
            ),
            (&["person", "list", "--json"], "person", true),
            (&["git-serve", "0190"], "git-serve", false),
            (&["git-hook", "pre-receive"], "git-hook", false),
            (
                &["git-hook", "update", "refs/heads/a", "0", "1"],
                "git-hook",
                false,
            ),
        ] {
            let c = parse(args);
            assert_eq!(c.name(), name, "{args:?}");
            assert_eq!(c.json(), json, "{args:?}");
        }
    }

    #[test]
    fn every_stub_refuses_before_the_graph() {
        // serve is built: it needs the graph and its store.
        assert_eq!(super::early(None, &parse(&["serve"])), None);
        assert_eq!(super::early(None, &parse(&["serve", "status"])), None);
        for args in [&["machine"][..], &["person"], &["remote"]] {
            let c = parse(args);
            assert_eq!(super::early(None, &c), Some(1), "{args:?}");
        }
    }
}
