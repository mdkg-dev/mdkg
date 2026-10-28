//! Output is the same run to run for the same graph (edd-4): no timings in
//! it (the orchestrator's D7, 2026-10-24; `--verbose` puts one on stderr).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn isolated(cmd: &mut Command) -> &mut Command {
    common::without_launcher_env(cmd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
}

fn copy_tree(src: &Path, dest: &Path) {
    for entry in fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            fs::create_dir_all(&target).expect("mkdir");
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

fn repo() -> Scratch {
    let dir = common::temp::dir("mdkg-cli-reproducible");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    copy_tree(&conformance.join("base"), &dir);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &dir);
    fs::remove_file(dir.join("commands")).ok();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "."],
        &["commit", "-q", "-m", "init"],
    ] {
        let ok = isolated(Command::new("git").arg("-C").arg(&dir).args(args))
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?}");
    }
    Scratch(fs::canonicalize(dir).expect("canonical"))
}

fn run(dir: &Path, args: &[&str]) -> (Vec<u8>, String) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(dir)
        .args(args)
        .output()
        .expect("run mdkg");
    assert!(out.status.success(), "{args:?}");
    (
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn index_status_is_the_same_twice_and_times_only_on_stderr() {
    let repo = repo();
    // The first brings the index up to date; after that, nothing changes.
    run(&repo.0, &["index", "status", "--json"]);
    for args in [&["index", "status", "--json"][..], &["index", "status"]] {
        let (first, _) = run(&repo.0, args);
        let (second, _) = run(&repo.0, args);
        assert_eq!(first, second, "{args:?} differs run to run");
        assert!(
            !String::from_utf8_lossy(&first).contains("took"),
            "{args:?}"
        );
    }
    let (out, err) = run(&repo.0, &["index", "status", "--verbose"]);
    assert!(!String::from_utf8_lossy(&out).contains("took"));
    assert!(err.contains("took: "), "{err}");
}
