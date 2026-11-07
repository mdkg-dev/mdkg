//! The network side of graph sync: `ls-remote`, a fetch and a push of
//! only `remoteRef`, each through one explicit refspec, on
//! [`network_command`] (the user's git setup, hooks included, with nothing
//! able to prompt; edd-01a18867, Sync, G9).
//!
//! What crosses the remote: `--no-tags`, no `--all` or `--mirror`,
//! `--recurse-submodules=no`, no FETCH_HEAD, and only objects reachable
//! from the graph tip. Failures are classified into stable codes
//! (`graph.remote_unreachable`, `graph.remote_auth`), with any URL's
//! userinfo redacted and git's own text cut to one line.

use std::path::Path;

use crate::graphref::Error;

/// How a push ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pushed {
    /// The remote took it.
    Accepted,
    /// The lease didn't hold: the remote ref moved since it was read.
    Stale,
    /// A hook refused it, ours (`pre-push`) or the remote's (`pre-receive`,
    /// `update`): never retried around. The text is git's reason.
    Refused(String),
}

/// The remote operations sync makes. [`Git`] is the real one; tests may
/// wrap it to count calls or inject failures.
pub trait Net {
    /// The tip of `remote_ref` on `remote`, `None` when it's absent.
    fn remote_tip(&self, remote: &str, remote_ref: &str) -> Result<Option<String>, Error>;
    /// The commits `remote`'s branches (`refs/heads/*`) point at now, as
    /// the destination itself reports them.
    fn heads(&self, remote: &str) -> Result<Vec<String>, Error>;
    /// Fetches `remote_ref` into the local `into` ref, and returns its tip.
    fn fetch(&self, remote: &str, remote_ref: &str, into: &str) -> Result<String, Error>;
    /// Pushes `commit` to `remote_ref`, as a compare-and-swap on the remote:
    /// it must still be `expect` (`None`: absent).
    fn push(
        &self,
        remote: &str,
        commit: &str,
        remote_ref: &str,
        expect: Option<&str>,
    ) -> Result<Pushed, Error>;
}

/// The network runner of one clone, by its common git folder and the
/// checkout sync was started from ([`checkout`]).
#[derive(Debug, Clone)]
pub struct Git {
    common: std::path::PathBuf,
    from: std::path::PathBuf,
}

impl Git {
    pub fn new(common: &Path, from: &Path) -> Self {
        Self {
            common: common.to_path_buf(),
            from: from.to_path_buf(),
        }
    }

    fn run(&self, args: &[&str]) -> Result<std::process::Output, Error> {
        network_command(&self.common, &self.from)?
            .args(args)
            .run(None)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::TimedOut {
                    Error::new(
                        "graph.remote_unreachable",
                        "the graph remote didn't answer in time",
                    )
                } else {
                    Error::new("graph.git", format!("git {}: {e}", args[0]))
                }
            })
    }
}

/// A git command that talks to a remote, against the common folder:
/// [`mdkg_git::network_command`] (the hardened runner with the user's own
/// setup given back, as G9 asks: hooks run, the user's transports and ssh
/// setup, nothing can prompt, `transfer.fsckObjects=true`), for the
/// checkout `from` is in.
///
/// It runs where a plain `git push` from `from` would: in the checkout
/// `from` is in ([`checkout`], verified to be this clone's). So a relative
/// `core.hooksPath` (`.githooks`) finds the same hooks, and a relative
/// local remote URL the same repository (Terra #17687, #17986). No layout
/// is guessed from the git folder. Git reads the checkout's own git folder
/// (`--git-dir`), so a linked worktree's `config.worktree` and
/// `includeIf.onbranch` apply as they do to that plain `git push`, hooks
/// path included (Terra #18102).
pub fn network_command(common: &Path, from: &Path) -> Result<mdkg_git::Git, Error> {
    let common = &canonical(common)?;
    let (top, git_dir) = checkout_dirs(common, from)?;
    mdkg_git::network_command(common, &git_dir, &top)
        .map_err(|e| Error::new("graph.git", e.to_string()))
}

/// [`mdkg_git::user_config_command`] for the checkout `from` is in: the
/// reader approval and the destination guard use for remote
/// configuration, the same view the network command reads (Terra #18297,
/// #18368).
pub fn config_command(common: &Path, from: &Path) -> Result<mdkg_git::Git, Error> {
    let common = &canonical(common)?;
    let (top, git_dir) = checkout_dirs(common, from)?;
    Ok(mdkg_git::user_config_command(common, &git_dir, &top))
}

