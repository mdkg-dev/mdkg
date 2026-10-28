use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use mdkg_coord_sqlite::git;
use mdkg_format::{Code, Digest, NodeId, node::FormatVersion};
use mdkg_graph::{ErrorKind, GraphError, Index, config, graph_checks, index, templates};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::coord::{Repo, git_error};
use crate::migrate::apply::inventory;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Move {
    pub id: String,
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub root: PathBuf,
    pub base: String,
    pub inventory: String,
    pub local_work: PathBuf,
    pub config_before: String,
    pub config_after: String,
    pub nodes: Vec<Move>,
    pub already_local: bool,
}

pub fn bad(message: &str) -> GraphError {
    GraphError::conflict(Code::UsageBadValue, message)
}

pub fn read(path: &Path, cap: u64) -> Result<Vec<u8>, GraphError> {
    use std::io::Read;
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| GraphError::failure(Code::IoReadFailed, e.to_string()))?;
    let f = fs::File::from(fd);
    if !f.metadata().is_ok_and(|m| m.is_file() && m.len() <= cap) {
        return Err(bad("localize needs bounded regular files, without links"));
    }
    let mut bytes = Vec::new();
    f.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| GraphError::failure(Code::IoReadFailed, e.to_string()))?;
    if bytes.len() as u64 > cap {
        return Err(bad("a localize input grew past its size limit"));
    }
    Ok(bytes)
}

/// Inspect the entire folder before the existing inventory/copy routines
/// read it. A FIFO, device or linked folder must never be followed.
pub fn plain_tree(dir: &Path) -> Result<(), GraphError> {
    let meta = fs::symlink_metadata(dir)
        .map_err(|e| GraphError::failure(Code::IoReadFailed, e.to_string()))?;
    if !meta.is_dir() {
        return Err(bad("localize needs a plain .mdkg folder"));
    }
    for entry in
        fs::read_dir(dir).map_err(|e| GraphError::failure(Code::IoReadFailed, e.to_string()))?
    {
        let entry = entry.map_err(|e| GraphError::failure(Code::IoReadFailed, e.to_string()))?;
        let kind = entry
            .file_type()
            .map_err(|e| GraphError::failure(Code::IoReadFailed, e.to_string()))?;
        if kind.is_dir() {
            plain_tree(&entry.path())?;
        } else if !kind.is_file() {
            return Err(bad("localize refuses links and special files in .mdkg"));
        }
    }
    Ok(())
}

