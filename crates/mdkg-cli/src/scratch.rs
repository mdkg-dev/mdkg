//! Read-only discovery of the repository's shared local scratch paths.
//!
//! Returning a path neither creates its folder nor opens the coordinator.
//! Scratch is outside the graph, under the git common directory, so all
//! checkouts of a repository return the same path for the same actor.

use std::path::{Component, Path};

use clap::Subcommand;
use mdkg_coord_sqlite::git;
use mdkg_format::Code;
use mdkg_graph::GraphError;
use schemars::JsonSchema;
use serde::Serialize;

use crate::coord::{Repo, git_error};
use crate::output::{self, Report};

#[derive(Debug, Subcommand)]
pub enum ScratchCommand {
    /// Print the actor's local scratch path, without creating it.
    #[command(after_help = examples!("scratch path", "scratch path --shared --json"))]
    Path {
        /// The repository-wide shared folder instead of the actor's folder.
        #[arg(long)]
        shared: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

impl ScratchCommand {
    pub fn name(&self) -> &'static str {
        "scratch.path"
    }

    pub fn json(&self) -> bool {
        match self {
            Self::Path { json, .. } => *json,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PathData {
    /// Absolute path under `<git common dir>/mdkg/scratch/`. This query
    /// does not create the folder.
    pub path: String,
    /// True for the repository-wide `shared` folder.
    pub shared: bool,
    /// The actor whose folder was requested; absent with `--shared`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

fn under(common: &Path, actor: &str, shared: bool) -> Result<PathData, GraphError> {
    let actor = if shared {
        None
    } else {
        crate::msg::check_actor_mailbox(actor, true)?;
        // Mailbox names may be `.` or `..`; filesystem names may not.
        // A platform prefix or path separator is never an actor folder.
        let mut parts = Path::new(actor).components();
        if !matches!(parts.next(), Some(Component::Normal(name)) if name == actor)
            || parts.next().is_some()
        {
            return Err(GraphError::usage(
                Code::UsageBadValue,
                "the scratch actor must be a single folder name",
            ));
        }
        Some(actor.to_string())
    };
    let path = common
        .join("mdkg/scratch")
        .join(actor.as_deref().unwrap_or("shared"));
    let path = path
        .to_str()
        .ok_or_else(|| GraphError::failure(Code::IoReadFailed, "the scratch path is not UTF-8"))?;
    Ok(PathData {
        path: path.to_string(),
        shared,
        actor,
    })
}

fn path(root: &Path, actor: Option<String>, shared: bool) -> Result<Report<PathData>, GraphError> {
    let repo = Repo::open(root)?;
    let common = git::common_dir(&repo.top).map_err(git_error)?;
    let actor = repo.actor(actor);
    under(&common, &actor, shared).map(Report::ok)
}

pub fn run(root: &Path, actor: Option<String>, command: &ScratchCommand) -> u8 {
    match command {
        ScratchCommand::Path { shared, json } => output::emit(
            command.name(),
            *json,
            path(root, actor, *shared).unwrap_or_else(Report::failed),
            |d| println!("{}", output::shown_path(&d.path)),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_paths_are_repository_wide_and_actor_specific() {
        let common = Path::new("/repo/.git");
        let first = under(common, "agent-a", false).expect("actor path");
        let second = under(common, "agent-b", false).expect("actor path");
        let shared = under(common, "agent-a", true).expect("shared path");
        assert_eq!(first.path, "/repo/.git/mdkg/scratch/agent-a");
        assert_eq!(second.path, "/repo/.git/mdkg/scratch/agent-b");
        assert_eq!(shared.path, "/repo/.git/mdkg/scratch/shared");
        assert_eq!(first.actor.as_deref(), Some("agent-a"));
        assert!(shared.actor.is_none());
        assert!(!first.shared);
        assert!(shared.shared);
        assert_ne!(first.path, second.path);
    }

    #[test]
    fn scratch_actor_cannot_change_the_path_scope_or_echo_input() {
        for actor in [
            "", ".", "..", "../other", "/tmp", "a/b", "a\\b", "x\ny", "é",
        ] {
            assert!(
                under(Path::new("/repo/.git"), actor, false).is_err(),
                "{actor:?}"
            );
        }
        let marker = "PRIVATE_ACTOR/../escape";
        let error = under(Path::new("/repo/.git"), marker, false).expect_err("invalid");
        assert!(!error.diagnostic.message.contains("PRIVATE_ACTOR"));
        for actor in ["Agent_1", "agent:part", "a@host", "agent.test", "-agent"] {
            assert!(
                under(Path::new("/repo/.git"), actor, false).is_ok(),
                "{actor}"
            );
        }
        // Asking for the shared path does not depend on an actor name.
        assert!(under(Path::new("/repo/.git"), "../ignored", true).is_ok());
    }
}
