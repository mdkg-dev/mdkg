//! Reading git for settlement: what main holds, where a worktree is, and
//! checking a file out of another branch's commit.
//!
//! Leaves are repository-relative file paths, so a node file maps to a scope
//! one to one. Content hashes are `sha256:` over the file's bytes, the same
//! in a blob and in a worktree file when no git filters rewrite content.
//!
//! The hardened Git runner, trusted worktrees and the worktree-check cache
//! are mdkg-git's, re-exported here (orchestrator, 2026-10-30, on g-authority
//! #10952): one cache and one request trust per process, so a command that
//! probes ref mode and then opens the coordinator checks its worktree once.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use mdkg_coord::{Scope, Source};
use mdkg_format::Digest;

use crate::digest_cache::DigestCache;

pub use mdkg_git::{
    Checkout, Git, HOOK_EVENTS, HOOK_KEYS, MIN_VERSION, WorktreeTrust, check_worktree,
    check_worktree_with_trust, command, common_command, common_dir, find_git, git_program, has_ref,
    hook_names, local_offset_minutes, main_ref, minimum_version, read_capped, read_small,
    ready_to_write, require_files_ref_storage, switch_off_hooks, toplevel, upgrade_hint,
    with_worktree_trust, worktrees,
};

fn failure(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn git(dir: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let out = command(dir).args(args).run(None)?;
    if !out.status.success() {
        return Err(failure(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

fn line(bytes: Vec<u8>) -> io::Result<String> {
    String::from_utf8(bytes)
        .map(|s| s.trim_end().to_string())
        .map_err(|e| failure(e.to_string()))
}

/// Where the coordinator database lives for a repository.
pub fn store_path(repo: &Path) -> io::Result<PathBuf> {
    Ok(common_dir(repo)?.join("mdkg").join("coord.sqlite"))
}

/// The worktree's current commit and branch.
pub fn head(worktree: &Path) -> io::Result<Source> {
    Ok(Source {
        commit: line(git(worktree, &["rev-parse", "HEAD"])?)?,
        branch: line(git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?)?,
    })
}

/// The file's spelling on disk, matching `path` in any case: the first
/// tracked or untracked match, or `path` itself if there is none.
pub fn disk_spelling(worktree: &Path, path: &str) -> String {
    files_under(worktree, path)
        .ok()
        .and_then(|files| {
            files
                .into_iter()
                .find(|f| Scope::new(f.as_str()) == Scope::new(path))
        })
        .unwrap_or_else(|| path.to_string())
}

/// Whether `path` is tracked and its file, staged or not, is as `HEAD`
/// has it: nothing about it is left to commit.
pub fn unchanged_since_head(worktree: &Path, path: &str) -> io::Result<bool> {
    let spec = format!(":(literal){path}");
    let tracked = !git(worktree, &["ls-files", "-z", "--full-name", "--", &spec])?.is_empty();
    if !tracked {
        return Ok(false);
    }
    let status = command(worktree)
        .args(["diff", "--quiet", "HEAD", "--", &spec])
        .run(None)?
        .status;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure(format!("can't tell whether {path} is committed"))),
    }
}

/// Whether `a` and `b` hold the same content under `path` (a file or a
/// folder, matched as scopes match ASCII case).
pub fn same_under(repo: &Path, a: &str, b: &str, path: &str) -> io::Result<bool> {
    let spec = format!(":(icase,literal){path}");
    let status = command(repo)
        .args(["diff", "--quiet", "--no-renames", a, b, "--", &spec])
        .run(None)?
        .status;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure(format!("can't compare {a} and {b} under {path}"))),
    }
}

/// Whether `tip` has a commit that `base` lacks and that changed `path`.
/// Every side of every merge is walked (`--full-history`): git's default
/// simplification would hide an edit on main's side behind a merge that
/// took the other side's content whole. A merge whose content under
/// `path` is one of its parents' changed nothing itself and doesn't count;
/// what its other side brought still does.
pub fn touched_between(repo: &Path, base: &str, tip: &str, path: &str) -> io::Result<bool> {
    let spec = format!(":(icase,literal){path}");
    let range = format!("{base}..{tip}");
    let listed = git(
        repo,
        &[
            "rev-list",
            "--full-history",
            "--parents",
            &range,
            "--",
            &spec,
        ],
    )?;
    for line in String::from_utf8_lossy(&listed).lines() {
        let mut ids = line.split_whitespace();
        let Some(commit) = ids.next() else {
            continue;
        };
        let parents: Vec<&str> = ids.collect();
        if parents.len() < 2 {
            return Ok(true);
        }
        let mut takes_a_side = false;
        for parent in &parents {
            if same_under(repo, parent, commit, path)? {
                takes_a_side = true;
                break;
            }
        }
        if !takes_a_side {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the repository has no commit at all yet: no branch, tag or
/// other ref names anything, and HEAD names no commit (a fresh `git
/// init`; a detached HEAD whose branches were deleted still has commits).
pub fn no_commits(repo: &Path) -> io::Result<bool> {
    if !git(repo, &["for-each-ref", "--count=1", "--format=%(refname)"])?.is_empty() {
        return Ok(false);
    }
    let head = command(repo)
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .run(None)?;
    Ok(!head.status.success())
}

/// The commit `refname` names.
pub fn commit_of(repo: &Path, refname: &str) -> io::Result<String> {
    mdkg_git::cached_ref(repo, refname, "coord_commit", || {
        line(git(
            repo,
            &["rev-parse", "--verify", &format!("{refname}^{{commit}}")],
        )?)
    })
}

/// The content hash of a worktree file, or `None` if it doesn't exist.
pub fn file_digest(worktree: &Path, path: &str) -> io::Result<Option<String>> {
    match fs::read(worktree.join(path)) {
        Ok(bytes) => Ok(Some(Digest::of_bytes(&bytes).to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Stages and commits exactly `paths` (other staged changes stay staged).
/// Returns false when they had nothing to commit.
pub fn commit_paths(worktree: &Path, paths: &[String], message: &str) -> io::Result<bool> {
    // No paths means nothing to commit, never "everything staged".
    if paths.is_empty() {
        return Ok(false);
    }
    // The files inside these scopes, matched as scopes are (in any case,
    // never as globs), named exactly for git. A deletion already staged
    // (`git rm`, `git mv`) is committed too, though `git add` can't name it.
    let (present, deleted) = files_in(worktree, paths)?;
    for p in paths {
        let scope = Scope::new(p.as_str());
        let found = present
            .iter()
            .chain(&deleted)
            .any(|f| scope.contains(&Scope::new(f.as_str())));
        if !found {
            return Err(failure(format!(
                "{p} matches no file git can see (is it ignored?)"
            )));
        }
    }
    let literal = |f: &String| format!(":(literal){f}");
    let staged: Vec<String> = present.iter().map(literal).collect();
    let specs: Vec<String> = present.iter().chain(&deleted).map(literal).collect();
    let paths = &specs;
    // The index entries for these paths before staging, so a failed commit
    // can put the index back exactly, including anything staged on purpose.
    let mut list = vec!["ls-files", "--stage", "-z", "--"];
    list.extend(paths.iter().map(String::as_str));
    let saved = git(worktree, &list)?;
    // Before anything is staged, so a refusal leaves the index as it was.
    ready_to_write(worktree)?;
    if !staged.is_empty() {
        let mut add = vec!["add", "-A", "--"];
        add.extend(staged.iter().map(String::as_str));
        git(worktree, &add)?;
    }
    // No textconv or external diff: their output must not decide.
    let mut diff = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--cached",
        "--quiet",
        "--",
    ];
    diff.extend(paths.iter().map(String::as_str));
    let unchanged = command(worktree).args(&diff).run(None)?.status.success();
    if unchanged {
        return Ok(false);
    }
    let mut commit = vec![
        "commit",
        "--quiet",
        "--no-verify",
        "--no-gpg-sign",
        "-m",
        message,
        "--",
    ];
    commit.extend(paths.iter().map(String::as_str));
    // Again just before the commit, so what config defines or HEAD names
    // can't change in between unseen.
    if let Err(e) = ready_to_write(worktree) {
        if let Err(r) = restore_index(worktree, paths, &saved) {
            return Err(failure(format!(
                "{e}; restoring the index also failed: {r}"
            )));
        }
        return Err(e);
    }
    let head = |rev: &str| {
        git(worktree, &["rev-parse", "--verify", "--quiet", rev])
            .ok()
            .and_then(|b| line(b).ok())
    };
    let before = head("HEAD");
    if let Err(e) = git(worktree, &commit) {
        // Cut short after our commit landed (a time limit hit while git
        // tidied up): HEAD moved on by one commit from where it was, and
        // holds these paths as staged.
        let landed = before.is_some() && head("HEAD^1") == before && head("HEAD") != before && {
            let mut same = vec!["diff", "--cached", "--quiet", "HEAD", "--"];
            same.extend(paths.iter().map(String::as_str));
            command(worktree)
                .args(&same)
                .run(None)
                .is_ok_and(|o| o.status.success())
        };
        if landed {
            return Ok(true);
        }
        if let Err(r) = restore_index(worktree, paths, &saved) {
            return Err(failure(format!(
                "{e}; restoring the index also failed: {r}"
            )));
        }
        return Err(e);
    }
    Ok(true)
}

/// Puts the index entries for `paths` back to `saved` (`ls-files --stage
/// -z` output): entries added since are dropped, saved ones written back.
fn restore_index(worktree: &Path, paths: &[String], saved: &[u8]) -> io::Result<()> {
    // -f: the entry may differ from both HEAD and the worktree (staged on
    // purpose, then edited), which plain rm refuses.
    let mut rm = vec![
        "rm",
        "--cached",
        "-f",
        "--quiet",
        "-r",
        "--ignore-unmatch",
        "--",
    ];
    rm.extend(paths.iter().map(String::as_str));
    git(worktree, &rm)?;
    if saved.is_empty() {
        return Ok(());
    }
    let out = command(worktree)
        .args(["update-index", "-z", "--index-info"])
        .run(Some(saved))?;
    if !out.status.success() {
        return Err(failure(String::from_utf8_lossy(&out.stderr).to_string()));
    }
    Ok(())
}

/// Pathspecs narrowing a listing to the top folders of `scopes`, so a
/// command doesn't list the whole repository. Git folds only ASCII case, so a
/// top folder with other letters means no narrowing (everything is listed).
fn narrowing(scopes: &[Scope]) -> Vec<String> {
    let mut tops: BTreeSet<String> = BTreeSet::new();
    for s in scopes {
        let top = s.as_str().split('/').next().unwrap_or_default();
        // The Kelvin sign folds to `k` for scopes but not for git, so a `k`
        // means no narrowing either.
        if top.is_empty() || !top.is_ascii() || top.contains(['k', 'K']) {
            return Vec::new();
        }
        tops.insert(format!(":(icase,literal){top}"));
    }
    tops.into_iter().collect()
}

/// NUL-separated names from git, as text. A name that isn't UTF-8 can't be
/// named back to git exactly, so it's reported rather than mangled.
fn names(out: &[u8]) -> Vec<Result<String, String>> {
    out.split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8(r.to_vec()).map_err(|_| String::from_utf8_lossy(r).into_owned()))
        .collect()
}

/// Files inside `scopes`: those in the index or the worktree (`present`, not
/// ignored), and those whose deletion is already staged (`deleted`), matched
/// as scopes match: in any case (with Unicode folding), segment by segment,
/// never as globs. Refuses two files that differ only in case, which scopes
/// can't tell apart, and names that aren't UTF-8.
fn files_in(worktree: &Path, scopes: &[String]) -> io::Result<(Vec<String>, Vec<String>)> {
    let scopes: Vec<Scope> = scopes.iter().map(|s| Scope::new(s.as_str())).collect();
    let narrow = narrowing(&scopes);
    let mut list = vec![
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
        "--full-name",
        "--",
    ];
    list.extend(narrow.iter().map(String::as_str));
    let mut gone = vec![
        "diff",
        "--cached",
        "--name-only",
        "-z",
        "--no-renames",
        "--diff-filter=D",
        "--",
    ];
    gone.extend(narrow.iter().map(String::as_str));
    let mut seen: BTreeMap<Scope, String> = BTreeMap::new();
    let mut take = |raw: Vec<Result<String, String>>, deleted: bool| -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for name in raw {
            let f = match name {
                Ok(f) => f,
                Err(lossy) => {
                    if scopes
                        .iter()
                        .any(|s| s.contains(&Scope::new(lossy.as_str())))
                    {
                        return Err(failure(format!(
                            "{lossy} has a name that isn't UTF-8, which mdkg can't name to git; rename it"
                        )));
                    }
                    continue;
                }
            };
            let leaf = Scope::new(f.as_str());
            if !scopes.iter().any(|s| s.contains(&leaf)) {
                continue;
            }
            match seen.get(&leaf) {
                Some(other) if *other == f => continue,
                Some(other) if deleted => {
                    return Err(failure(format!(
                        "a rename of {f} to {other}, differing only in case, is staged; \
                         commit it with `git commit` first, then try again"
                    )));
                }
                Some(other) => {
                    return Err(failure(format!(
                        "{other} and {f} differ only in case; mdkg treats them as one file, \
                         so rename one with `git mv`"
                    )));
                }
                None => {
                    seen.insert(leaf, f.clone());
                    out.push(f);
                }
            }
        }
        out.sort();
        Ok(out)
    };
    let present = take(names(&git(worktree, &list)?), false)?;
    let deleted = take(names(&git(worktree, &gone)?), true)?;
    Ok((present, deleted))
}

/// Tracked and untracked (not ignored) files inside `path`, relative to the
/// worktree's top folder, matched as scopes match (see [`files_in`]).
pub fn files_under(worktree: &Path, path: &str) -> io::Result<Vec<String>> {
    Ok(files_in(worktree, &[path.to_string()])?.0)
}

/// Files changed on this branch since it left `base`, plus uncommitted and
/// untracked ones, relative to the worktree's top folder.
pub fn changed_since(worktree: &Path, base: &str) -> io::Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let range = format!("{base}...HEAD");
    for args in [
        vec!["diff", "--name-only", "-z", range.as_str()],
        vec!["diff", "--name-only", "-z", "HEAD"],
        vec![
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--full-name",
        ],
    ] {
        for name in git(worktree, &args)?
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
        {
            out.insert(String::from_utf8_lossy(name).into_owned());
        }
    }
    Ok(out)
}

/// Brings `commit` into the worktree's branch (`git merge`), so work that
/// builds on another branch's unmerged version keeps its history. This is
/// `sync` for editing: copying the file alone loses the ancestry, and the
/// later merge to main then conflicts. Worktrees share one object store, so
/// another local branch's commit needs no fetch.
pub fn stack_on(worktree: &Path, commit: &str) -> io::Result<()> {
    ready_to_write(worktree)?;
    let out = command(worktree)
        .args([
            "merge",
            "--no-edit",
            "--no-verify",
            "--no-gpg-sign",
            "--no-verify-signatures",
            commit,
        ])
        .run(None)?;
    if out.status.success() {
        // A shallow history can make git report "Already up to date"
        // without merging: only a merge that took counts.
        if !is_ancestor(worktree, commit)? {
            return Err(failure(format!(
                "git merge {commit} reported success, but HEAD doesn't contain it (a shallow \
                 clone? `git fetch --unshallow`)"
            )));
        }
        return Ok(());
    }
    // A conflict is reported on stdout, a refusal to start on stderr.
    let said = [out.stdout, out.stderr]
        .iter()
        .map(|b| String::from_utf8_lossy(b).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    Err(failure(format!("git merge {commit}: {said}")))
}

/// Whether `commit` is already in the history of the worktree's HEAD.
pub fn is_ancestor(worktree: &Path, commit: &str) -> io::Result<bool> {
    is_ancestor_of(worktree, commit, "HEAD")
}

/// Whether `commit` is in the history of `refname` (itself included).
pub fn is_ancestor_of(repo: &Path, commit: &str, refname: &str) -> io::Result<bool> {
    let status = command(repo)
        .args(["merge-base", "--is-ancestor", commit, refname])
        .run(None)?
        .status;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure(format!(
            "can't tell whether {commit} is in {refname}"
        ))),
    }
}

/// [`is_ancestor_of`], read from a common directory ([`common_command`]).
pub fn is_ancestor_in_common(common: &Path, commit: &str, head: &str) -> io::Result<bool> {
    let status = common_command(common)
        .args(["merge-base", "--is-ancestor", commit, head])
        .run(None)?
        .status;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure(format!("can't tell whether {commit} is in {head}"))),
    }
}

/// The commits in `commit`'s history that `base` doesn't have.
pub fn commits_not_on(repo: &Path, commit: &str, base: &str) -> io::Result<BTreeSet<String>> {
    let out = git(repo, &["rev-list", commit, "--not", base, "--"])?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_string)
        .collect())
}