fn has_markdown(dir: &Path) -> Result<bool, GraphError> {
    plain_tree(dir)?;
    for entry in fs::read_dir(dir).map_err(git_error)? {
        let path = entry.map_err(git_error)?.path();
        if (path.is_dir() && has_markdown(&path)?) || path.extension().is_some_and(|e| e == "md") {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn local_config(text: &str) -> Result<String, GraphError> {
    let mut value: Value = serde_json::from_str(text).map_err(|_| bad("the config isn't JSON"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| bad("the config isn't an object"))?;
    if object.get("work").is_none_or(Value::is_null) {
        object.insert("work".into(), serde_json::json!({}));
    }
    let work = value
        .get_mut("work")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| bad("the config needs a work object"))?;
    work.remove("committed");
    work.insert("location".into(), Value::String("local".into()));
    serde_json::to_string_pretty(&value)
        .map(|s| s + "\n")
        .map_err(|_| bad("can't encode the local config"))
}

pub fn preview(graph: &Index, local: &Path) -> Result<(), GraphError> {
    let mut nodes = graph.nodes.clone();
    for n in &mut nodes {
        if mdkg_graph::store_work::STORE_WORK_TYPES.contains(&n.node_type.as_str()) {
            n.path = local
                .join(format!("{}.md", n.id))
                .to_string_lossy()
                .into_owned();
        }
    }
    let mut after = Index::assemble_in(nodes, graph.workspaces.clone(), graph.graph_id);
    after.private_work = true;
    if let Some(d) = graph_checks::private_targets(&after).into_iter().next() {
        return Err(GraphError::new(ErrorKind::Validation, d)
            .with_hint("remove the committed node's reference to work, or point it at a committed container, then plan again"));
    }
    // Match validate's link grammar and UUID/graph resolution, including
    // stale type spellings. Work-to-work links remain local together.
    for node in after
        .nodes
        .iter()
        .filter(|n| !mdkg_graph::store_work::is_store_path(&n.path))
    {
        for link in mdkg_graph::validate::body_links(&node.body) {
            if let index::RefTarget::Local { node: target, .. } = after.target(link)
                && mdkg_graph::store_work::is_store_path(&target.path)
            {
                return Err(GraphError::new(
                    ErrorKind::Validation,
                    mdkg_format::Diagnostic::error(
                        Code::EdgePrivateTarget,
                        format!("{} has a body link to {}, a work node being moved out of git", node.id, target.id),
                    )
                    .at(node.path.clone(), None)
                    .for_node(node.qid.clone()),
                )
                .with_hint("remove the committed body link to work, or point it at a committed container, then plan again"));
            }
        }
    }
    Ok(())
}

pub fn validate(root: &Path) -> Result<(), GraphError> {
    let cfg = config::load(root)?;
    let receipt = mdkg_graph::validate::validate(root, &cfg)?;
    if let Some(error) = receipt.errors.into_iter().next() {
        return Err(GraphError::new(ErrorKind::Validation, error));
    }
    Ok(())
}

pub fn build(repo: &Repo) -> Result<Plan, GraphError> {
    let root = fs::canonicalize(&repo.root)
        .map_err(|e| GraphError::failure(Code::IoReadFailed, e.to_string()))?;
    let cfg = config::load(&root)?;
    if cfg.format != FormatVersion::V07 {
        return Err(bad("localize needs a format 0.7 graph; migrate it first"));
    }
    let enabled: Vec<_> = cfg.workspaces.iter().filter(|w| w.enabled).collect();
    if enabled.len() != 1
        || enabled.iter().any(|w| {
            w.alias != "root"
                || !Path::new(&w.path)
                    .components()
                    .all(|p| p == Component::CurDir)
                || w.mdkg_dir != ".mdkg"
        })
    {
        return Err(bad(
            "localize one root workspace at a time; disable imported workspaces first",
        ));
    }
    let graph_dir = root.join(".mdkg");
    plain_tree(&graph_dir)?;
    validate(&root)?;
    let before = inventory(&graph_dir).map_err(git_error)?;
    let config_before = String::from_utf8(read(&root.join(config::CONFIG_PATH), 4 << 20)?)
        .map_err(|_| bad("the config isn't UTF-8"))?;
    let local_work = mdkg_graph::store_work::work_dir(&root)
        .map_err(|e| GraphError::failure(Code::IoGitFailed, e))?
        .ok_or_else(|| bad("localize needs a git common directory"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let common = git::common_dir(&repo.top).map_err(git_error)?;
        if fs::metadata(&graph_dir).map_err(git_error)?.dev()
            != fs::metadata(common).map_err(git_error)?.dev()
        {
            return Err(bad(
                "localize requires .mdkg and the git common directory on one filesystem",
            ));
        }
    }
    let already_local = cfg.work_committed == config::WorkCommitted::None;
    if root.to_str().is_none() || local_work.to_str().is_none() {
        return Err(bad("localize requires UTF-8 graph and local-work paths"));
    }
    let graph = index::build(&root, &cfg)?;
    let mut nodes = Vec::new();
    let schemas = templates::load(&root, &cfg)?.schemas;
    let opts = mdkg_format::node::ParseOptions {
        work_status: &cfg.status_enum,
        priority_min: cfg.priority_min,
        priority_max: cfg.priority_max,
        templates: &schemas,
        allow_extension_keys: true,
        format: cfg.format,
    };
    if !already_local {
        preview(&graph, &local_work)?;
        // An earlier conversion in another checkout must arrive by merging
        // its commit, rather than combining two diverging sets of work.
        if local_work.is_dir() && has_markdown(&local_work)? {
            return Err(bad(
                "local work already exists; merge the checkout that localized it, then try again",
            ));
        }
        for n in &graph.nodes {
            if !mdkg_graph::store_work::STORE_WORK_TYPES.contains(&n.node_type.as_str()) {
                continue;
            }
            let content = String::from_utf8(read(&root.join(&n.path), cfg.limits.max_file_bytes)?)
                .map_err(|_| bad("a work node isn't UTF-8"))?;
            let parsed = mdkg_format::node::parse_node(&content, &n.path, &opts)
                .map_err(|e| GraphError::from_format(&e, &root))?;
            if index::IndexNode::from_parsed(parsed, &n.ws, n.path.clone()) != *n {
                return Err(bad("a work node changed while localize read it"));
            }
            nodes.push(Move {
                id: n.id.clone(),
                path: n.path.clone(),
                content,
            });
        }
    }
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    let prefix = format!("{}.mdkg", repo.prefix);
    let out = git::command(&repo.top)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            &format!(":(literal){prefix}"),
        ])
        .run(None)
        .map_err(git_error)?;
    if !out.status.success() || !out.stdout.is_empty() {
        return Err(bad(
            "commit or remove changes inside .mdkg before localizing",
        ));
    }
    let base = git::head(&repo.top).map_err(git_error)?.commit;
    for path in nodes
        .iter()
        .map(|n| n.path.as_str())
        .chain([config::CONFIG_PATH])
    {
        if !git::unchanged_since_head(&repo.top, repo.scope_for(path).as_str())
            .map_err(git_error)?
        {
            return Err(bad(
                "commit every work definition and the config before localizing",
            ));
        }
    }
    if inventory(&graph_dir).map_err(git_error)? != before {
        return Err(bad("the graph changed while localize planned it"));
    }
    let p = Plan {
        root,
        base,
        inventory: before,
        local_work,
        config_after: if already_local {
            config_before.clone()
        } else {
            local_config(&config_before)?
        },
        config_before,
        nodes,
        already_local,
    };
    p.check(repo)?;
    Ok(p)
}

impl Plan {
    pub fn hash(&self) -> String {
        Digest::of_bytes(&serde_json::to_vec(self).unwrap_or_default()).to_string()
    }

    /// Validate persisted plans before deriving paths from any of their data.
    pub fn check(&self, repo: &Repo) -> Result<(), GraphError> {
        let root = fs::canonicalize(&repo.root).map_err(git_error)?;
        if self.root != root
            || self.local_work
                != mdkg_graph::store_work::work_dir(&root)
                    .map_err(|_| bad("can't locate local work"))?
                    .ok_or_else(|| bad("not in git"))?
            || self.base.len() < 40
            || !self.base.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(bad(
                "the localize plan belongs to another graph or repository",
            ));
        }
        if !self.already_local && local_config(&self.config_before)? != self.config_after {
            return Err(bad("the localize plan changes more than work location"));
        }
        let mut ids = HashSet::new();
        let mut paths = HashSet::new();
        let cfg = config::load(&root)?;
        let schemas = templates::load(&root, &cfg)?.schemas;
        let opts = mdkg_format::node::ParseOptions {
            work_status: &cfg.status_enum,
            priority_min: cfg.priority_min,
            priority_max: cfg.priority_max,
            templates: &schemas,
            allow_extension_keys: true,
            format: cfg.format,
        };
        for n in &self.nodes {
            let id =
                NodeId::parse(&n.id).map_err(|_| bad("the localize plan has an invalid id"))?;
            if !mdkg_graph::store_work::STORE_WORK_TYPES.contains(&id.node_type())
                || !ids.insert(n.id.clone())
                || !paths.insert(n.path.to_ascii_lowercase())
                || !n.path.starts_with(".mdkg/")
                || !n.path.ends_with(".md")
                || !Path::new(&n.path)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_)))
            {
                return Err(bad(
                    "the localize plan has an unsafe or duplicate node path",
                ));
            }
            let parsed = mdkg_format::node::parse_node(&n.content, &n.path, &opts)
                .map_err(|e| GraphError::from_format(&e, &root))?;
            if parsed.id != n.id || parsed.node_type != id.node_type() {
                return Err(bad("a localize node doesn't match its planned identity"));
            }
        }
        Ok(())
    }
}
