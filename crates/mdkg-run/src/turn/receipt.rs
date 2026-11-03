//! Read-only Git evidence for advisory turn receipts. A span is newly reachable
//! history between two observed HEADs, not proof of who authored its commits.
//! Immutable object ids pin reads after discovery; a changed HEAD, missing
//! history, invalid output or exhausted budget refuses the entire collection.
//! Native activity supplies local-work facts separately. Graph node identity
//! and done transitions are resolved by the CLI, not guessed from filenames here.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::process::Output;
use std::time::{Duration, Instant};

use crate::RunError;
use crate::state::Pinned;

pub mod delivery;
pub mod tasks;

/// One version in a raw tree diff. Modes preserve regular files, links and
/// submodules distinctly; only regular blobs may later be parsed as nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub mode: String,
    pub object: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Exact UTF-8 Git path, never split on whitespace or interpreted by a shell.
    pub path: String,
    pub before: Option<Version>,
    pub after: Option<Version>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    /// Diff against the first parent, or the empty tree for a root commit.
    /// Renames are represented as delete/add, without heuristic similarity.
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSpan {
    /// Full symbolic ref, or None for detached HEAD.
    pub branch: Option<String>,
    pub start: Option<String>,
    pub head: Option<String>,
    /// Parent-before-child topological order, including newly merged history.
    pub commits: Vec<Commit>,
}

#[derive(Clone, Copy)]
struct Limits {
    time: Duration,
    bytes: usize,
    commits: usize,
    changes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            time: Duration::from_secs(30),
            bytes: 16 << 20,
            commits: 10_000,
            changes: 100_000,
        }
    }
}

fn unavailable(reason: &str) -> RunError {
    RunError::failure(
        crate::error::CODE_AGENT_FAILED,
        format!("receipt Git span unavailable: {reason}"),
    )
}

struct Reader<'a> {
    pin: &'a Pinned,
    started: Instant,
    limits: Limits,
    used: usize,
    hook_names: Vec<OsString>,
    empty_tree: &'static str,
    #[cfg(test)]
    calls: usize,
}

impl<'a> Reader<'a> {
    fn new(pin: &'a Pinned, limits: Limits) -> Result<Self, RunError> {
        let mut r = Self {
            pin,
            started: Instant::now(),
            limits,
            used: 0,
            hook_names: Vec::new(),
            empty_tree: super::EMPTY_TREE_SHA1,
            #[cfg(test)]
            calls: 0,
        };
        // Match the launcher's Git policy, but include discovery itself in
        // this collector's byte/time budget, rather than an unbounded read.
        let hooks = r.run(&[
            "config",
            "--name-only",
            "-z",
            "--get-regexp",
            mdkg_coord_sqlite::git::HOOK_KEYS,
        ])?;
        r.hook_names = match hooks.status.code() {
            Some(0) => mdkg_coord_sqlite::git::hook_names(&hooks.stdout),
            Some(1) => Vec::new(),
            _ => return Err(unavailable("can't read Git hook configuration")),
        };
        let format = r.ok(&["rev-parse", "--show-object-format"])?;
        r.empty_tree = match format.as_slice() {
            b"sha1\n" => super::EMPTY_TREE_SHA1,
            b"sha256\n" => super::EMPTY_TREE_SHA256,
            _ => return Err(unavailable("unknown Git object format")),
        };
        // rev-list treats a shallow boundary as a root without reporting
        // missing history. That can silently omit newly merged commits.
        match r.ok(&["rev-parse", "--is-shallow-repository"])?.as_slice() {
            b"false\n" => {}
            b"true\n" => return Err(unavailable("shallow history is incomplete")),
            _ => return Err(unavailable("can't determine whether history is complete")),
        }
        Ok(r)
    }

    fn run(&mut self, args: &[&str]) -> Result<Output, RunError> {
        self.run_input(args, None)
    }

    fn run_input(&mut self, args: &[&str], input: Option<&[u8]>) -> Result<Output, RunError> {
        let left = self.limits.time.saturating_sub(self.started.elapsed());
        if left.is_zero() || self.used >= self.limits.bytes {
            return Err(unavailable("collection limit exceeded"));
        }
        let tree = format!("--work-tree={}", self.pin.top.display());
        let attr = format!("--attr-source={}", self.empty_tree);
        let mut all = vec![
            tree.as_str(),
            attr.as_str(),
            "-c",
            "core.attributesFile=/dev/null",
        ];
        all.extend_from_slice(args);
        let mut cmd = super::git_plain(&self.pin.git_dir, &self.pin.top, &all);
        mdkg_coord_sqlite::git::switch_off_hooks(&mut cmd, &self.hook_names);
        cmd.env("GIT_ATTR_NOSYSTEM", "1");
        #[cfg(test)]
        {
            self.calls += 1;
        }
        let out = super::run_bounded(
            &mut cmd,
            input,
            (self.limits.bytes - self.used) as u64,
            left,
        )
        .map_err(|_| unavailable("Git read failed or exceeded its collection limit"))?;
        self.account(out)
    }

