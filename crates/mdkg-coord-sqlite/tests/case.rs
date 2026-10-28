//! Scopes ignore case (dec-2), so a repository that tracks two files
//! differing only in case is refused rather than read as one file.

#[path = "common/temp.rs"]
mod temp;

use std::path::{Path, PathBuf};
use std::process::Command;

use mdkg_coord_sqlite::git;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn run(dir: &Path, args: &[&str], stdin: Option<&str>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git runs");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write");
    }
    let out = child.wait_with_output().expect("git");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn files_that_differ_only_in_case_are_refused() {
    let dir = Scratch(temp::dir("mdkg-case"));
    run(&dir.0, &["init", "-q", "-b", "main"], None);
    // Built with plumbing, so it works on a disk that ignores case too.
    let upper = run(&dir.0, &["hash-object", "-w", "--stdin"], Some("upper\n"));
    let lower = run(&dir.0, &["hash-object", "-w", "--stdin"], Some("lower\n"));
    for (oid, path) in [(&upper, ".mdkg/README.md"), (&lower, ".mdkg/readme.md")] {
        run(
            &dir.0,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{oid},{path}"),
            ],
            None,
        );
    }
    let tree = run(&dir.0, &["write-tree"], None);
    let commit = run(&dir.0, &["commit-tree", &tree, "-m", "two cases"], None);
    run(&dir.0, &["update-ref", "refs/heads/main", &commit], None);

    let err = git::leaves(&dir.0, "main", ".mdkg").expect_err("two spellings of one file");
    let message = err.to_string();
    assert!(message.contains("differ only in case"), "{message}");
}

fn repo_with(files: &[(&str, &str)]) -> Scratch {
    let dir = Scratch(temp::dir("mdkg-paths"));
    for (path, text) in files {
        let p = dir.0.join(path);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
        std::fs::write(p, text).expect("write");
    }
    run(&dir.0, &["init", "-q", "-b", "main"], None);
    run(&dir.0, &["add", "."], None);
    run(&dir.0, &["commit", "-q", "-m", "init"], None);
    dir
}

#[test]
fn paths_match_as_scopes_do_never_as_globs() {
    let dir = repo_with(&[
        ("app/[id]/page.tsx", "a\n"),
        ("app/i/page.tsx", "b\n"),
        ("src/lib.rs", "c\n"),
        ("docs/Émile.md", "d\n"),
    ]);
    for (path, text) in [("app/[id]/page.tsx", "a2\n"), ("app/i/page.tsx", "b2\n")] {
        std::fs::write(dir.0.join(path), text).expect("write");
    }
    // `[id]` is a folder name, not a pattern: only that file is committed.
    assert!(git::commit_paths(&dir.0, &["app/[id]/page.tsx".into()], "one").expect("commit"));
    let committed = run(&dir.0, &["show", "--name-only", "--format=", "HEAD"], None);
    assert_eq!(committed, "app/[id]/page.tsx");

    // Any case, Unicode included, names the file as the disk spells it.
    std::fs::write(dir.0.join("src/lib.rs"), "c2\n").expect("write");
    assert!(git::commit_paths(&dir.0, &["SRC/LIB.RS".into()], "two").expect("commit"));
    assert_eq!(
        run(&dir.0, &["show", "--name-only", "--format=", "HEAD"], None),
        "src/lib.rs"
    );
    assert_eq!(
        git::files_under(&dir.0, "SRC").expect("files"),
        vec!["src/lib.rs"]
    );
    assert_eq!(git::disk_spelling(&dir.0, "DOCS/ÉMILE.MD"), "docs/Émile.md");
}

#[test]
fn a_commit_refuses_files_that_differ_only_in_case() {
    let dir = repo_with(&[("w/a.md", "a\n")]);
    // A second spelling in the index (plumbing: this disk may ignore case).
    let oid = run(&dir.0, &["hash-object", "-w", "--stdin"], Some("A\n"));
    run(
        &dir.0,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{oid},w/A.md"),
        ],
        None,
    );
    let err = git::commit_paths(&dir.0, &["w".into()], "both").expect_err("refused");
    assert!(err.to_string().contains("differ only in case"), "{err}");
}

#[test]
fn staged_renames_and_deletions_are_committed() {
    let dir = repo_with(&[
        (".mdkg/n/a.md", "a\n"),
        (".mdkg/n/b.md", "b\n"),
        (".mdkg/n/d.md", "d\n"),
    ]);
    run(&dir.0, &["mv", ".mdkg/n/a.md", ".mdkg/n/c.md"], None);
    run(&dir.0, &["rm", "-q", ".mdkg/n/d.md"], None);
    std::fs::write(dir.0.join(".mdkg/n/b.md"), "b2\n").expect("write");
    assert!(git::commit_paths(&dir.0, &[".mdkg/n".into()], "folder").expect("commit"));
    let committed = run(
        &dir.0,
        &["show", "--name-status", "--no-renames", "--format=", "HEAD"],
        None,
    );
    for line in [
        "D\t.mdkg/n/a.md",
        "M\t.mdkg/n/b.md",
        "A\t.mdkg/n/c.md",
        "D\t.mdkg/n/d.md",
    ] {
        assert!(
            committed.contains(line),
            "{line} missing from:\n{committed}"
        );
    }
    assert_eq!(
        run(&dir.0, &["status", "--porcelain"], None),
        "",
        "nothing left staged"
    );
}

#[test]
fn an_ignored_path_is_an_error_not_an_empty_commit() {
    let dir = repo_with(&[(".gitignore", "*.tmp\n"), (".mdkg/a.md", "a\n")]);
    std::fs::write(dir.0.join(".mdkg/x.tmp"), "x\n").expect("write");
    let err = git::commit_paths(&dir.0, &[".mdkg/x.tmp".into()], "x").expect_err("ignored");
    assert!(err.to_string().contains("matches no file"), "{err}");
}
