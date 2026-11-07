//! `holds`'s text when nothing is held: the claim it suggests takes the
//! same scope, from the graph root `claim` reads paths from, and never
//! carries a name a shell would read as more than a word (Terra #17810).

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn mdkg(root: &Path, args: &[&str]) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    common::without_launcher_env(&mut c)
        .arg("--root")
        .arg(root)
        .args(["--as", "tester"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("mdkg")
}

#[test]
fn the_suggested_claim_takes_the_same_path_and_is_shell_safe() {
    let tmp = Dir(std::env::temp_dir().join(format!("mdkg-holds-{}", uuid::Uuid::now_v7())));
    let repo = tmp.0.join("repo");
    let graph = repo.join("sub");
    std::fs::create_dir_all(graph.join("src")).expect("folders");
    git(&repo, &["init", "-q", "-b", "main"]);
    let out = mdkg(&graph, &["init", "--in-checkout", "--json"]);
    assert!(out.status.success(), "init: {out:?}");
    std::fs::write(graph.join("src/lib.rs"), "").expect("file");
    let odd = "src/$(touch pwn).rs";
    std::fs::write(graph.join(odd), "").expect("odd file");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "a nested graph"]);
    // `holds` takes the scope from the repository's top; the claim it
    // suggests names it from the graph root, as `claim` reads it.
    let out = mdkg(&graph, &["holds", "sub/src/lib.rs"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(text.starts_with("not held: sub/src/lib.rs; "), "{text}");
    assert!(text.contains(" claim src/lib.rs` takes it"), "{text}");
    assert!(!text.contains("claim sub/src/lib.rs"), "{text}");
    // A name a shell would expand isn't pasted into the command.
    let out = mdkg(&graph, &["holds", &format!("sub/{odd}")]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(text.contains(" claim <path>` takes it"), "{text}");
    assert!(!text.contains("claim src/$("), "{text}");
}
