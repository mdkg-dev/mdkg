//! The hooks' reads against the real repository: the actor's unread mail,
//! its leases, and the shared records under the git folder
//! ([`mdkg_run::hooks`]).

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use mdkg_coord::Status;
use mdkg_coord_sqlite::mail::InboxQuery;
use mdkg_format::Code;

use super::{Changes, Holds, Lease, Mail, MailSource, Reminded};
use crate::coord::{Repo, now_ms};

pub(super) struct Live {
    pub root: PathBuf,
    pub actor: String,
    /// The agent's folder, which its relative paths start from.
    pub cwd: Option<PathBuf>,
}

/// How often `wait` looks: `$MDKG_WAIT_POLL_MS`, else once a second, as
/// `msg wait` does.
pub(super) fn poll_interval() -> Duration {
    let ms = std::env::var("MDKG_WAIT_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000u64);
    Duration::from_millis(ms.max(10))
}

impl MailSource for Live {
    /// Read as `msg inbox` reads it, the actor's own mailbox and the roles
    /// it holds, straight from the store, whose `inbox()` marks nothing as
    /// shown (lane 2): a hook names messages and never shows a body.
    fn unread(&self) -> Result<Vec<Mail>, String> {
        let repo = Repo::open(&self.root).map_err(|e| e.diagnostic.message)?;
        let mut store = repo.store().map_err(|e| e.diagnostic.message)?;
        let now = now_ms();
        let state = store.coordinator().map_err(|e| e.to_string())?;
        let mut mailboxes = vec![self.actor.clone()];
        mailboxes.extend(
            state
                .leases()
                .filter(|l| {
                    l.owner == self.actor
                        && l.confirmed
                        && l.status == Status::Active
                        && l.deadline > now
                        && l.scope.as_str().starts_with("role:")
                })
                .map(|l| l.scope.as_str().to_string()),
        );
        let q = InboxQuery {
            mailboxes,
            room: None,
            kinds: Vec::new(),
            room_kinds: Vec::new(),
            after: None,
            all: false,
            limit: None,
            now_ms: now,
        };
        let inbox = store.inbox(&q).map_err(|e| e.to_string())?;
        Ok(inbox
            .messages
            .into_iter()
            .map(|m| Mail {
                seq: m.seq,
                kind: m.kind,
                sender: m.sender,
                room: m.room,
            })
            .collect())
    }
}

/// `.` and `..` resolved without the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// A path as the disk has it: its longest existing part resolved, links
/// followed, and the rest, which doesn't exist yet, as written.
fn resolve(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while std::fs::symlink_metadata(&existing).is_err() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = std::fs::canonicalize(&existing).unwrap_or(existing);
    for part in rest.into_iter().rev() {
        out.push(part);
    }
    out
}

impl Live {
    fn requested(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        match &self.cwd {
            Some(cwd) if p.is_relative() => cwd.join(p),
            _ if p.is_relative() => self.root.join(p),
            _ => p.to_path_buf(),
        }
    }

    fn held(&self, root: &Path, scope: &str) -> Result<Lease, String> {
        let report = crate::coord::holds_report(root, Some(self.actor.clone()), scope, None)
            .map_err(|e| e.diagnostic.message)?;
        match report.data {
            Some(d) if d.holds => Ok(Lease::Held),
            Some(_) => Ok(Lease::Unheld(scope.to_string())),
            None => Err(report
                .diagnostics
                .first()
                .map_or_else(|| "no answer".to_string(), |d| d.message.clone())),
        }
    }

    fn common(&self) -> Option<PathBuf> {
        let repo = Repo::open(&self.root).ok()?;
        mdkg_coord_sqlite::git::common_dir(&repo.top).ok()
    }
}

/// The top of the repository worktree holding `abs` when git says its
/// common dir is `common`, though no worktree listing named it: asked of
/// git in its nearest existing folder, with no inherited GIT_DIR.
fn unlisted_worktree(abs: &Path, common: &Path) -> Option<PathBuf> {
    let dir = abs
        .ancestors()
        .find(|d| d.is_dir())
        .map(Path::to_path_buf)?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--show-toplevel",
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines();
    let (theirs, top) = (lines.next()?, lines.next()?);
    let theirs = std::fs::canonicalize(theirs).ok()?;
    (theirs == common).then(|| std::fs::canonicalize(top).unwrap_or_else(|_| top.into()))
}

impl Holds for Live {
    /// The lease covering a path: through this worktree's own scope when
    /// it's here; through the sibling worktree's when it's in another of
    /// the repository's (a lease is the repository's, whatever checkout it
    /// was claimed from); refused in git's own folder, a checkout's `.git`
    /// or the common dir; external anywhere else.
    fn lease(&self, path: &str) -> Result<Lease, String> {
        let repo = Repo::open(&self.root).map_err(|e| e.diagnostic.message)?;
        let requested = self.requested(path);
        let abs = resolve(&normalize(&requested));
        let common = mdkg_coord_sqlite::git::common_dir(&repo.top)
            .ok()
            .and_then(|c| std::fs::canonicalize(c).ok());
        let mut tops = mdkg_coord_sqlite::git::worktrees(&repo.top).unwrap_or_default();
        tops.push(repo.top.clone());
        let tops: Vec<PathBuf> = tops
            .into_iter()
            .map(|t| std::fs::canonicalize(&t).unwrap_or(t))
            .collect();
        if common.as_ref().is_some_and(|c| abs.starts_with(c))
            || tops.iter().any(|t| abs.starts_with(t.join(".git")))
        {
            return Ok(Lease::GitAdmin);
        }
        // The worktree holding it, the deepest when one sits in another.
        // The list can miss one (a main checkout made with
        // --separate-git-dir under another name), so a path outside every
        // listed worktree is asked of git before it counts as external
        // (review of bb0f8fb).
        let found = tops
            .iter()
            .filter(|t| abs.starts_with(t))
            .max_by_key(|t| t.components().count())
            .cloned();
        let top = match found {
            Some(t) => t,
            None => match common.as_deref().and_then(|c| unlisted_worktree(&abs, c)) {
                Some(t) => t,
                None => return Ok(Lease::External),
            },
        };
        let here = std::fs::canonicalize(&repo.top).unwrap_or_else(|_| repo.top.clone());
        let (owner, root) = if top == here {
            (repo, self.root.clone())
        } else {
            (
                Repo::open(&top).map_err(|e| e.diagnostic.message)?,
                top.clone(),
            )
        };
        match owner.path_target(&abs.to_string_lossy()) {
            Ok(t) => self.held(&root, t.scope.as_str()),
            Err(e) if e.diagnostic.code == Code::UsagePathOutsideWorktree => Ok(Lease::GitAdmin),
            Err(e) => Err(e.diagnostic.message),
        }
    }
}

impl Changes for Live {
    /// One bounded, read-only `git status` of the worktree
    /// ([`mdkg_run::hooks::changed_paths`]), then each changed entry's own
    /// lease, by the path git reports, never through a link to its target
    /// (review of b02b639): a link is its own file.
    fn unleased(&self) -> Result<Vec<String>, String> {
        let repo = Repo::open(&self.root).map_err(|e| e.diagnostic.message)?;
        let mut unleased = Vec::new();
        for path in mdkg_run::hooks::changed_paths(&repo.top)? {
            if let Lease::Unheld(scope) = self.held(&self.root, &path)? {
                unleased.push(scope);
            }
        }
        unleased.sort();
        unleased.dedup();
        Ok(unleased)
    }
}

impl Reminded for Live {
    fn last(&self, session: &str) -> u64 {
        self.common().map_or(0, |c| {
            mdkg_run::hooks::reminded_through(&c, &self.actor, session)
        })
    }

    fn claim(&self, session: &str, seq: u64) -> bool {
        self.common()
            .is_some_and(|c| mdkg_run::hooks::claim_reminder(&c, &self.actor, session, seq))
    }

    fn confirm(&self, session: &str, seq: u64) -> Result<bool, String> {
        let common = self
            .common()
            .ok_or_else(|| "can't find the reminder store".to_string())?;
        mdkg_run::hooks::confirm_reminder(&common, &self.actor, session, seq)
            .map_err(|e| e.to_string())
    }

    fn claim_audit(&self, session: &str, fingerprint: u64) -> bool {
        self.common()
            .is_some_and(|c| mdkg_run::hooks::claim_audit(&c, &self.actor, session, fingerprint))
    }
}

/// A session's wait, held while it runs: an OS lock on an open file, which
/// the kernel releases when this process ends however it ends, so a crash
/// leaves nothing stale to recover and no two waits hold it at once.
pub(super) struct Waiting(#[allow(dead_code)] std::fs::File);

impl Live {
    /// Takes the session's wait, unless another process holds it.
    pub(super) fn waiting(&self, session: &str) -> Option<Waiting> {
        let dir = mdkg_run::hooks::dir(&self.common()?).join("waiting");
        std::fs::create_dir_all(&dir).ok()?;
        let name: String = format!("{}-{session}", self.actor)
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '%'
                }
            })
            .take(160)
            .collect();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{name}.lock")))
            .ok()?;
        file.try_lock().ok()?;
        Some(Waiting(file))
    }
}
