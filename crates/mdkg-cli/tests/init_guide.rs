//! Execute the published setup recipe, not a second copy of its commands.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        if let Some(pid) = rustix::process::Pid::from_raw(self.0.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.0.wait();
    }
}

#[test]
fn documented_agent_setup_commits_the_definition_and_runs_from_the_graph_checkout() {
    let guide = include_str!("../src/init_readme.md");
    let commands = guide
        .split_once("## Start agents\n")
        .expect("start section")
        .1
        .split_once("```sh\n")
        .expect("setup example")
        .1
        .split_once("\n```")
        .expect("end of example")
        .0;
    // Bound the documented watcher to its documented one-pass alternative.
    // Every other command runs verbatim, including Git and install ordering.
    assert!(
        commands
            .lines()
            .last()
            .unwrap()
            .ends_with("agents up worker")
    );
    let commands = format!("{commands} --once");

    for work in ["local", "shared"] {
        let dir = std::env::temp_dir().join(format!("mdkg-init-guide-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&dir).expect("scratch");
        let scratch = Scratch(fs::canonicalize(dir).expect("canonical scratch"));
        let repo = scratch.0.join("repo");
        let home = scratch.0.join("home");
        let bin = scratch.0.join("bin");
        for path in [&repo, &home, &bin] {
            fs::create_dir(path).expect("fixture directory");
        }
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_mdkg"), bin.join("mdkg"))
            .expect("candidate CLI on fixture PATH");
        // Preserve the runner's Git selection, including Homebrew on macOS.
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").expect("runner PATH"),
        )))
        .expect("fixture PATH");
        let stdout = scratch.0.join("stdout");
        let stderr = scratch.0.join("stderr");
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../mdkg-run/tests/fixtures/fake-claude.sh");
        // Supply eligible mail so the bounded watcher must launch its worker.
        let script = format!(
            "set -eu\ngit init -q -b main\nmdkg init --work {work}\n\
             mdkg --as guide-check msg send --to worker --kind note --text 'Guide check'\n\
             {commands}\n\
             git -C .worktrees/graph show HEAD:.mdkg/agents/worker.json > committed-definition\n\
             git -C .worktrees/graph diff-tree --no-commit-id --name-only -r HEAD > definition-paths\n"
        );
        let mut child = Child(
            Command::new("/bin/sh")
                .args(["-c", &script])
                .current_dir(&repo)
                .env_clear()
                .env("PATH", path)
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .env("MDKG_RUN_CLAUDE", fixture)
                .env("MDKG_RUN_SECURITY", "/usr/bin/false")
                .stdin(Stdio::null())
                .stdout(fs::File::create(&stdout).expect("stdout"))
                .stderr(fs::File::create(&stderr).expect("stderr"))
                .process_group(0)
                .spawn()
                .expect("run recipe"),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.0.try_wait().expect("poll recipe") {
                break status;
            }
            assert!(Instant::now() < deadline, "recipe exceeded 60 seconds");
            std::thread::sleep(Duration::from_millis(20));
        };
        let output = format!(
            "{}\n{}",
            fs::read_to_string(stdout).expect("stdout"),
            fs::read_to_string(stderr).expect("stderr")
        );
        assert!(status.success(), "{work}: {output}");
        assert_eq!(
            fs::read(repo.join("committed-definition")).expect("committed definition"),
            fs::read(repo.join(".worktrees/graph/.mdkg/agents/worker.json"))
                .expect("authored definition"),
            "{work}: agents add must commit the exact definition"
        );
        assert_eq!(
            fs::read_to_string(repo.join("definition-paths")).expect("commit paths"),
            ".mdkg/agents/worker.json\n",
            "{work}: the definition commit must contain only the definition"
        );
        assert!(
            repo.join(".worktrees/worker/claude.args").is_file(),
            "{work}: recipe must reach the fake harness: {output}"
        );
    }
}
