//! The git plumbing a migration uses, all through the hardened runner
//! (`mdkg_coord_sqlite::git::command`): no hooks, filters or attributes.
//!
//! The migration commit is built from objects alone (blobs, `mktree`,
//! `commit-tree`), so building it touches neither the index nor the
//! worktree; only `update-ref` makes it visible.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use mdkg_coord_sqlite::git;

fn failure(what: &str, out: &std::process::Output) -> io::Error {
    io::Error::other(format!(
        "git {what} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// Runs git in `dir` and returns its stdout, failing on a non-zero exit.
pub fn run(dir: &Path, args: &[&str], input: Option<&[u8]>) -> io::Result<Vec<u8>> {
    let out = git::command(dir).args(args).run(input)?;
    if !out.status.success() {
        return Err(failure(args.first().copied().unwrap_or("git"), &out));
    }
    Ok(out.stdout)
}

fn line(bytes: Vec<u8>) -> io::Result<String> {
    String::from_utf8(bytes)
        .map(|s| s.trim().to_string())
        .map_err(|_| io::Error::other("git printed something that isn't UTF-8"))
}

/// The commit HEAD names.
pub fn head(dir: &Path) -> io::Result<String> {
    line(run(dir, &["rev-parse", "--verify", "HEAD^{commit}"], None)?)
}

/// Whether the worktree has any change git can see: staged, unstaged or
/// untracked. Ignored files don't count, nor do submodules at other
/// commits: the migration never touches either.
pub fn dirty(dir: &Path) -> io::Result<Vec<String>> {
    dirty_under(dir, None)
}

/// [`dirty`], for the paths under `prefix` only (relative to the top).
pub fn dirty_under(dir: &Path, prefix: Option<&str>) -> io::Result<Vec<String>> {
    let mut args = vec![
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--no-renames",
        "--ignore-submodules=all",
    ];
    let spec = prefix.map(|p| format!(":(top,literal){p}"));
    if let Some(spec) = &spec {
        args.extend(["--", spec.as_str()]);
    }
    let out = run(dir, &args, None)?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|e| e.len() > 3)
        .map(|e| String::from_utf8_lossy(&e[3..]).into_owned())
        .collect())
}

/// Which of `paths` (relative to the top) the ignore rules match, judged
/// from the patterns alone, never from git's index.
pub fn ignored(dir: &Path, paths: &[String]) -> io::Result<BTreeSet<String>> {
    if paths.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut input = paths.join("\0");
    input.push('\0');
    let out = git::command(dir)
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .run(Some(input.as_bytes()))?;
    // 1: nothing is ignored.
    if !(out.status.success() || out.status.code() == Some(1)) {
        return Err(failure("check-ignore", &out));
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .map(|e| String::from_utf8_lossy(e).into_owned())
        .collect())
}

/// The oldest root commit reachable from HEAD, with its commit time in
/// milliseconds: the repository's identity for minting a graph id.
pub fn root_commit(dir: &Path) -> io::Result<(String, u64)> {
    let out = line(run(
        dir,
        &["log", "--max-parents=0", "--format=%ct %H", "HEAD"],
        None,
    )?)?;
    let mut roots: Vec<(u64, String)> = out
        .lines()
        .filter_map(|l| {
            let (t, h) = l.split_once(' ')?;
            Some((t.parse().ok()?, h.to_string()))
        })
        .collect();
    roots.sort();
    let (secs, sha) = roots
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::other("HEAD has no root commit"))?;
    Ok((sha, secs * 1000))
}

/// Every tracked path under `prefixes` (all of them for an empty list),
/// relative to the repository's top.
pub fn tracked(dir: &Path, prefixes: &[String]) -> io::Result<Vec<String>> {
    let mut args = vec!["ls-files", "-z", "--full-name", "--"];
    let specs: Vec<String> = prefixes.iter().map(|p| format!(":(literal){p}")).collect();
    args.extend(specs.iter().map(String::as_str));
    let out = run(dir, &args, None)?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .map(|e| String::from_utf8_lossy(e).into_owned())
        .collect())
}

/// One entry of a tree listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mode: String,
    pub kind: String,
    pub oid: String,
}

/// Every entry of `commit`'s tree, recursively, trees included, by path.
pub fn ls_tree(dir: &Path, commit: &str) -> io::Result<BTreeMap<String, Entry>> {
    let out = run(
        dir,
        &["ls-tree", "-r", "-t", "-z", "--full-tree", commit],
        None,
    )?;
    parse_ls_tree(&out)
}

/// The files under `folders` (paths from the repository's top, taken
/// literally) in `commit`, recursively: what [`ls_tree`] lists there, without
/// reading the rest of the tree.
pub fn ls_tree_under(
    dir: &Path,
    commit: &str,
    folders: &[String],
) -> io::Result<BTreeMap<String, Entry>> {
    let mut args = vec![
        "--literal-pathspecs",
        "ls-tree",
        "-r",
        "-z",
        "--full-tree",
        commit,
        "--",
    ];
    args.extend(folders.iter().map(String::as_str));
    parse_ls_tree(&run(dir, &args, None)?)
}

