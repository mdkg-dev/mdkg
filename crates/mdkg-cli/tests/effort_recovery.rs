//! Recovery procedures run against disposable repositories, without a model.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mdkg_run::{
    Harness, Role,
    state::{self, SpecFlags},
};
use serde_json::{Value, json};

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    graph: PathBuf,
}
impl Fixture {
    fn command(&self, program: impl AsRef<std::ffi::OsStr>, cwd: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(cwd)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("runner PATH"))
            .env("HOME", self.dir.join("home"))
            .env("XDG_CONFIG_HOME", self.dir.join("home/.config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("MDKG_RUN_OPENCODE", "/usr/bin/false")
            .env("MDKG_RUN_SECURITY", "/usr/bin/false");
        command
    }
    fn git(&self, args: &[&str]) {
        let out = self
            .command("git", &self.repo)
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "{out:?}");
    }
    fn cli(&self, root: &Path, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_mdkg"), root)
            .args(["--as", "fixture"])
            .args(args)
            .arg("--json")
            .output()
            .expect("fixture CLI runs")
    }
    fn preview(&self) -> Output {
        self.cli(&self.graph, &["agents", "up", "old", "--once", "--dry-run"])
    }
    fn new(inline: bool) -> Self {
        let dir =
            std::env::temp_dir().join(format!("mdkg-effort-recovery-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(dir.join("repo")).expect("create repository");
        fs::create_dir(dir.join("home")).expect("create fixture home");
        let dir = fs::canonicalize(dir).expect("canonical fixture");
        let f = Self {
            repo: dir.join("repo"),
            graph: dir.join("repo/.worktrees/graph"),
            dir,
        };
        f.git(&["init", "-q", "-b", "main"]);
        let out = f.cli(&f.repo, &["init", "--work", "local"]);
        assert!(out.status.success(), "{out:?}");
        f.git(&["add", ".mdkg"]);
        f.git(&["commit", "-qm", "fixture"]);
        // Create and register the managed graph checkout through public setup.
        let out = f.cli(&f.repo, &["install"]);
        assert!(out.status.success(), "{out:?}");
        f.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "agent/old",
            ".worktrees/old",
            "main",
        ]);
        let mut saved = state::resolve(
            "old",
            None,
            SpecFlags {
                harness: Some(Harness::OpenCode),
                role: Some(Role::Worker),
                ..Default::default()
            },
            &f.repo.join(".worktrees/old"),
        )
        .expect("resolve initial state");
        saved.spec.effort = Some("low".into());
        let run = f.repo.join(".git/mdkg/run");
        fs::create_dir_all(&run).expect("create state directory");
        state::save(&run, &saved).expect("save legacy state");
        if inline {
            let config = f.graph.join(".mdkg/config.json");
            let mut value: Value = serde_json::from_slice(&fs::read(&config).expect("read config"))
                .expect("parse config");
            value["agents"] = json!({"old":{"harness":"opencode","role":"worker","effort":"low"}});
            fs::write(config, value.to_string()).expect("write inline config");
            // Commit the old inline settings as fixture input so the graph
            // repair gate does not mask the effort diagnostic being tested.
            let graph = f.graph.to_str().expect("UTF-8 fixture path");
            f.git(&["-C", graph, "add", ".mdkg/config.json"]);
            f.git(&["-C", graph, "commit", "-qm", "old inline settings"]);
        }
        f
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).expect("remove fixture");
    }
}

#[test]
fn recovery_hint_handles_saved_and_inline_state_without_a_manual_definition_commit() {
    for inline in [false, true] {
        let f = Fixture::new(inline);
        let refused = f.preview();
        assert!(!refused.status.success());
        let result: Value = serde_json::from_slice(&refused.stdout).unwrap();
        let hint = result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|d| d["hint"].as_str())
            .expect("recovery hint");
        assert!(hint.contains("saved or inline"), "{result}");
        assert!(
            hint.contains(
                "remove its effort field there, then commit that file in the graph checkout"
            ),
            "file-backed recovery must include the commit step: {result}"
        );
        let command = hint
            .split('`')
            .nth(1)
            .unwrap()
            .replace("<name>", "old")
            .replace("<role>", "worker");
        let args = command.split_whitespace().skip(1).collect::<Vec<_>>();
        let out = f.cli(&f.graph, &args);
        assert!(out.status.success(), "{out:?}");
        let path = f.graph.join(".mdkg/agents/old.json");
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(value.get("effort").is_none());
        let resumed = f.preview();
        assert!(resumed.status.success(), "{resumed:?}");
        let events: Vec<Value> = String::from_utf8(resumed.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            events
                .iter()
                .any(|v| v["data"]["turn"]["harness"] == "opencode"),
            "{events:?}"
        );

        // Invalid existing file definitions cannot be repaired by --unset:
        // the old file is parsed before management applies the removal.
        let mut invalid = value.clone();
        invalid["effort"] = json!("low");
        fs::write(&path, invalid.to_string()).unwrap();
        // Keep deliberate fixture edits committed so the dirty-graph guard
        // cannot mask the effort diagnostic this test exercises.
        let graph = f.graph.to_str().expect("UTF-8 fixture path");
        f.git(&["-C", graph, "add", ".mdkg/agents/old.json"]);
        f.git(&["-C", graph, "commit", "-qm", "invalid effort fixture"]);
        let refused = f.cli(&f.graph, &["agents", "edit", "old", "--unset", "effort"]);
        assert!(!refused.status.success());
        let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
        assert!(refused["diagnostics"].as_array().unwrap().iter().any(|d| {
            d["message"]
                .as_str()
                .is_some_and(|s| s.contains("OpenCode takes no effort"))
        }));
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid.to_string());
        // Follow the file-backed branch. The edit alone is still frozen;
        // the hint's commit step makes the repaired definition runnable.
        fs::write(path, value.to_string()).unwrap();
        let uncommitted = f.preview();
        assert!(!uncommitted.status.success());
        let uncommitted: Value = serde_json::from_slice(&uncommitted.stdout).unwrap();
        assert!(
            uncommitted["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| {
                    d["message"]
                        .as_str()
                        .is_some_and(|s| s.contains("graph worktree needs repair"))
                }),
            "{uncommitted}"
        );
        f.git(&["-C", graph, "add", ".mdkg/agents/old.json"]);
        f.git(&["-C", graph, "commit", "-qm", "repair effort fixture"]);
        let resumed = f.preview();
        assert!(resumed.status.success(), "{resumed:?}");
    }
}
