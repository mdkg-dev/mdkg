//! The decision send gate. No message part or project configuration is authority.

use mdkg_coord_sqlite::{git, mail::DecisionGrant};
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

pub(super) fn grant(
    repo: &Repo,
    sender: &str,
    action: Option<&str>,
    reference: Option<&str>,
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
    let reference = reference.ok_or_else(|| refused("decision requires --ref <committed-node>"))?;
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
        refused("decision sender has no user or machine-local delegation; project delegation is refused")
            .with_hint(grants.warnings.join("; "))
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
    let commit = git::commit_of(&repo.top, "HEAD")
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
    let actual = git::read_capped(&repo.top.join(&path), 1 << 20)
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
