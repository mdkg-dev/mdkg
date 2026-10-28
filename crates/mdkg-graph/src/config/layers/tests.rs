use super::*;
use crate::config::{Unleased, WorkCommitted};
use serde_json::json;

struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    home: PathBuf,
    common: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mdkg-graph-layers-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("fixture");
        let dir = std::fs::canonicalize(dir).expect("canonical");
        let f = Self {
            root: dir.join("project"),
            home: dir.join("home"),
            common: dir.join("metadata.git"),
            dir,
        };
        f.metadata("metadata.git");
        f.write("project/.git", b"gitdir: ../metadata.git\n");
        f.project(json!({}));
        f
    }

    fn metadata(&self, name: &str) {
        for dir in ["objects", "refs"] {
            std::fs::create_dir_all(self.dir.join(name).join(dir)).expect("metadata directories");
        }
        self.write(&format!("{name}/HEAD"), b"ref: refs/heads/main\n");
        self.write(&format!("{name}/config"), b"[core]\n bare = false\n");
    }

    fn write(&self, name: &str, data: impl AsRef<[u8]>) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, data).expect("write");
        path
    }

    fn project(&self, patch: Value) {
        let mut v = json!({"schema_version":3, "format_version":"0.7", "graph_id":"0192f3a0-0000-7000-8000-000000000000"});
        v.as_object_mut()
            .expect("object")
            .extend(patch.as_object().expect("patch").clone());
        self.write("project/.mdkg/config.json", v.to_string());
    }

    fn user(&self, v: Value) -> PathBuf {
        self.write("home/.config/mdkg/config.json", v.to_string())
    }

    fn local(&self, v: Value) -> PathBuf {
        self.write("metadata.git/mdkg/config.local.json", v.to_string())
    }

    fn environment(&self) -> Environment<'_> {
        Environment {
            home: Some(&self.home),
            ..Environment::default()
        }
    }

    fn load(&self, additions: &[Input], flags: Option<&Value>) -> Result<Resolved, GraphError> {
        load(&self.root, self.environment(), additions, flags)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).expect("clean fixture");
    }
}

#[test]
fn typed_values_and_origins_follow_the_five_layers() {
    let f = Fixture::new();
    let user = f.user(json!({"work":{"priority_min":1,"priority_max":3}, "custom":{"keep":true}}));
    f.project(json!({"work":{"priority_min":2}}));
    let local =
        f.local(json!({"work":{"priority_max":7}, "coordination":{"leases":{"unleased":"block"}}}));
    let out = f
        .load(
            &[],
            Some(&json!({"work":{"priority_min":4},"index":{"tolerant":true}})),
        )
        .expect("layers");
    let cfg = out.config().expect("typed config");
    assert_eq!(
        (cfg.priority_min, cfg.priority_max, cfg.tolerant),
        (4, 7, true)
    );
    assert_eq!(cfg.leases.unleased, Unleased::Block);
    assert_eq!(
        out.effective.origins["/work/priority_min"].layer,
        Layer::Flags
    );
    assert_eq!(
        out.effective.origins["/work/priority_max"].path,
        Some(local)
    );
    assert_eq!(
        out.effective.origins["/work/status_enum/0"].layer,
        Layer::BuiltIn
    );
    assert_eq!(
        out.effective.origins["/graph_id"].path,
        Some(f.root.join(CONFIG_PATH))
    );
    assert_eq!(out.effective.origins["/custom/keep"].path, Some(user));
    assert_eq!(out.effective.values["custom"]["keep"], true);
    assert!(out.warnings.is_empty());
    assert!(!f.common.join("mdkg/coord.sqlite").exists());
}

