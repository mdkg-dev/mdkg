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
    out.push_str(
        "\n## Anchor output\n\n\
         mdkg anchor leads with the anchor path to commit, then counts distinct\n\
         actors by kind (launcher, named and system). For example:\n\n\
         ~~~text\n\
         anchored store <store> at seq 42 (<hash>) in .mdkg/anchors/<store>.json; commit it\n\
         checkpoint 42: records 1 to 41; actors: launcher 35, named 2 (full list: --json)\n\
         ~~~\n\n\
         mdkg anchor --json includes the complete sorted list in\n\
         data.checkpoint.actors, including actors omitted from the bounded prose\n\
         summary. Verified by anchor_text_counts_actors_and_json_keeps_the_complete_list\n\
         in crates/mdkg-cli/tests/work.rs.\n",
    );
    out.push_str(
        "\n## Doctor build warnings\n\n\
         Doctor prints one builds row with the stale process count, current and\n\
         observed builds, and PIDs grouped by build and process kind. For example:\n\n\
         ~~~text\n\
         warn builds   mixed mdkg builds: 2 stale launcher/MCP process(es); current 0.7.1 at <new> (clean, built <date>); 0.7.0 at <old> (clean, built <date>): launcher pids [123, 456]\n\
           hint: restart stale launcher and MCP processes after upgrading mdkg\n\
         ~~~\n\n\
         JSON retains the builds check and its coded diagnostic. The same grouped\n\
         warnings appear in agents status and frozen-checkout diagnostics, for\n\
         Claude, Codex and OpenCode supervisors alike. Verified by\n\
         mixed_live_builds_warn_in_doctor_and_status_text_and_json in\n\
         crates/mdkg-cli/tests/process_builds.rs and\n\
         warnings_group_pids_by_full_build_and_preserve_independent_problems in\n\
         crates/mdkg-run/src/builds.rs.\n",
    );
    out.push_str(
        "\n## Graph identity repair\n\n\
         Plain doctor previews the exact clone-local Git identity records that\n\
         are missing for an existing, validated graph checkout. For example:\n\n\
         ~~~text\n\
         warn graph_identity identity repair would write: git config --local --add mdkg.graph.<graph-id>.worktree graph\n\
         ~~~\n\n\
         Run mdkg doctor --repair to add those records. Repeating it is a no-op.\n\
         It keeps conflicting records and refuses dirty graph checkouts, never\n\
         advances the checkout or syncs Claude, Codex or OpenCode projections.\n\
         This works from the primary, graph or ordinary sibling checkout.\n\
         A subgraph source_path under a Git submodule belongs to that separate\n\
         repository: parent install and repair skip its identity. Repair or install\n\
         the submodule's own graph checkout from that repository instead. A workspace\n\
         under a gitlink is still refused because workspaces belong to the parent.\n\
         Correct it to a subgraph declaration on the configured main ref, then rerun\n\
         install: its guarded fast-forward precedes identity validation. A preview\n\
         checks the pinned target tree when that update can proceed. If install\n\
         advances the checkout before refusing identity publication, its error names\n\
         the updated commit. Doctor never advances the checkout. Repair uses the\n\
         shared clone-local graph coordination lock, which may create its lock file.\n\
         Verified by doctor_identity_repair_previews_only_identity_writes_and_is_idempotent,\n\
         doctor_identity_repair_preserves_conflicts_and_dirty_checkouts,\n\
         doctor_identity_repair_adds_only_missing_declared_ids_without_advancing_head,\n\
         doctor_identity_repair_finds_moved_graph_from_an_ordinary_sibling,\n\
         concurrent_doctor_identity_repairs_do_not_duplicate_records,\n\
         doctor_identity_repair_explains_declared_submodule_workspace_boundary,\n\
         install_skips_submodule_subgraphs_and_records_only_the_parent,\n\
         install_heals_a_refused_gitlink_workspace_after_correction_on_main,\n\
         subgraph_symlink_and_blob_parents_refuse_and_preserve_identity_records and\n\
         install_identity_refusal_reports_an_already_completed_fast_forward in\n\
         crates/mdkg-cli/tests/install_worktree.rs.\n",
    );
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
