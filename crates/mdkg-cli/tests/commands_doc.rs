//! `docs/commands.md` is generated from the CLI's own `--help`, so it never
//! drifts: this test fails when it's stale. Regenerate it with
//! `scripts/commands-doc.sh` (or `MDKG_WRITE_COMMANDS_DOC=1 cargo test -p
//! mdkg-cli --test commands_doc`).

mod common;

use std::path::Path;
use std::process::Command;

/// The binary's name in the docs: the public one, whatever the build is
/// called today.
const NAME: &str = "mdkg";

fn help(path: &[&str]) -> String {
    let out = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .args(path)
        .arg("--help")
        .env("NO_COLOR", "1")
        .env("COLUMNS", "100")
        .output()
        .expect("run --help");
    assert!(out.status.success(), "{path:?} --help failed");
    let text = String::from_utf8(out.stdout).expect("utf-8 help");
    // The public name, `.exe` or not; no trailing spaces, which editors
    // strip and would leave the file looking stale.
    text.replace("mdkg.exe", NAME)
        .replace("mdkg", NAME)
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The subcommands a help text lists, `help` aside.
fn subcommands(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = text.lines().skip_while(|l| *l != "Commands:").skip(1);
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            break;
        }
        if let Some(name) = line.split_whitespace().next()
            && line.starts_with("  ")
            && name != "help"
        {
            out.push(name.to_string());
        }
    }
    out
}

fn walk(path: &mut Vec<String>, out: &mut String) {
    let args: Vec<&str> = path.iter().map(String::as_str).collect();
    let text = help(&args);
    let title = if path.is_empty() {
        NAME.to_string()
    } else {
        format!("{NAME} {}", path.join(" "))
    };
    let level = "#".repeat((path.len() + 2).min(4));
    out.push_str(&format!(
        "\n{level} `{title}`\n\n```text\n{}\n```\n",
        text.trim_end()
    ));
    for sub in subcommands(&text) {
        path.push(sub);
        walk(path, out);
        path.pop();
    }
}

fn generate() -> String {
    let mut out = String::from(
        "# Command reference\n\n\
         Every command's `--help`, generated from the CLI itself by\n\
         `scripts/commands-doc.sh`; a test fails when this file is stale.\n\
         [Concepts](concepts.md) explains the ideas behind them. Add `--json`\n\
         to a command for one machine-readable envelope, and see its shape\n\
         with `mdkg schema <command>`.\n",
    );
    walk(&mut Vec::new(), &mut out);
    out.push_str(&exit_codes());
    out
}

/// The exit-code contract, from the CLI's own error kinds.
fn exit_codes() -> String {
    let mut out = String::from(
        "\n## Exit codes\n\n\
         Every command exits with one of these. With `--json`, the envelope's\n\
         `diagnostics` carry the stable code that says why.\n\n\
         | Exit | Means |\n|---|---|\n\
         | 0 | Success, warnings included |\n",
    );
    for kind in mdkg_graph::ErrorKind::ALL {
        out.push_str(&format!("| {} | {} |\n", kind.exit_code(), kind.meaning()));
    }
    out
}

#[test]
fn the_command_reference_matches_the_cli() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/commands.md");
    let want = generate();
    if std::env::var("MDKG_WRITE_COMMANDS_DOC").as_deref() == Ok("1") {
        std::fs::write(&path, &want).expect("write docs/commands.md");
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        have == want,
        "docs/commands.md is stale: run scripts/commands-doc.sh"
    );
}
