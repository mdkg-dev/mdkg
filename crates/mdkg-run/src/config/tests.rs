use super::*;
use mdkg_format::settings::Environment;
use serde_json::json;

struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("mdkg-config-edit-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("fixture");
        let dir = std::fs::canonicalize(dir).expect("canonical");
        let f = Self {
            root: dir.join("project"),
            home: dir.join("home"),
            dir,
        };
        for path in ["project/.git/objects", "project/.git/refs"] {
            std::fs::create_dir_all(f.dir.join(path)).expect("metadata");
        }
        f.write("project/.git/HEAD", b"ref: refs/heads/main\n");
        f.write("project/.git/config", b"[core]\n bare=false\n");
        f.write("project/.mdkg/config.json", br#"{"schema_version":3,"format_version":"0.7","graph_id":"0192f3a0-0000-7000-8000-000000000000","unknown":{"keep":true}}"#);
        f
    }
    fn write(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, bytes).expect("write");
        path
    }
    fn paths(&self) -> Paths {
        Paths::discover(
            &self.root,
            Environment {
                home: Some(&self.home),
                ..Environment::default()
            },
        )
        .expect("paths")
    }
    fn source(&self, scope: Scope) -> PathBuf {
        let paths = self.paths();
        match scope {
            Scope::Project => paths.project,
            Scope::User => paths.user.expect("home"),
            Scope::Local => paths.local.expect("repo"),
        }
    }
    fn prepare(&self, scope: Scope, key: &str, value: Value) -> Prepared {
        Prepared::new(&self.paths(), scope, key, value).expect("prepare")
    }
    fn assert_no_stage(&self, folder: &Path) {
        assert!(
            !std::fs::read_dir(folder).expect("list").any(|e| e
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".mdkg-config-")),
            "staging cleaned"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).expect("cleanup");
    }
}

#[test]
fn each_scope_changes_only_its_file_and_keeps_unknown_keys() {
    for scope in [Scope::Project, Scope::User, Scope::Local] {
        let f = Fixture::new();
        let paths = f.paths();
        let project_before = std::fs::read(&paths.project).expect("project");
        let project_mode = std::fs::metadata(&paths.project)
            .expect("project mode")
            .mode()
            & 0o777;
        let edit = f.prepare(scope, "work.priority_min", json!(2));
        assert_eq!(edit.candidate().origin.layer, scope.layer());
        assert_eq!(edit.candidate().value["work"]["priority_min"], 2);
        if scope != Scope::Project {
            assert!(!f.source(scope).exists(), "preparation has no writes");
        }
        let receipt = edit
            .apply(|input| {
                assert_eq!(input.value["work"]["priority_min"], 2);
                Ok(())
            })
            .expect("apply");
        assert!(receipt.changed);
        assert_eq!(receipt.path, f.source(scope));
        let value: Value =
            serde_json::from_slice(&std::fs::read(&receipt.path).expect("source")).expect("JSON");
        assert_eq!(value["work"]["priority_min"], 2);
        if scope == Scope::Project {
            assert_eq!(value["unknown"]["keep"], true);
        } else {
            assert_eq!(
                std::fs::read(&paths.project).expect("unchanged project"),
                project_before
            );
        }
        assert_eq!(
            std::fs::metadata(&receipt.path).expect("mode").mode() & 0o777,
            if scope == Scope::Project {
                project_mode
            } else {
                0o600
            }
        );
        f.assert_no_stage(receipt.path.parent().expect("parent"));
        assert!(!f.root.join(".git/mdkg/coord.sqlite").exists());
    }
}

#[test]
fn no_op_preserves_bytes_mode_and_inode() {
    let f = Fixture::new();
    let path = f.source(Scope::Project);
    let before = std::fs::read(&path).expect("before");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("mode");
    let metadata = std::fs::metadata(&path).expect("meta");
    let out = f
        .prepare(Scope::Project, "unknown.keep", json!(true))
        .apply(|_| Ok(()))
        .expect("no-op");
    assert!(!out.changed);
    assert_eq!(std::fs::read(&path).expect("after"), before);
    let after = std::fs::metadata(&path).expect("after meta");
    assert_eq!(
        (after.dev(), after.ino(), after.mode()),
        (metadata.dev(), metadata.ino(), metadata.mode())
    );
    f.assert_no_stage(path.parent().expect("parent"));
}

#[test]
fn validation_refusal_makes_no_directories_or_files() {
    let f = Fixture::new();
    let edit = f.prepare(Scope::User, "tool", json!("test"));
    let error = edit
        .apply(|_| Err(invalid("rejected candidate")))
        .expect_err("refused");
    assert!(!error.published);
    assert!(!f.home.exists());
}