/// The hooks folder the network command runs hooks from, for the checkout
/// `from` is in ([`mdkg_git::network_hooks_path`]).
pub fn hooks_path(common: &Path, from: &Path) -> Result<String, Error> {
    let common = &canonical(common)?;
    let (top, git_dir) = checkout_dirs(common, from)?;
    mdkg_git::network_hooks_path(common, &git_dir, &top)
        .map_err(|e| Error::new("graph.git", e.to_string()))
}

fn canonical(common: &Path) -> Result<std::path::PathBuf, Error> {
    std::fs::canonicalize(common).map_err(|e| {
        Error::new(
            "graph.git",
            format!("the git folder {}: {e}", common.display()),
        )
    })
}

/// The checkout `from` is in, verified as git finds it (mdkg-git's
/// trusted-worktree check, made fresh): its top folder, when its common
/// git folder is `common`. Sync runs network git there, where the person
/// or launcher started it, as a plain `git push` from there would (Terra
/// #17986: deriving the checkout from the git folder guessed wrong for a
/// separate git folder). Anything else is `Err(graph.git)`; a bare clone
/// has no checkout to sync from.
pub fn checkout(common: &Path, from: &Path) -> Result<std::path::PathBuf, Error> {
    checkout_dirs(common, from).map(|(top, _)| top)
}

/// [`checkout`]'s top folder and its own git folder.
pub(crate) fn checkout_dirs(
    common: &Path,
    from: &Path,
) -> Result<(std::path::PathBuf, std::path::PathBuf), Error> {
    let refused = |why: String| {
        Error::new(
            "graph.git",
            format!(
                "graph sync runs git from the checkout it's started in, and {} isn't one of this \
                 clone's ({why})",
                from.display()
            ),
        )
    };
    let common = std::fs::canonicalize(common).map_err(|e| refused(e.to_string()))?;
    let found = mdkg_git::check_worktree(from).map_err(|e| refused(e.to_string()))?;
    let theirs = std::fs::canonicalize(&found.common).map_err(|e| refused(e.to_string()))?;
    if theirs != common {
        return Err(refused(format!("its git folder is {}", theirs.display())));
    }
    let top = std::fs::canonicalize(&found.top).map_err(|e| refused(e.to_string()))?;
    let git_dir = std::fs::canonicalize(&found.git_dir).map_err(|e| refused(e.to_string()))?;
    Ok((top, git_dir))
}

/// The local ref a fetch of `remote_ref` from `remote` lands in.
pub fn tracking_ref(remote: &str, remote_ref: &str) -> String {
    format!(
        "refs/mdkg/sync/{remote}/{}",
        remote_ref.strip_prefix("refs/").unwrap_or(remote_ref)
    )
}

