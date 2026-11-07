//! `doctor` never shows a credential it reads from a projected MCP config:
//! a server command that looks like one is `<redacted>` in text and JSON.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) -> std::process::Output {
    let home = dir.join("home");
    common::without_launcher_env(&mut Command::new(program))
        .current_dir(dir.join("repo"))
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
        .args(args)
        .output()
        .expect("runs")
}

#[test]
fn a_secret_like_mcp_command_is_redacted_in_text_and_json() {
    let dir = Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-doctor-redact")).expect("dir"));
    fs::create_dir_all(dir.0.join("repo")).expect("repo");
    fs::create_dir_all(dir.0.join("home")).expect("home");
    let mdkg = env!("CARGO_BIN_EXE_mdkg");
    assert!(
        run(&dir.0, "git", &["init", "-q", "-b", "main"])
            .status
            .success()
    );
    let init = run(&dir.0, mdkg, &["--as", "launcher", "init", "--json"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stdout)
    );
    let install = run(
        &dir.0,
        mdkg,
        &[
            "--as",
            "launcher",
            "install",
            "--enable",
            "mcp:claude,mcp:codex,mcp:opencode",
            "--json",
        ],
    );
    assert!(
        install.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&install.stdout),
        String::from_utf8_lossy(&install.stderr)
    );
    // A different credential shape in each file, assembled so none is
    // whole in the source: a URL with a password, a known token after an
    // argument space, and an opaque one with no known prefix, which only
    // the rule that a command with arguments is never shown hides (review
    // of 40fbd146).
    let secrets = [
        concat!(
            "https://deploy:",
            "Zq8vR2mXk4pL9wT",
            "@example.invalid/mdkg"
        ),
        concat!(
            "mdkg --token ",
            "ghp_",
            "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8"
        ),
        concat!("mdkg --token ", "Q7rT2vX9mK4p", "L8wZ3nB6yH1c"),
    ];
    let repo = dir.0.join("repo");
    let edit_json = |rel: &str, pointer: &str, value: &str| {
        let path = repo.join(rel);
        let mut v: Value = serde_json::from_slice(&fs::read(&path).expect(rel)).expect("JSON");
        assert!(v.pointer(pointer).is_some(), "{rel} {pointer}: {v}");
        *v.pointer_mut(pointer).expect("pointer") = value.into();
        fs::write(&path, serde_json::to_vec_pretty(&v).expect("JSON")).expect("write");
    };
    edit_json(".mcp.json", "/mcpServers/mdkg/command", secrets[0]);
    edit_json("opencode.json", "/mcp/mdkg/command/0", secrets[1]);
    let toml = repo.join(".codex/config.toml");
    let mut doc: toml_edit::DocumentMut = fs::read_to_string(&toml)
        .expect("codex config")
        .parse()
        .expect("TOML");
    doc["mcp_servers"]["mdkg"]["command"] = toml_edit::value(secrets[2]);
    fs::write(&toml, doc.to_string()).expect("write");

    let text = run(&dir.0, mdkg, &["doctor"]);
    let json = run(&dir.0, mdkg, &["doctor", "--json"]);
    for out in [&text, &json] {
        let shown = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        for secret in secrets {
            assert!(!shown.contains(secret), "{secret} shown:\n{shown}");
            // Nor its credential part alone.
            let part = secret.rsplit([' ', ':']).next().expect("part");
            assert!(!shown.contains(part), "{part} shown:\n{shown}");
        }
    }
    let envelope: Value = serde_json::from_slice(&json.stdout).expect("doctor JSON");
    let mcp = envelope["data"]["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "mcp")
        .expect("mcp check")
        .clone();
    // The check still judges the real commands: none is on PATH.
    assert_eq!(mcp["status"], "warning", "{mcp}");
    let summary = mcp["summary"].as_str().expect("summary");
    for file in [".mcp.json", "opencode.json", ".codex/config.toml"] {
        assert!(
            summary.contains(&format!("{file} runs `<redacted>`")),
            "{summary}"
        );
    }
}