#[test]
fn stale_bytes_replaced_files_and_competing_creations_are_refused() {
    for replace_inode in [false, true] {
        let f = Fixture::new();
        let path = f.source(Scope::Project);
        let edit = f.prepare(Scope::Project, "tool", json!("new"));
        let other = b"{\"external\":true}\n";
        if replace_inode {
            std::fs::rename(&path, path.with_extension("old")).expect("move");
        }
        std::fs::write(&path, other).expect("external edit");
        let error = edit.apply(|_| Ok(())).expect_err("stale");
        assert_eq!(error.error.kind, ErrorKind::Conflict);
        assert!(!error.published);
        assert_eq!(std::fs::read(&path).expect("keep edit"), other);
    }
    let f = Fixture::new();
    let first = f.prepare(Scope::User, "one", json!(1));
    let second = f.prepare(Scope::User, "two", json!(2));
    first.apply(|_| Ok(())).expect("first");
    assert_eq!(
        second
            .apply(|_| Ok(()))
            .expect_err("stale creation")
            .error
            .kind,
        ErrorKind::Conflict
    );
    let value: Value =
        serde_json::from_slice(&std::fs::read(f.source(Scope::User)).expect("user")).expect("JSON");
    assert_eq!(value, json!({"one":1}));
}

#[test]
fn named_directory_swaps_cannot_redirect_a_write() {
    for scope in [Scope::Project, Scope::User, Scope::Local] {
        for checkpoint in [Checkpoint::Validated, Checkpoint::Staged] {
            let f = Fixture::new();
            let path = f.source(scope);
            if scope != Scope::Project {
                f.write(
                    path.strip_prefix(&f.dir)
                        .expect("relative")
                        .to_str()
                        .expect("UTF8"),
                    b"{}",
                );
            }
            let edit = f.prepare(scope, "tool", json!("new"));
            let parent = path.parent().expect("parent").to_path_buf();
            let moved = parent.with_file_name("moved-settings");
            let outside = f.dir.join("outside");
            std::fs::create_dir(&outside).expect("outside");
            let result = edit.apply_with(
                |_| Ok(()),
                |at| {
                    if at == checkpoint {
                        std::fs::rename(&parent, &moved)?;
                        std::os::unix::fs::symlink(&outside, &parent)?;
                    }
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert_eq!(
                std::fs::read_dir(&outside)
                    .expect("outside untouched")
                    .count(),
                0
            );
            f.assert_no_stage(&moved);
        }
    }
}

#[test]
fn failed_write_and_post_publication_failure_report_the_actual_state() {
    for checkpoint in [
        Checkpoint::Validated,
        Checkpoint::Staged,
        Checkpoint::Published,
    ] {
        let f = Fixture::new();
        let path = f.source(Scope::Project);
        let before = std::fs::read(&path).expect("before");
        let error = f
            .prepare(Scope::Project, "tool", json!("new"))
            .apply_with(
                |_| Ok(()),
                |at| {
                    if at == checkpoint {
                        Err(io::Error::from_raw_os_error(
                            rustix::io::Errno::NOSPC.raw_os_error(),
                        ))
                    } else {
                        Ok(())
                    }
                },
            )
            .expect_err("injected write failure");
        assert_eq!(error.published, checkpoint == Checkpoint::Published);
        let after = std::fs::read(&path).expect("after");
        if error.published {
            assert_eq!(
                serde_json::from_slice::<Value>(&after).expect("complete JSON")["tool"],
                "new"
            );
            assert!(error.to_string().contains("was published"));
        } else {
            assert_eq!(after, before);
        }
        f.assert_no_stage(path.parent().expect("parent"));
    }
}

#[test]
fn concurrent_setters_keep_the_lock_across_replacement() {
    for checkpoint in [Checkpoint::Staged, Checkpoint::Published] {
        let f = Fixture::new();
        let first = f.prepare(Scope::Project, "one", json!(1));
        first
            .apply_with(
                |_| Ok(()),
                |at| {
                    if at == checkpoint {
                        let second = f.prepare(Scope::Project, "two", json!(2));
                        let failure = second.apply(|_| Ok(())).expect_err("busy file lock");
                        assert!(!failure.published);
                        assert_eq!(failure.error.kind, ErrorKind::Conflict);
                    }
                    Ok(())
                },
            )
            .expect("first");
        let value: Value =
            serde_json::from_slice(&std::fs::read(f.source(Scope::Project)).expect("source"))
                .expect("JSON");
        assert_eq!(value["one"], 1);
        assert!(value.get("two").is_none());
    }
}

#[test]
fn links_special_files_and_bad_json_are_refused_without_touching_their_targets() {
    let f = Fixture::new();
    let path = f.source(Scope::User);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    std::os::unix::fs::symlink(f.source(Scope::Project), &path).expect("link");
    assert!(Prepared::new(&f.paths(), Scope::User, "tool", json!("bad")).is_err());
    std::fs::remove_file(&path).expect("remove link");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo")
            .success()
    );
    assert!(Prepared::new(&f.paths(), Scope::User, "tool", json!("bad")).is_err());
    std::fs::remove_file(&path).expect("remove FIFO");
    std::fs::write(&path, b"{not JSON\n").expect("bad source");
    assert_eq!(
        Prepared::new(&f.paths(), Scope::User, "tool", json!("bad"))
            .expect_err("bad source")
            .kind,
        ErrorKind::Failure
    );
    assert_eq!(std::fs::read(&path).expect("unchanged"), b"{not JSON\n");
}

#[test]
fn update_replaces_hard_links_and_preserves_existing_permission_bits() {
    let f = Fixture::new();
    let path = f.source(Scope::Project);
    let sibling = f.dir.join("original.json");
    std::fs::hard_link(&path, &sibling).expect("hard link");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("mode");
    let original = std::fs::read(&path).expect("old bytes");
    f.prepare(Scope::Project, "tool", json!("new"))
        .apply(|_| Ok(()))
        .expect("replace");
    assert_eq!(std::fs::read(&sibling).expect("other link"), original);
    assert_eq!(
        std::fs::metadata(&path).expect("mode").mode() & 0o777,
        0o640
    );
}

#[test]
fn a_new_link_in_a_missing_parent_cannot_redirect_creation() {
    let f = Fixture::new();
    let edit = f.prepare(Scope::User, "tool", json!("new"));
    let outside = f.dir.join("outside");
    std::fs::create_dir(&outside).expect("outside");
    std::fs::create_dir(&f.home).expect("home");
    std::os::unix::fs::symlink(&outside, f.home.join(".config")).expect("parent link");
    let error = edit.apply(|_| Ok(())).expect_err("link refusal");
    assert!(!error.published);
    assert_eq!(std::fs::read_dir(&outside).expect("untouched").count(), 0);
}

#[test]
fn source_and_output_byte_caps_do_not_break_no_op_preservation() {
    let f = Fixture::new();
    let large = json!({"items":vec![0;170_000],"keep":true});
    let compact = serde_json::to_vec(&large).expect("compact");
    assert!(compact.len() as u64 <= MAX_BYTES);
    assert!(serde_json::to_vec_pretty(&large).expect("pretty").len() as u64 > MAX_BYTES);
    let path = f.write("home/.config/mdkg/config.json", &compact);
    assert!(
        !f.prepare(Scope::User, "keep", json!(true))
            .apply(|_| Ok(()))
            .expect("no-op")
            .changed
    );
    assert!(
        Prepared::new(&f.paths(), Scope::User, "keep", json!(false)).is_err(),
        "expanded file would exceed the cap"
    );
    assert_eq!(std::fs::read(&path).expect("unchanged bytes"), compact);
    File::options()
        .write(true)
        .open(&path)
        .expect("file")
        .set_len(MAX_BYTES + 1)
        .expect("oversize source");
    assert!(Prepared::new(&f.paths(), Scope::User, "keep", json!(false)).is_err());
}

#[test]
fn unsupported_scopes_and_absent_projects_are_refused() {
    let f = Fixture::new();
    let mut paths = f.paths();
    paths.user = None;
    assert!(Prepared::new(&paths, Scope::User, "tool", json!("x")).is_err());
    paths.repository = None;
    paths.local = None;
    assert!(Prepared::new(&paths, Scope::Local, "tool", json!("x")).is_err());
    std::fs::remove_file(f.source(Scope::Project)).expect("remove project");
    assert!(Prepared::new(&f.paths(), Scope::Project, "tool", json!("x")).is_err());
    assert!(!f.home.exists());
    let paths = Paths::discover(
        &f.dir,
        Environment {
            home: Some(&f.home),
            ..Environment::default()
        },
    )
    .expect("outside checkout");
    assert!(paths.repository.is_none());
    Prepared::new(&paths, Scope::User, "tool", json!("global"))
        .expect("user-only operation")
        .apply(|_| Ok(()))
        .expect("write user settings without a graph");
    assert!(paths.user.expect("user path").is_file());
}

#[test]
fn a_change_during_staging_is_detected_before_publication() {
    let f = Fixture::new();
    let path = f.source(Scope::Project);
    let external = b"{\"edited_while_staging\":true}\n";
    let error = f
        .prepare(Scope::Project, "tool", json!("new"))
        .apply_with(
            |_| Ok(()),
            |at| {
                if at == Checkpoint::Staged {
                    std::fs::write(&path, external)?;
                }
                Ok(())
            },
        )
        .expect_err("changed baseline");
    assert!(!error.published);
    assert_eq!(error.error.kind, ErrorKind::Conflict);
    assert_eq!(std::fs::read(&path).expect("external preserved"), external);
    f.assert_no_stage(path.parent().expect("parent"));
}

#[test]
fn retry_preserves_abandoned_stages_from_an_earlier_process() {
    let f = Fixture::new();
    let abandoned = f.write(
        "project/.mdkg/.mdkg-config-abandoned.tmp",
        b"previous private stage\n",
    );
    f.prepare(Scope::Project, "tool", json!("new"))
        .apply(|_| Ok(()))
        .expect("retry");
    assert_eq!(
        std::fs::read(abandoned).expect("stage retained"),
        b"previous private stage\n"
    );
}
