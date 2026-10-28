//! `mdkg --version` and `version --json` name the commit the binary
//! was built from, so a reinstall can confirm what it installed.

use std::process::Command;

use serde_json::Value;

mod common;

/// The commit build.rs recorded for this build: the binary under test was
/// compiled from the same run, so it must report this one. Live HEAD can
/// move after the build (a commit while check.sh runs).
const BUILT_FROM: &str = env!("MDKG_BUILD_COMMIT");

/// Whether `commit` is the checkout's HEAD or an ancestor of it. Git
/// failing (no checkout, no such object) panics, saying so. No `GIT_*`
/// variable (a hook's `GIT_DIR`) points it at another repository.
fn reaches_head(commit: &str) -> bool {
    let mut git = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            git.env_remove(key);
        }
    }
    let status = git
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(["merge-base", "--is-ancestor", commit, "HEAD"])
        .status()
        .expect("git runs");
    match status.code() {
        Some(0) => true,
        Some(1) => false,
        _ => panic!("git merge-base failed ({status}) for {commit}"),
    }
}

#[test]
fn version_names_the_commit_it_was_built_from() {
    let out = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .args(["version", "--json"])
        .output()
        .expect("run mdkg");
    assert!(out.status.success());
    let envelope: Value = serde_json::from_slice(&out.stdout).expect("json");
    common::conforms("version", &envelope);
    let data = &envelope["data"];
    assert!(
        matches!(BUILT_FROM.len(), 40 | 64) && BUILT_FROM.bytes().all(|b| b.is_ascii_hexdigit()),
        "a full commit id: {BUILT_FROM}"
    );
    assert_eq!(data["commit"], BUILT_FROM, "{envelope}");
    assert!(
        reaches_head(BUILT_FROM),
        "{BUILT_FROM} is HEAD or behind it"
    );
    assert_eq!(data["version"], env!("CARGO_PKG_VERSION"));
    assert!(data["dirty"].is_boolean(), "{envelope}");
    assert_eq!(data["lease_authority"], 1);
    let built = data["built"].as_str().expect("built");
    assert!(built.len() == 20 && built.ends_with('Z'), "{built}");

    let line = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--version")
        .output()
        .expect("run mdkg");
    assert!(line.status.success());
    let text = String::from_utf8(line.stdout).expect("utf-8");
    assert!(
        text.starts_with(&format!(
            "mdkg {} (commit {BUILT_FROM}",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    // The plain command says the same, and needs no graph.
    let plain = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("version")
        .current_dir(std::env::temp_dir())
        .output()
        .expect("run mdkg");
    assert!(plain.status.success());
    assert_eq!(String::from_utf8(plain.stdout).expect("utf-8"), text);
}
