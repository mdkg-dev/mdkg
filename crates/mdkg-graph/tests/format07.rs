//! Loading and validating format 0.7 graphs (docs/format-0.7.md).

use std::path::{Path, PathBuf};

use mdkg_format::Code;
use mdkg_graph::resolve::{Resolution, resolve_qid};
use mdkg_graph::{config, index, validate};

const GRAPH: &str = "0192f3a0-0000-7000-8000-000000000000";
const GOAL: &str = "goal-0192f3a4-0000-7000-8000-000000000001";
const TASK: &str = "task-0192f3a4-0000-7000-8000-000000000002";
const STAMP: &str = "2026-10-24T18:03:11.482Z";

struct Graph(PathBuf);

impl Drop for Graph {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

const V07: &str = r#""schema_version": 3, "format_version": "0.7", "graph_id": "0192f3a0-0000-7000-8000-000000000000""#;
const ROOT_WS: &str = r#""root": {"path": ".", "enabled": true, "mdkg_dir": ".mdkg"}"#;

fn graph(name: &str, files: &[(&str, String)]) -> Graph {
    graph_with(
        name,
        &format!(r#"{{{V07}, "workspaces": {{{ROOT_WS}}}}}"#),
        files,
    )
}

fn graph_with(name: &str, config: &str, files: &[(&str, String)]) -> Graph {
    let root = std::env::temp_dir().join(format!(
        "mdkg-format07-{name}-{}-{}",
        std::process::id(),
        mdkg_format::Uuid7::now()
    ));
    std::fs::create_dir_all(root.join(".mdkg/work")).expect("test setup");
    std::fs::write(root.join(".mdkg/config.json"), config).expect("test setup");
    for (rel, text) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("test setup")).expect("test setup");
        std::fs::write(path, text).expect("test setup");
    }
    Graph(root)
}

fn node(id: &str, node_type: &str, extra: &str, body: &str) -> String {
    let status = if node_type == "goal" {
        "status: todo\ngoal_state: active\ngoal_condition: done\n"
    } else {
        "status: todo\n"
    };
    format!(
        "---\nid: {id}\ntype: {node_type}\ntitle: {node_type} title\n{status}created: {STAMP}\nupdated: {STAMP}\n{extra}---\n{body}"
    )
}

fn load(root: &Path) -> Result<mdkg_graph::Index, mdkg_graph::GraphError> {
    let config = config::load(root)?;
    index::build(root, &config)
}

fn codes(root: &Path) -> (Vec<Code>, Vec<Code>) {
    let config = config::load(root).expect("test setup");
    let receipt = validate::validate(root, &config).expect("test setup");
    (
        receipt.errors.iter().map(|d| d.code).collect(),
        receipt.warnings.iter().map(|d| d.code).collect(),
    )
}

#[test]
fn a_v07_graph_links_by_uuid_and_resolves_legacy_ids() {
    let g = graph(
        "links",
        &[
            (
                ".mdkg/work/goal-ship.md",
                node(GOAL, "goal", "legacy_id: goal-4\n", ""),
            ),
            (
                ".mdkg/work/task-do.md",
                node(
                    TASK,
                    "task",
                    &format!("legacy_id: task-19\nparent: {GOAL}\nx-acme.owner: a\n"),
                    &format!("See [the goal]({GOAL}).\n"),
                ),
            ),
        ],
    );
    let ix = load(&g.0).expect("test setup");
    let task = ix.get(&format!("root:{TASK}")).expect("test setup");
    assert_eq!(
        task.edges.parent.as_deref(),
        Some(format!("root:{GOAL}").as_str())
    );
    assert_eq!(task.legacy_id.as_deref(), Some("task-19"));
    assert_eq!(
        ix.reverse("parent", &format!("root:{GOAL}")),
        [format!("root:{TASK}")]
    );
    for handle in [TASK.to_string(), "task-19".into(), "TASK-19".into()] {
        assert_eq!(
            resolve_qid(&ix, &handle, None),
            Resolution::Ok(format!("root:{TASK}")),
            "{handle}"
        );
    }
    // A stale type still resolves by UUID.
    let stale = TASK.replacen("task", "bug", 1);
    assert_eq!(
        resolve_qid(&ix, &stale, None),
        Resolution::Ok(format!("root:{TASK}"))
    );
    let (errors, warnings) = codes(&g.0);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!warnings.contains(&Code::NodeStaleType), "{warnings:?}");
}

