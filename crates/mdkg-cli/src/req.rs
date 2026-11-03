//! `mdkg req list`: the requirements in a node and their ledger states.

use std::collections::HashMap;
use std::path::Path;

use clap::{Args, Subcommand};
use mdkg_coord_sqlite::entries::RequirementStateReader;
use mdkg_coord_sqlite::ledger::NodeLink;
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::requirements::{self, EarsShape, RelationKind, Requirement};
use mdkg_graph::resolve::Resolution;
use mdkg_graph::{GraphError, IndexNode, config, index};
use mdkg_index::resolve::{display_id, handle_error, resolve_handle, shadowed};
use schemars::JsonSchema;
use serde::Serialize;

use crate::coord::Repo;
use crate::output::{self, Report};

#[derive(Debug, Subcommand)]
pub enum ReqCommand {
    /// List requirements in a node body, with their ledger states.
    List(ListArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// A node id, qualified id, unique prefix, or title fragment.
    pub node: String,
    /// Print one JSON envelope.
    #[arg(long)]
    pub json: bool,
}

impl ReqCommand {
    pub fn name(&self) -> &'static str {
        "req.list"
    }

    pub fn json(&self) -> bool {
        match self {
            Self::List(args) => args.json,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RelationOut {
    pub kind: &'static str,
    pub node: String,
    pub requirement: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RequirementOut {
    pub id: String,
    pub relations: Vec<RelationOut>,
    pub statement: String,
    pub verify: Option<String>,
    /// One-based line within the node body.
    pub line: usize,
    pub retired: bool,
    pub ears: Option<&'static str>,
    /// Null for a retired item or when the coordinator cannot be read.
    pub state: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListData {
    pub node: String,
    pub requirements: Vec<RequirementOut>,
}

fn shape(shape: EarsShape) -> &'static str {
    match shape {
        EarsShape::Ubiquitous => "ubiquitous",
        EarsShape::EventDriven => "event_driven",
        EarsShape::StateDriven => "state_driven",
        EarsShape::Unwanted => "unwanted",
        EarsShape::OptionalFeature => "optional_feature",
        EarsShape::Complex => "complex",
    }
}

/// Format parsed text without claiming that unavailable state is unverified.
pub(crate) fn format_requirements(
    requirements: Vec<Requirement>,
    states: Option<&HashMap<String, String>>,
) -> Vec<RequirementOut> {
    requirements
        .into_iter()
        .map(|r| {
            let state = if r.retired {
                None
            } else {
                states.map(|s| s.get(&r.id).cloned().unwrap_or_else(|| "unverified".into()))
            };
            RequirementOut {
                id: r.id,
                relations: r
                    .relations
                    .into_iter()
                    .map(|rel| RelationOut {
                        kind: match rel.kind {
                            RelationKind::Adds => "adds",
                            RelationKind::Changes => "changes",
                            RelationKind::Removes => "removes",
                        },
                        node: rel.node,
                        requirement: rel.requirement,
                    })
                    .collect(),
                statement: r.statement,
                verify: r.verify,
                line: r.line,
                retired: r.retired,
                ears: r.ears.map(shape),
                state,
            }
        })
        .collect()
}

/// The coordinator's requirement states, opened once for every node a
/// command reports, through the entry ledger's read-only reader (it never
/// creates or upgrades a store). An unavailable coordinator means state is
/// unknown, not that every requirement is unverified.
pub(crate) struct Ledger {
    repo: Option<Repo>,
    reader: Result<RequirementStateReader, Diagnostic>,
}

impl Ledger {
    pub(crate) fn open(root: &Path) -> Self {
        Self {
            repo: Repo::open(root).ok(),
            reader: crate::entry::requirement_reader(root),
        }
    }

    /// Each requirement id's state, or why it is unknown. A node without a
    /// 0.7 id has no ledger, so nothing is recorded for it. Entries are
    /// keyed by the node's folder as its lease key spells it.
    // The reader's interface returns a `Diagnostic` as its error.
    #[allow(clippy::result_large_err)]
    pub(crate) fn states(&self, node: &IndexNode) -> Result<HashMap<String, String>, Diagnostic> {
        let reader = self.reader.as_ref().map_err(Clone::clone)?;
        if NodeLink::of(&node.id).is_err() {
            return Ok(HashMap::new());
        }
        let repo = self
            .repo
            .as_ref()
            .ok_or_else(|| unknown_states("the repository can't be opened"))?;
        let leaf = repo.leaf(&node.path).map_err(|e| *e.diagnostic)?;
        let key = repo.node_key(&leaf, &node.qid);
        // A folder may hold `#`; an id never does (a-flow's #6919).
        let folder = key.rsplit_once('#').map_or(key.as_str(), |(f, _)| f);
        reader.states(folder, &node.id)
    }
}

/// The warning for requirement states the coordinator couldn't give.
pub(crate) fn unknown_states(why: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::warning(
        Code::CoordStoreFailed,
        format!("requirement states are unknown: {why}"),
    )
}

/// `states` as a read-only command reports it: the reason becomes a
/// warning and the states unknown.
pub(crate) fn states_or_warn(
    ledger: &Ledger,
    node: &IndexNode,
    warnings: &mut Vec<Diagnostic>,
) -> Option<HashMap<String, String>> {
    match ledger.states(node) {
        Ok(states) => Some(states),
        Err(d) => {
            // The reader's own failure already says the states are unknown.
            let warning = if d.code == Code::CoordStoreFailed {
                Diagnostic::warning(d.code, d.message)
            } else {
                unknown_states(&d.message)
            };
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
            None
        }
    }
}

fn list(root: &Path, args: &ListArgs) -> Result<Report<ListData>, GraphError> {
    let config = config::load(root)?;
    let graph = index::build(root, &config)?;
    let resolved = resolve_handle(&graph, &args.node, None);
    let mut warnings: Vec<Diagnostic> = shadowed(&graph, &args.node, None, &resolved)
        .into_iter()
        .collect();
    let qid = match resolved {
        Resolution::Ok(qid) => qid,
        other => return Err(handle_error(&graph, "id", &args.node, &other, None)),
    };
    let node = graph.get(&qid).ok_or_else(|| {
        GraphError::not_found(
            Code::NodeNotFound,
            "the resolved node is no longer in the graph",
        )
    })?;
    let states = states_or_warn(&Ledger::open(root), node, &mut warnings);
    let requirements = format_requirements(
        requirements::parse(&node.body).requirements,
        states.as_ref(),
    );
    Ok(Report::ok(ListData {
        node: display_id(&graph, &qid),
        requirements,
    })
    .with(warnings))
}

pub fn run(root: &Path, command: &ReqCommand) -> u8 {
    match command {
        ReqCommand::List(args) => {
            let report = list(root, args).unwrap_or_else(Report::failed);
            output::emit("req.list", args.json, report, |data| {
                for r in &data.requirements {
                    let state = if r.retired {
                        "retired"
                    } else {
                        r.state.as_deref().unwrap_or("unknown")
                    };
                    println!(
                        "{} ({}): {}",
                        r.id,
                        output::shown_body(state),
                        output::shown_body(&r.statement)
                    );
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_unverified_and_retired_states_stay_distinct() {
        let text =
            "# Requirements\n- R1: The service shall respond.\n- R2 (retired): No longer used.\n";
        let parsed = requirements::parse(text).requirements;
        let unknown = format_requirements(parsed.clone(), None);
        assert_eq!(unknown[0].state, None);
        assert_eq!(unknown[1].state, None);
        assert!(unknown[1].retired);

        let mut states = HashMap::new();
        let unverified = format_requirements(parsed.clone(), Some(&states));
        assert_eq!(unverified[0].state.as_deref(), Some("unverified"));
        assert_eq!(unverified[1].state, None);
        states.insert("R1".to_string(), "met".to_string());
        states.insert("R2".to_string(), "met".to_string());
        let met = format_requirements(parsed, Some(&states));
        assert_eq!(met[0].state.as_deref(), Some("met"));
        assert_eq!(met[1].state, None);
    }
}
