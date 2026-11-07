//! CLI graph interpretation for advisory task receipts. All configuration and
//! documents come from bounded immutable Git objects, never the current checkout,
//! local settings or a coordinator/index refresh. Format 0.7 has fixed schemas;
//! legacy graphs need captured templates and are unavailable here. Active config
//! changes, malformed nodes or ambiguous identities refuse the phase. Snapshot
//! capture has a separate 30 s/16 MiB budget; pure adapter construction is capped
//! at 30 s and 1024 graph/workspace/document roots. Unrelated config blobs count
//! against capture capacity but their content is not interpreted as active policy.

use std::collections::BTreeSet;

use mdkg_coord::scope::Scope;
use mdkg_format::{
    NodeId,
    node::{FormatVersion, Node, ParseOptions, WORK_TYPES, parse_node},
    settings,
};
use mdkg_graph::config::{self, Config};
use mdkg_run::{
    RunError,
    state::Pinned,
    turn::receipt::{
        GitSpan,
        tasks::{self, Evidence, Metadata, Resolver, Task, snapshot::Snapshot},
    },
};

struct Graph {
    root: Scope,
    config: Config,
    /// Discovery uses enabled workspaces; coordinator keys use every folder.
    documents: Vec<String>,
    folders: Vec<Scope>,
}

struct Adapter {
    snapshot: Snapshot,
    graphs: Vec<Graph>,
    roots: Vec<String>,
}