fn parse_ls_tree(out: &[u8]) -> io::Result<BTreeMap<String, Entry>> {
    let mut entries = BTreeMap::new();
    for e in out.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let text = String::from_utf8_lossy(e);
        let (meta, path) = text
            .split_once('\t')
            .ok_or_else(|| io::Error::other("unexpected ls-tree output"))?;
        let mut parts = meta.split(' ');
        let (Some(mode), Some(kind), Some(oid)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(io::Error::other("unexpected ls-tree output"));
        };
        entries.insert(
            path.to_string(),
            Entry {
                mode: mode.into(),
                kind: kind.into(),
                oid: oid.into(),
            },
        );
    }
    Ok(entries)
}

/// Every local branch's tip commit, by ref name.
pub fn branch_tips(dir: &Path) -> io::Result<Vec<(String, String)>> {
    let out = run(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
        ],
        None,
    )?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| {
            let (name, sha) = l.rsplit_once(' ')?;
            Some((name.to_string(), sha.to_string()))
        })
        .collect())
}

/// The contents of `oids` (blobs), in order, read in one `cat-file` run.
pub fn read_blobs(dir: &Path, oids: &[String]) -> io::Result<Vec<Vec<u8>>> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = oids.join("\n");
    input.push('\n');
    let out = run(dir, &["cat-file", "--batch"], Some(input.as_bytes()))?;
    let mut blobs = Vec::with_capacity(oids.len());
    let mut rest = out.as_slice();
    for _ in oids {
        let nl = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| io::Error::other("cat-file output ended early"))?;
        let header = String::from_utf8_lossy(&rest[..nl]).into_owned();
        rest = &rest[nl + 1..];
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .filter(|_| !header.ends_with(" missing"))
            .ok_or_else(|| io::Error::other(format!("cat-file: {header}")))?;
        if rest.len() < size + 1 {
            return Err(io::Error::other("cat-file output ended early"));
        }
        blobs.push(rest[..size].to_vec());
        rest = &rest[size + 1..];
    }
    Ok(blobs)
}

/// Writes one blob and returns its id.
pub fn write_blob(dir: &Path, content: &[u8]) -> io::Result<String> {
    line(run(
        dir,
        &["hash-object", "-w", "--no-filters", "--stdin"],
        Some(content),
    )?)
}

/// The blob ids of files on disk, as `hash-object` computes them (not
/// written), in order.
pub fn hash_paths(dir: &Path, paths: &[String]) -> io::Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = paths.join("\n");
    input.push('\n');
    let out = run(
        dir,
        &["hash-object", "--no-filters", "--stdin-paths"],
        Some(input.as_bytes()),
    )?;
    let oids: Vec<String> = String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_string)
        .collect();
    if oids.len() != paths.len() {
        return Err(io::Error::other(
            "hash-object printed the wrong number of ids",
        ));
    }
    Ok(oids)
}

/// A file's new state in the commit: its content, or deleted.
pub enum Edit<'a> {
    Write(&'a [u8]),
    Delete,
}

