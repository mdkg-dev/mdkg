//! The decision send gate. No message part or project configuration is authority.

use crate::coord::graph_git::git;
use mdkg_coord_sqlite::mail::DecisionGrant;
use mdkg_format::{Code, Diagnostic, Digest, NodeId, NodeRef, settings::Environment};
use mdkg_graph::{
    GraphError, config, index,
    resolve::{Resolution, resolve_qid},
};
use mdkg_run::approval::Delegations;
use std::path::{Component, Path, PathBuf};

use crate::coord::Repo;

pub(super) fn refused(message: impl Into<String>) -> GraphError {
    GraphError::usage(Code::UsageInvalidArguments, message)
}

/// Where a person grants `sender` delegation, with the grant's shape
/// (docs/delegated-decisions.md), then any warnings from loading the
/// grants. Only the person's user settings file grants it: never the
/// project's, nor the machine-local file workers can write
/// (dec-01a18ee0 item 7).
fn delegation_hint(
    sender: &str,
    home: Option<&Path>,
    xdg: Option<&Path>,
    warnings: &[String],
) -> String {
    let user = match (xdg.filter(|x| !x.as_os_str().is_empty()), home) {
        (Some(x), _) => x.join("mdkg/config.json").display().to_string(),
        (None, Some(h)) => h.join(".config/mdkg/config.json").display().to_string(),
        (None, None) => "$HOME/.config/mdkg/config.json".into(),
    };
    let grant = serde_json::json!({
        "authority": {"delegates": {sender: {"for": "human:<name>", "may": ["decision"]}}}
    });
    let mut hint = format!(
        "a person grants it in their user config ({user}): {grant}; until then, relay the \
         answer as --kind resolution --reply-to <id>"
    );
    for w in warnings {
        hint.push_str("; ");
        hint.push_str(w);
    }
    hint
}

/// `replying`: the send has `--reply-to`, so a refusal for a missing
/// `--ref` can point at the answer that needs no node.
pub(super) fn grant(
    repo: &Repo,
    sender: &str,
    action: Option<&str>,
    reference: Option<&str>,
    replying: bool,
) -> Result<(DecisionGrant, Vec<Diagnostic>), GraphError> {
    let common =
        git::common_dir(&repo.top).map_err(|_| refused("cannot identify decision coordinator"))?;
    let cwd =
        std::env::current_dir().map_err(|_| refused("cannot identify decision sender worktree"))?;
    mdkg_run::approval::outside_turn(
        &common,
        sender,
        &[&repo.top, &cwd],
        std::env::var_os(mdkg_run::profile::TURN_MARK).is_some(),
    )
    .map_err(refused)?;
    let action = action.filter(|a| mdkg_coord_sqlite::mail::DECISION_ACTIONS.contains(a))
        .ok_or_else(|| refused("decision requires --action implement|test|commit|design|review; fixed-limit actions cannot be delegated"))?;
    let reference = reference.ok_or_else(|| {
        refused("decision requires --ref <committed-node>").with_hint(if replying {
            "a decision relays a person's approval of a committed node; to answer the \
             question without one, send --kind resolution --reply-to <id> instead, which \
             wakes the asker too"
        } else {
            "commit a node holding the decision and pass --ref <node>; an answer that \
             needs no node is --kind resolution --reply-to <question id>"
        })
    })?;
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(std::env::home_dir);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let grants = Delegations::load(
        &repo.root,
        Environment {
            home: home.as_deref(),
            xdg_config_home: xdg.as_deref(),
            git: None,
        },
    )
    .map_err(refused)?;
    let delegate = grants.delegates.get(sender).ok_or_else(|| {
        refused("decision sender has no delegation in the user settings file; project and machine-local delegations are ignored")
            .with_hint(delegation_hint(sender, home.as_deref(), xdg.as_deref(), &grants.warnings))
    })?;
    let config = config::load(&repo.root)?;
    let index = index::build(&repo.root, &config)?;
    let Resolution::Ok(qid) = resolve_qid(&index, reference, None) else {
        return Err(refused("decision --ref must resolve to one node"));
    };
    let node = index
        .get(&qid)
        .ok_or_else(|| refused("decision node is absent"))?;
    let id = NodeId::parse(&node.id)
        .map_err(|_| refused("delegated decisions require a format 0.7 node"))?;
    let graph = config
        .graph_id
        .ok_or_else(|| refused("delegated decisions require a format 0.7 graph"))?;
    let path = format!("{}{}", repo.prefix, node.path);
    if path.len() > 1024
        || !Path::new(&path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        || crate::coord::hidden_characters(&path)
    {
        return Err(refused(
            "decision node must have a safe repository-relative committed path",
        ));
    }
    // The node's commit: the graph tip the cache holds in ref mode, else
    // the checkout's HEAD.
    let commit = repo
        .graph_git()
        .head()
        .map(|h| h.commit)
        .map_err(|_| refused("decision node must be committed"))?;
    let graph_path = format!("{}.mdkg/config.json", repo.prefix);
    let graph_bytes =
        mdkg_run::approval::committed_node(&repo.top, &commit, &graph_path).map_err(refused)?;
    let graph_value: serde_json::Value = serde_json::from_slice(&graph_bytes)
        .map_err(|_| refused("committed graph config is invalid"))?;
    if graph_value["graph_id"].as_str() != Some(graph.to_string().as_str()) {
        return Err(refused(
            "committed graph identity differs from the decision graph",
        ));
    }
    let bytes = mdkg_run::approval::committed_node(&repo.top, &commit, &path).map_err(refused)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| refused("decision node is not UTF-8"))?;
    let parsed = mdkg_format::frontmatter::parse(text, &path)
        .map_err(|_| refused("committed decision node is invalid"))?;
    if parsed
        .frontmatter
        .get("id")
        .and_then(mdkg_format::frontmatter::Value::as_str)
        != Some(node.id.as_str())
    {
        return Err(refused("committed node identity differs from --ref"));
    }
    // Refuse uncommitted content rather than approve an older version without
    // the sender realizing it. Authority is bound to immutable bytes below.
    let actual = git::read_capped(&repo.disk_base(&path).join(&path), 1 << 20)
        .map_err(|_| refused("cannot read decision node"))?;
    if actual != bytes {
        return Err(refused("decision node has uncommitted changes"));
    }
    let digest = Digest::of_bytes(&bytes);
    if NodeRef::parse(reference)
        .ok()
        .and_then(|r| r.pin)
        .is_some_and(|pin| pin != digest.hex())
    {
        return Err(refused(
            "decision --ref content pin does not match the committed node",
        ));
    }
    let node_ref = NodeRef {
        graph: Some(graph),
        id,
        pin: Some(digest.hex().into()),
    }
    .to_string();
    Ok((
        DecisionGrant {
            human: delegate.human.clone(),
            source_file: delegate.source_file.to_string_lossy().into_owned(),
            action: action.into(),
            graph_config_path: graph_path,
            node_ref,
            node_commit: commit,
            node_path: path,
            node_digest: digest.to_string(),
        },
        grants
            .warnings
            .into_iter()
            .map(|w| Diagnostic::warning(Code::ConfigInvalid, w))
            .collect(),
    ))
}
