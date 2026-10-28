//! Scoped setting pipeline before its command dispatch is connected.
#[path = "common/temp.rs"]
mod temp;

use std::path::PathBuf;

use mdkg_format::settings::{Environment, Layer};
use mdkg_graph::config::layers::Project;
use mdkg_run::config::{Prepared, Scope};
use serde_json::json;

struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = temp::dir("mdkg-set-pipeline");
        let dir = std::fs::canonicalize(dir).expect("canonical");
        let f = Self {
            root: dir.join("project"),
            home: dir.join("home"),
            dir,
        };
        for dir in [".git/objects", ".git/refs", ".mdkg"] {
            std::fs::create_dir_all(f.root.join(dir)).expect("mkdir");
        }
        std::fs::write(f.root.join(".git/HEAD"), b"ref: refs/heads/main\n").expect("HEAD");
        std::fs::write(f.root.join(".git/config"), b"[core]\n bare=false\n").expect("config");
        std::fs::write(f.root.join(".mdkg/config.json"),json!({"schema_version":3,"format_version":"0.7","graph_id":"0192f3a0-0000-7000-8000-000000000000","extra":{"keep":true}}).to_string()).expect("project");
        f
    }
    fn capture(&self) -> Project {
        Project::capture(
            &self.root,
            Environment {
                home: Some(&self.home),
                ..Environment::default()
            },
        )
        .expect("capture")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).expect("cleanup");
    }
}

#[test]
fn all_scopes_preview_validate_publish_and_reload_with_their_origin() {
    for scope in [Scope::Project, Scope::User, Scope::Local] {
        let f = Fixture::new();
        let graph = f.capture();
        let edit =
            Prepared::new(graph.paths(), scope, "work.priority_min", json!(4)).expect("prepare");
        let receipt = edit
            .apply(|candidate| {
                graph
                    .resolve_replacing(candidate, &[], None)
                    .and_then(|resolved| resolved.config())
                    .map(|_| ())
                    .map_err(|e| mdkg_run::RunError::usage(e.code().as_str(), e.diagnostic.message))
            })
            .expect("apply");
        let resolved = f.capture().resolve(&[], None).expect("reload");
        assert_eq!(resolved.config().expect("typed config").priority_min, 4);
        assert_eq!(
            resolved.effective.origins["/work/priority_min"].layer,
            scope.layer()
        );
        assert_eq!(
            resolved.effective.origins["/work/priority_min"].path,
            Some(receipt.path)
        );
        assert_eq!(
            resolved.effective.origins["/graph_id"].layer,
            Layer::Project
        );
        assert_eq!(resolved.effective.values["extra"]["keep"], true);
        assert!(!f.root.join(".git/mdkg/coord.sqlite").exists());
    }
}

#[test]
fn typed_validation_rejects_before_even_creating_the_user_directory() {
    let f = Fixture::new();
    let graph = f.capture();
    let edit = Prepared::new(
        graph.paths(),
        Scope::User,
        "templates.root_path",
        json!("/outside"),
    )
    .expect("syntactic candidate");
    let failure = edit
        .apply(|candidate| {
            graph
                .resolve_replacing(candidate, &[], None)
                .and_then(|resolved| resolved.config())
                .map(|_| ())
                .map_err(|e| mdkg_run::RunError::usage(e.code().as_str(), e.diagnostic.message))
        })
        .expect_err("typed refusal");
    assert!(!failure.published);
    assert!(!f.home.exists());
}
