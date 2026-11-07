//! `agents status --no-probe` reads mdkg's own records and git only: no
//! harness binary, Keychain listing or billing read runs, so a client can
//! poll it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};

mod common;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

/// The folder holding the real `git`, so stubs can come first on `PATH`.
fn git_dir() -> PathBuf {
    let which = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("sh");
    PathBuf::from(String::from_utf8(which.stdout).expect("utf-8").trim())
        .parent()
        .expect("git's folder")
        .to_path_buf()
}

struct Fixture {
    dir: Scratch,
    repo: PathBuf,
    calls: PathBuf,
    path: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-no-probe")).expect("dir"));
        let repo = dir.0.join("repo");
        let bin = dir.0.join("bin");
        let calls = dir.0.join("calls.log");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&bin).expect("bin");
        fs::create_dir_all(dir.0.join("home")).expect("home");
        // Each harness and `security`: record the call, then fail.
        for name in ["claude", "codex", "opencode", "security"] {
            let stub = bin.join(name);
            fs::write(
                &stub,
                format!(
                    "#!/bin/sh\necho \"{name} $*\" >> '{}'\nexit 1\n",
                    calls.display()
                ),
            )
            .expect("stub");
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let path = format!("{}:{}:/usr/bin:/bin", bin.display(), git_dir().display());
        let fixture = Self {
            dir,
            repo,
            calls,
            path,
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fs::write(fixture.repo.join("code.rs"), "fn main() {}\n").expect("code");
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-q", "-m", "code"]);
        let (code, out) = fixture.mdkg(&["init"]);
        assert_eq!(code, 0, "{out}");
        let agents = fixture.repo.join(".mdkg/agents");
        fs::create_dir_all(&agents).expect("agents");
        for (name, harness) in [
            ("w-claude", "claude"),
            ("w-codex", "codex"),
            ("w-opencode", "opencode"),
        ] {
            fs::write(
                agents.join(format!("{name}.json")),
                json!({"schema": "mdkg.agent.v1", "harness": harness, "role": "worker"})
                    .to_string(),
            )
            .expect("definition");
        }
        // One inline in the config, which isn't file-backed.
        let config_path = fixture.repo.join(".mdkg/config.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&config_path).expect("config")).expect("JSON");
        config["agents"] = json!({"inline": {"harness": "claude", "role": "worker"}});
        fs::write(
            &config_path,
            serde_json::to_vec_pretty(&config).expect("JSON"),
        )
        .expect("config");
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-q", "-m", "graph"]);
        fixture
    }

    fn isolated<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        let home = self.dir.0.join("home");
        let cmd = common::without_launcher_env(cmd);
        // The caller's git routing (GIT_DIR, GIT_INDEX_FILE in a hook)
        // never reaches the fixture's git or mdkg.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(name);
            }
        }
        for key in ["MDKG_RUN_CLAUDE", "MDKG_RUN_CODEX", "MDKG_RUN_OPENCODE"] {
            cmd.env_remove(key);
        }
        cmd.env("PATH", &self.path)
            .env("MDKG_RUN_SECURITY", self.dir.0.join("bin/security"))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
    }

    fn git(&self, args: &[&str]) {
        let out = self
            .isolated(Command::new("git").arg("-C").arg(&self.repo).args(args))
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn mdkg(&self, args: &[&str]) -> (i32, Value) {
        let out = self
            .isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .arg("--root")
            .arg(&self.repo)
            .args(["--as", "launcher"])
            .args(args)
            .arg("--json")
            .output()
            .expect("mdkg");
        let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            panic!(
                "not JSON: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), envelope)
    }

    fn calls(&self) -> String {
        fs::read_to_string(&self.calls).unwrap_or_default()
    }
}

fn agent<'a>(status: &'a Value, name: &str) -> &'a Value {
    status["data"]["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .find(|a| a["name"] == name)
        .unwrap_or_else(|| panic!("{name} in {status}"))
}

#[test]
fn no_probe_runs_no_harness_and_leaves_probe_fields_out() {
    let fixture = Fixture::new();
    assert_eq!(fixture.calls(), "", "setup ran a stub");
    // A usage-limit note, which only the billing read reports.
    let billing = fixture.repo.join(".git/mdkg/run/billing");
    fs::create_dir_all(&billing).expect("billing");
    // Private, as mdkg makes it: a group-writable folder is refused
    // before the note is read, whatever the caller's umask (review of
    // 4f7611f3).
    fs::set_permissions(&billing, fs::Permissions::from_mode(0o700)).expect("chmod");
    fs::write(billing.join("w-claude.limited"), "").expect("limit note");
    let (code, status) = fixture.mdkg(&["agents", "status", "--no-probe"]);
    assert_eq!(code, 0, "{status}");
    common::conforms("agents.status", &status);
    assert_eq!(fixture.calls(), "", "--no-probe ran a harness or security");
    for name in ["w-claude", "w-codex", "w-opencode"] {
        let a = agent(&status, name);
        assert_eq!(a["file_backed"], true, "{a}");
        assert!(a["effective"].is_object(), "{a}");
        assert!(a.get("auth").is_none(), "{a}");
        assert!(a.get("billing").is_none(), "{a}");
        // Liveness and progress stay.
        assert!(a["state"].is_string(), "{a}");
        assert!(a.get("supervisor").is_some(), "{a}");
        assert!(a["follow_from"].is_string(), "{a}");
    }
    let inline = agent(&status, "inline");
    assert_eq!(inline["file_backed"], false, "{inline}");
    assert!(inline.get("effective").is_none(), "{inline}");
    assert!(status["data"]["progress"].is_array(), "{status}");

    // Without the flag the same status probes, so the stubs see it: the
    // check above would catch a probe. (OpenCode's login check reads its
    // own files and runs no program.)
    let (code, status) = fixture.mdkg(&["agents", "status"]);
    assert_eq!(code, 0, "{status}");
    let calls = fixture.calls();
    for harness in ["claude", "codex", "security"] {
        assert!(
            calls.lines().any(|l| l.starts_with(&format!("{harness} "))),
            "{harness} wasn't probed: {calls}"
        );
    }
    for name in ["w-claude", "w-codex", "w-opencode"] {
        assert!(agent(&status, name)["auth"].is_object(), "{status}");
    }
    // The seeded note shows only when probing, so the check above that
    // `--no-probe` left `billing` out tests the gate.
    let note = agent(&status, "w-claude")["billing"]
        .as_str()
        .unwrap_or_default();
    assert!(note.contains("subscription limit reached"), "{status}");
}
