//! Usage errors (task-45): both output modes print the coded diagnostic,
//! never what was typed (edd-4: a typed value may be a secret), and the
//! hint names the typed command's own help.

mod common;

use std::process::Command;

use serde_json::Value;

/// Runs mdkg where no graph is needed (a parse fails first); returns
/// (exit code, stdout, stderr).
fn run(args: &[&str]) -> (i32, String, String) {
    let out = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .args(args)
        .current_dir(std::env::temp_dir())
        .output()
        .expect("run mdkg");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const SECRET: &str = "hunter2-sk_live_secret";

#[test]
fn text_mode_errors_are_coded_and_quote_nothing_typed() {
    let bad_value = format!("--priority={SECRET}");
    let unknown = format!("--{SECRET}");
    for args in [
        vec!["new", "task", "x", bad_value.as_str()],
        vec!["claim", unknown.as_str()],
        vec!["list", "--tags-mode", SECRET],
        vec!["show", "a", SECRET],
    ] {
        let (code, out, err) = run(&args);
        let all = format!("{out}{err}");
        assert_eq!(code, 1, "{args:?}: {all}");
        assert!(!all.contains(SECRET), "{args:?} echoed the value: {all}");
        assert!(
            all.contains("usage.invalid_arguments"),
            "{args:?} isn't coded: {all}"
        );
    }
}

#[test]
fn a_flag_with_fixed_values_lists_them() {
    let (_, out, err) = run(&["list", "--tags-mode", "sometimes", "--json"]);
    let v: Value = serde_json::from_str(&out).expect("one envelope");
    assert_eq!(v["ok"], false, "{v}");
    let d = &v["diagnostics"][0];
    let message = d["message"].as_str().unwrap_or_default();
    assert!(message.contains("expected one of: any, all"), "{v} {err}");
    assert!(!message.contains("sometimes"), "{v}");
}

#[test]
fn conflicts_name_both_arguments_without_quoting_their_values() {
    for (args, names) in [
        (
            vec!["agents", "add", "worker", "--worktree", "--cwd", SECRET],
            vec!["--worktree", "--cwd"],
        ),
        (
            vec![
                "agents",
                "edit",
                "worker",
                "--cwd",
                SECRET,
                "--worktree",
                "--json",
            ],
            vec!["--cwd", "--worktree"],
        ),
        (
            vec!["release", "--all", SECRET, "--json"],
            vec!["--all", "[TARGET]"],
        ),
    ] {
        let (code, out, err) = run(&args);
        let all = format!("{out}{err}");
        assert_eq!(code, 1, "{args:?}: {all}");
        assert!(!all.contains(SECRET), "{args:?} echoed the value: {all}");
        let message = if args.contains(&"--json") {
            let v: Value = serde_json::from_str(&out).expect("one envelope");
            assert!(err.is_empty(), "{args:?}: {err}");
            assert_eq!(v["ok"], false, "{v}");
            assert_eq!(
                v["diagnostics"][0]["code"], "usage.invalid_arguments",
                "{v}"
            );
            v["diagnostics"][0]["message"].as_str().unwrap().to_owned()
        } else {
            assert!(out.is_empty(), "{args:?}: {out}");
            assert!(err.contains("usage.invalid_arguments"), "{err}");
            err
        };
        assert!(
            message.contains("conflicting arguments"),
            "{args:?}: {message}"
        );
        for name in names {
            assert!(message.contains(name), "{args:?} omitted {name}: {message}");
        }
    }
}

#[test]
fn repeated_arguments_still_name_the_repeated_flag() {
    let (code, out, err) = run(&[
        "agents", "edit", "worker", "--cwd", SECRET, "--cwd", SECRET, "--json",
    ]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(!format!("{out}{err}").contains(SECRET));
    let v: Value = serde_json::from_str(&out).expect("one envelope");
    let message = v["diagnostics"][0]["message"].as_str().unwrap();
    assert!(message.contains("given more than once: --cwd"), "{v}");
}

#[test]
fn the_hint_names_the_typed_command_and_stops_at_the_terminator() {
    let hint = |args: &[&str]| {
        let (_, out, _) = run(args);
        let v: Value = serde_json::from_str(&out).expect("one envelope");
        v["diagnostics"][0]["hint"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let bin = exe.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    assert_eq!(
        hint(&["msg", "send", "--nope", "--json"]),
        format!("run `{bin} msg send --help`")
    );
    // An alias names the command it stands for; `schema` has no schema of
    // its own but still has a help page.
    assert_eq!(
        hint(&["skill", "nope", "--json"]),
        format!("run `{bin} skills --help`")
    );
    assert_eq!(
        hint(&["schema", "--json"]),
        format!("run `{bin} schema --help`")
    );
    // Global flags and their values are skipped, attached or not.
    assert_eq!(
        hint(&["--root", "msg", "--as=x", "claim", "--nope", "--json"]),
        format!("run `{bin} claim --help`")
    );
    // After `--` a word is a value: `send` here was never selected.
    assert_eq!(
        hint(&["msg", "--", "send", "--json"]),
        format!("run `{bin} msg --help`")
    );
    for flag in ["--root", "-r", "--as"] {
        assert_eq!(
            hint(&[flag, "--", "msg", "send", "--json"]),
            format!("run `{bin} --help`"),
            "{flag}"
        );
    }
    assert_eq!(hint(&["frob", "--json"]), format!("run `{bin} --help`"));
}