/// Files with unresolved conflicts, when a merge is in progress.
pub fn merge_conflicts(worktree: &Path) -> io::Result<Option<Vec<String>>> {
    if !has_ref(worktree, "MERGE_HEAD") {
        return Ok(None);
    }
    let out = git(worktree, &["diff", "--name-only", "-z", "--diff-filter=U"])?;
    Ok(Some(
        out.split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect(),
    ))
}

/// Writes `path` in the worktree from `commit` (or deletes it if the commit
/// doesn't have it), without touching the index or history. For reading
/// another branch's version; to edit on top of it, use [`stack_on`].
pub fn checkout_file(worktree: &Path, commit: &str, path: &str) -> io::Result<()> {
    let spec = format!("{commit}:{path}");
    let exists = command(worktree)
        .args(["cat-file", "-e", &spec])
        .run(None)?
        .status
        .success();
    if exists {
        git(
            worktree,
            &["restore", "--source", commit, "--worktree", "--", path],
        )
        .map(drop)
    } else {
        match fs::remove_file(worktree.join(path)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Every file under `prefix` on `refname`, with its content hash.
pub fn leaves(repo: &Path, refname: &str, prefix: &str) -> io::Result<BTreeMap<Scope, String>> {
    leaves_cached(repo, refname, prefix, None)
}

/// [`leaves`], taking the hashes of blobs it has seen before from the
/// digest cache at `cache` (see `digest_cache`), and hashing only the rest.
/// The result is the same with or without the cache.
pub fn leaves_cached(
    repo: &Path,
    refname: &str,
    prefix: &str,
    cache: Option<&Path>,
) -> io::Result<BTreeMap<Scope, String>> {
    leaves_with_cache(repo, refname, prefix, cache, false)
}

/// The digest cache beside a coordinator store.
pub fn digest_cache_path(store: &Path) -> Option<PathBuf> {
    store
        .parent()
        .map(|dir| dir.join(crate::digest_cache::FILE))
}

/// Reads warm hashes without creating, repairing or replacing the cache.
pub fn leaves_read_only_cached(
    repo: &Path,
    refname: &str,
    prefix: &str,
    cache: Option<&Path>,
) -> io::Result<BTreeMap<Scope, String>> {
    leaves_with_cache(repo, refname, prefix, cache, true)
}

fn leaves_with_cache(
    repo: &Path,
    refname: &str,
    prefix: &str,
    cache: Option<&Path>,
    read_only: bool,
) -> io::Result<BTreeMap<Scope, String>> {
    // A prefix is a path, never a pathspec: `:(top)team` or `a*` is the
    // folder so named (orchestrator #14475).
    let literal = if prefix.is_empty() {
        String::new()
    } else {
        format!(":(literal){prefix}")
    };
    let listing = git(
        repo,
        &[
            "ls-tree",
            "-r",
            "-l",
            "-z",
            "--full-tree",
            refname,
            "--",
            &literal,
        ],
    )?;
    let mut paths: Vec<(String, String)> = Vec::new();
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let record = String::from_utf8(record.to_vec()).map_err(|e| failure(e.to_string()))?;
        let (meta, path) = record
            .split_once('\t')
            .ok_or_else(|| failure(format!("unexpected ls-tree record {record:?}")))?;
        // `-l` pads the size column with spaces.
        let mut fields = meta.split_whitespace();
        if let (Some(_mode), Some("blob"), Some(oid), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        {
            let size = size
                .parse()
                .map_err(|_| failure(format!("unexpected ls-tree record {record:?}")))?;
            sizes.insert(oid.to_string(), size);
            paths.push((path.to_string(), oid.to_string()));
        }
    }
    // Scopes ignore case, so two files that differ only in case would be one
    // leaf, and one would hide the other. Refuse instead.
    let mut seen: BTreeMap<Scope, &str> = BTreeMap::new();
    for (path, _) in &paths {
        if let Some(other) = seen.insert(Scope::new(path.as_str()), path) {
            return Err(failure(format!(
                "on {refname}, {other} and {path} differ only in case; mdkg treats them as \
                 one file, so rename one with `git mv` on a branch and merge it to {refname}"
            )));
        }
    }
    let wanted: BTreeMap<&str, u64> = sizes
        .iter()
        .map(|(oid, size)| (oid.as_str(), *size))
        .collect();
    let mut cache = cache.and_then(|p| {
        if read_only {
            DigestCache::open_read_only(p)
        } else {
            DigestCache::open(p)
        }
    });
    let (known, rows) = cache
        .as_ref()
        .and_then(|c| c.lookup(&wanted))
        .unwrap_or_default();
    let missing: BTreeSet<&str> = wanted
        .keys()
        .copied()
        .filter(|oid| !known.contains_key(*oid))
        .collect();
    let mut digests = blob_digests(repo, &missing)?;
    digests.extend(known);
    // Rewritten only when main brought new blobs or dropped some.
    if !read_only
        && let Some(cache) = cache.as_mut()
        && (!missing.is_empty() || rows != wanted.len())
    {
        let rows: Vec<(&str, u64, &str)> = wanted
            .iter()
            .filter_map(|(oid, size)| digests.get(*oid).map(|d| (*oid, *size, d.as_str())))
            .collect();
        cache.replace(&rows);
    }
    paths
        .into_iter()
        .map(|(path, oid)| {
            let digest = digests
                .get(&oid)
                .cloned()
                .ok_or_else(|| failure(format!("missing blob {oid}")))?;
            Ok((Scope::new(path), digest))
        })
        .collect()
}

/// Hashes blobs through one `git cat-file --batch`.
fn blob_digests(repo: &Path, oids: &BTreeSet<&str>) -> io::Result<HashMap<String, String>> {
    if oids.is_empty() {
        return Ok(HashMap::new());
    }
    let input: String = oids.iter().map(|oid| format!("{oid}\n")).collect();
    let out = command(repo)
        .args(["cat-file", "--batch"])
        .run(Some(input.as_bytes()))?;
    if !out.status.success() {
        return Err(failure(String::from_utf8_lossy(&out.stderr).to_string()));
    }
    let mut digests = HashMap::new();
    let mut rest = out.stdout.as_slice();
    while !rest.is_empty() {
        let end = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| failure("truncated cat-file header"))?;
        let header = String::from_utf8_lossy(&rest[..end]).to_string();
        let mut fields = header.split(' ');
        let (Some(oid), Some(kind), Some(size)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(failure(format!("unexpected cat-file header {header:?}")));
        };
        if kind != "blob" {
            return Err(failure(format!("{oid} is a {kind}, not a blob")));
        }
        let size: usize = size.parse().map_err(|_| failure("bad blob size"))?;
        let body = rest
            .get(end + 1..end + 1 + size)
            .ok_or_else(|| failure("truncated blob"))?;
        digests.insert(oid.to_string(), Digest::of_bytes(body).to_string());
        rest = rest.get(end + 2 + size..).unwrap_or_default();
    }
    Ok(digests)
}