/// Builds the tree of `base` with `edits` applied, and returns its id.
/// Only the folders on an edited path are rebuilt; every other entry,
/// submodules included, keeps its object.
pub fn edited_tree(dir: &Path, base: &str, edits: &[(String, Edit<'_>)]) -> io::Result<String> {
    let mut entries = ls_tree(dir, base)?;
    let mut dirty_dirs: BTreeSet<String> = BTreeSet::new();
    dirty_dirs.insert(String::new());
    for (path, edit) in edits {
        let mut parent = path.as_str();
        while let Some((p, _)) = parent.rsplit_once('/') {
            dirty_dirs.insert(p.to_string());
            parent = p;
        }
        match edit {
            Edit::Write(content) => {
                let oid = write_blob(dir, content)?;
                let mode = entries
                    .get(path)
                    .filter(|e| e.kind == "blob")
                    .map_or("100644".to_string(), |e| e.mode.clone());
                entries.insert(
                    path.clone(),
                    Entry {
                        mode,
                        kind: "blob".into(),
                        oid,
                    },
                );
                // Folders a new file lands in exist from here on.
                let mut parent = path.as_str();
                while let Some((p, _)) = parent.rsplit_once('/') {
                    entries.entry(p.to_string()).or_insert(Entry {
                        mode: "040000".into(),
                        kind: "tree".into(),
                        oid: String::new(),
                    });
                    parent = p;
                }
            }
            Edit::Delete => {
                entries.remove(path);
            }
        }
    }
    // Deepest folders first, so each parent sees its children's new ids.
    let mut order: Vec<String> = dirty_dirs.into_iter().collect();
    order.sort_by_key(|d| {
        std::cmp::Reverse(if d.is_empty() {
            0
        } else {
            d.matches('/').count() + 1
        })
    });
    let mut root = String::new();
    for d in order {
        let prefix = if d.is_empty() {
            String::new()
        } else {
            format!("{d}/")
        };
        let mut listing = Vec::new();
        for (path, e) in entries.range(prefix.clone()..) {
            let Some(name) = path.strip_prefix(&prefix) else {
                break;
            };
            if name.is_empty() || name.contains('/') {
                continue;
            }
            listing
                .extend_from_slice(format!("{} {} {}\t{name}", e.mode, e.kind, e.oid).as_bytes());
            listing.push(0);
        }
        if d.is_empty() {
            root = line(run(dir, &["mktree", "-z"], Some(&listing))?)?;
            continue;
        }
        let oid = if listing.is_empty() {
            None
        } else {
            Some(line(run(dir, &["mktree", "-z"], Some(&listing))?)?)
        };
        match oid {
            Some(oid) => {
                entries.insert(
                    d.clone(),
                    Entry {
                        mode: "040000".into(),
                        kind: "tree".into(),
                        oid,
                    },
                );
            }
            // A folder left empty disappears, as git keeps no empty trees.
            None => {
                entries.remove(&d);
            }
        }
    }
    Ok(root)
}

/// Creates a commit of `tree` on `parent`, with the repository's identity.
pub fn commit_tree(dir: &Path, tree: &str, parent: &str, message: &str) -> io::Result<String> {
    line(run(
        dir,
        &[
            "commit-tree",
            "--no-gpg-sign",
            tree,
            "-p",
            parent,
            "-F",
            "-",
        ],
        Some(message.as_bytes()),
    )?)
}

/// The tree git's index holds, written as an object.
pub fn index_tree(dir: &Path) -> io::Result<String> {
    line(run(dir, &["write-tree"], None)?)
}

/// The tree a commit names.
pub fn tree_of(dir: &Path, commit: &str) -> io::Result<String> {
    line(run(
        dir,
        &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
        None,
    )?)
}

/// Moves HEAD (or the branch it names) from `old` to `new`, only if it
/// still names `old`.
pub fn move_head(dir: &Path, new: &str, old: &str, reason: &str) -> io::Result<()> {
    run(dir, &["update-ref", "-m", reason, "HEAD", new, old], None).map(|_| ())
}

/// Whether git's index has unmerged entries (conflict stages).
pub fn unmerged(dir: &Path) -> io::Result<bool> {
    Ok(!run(dir, &["ls-files", "-u", "-z"], None)?.is_empty())
}

/// Moves the index from `from`'s tree to `to`'s (the worktree's files
/// already match `to`), then refreshes its stat data. An index-only
/// two-tree merge (`read-tree -m -i <from> <to>`): git holds its own index
/// lock throughout, the entries that differ between the trees take `to`'s,
/// and every other entry is carried forward as it is, so someone else's
/// staged change (a submodule pointer), staged before or while this runs,
/// is kept. An unmerged index (a conflicted `git apply --3way`) is refused,
/// its conflict stages untouched. `-i` skips the check against the
/// worktree, whose files the swap already changed.
pub fn reset_index(dir: &Path, from: &str, to: &str) -> io::Result<()> {
    run(dir, &["read-tree", "-m", "-i", from, to], None)?;
    // A refresh that finds changes exits 1; the files are checked after.
    git::command(dir)
        .args(["update-index", "-q", "--refresh"])
        .run(None)?;
    Ok(())
}

/// Creates `refs/tags/<name>` at `commit` if no such tag exists. Returns
/// whether the tag now names `commit`.
pub fn tag(dir: &Path, name: &str, commit: &str) -> io::Result<bool> {
    let refname = format!("refs/tags/{name}");
    let out = git::command(dir)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{refname}^{{commit}}"),
        ])
        .run(None)?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim() == commit);
    }
    // Zero old value: create only.
    let zero = "0".repeat(commit.len());
    run(dir, &["update-ref", &refname, commit, &zero], None)?;
    Ok(true)
}

/// A unified diff of `old` to `new` content for `path`, as git writes it.
pub fn diff(dir: &Path, path: &str, old: &[u8], new: &[u8]) -> io::Result<String> {
    let a = write_blob(dir, old)?;
    let b = write_blob(dir, new)?;
    let out = run(
        dir,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            &a,
            &b,
        ],
        None,
    )?;
    let text = String::from_utf8_lossy(&out).into_owned();
    Ok(text
        .lines()
        .map(|l| {
            if l.starts_with("diff --git ") {
                format!("diff --git a/{path} b/{path}")
            } else if l.starts_with("--- ") {
                format!("--- a/{path}")
            } else if l.starts_with("+++ ") {
                format!("+++ b/{path}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}
