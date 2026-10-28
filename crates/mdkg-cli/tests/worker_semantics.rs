//! Semantic refusals do not depend on a harness's command matching rules.

use std::fs;
use std::process::Command;

mod common;

fn isolated(command: &mut Command, dir: &std::path::Path) {
    common::without_launcher_env(command)
        .current_dir(dir)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
}

#[test]
fn agent_auth_and_abandon_refusals_handle_global_option_order() {
    let dir = std::env::temp_dir().join(format!("mdkg-agent-semantics-{}", uuid::Uuid::now_v7()));
    fs::create_dir_all(&dir).expect("fixture");
    let mut git = Command::new("git");
    isolated(&mut git, &dir);
    assert!(
        git.args(["init", "--quiet"])
            .output()
            .expect("git init")
            .status
            .success()
    );
    let mut init = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    isolated(&mut init, &dir);
    let initialized = init
        .args(["init", "--work", "local"])
        .output()
        .expect("init");
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    // Auth must refuse before trying even this inert credential source.
    let source = dir.join("key-source");
    fs::write(
        &source,
        "#!/bin/sh\ntouch key-source-ran\nprintf 'fake-key\\n'\n",
    )
    .expect("fake key source");
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).expect("executable");
    for prefix in [
        vec![],
        vec!["--root", "."],
        vec!["--as", "other", "--root", "."],
    ] {
        for sub in [
            vec!["auth", "key", "anthropic"],
            vec!["abandon", "--branch", "other", "--json"],
        ] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_mdkg"));
            isolated(&mut command, &dir);
            let out = command
                .current_dir(&dir)
                .args(&prefix)
                .args(&sub)
                .env("MDKG_RUN_TURN", "test-turn")
                .env("MDKG_RUN_SECURITY", &source)
                .output()
                .expect("mdkg");
            assert!(!out.status.success(), "{prefix:?} {sub:?}");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                text.contains("refused in an agent turn"),
                "{prefix:?} {sub:?}: {text}"
            );
            assert!(!text.contains("fake-key"), "{text}");
            assert!(!dir.join("key-source-ran").exists());
        }
    }
    let mut search = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    isolated(&mut search, &dir);
    let ordinary = search
        .args(["--as", "worker", "search", "auth", "abandon", "--json"])
        .env("MDKG_RUN_TURN", "test-turn")
        .output()
        .expect("ordinary command");
    assert!(
        ordinary.status.success(),
        "{}{}",
        String::from_utf8_lossy(&ordinary.stdout),
        String::from_utf8_lossy(&ordinary.stderr)
    );

    // The fixed launcher helper clears its mark and can read only our fake.
    let helper = mdkg_run::profile::key_helper(
        std::path::Path::new(env!("CARGO_BIN_EXE_mdkg")),
        "mdkg-anthropic",
    );
    let mut shell = Command::new("/bin/sh");
    isolated(&mut shell, &dir);
    let key = shell
        .args(["-c", &helper])
        .env("MDKG_RUN_TURN", "test-turn")
        .env("MDKG_RUN_SECURITY", &source)
        .output()
        .expect("launcher helper");
    assert!(
        key.status.success(),
        "{}",
        String::from_utf8_lossy(&key.stderr)
    );
    assert_eq!(key.stdout, b"fake-key\n");
    assert!(dir.join("key-source-ran").exists());
    fs::remove_dir_all(&dir).expect("cleanup");
}
