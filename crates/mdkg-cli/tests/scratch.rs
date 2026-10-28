//! Scratch path discovery with handwritten Git metadata. These tests make
//! no Git write commands and never open a live coordinator store.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

mod common;

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("mdkg-scratch-{}", uuid::Uuid::now_v7()));
        let root = dir.join("main");
        for folder in [".git/objects", ".git/refs"] {
            fs::create_dir_all(root.join(folder)).expect("git metadata");
        }
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        fs::write(
            root.join(".git/config"),
            "[core]\nrepositoryformatversion = 0\nbare = false\n",
        )
        .expect("git config");
        let f = Self(fs::canonicalize(dir).expect("fixture path"));
        let output = command(&f.main())
            .args([
                "init",
                "--graph-only",
                "--work",
                "local",
                "--no-gitignore",
                "--json",
            ])
            .output()
            .expect("init fixture");
        assert!(output.status.success(), "{output:?}");
        f
    }

    fn main(&self) -> PathBuf {
        self.0.join("main")
    }

    fn linked(&self) -> PathBuf {
        let top = self.0.join("linked");
        let admin = self.main().join(".git/worktrees/linked");
        fs::create_dir_all(&admin).expect("worktree admin");
        fs::create_dir_all(top.join(".mdkg")).expect("linked graph");
        fs::write(top.join(".git"), format!("gitdir: {}\n", admin.display())).expect("gitdir");
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", top.join(".git").display()),
        )
        .expect("backlink");
        fs::write(admin.join("commondir"), "../..\n").expect("common dir");
        fs::write(admin.join("HEAD"), "ref: refs/heads/linked\n").expect("HEAD");
        fs::copy(
            self.main().join(".mdkg/config.json"),
            top.join(".mdkg/config.json"),
        )
        .expect("same graph config");
        top
    }
}

fn command(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    common::without_launcher_env(&mut c)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE");
    c
}

fn envelope(out: Output) -> Value {
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{out:?}"));
    common::conforms("scratch.path", &value);
    assert!(out.status.success(), "{value}");
    assert_eq!(value["schema"], "mdkg.scratch.path.v1");
    value
}

fn path(root: &Path, actor: &str, shared: bool) -> Value {
    let mut c = command(root);
    c.args(["--as", actor, "scratch", "path", "--json"]);
    if shared {
        c.arg("--shared");
    }
    envelope(c.output().expect("scratch query"))["data"].clone()
}

fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, found: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(root).expect("list fixture") {
            let entry = entry.expect("entry");
            if entry.file_type().expect("entry type").is_dir() {
                walk(&entry.path(), found);
            } else {
                found.push((entry.path(), fs::read(entry.path()).expect("fixture bytes")));
            }
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

#[test]
fn scratch_paths_share_the_git_common_dir_without_creating_anything() {
    let fixture = Fixture::new();
    let main = fixture.main();
    let linked = fixture.linked();
    let before = snapshot(&fixture.0);
    let a = path(&main, "agent-a", false);
    assert_eq!(a, path(&linked, "agent-a", false));
    assert_ne!(a["path"], path(&linked, "agent-b", false)["path"]);
    assert_eq!(
        a["path"],
        main.join(".git/mdkg/scratch/agent-a").to_str().unwrap()
    );
    assert_eq!(a["actor"], "agent-a");
    assert_eq!(a["shared"], false);
    let shared = path(&main, "agent-a", true);
    assert_eq!(shared, path(&linked, "agent-b", true));
    assert_eq!(
        shared["path"],
        main.join(".git/mdkg/scratch/shared").to_str().unwrap()
    );
    assert_eq!(shared["shared"], true);
    assert!(shared.get("actor").is_none());
    assert!(
        !main.join(".git/mdkg").exists(),
        "no scratch folder or store"
    );
    assert_eq!(before, snapshot(&fixture.0), "all fixture files unchanged");
}

#[test]
fn scratch_uses_the_normal_actor_precedence_and_text_is_one_path() {
    let fixture = Fixture::new();
    let root = fixture.main();
    let base = root.join(".git/mdkg/scratch");
    let default = envelope(
        command(&root)
            .args(["scratch", "path", "--json"])
            .output()
            .expect("default actor"),
    );
    assert_eq!(default["data"]["actor"], "main");
    let env = envelope(
        command(&root)
            .env("MDKG_ACTOR", "env-actor")
            .args(["scratch", "path", "--json"])
            .output()
            .expect("env actor"),
    );
    assert_eq!(
        env["data"]["path"],
        base.join("env-actor").to_str().unwrap()
    );
    let flag = envelope(
        command(&root)
            .env("MDKG_ACTOR", "env-actor")
            .args(["--as=flag-actor", "scratch", "path", "--json"])
            .output()
            .expect("flag actor"),
    );
    assert_eq!(flag["data"]["actor"], "flag-actor");
    let text = command(&root)
        .args(["--as=flag-actor", "scratch", "path"])
        .output()
        .expect("text output");
    assert!(text.status.success());
    assert_eq!(
        String::from_utf8(text.stdout).unwrap(),
        format!("{}\n", base.join("flag-actor").display())
    );
    assert!(text.stderr.is_empty());
}

#[test]
fn scratch_rejects_traversal_and_unreadable_git_metadata() {
    let fixture = Fixture::new();
    let root = fixture.main();
    for actor in [".", "..", "../escape", "/tmp/elsewhere", "PRIVATE_ACTOR\n"] {
        let out = command(&root)
            .args(["--as", actor, "scratch", "path", "--json"])
            .output()
            .expect("invalid actor");
        assert!(!out.status.success());
        let value: Value = serde_json::from_slice(&out.stdout).expect("error envelope");
        common::conforms("scratch.path", &value);
        assert_eq!(value["ok"], false);
        assert!(!value.to_string().contains("PRIVATE_ACTOR"));
    }
    let linked = fixture.linked();
    fs::write(linked.join(".git"), "not a gitdir file\n").expect("damage metadata");
    let out = command(&linked)
        .args(["scratch", "path", "--json"])
        .output()
        .expect("damaged checkout");
    assert!(!out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).expect("error envelope");
    common::conforms("scratch.path", &value);
    assert_eq!(value["ok"], false);
    assert!(value.get("data").is_none());
}

#[test]
fn scratch_mcp_returns_the_same_envelope_and_is_read_only() {
    let fixture = Fixture::new();
    let root = fixture.main();
    let expected = path(&root, "mcp-actor", true);
    let before = snapshot(&fixture.0);
    let mut child = command(&root)
        .args(["--as=mcp-actor", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("MCP server");
    let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}});
    let mut input = child.stdin.take().expect("MCP stdin");
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":meta}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "_meta":meta,"name":"scratch_path","arguments":{"shared":true}}}),
    ] {
        writeln!(input, "{request}").expect("MCP request");
    }
    drop(input);
    let out = child.wait_with_output().expect("MCP response");
    assert!(out.status.success(), "{out:?}");
    let replies: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON RPC"))
        .collect();
    let list = replies
        .iter()
        .find(|r| r["id"] == 1)
        .expect("list response");
    let tool = list["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|t| t["name"] == "scratch_path")
        .expect("scratch tool");
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    let result = &replies
        .iter()
        .find(|r| r["id"] == 2)
        .expect("call response")["result"];
    assert_eq!(result["isError"], false, "{result}");
    let envelope = &result["structuredContent"];
    common::conforms("scratch.path", envelope);
    assert_eq!(envelope["data"], expected);
    assert_eq!(before, snapshot(&fixture.0));
    assert!(!root.join(".git/mdkg").exists());
}