#[test]
fn stale_types_dates_and_legacy_keys_warn() {
    // The task names its goal as a `feat`, and is itself typed `bug` in its id.
    let stale_goal = GOAL.replacen("goal", "feat", 1);
    let bug_id = TASK.replacen("task", "bug", 1);
    let task = node(
        &bug_id,
        "task",
        &format!("parent: {stale_goal}\nx_owner: a\n"),
        &format!("[g]({stale_goal})\n"),
    )
    .replacen(&format!("created: {STAMP}"), "created: 2026-10-24", 1);
    let g = graph(
        "stale",
        &[
            (".mdkg/work/goal-ship.md", node(GOAL, "goal", "", "")),
            (".mdkg/work/task-do.md", task),
        ],
    );
    let ix = load(&g.0).expect("test setup");
    let t = ix.get(&format!("root:{bug_id}")).expect("test setup");
    assert_eq!(
        t.edges.parent.as_deref(),
        Some(format!("root:{GOAL}").as_str())
    );
    let (errors, warnings) = codes(&g.0);
    assert!(errors.is_empty(), "{errors:?}");
    let count = |c| warnings.iter().filter(|w| **w == c).count();
    // Its own id, the parent edge and the body link.
    assert_eq!(count(Code::NodeStaleType), 3, "{warnings:?}");
    assert_eq!(count(Code::NodeDateOnly), 1);
    assert_eq!(count(Code::NodeLegacyExtensionKey), 1);
}

#[test]
fn one_uuid_in_two_files_is_a_duplicate_whatever_the_type() {
    let g = graph(
        "dup",
        &[
            (".mdkg/work/task-a.md", node(TASK, "task", "", "")),
            (
                ".mdkg/work/bug-a.md",
                node(&TASK.replacen("task", "bug", 1), "bug", "", ""),
            ),
        ],
    );
    assert_eq!(
        load(&g.0).expect_err("refused").code(),
        Code::NodeDuplicateId
    );
    assert!(codes(&g.0).0.contains(&Code::NodeDuplicateId));
}

#[test]
fn a_shared_legacy_id_is_an_error() {
    let other = "task-0192f3a4-0000-7000-8000-000000000003";
    let g = graph(
        "duplegacy",
        &[
            (
                ".mdkg/work/task-a.md",
                node(TASK, "task", "legacy_id: task-1\n", ""),
            ),
            (
                ".mdkg/work/task-b.md",
                node(other, "task", "legacy_id: task-1\n", ""),
            ),
        ],
    );
    assert_eq!(
        load(&g.0).expect_err("refused").code(),
        Code::NodeDuplicateLegacyId
    );
    assert!(codes(&g.0).0.contains(&Code::NodeDuplicateLegacyId));
}

#[test]
fn missing_targets_and_body_links_are_errors() {
    let missing = "goal-0192f3a4-0000-7000-8000-0000000000ff";
    let g = graph(
        "missing",
        &[(
            ".mdkg/work/task-a.md",
            node(
                TASK,
                "task",
                "",
                &format!("[x]({missing}) `[y]({missing})`\n"),
            ),
        )],
    );
    let (errors, _) = codes(&g.0);
    assert_eq!(errors, vec![Code::NodeBadBodyLink], "{errors:?}");
    let g = graph(
        "missing-edge",
        &[(
            ".mdkg/work/task-a.md",
            node(TASK, "task", &format!("relates: [{missing}]\n"), ""),
        )],
    );
    assert_eq!(
        load(&g.0).expect_err("refused").code(),
        Code::EdgeMissingTarget
    );
}

#[test]
fn legacy_forms_are_refused_in_a_v07_graph() {
    let g = graph(
        "legacyref",
        &[(
            ".mdkg/work/task-a.md",
            node(TASK, "task", "relates: [goal-4]\n", ""),
        )],
    );
    assert_eq!(load(&g.0).expect_err("refused").code(), Code::NodeBadRef);
    let g = graph(
        "legacyid",
        &[(".mdkg/work/task-a.md", node("task-1", "task", "", ""))],
    );
    assert_eq!(load(&g.0).expect_err("refused").code(), Code::NodeBadId);
}

