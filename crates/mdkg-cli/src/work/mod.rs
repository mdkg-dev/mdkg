//! Conversion of committed work definitions to the shared local folder.

mod apply;
mod plan;

use std::path::Path;

use clap::{ArgGroup, Args, Subcommand};
use mdkg_graph::GraphError;
use schemars::JsonSchema;
use serde::Serialize;

use crate::coord::Repo;
use crate::output::{self, Report};

#[derive(Debug, Subcommand)]
pub enum WorkCommand {
    /// Move work definitions beside the store, preserving their ids.
    #[command(after_help = examples!(
        "work localize --dry-run",
        "work localize --apply sha256:<plan-hash>"
    ))]
    Localize(LocalizeArgs),
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("mode").required(true).args(["dry_run", "apply"])))]
pub struct LocalizeArgs {
    /// Check references and print the conversion plan without writing.
    #[arg(long)]
    dry_run: bool,
    /// Apply the exact plan from --dry-run; repeat to finish an interruption.
    #[arg(long, value_name = "PLAN_HASH")]
    apply: Option<String>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

impl WorkCommand {
    pub fn name(&self) -> &'static str {
        "work.localize"
    }

    pub fn json(&self) -> bool {
        match self {
            Self::Localize(a) => a.json,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MoveRecord {
    pub id: String,
    /// The original graph-relative path.
    pub from: String,
    /// The absolute local path.
    pub to: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LocalizeData {
    pub plan: String,
    pub applied: bool,
    pub already_local: bool,
    pub local_work: String,
    pub nodes: Vec<MoveRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// A retained copy of the original .mdkg, outside git.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

fn data(
    p: &plan::Plan,
    applied: bool,
    commit: Option<String>,
    backup: Option<String>,
) -> LocalizeData {
    LocalizeData {
        plan: p.hash(),
        applied,
        already_local: p.already_local,
        local_work: p.local_work.to_string_lossy().into_owned(),
        nodes: p
            .nodes
            .iter()
            .map(|n| MoveRecord {
                id: n.id.clone(),
                from: n.path.clone(),
                to: p
                    .local_work
                    .join(format!("{}.md", n.id))
                    .to_string_lossy()
                    .into_owned(),
            })
            .collect(),
        commit,
        backup,
    }
}

pub fn run(root: &Path, actor: Option<String>, command: &WorkCommand) -> u8 {
    let report = (|| -> Result<Report<LocalizeData>, GraphError> {
        let repo = Repo::open(root)?;
        let WorkCommand::Localize(a) = command;
        match &a.apply {
            Some(hash) => apply::apply(&repo, actor, hash).map(Report::ok),
            None => plan::build(&repo).map(|p| Report::ok(data(&p, false, None, None))),
        }
    })()
    .unwrap_or_else(Report::failed);
    output::emit(command.name(), command.json(), report, |d| {
        if d.already_local {
            println!("work is already local");
        } else {
            println!(
                "{} {} work nodes",
                if d.applied {
                    "localized"
                } else {
                    "would localize"
                },
                d.nodes.len()
            );
            for n in &d.nodes {
                println!(
                    "{}",
                    output::shown_path(&format!("{}: {} -> {}", n.id, n.from, n.to))
                );
            }
        }
        println!("plan: {}", d.plan);
        if let Some(commit) = &d.commit {
            println!("commit: {commit}");
        }
        if let Some(backup) = &d.backup {
            println!("backup: {}", output::shown_path(backup));
        }
    })
}
