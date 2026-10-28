//! Temporary work states and readable aliases, separate from node files.
//!
//! Lasting statuses (backlog, todo, done) live in node files. Temporary ones
//! live here, so they never become commits: `progress` is implied by an
//! active lease, and `review` or `blocked` are recorded until cleared.
//!
//! `done` leaves a mark tied to the changeset that recorded it, so other
//! worktrees see the item as done before that changeset merges. The mark
//! counts while the changeset is sequenced or conflicted (finished work
//! waiting for a merge), and once it's merged, until main's copy of the node
//! changes again, so a later edit on main wins. An abandoned changeset drops
//! it. Marks are kept apart from `review` and `blocked`,
//! so clearing those leaves the mark.

use serde::{Deserialize, Serialize};

use crate::lease::{Coordinator, Error, Ms, Status};
use crate::scope::Scope;
use crate::settlement::{ChangesetId, Settlement};

/// A temporary state recorded for one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkStatus {
    pub status: String,
    pub actor: String,
    pub at: Ms,
    /// For a `done` mark: the changeset that recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changeset: Option<ChangesetId>,
}

/// A `done` mark: the changeset that recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneMark {
    pub changeset: ChangesetId,
    pub actor: String,
    pub at: Ms,
}

/// `<prefix>-<n>` at the start of a node file name.
fn alias_number(leaf: &Scope, prefix: &str) -> Option<u64> {
    let name = leaf.as_str().rsplit('/').next()?;
    let rest = name.strip_prefix(prefix)?.strip_prefix('-')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = &rest[digits.len()..];
    if digits.is_empty() || !(after.is_empty() || after.starts_with('-') || after.starts_with('.'))
    {
        return None;
    }
    digits.parse().ok()
}

impl Coordinator {
    /// Issues the next readable alias for `prefix` to `node`. It is above
    /// `floor` (the highest number the caller's worktree uses), every alias
    /// issued before, and every number in a file name the coordinator knows,
    /// so parallel worktrees never collide.
    pub fn issue_alias(&mut self, prefix: &str, node: &str, floor: u64) -> String {
        let known = self
            .leaves
            .keys()
            .filter_map(|leaf| alias_number(leaf, prefix))
            .max()
            .unwrap_or(0);
        self.aliases.issue_after(prefix, node, floor.max(known))
    }

    /// The alias module, for resolving issued aliases.
    pub fn aliases(&self) -> &crate::aliases::Aliases {
        &self.aliases
    }

    /// Records (or with `None`, clears) a temporary state for `leaf`.
    /// Refused while another actor holds a lease covering it.
    pub fn set_work(
        &mut self,
        leaf: &Scope,
        status: Option<String>,
        changeset: Option<ChangesetId>,
        actor: &str,
        now: Ms,
    ) -> Result<(), Error> {
        let held_by_other = self.leases.values().any(|l| {
            l.confirmed && l.status == Status::Active && l.owner != actor && l.scope.contains(leaf)
        });
        if held_by_other {
            return Err(Error::NotOwner);
        }
        match (status, changeset) {
            (Some(status), Some(changeset)) if status == "done" => {
                self.done.insert(
                    leaf.clone(),
                    DoneMark {
                        changeset,
                        actor: actor.to_string(),
                        at: now,
                    },
                );
                // Finishing ends any review or blocked state (and drops a
                // mark written before marks were kept apart).
                self.work.remove(leaf);
            }
            (Some(status), _) => {
                self.work.insert(
                    leaf.clone(),
                    WorkStatus {
                        status,
                        actor: actor.to_string(),
                        at: now,
                        changeset: None,
                    },
                );
            }
            (None, _) => {
                self.work.remove(leaf);
            }
        }
        Ok(())
    }

    /// Moves `done` marks written before marks were kept apart (as a `done`
    /// work state) into the marks, so `update` and `--clear` can't lose
    /// them. Stores call this when they load a snapshot.
    pub fn upgrade_done_marks(&mut self) {
        let legacy: Vec<Scope> = self
            .work
            .iter()
            .filter(|(_, w)| w.status == "done" && w.changeset.is_some())
            .map(|(leaf, _)| leaf.clone())
            .collect();
        for leaf in legacy {
            if let Some(w) = self.work.remove(&leaf)
                && let Some(changeset) = w.changeset
            {
                self.done.entry(leaf).or_insert(DoneMark {
                    changeset,
                    actor: w.actor,
                    at: w.at,
                });
            }
        }
    }

    /// The temporary state recorded for `leaf` (`review` or `blocked`).
    pub fn work(&self, leaf: &Scope) -> Option<&WorkStatus> {
        self.work.get(leaf).filter(|w| w.status != "done")
    }