fn join(parts: &[&str]) -> String {
    parts
        .iter()
        .flat_map(|p| p.split('/'))
        .filter(|p| !p.is_empty() && *p != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn below(path: &str, folder: &str) -> bool {
    path.strip_prefix(folder)
        .is_some_and(|rest| rest.starts_with('/'))
}

impl Graph {
    fn discovers(&self, path: &str) -> bool {
        path.ends_with(".md")
            && !path.ends_with("/core/core.md")
            && self.documents.iter().any(|folder| below(path, folder))
    }

    fn parse(&self, path: &str, bytes: &[u8]) -> Result<Node, String> {
        let options = ParseOptions {
            work_status: &self.config.status_enum,
            priority_min: self.config.priority_min,
            priority_max: self.config.priority_max,
            templates: mdkg_format::schema07::schemas(),
            allow_extension_keys: true,
            format: self.config.format,
        };
        let node = parse_node(
            std::str::from_utf8(bytes).map_err(|_| "node is not UTF-8")?,
            path,
            &options,
        )
        .map_err(|e| e.to_string())?;
        if node.identity.is_some() {
            return Err("legacy node identity is unsupported".into());
        }
        Ok(node)
    }
}

impl Adapter {
    fn new(snapshot: Snapshot, selected_root: &str) -> Result<Self, String> {
        let started = std::time::Instant::now();
        let selected = Scope::new(selected_root);
        if selected.as_str() != selected_root || selected_root.starts_with("..") {
            return Err("invalid selected graph root".into());
        }
        let policy_roots = snapshot.policy_roots();
        if policy_roots.len() > 1024 {
            return Err("too many captured graph roots".into());
        }
        let mut pending = BTreeSet::from([selected_root.to_string()]);
        let mut visited = BTreeSet::new();
        let mut workspace_count = 0usize;
        let mut graphs = Vec::new();
        let mut roots = BTreeSet::new();
        let mut graph_roots = BTreeSet::new();
        while let Some(root) = pending.pop_first() {
            if !visited.insert(root.clone()) {
                continue;
            }
            if started.elapsed() > std::time::Duration::from_secs(30) {
                return Err("configuration parsing time limit exceeded".into());
            }
            let bytes = snapshot.config(&root)?;
            let path = join(&[&root, config::CONFIG_PATH]);
            let root_scope = Scope::new(&root);
            if !graph_roots.insert(root_scope.clone()) {
                return Err("ambiguous graph roots".into());
            }
            let input = settings::Input::parse(
                settings::Origin::file(settings::Layer::Project, &path),
                bytes,
            )
            .map_err(|e| e.to_string())?;
            let config = config::from_snapshot(input.value).map_err(|e| e.to_string())?;
            if config.format != FormatVersion::V07 {
                return Err("legacy graph schemas are unavailable".into());
            }
            workspace_count = workspace_count.saturating_add(config.workspaces.len());
            if workspace_count > 1024 {
                return Err("too many captured workspaces".into());
            }
            let mut folders = BTreeSet::from([Scope::new(join(&[&root, ".mdkg"]))]);
            let mut documents = BTreeSet::new();
            for workspace in &config.workspaces {
                let folder = join(&[&root, &workspace.path, &workspace.mdkg_dir]);
                snapshot.directory(&folder)?;
                folders.insert(Scope::new(&folder));
                if workspace.enabled {
                    for section in ["core", "design", "work"] {
                        let dir = join(&[&folder, section]);
                        snapshot.directory(&dir)?;
                        documents.insert(dir.clone());
                        roots.insert(dir);
                    }
                }
            }
            for candidate in &policy_roots {
                let candidate_scope = Scope::new(candidate);
                if selected.contains(&candidate_scope)
                    && documents.iter().any(|d| {
                        let document = Scope::new(d);
                        document.overlaps(&candidate_scope)
                    })
                    && !visited.contains(candidate)
                {
                    pending.insert(candidate.clone());
                }
            }
            // A graph index would discover a node twice under overlapping
            // enabled document roots. Refuse that ambiguous layout.
            let dirs: Vec<_> = documents.iter().map(Scope::new).collect();
            for (i, dir) in dirs.iter().enumerate() {
                if dirs[i + 1..].iter().any(|other| dir.overlaps(other)) {
                    return Err("overlapping enabled document roots".into());
                }
            }
            graphs.push(Graph {
                root: root_scope,
                config,
                documents: documents.into_iter().collect(),
                folders: folders.into_iter().collect(),
            });
        }
        if roots.len() > 1024 {
            return Err("too many captured document roots".into());
        }
        // Detect case-folded aliases even on a case-sensitive checkout.
        let mut scopes = BTreeSet::new();
        for root in &roots {
            if !scopes.insert(Scope::new(root)) {
                return Err("ambiguous document roots".into());
            }
        }
        // Git pathspecs are literal and case-sensitive; coordinator scopes are
        // not. Never silently omit a differently spelled subtree (including a
        // path removed earlier in the span), or follow an obstructing ancestor.
        let spellings: Vec<_> = roots.iter().map(|r| (r, Scope::new(r))).collect();
        for path in snapshot.paths() {
            if started.elapsed() > std::time::Duration::from_secs(30) {
                return Err("configuration parsing time limit exceeded".into());
            }
            let path_scope = Scope::new(path);
            for (folder, scope) in &spellings {
                if path_scope.contains(scope) {
                    return Err("document root is obstructed by a tracked leaf".into());
                }
                if scope.contains(&path_scope) && !below(path, folder) {
                    return Err("document root spelling differs from the committed tree".into());
                }
            }
        }
        Ok(Self {
            snapshot,
            graphs,
            roots: roots.into_iter().collect(),
        })
    }

    fn identity_folder(&self, path: &str) -> Result<&Scope, String> {
        let scope = Scope::new(path);
        self.graphs
            .iter()
            .filter(|g| g.root.contains(&scope))
            .filter_map(|g| {
                g.folders
                    .iter()
                    .filter(|f| **f != scope && f.contains(&scope))
                    .max_by_key(|f| f.as_str().len())
                    .map(|f| (g, f))
            })
            .max_by_key(|(g, _)| g.root.as_str().len())
            .map(|(_, f)| f)
            .ok_or_else(|| "no canonical node folder".into())
    }
}

impl Resolver for Adapter {
    fn validate(&self, span: &GitSpan) -> Result<(), String> {
        self.snapshot.validate(span)
    }
    fn roots(&self) -> &[String] {
        &self.roots
    }
    fn candidate(&self, path: &str) -> bool {
        self.graphs.iter().any(|g| g.discovers(path))
    }
    fn resolve(&self, path: &str, bytes: &[u8]) -> Result<Option<Task>, String> {
        let graph = self
            .graphs
            .iter()
            .filter(|g| g.discovers(path))
            .max_by_key(|g| g.root.as_str().len())
            .ok_or("no discovering graph")?;
        let node = graph.parse(path, bytes)?;
        if !WORK_TYPES.contains(&node.node_type.as_str()) {
            return Ok(None);
        }
        let folder = self.identity_folder(path)?;
        Ok(Some(Task {
            key: format!(
                "{}#{}",
                folder.as_str().to_lowercase(),
                node.id.to_ascii_lowercase()
            ),
            done: node.status.as_deref() == Some("done"),
            metadata: Metadata {
                node_type: node.node_type,
                title: node.title,
            },
        }))
    }
    fn unique_identities(&self, path: &str, bytes: &[u8]) -> Result<Vec<String>, String> {
        // One document may be discovered by an outer graph and a nested one.
        // Validate under every applicable config and enforce each UUID domain.
        self.graphs
            .iter()
            .filter(|g| g.discovers(path))
            .map(|g| {
                let node = g.parse(path, bytes)?;
                let id = NodeId::parse(&node.id).map_err(|e| e.to_string())?;
                Ok(format!("{}#{}", g.root.as_str().to_lowercase(), id.uuid()))
            })
            .collect()
    }
}

pub(crate) fn collect(
    pin: &Pinned,
    span: &GitSpan,
    graph_root: &str,
) -> Result<Evidence, RunError> {
    let snapshot = Snapshot::capture(pin, span)?;
    let adapter = Adapter::new(snapshot, graph_root).map_err(|reason| {
        RunError::failure(
            mdkg_run::error::CODE_AGENT_FAILED,
            format!("receipt task evidence unavailable: {reason}"),
        )
    })?;
    tasks::collect(pin, span, &adapter)
}

pub(crate) fn callbacks() -> mdkg_run::mail::ReceiptCallbacks {
    mdkg_run::mail::ReceiptCallbacks {
        baseline: crate::msg::receipt_baseline,
        activity: crate::msg::receipt_activity,
        tasks: collect,
        deliver: crate::msg::deliver_receipt,
        fault: crate::msg::deliver_fault,
        clear: crate::msg::clear_faults,
        clears: crate::msg::fault_clears,
        sent: crate::msg::sent_to,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdkg_run::turn::receipt::harvest;
    use serde_json::{Value, json};
    use std::{fs, path::PathBuf, process::Command};

    const ID: &str = "task-0192f3a4-0000-7000-8000-000000000001";
    const FILE: &str = ".mdkg/work/any-name.md";
    struct Repo(PathBuf);
    impl Repo {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("mdkg-receipt-adapter-{}", uuid::Uuid::now_v7()));
            fs::create_dir(&dir).unwrap();
            let repo = Self(fs::canonicalize(dir).unwrap());
            repo.git(&["init", "-q", "-b", "main"]);
            repo
        }
        fn git(&self, args: &[&str]) -> String {
            let mut cmd = Command::new("git");
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("GIT_") {
                    cmd.env_remove(key);
                }
            }
            let out = cmd
                .current_dir(&self.0)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args([
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout)
                .unwrap()
                .trim_end()
                .to_string()
        }
        fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        fn commit(&self) -> String {
            self.git(&["add", "--all"]);
            self.git(&["commit", "--quiet", "--allow-empty", "-m", "fixture"]);
            self.git(&["rev-parse", "HEAD"])
        }
        fn pin(&self) -> Pinned {
            Pinned {
                top: self.0.clone(),
                git_dir: self.0.join(".git"),
            }
        }
        fn evidence(&self, start: &str) -> Result<Evidence, RunError> {
            collect(&self.pin(), &harvest(&self.pin(), Some(start))?, "")
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config() -> Value {
        json!({"schema_version":3,"format_version":"0.7","graph_id":"0192f3a4-0000-7000-8000-000000000099"})
    }
    fn node(status: &str) -> String {
        format!(
            "---\nid: {ID}\ntype: task\ntitle: Fixture\nstatus: {status}\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\nBody\n"
        )
    }
    fn setup() -> (Repo, String) {
        let r = Repo::new();
        r.write(config::CONFIG_PATH, config().to_string());
        r.write(FILE, node("todo"));
        let start = r.commit();
        r.write(FILE, node("done"));
        r.commit();
        (r, start)
    }

    #[test]
    fn every_work_type_is_collected_from_its_actual_type() {
        let r = Repo::new();
        r.write(config::CONFIG_PATH, config().to_string());
        let mut nodes = Vec::new();
        for (i, kind) in mdkg_format::node::WORK_TYPES.iter().enumerate() {
            // Deliberately retain a task prefix: a filename or ID hint is
            // not the node's actual type.
            let id = format!("task-0192f3a4-0000-7000-8000-{:012x}", i + 1);
            let mut text = node("todo")
                .replace(ID, &id)
                .replace("type: task", &format!("type: {kind}"))
                .replace("title: Fixture", &format!("title: Fixture {kind}"));
            if *kind == "goal" {
                text = text.replace(
                    "status: todo",
                    "status: todo\ngoal_state: active\ngoal_condition: fixture",
                );
            }
            let path = format!(".mdkg/work/{kind}.md");
            r.write(&path, &text);
            nodes.push((path, text, format!(".mdkg#{id}")));
        }
        let start = r.commit();
        for (path, text, _) in &nodes {
            r.write(path, text.replace("status: todo", "status: done"));
        }
        r.commit();
        let actual = r.evidence(&start).unwrap();
        let keys: BTreeSet<_> = nodes.iter().map(|(_, _, key)| key.clone()).collect();
        assert_eq!(actual.touched.into_iter().collect::<BTreeSet<_>>(), keys);
        assert_eq!(
            actual
                .completed
                .iter()
                .map(|c| c.node.clone())
                .collect::<BTreeSet<_>>(),
            keys
        );
        for (i, kind) in WORK_TYPES.iter().enumerate() {
            let key = &nodes[i].2;
            let completion = actual.completed.iter().find(|c| &c.node == key).unwrap();
            assert_eq!(actual.metadata[completion].node_type, *kind);
            assert_eq!(actual.metadata[completion].title, format!("Fixture {kind}"));
        }
    }

    #[test]
    fn retyped_ids_retain_distinct_coordinator_keys() {
        let r = Repo::new();
        r.write(config::CONFIG_PATH, config().to_string());
        r.write(FILE, node("done"));
        let start = r.commit();
        let id = ID.replacen("task-", "bug-", 1);
        r.write(
            FILE,
            node("done")
                .replace(ID, &id)
                .replace("type: task", "type: bug"),
        );
        r.commit();
        let actual = r.evidence(&start).unwrap();
        // The coordinator key contains the typed id. Preserve that contract;
        // this is an observed new done key, not a claim of new authorship.
        assert_eq!(
            actual.touched,
            [format!(".mdkg#{id}"), format!(".mdkg#{ID}")]
        );
        assert_eq!(actual.completed.len(), 1);
        assert_eq!(actual.completed[0].node, format!(".mdkg#{id}"));
        assert_eq!(actual.metadata[&actual.completed[0]].node_type, "bug");
    }

    #[test]
    fn committed_graph_ignores_current_checkout_config_and_node_bytes() {
        let (r, start) = setup();
        r.write(config::CONFIG_PATH, b"invalid current configuration");
        r.write(FILE, b"invalid current node");
        let e = r.evidence(&start).unwrap();
        assert_eq!(e.touched, [format!(".mdkg#{ID}")]);
        assert_eq!(e.completed.len(), 1);
        assert_eq!(e.metadata[&e.completed[0]].title, "Fixture");
        assert_eq!(
            e.completed[0].revision,
            mdkg_format::Digest::of_bytes(node("done").as_bytes()).to_string()
        );
    }

    #[test]
    fn adapter_workspace_and_document_capacity_boundaries_refuse_explicitly() {
        for (count, enabled, expected) in [
            (1024, false, None),
            (1025, false, Some("too many captured workspaces")),
            (341, true, None),
            (342, true, Some("too many captured document roots")),
        ] {
            let r = Repo::new();
            let mut c = config();
            let mut workspaces = serde_json::Map::from_iter([("root".into(), json!({"path":"."}))]);
            for i in 1..count {
                workspaces.insert(
                    format!("workspace_{i}"),
                    json!({"path":format!("area-{i}"),"enabled":enabled}),
                );
            }
            c["workspaces"] = Value::Object(workspaces);
            r.write(config::CONFIG_PATH, c.to_string());
            r.write(FILE, node("todo"));
            let start = r.commit();
            r.write(FILE, node("done"));
            r.commit();
            let span = harvest(&r.pin(), Some(&start)).unwrap();
            let snapshot = Snapshot::capture(&r.pin(), &span).unwrap();
            // Test construction itself: capture succeeds, and no later
            // collection can disguise refusal as an empty evidence result.
            match (Adapter::new(snapshot, ""), expected) {
                (Ok(adapter), None) => {
                    assert_eq!(adapter.graphs[0].config.workspaces.len(), count);
                    assert_eq!(adapter.roots.len(), if enabled { count * 3 } else { 3 });
                }
                (Err(reason), Some(expected)) => assert_eq!(reason, expected),
                (Err(reason), None) => panic!("boundary should fit: {reason}"),
                (Ok(_), Some(expected)) => panic!("expected refusal: {expected}"),
            }
        }
    }

    #[test]
    fn changed_then_restored_configuration_is_unavailable() {
        let (r, start) = setup();
        let mut changed = config();
        changed["workspaces"] = json!({});
        r.write(config::CONFIG_PATH, changed.to_string());
        r.commit();
        r.write(config::CONFIG_PATH, config().to_string());
        r.commit();
        assert!(
            r.evidence(&start)
                .unwrap_err()
                .message
                .contains("configuration changed")
        );
    }

    #[test]
    fn unrelated_legacy_fixture_graphs_and_their_changes_are_not_active_policy() {
        let r = Repo::new();
        r.write(config::CONFIG_PATH, config().to_string());
        r.write(FILE, node("todo"));
        r.write(
            "conformance/identity-v2/.mdkg/graph.json",
            b"legacy fixture",
        );
        r.write("conformance/legacy/.mdkg/config.json", b"{}");
        let start = r.commit();
        r.write(FILE, node("done"));
        r.write(
            "conformance/legacy/.mdkg/config.json",
            b"not even valid JSON",
        );
        r.commit();
        assert_eq!(
            r.evidence(&start).unwrap().completed[0].node,
            format!(".mdkg#{ID}")
        );
    }

    #[test]
    fn overlapping_enabled_workspace_discovery_is_unavailable() {
        let r = Repo::new();
        let mut c = config();
        c["workspaces"] =
            json!({"root":{"path":"."},"deep":{"path":".mdkg/work/deep","mdkg_dir":"nodes"}});
        r.write(config::CONFIG_PATH, c.to_string());
        let file = ".mdkg/work/deep/nodes/work/task.md";
        r.write(file, node("todo"));
        let start = r.commit();
        r.write(file, node("done"));
        r.commit();
        assert!(
            r.evidence(&start)
                .unwrap_err()
                .message
                .contains("overlapping enabled")
        );
    }

    #[test]
    fn case_folded_document_roots_cannot_silently_omit_current_or_removed_nodes() {
        for remove_before_end in [false, true] {
            let r = Repo::new();
            let mut c = config();
            c["workspaces"] = json!({"root":{"path":"."},"other":{"path":"Areas/Project"}});
            r.write(config::CONFIG_PATH, c.to_string());
            let path = "areas/project/.mdkg/work/task.md";
            r.write(path, node("todo"));
            let start = r.commit();
            if remove_before_end {
                fs::remove_file(r.0.join(path)).unwrap();
            } else {
                r.write(path, node("done"));
            }
            r.commit();
            assert!(
                r.evidence(&start)
                    .unwrap_err()
                    .message
                    .contains("spelling differs")
            );
        }
    }

    #[test]
    fn removed_nested_configuration_cannot_change_earlier_identity() {
        let r = Repo::new();
        let mut c = config();
        c["workspaces"] = json!({"root":{"path":"."},"nested":{"path":"nested"}});
        r.write(config::CONFIG_PATH, c.to_string());
        r.write("nested/.mdkg/config.json", config().to_string());
        r.write("nested/.mdkg/work/task.md", node("todo"));
        let start = r.commit();
        fs::remove_file(r.0.join("nested/.mdkg/config.json")).unwrap();
        r.write("nested/.mdkg/work/task.md", node("done"));
        r.commit();
        assert!(
            r.evidence(&start)
                .unwrap_err()
                .message
                .contains("configuration changed")
        );
    }

    #[test]
    fn custom_enabled_workspace_uses_coordinator_folder_and_ignores_other_markdown() {
        let r = Repo::new();
        let mut c = config();
        c["workspaces"] = json!({"root":{"path":"."},"project":{"path":"Areas/Project","mdkg_dir":"Graph"},"off":{"path":"off","enabled":false}});
        c["work"] = json!({"location":"local"});
        r.write(config::CONFIG_PATH, c.to_string());
        let file = "Areas/Project/Graph/work/not-an-id.md";
        r.write(file, node("todo"));
        for ignored in [
            "off/.mdkg/work/bad.md",
            "Areas/Project/Graph/core/core.md",
            "Areas/Project/Graph/notes/bad.md",
            "Areas/Project/Graph/work/UPPER.MD",
        ] {
            r.write(ignored, b"not a node");
        }
        let start = r.commit();
        r.write(file, node("done"));
        r.commit();
        let e = r.evidence(&start).unwrap();
        assert_eq!(e.completed[0].node, format!("areas/project/graph#{ID}"));
    }

    #[test]
    fn nested_graph_identity_uses_nearest_config_and_innermost_folder() {
        let r = Repo::new();
        let mut outer = config();
        outer["workspaces"] =
            json!({"root":{"path":"."},"nested":{"path":"nested","mdkg_dir":".mdkg"}});
        r.write(config::CONFIG_PATH, outer.to_string());
        let mut inner = config();
        inner["workspaces"] = json!({"root":{"path":"."},"deep":{"path":".mdkg/work/deep","mdkg_dir":"nodes","enabled":false}});
        r.write("nested/.mdkg/config.json", inner.to_string());
        let file = "nested/.mdkg/work/deep/nodes/work/file.md";
        r.write(file, node("todo"));
        let start = r.commit();
        r.write(file, node("done"));
        r.commit();
        assert_eq!(
            r.evidence(&start).unwrap().completed[0].node,
            format!("nested/.mdkg/work/deep/nodes#{ID}")
        );
    }

    #[test]
    fn duplicate_uuid_across_workspaces_and_types_refuses_all_task_evidence() {
        let r = Repo::new();
        let mut c = config();
        c["workspaces"] = json!({"root":{"path":"."},"other":{"path":"other"}});
        r.write(config::CONFIG_PATH, c.to_string());
        r.write(FILE, node("todo"));
        r.write(
            "other/.mdkg/work/collision.md",
            node("todo").replace("task", "bug"),
        );
        let start = r.commit();
        r.write(FILE, node("done"));
        r.commit();
        assert!(
            r.evidence(&start)
                .unwrap_err()
                .message
                .contains("graph-wide node identity")
        );
    }

    #[test]
    fn separate_graphs_may_use_the_same_node_uuid() {
        let r = Repo::new();
        for root in ["one", "two"] {
            r.write(&format!("{root}/.mdkg/config.json"), config().to_string());
            r.write(&format!("{root}/{FILE}"), node("todo"));
        }
        let start = r.commit();
        r.write(&format!("one/{FILE}"), node("done"));
        r.commit();
        assert_eq!(
            collect(&r.pin(), &harvest(&r.pin(), Some(&start)).unwrap(), "one")
                .unwrap()
                .completed[0]
                .node,
            format!("one/.mdkg#{ID}")
        );
    }

    #[test]
    fn snapshot_refuses_legacy_config_manifest_and_duplicate_json_keys() {
        for bytes in [
            "{}".to_owned(),
            config().to_string().replacen(
                "\"schema_version\":3",
                "\"schema_version\":3,\"schema_version\":3",
                1,
            ),
        ] {
            let r = Repo::new();
            r.write(config::CONFIG_PATH, bytes);
            r.write(FILE, node("todo"));
            let start = r.commit();
            r.write(FILE, node("done"));
            r.commit();
            assert!(r.evidence(&start).is_err());
        }
        let (r, start) = setup();
        r.write(".mdkg/graph.json", b"{}");
        r.commit();
        assert!(r.evidence(&start).is_err());
    }

    #[test]
    fn snapshot_refuses_linked_config_and_obstructed_workspace() {
        use std::os::unix::fs::symlink;
        for linked_config in [true, false] {
            let r = Repo::new();
            if linked_config {
                r.write("real-config", config().to_string());
                fs::create_dir(r.0.join(".mdkg")).unwrap();
                symlink("../real-config", r.0.join(config::CONFIG_PATH)).unwrap();
            } else {
                let mut c = config();
                c["workspaces"] = json!({"root":{"path":"."},"bad":{"path":"linked"}});
                r.write(config::CONFIG_PATH, c.to_string());
                symlink("outside", r.0.join("linked")).unwrap();
            }
            let start = r.commit();
            r.write("README", b"change");
            r.commit();
            let error = r.evidence(&start).unwrap_err();
            assert!(
                error.message.contains(if linked_config {
                    "not a regular blob"
                } else {
                    "obstructed"
                }),
                "{}",
                error.message
            );
        }
    }
}