// Review of d7dacec.

#[test]
fn a_legacy_graph_never_matches_uuid_shaped_portable_ids_by_uuid() {
    let u = "0192f3a4-0000-7000-8000-000000000002";
    let portable = |t: &str| {
        format!(
            "---\nid: {t}-{u}\ntype: {t}\ntitle: {t}\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n"
        )
    };
    let g = graph_with(
        "legacy-uuid",
        &format!(r#"{{"schema_version": 1, "workspaces": {{{ROOT_WS}}}}}"#),
        &[
            (".mdkg/work/spec-a.md", portable("spec")),
            (".mdkg/work/manifest-a.md", portable("manifest")),
        ],
    );
    let ix = load(&g.0).expect("loads");
    assert!(ix.graph_id.is_none());
    for t in ["spec", "manifest"] {
        assert_eq!(
            resolve_qid(&ix, &format!("{t}-{u}"), None),
            Resolution::Ok(format!("root:{t}-{u}"))
        );
    }
}

#[test]
fn refs_into_this_graph_resolve_and_refs_elsewhere_are_not_checked() {
    let own = format!("mdkg://{GRAPH}/{TASK}");
    let foreign = format!("mdkg://0190bbbb-0000-7000-8000-000000000000/{TASK}");
    let g = graph(
        "graph-refs",
        &[
            (
                ".mdkg/work/goal-a.md",
                node(
                    GOAL,
                    "goal",
                    &format!("scope_refs: [{own}, {foreign}]\nactive_node: {own}\n"),
                    "",
                ),
            ),
            (
                ".mdkg/work/task-a.md",
                node(TASK, "task", &format!("refs: [{foreign}]\n"), ""),
            ),
        ],
    );
    load(&g.0).expect("loads");
    let (errors, _) = codes(&g.0);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn active_node_resolves_across_workspaces_and_stale_types() {
    let stale = TASK.replacen("task", "bug", 1);
    let g = graph_with(
        "active",
        &format!(
            r#"{{{V07}, "workspaces": {{{ROOT_WS}, "sub": {{"path": "sub", "enabled": true, "mdkg_dir": ".mdkg"}}}}}}"#
        ),
        &[
            (
                ".mdkg/work/goal-a.md",
                node(
                    GOAL,
                    "goal",
                    &format!("scope_refs: [{TASK}]\nactive_node: {stale}\n"),
                    "",
                ),
            ),
            ("sub/.mdkg/work/task-a.md", node(TASK, "task", "", "")),
        ],
    );
    load(&g.0).expect("loads");
    let (errors, warnings) = codes(&g.0);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(warnings.contains(&Code::NodeStaleType), "{warnings:?}");
}

#[test]
fn a_goal_scope_skips_children_it_cant_hold_and_refuses_them_named() {
    let id = |t: &str, n: u32| format!("{t}-0192f3a4-0000-7000-8000-{n:012x}");
    let (epic, lp, feat) = (id("epic", 1), id("loop", 2), id("feat", 3));
    let under = |t: &str, n: u32, key: &str, up: &str| {
        (
            format!(".mdkg/work/{t}-{n}.md"),
            node(&id(t, n), t, &format!("{key}: {up}\n"), ""),
        )
    };
    let mut files: Vec<(String, String)> = vec![
        (".mdkg/work/epic.md".into(), node(&epic, "epic", "", "")),
        (".mdkg/work/loop.md".into(), node(&lp, "loop", "", "")),
        (".mdkg/work/feat.md".into(), node(&feat, "feat", "", "")),
        under("task", 4, "epic", &epic),
        // A checkpoint and a goal under each container, and a task under a
        // skipped checkpoint: none of them is in the scope.
        under("checkpoint", 10, "epic", &epic),
        under("checkpoint", 11, "parent", &lp),
        under("checkpoint", 12, "epic", &feat),
        under("goal", 20, "parent", &epic),
        under("goal", 21, "epic", &lp),
        under("goal", 22, "parent", &feat),
        under("task", 5, "parent", &id("checkpoint", 11)),
    ];
    let goal_file = |n: u32, refs: &str| {
        (
            format!(".mdkg/work/goal-{n}.md"),
            node(
                &id("goal", n),
                "goal",
                &format!("scope_refs: [{refs}]\n"),
                "",
            )
            .replace("goal_state: active", "goal_state: paused"),
        )
    };
    files.push(goal_file(30, &format!("{epic}, {lp}, {feat}")));
    let files: Vec<(&str, String)> = files.iter().map(|(p, t)| (p.as_str(), t.clone())).collect();
    let g = graph("scope-skips", &files);
    let index = load(&g.0).expect("loads");
    let goal = index
        .get(&format!("root:{}", id("goal", 30)))
        .expect("goal");
    let scope = mdkg_graph::graph_checks::goal_scope(&index, goal);
    let mut qids: Vec<&str> = scope.qids.iter().map(String::as_str).collect();
    qids.sort_unstable();
    let mut want = vec![
        format!("root:{epic}"),
        format!("root:{lp}"),
        format!("root:{feat}"),
        format!("root:{}", id("task", 4)),
    ];
    want.sort();
    assert_eq!(qids, want);
    assert!(scope.invalid_refs.is_empty(), "{:?}", scope.invalid_refs);
    assert_eq!(codes(&g.0).0, Vec::<Code>::new());
    // Named directly, a checkpoint or a goal is still the wrong type, once,
    // when it's below a scoped container too.
    for (n, named) in [
        (31, id("checkpoint", 10)),
        (32, id("goal", 21)),
        (33, format!("{epic}, {}", id("checkpoint", 10))),
    ] {
        let (path, text) = goal_file(n, &named);
        std::fs::write(g.0.join(&path), text).expect("test setup");
        let (errors, _) = codes(&g.0);
        assert_eq!(errors, vec![Code::GoalScopeRefWrongType], "{errors:?}");
        std::fs::remove_file(g.0.join(&path)).expect("test setup");
    }
}

#[test]
fn a_legacy_goal_scope_keeps_0_6_0s_walk() {
    // 0.6.0 refuses every descendant goals can't scope, so a legacy graph
    // does too: the 0.6.0 CLI must keep reading it.
    let legacy = |id: &str, t: &str, extra: &str| {
        format!(
            "---\nid: {id}\ntype: {t}\ntitle: {id}\nstatus: todo\n{extra}created: 2026-10-24\nupdated: 2026-10-24\n---\n"
        )
    };
    let g = graph_with(
        "legacy-scope",
        &format!(r#"{{"schema_version": 1, "workspaces": {{{ROOT_WS}}}}}"#),
        &[
            (".mdkg/work/epic-1.md", legacy("epic-1", "epic", "")),
            (
                ".mdkg/work/chk-1.md",
                legacy("chk-1", "checkpoint", "epic: epic-1\n"),
            ),
            (
                ".mdkg/work/goal-1.md",
                legacy(
                    "goal-1",
                    "goal",
                    "goal_state: paused\ngoal_condition: done\nscope_refs: [epic-1]\n",
                ),
            ),
        ],
    );
    let (errors, _) = codes(&g.0);
    assert_eq!(errors, vec![Code::GoalScopeRefWrongType], "{errors:?}");
}

#[test]
fn every_ref_key_must_name_a_node() {
    let missing = "goal-0192f3a4-0000-7000-8000-0000000000ff";
    let dec_missing = "dec-0192f3a4-0000-7000-8000-0000000000fe";
    let chk = "checkpoint-0192f3a4-0000-7000-8000-000000000007";
    for (name, id, t, extra) in [
        ("refs", TASK, "task", format!("refs: [{missing}]\n")),
        ("scope", chk, "checkpoint", format!("scope: [{missing}]\n")),
        (
            "context",
            TASK,
            "task",
            format!("context_refs: [mdkg://{GRAPH}/{missing}]\n"),
        ),
        (
            "pinned",
            TASK,
            "task",
            format!("evidence_refs: [{missing}@sha256:{}]\n", "a".repeat(64)),
        ),
    ] {
        let g = graph(name, &[(".mdkg/work/a.md", node(id, t, &extra, ""))]);
        assert_eq!(
            load(&g.0).expect_err(name).code(),
            Code::EdgeMissingTarget,
            "{name}"
        );
        assert!(codes(&g.0).0.contains(&Code::EdgeMissingTarget), "{name}");
    }
    let dec = format!(
        "---\nid: dec-0192f3a4-0000-7000-8000-000000000009\ntype: dec\ntitle: D\nstatus: accepted\nsupersedes: {dec_missing}\ncreated: {STAMP}\nupdated: {STAMP}\n---\n"
    );
    let g = graph("supersedes", &[(".mdkg/design/dec-a.md", dec)]);
    assert_eq!(
        load(&g.0).expect_err("supersedes").code(),
        Code::EdgeMissingTarget
    );
}

#[test]
fn a_missing_full_id_never_falls_through_to_a_legacy_id() {
    // A migrated node whose 0.6.0 portable id looked like a 0.7 id.
    let absent = "spec-0192f3a4-0000-7000-8000-0000000000aa";
    let g = graph(
        "fallthrough",
        &[(
            ".mdkg/work/task-a.md",
            node(TASK, "task", &format!("legacy_id: {absent}\n"), ""),
        )],
    );
    let ix = load(&g.0).expect("loads");
    assert_eq!(
        resolve_qid(&ix, absent, None),
        Resolution::Missing(Vec::new())
    );
    assert_eq!(
        resolve_qid(&ix, TASK, None),
        Resolution::Ok(format!("root:{TASK}"))
    );
}

#[test]
fn staleness_follows_the_type_field() {
    // The node's own id still says task, but it is a bug: refs saying bug
    // are current, refs saying task are stale.
    let bug = TASK.replacen("task", "bug", 1);
    let other = "task-0192f3a4-0000-7000-8000-000000000003";
    let g = graph(
        "type-field",
        &[
            (".mdkg/work/bug-a.md", node(TASK, "bug", "", "")),
            (
                ".mdkg/work/task-b.md",
                node(
                    other,
                    "task",
                    &format!("relates: [{bug}]\nblocks: [{TASK}]\n"),
                    "",
                ),
            ),
        ],
    );
    let config = config::load(&g.0).expect("config");
    let receipt = validate::validate(&g.0, &config).expect("validates");
    let stale: Vec<&str> = receipt
        .warnings
        .iter()
        .filter(|w| w.code == Code::NodeStaleType)
        .map(|w| w.message.as_str())
        .collect();
    assert_eq!(stale.len(), 2, "{stale:?}");
    assert!(
        stale.iter().any(|m| m.starts_with("id names type task")),
        "{stale:?}"
    );
    assert!(
        stale
            .iter()
            .any(|m| m.starts_with("blocks names") && m.ends_with(&format!("it is now {bug}"))),
        "{stale:?}"
    );
}

fn copy_tree(src: &Path, dest: &Path) {
    for entry in std::fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// `conformance/v07`: each case's codes are the spec's.
#[test]
fn conformance_v07_cases() {
    let suite = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    let v07_warnings = [
        Code::NodeStaleType,
        Code::NodeDateOnly,
        Code::NodeLegacyExtensionKey,
        Code::EdgeBlocksAncestor,
    ];
    let mut cases: Vec<PathBuf> = std::fs::read_dir(suite.join("v07/cases"))
        .expect("cases")
        .map(|e| e.expect("entry").path())
        .collect();
    cases.sort();
    assert!(cases.len() >= 10, "{cases:?}");
    for case in cases {
        let name = case
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let g = graph_with(
            &name,
            &std::fs::read_to_string(suite.join("v07/config.json")).expect("config"),
            &[],
        );
        copy_tree(
            &suite.join("base/.mdkg/templates"),
            &g.0.join(".mdkg/templates"),
        );
        copy_tree(&case.join(".mdkg"), &g.0.join(".mdkg"));
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(case.join("expected.json")).expect("expected"),
        )
        .expect("json");
        let config = config::load(&g.0).expect("config");
        let receipt = validate::validate(&g.0, &config).expect("validates");
        let mut errors: Vec<&str> = receipt.errors.iter().map(|d| d.code.as_str()).collect();
        let mut warnings: Vec<&str> = receipt
            .warnings
            .iter()
            .filter(|d| v07_warnings.contains(&d.code))
            .map(|d| d.code.as_str())
            .collect();
        errors.sort_unstable();
        warnings.sort_unstable();
        let list = |k: &str| -> Vec<&str> {
            expected[k]
                .as_array()
                .expect(k)
                .iter()
                .filter_map(|v| v.as_str())
                .collect()
        };
        assert_eq!(
            errors,
            list("errors"),
            "{name} errors: {:?}",
            receipt.errors
        );
        assert_eq!(warnings, list("warnings"), "{name} warnings");
    }
}

#[test]
fn a_minimal_v07_config_needs_no_workspaces_or_templates() {
    // What init writes (lane 6): no workspaces, no customization, no
    // templates folder.
    let g = graph_with(
        "minimal",
        &format!(
            r#"{{"schema_version": 3, "format_version": "0.7", "graph_id": "{GRAPH}", "work": {{"committed": "definitions"}}}}"#
        ),
        &[(".mdkg/work/task-a.md", node(TASK, "task", "", ""))],
    );
    let ix = load(&g.0).expect("loads");
    assert!(ix.get(&format!("root:{TASK}")).is_some());
    let (errors, warnings) = codes(&g.0);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!warnings.contains(&Code::TemplateFallback), "{warnings:?}");
}

#[test]
fn local_templates_dont_change_what_a_v07_node_may_hold() {
    // A local task template adding `colour`: 0.6.0's rule would accept the
    // key; format 0.7's node schema doesn't.
    let template = "---\nid: {{id}}\ntype: task\ntitle: {{title}}\nstatus: {{status}}\ncolour: {{colour}}\ncreated: {{created}}\nupdated: {{updated}}\n---\n";
    let g = graph(
        "local-template",
        &[
            (".mdkg/templates/default/task.md", template.to_string()),
            (
                ".mdkg/work/task-a.md",
                node(TASK, "task", "colour: blue\n", ""),
            ),
        ],
    );
    assert_eq!(
        load(&g.0).expect_err("refused").code(),
        Code::NodeUnknownKey
    );
}

/// `conformance/identity-v2`, copied to a scratch graph.
fn v2_graph() -> Graph {
    let g = graph_with("identity-v2", "{}", &[]);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/identity-v2");
    copy_tree(&fixture.join(".mdkg"), &g.0.join(".mdkg"));
    g
}

#[test]
fn identity_v2_graphs_read_only_for_migrate() {
    use mdkg_graph::identity_v2;
    let g = v2_graph();
    // Every other reader refuses it, and says to migrate.
    let refused = config::load(&g.0).expect_err("refused");
    assert_eq!(refused.code(), Code::ConfigInvalid);
    assert!(
        refused
            .diagnostic
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("migrate")),
        "{refused:?}"
    );
    assert!(identity_v2::is_identity_v2(&g.0));

    let v2 = identity_v2::read(&g.0).expect("reads");
    assert_eq!(
        v2.graph_id.to_string(),
        "6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b"
    );
    let ids: Vec<(String, String)> = v2
        .nodes
        .iter()
        .map(|n| (n.legacy_id.clone(), n.new_id().expect("id").to_string()))
        .collect();
    assert!(ids.contains(&(
        "task-1".into(),
        "task-1b2c3d4e-5f60-4718-9a2b-3c4d5e6f7a8b".into()
    )));
    assert!(ids.contains(&(
        "goal-1".into(),
        "goal-3a2b1c0d-9e8f-4a7b-b6c5-d4e3f2a1b0c9".into()
    )));
    // Refs: this graph's become node ids, another graph's keep their graph.
    let local = "mdkg://6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b/1b2c3d4e-5f60-4718-9a2b-3c4d5e6f7a8b";
    assert_eq!(
        v2.map_ref(local, |_, _| None).as_deref(),
        Some("task-1b2c3d4e-5f60-4718-9a2b-3c4d5e6f7a8b")
    );
    let foreign =
        "mdkg://7e8d9c0b-1a2f-4e3d-8c4b-5a6f7e8d9c0b/0a1b2c3d-4e5f-4a6b-9c7d-8e9f0a1b2c3d";
    assert_eq!(
        v2.map_ref(foreign, |_, _| Some("spec".into())).as_deref(),
        Some(
            "mdkg://7e8d9c0b-1a2f-4e3d-8c4b-5a6f7e8d9c0b/spec-0a1b2c3d-4e5f-4a6b-9c7d-8e9f0a1b2c3d"
        )
    );
    assert_eq!(v2.map_ref(foreign, |_, _| None), None);
    assert_eq!(v2.map_ref("task-1", |_, _| None), None);
    // The mapped ids are format 0.7 ids (version 4 UUIDs included).
    for (_, id) in &ids {
        mdkg_format::NodeId::parse(id).expect("a 0.7 id");
    }
}

#[test]
fn a_v2_node_without_its_identity_is_refused() {
    use mdkg_graph::identity_v2;
    let g = v2_graph();
    let path = g.0.join(".mdkg/work/task-2-finish.md");
    let text = std::fs::read_to_string(&path).expect("read");
    let without: String = text
        .lines()
        .filter(|l| !l.starts_with("node_id:"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&path, without).expect("write");
    let e = identity_v2::read(&g.0).expect_err("refused");
    assert_eq!(e.code(), Code::NodeBadIdentity, "{e:?}");
}

#[test]
fn concepts_lists_each_types_recommended_headings() {
    // docs/concepts.md's table is the templates' headings, which validate
    // recommends in a 0.7 graph; regenerate it with
    // `MDKG_WRITE_HEADINGS_DOC=1 cargo test -p mdkg-graph --test format07`.
    let g = graph("headings", &[]);
    let config = config::load(&g.0).expect("config");
    let mut table = String::from("| Type | Recommended headings |\n|---|---|\n");
    for t in [
        "task",
        "bug",
        "feat",
        "spike",
        "test",
        "epic",
        "goal",
        "loop",
        "checkpoint",
        "edd",
        "dec",
        "prd",
        "prop",
        "rule",
        "archive",
        "manifest",
        "spec",
        "work",
        "work_order",
        "receipt",
        "feedback",
        "dispute",
        "proposal",
    ] {
        let headings = validate::template_headings(&g.0, &config, t);
        assert!(!headings.is_empty(), "{t} has a template");
        table.push_str(&format!("| `{t}` | {} |\n", headings.join(", ")));
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/concepts.md");
    let doc = std::fs::read_to_string(&path).expect("concepts.md");
    let (start, end) = ("<!-- headings:start -->\n", "<!-- headings:end -->");
    let (Some(a), Some(b)) = (doc.find(start), doc.find(end)) else {
        panic!("concepts.md has no headings markers");
    };
    let want = format!("{}{start}{table}{}", &doc[..a], &doc[b..]);
    if std::env::var("MDKG_WRITE_HEADINGS_DOC").as_deref() == Ok("1") {
        std::fs::write(&path, want).expect("write concepts.md");
        return;
    }
    assert!(
        doc == want,
        "concepts.md's headings table is stale: regenerate it"
    );
}

#[test]
fn a_v07_node_is_warned_about_its_own_templates_headings() {
    // The graph's own template sets the recommended headings: a heading at
    // another level, in another case and order counts; one it lacks warns.
    let template = "---\nid: \"{{id}}\"\ntype: task\ntitle: \"{{title}}\"\nstatus: todo\ncreated: \"{{created}}\"\nupdated: \"{{updated}}\"\n---\n# Overview\n\n## Rollback\n";
    let g = graph(
        "own-template",
        &[
            (".mdkg/templates/default/task.md", template.to_string()),
            (
                ".mdkg/work/task-a.md",
                node(TASK, "task", "", "Text.\n\n#### OVERVIEW\n"),
            ),
        ],
    );
    let config = config::load(&g.0).expect("config");
    let receipt = validate::validate(&g.0, &config).expect("validates");
    let missing: Vec<&str> = receipt
        .warnings
        .iter()
        .filter(|d| d.code == Code::NodeHeadingMissing)
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(missing, ["missing recommended heading \"Rollback\""]);
}

#[test]
fn a_legacy_goals_required_skills_arent_checked() {
    // 0.6.0 never checked required_skills, so a legacy graph doesn't.
    let g = graph_with(
        "legacy-skills",
        &format!(r#"{{"schema_version": 1, "workspaces": {{{ROOT_WS}}}}}"#),
        &[(
            ".mdkg/work/goal-1.md",
            "---\nid: goal-1\ntype: goal\ntitle: G\nstatus: todo\ngoal_state: paused\ngoal_condition: done\nrequired_skills: [no-such-skill]\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n".to_string(),
        )],
    );
    let (errors, _) = codes(&g.0);
    assert!(errors.is_empty(), "{errors:?}");
}