    fn account(&mut self, out: Output) -> Result<Output, RunError> {
        // The shared process reader retains only a bounded stderr prefix.
        // A full prefix cannot prove the stream ended there, so refuse rather
        // than charging discarded bytes as if they had never been produced.
        if out.stderr.len() as u64 >= super::STDERR_CAPTURE_LIMIT {
            return Err(unavailable("Git stderr reached its capture limit"));
        }
        self.used = self
            .used
            .saturating_add(out.stdout.len())
            .saturating_add(out.stderr.len());
        if self.used > self.limits.bytes || self.started.elapsed() > self.limits.time {
            return Err(unavailable("collection limit exceeded"));
        }
        Ok(out)
    }

    fn ok(&mut self, args: &[&str]) -> Result<Vec<u8>, RunError> {
        let out = self.run(args)?;
        if !out.status.success() {
            return Err(unavailable("Git could not read the requested history"));
        }
        Ok(out.stdout)
    }

    fn head(&mut self) -> Result<(Option<String>, Option<String>), RunError> {
        let reference = self.run(&["symbolic-ref", "--quiet", "HEAD"])?;
        let branch = match reference.status.code() {
            Some(0) => Some(line(&reference.stdout)?.to_string()),
            Some(1) if reference.stdout.is_empty() => None,
            _ => return Err(unavailable("can't read HEAD's branch")),
        };
        let out = self.run(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])?;
        if out.status.success() {
            let head = line(&out.stdout)?;
            if !object_id(head) {
                return Err(unavailable("invalid HEAD object id"));
            }
            return Ok((branch, Some(head.into())));
        }
        // Only a missing symbolic branch is unborn. A corrupt/detached HEAD
        // or a ref whose object has disappeared must not look like no history.
        if let Some(branch) = &branch
            && out.status.code() == Some(1)
            && out.stdout.is_empty()
        {
            let absent = self.run(&["show-ref", "--verify", "--quiet", branch])?;
            if absent.status.code() == Some(1)
                && absent.stdout.is_empty()
                && absent.stderr.is_empty()
            {
                return Ok((Some(branch.clone()), None));
            }
        }
        Err(unavailable(
            "HEAD is neither a readable commit nor an unborn branch",
        ))
    }
}

fn line(bytes: &[u8]) -> Result<&str, RunError> {
    let s = std::str::from_utf8(bytes).map_err(|_| unavailable("Git output is not UTF-8"))?;
    let s = s
        .strip_suffix('\n')
        .ok_or_else(|| unavailable("incomplete Git output"))?;
    if s.is_empty() || s.contains(['\n', '\r', '\0']) {
        return Err(unavailable("invalid Git output line"));
    }
    Ok(s)
}

fn object_id(s: &str) -> bool {
    super::full_hex(s) && s.bytes().all(|b| !b.is_ascii_uppercase())
}

/// Harvest a whole observed span. `start` comes from the launcher's pre-turn
/// context; None means that context observed an unborn checkout. No fallback
/// to a merge base or current status silently manufactures turn evidence.
pub fn harvest(pin: &Pinned, start: Option<&str>) -> Result<GitSpan, RunError> {
    harvest_with(pin, start, Limits::default())
}

fn harvest_with(pin: &Pinned, start: Option<&str>, limits: Limits) -> Result<GitSpan, RunError> {
    harvest_observed(pin, start, limits, || {})
}

// The no-op boundary also lets fixtures change HEAD at the exact observation
// boundary, without racing a real Git process or changing the production clock.
fn harvest_observed(
    pin: &Pinned,
    start: Option<&str>,
    limits: Limits,
    before_final_head: impl FnOnce(),
) -> Result<GitSpan, RunError> {
    if start.is_some_and(|s| !object_id(s)) {
        return Err(unavailable("invalid starting object id"));
    }
    let mut r = Reader::new(pin, limits)?;
    let (branch, head) = r.head()?;
    let mut commits = Vec::new();
    if let Some(head) = head.as_deref() {
        if let Some(start) = start {
            let ancestor = r.run(&["merge-base", "--is-ancestor", start, head])?;
            match ancestor.status.code() {
                Some(0) => {}
                Some(1) => {
                    return Err(unavailable(&format!("{head} does not contain {start}")));
                }
                _ => return Err(unavailable("starting history is missing or unreadable")),
            }
        }
        let exclude = start.map(|s| format!("^{s}"));
        // One extra entry lets the cap refuse instead of truncating evidence.
        let max = format!("--max-count={}", limits.commits.saturating_add(1));
        let mut args = vec![
            "rev-list",
            "--reverse",
            "--topo-order",
            "--parents",
            &max,
            head,
        ];
        args.extend(exclude.as_deref());
        args.push("--");
        let list = r.ok(&args)?;
        let list = std::str::from_utf8(&list).map_err(|_| unavailable("invalid commit list"))?;
        if !list.is_empty() && !list.ends_with('\n') {
            return Err(unavailable("incomplete commit list"));
        }
        let mut seen = BTreeSet::new();
        for row in list.lines() {
            if commits.len() == limits.commits {
                return Err(unavailable("commit count limit exceeded"));
            }
            let ids: Vec<_> = row.split(' ').collect();
            if ids.is_empty() || ids.iter().any(|s| !object_id(s)) || !seen.insert(ids[0]) {
                return Err(unavailable("invalid commit list"));
            }
            commits.push(Commit {
                id: ids[0].into(),
                parents: ids[1..].iter().map(|s| (*s).into()).collect(),
                changes: Vec::new(),
            });
        }
        if !commits.is_empty() {
            // --always emits an id for empty commits too. With -z each id is
            // NUL-delimited, then raw header/path pairs follow. One process
            // covers the whole span rather than paying a spawn per commit.
            // A stdin commit line can explicitly supply its parents. Supply
            // only the already-read first parent: -m otherwise emits a frame
            // for each parent, despite --first-parent on diff-tree itself.
            let input: String = commits
                .iter()
                .map(|c| match c.parents.first() {
                    Some(parent) => format!("{} {parent}\n", c.id),
                    None => format!("{}\n", c.id),
                })
                .collect();
            let out = r.run_input(
                &[
                    "diff-tree",
                    "--stdin",
                    "--root",
                    "--always",
                    "-m",
                    "--raw",
                    "--no-abbrev",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--ignore-submodules=none",
                    "-r",
                    "-z",
                ],
                Some(input.as_bytes()),
            )?;
            if !out.status.success() {
                return Err(unavailable("Git could not read the requested diffs"));
            }
            batch_diff(&out.stdout, &mut commits, limits.changes)?;
        }
    } else if start.is_some() {
        return Err(unavailable("the turn's starting history disappeared"));
    }
    before_final_head();
    if r.head()? != (branch.clone(), head.clone()) {
        return Err(unavailable("HEAD changed while collecting the receipt"));
    }
    Ok(GitSpan {
        branch,
        start: start.map(Into::into),
        head,
        commits,
    })
}

