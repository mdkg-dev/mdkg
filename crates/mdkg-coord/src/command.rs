//! Every coordinator operation as a serializable command.
//!
//! A backend journals commands and rebuilds state by replaying them: the
//! coordinator is deterministic, so the same commands always give the same
//! state. The same commands are what a remote protocol would carry.

use serde::{Deserialize, Serialize};

use crate::lease::{Coordinator, Error, Lease, LeaseId, Mode, Ms, Request, Status};
use crate::scope::Scope;
use crate::settlement::{Changes, ChangesetId, Observation, ObserveReport, Source};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    Register {
        leaf: Scope,
        content: String,
    },
    Claim {
        owner: String,
        scope: Scope,
        /// Absent in journals written before lease modes: exclusive.
        #[serde(default)]
        mode: Mode,
        now: Ms,
        request: Option<Request>,
        /// The node the scope holds, when it's a node file: no other actor
        /// may hold that node under any path (a renamed copy included).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
    },
    Heartbeat {
        owner: String,
        lease: LeaseId,
        now: Ms,
    },
    Release {
        owner: String,
        lease: LeaseId,
        now: Ms,
    },
    Expire {
        lease: LeaseId,
        now: Ms,
    },
    ExpireDue {
        now: Ms,
    },
    Commit {
        owner: String,
        lease: LeaseId,
        changes: Changes,
        source: Option<Source>,
        now: Ms,
        /// The caller found the changes already on main and nothing of
        /// main's lost to them (see [`Coordinator::commit_landed`]). Left
        /// out when false, so earlier commands read and hash as they were.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        landed: bool,
    },
    Reconcile {
        lease: Box<Lease>,
        now: Ms,
    },
    ObserveMain {
        observation: Observation,
    },
    Abandon {
        changeset: ChangesetId,
    },
    PruneRequests {
        now: Ms,
    },
    IssueAlias {
        prefix: String,
        node: String,
        floor: u64,
    },
    SetWork {
        leaf: Scope,
        status: Option<String>,
        /// For a `done` mark: the changeset that recorded it.
        #[serde(default)]
        changeset: Option<ChangesetId>,
        actor: String,
        now: Ms,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    Done,
    Lease { lease: LeaseId },
    Deadline { deadline: Ms },
    Expired { leases: Vec<LeaseId> },
    Committed { changeset: ChangesetId },
    Reconciled { lease: LeaseId, status: Status },
    Observed { report: ObserveReport },
    Abandoned { changesets: Vec<ChangesetId> },
    Pruned { count: usize },
    Alias { alias: String },
    Refused { error: Error },
}

impl Outcome {
    pub fn error(&self) -> Option<&Error> {
        match self {
            Outcome::Refused { error } => Some(error),
            _ => None,
        }
    }
}

fn outcome<T>(r: Result<T, Error>, ok: impl FnOnce(T) -> Outcome) -> Outcome {
    match r {
        Ok(v) => ok(v),
        Err(error) => Outcome::Refused { error },
    }
}

impl Coordinator {
    /// Runs one command. Refusals are outcomes, not panics, so a journal can
    /// record and replay them like any other result.
    pub fn apply(&mut self, command: &Command) -> Outcome {
        match command {
            Command::Register { leaf, content } => {
                outcome(self.register(leaf.clone(), content.clone()), |()| {
                    Outcome::Done
                })
            }
            Command::Claim {
                owner,
                scope,
                mode,
                now,
                request,
                node,
            } => outcome(
                self.claim_node(owner, scope, *mode, node.as_deref(), *now, request.clone()),
                |lease| Outcome::Lease { lease },
            ),
            Command::Heartbeat { owner, lease, now } => {
                outcome(self.heartbeat(owner, lease, *now), |deadline| {
                    Outcome::Deadline { deadline }
                })
            }
            Command::Release { owner, lease, now } => {
                outcome(self.release(owner, lease, *now), |()| Outcome::Done)
            }
            Command::Expire { lease, now } => outcome(self.expire(lease, *now), |()| Outcome::Done),
            Command::ExpireDue { now } => Outcome::Expired {
                leases: self.expire_due(*now),
            },
            Command::Commit {
                owner,
                lease,
                changes,
                source,
                now,
                landed,
            } => outcome(
                self.commit_landed(owner, lease, changes, source.clone(), *now, *landed),
                |changeset| Outcome::Committed { changeset },
            ),
            Command::Reconcile { lease, now } => outcome(
                self.reconcile((**lease).clone(), *now),
                |(lease, status)| Outcome::Reconciled { lease, status },
            ),
            Command::ObserveMain { observation } => {
                outcome(self.observe_main(observation), |report| Outcome::Observed {
                    report,
                })
            }
            Command::Abandon { changeset } => outcome(self.abandon(*changeset), |changesets| {
                Outcome::Abandoned { changesets }
            }),
            Command::PruneRequests { now } => Outcome::Pruned {
                count: self.prune_requests(*now),
            },
            Command::IssueAlias {
                prefix,
                node,
                floor,
            } => Outcome::Alias {
                alias: self.issue_alias(prefix, node, *floor),
            },
            Command::SetWork {
                leaf,
                status,
                changeset,
                actor,
                now,
            } => outcome(
                self.set_work(leaf, status.clone(), *changeset, actor, *now),
                |()| Outcome::Done,
            ),
        }
    }
}