/// A failed network command's error: unreachable, an authentication
/// failure, or a git error, with one line of git's text, redacted.
pub fn classify(what: &str, stderr: &[u8]) -> Error {
    let text = String::from_utf8_lossy(stderr);
    let lower = text.to_lowercase();
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
        .or_else(|| text.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("no output");
    let line: String = redact_line(line)
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect();
    const AUTH: &[&str] = &[
        "authentication failed",
        "permission denied",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "error: 403",
        "error: 401",
        "invalid credentials",
        "access denied",
    ];
    const UNREACHABLE: &[&str] = &[
        "could not resolve host",
        "connection refused",
        "connection timed out",
        "timed out",
        "network is unreachable",
        "no route to host",
        "unable to access",
        "does not appear to be a git repository",
        "could not read from remote repository",
        "repository not found",
        "no such file or directory",
        "connection reset",
    ];
    let code = if AUTH.iter().any(|p| lower.contains(p)) {
        "graph.remote_auth"
    } else if UNREACHABLE.iter().any(|p| lower.contains(p)) {
        "graph.remote_unreachable"
    } else {
        "graph.git"
    };
    Error::new(code, format!("{what}: {line}"))
}

/// A line of git's output with every URL's userinfo redacted.
fn redact_line(line: &str) -> String {
    line.split(' ')
        .map(|w| {
            if w.contains("://") {
                let (pre, url) = w.split_at(w.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(0));
                format!("{pre}{}", super::guard::redacted(url))
            } else {
                w.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn hex(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Net for Git {
    fn remote_tip(&self, remote: &str, remote_ref: &str) -> Result<Option<String>, Error> {
        let out = self.run(&["ls-remote", "--exit-code", "--refs", remote, remote_ref])?;
        match out.status.code() {
            Some(0) => {}
            Some(2) => return Ok(None),
            _ => return Err(classify("ls-remote", &out.stderr)),
        }
        // `ls-remote` matches the pattern's tail: only the exact ref counts.
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some((sha, name)) = line.split_once('\t')
                && name == remote_ref
                && hex(sha)
            {
                return Ok(Some(sha.to_owned()));
            }
        }
        Ok(None)
    }

    fn heads(&self, remote: &str) -> Result<Vec<String>, Error> {
        let out = self.run(&["ls-remote", "--refs", remote, "refs/heads/*"])?;
        if !out.status.success() {
            return Err(classify("ls-remote", &out.stderr));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut all: Vec<String> = text
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter(|(sha, name)| hex(sha) && name.starts_with("refs/heads/"))
            .map(|(sha, _)| sha.to_owned())
            .collect();
        all.sort();
        all.dedup();
        Ok(all)
    }

    fn fetch(&self, remote: &str, remote_ref: &str, into: &str) -> Result<String, Error> {
        let refspec = format!("+{remote_ref}:{into}");
        // `--refmap=` so the remote's configured fetch refspecs update no
        // other ref (a mirror-style `refs/heads/*:refs/heads/*` would move
        // the graph ref past every check), and `--no-prune` so none is
        // deleted: the one refspec is the only ref written (design review).
        let out = self.run(&[
            "fetch",
            "--refmap=",
            "--no-prune",
            "--no-tags",
            "--no-write-fetch-head",
            "--recurse-submodules=no",
            "--no-auto-maintenance",
            "--quiet",
            remote,
            &refspec,
        ])?;
        if !out.status.success() {
            return Err(classify("fetch", &out.stderr));
        }
        let tip = super::guard::git_ok(
            &self.common,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{into}^{{commit}}"),
            ],
        )?;
        let tip = String::from_utf8_lossy(&tip).trim().to_owned();
        if !hex(&tip) {
            return Err(Error::new(
                "graph.git",
                "the fetched graph ref isn't a commit",
            ));
        }
        Ok(tip)
    }

    fn push(
        &self,
        remote: &str,
        commit: &str,
        remote_ref: &str,
        expect: Option<&str>,
    ) -> Result<Pushed, Error> {
        let lease = format!("--force-with-lease={remote_ref}:{}", expect.unwrap_or(""));
        let refspec = format!("{commit}:{remote_ref}");
        let out = self.run(&[
            "push",
            "--porcelain",
            "--no-follow-tags",
            "--recurse-submodules=no",
            &lease,
            remote,
            &refspec,
        ])?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        // `<flag>\t<from>:<to>\t<summary>` for the one ref.
        let line = stdout.lines().find(|l| {
            l.split('\t')
                .nth(1)
                .is_some_and(|r| r.ends_with(&format!(":{remote_ref}")))
        });
        if out.status.success() {
            return match line.and_then(|l| l.chars().next()) {
                Some(' ' | '*' | '=' | '+') => Ok(Pushed::Accepted),
                _ => Err(classify("push", &out.stderr)),
            };
        }
        if let Some(l) = line {
            let summary = l.split('\t').nth(2).unwrap_or_default();
            if summary.contains("stale info") || summary.contains("fetch first") {
                return Ok(Pushed::Stale);
            }
            if summary.contains("remote rejected") || summary.contains("hook") {
                return Ok(Pushed::Refused(redact_line(summary)));
            }
        }
        let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
        // A `pre-push` hook's refusal is the one failure that reaches no
        // ref: no status line, no `fatal:`, only git's summary.
        if line.is_none()
            && err.contains("failed to push some refs")
            && !err.lines().any(|l| l.trim_start().starts_with("fatal:"))
        {
            return Ok(Pushed::Refused("the pre-push hook refused the push".into()));
        }
        Err(classify("push", &out.stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, tracking_ref};

    #[test]
    fn failures_are_classified_and_redacted() {
        let e = classify(
            "fetch",
            concat!(
                "fatal: unable to access 'https://someone:pw",
                "@example.invalid/x/': Could not resolve host: example.invalid\n"
            )
            .as_bytes(),
        );
        assert_eq!(e.code, "graph.remote_unreachable");
        assert!(!e.message.contains("pw@"), "{}", e.message);
        let e = classify("push", b"remote: Permission denied (publickey).\nfatal: Could not read from remote repository.\n");
        assert_eq!(e.code, "graph.remote_auth");
        assert_eq!(
            classify("push", b"fatal: something else\n").code,
            "graph.git"
        );
    }

    #[test]
    fn the_tracking_ref_is_private_to_mdkg() {
        assert_eq!(
            tracking_ref("origin", "refs/heads/mdkg-graph"),
            "refs/mdkg/sync/origin/heads/mdkg-graph"
        );
    }
}