fn batch_diff(bytes: &[u8], commits: &mut [Commit], limit: usize) -> Result<(), RunError> {
    let mut fields = bytes.split(|b| *b == 0).peekable();
    let mut count = 0usize;
    for commit in commits {
        if fields.next() != Some(commit.id.as_bytes()) {
            return Err(unavailable("incomplete or out-of-order commit diff"));
        }
        let mut diff = Vec::new();
        while fields.peek().is_some_and(|h| h.starts_with(b":")) {
            diff.extend_from_slice(fields.next().expect("peeked header"));
            diff.push(0);
            let path = fields
                .next()
                .ok_or_else(|| unavailable("missing batch diff path"))?;
            diff.extend_from_slice(path);
            diff.push(0);
        }
        commit.changes = parse_diff(&diff)?;
        count = count.saturating_add(commit.changes.len());
        if count > limit {
            return Err(unavailable("changed path count limit exceeded"));
        }
        commit.changes.sort_by(|a, b| a.path.cmp(&b.path));
    }
    if fields.next() != Some(b"".as_slice()) || fields.next().is_some() {
        return Err(unavailable("unexpected trailing commit diff output"));
    }
    Ok(())
}

fn parse_diff(bytes: &[u8]) -> Result<Vec<Change>, RunError> {
    let mut fields = bytes.split(|b| *b == 0);
    let mut result = Vec::new();
    while let Some(header) = fields.next() {
        if header.is_empty() && fields.next().is_none() {
            break;
        }
        let header = std::str::from_utf8(header).map_err(|_| unavailable("invalid tree diff"))?;
        let columns: Vec<_> = header.strip_prefix(':').unwrap_or("").split(' ').collect();
        if columns.len() != 5 || !matches!(columns[4], "A" | "D" | "M" | "T") {
            return Err(unavailable("invalid tree diff header"));
        }
        let version = |mode: &str, id: &str| -> Result<Option<Version>, RunError> {
            if !object_id(id) || mode.len() != 6 || !mode.bytes().all(|b| matches!(b, b'0'..=b'7'))
            {
                return Err(unavailable("invalid tree version"));
            }
            if mode == "000000" && id.bytes().all(|b| b == b'0') {
                return Ok(None);
            }
            if !matches!(mode, "100644" | "100755" | "120000" | "160000")
                || id.bytes().all(|b| b == b'0')
            {
                return Err(unavailable("unsupported tree version"));
            }
            Ok(Some(Version {
                mode: mode.into(),
                object: id.into(),
            }))
        };
        let before = version(columns[0], columns[2])?;
        let after = version(columns[1], columns[3])?;
        if !matches!(
            (columns[4], before.is_some(), after.is_some()),
            ("A", false, true) | ("D", true, false) | ("M" | "T", true, true)
        ) {
            return Err(unavailable("inconsistent tree diff"));
        }
        let path = fields
            .next()
            .ok_or_else(|| unavailable("missing tree path"))?;
        let path = std::str::from_utf8(path).map_err(|_| unavailable("tree path is not UTF-8"))?;
        if path.is_empty()
            || path.starts_with('/')
            || path.split('/').any(|s| matches!(s, "" | "." | ".."))
        {
            return Err(unavailable("invalid tree path"));
        }
        result.push(Change {
            path: path.into(),
            before,
            after,
        });
    }
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(unavailable("incomplete tree diff"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