#[test]
fn captured_xdg_input_and_git_warnings_reach_the_graph_result() {
    let f = Fixture::new();
    f.write(
        "home/.config/mdkg/config.json",
        b"bad JSON must not be read\n",
    );
    let user = f.write("xdg/mdkg/config.json", br#"{"work":{"priority_min":8}}"#);
    f.metadata("elsewhere.git");
    let xdg = f.dir.join("xdg");
    let project = Project::capture(
        &f.root,
        Environment {
            xdg_config_home: Some(&xdg),
            git: Some(git::Environment {
                cwd: &f.dir,
                dir: Some(Path::new("elsewhere.git")),
                common_dir: None,
            }),
            ..f.environment()
        },
    )
    .expect("capture");
    assert_eq!(
        project.paths().local,
        Some(f.common.join("mdkg/config.local.json"))
    );
    let out = project.resolve(&[], None).expect("resolve");
    assert_eq!(out.config().expect("config").priority_min, 8);
    assert_eq!(out.effective.origins["/work/priority_min"].path, Some(user));
    assert_eq!(out.warnings.len(), 1);
    assert_eq!(out.warnings[0].variable, git::Variable::Dir);
    assert_eq!(out.warnings[0].selected, Some(f.common.clone()));
    assert_eq!(out.warnings[0].resolved, Some(f.dir.join("elsewhere.git")));
}

#[test]
fn aliases_replace_lower_choices_without_manufacturing_a_conflict() {
    let f = Fixture::new();
    f.user(json!({"work":{"committed":"none"}}));
    f.project(json!({"work":{"location":"git"}}));
    assert_eq!(
        f.load(&[], None)
            .expect("project wins")
            .config()
            .expect("config")
            .work_committed,
        WorkCommitted::Definitions
    );
    let local = f.local(json!({"work":{"committed":"none"}}));
    let out = f.load(&[], None).expect("local alias wins");
    let cfg = out.config().expect("local config");
    assert_eq!(cfg.work_committed, WorkCommitted::None);
    assert_eq!(
        cfg.store_work_dir,
        Some(crate::store_work::under(&f.common, ""))
    );
    assert!(out.effective.values.pointer("/work/committed").is_none());
    assert_eq!(out.effective.origins["/work/location"].path, Some(local));
    let out = f
        .load(&[], Some(&json!({"work":{"committed":"definitions"}})))
        .expect("flag alias");
    assert_eq!(
        out.config().expect("config").work_committed,
        WorkCommitted::Definitions
    );
    assert_eq!(out.effective.origins["/work/location"].layer, Layer::Flags);
}

#[test]
fn conflicting_or_invalid_aliases_name_their_source_without_values() {
    for layer in [Layer::User, Layer::Project, Layer::Local, Layer::Flags] {
        for work in [
            json!({"location":"git","committed":"none"}),
            json!({"committed":"VALUE_SENTINEL"}),
            json!({"location":"VALUE_SENTINEL"}),
        ] {
            let f = Fixture::new();
            let patch = json!({"work":work});
            let path = match layer {
                Layer::User => Some(f.user(patch.clone())),
                Layer::Project => {
                    f.project(patch.clone());
                    Some(f.root.join(CONFIG_PATH))
                }
                Layer::Local => Some(f.local(patch.clone())),
                _ => None,
            };
            let flags = (layer == Layer::Flags).then_some(&patch);
            let error = f.load(&[], flags).expect_err("invalid source");
            assert_eq!(error.code(), Code::ConfigInvalid);
            assert_eq!(
                error.diagnostic.path,
                path.map(|p| p.to_string_lossy().into_owned())
            );
            assert!(!error.to_string().contains("VALUE_SENTINEL"));
        }
    }
}

#[test]
fn physical_identity_is_checked_before_any_optional_source() {
    let f = Fixture::new();
    f.write("home/.config/mdkg/config.json", b"not JSON\n");
    f.write("metadata.git/mdkg/config.local.json", b"not JSON\n");
    for patch in [
        json!({"schema_version":99}),
        json!({"schema_version":2}),
        json!({"graph_id":"bad"}),
        json!({"format_version":"later"}),
    ] {
        f.project(patch);
        let error = f
            .load(&[], Some(&json!({"schema_version":3})))
            .expect_err("physical identity first");
        assert_eq!(error.code(), Code::ConfigInvalid);
        assert_eq!(error.diagnostic.path.as_deref(), Some(CONFIG_PATH));
        assert!(!error.diagnostic.message.contains("User layer"));
    }
}

#[test]
fn project_bytes_and_common_dir_are_not_rediscovered_after_capture() {
    let f = Fixture::new();
    f.project(json!({"work":{"location":"local"}}));
    let captured = Project::capture(&f.root, f.environment()).expect("capture");
    let original_id = captured.input.value["graph_id"].clone();
    f.project(
        json!({"graph_id":"0192f3a0-0000-7000-8000-000000000001", "work":{"location":"local"}}),
    );
    f.metadata("other.git");
    f.write("project/.git", b"gitdir: ../other.git\n");
    let out = captured.resolve(&[], None).expect("captured layers");
    assert_eq!(out.effective.values["graph_id"], original_id);
    assert_eq!(
        out.config().expect("captured config").store_work_dir,
        Some(crate::store_work::under(&f.common, ""))
    );
    let fresh = f.load(&[], None).expect("new operation");
    assert_ne!(fresh.effective.values["graph_id"], original_id);
    assert_eq!(
        fresh.config().expect("fresh config").store_work_dir,
        Some(crate::store_work::under(&f.dir.join("other.git"), ""))
    );
}

#[test]
fn a_nested_graph_keeps_its_own_work_prefix_in_the_captured_repository() {
    let f = Fixture::new();
    f.write("project/nested/graph/.mdkg/config.json", json!({
        "schema_version":3, "format_version":"0.7", "graph_id":"0192f3a0-0000-7000-8000-000000000002", "work":{"location":"local"}
    }).to_string());
    let root = f.root.join("nested/graph");
    let cfg = load(&root, f.environment(), &[], None)
        .expect("nested layers")
        .config()
        .expect("typed config");
    assert_eq!(
        cfg.store_work_dir,
        Some(crate::store_work::under(&f.common, "nested/graph"))
    );
    assert_eq!(cfg, crate::config::load(&root).expect("physical config"));
}

#[test]
fn identity_patches_are_refused_in_every_nonphysical_source() {
    for layer in [Layer::User, Layer::Local, Layer::Flags] {
        let f = Fixture::new();
        let patch = json!({"graph_id":"IDENTITY_SENTINEL"});
        match layer {
            Layer::User => {
                f.user(patch.clone());
            }
            Layer::Local => {
                f.local(patch.clone());
            }
            _ => (),
        }
        let error = f
            .load(&[], (layer == Layer::Flags).then_some(&patch))
            .expect_err("project-only identity");
        assert!(
            error
                .diagnostic
                .message
                .contains("graph_id is project-only")
        );
        assert!(!error.to_string().contains("IDENTITY_SENTINEL"));
    }
    let f = Fixture::new();
    let definition = Input::new(
        Origin::file(Layer::Project, f.root.join(".mdkg/agents/worker.json")),
        json!({"graph_id":"IDENTITY_SENTINEL"}),
    )
    .expect("project-shaped input");
    let error = f
        .load(&[definition], None)
        .expect_err("only physical project identity");
    assert!(!error.to_string().contains("IDENTITY_SENTINEL"));
    assert!(
        error
            .diagnostic
            .path
            .expect("source path")
            .ends_with("agents/worker.json")
    );
}

#[test]
fn per_file_definitions_stay_below_local_and_flag_overrides() {
    let f = Fixture::new();
    f.user(json!({"agents":{"worker":{"model":"user", "effort":"high"}}}));
    f.project(json!({"agents":{"worker":{"model":"inline"}}}));
    f.local(json!({"agents":{"worker":{"model":"local"}}}));
    let source = f.root.join(".mdkg/agents/worker.json");
    let definition = Input::new(
        Origin::file(Layer::Project, &source),
        json!({"agents":{"worker":{"model":"definition","role":"worker","harness":"claude"}}}),
    )
    .expect("definition");
    let out = f
        .load(
            &[definition],
            Some(&json!({"agents":{"worker":{"model":"flag"}}})),
        )
        .expect("merged definition");
    assert_eq!(
        out.effective.values["agents"]["worker"],
        json!({"model":"flag","role":"worker","harness":"claude","effort":"high"})
    );
    assert_eq!(
        out.effective.origins["/agents/worker/model"].layer,
        Layer::Flags
    );
    assert_eq!(
        out.effective.origins["/agents/worker/role"].path,
        Some(source)
    );
    assert!(out.config().is_ok());
}

#[test]
fn optional_source_failures_are_visible_and_effective_errors_remain_inspectable() {
    let f = Fixture::new();
    let path = f.local(json!({"templates":{"root_path":"/outside"}}));
    let out = f.load(&[], None).expect("JSON can be inspected");
    let error = out.config().expect_err("invalid typed setting");
    assert_eq!(error.code(), Code::ConfigInvalid);
    assert!(
        error.diagnostic.path.is_none(),
        "do not falsely blame the project file"
    );
    assert_eq!(
        out.effective.origins["/templates/root_path"].path,
        Some(path.clone())
    );
    std::fs::write(&path, b"{bad\n").expect("bad source");
    assert_eq!(
        f.load(&[], None)
            .expect_err("bad local JSON")
            .diagnostic
            .path,
        Some(path.to_string_lossy().into_owned())
    );
    std::fs::remove_file(&path).expect("remove local");
    std::os::unix::fs::symlink(f.root.join(CONFIG_PATH), &path).expect("local link");
    assert!(f.load(&[], None).is_err(), "layer links stay refused");
}

#[test]
fn conditional_legacy_defaults_and_projection_selection_stay_with_their_validators() {
    let f = Fixture::new();
    f.write(
        "project/.mdkg/config.json",
        br#"{"workspaces":{"root":{"path":"."}}}"#,
    );
    f.user(json!({"customization":{"skill_mirrors":{"targets":["custom/skills"]}}}));
    let out = f.load(&[], None).expect("legacy config");
    let cfg = out.config().expect("legacy defaults");
    assert_eq!(cfg.work_committed, WorkCommitted::Definitions);
    assert_eq!(cfg.skill_mirror_targets, ["custom/skills"]);
    assert!(out.effective.values.get("projections").is_none());
    assert!(out.effective.values.pointer("/work/location").is_none());
    f.local(json!({"projections":[]}));
    assert!(
        f.load(&[], None)
            .expect("projection override")
            .config()
            .expect("config")
            .skill_mirror_targets
            .is_empty()
    );
}

#[test]
fn without_overrides_typed_values_match_existing_physical_loading() {
    let f = Fixture::new();
    for patch in [
        json!({}),
        json!({"work":{"committed":"none"}}),
        json!({"projections":[],"work":{"priority_max":4}}),
    ] {
        f.project(patch);
        assert_eq!(
            f.load(&[], None).expect("layers").config().expect("typed"),
            crate::config::load(&f.root).expect("physical")
        );
    }
    f.write(
        "project/.mdkg/config.json",
        br#"{"schema_version":1,"workspaces":{"root":{"path":"."}}}"#,
    );
    assert_eq!(
        f.load(&[], None)
            .expect("legacy layers")
            .config()
            .expect("typed"),
        crate::config::load(&f.root).expect("legacy physical")
    );
}

#[test]
fn scoped_replacements_preview_at_their_own_priority_without_writes() {
    let f = Fixture::new();
    let user = f.user(json!({"work":{"priority_min":1}}));
    f.project(json!({"work":{"priority_min":2}}));
    let local = f.local(json!({"work":{"priority_min":3}}));
    let project = Project::capture(&f.root, f.environment()).expect("capture");
    for (layer, path, expected) in [
        (Layer::User, user, 3),
        (Layer::Project, f.root.join(CONFIG_PATH), 3),
        (Layer::Local, local, 9),
    ] {
        let before = std::fs::read(&path).expect("before");
        let input = Input::parse(Origin::file(layer, &path), &before).expect("source");
        let replacement = mdkg_format::settings::edit::set(&input, "work.priority_min", json!(9))
            .expect("candidate");
        let preview = project
            .resolve_replacing(&replacement, &[], None)
            .expect("preview");
        assert_eq!(
            preview.config().expect("typed preview").priority_min,
            expected
        );
        assert_eq!(std::fs::read(&path).expect("unchanged"), before);
        assert_eq!(
            project
                .resolve(&[], None)
                .expect("actual")
                .config()
                .expect("actual config")
                .priority_min,
            3
        );
    }
}

#[test]
fn replacement_paths_and_project_identity_cannot_escape_the_capture() {
    let f = Fixture::new();
    let project = Project::capture(&f.root, f.environment()).expect("capture");
    let wrong = Input::new(
        Origin::file(Layer::User, f.dir.join("other.json")),
        json!({}),
    )
    .expect("input");
    assert!(project.resolve_replacing(&wrong, &[], None).is_err());
    for key in ["schema_version", "format_version", "graph_id"] {
        let mut value = project.input.value.clone();
        value[key] = json!("IDENTITY_SENTINEL");
        let replacement = Input::new(project.input.origin.clone(), value).expect("project input");
        let error = project
            .resolve_replacing(&replacement, &[], None)
            .expect_err("identity frozen");
        assert!(!error.to_string().contains("IDENTITY_SENTINEL"));
    }
    for layer in [Layer::BuiltIn, Layer::Flags] {
        let replacement = Input::new(Origin::value(layer), json!({})).expect("input");
        assert!(project.resolve_replacing(&replacement, &[], None).is_err());
    }
}

#[test]
fn simultaneous_replacements_keep_capture_and_validate_before_optional_reads() {
    let f = Fixture::new();
    let user = f.user(json!({"work":{"priority_min":1}}));
    let local = f.local(json!({"work":{"priority_max":4}}));
    f.project(json!({"work":{"priority_min":2}}));
    let project = Project::capture(&f.root, f.environment()).expect("capture");
    let captured = project.project_input().clone();
    let mut cleaned = captured.clone();
    cleaned.value.as_object_mut().unwrap().remove("work");
    let user = Input::new(
        Origin::file(Layer::User, user),
        json!({"work":{"priority_min":3}}),
    )
    .unwrap();
    let local = Input::new(
        Origin::file(Layer::Local, local),
        json!({"work":{"priority_max":8}}),
    )
    .unwrap();
    // Replacements must be used as captured, even if their files now differ.
    f.write("project/.mdkg/config.json", "unreadable project");
    f.write("home/.config/mdkg/config.json", "unreadable user");
    f.write("metadata.git/mdkg/config.local.json", "unreadable local");
    let resolved = project
        .resolve_replacements(&[cleaned.clone(), user.clone(), local], &[], None)
        .expect("all inputs supplied");
    let config = resolved.config().unwrap();
    assert_eq!((config.priority_min, config.priority_max), (3, 8));
    assert_eq!(project.project_input().value, captured.value);
    assert_eq!(
        resolved.effective.origins["/work/priority_min"].layer,
        Layer::User
    );
    let error = project
        .resolve_replacements(&[user.clone(), user], &[], None)
        .expect_err("duplicate layer before optional read");
    assert!(error.to_string().contains("duplicate replacement layer"));
    cleaned.value["graph_id"] = json!("IDENTITY_SENTINEL");
    let error = project
        .resolve_replacements(&[cleaned], &[], None)
        .expect_err("identity before optional read");
    assert!(
        error
            .to_string()
            .contains("project identity cannot be changed")
    );
    assert!(!error.to_string().contains("IDENTITY_SENTINEL"));
}