    /// The changeset of `leaf`'s `done` mark, if the mark still counts: while
    /// the changeset is sequenced or conflicted, and once committed, while
    /// main's copy of the leaf is still that changeset's version.
    pub fn done_mark(&self, leaf: &Scope) -> Option<ChangesetId> {
        let id = self.done.get(leaf).map(|m| m.changeset).or_else(|| {
            self.work
                .get(leaf)
                .filter(|w| w.status == "done")
                .and_then(|w| w.changeset)
        })?;
        let counts = match self.changeset(id)?.state {
            Settlement::Sequenced | Settlement::Conflicted => true,
            Settlement::Committed => self.leaf(leaf).is_some_and(|l| {
                l.entries
                    .iter()
                    .rev()
                    .find(|e| e.state == crate::settlement::EntryState::Committed)
                    .is_some_and(|e| e.origin == crate::settlement::Origin::Changeset(id))
            }),
            Settlement::Abandoned => false,
        };
        counts.then_some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_numbers_come_from_file_names() {
        let n = |p: &str| alias_number(&Scope::new(p), "task");
        assert_eq!(n(".mdkg/work/task-12-some-title.md"), Some(12));
        assert_eq!(n(".mdkg/work/task-7.md"), Some(7));
        assert_eq!(n(".mdkg/work/taskforce-3-x.md"), None);
        assert_eq!(n(".mdkg/work/task-p3f9a-x.md"), None);
        assert_eq!(n(".mdkg/work/task-12x.md"), None);
    }

    #[test]
    fn aliases_skip_numbers_in_use_anywhere() {
        let mut c = Coordinator::new(10);
        c.register(Scope::new(".mdkg/work/task-4-a.md"), "a")
            .unwrap();
        assert_eq!(c.issue_alias("task", ".mdkg/work/new-1", 2), "task-5");
        assert_eq!(c.issue_alias("task", ".mdkg/work/new-2", 9), "task-10");
        assert_eq!(c.issue_alias("task", ".mdkg/work/new-3", 0), "task-11");
        assert_eq!(c.issue_alias("chk", ".mdkg/work/new-4", 0), "chk-1");
    }

    #[test]
    fn work_states_belong_to_the_lease_holder() {
        let mut c = Coordinator::new(10);
        let leaf = Scope::new(".mdkg/work/task-1-a.md");
        c.register(leaf.clone(), "a").unwrap();
        c.claim("x", &leaf, 0, None).unwrap();
        assert_eq!(
            c.set_work(&leaf, Some("review".into()), None, "y", 1),
            Err(Error::NotOwner)
        );
        c.set_work(&leaf, Some("review".into()), None, "x", 1)
            .unwrap();
        assert_eq!(c.work(&leaf).map(|w| w.status.as_str()), Some("review"));
        c.set_work(&leaf, None, None, "x", 2).unwrap();
        assert!(c.work(&leaf).is_none());
    }

    fn observe(c: &mut Coordinator, leaf: &Scope, content: &str) {
        let obs = crate::settlement::Observation {
            read_generation: c.generation(),
            leaves: std::collections::BTreeMap::from([(leaf.clone(), content.to_string())]),
            ..Default::default()
        };
        c.observe_main(&obs).unwrap();
    }

    fn done(c: &mut Coordinator, leaf: &Scope, content: &str, now: Ms) -> ChangesetId {
        let l = c.claim("x", leaf, now, None).unwrap();
        let changes = std::collections::BTreeMap::from([(leaf.clone(), Some(content.to_string()))]);
        let cs = c.commit("x", &l, &changes, None, now + 1).unwrap();
        c.set_work(leaf, Some("done".into()), Some(cs), "x", now + 2)
            .unwrap();
        cs
    }

    #[test]
    fn a_done_mark_lasts_until_main_moves_on() {
        let leaf = Scope::new(".mdkg/work/task-1-a.md");

        // Sequenced: counts, and clearing review or blocked keeps it.
        let mut c = Coordinator::new(100);
        c.register(leaf.clone(), "todo").unwrap();
        let cs = done(&mut c, &leaf, "done", 0);
        assert_eq!(c.done_mark(&leaf), Some(cs));
        c.set_work(&leaf, Some("review".into()), None, "x", 5)
            .unwrap();
        c.set_work(&leaf, None, None, "x", 6).unwrap();
        assert_eq!(c.done_mark(&leaf), Some(cs), "--clear keeps the mark");

        // Conflicted (main edited the node meanwhile): still finished work.
        observe(&mut c, &leaf, "todo, with notes");
        assert_eq!(c.done_mark(&leaf), Some(cs), "conflicted work stays done");

        // Merged: counts until main's copy changes again.
        let mut c = Coordinator::new(100);
        c.register(leaf.clone(), "todo").unwrap();
        let cs = done(&mut c, &leaf, "done", 0);
        observe(&mut c, &leaf, "done");
        assert_eq!(c.done_mark(&leaf), Some(cs));
        observe(&mut c, &leaf, "reopened");
        assert_eq!(c.done_mark(&leaf), None, "a later edit on main wins");

        // Abandoned: gone.
        let mut c = Coordinator::new(100);
        c.register(leaf.clone(), "todo").unwrap();
        let cs = done(&mut c, &leaf, "done", 0);
        c.abandon(cs).unwrap();
        assert_eq!(c.done_mark(&leaf), None, "abandoned work is not done");
    }

    #[test]
    fn legacy_done_marks_move_into_the_marks() {
        let leaf = Scope::new(".mdkg/work/task-1-a.md");
        let mut c = Coordinator::new(100);
        c.register(leaf.clone(), "todo").unwrap();
        let cs = done(&mut c, &leaf, "done", 0);
        // As an older store kept it: the mark as a `done` work state.
        c.done.clear();
        c.work.insert(
            leaf.clone(),
            WorkStatus {
                status: "done".into(),
                actor: "x".into(),
                at: 2,
                changeset: Some(cs),
            },
        );
        c.upgrade_done_marks();
        c.set_work(&leaf, Some("review".into()), None, "x", 5)
            .unwrap();
        c.set_work(&leaf, None, None, "x", 6).unwrap();
        assert_eq!(c.done_mark(&leaf), Some(cs), "review and --clear keep it");
    }
}
