//! Prepare the shared graph checkout and advance it without merging work.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use mdkg_coord_sqlite::git;
use mdkg_format::program::BIN;

use crate::RunError;
use crate::orchestration::Names;

const BRANCH: &str = "mdkg-graph";
const IGNORE: &str = "/.worktrees/";
const CAP: u64 = 1 << 20;
const MAX_DECLARATIONS: usize = 1024;
const BATCH_CAP: u64 = 4 * CAP;
const TREE_ARGS_CAP: usize = 16 * 1024;
const CONFIG_PATH_CAP: usize = 4096;
const CONFIG_DEPTH_CAP: usize = 64;

fn refused(why: impl Into<String>) -> RunError {
    RunError::usage("install.unsupported_layout", why)
}

fn primary(root: &Path) -> Result<PathBuf, RunError> {
    known_primary(root)?.ok_or_else(|| refused("the repository has no primary checkout"))
}

fn same_folder(a: &Path, b: &Path) -> Result<bool, RunError> {
    let a = fs::metadata(a).map_err(|e| RunError::io("can't inspect checkout identity", e))?;
    let b = fs::metadata(b).map_err(|e| RunError::io("can't inspect checkout identity", e))?;
    Ok(a.is_dir() && b.is_dir() && (a.dev(), a.ino()) == (b.dev(), b.ino()))
}

/// Separate Git directories need not record a primary checkout. A linked
/// entry at the front of the list must not be mistaken for the missing one.
fn known_primary(root: &Path) -> Result<Option<PathBuf>, RunError> {
    let common =
        git::common_dir(root).map_err(|e| RunError::io("can't inspect caller repository", e))?;
    let Some(candidate) = git::worktrees(root)
        .map_err(|e| RunError::io("can't list worktrees", e))?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    // The usual primary has the common directory as its .git folder. Its
    // identity is enough; preserve the caller's existing Git-check cache.
    if same_folder(&candidate.join(".git"), &common)? {
        return Ok(Some(candidate));
    }
    let checkout = git::check_worktree_with_trust(
        &candidate,
        &git::WorktreeTrust {
            common: Some(common.clone()),
            worktree: Some(candidate.clone()),
        },
    )
    .map_err(|e| RunError::io("can't inspect primary checkout", e))?;
    Ok(same_folder(&checkout.git_dir, &common)?.then_some(candidate))
}

fn plain_folder(path: &Path) -> Result<(), RunError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(refused(format!(
            "{} must be a plain folder",
            path.display()
        ))),
    }
}

fn ignore_file(root: &Path, create: bool) -> Result<Option<File>, RunError> {
    let path = root.join(".gitignore");
    let file = fs::OpenOptions::new()
        .read(true)
        .append(create)
        .create(create)
        .mode(0o644)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(&path);
    let file = match file {
        Ok(f) => f,
        Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RunError::io("can't open .gitignore", e)),
    };
    let m = file
        .metadata()
        .map_err(|e| RunError::io("can't inspect .gitignore", e))?;
    if !m.is_file() || m.nlink() != 1 || m.len() > CAP {
        return Err(refused(
            ".gitignore must be a plain file with one link, at most 1 MiB",
        ));
    }
    Ok(Some(file))
}

/// Add the worktree ignore rule when creating the first worktree, preserving
/// the user's content. A dry run checks the file but creates nothing.
pub fn ignore_worktrees(root: &Path, dry_run: bool) -> Result<(), RunError> {
    let root = primary(root)?;
    let Some(mut file) = ignore_file(&root, !dry_run)? else {
        return Ok(());
    };
    if !dry_run {
        file.lock()
            .map_err(|e| RunError::io("can't lock .gitignore", e))?;
    }
    let mut text = String::new();
    (&mut file)
        .take(CAP + 1)
        .read_to_string(&mut text)
        .map_err(|e| RunError::io("can't read .gitignore", e))?;
    if text.len() as u64 > CAP {
        return Err(refused(".gitignore is over 1 MiB"));
    }
    if dry_run
        || text
            .lines()
            .any(|line| matches!(line.trim_end_matches('\r'), IGNORE | ".worktrees/"))
    {
        return Ok(());
    }
    let sep = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    file.write_all(format!("{sep}{IGNORE}\n").as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| RunError::io("can't update .gitignore", e))
}

fn identity_error(why: impl std::fmt::Display) -> RunError {
    refused(format!("graph checkout identity needs repair: {why}; inspect the clone-local mdkg.graph.<graph_id>.worktree records and rerun {BIN} install from the primary checkout"))
        .with_hint(format!("Run git config --local --get-regexp '^mdkg\\.graph\\..*\\.worktree$'; after verifying the checkout, remove only an incorrect record with git config --local --unset-all <key>, then rerun {BIN} install. No checkout or record is reset automatically."))
}

#[derive(Debug)]
struct Identity {
    id: String,
    admin: String,
}

impl Identity {
    fn key(&self) -> String {
        format!("mdkg.graph.{}.worktree", self.id)
    }
}

/// Pin and bound the physical source before Git parses it. A config linked
/// to another file cannot establish or receive trusted local identity.
fn local_config(root: &Path) -> Result<Vec<u8>, RunError> {
    let common = git::common_dir(root).map_err(|e| RunError::io("can't inspect repository", e))?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(common.join("config"))
        .map_err(|e| identity_error(format!("cannot open local Git config: {e}")))?;
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.nlink() == 1 && m.len() <= CAP)
    {
        return Err(identity_error(
            "local Git config must be a plain file with one link, at most 1 MiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take(CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| identity_error(format!("cannot read local Git config: {e}")))?;
    if bytes.len() as u64 > CAP {
        return Err(identity_error("local Git config is over 1 MiB"));
    }
    Ok(bytes)
}

/// Only the clone's common config establishes identity. Parse one checked
/// snapshot without includes, global config or config.worktree overrides.
fn identities(root: &Path) -> Result<Vec<Identity>, RunError> {
    let bytes = local_config(root)?;
    let out = git::command(root)
        .args(["config", "--file", "-", "--no-includes", "--null", "--list"])
        .run(Some(&bytes))
        .map_err(|e| identity_error(format!("cannot parse local Git config: {e}")))?;
    if !out.status.success() {
        return Err(identity_error("cannot parse local Git config"));
    }
    let mut records = Vec::new();
    for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let mut fields = entry.splitn(2, |b| *b == b'\n');
        let key = fields.next().unwrap_or_default();
        let Some(id) = key
            .strip_prefix(b"mdkg.graph.")
            .and_then(|s| s.strip_suffix(b".worktree"))
        else {
            continue;
        };
        let id = std::str::from_utf8(id)
            .map_err(|_| identity_error("graph identity key must be UTF-8"))?;
        let value = fields.next().filter(|v| !v.is_empty()).ok_or_else(|| {
            identity_error(format!(
                "mdkg.graph.{id}.worktree requires a non-empty worktree admin name"
            ))
        })?;
        let admin = std::str::from_utf8(value)
            .map_err(|_| identity_error("graph admin name must be UTF-8"))?;
        records.push(Identity {
            id: id.into(),
            admin: admin.into(),
        });
    }
    Ok(records)
}

fn admin_name(checkout: &git::Checkout) -> Result<String, RunError> {
    if checkout.git_dir.parent() != Some(checkout.common.join("worktrees").as_path()) {
        return Err(identity_error("the graph must be a linked worktree"));
    }
    checkout
        .git_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .ok_or_else(|| identity_error("the worktree admin name must be UTF-8"))
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntry {
    mode: String,
    kind: String,
    object: String,
}

type ConfigTree = BTreeMap<Vec<u8>, TreeEntry>;

/// Exact, prefix-free pathspecs keep ls-tree from listing unrelated children.
/// Bound both argument size and ancestor amplification before invoking Git.
fn config_paths(paths: &[PathBuf]) -> Result<BTreeSet<PathBuf>, RunError> {
    use std::os::unix::ffi::OsStrExt as _;
    let paths: BTreeSet<_> = paths.iter().cloned().collect();
    for path in &paths {
        if path.as_os_str().as_bytes().len() > CONFIG_PATH_CAP
            || path.components().count() > CONFIG_DEPTH_CAP
        {
            return Err(identity_error(format!(
                "declared config paths must fit {CONFIG_PATH_CAP} bytes and {CONFIG_DEPTH_CAP} components"
            )));
        }
        if path
            .ancestors()
            .skip(1)
            .any(|parent| paths.contains(parent))
        {
            return Err(identity_error(
                "declared config paths must not contain one another",
            ));
        }
    }
    Ok(paths)
}

fn tree_entries(
    root: &Path,
    revision: &str,
    paths: &BTreeSet<PathBuf>,
) -> Result<ConfigTree, RunError> {
    use std::os::unix::ffi::OsStrExt as _;
    let paths: Vec<_> = paths.iter().collect();
    let mut tree = BTreeMap::new();
    let mut start = 0;
    while start < paths.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < paths.len() {
            let size = paths[end].as_os_str().as_bytes().len() + 1;
            if bytes + size > TREE_ARGS_CAP {
                break;
            }
            bytes += size;
            end += 1;
        }
        let out = git::command(root)
            .args([
                "--literal-pathspecs",
                "ls-tree",
                "-t",
                "-z",
                "--full-tree",
                revision,
                "--",
            ])
            .args(&paths[start..end])
            .run(None)
            .map_err(|e| RunError::io("can't list committed graph configs", e))?;
        if !out.status.success() {
            return Err(identity_error("cannot list committed graph configs"));
        }
        for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let at = entry
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| identity_error("invalid graph tree entry"))?;
            let fields: Vec<_> = std::str::from_utf8(&entry[..at])
                .map_err(|_| identity_error("invalid graph tree entry"))?
                .split_ascii_whitespace()
                .collect();
            if fields.len() != 3
                || !matches!(fields[2].len(), 40 | 64)
                || !fields[2].bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(identity_error("invalid graph tree entry"));
            }
            let item = TreeEntry {
                mode: fields[0].into(),
                kind: fields[1].into(),
                object: fields[2].into(),
            };
            let key = entry[at + 1..].to_vec();
            if let Some(previous) = tree.get(&key)
                && previous != &item
            {
                return Err(identity_error("conflicting graph tree entries"));
            }
            tree.insert(key, item);
        }
        start = end;
    }
    Ok(tree)
}

/// Query declared paths only, against one pinned tree. An exact descendant
/// query omits a blocking symlink/gitlink, so also query each first missing
/// prefix. These prefixes cannot be existing directories in this snapshot.
fn config_tree(root: &Path, revision: &str, paths: &[PathBuf]) -> Result<ConfigTree, RunError> {
    use std::os::unix::ffi::OsStrExt as _;
    let paths = config_paths(paths)?;
    let mut tree = tree_entries(root, revision, &paths)?;
    let mut missing = BTreeSet::new();
    for path in &paths {
        let mut prefix = PathBuf::new();
        for part in path.components() {
            prefix.push(part);
            if !tree.contains_key(prefix.as_os_str().as_bytes()) {
                missing.insert(prefix);
                break;
            }
        }
    }
    let missing = config_paths(&missing.into_iter().collect::<Vec<_>>())?;
    tree.extend(tree_entries(root, revision, &missing)?);
    Ok(tree)
}

fn config_object<'a>(tree: &'a ConfigTree, path: &Path) -> Result<Option<&'a str>, RunError> {
    use std::os::unix::ffi::OsStrExt as _;
    for parent in path
        .ancestors()
        .skip(1)
        .filter(|p| !p.as_os_str().is_empty())
    {
        if let Some(entry) = tree.get(parent.as_os_str().as_bytes())
            && (entry.mode != "040000" || entry.kind != "tree")
        {
            return Err(identity_error(format!(
                "committed {} must be a directory to serve declared {}",
                parent.display(),
                path.display()
            )));
        }
    }
    let Some(entry) = tree.get(path.as_os_str().as_bytes()) else {
        return Ok(None);
    };
    if entry.kind != "blob" || !matches!(entry.mode.as_str(), "100644" | "100755") {
        return Err(identity_error(format!(
            "committed {} must be a regular config file",
            path.display()
        )));
    }
    Ok(Some(&entry.object))
}

fn config_header(line: &[u8], object: &str) -> Result<u64, RunError> {
    let fields: Vec<_> = line.split(|b| *b == b' ').collect();
    if fields.len() != 3 || fields[0] != object.as_bytes() || fields[1] != b"blob" {
        return Err(identity_error("cannot read graph config blob metadata"));
    }
    std::str::from_utf8(fields[2])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| identity_error("cannot read graph config blob size"))
}

/// Bound blobs before reading them. Batch only immutable object IDs, never
/// paths (which may contain newlines); deduplicate objects and keep each
/// buffered batch below 4 MiB plus headers. Unselected blobs stay unread.
fn committed_configs(
    root: &Path,
    tree: &ConfigTree,
    paths: &[PathBuf],
    mut visit: impl FnMut(&Path, &serde_json::Value) -> Result<(), RunError>,
) -> Result<(), RunError> {
    let mut objects: BTreeMap<String, Vec<&Path>> = BTreeMap::new();
    for path in paths {
        if let Some(object) = config_object(tree, path)? {
            objects.entry(object.into()).or_default().push(path);
        }
    }
    if objects.is_empty() {
        return Ok(());
    }
    let request = objects
        .keys()
        .map(|id| format!("{id}\n"))
        .collect::<String>();
    let sizes = git::command(root)
        .args(["cat-file", "--batch-check"])
        .run(Some(request.as_bytes()))
        .map_err(|e| RunError::io("can't inspect config blobs", e))?;
    if !sizes.status.success() || !sizes.stdout.ends_with(b"\n") {
        return Err(identity_error("cannot read graph config blob sizes"));
    }
    let lines: Vec<_> = sizes.stdout[..sizes.stdout.len() - 1]
        .split(|b| *b == b'\n')
        .collect();
    if lines.len() != objects.len() {
        return Err(identity_error("incomplete graph config blob sizes"));
    }
    let mut checked = Vec::new();
    for ((object, paths), line) in objects.iter().zip(lines) {
        let size = config_header(line, object)?;
        if size > CAP {
            return Err(identity_error(format!(
                "committed {} is over 1 MiB",
                paths[0].display()
            )));
        }
        checked.push((object, paths, size));
    }
    let mut start = 0;
    while start < checked.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < checked.len() && bytes + checked[end].2 <= BATCH_CAP {
            bytes += checked[end].2;
            end += 1;
        }
        let batch = &checked[start..end];
        let request = batch
            .iter()
            .map(|(id, _, _)| format!("{id}\n"))
            .collect::<String>();
        let out = git::command(root)
            .args(["cat-file", "--batch"])
            .run(Some(request.as_bytes()))
            .map_err(|e| RunError::io("can't read config blobs", e))?;
        if !out.status.success() {
            return Err(identity_error("cannot read graph config blobs"));
        }
        let mut rest = out.stdout.as_slice();
        for (object, paths, size) in batch {
            let at = rest
                .iter()
                .position(|b| *b == b'\n')
                .ok_or_else(|| identity_error("incomplete graph config blob header"))?;
            if config_header(&rest[..at], object)? != *size {
                return Err(identity_error("graph config blob size changed"));
            }
            rest = &rest[at + 1..];
            let size = *size as usize;
            if rest.len() <= size || rest[size] != b'\n' {
                return Err(identity_error("incomplete graph config blob"));
            }
            let config: serde_json::Value =
                serde_json::from_slice(&rest[..size]).map_err(|_| {
                    identity_error(format!(
                        "committed {} is not valid JSON",
                        paths[0].display()
                    ))
                })?;
            if !config.is_object() {
                return Err(identity_error(format!(
                    "committed {} must be a JSON object",
                    paths[0].display()
                )));
            }
            for path in paths.iter() {
                visit(path, &config)?;
            }
            rest = &rest[size + 1..];
        }
        if !rest.is_empty() {
            return Err(identity_error("unexpected graph config blob output"));
        }
        start = end;
    }
    Ok(())
}

/// Match config workspace path spelling without reading the working copy.
/// Subgraphs may name external paths; those have no entry in this tree.
fn contained_declaration(value: &str) -> Option<PathBuf> {
    let value = mdkg_format::js::trim(value);
    let bytes = value.as_bytes();
    if value.is_empty()
        || value.contains('\0')
        || value.starts_with(['/', '\\'])
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
    {
        return None;
    }
    if value.split(['/', '\\']).any(|part| part == "..") {
        return None;
    }
    // Graph readers use Unix paths verbatim: backslashes are validated as
    // separators for containment but are literal directory-name characters.
    Some(
        Path::new(value)
            .components()
            .filter(|part| *part != std::path::Component::CurDir)
            .collect(),
    )
}

fn declared_configs(config: &serde_json::Value) -> Result<Vec<PathBuf>, RunError> {
    let count = 1 + ["workspaces", "subgraphs"]
        .iter()
        .filter_map(|name| config.get(name).and_then(|v| v.as_object()))
        .map(|entries| entries.len())
        .sum::<usize>();
    if count > MAX_DECLARATIONS {
        return Err(identity_error(format!(
            "at most {MAX_DECLARATIONS} graph declarations are supported, including the root"
        )));
    }
    let mut paths = vec![PathBuf::from(mdkg_graph::config::CONFIG_PATH)];
    if let Some(workspaces) = config.get("workspaces") {
        let workspaces = workspaces
            .as_object()
            .ok_or_else(|| identity_error("committed workspaces must be an object"))?;
        for (alias, entry) in workspaces {
            if !entry.is_object() {
                return Err(identity_error(format!(
                    "committed workspaces.{alias} must be an object"
                )));
            }
            let path = entry
                .get("path")
                .and_then(|v| v.as_str())
                .or_else(|| (alias == "root" && entry.get("path").is_none()).then_some("."))
                .and_then(contained_declaration);
            let folder = match entry.get("mdkg_dir") {
                None => Some(PathBuf::from(".mdkg")),
                Some(value) => value.as_str().and_then(contained_declaration),
            };
            let (Some(path), Some(folder)) = (path, folder) else {
                return Err(identity_error(format!(
                    "committed workspaces.{alias} requires contained path and mdkg_dir values"
                )));
            };
            paths.push(path.join(folder).join("config.json"));
        }
    }
    if let Some(subgraphs) = config.get("subgraphs") {
        let subgraphs = subgraphs
            .as_object()
            .ok_or_else(|| identity_error("committed subgraphs must be an object"))?;
        for (alias, entry) in subgraphs {
            match entry.get("source_path") {
                None => (), // An external reference need not have a local source.
                Some(serde_json::Value::String(value)) => {
                    if let Some(path) = contained_declaration(value) {
                        paths.push(path.join(mdkg_graph::config::CONFIG_PATH));
                    }
                }
                Some(_) => {
                    return Err(identity_error(format!(
                        "committed subgraphs.{alias}.source_path must be a string"
                    )));
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The root and only its declared workspace/subgraph configs establish
/// identity. Missing child configs are document workspaces or external graphs,
/// not separate graphs served by this repository's checkout.
fn graph_ids(root: &Path) -> Result<Vec<String>, RunError> {
    let revision = output(root, &["rev-parse", "--verify", "HEAD^{tree}"])?;
    let revision = String::from_utf8(revision.stdout)
        .ok()
        .filter(|_| revision.status.success())
        .ok_or_else(|| identity_error("cannot pin the graph config tree"))?;
    let revision = revision.trim();
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(identity_error("invalid graph config tree identity"));
    }
    let root_path = Path::new(mdkg_graph::config::CONFIG_PATH);
    let tree = config_tree(root, revision, &[root_path.to_owned()])?;
    let mut root_config = None;
    committed_configs(root, &tree, &[root_path.to_owned()], |_, config| {
        root_config = Some(config.clone());
        Ok(())
    })?;
    let root_config =
        root_config.ok_or_else(|| identity_error("the checkout has no committed root graph"))?;
    let mut ids = Vec::new();
    let paths = declared_configs(&root_config)?;
    let tree = config_tree(root, revision, &paths)?;
    let mut visit = |path: &Path, config: &serde_json::Value| {
        let v07 = config.get("schema_version").and_then(|v| v.as_f64())
            == Some(mdkg_graph::config::V07_SCHEMA_VERSION as f64);
        let legacy = config.get("schema_version").is_none_or(|v| {
            v.as_f64().is_some_and(|n| {
                n >= 0.0 && n.fract() == 0.0 && n < mdkg_graph::config::V07_SCHEMA_VERSION as f64
            })
        });
        let id = (v07
            && config.get("format_version").and_then(|v| v.as_str())
                == Some(mdkg_graph::config::FORMAT_VERSION_07))
        .then(|| config.get("graph_id").and_then(|v| v.as_str()))
        .flatten()
        .filter(|id| mdkg_format::Uuid::parse(id).is_ok());
        match id {
            Some(id) if !ids.iter().any(|existing| existing == id) => ids.push(id.to_owned()),
            Some(_) => (),
            None if path == root_path
                || !legacy
                || config.get("format_version").is_some()
                || config.get("graph_id").is_some() =>
            {
                let hint = if path == root_path {
                    format!(
                        "If this graph is unmigrated, run {BIN} migrate --dry-run from its primary \
                         checkout, review the plan, then {BIN} migrate --apply <plan-hash>. If it \
                         was already migrated, restore its valid committed graph identity. Ensure \
                         any existing graph checkout contains that valid identity commit before \
                         rerunning {BIN} install from the primary checkout. No checkout or \
                         identity record is reset automatically."
                    )
                } else {
                    format!(
                        "Restore the correct {} graph identity in {} and commit any correction. \
                         Ensure any existing graph checkout contains that valid identity commit before \
                         rerunning {BIN} install from the primary checkout. No checkout or \
                         identity record is reset automatically.",
                        mdkg_graph::config::FORMAT_VERSION_07,
                        path.display()
                    )
                };
                return Err(refused(format!(
                    "graph checkout identity needs repair: committed {} has no valid {} graph identity",
                    path.display(),
                    mdkg_graph::config::FORMAT_VERSION_07
                ))
                .with_hint(hint));
            }
            None => (), // A declared legacy document workspace has no 0.7 identity.
        }
        Ok(())
    };
    visit(root_path, &root_config)?;
    let children: Vec<_> = paths.into_iter().filter(|path| path != root_path).collect();
    committed_configs(root, &tree, &children, visit)?;
    Ok(ids)
}

fn registered_checkout(root: &Path, records: &[Identity]) -> Result<PathBuf, RunError> {
    let common = git::common_dir(root).map_err(|e| RunError::io("can't inspect repository", e))?;
    let first = records
        .first()
        .ok_or_else(|| identity_error("missing identity record"))?;
    for record in records {
        let mut components = Path::new(&record.admin).components();
        if mdkg_format::Uuid::parse(&record.id).is_err()
            || !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
            || record.admin != first.admin
            || records.iter().filter(|r| r.id == record.id).count() != 1
        {
            return Err(identity_error(format!(
                "invalid or conflicting record {}",
                record.key()
            )));
        }
    }
    let admin = common.join("worktrees").join(&first.admin);
    plain_folder(&common.join("worktrees"))?;
    plain_folder(&admin)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(admin.join("gitdir"))
        .map_err(|e| identity_error(format!("cannot read {} registration: {e}", first.admin)))?;
    if !file.metadata().is_ok_and(|m| m.is_file() && m.len() <= CAP) {
        return Err(identity_error(
            "registration must be a plain file at most 1 MiB",
        ));
    }
    let mut back = String::new();
    file.take(CAP + 1)
        .read_to_string(&mut back)
        .map_err(|e| identity_error(format!("cannot read registration: {e}")))?;
    if back.len() as u64 > CAP {
        return Err(identity_error("registration is over 1 MiB"));
    }
    let back = admin.join(back.trim_end_matches(['\n', '\r']));
    if back.file_name().is_none_or(|name| name != ".git") {
        return Err(identity_error(
            "the worktree registration must name its .git file",
        ));
    }
    let path = back
        .parent()
        .ok_or_else(|| identity_error("invalid worktree registration"))?;
    // A registered path may move, but may not redirect through a symlink.
    for ancestor in path.ancestors() {
        plain_folder(ancestor)?;
    }
    let checkout = git::check_worktree_with_trust(
        path,
        &git::WorktreeTrust {
            common: Some(common),
            worktree: Some(path.into()),
        },
    )
    .map_err(|e| {
        identity_error(format!(
            "worktree {} does not match its registration: {e}",
            first.admin
        ))
    })?;
    if admin_name(&checkout)? != first.admin || !same_folder(&admin, &checkout.git_dir)? {
        return Err(identity_error(
            "the record points at a different worktree admin directory",
        ));
    }
    Ok(checkout.top)
}

fn check_identities(
    root: &Path,
    records: &[Identity],
    allow_missing: bool,
) -> Result<(), RunError> {
    let ids = graph_ids(root)?;
    let checkout = git::check_worktree(root).map_err(|e| RunError::io("can't inspect graph", e))?;
    let admin = admin_name(&checkout)?;
    // Guidance must not make a valid identity depend on an available primary.
    // Resolve its path only when a record actually needs repair.
    let install = || match known_primary(root) {
        Ok(Some(primary)) => format!(
            "{} --root {} install",
            super::shell_word(BIN.as_str()),
            super::shell_word(&primary.to_string_lossy())
        ),
        _ => format!("{BIN} install from the primary checkout"),
    };
    for record in records {
        if record.admin != admin {
            let target = registered_checkout(root, records)?;
            return Err(identity_error(format!(
                "{} names worktree admin {}, not caller admin {admin}", record.key(), record.admin
            )).with_hint(format!(
                "This record belongs to the registered checkout {}. Run from that checkout instead: {} --root {} doctor. Preserve the record while verifying its registration.",
                target.display(), super::shell_word(BIN.as_str()), super::shell_word(&target.to_string_lossy())
            )));
        }
        if !ids.contains(&record.id) {
            return Err(identity_error(format!(
                "{} does not identify this graph checkout",
                record.key()
            )).with_hint(format!(
                "After verifying this record is incorrect or its graph was removed, remove only this key: git -C {} config --local --unset-all {}; then run {}. No checkout or record is reset automatically.",
                super::shell_word(&root.to_string_lossy()), super::shell_word(&record.key()), install()
            )));
        }
    }
    if !allow_missing {
        for id in ids {
            if !records.iter().any(|r| r.id == id) {
                return Err(identity_error(format!("missing mdkg.graph.{id}.worktree"))
                    .with_hint(format!("Register the committed graphs from the primary checkout: {}. Existing conflicting records are never overwritten.", install())));
            }
        }
    }
    Ok(())
}

fn record_identities(root: &Path) -> Result<(), RunError> {
    let records = identities(root)?;
    check_identities(root, &records, true)?;
    let checkout = git::check_worktree(root).map_err(|e| RunError::io("can't inspect graph", e))?;
    let admin = admin_name(&checkout)?;
    for id in graph_ids(root)? {
        if !records.iter().any(|record| record.id == id) {
            let key = format!("mdkg.graph.{id}.worktree");
            // Recheck the physical publication target for each Git update.
            local_config(root)?;
            if !output(
                root,
                &["config", "--local", "--no-includes", "--add", &key, &admin],
            )?
            .status
            .success()
            {
                return Err(identity_error(format!("cannot write {key}")));
            }
        }
    }
    Ok(())
}

/// A checked install plan. Only `create` changes the repository.
#[derive(Debug)]
pub struct GraphPlan {
    pub path: PathBuf,
    root: PathBuf,
    common: PathBuf,
    main_ref: String,
    no_graph_commit: bool,
}

impl GraphPlan {
    pub fn inspect(root: &Path, main_ref: &str) -> Result<Self, RunError> {
        let root = primary(root)?;
        let common =
            git::common_dir(&root).map_err(|e| RunError::io("can't find git folder", e))?;
        let records = identities(&root)?;
        let path = if records.is_empty() {
            plain_folder(&root.join(".worktrees"))?;
            root.join(".worktrees/graph")
        } else {
            registered_checkout(&root, &records)?
        };
        plain_folder(&path)?;
        let main_ref = main_ref.strip_prefix("refs/heads/").unwrap_or(main_ref);
        if main_ref == BRANCH {
            return Err(refused("mdkg-graph cannot also be the main branch"));
        }
        let unborn = !git::has_ref(&root, &format!("refs/heads/{main_ref}"));
        if unborn && !git::no_commits(&root).map_err(|e| RunError::io("can't inspect HEAD", e))? {
            return Err(refused(format!("main branch {main_ref} does not exist")));
        }
        let no_graph_commit = unborn
            || !output(
                &root,
                &[
                    "cat-file",
                    "-e",
                    &format!("refs/heads/{main_ref}:.mdkg/config.json"),
                ],
            )?
            .status
            .success();
        if path.exists() {
            let checkout = git::check_worktree_with_trust(
                &path,
                &git::WorktreeTrust {
                    common: Some(common.clone()),
                    worktree: Some(path.clone()),
                },
            )
            .map_err(|e| RunError::io("can't check graph worktree", e))?;
            if !same_folder(&checkout.top, &path)? {
                return Err(identity_error(
                    "the graph must be the exact registered checkout",
                ));
            }
            git::with_worktree_trust(
                &git::WorktreeTrust {
                    common: Some(common.clone()),
                    worktree: Some(path.clone()),
                },
                || {
                    check_identities(&path, &records, true)?;
                    if let Some(status) = repair_checkout(path.clone(), &common)? {
                        return Err(status.error());
                    }
                    Ok(())
                },
            )?;
        } else {
            crate::turn::plan_agent_worktree(&common, "graph", main_ref, &names()?, &[])?;
            ignore_worktrees(&root, true)?;
        }
        Ok(Self {
            path,
            root,
            common,
            main_ref: main_ref.into(),
            no_graph_commit,
        })
    }

    /// Create the graph checkout on its own branch. Undo never removes a
    /// worktree or branch, since either may contain subsequent user work.
    pub fn create(&self, dry_run: bool) -> Result<String, RunError> {
        let _lock = if dry_run {
            None
        } else {
            Some(graph_lock(&self.common)?)
        };
        // Recheck after the lock: another installer may have made it, or
        // main may have received its first commit since planning.
        let plan = Self::inspect(&self.root, &self.main_ref)?;
        if plan.no_graph_commit {
            return Ok(
                "graph worktree deferred: commit the initial graph, then run install again".into(),
            );
        }
        if plan.path.exists() {
            if !dry_run {
                git::with_worktree_trust(
                    &git::WorktreeTrust {
                        common: Some(plan.common.clone()),
                        worktree: Some(plan.path.clone()),
                    },
                    || record_identities(&plan.path),
                )?;
            }
            drop(_lock);
            return plan.with_refresh_note(
                format!("graph worktree: {} (kept)", plan.path.display()),
                dry_run,
            );
        }
        if dry_run {
            return Ok(format!(
                "would create graph worktree: {} on {BRANCH}",
                self.path.display()
            ));
        }
        ignore_worktrees(&self.root, false)?;
        crate::turn::agent_worktree(&self.common, "graph", &self.main_ref, &names()?)?;
        git::with_worktree_trust(
            &git::WorktreeTrust {
                common: Some(plan.common.clone()),
                worktree: Some(plan.path.clone()),
            },
            || record_identities(&plan.path),
        )?;
        drop(_lock);
        plan.with_refresh_note(
            format!(
                "graph worktree: {} on {BRANCH}; launch with {} --root {} agents up",
                plan.path.display(),
                super::shell_word(BIN.as_str()),
                super::shell_word(&plan.path.to_string_lossy())
            ),
            false,
        )
    }

    fn with_refresh_note(&self, mut note: String, dry_run: bool) -> Result<String, RunError> {
        let sync = git::with_worktree_trust(
            &git::WorktreeTrust {
                common: Some(self.common.clone()),
                worktree: Some(self.path.clone()),
            },
            || {
                let sync = refresh_graph(&self.path, &self.main_ref, dry_run);
                // Recheck before projections can be published. Only install
                // may add the identity of a graph brought in by this sync.
                let _lock = if dry_run {
                    None
                } else {
                    Some(graph_lock(&self.common)?)
                };
                if let Some(status) = repair_checkout(self.path.clone(), &self.common)? {
                    return Err(status.error());
                }
                if !dry_run {
                    record_identities(&self.path)?;
                    ensure_graph_writable(&self.path)?;
                } else {
                    check_identities(&self.path, &identities(&self.path)?, true)?;
                }
                Ok(sync)
            },
        )?;
        if let Some(update) = sync.note() {
            note.push_str("; ");
            note.push_str(&update);
        }
        Ok(note)
    }
}

fn names() -> Result<Names, RunError> {
    Names::new("mdkg-{agent}", ".worktrees/{agent}")
}

fn graph_lock(common: &Path) -> Result<File, RunError> {
    let file = graph_lock_file(common)?;
    wait_graph_lock(file, common, std::time::Duration::from_secs(60))
}

fn wait_graph_lock(
    file: File,
    common: &Path,
    timeout: std::time::Duration,
) -> Result<File, RunError> {
    let started = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => (),
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(RunError::io("can't lock graph worktree", error));
            }
        }
        let Some(remaining) = timeout
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
        else {
            return Err(RunError::failure(
                "run.io_failed",
                format!(
                    "graph checkout busy: timed out waiting for {}",
                    common.join("mdkg/graph-worktree.lock").display()
                ),
            )
            .with_hint(
                "Let the active graph writer finish, then retry. Do not remove the lock file.",
            ));
        };
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(50)));
    }
}

fn graph_lock_file(common: &Path) -> Result<File, RunError> {
    let folder = common.join("mdkg");
    fs::create_dir_all(&folder).map_err(|e| RunError::io("can't create graph lock folder", e))?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(folder.join("graph-worktree.lock"))
        .map_err(|e| RunError::io("can't open graph lock", e))?;
    if !file.metadata().is_ok_and(|m| m.is_file() && m.nlink() == 1) {
        return Err(refused("graph lock must be a plain file with one link"));
    }
    Ok(file)
}

/// Holds off graph sync for the entire read/modify/commit operation. CLI
/// dispatch owns this guard; MCP mutations dispatch through the same CLI.
#[must_use = "keep this guard until the graph operation has finished"]
#[derive(Debug)]
pub struct GraphWriteGuard {
    _lock: File,
}

pub fn lock_graph_writer(root: &Path) -> Result<Option<GraphWriteGuard>, RunError> {
    if !is_graph_checkout(root)? {
        return Ok(None);
    }
    // Preserve immediate freeze diagnostics without creating lock state.
    ensure_graph_writable(root)?;
    let common = git::common_dir(root).map_err(|e| RunError::io("can't find git folder", e))?;
    let lock = graph_lock(&common)?;
    // Sync may have changed the checkout while this writer waited.
    ensure_graph_writable(root)?;
    Ok(Some(GraphWriteGuard { _lock: lock }))
}

/// Result carried into a turn receipt. A stale graph does not block work.
#[derive(Debug, Clone, Default)]
pub struct GraphSync {
    pub head: Option<String>,
    pub updated: bool,
    pub reason: Option<String>,
}

impl GraphSync {
    /// Human-readable outcome for install notes or a supervisor's reporting
    /// channel. An up-to-date or unrelated checkout has nothing to report.
    pub fn note(&self) -> Option<String> {
        self.reason.clone().or_else(|| {
            self.updated
                .then(|| format!("graph synced to {}", self.head.as_deref().unwrap_or("HEAD")))
        })
    }
}

/// Best-effort automatic refresh for startup and between settled turns.
/// Callers report `note()` without ending an already-running supervisor.
/// Identity and freeze errors still stop this update, and retain their repair
/// guidance. Actual graph writers continue to use the strict writable gate.
pub fn refresh_graph(root: &Path, main_ref: &str, dry_run: bool) -> GraphSync {
    match sync_graph(root, main_ref, dry_run) {
        Ok(sync) => sync,
        Err(error) => GraphSync {
            reason: Some(format!(
                "graph not synced: {}{}",
                error.message,
                error.hint.map_or(String::new(), |hint| format!("; {hint}"))
            )),
            ..GraphSync::default()
        },
    }
}

fn output(root: &Path, args: &[&str]) -> Result<std::process::Output, RunError> {
    git::command(root)
        .args(args)
        .run(None)
        .map_err(|e| RunError::io("can't inspect graph", e))
}

/// Read the caller's own linked-worktree registration without following the
/// graph slot, which may alias an unrelated checkout or be unresolvable.
fn registered_graph(checkout: &git::Checkout, primary: &Path) -> Result<bool, RunError> {
    if checkout.git_dir == checkout.common {
        // An unpinned .git entry may lend the common directory to a different
        // folder. Only the actual primary gets the non-graph exemption.
        let expected = fs::metadata(primary)
            .map_err(|e| RunError::io("can't identify primary checkout", e))?;
        let actual = fs::metadata(&checkout.top)
            .map_err(|e| RunError::io("can't identify common-directory caller", e))?;
        if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
            // A separate Git directory named .git makes Git infer its parent
            // as primary even when the caller is an independent checkout.
            // Require graph identity before treating that caller as damaged.
            match fs::metadata(primary.join(".worktrees/graph")) {
                Ok(graph) if (graph.dev(), graph.ino()) != (actual.dev(), actual.ino()) => {
                    return Ok(false);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(false);
                }
                _ => (),
            }
            return Err(refused(
                "a checkout using the common Git directory must be the primary checkout; repair its .git entry before writing",
            ));
        }
        return Ok(false);
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(checkout.git_dir.join("gitdir"))
        .map_err(|e| RunError::io("can't read caller worktree registration", e))?;
    if !file.metadata().is_ok_and(|m| m.is_file() && m.len() <= CAP) {
        return Err(refused(
            "caller worktree registration must be a plain file at most 1 MiB",
        ));
    }
    let mut back = String::new();
    file.take(CAP + 1)
        .read_to_string(&mut back)
        .map_err(|e| RunError::io("can't read caller worktree registration", e))?;
    if back.len() as u64 > CAP {
        return Err(refused("caller worktree registration is over 1 MiB"));
    }
    registration_uses_graph(
        &checkout.git_dir.join(back.trim_end_matches(['\n', '\r'])),
        primary,
    )
}

// Resolve registration aliases one component at a time, remembering passage
// through the standard graph slot before following a link there. A final
// canonical path alone loses that information when the slot has been moved.
fn registration_uses_graph(path: &Path, primary: &Path) -> Result<bool, RunError> {
    let primary_meta =
        fs::metadata(primary).map_err(|e| RunError::io("can't inspect primary checkout", e))?;
    let graph = primary.join(".worktrees/graph");
    let mut path = path.to_path_buf();
    for _ in 0..64 {
        let mut at = PathBuf::new();
        let mut parts = path.components();
        let mut redirect = None;
        while let Some(part) = parts.next() {
            match part {
                std::path::Component::ParentDir => {
                    at.pop();
                }
                std::path::Component::CurDir => continue,
                other => at.push(other),
            }
            // Recognize the pending graph destination before a linked
            // .worktrees parent redirects traversal away from its spelling.
            let mut named = PathBuf::new();
            for component in at.join(parts.as_path()).components() {
                match component {
                    std::path::Component::ParentDir => {
                        named.pop();
                    }
                    std::path::Component::CurDir => (),
                    other => named.push(other),
                }
            }
            if named.starts_with(&graph) {
                return Ok(true);
            }
            let meta = fs::symlink_metadata(&at)
                .map_err(|e| RunError::io("can't inspect registration path", e))?;
            if meta.is_symlink() {
                let target = fs::read_link(&at)
                    .map_err(|e| RunError::io("can't read registration alias", e))?;
                redirect = Some(
                    at.parent()
                        .unwrap_or(Path::new("/"))
                        .join(target)
                        .join(parts.as_path()),
                );
                break;
            }
            if meta.is_dir() && (meta.dev(), meta.ino()) == (primary_meta.dev(), primary_meta.ino())
            {
                at = primary.to_path_buf();
            }
        }
        match redirect {
            Some(next) => path = next,
            None => return Ok(false),
        }
    }
    Err(refused("worktree registration has too many aliases"))
}

/// Identify the standard checkout before validating its layout. Canonical
/// path spelling changes when a parent is moved behind a link, so compare
/// the actual folder and Git directory identities instead.
fn unregistered_graph_checkout(root: &Path) -> Result<bool, RunError> {
    let checkout =
        git::check_worktree(root).map_err(|e| RunError::io("can't inspect caller checkout", e))?;
    let Some(primary) = known_primary(root)? else {
        // No managed standard graph can be located without a recorded
        // primary. This checked checkout remains an ordinary writable one.
        return Ok(false);
    };
    let graph = primary.join(".worktrees/graph");
    let plain_slot = fs::symlink_metadata(primary.join(".worktrees")).is_ok_and(|m| m.is_dir())
        && fs::symlink_metadata(&graph).is_ok_and(|m| m.is_dir());
    // Plain slots are identified physically before inspecting registration
    // spelling, including case-only moves on case-insensitive filesystems.
    let physical = plain_slot && same_folder(&checkout.top, &graph)?;
    if !physical && !registered_graph(&checkout, &primary)? {
        // A linked registration changed to an unrelated path must still be
        // reciprocal, not a truncated or rewritten graph backlink.
        if checkout.git_dir != checkout.common {
            git::check_worktree(root)
                .map_err(|e| RunError::io("can't recheck independent checkout", e))?;
        }
        return Ok(false);
    }
    let expected = match fs::metadata(&graph) {
        Ok(meta) => meta,
        Err(e) => return Err(RunError::io("can't identify graph checkout", e)),
    };
    let actual = fs::metadata(&checkout.top)
        .map_err(|e| RunError::io("can't identify caller checkout", e))?;
    if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
        return Err(refused(
            "registered graph checkout changed during inspection",
        ));
    }
    let at_graph = git::check_worktree_with_trust(
        &graph,
        &git::WorktreeTrust {
            common: Some(checkout.common.clone()),
            worktree: Some(checkout.top.clone()),
        },
    )
    .map_err(|e| RunError::io("can't identify graph Git directory", e))?;
    let actual_git = fs::metadata(&checkout.git_dir)
        .map_err(|e| RunError::io("can't identify caller Git directory", e))?;
    let graph_git = fs::metadata(&at_graph.git_dir)
        .map_err(|e| RunError::io("can't identify graph Git directory", e))?;
    if (actual_git.dev(), actual_git.ino()) != (graph_git.dev(), graph_git.ino()) {
        return Err(refused(
            "graph checkout Git directory changed during inspection",
        ));
    }
    Ok(true)
}

/// The durable record identifies managed checkouts even after a move or a
/// branch switch. Legacy layout detection is used only to refuse an absent
/// record, never to authorize a graph operation without one.
fn is_graph_checkout(root: &Path) -> Result<bool, RunError> {
    let checkout =
        git::check_worktree(root).map_err(|e| RunError::io("can't inspect caller checkout", e))?;
    let records = identities(root)?;
    let admin = admin_name(&checkout).ok();
    if records.iter().any(|r| Some(&r.admin) == admin.as_ref()) {
        let graph = registered_checkout(root, &records)?;
        if !same_folder(&graph, &checkout.top)? {
            return Err(identity_error(
                "the registered checkout differs from the caller",
            ));
        }
        check_identities(root, &records, false)?;
        return Ok(true);
    }
    if git::head(root).is_ok_and(|h| h.branch == BRANCH) || unregistered_graph_checkout(root)? {
        return Err(identity_error(
            "missing or mismatched identity record for this checkout",
        ));
    }
    Ok(false)
}

/// Advance the standard graph checkout only when clean and behind main.
/// Other launcher checkouts are left alone. Git owns the ref, index and
/// checkout update; interrupted writes require operator repair, never replay.
pub fn sync_graph(root: &Path, main_ref: &str, dry_run: bool) -> Result<GraphSync, RunError> {
    if !is_graph_checkout(root)? {
        return Ok(GraphSync::default());
    }
    let root = git::toplevel(root).map_err(|e| RunError::io("can't find graph checkout", e))?;
    ensure_graph_writable(&root)?;
    let common = git::common_dir(&root).map_err(|e| RunError::io("can't find git folder", e))?;
    let _lock = if dry_run {
        None
    } else {
        let lock = graph_lock_file(&common)?;
        match lock.try_lock() {
            Ok(()) => Some(lock),
            Err(fs::TryLockError::WouldBlock) => {
                return Ok(GraphSync {
                    reason: Some(
                        "graph not synced: a graph writer or another sync holds the checkout"
                            .into(),
                    ),
                    ..GraphSync::default()
                });
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(RunError::io("can't lock graph worktree", e));
            }
        }
    };
    // A second launcher or an operator may have changed it while we waited.
    ensure_graph_writable(&root)?;
    let old =
        git::commit_of(&root, "HEAD").map_err(|e| RunError::io("can't read graph HEAD", e))?;
    let main = format!(
        "refs/heads/{}",
        main_ref.strip_prefix("refs/heads/").unwrap_or(main_ref)
    );
    let target = git::commit_of(&root, &main).map_err(|e| RunError::io("can't read main", e))?;
    let reason = preflight(&root, &old, &target)?;
    let mut result = GraphSync {
        head: Some(old.clone()),
        ..GraphSync::default()
    };
    if let Some(reason) = reason {
        result.reason = Some(format!("graph not synced: {reason} at {old}"));
    } else if target != old {
        if dry_run {
            result.reason = Some(format!("graph would fast-forward from {old} to {target}"));
        } else if let Some(reason) = advance_graph(&root, &old, &target, |_| ())? {
            result.reason = Some(format!("graph not synced: {reason} at {old}"));
        } else {
            result.head = Some(target);
            result.updated = true;
        }
    }
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdvancePhase {
    BeforeMerge,
    AfterMerge,
}

#[derive(Debug)]
struct Change {
    path: PathBuf,
    old: bool,
    incoming: bool,
}

fn changed_paths(root: &Path, old: &str, target: &str) -> Result<Vec<Change>, RunError> {
    let diff = output(
        root,
        &[
            "diff-tree",
            "-r",
            "--no-commit-id",
            "--no-renames",
            "--raw",
            "--no-abbrev",
            "-z",
            old,
            target,
        ],
    )?;
    if !diff.status.success() {
        return Err(RunError::failure(
            "io.read_failed",
            "can't compute graph sync paths",
        ));
    }
    let mut fields: Vec<&[u8]> = diff.stdout.split(|b| *b == 0).collect();
    if fields.last() == Some(&b"".as_slice()) {
        fields.pop();
    }
    if !fields.len().is_multiple_of(2) {
        return Err(refused("invalid changed-path output"));
    }
    let mut result = Vec::new();
    for pair in fields.chunks_exact(2) {
        let header =
            std::str::from_utf8(pair[0]).map_err(|_| refused("invalid graph tree entry"))?;
        let modes: Vec<&str> = header
            .trim_start_matches(':')
            .split_ascii_whitespace()
            .collect();
        let allowed = ["000000", "100644", "100755", "120000"];
        if modes.len() != 5 || !allowed.contains(&modes[0]) || !allowed.contains(&modes[1]) {
            return Err(refused(
                "graph sync does not change submodules or special tree entries",
            ));
        }
        let path = PathBuf::from(
            std::str::from_utf8(pair[1]).map_err(|_| refused("graph sync paths must be UTF-8"))?,
        );
        if path.components().any(
            |c| !matches!(c, std::path::Component::Normal(n) if !n.eq_ignore_ascii_case(".git")),
        ) || path.as_os_str().is_empty()
        {
            return Err(refused("invalid graph sync path"));
        }
        result.push(Change {
            path,
            old: modes[0] != "000000",
            incoming: modes[1] != "000000",
        });
    }
    Ok(result)
}

fn names_from(output: std::process::Output) -> Result<Vec<PathBuf>, RunError> {
    use std::os::unix::ffi::OsStrExt as _;
    if !output.status.success() {
        return Err(RunError::failure(
            "io.read_failed",
            "can't inspect graph paths",
        ));
    }
    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
        .collect())
}

/// Walk each component with lstat. A missing leaf under a symlink must
/// never be mistaken for an empty incoming path that Git may replace.
fn paths_clear(root: &Path, changes: &[Change]) -> Result<(), String> {
    let tracked = names_from(output(root, &["ls-files", "--cached", "-z"]).map_err(|e| e.message)?)
        .map_err(|e| e.message)?;
    for change in changes.iter().filter(|c| c.incoming) {
        let mut relative = PathBuf::new();
        let mut parts = change.path.components().peekable();
        while let Some(part) = parts.next() {
            relative.push(part);
            let parent = parts.peek().is_some();
            let meta = match fs::symlink_metadata(root.join(&relative)) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => {
                    return Err(format!(
                        "incoming path {} could not be inspected",
                        relative.display()
                    ));
                }
            };
            if parent && !meta.is_dir() {
                return Err(format!(
                    "incoming parent {} is not a plain directory",
                    relative.display()
                ));
            }
            let tracked_here = if parent {
                tracked
                    .iter()
                    .any(|p| p.starts_with(&relative) && p != &relative)
            } else {
                change.old && tracked.contains(&relative) && (meta.is_file() || meta.is_symlink())
            };
            let ignored = git::command(root)
                .args(["check-ignore", "-q", "--"])
                .arg(&relative)
                .run(None)
                .map_err(|e| format!("can't inspect ignored path: {e}"))?;
            if !matches!(ignored.status.code(), Some(0 | 1)) {
                return Err("can't inspect ignored incoming paths".into());
            }
            if !tracked_here || ignored.status.success() {
                return Err(format!(
                    "incoming {} {} is existing ignored or untracked content",
                    if parent { "parent" } else { "path" },
                    relative.display()
                ));
            }
        }
    }
    Ok(())
}

fn expected_head(root: &Path, commit: &str) -> Result<bool, RunError> {
    let symbolic = output(root, &["symbolic-ref", "-q", "HEAD"])?;
    Ok(symbolic.status.success()
        && String::from_utf8_lossy(&symbolic.stdout).trim() == "refs/heads/mdkg-graph"
        && git::commit_of(root, "HEAD").is_ok_and(|head| head == commit))
}

fn preflight(root: &Path, old: &str, target: &str) -> Result<Option<String>, RunError> {
    ensure_graph_writable(root)?;
    if !expected_head(root, old)? {
        return Ok(Some("branch or HEAD changed before fast-forward".into()));
    }
    let status = output(
        root,
        &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
    )?;
    if !status.status.success() {
        return Ok(Some("status unavailable".into()));
    }
    if !status.stdout.is_empty() {
        return Ok(Some("dirty".into()));
    }
    let ancestor = output(root, &["merge-base", "--is-ancestor", old, target])?;
    if !ancestor.status.success() {
        return Ok(Some(
            if ancestor.status.code() == Some(1) {
                "diverged"
            } else {
                "ancestry unavailable"
            }
            .into(),
        ));
    }
    let sparse = output(root, &["config", "--bool", "core.sparseCheckout"])?;
    if sparse.status.success() && String::from_utf8_lossy(&sparse.stdout).trim() == "true" {
        return Ok(Some(
            "fast-forward unavailable: sparse graph checkout requires a manual update".into(),
        ));
    }
    let changes = match changed_paths(root, old, target) {
        Ok(changes) => changes,
        Err(e) => return Ok(Some(format!("fast-forward unavailable: {}", e.message))),
    };
    Ok(paths_clear(root, &changes)
        .err()
        .map(|s| format!("fast-forward unavailable: {s}")))
}

fn merge(root: &Path, target: &str) -> Result<bool, RunError> {
    output(
        root,
        &[
            "-c",
            "merge.autoStash=false",
            "-c",
            "submodule.recurse=false",
            "merge",
            "--ff-only",
            "--no-overwrite-ignore",
            "--no-edit",
            "--no-stat",
            "--no-progress",
            target,
        ],
    )
    .map(|out| out.status.success())
}

fn advance_graph(
    root: &Path,
    old: &str,
    target: &str,
    probe: impl FnMut(AdvancePhase),
) -> Result<Option<String>, RunError> {
    advance_with(root, old, target, probe, merge)
}

fn advance_with(
    root: &Path,
    old: &str,
    target: &str,
    mut probe: impl FnMut(AdvancePhase),
    mut merge: impl FnMut(&Path, &str) -> Result<bool, RunError>,
) -> Result<Option<String>, RunError> {
    if let Some(reason) = preflight(root, old, target)? {
        return Ok(Some(reason));
    }
    probe(AdvancePhase::BeforeMerge);
    // Recheck at the mutation boundary. A branch switch in the remaining
    // window is a documented residual: Git may fast-forward that branch,
    // but loses no commits. The postflight reports it and writers freeze.
    if let Some(reason) = preflight(root, old, target)? {
        return Ok(Some(reason));
    }
    let merged = merge(root, target);
    probe(AdvancePhase::AfterMerge);
    if let Some(status) = repair_status(root)? {
        return Err(status.error());
    }
    let expected = if matches!(merged, Ok(true)) {
        target
    } else {
        old
    };
    if !expected_head(root, expected)? {
        return Err(RunError::failure(
            "io.write_failed",
            format!(
                "graph sync postflight failed: HEAD or mdkg-graph changed; inspect with {}",
                status_command(root)
            ),
        ));
    }
    match merged {
        Ok(true) => Ok(None),
        // A refused merge with no damage does not freeze an intact checkout.
        Ok(false) => Ok(Some(
            "git merge --ff-only refused; checkout unchanged".into(),
        )),
        Err(error) => Err(error),
    }
}

/// Observed repair state, never a transaction journal or an automatic restore.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct RepairStatus {
    pub checkout: PathBuf,
    /// Why graph writes are frozen (index lock, branch, flags or tracked changes).
    pub states: Vec<String>,
    /// Affected tracked paths, relative to the graph checkout.
    pub paths: Vec<PathBuf>,
    /// Operator guidance; starts with a read-only Git status command.
    pub guidance: Vec<String>,
    pub reason: String,
}

impl RepairStatus {
    fn error(&self) -> RunError {
        RunError::failure("io.write_failed", self.reason.clone())
            .with_hint(self.guidance.join("\n"))
    }
}

fn status_command(root: &Path) -> String {
    format!(
        "git -C {} status",
        super::shell_word(&root.to_string_lossy())
    )
}

/// Repository-wide, read-only diagnostics. No graph config, lock acquisition,
/// index refresh, marker creation or recovery is needed to observe damage.
pub fn repair_status(root: &Path) -> Result<Option<RepairStatus>, RunError> {
    // Validate the caller under its existing pins before deriving a sibling
    // checkout. Only that checked common directory may be inspected below.
    let common =
        git::common_dir(root).map_err(|e| RunError::io("can't inspect caller repository", e))?;
    let records = identities(root)?;
    if records.is_empty() {
        if let Some(primary) = known_primary(root)?
            && fs::symlink_metadata(primary.join(".worktrees/graph")).is_ok()
        {
            return Err(identity_error("missing graph identity record"));
        }
        // Missing records never authorize a role. The expected branch is
        // enough evidence to diagnose a missing record even after a move.
        for path in git::worktrees(root).map_err(|e| RunError::io("can't list worktrees", e))? {
            let on_graph = if same_folder(&path, root).unwrap_or(false) {
                // The caller was already checked under its own pins.
                // Re-pinning it here would replace that cached checkout and
                // force both this read and the next caller read to reopen it.
                git::head(root).is_ok_and(|head| head.branch == BRANCH)
            } else {
                git::with_worktree_trust(
                    &git::WorktreeTrust {
                        common: Some(common.clone()),
                        worktree: Some(path.clone()),
                    },
                    || git::head(&path).is_ok_and(|head| head.branch == BRANCH),
                )
            };
            if on_graph {
                return Err(identity_error(format!(
                    "missing graph identity record for {}",
                    path.display()
                )));
            }
        }
        return Ok(None);
    }
    let graph = registered_checkout(root, &records)?;
    let trust = git::WorktreeTrust {
        common: Some(common.clone()),
        worktree: Some(graph.clone()),
    };
    git::with_worktree_trust(&trust, || {
        check_identities(&graph, &records, false)?;
        repair_checkout(graph, &common)
    })
}

/// Ordinary diffs deliberately trust these flags, so they cannot establish
/// that every tracked path matches HEAD until the flags have been cleared.
fn flagged_index_paths(root: &Path) -> Result<Vec<PathBuf>, RunError> {
    use std::os::unix::ffi::OsStrExt as _;
    let flags = output(root, &["ls-files", "--cached", "-v", "-z"])?;
    if !flags.status.success() {
        return Err(refused("can't read graph index flags"));
    }
    let mut paths = Vec::new();
    for entry in flags.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        if entry.len() < 3 || entry[1] != b' ' || !entry[0].is_ascii_alphabetic() {
            return Err(refused("invalid graph index flags"));
        }
        // Lowercase tags mark assume-unchanged; S/s marks skip-worktree.
        if entry[0].is_ascii_lowercase() || entry[0] == b'S' {
            paths.push(PathBuf::from(std::ffi::OsStr::from_bytes(&entry[2..])));
        }
    }
    Ok(paths)
}

fn repair_checkout(graph: PathBuf, common: &Path) -> Result<Option<RepairStatus>, RunError> {
    let checkout =
        git::check_worktree(&graph).map_err(|e| RunError::io("can't inspect graph checkout", e))?;
    if !same_folder(&checkout.top, &graph)? || !same_folder(&checkout.common, common)? {
        return Err(refused(
            ".worktrees/graph must be the exact checkout of the expected repository",
        ));
    }
    let mut states = Vec::new();
    let lock = checkout.git_dir.join("index.lock");
    let lock_exists = match fs::symlink_metadata(&lock) {
        Ok(_) => {
            states.push("leftover index.lock (or a Git operation still running)".into());
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(RunError::io("can't inspect graph index lock", e)),
    };
    let symbolic = output(&graph, &["symbolic-ref", "-q", "HEAD"])?;
    let on_graph = symbolic.status.success()
        && String::from_utf8_lossy(&symbolic.stdout).trim() == "refs/heads/mdkg-graph";
    if !on_graph {
        states.push("HEAD is not refs/heads/mdkg-graph".into());
    }
    let mut paths = Vec::new();
    for args in [
        vec![
            "diff",
            "--no-renames",
            "--cached",
            "--name-only",
            "-z",
            "HEAD",
            "--",
        ],
        vec!["diff", "--no-renames", "--name-only", "-z", "--"],
    ] {
        match names_from(output(&graph, &args)?) {
            Ok(changes) => paths.extend(changes),
            Err(_) => {
                states.push("Git cannot read tracked changes; inspect the index and HEAD".into())
            }
        }
    }
    if !paths.is_empty() {
        states.push("tracked files or index do not match HEAD".into());
    }
    let flagged = match flagged_index_paths(&graph) {
        Ok(flagged) => {
            if !flagged.is_empty() {
                states.push(
                    "index flags hide tracked changes (assume-unchanged or skip-worktree)".into(),
                );
                paths.extend(flagged.iter().cloned());
            }
            Some(flagged)
        }
        Err(_) => {
            states.push(
                "Git cannot read index flags; inspect the index before restoring paths".into(),
            );
            None
        }
    };
    paths.sort();
    paths.dedup();
    if states.is_empty() {
        return Ok(None);
    }
    let git = format!("git -C {}", super::shell_word(&graph.to_string_lossy()));
    let mut guidance = vec![status_command(&graph), format!("{git} diff HEAD --")];
    if lock_exists {
        guidance.push(format!("Only after confirming no Git process is running for this worktree, remove the leftover lock: rm -- {}",
            super::shell_word(&lock.to_string_lossy())));
    }
    if flagged.is_none() {
        guidance.push("Git cannot read index flags. Inspect the index manually and rerun doctor before switching branches or restoring paths.".into());
    } else if let Some(flagged) = flagged.filter(|f| !f.is_empty()) {
        if flagged.iter().any(|p| p.to_str().is_none()) {
            guidance.push("Non-UTF-8 index-flagged paths require manual review before clearing flags, switching branches or restoring files.".into());
        } else {
            let listed = flagged
                .iter()
                .map(|p| super::shell_word(&p.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ");
            guidance.push("Save any wanted file contents outside this checkout first. Clear both flags with the following commands, then rerun doctor before switching branches or restoring paths.".into());
            // update-index applies only the last flag-selection option, so
            // clear the two independent bits in separate invocations.
            guidance.push(format!("Clear assume-unchanged: {git} --literal-pathspecs update-index --no-assume-unchanged -- {listed}"));
            guidance.push(format!("Clear skip-worktree: {git} --literal-pathspecs update-index --no-skip-worktree -- {listed}"));
        }
    } else if !on_graph {
        guidance.push(format!("Preserve any wanted edits first, then return to the graph branch: {git} switch --no-overwrite-ignore mdkg-graph; rerun doctor before restoring paths."));
    } else if paths.iter().any(|p| p.to_str().is_none()) {
        guidance.push(
            "Non-UTF-8 paths require manual review before restoring the index or files.".into(),
        );
    } else if !paths.is_empty() {
        let listed = paths
            .iter()
            .map(|p| super::shell_word(&p.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        guidance.push(format!("Save any wanted tracked edits outside this checkout first. Then restore the index from HEAD: {git} --literal-pathspecs restore --source=HEAD --staged -- {listed}"));
        // Index-only restore leaves any newly added files untracked, rather
        // than deleting them. Only paths present in HEAD get a worktree restore.
        let head_paths = names_from(output(
            &graph,
            &["ls-tree", "-r", "--name-only", "-z", "HEAD"],
        )?)?;
        let changes: Vec<_> = paths
            .iter()
            .filter(|p| head_paths.contains(p))
            .map(|p| Change {
                path: p.clone(),
                old: true,
                incoming: true,
            })
            .collect();
        if !changes.is_empty() {
            match paths_clear(&graph, &changes) {
                Ok(()) => guidance.push(format!("After saving edits and restoring the index, restore the listed HEAD paths: {git} --literal-pathspecs restore --source=HEAD --worktree -- {}",
                    changes.iter().map(|c| super::shell_word(&c.path.to_string_lossy())).collect::<Vec<_>>().join(" "))),
                Err(reason) => guidance.push(format!("Before restoring tracked files, move this collision aside without deleting it, then rerun doctor: {reason}")),
            }
        }
    }
    guidance.push("Untracked and ignored content is left in place; move any incoming-path collisions aside yourself before retrying sync.".into());
    let reason = format!(
        "graph worktree needs repair: {}; checkout {}; paths: {}",
        states.join("; "),
        graph.display(),
        if paths.is_empty() {
            "(none)".into()
        } else {
            paths
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    Ok(Some(RepairStatus {
        checkout: graph,
        states,
        paths,
        guidance,
        reason,
    }))
}

/// Refuse writes through the graph checkout, leaving other checkouts usable.
pub fn ensure_graph_writable(root: &Path) -> Result<(), RunError> {
    if !is_graph_checkout(root)? {
        return Ok(());
    }
    match repair_status(root)? {
        Some(status) => Err(status.error()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn command(root: &Path, args: &[&str]) -> String {
        let mut command = std::process::Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        let out = command
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .expect("git");
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).expect("UTF-8").trim().into()
    }

    const CONFIG: &str = r#"{"schema_version":3,"format_version":"0.7","graph_id":"0192f3a0-0000-7000-8000-000000000000"}"#;

    fn setup() -> (Scratch, PathBuf, String, String) {
        let root = std::env::temp_dir().join(format!("mdkg-graph-race-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(root.join(".mdkg")).expect("graph");
        fs::write(root.join(".mdkg/config.json"), CONFIG).expect("config");
        command(&root, &["init", "-qb", "main"]);
        command(&root, &["add", ".mdkg"]);
        command(&root, &["commit", "-qm", "initial graph"]);
        GraphPlan::inspect(&root, "main")
            .expect("plan")
            .create(false)
            .expect("graph checkout");
        let graph = root.join(".worktrees/graph");
        let old = command(&graph, &["rev-parse", "HEAD"]);
        command(&graph, &["branch", "other", &old]);
        fs::write(root.join("new-main"), "new main content").expect("file");
        command(&root, &["add", "new-main"]);
        command(&root, &["commit", "-qm", "advance main"]);
        let target = command(&root, &["rev-parse", "HEAD"]);
        (Scratch(root), graph, old, target)
    }

    #[test]
    fn selected_config_trees_exclude_unrelated_entries_and_keep_blockers() {
        git::with_worktree_trust(&Default::default(), || {
            let root = Scratch(
                std::env::temp_dir().join(format!("mdkg-selected-tree-{}", uuid::Uuid::now_v7())),
            );
            fs::create_dir_all(root.0.join("unrelated/deep")).expect("unrelated");
            fs::write(root.0.join("unrelated/deep/config.json"), "unrelated")
                .expect("unrelated blob");
            let mut paths = Vec::new();
            // Enough argument bytes to cross a query batch boundary.
            for i in 0..100 {
                let path = PathBuf::from(format!(
                    "packages/{}-{i}/.mdkg/config.json",
                    "x".repeat(200)
                ));
                fs::create_dir_all(root.0.join(&path).parent().expect("parent")).expect("folder");
                fs::write(root.0.join(&path), CONFIG).expect("config");
                paths.push(path);
            }
            fs::create_dir_all(root.0.join("nested")).expect("nested");
            std::os::unix::fs::symlink("../unrelated", root.0.join("nested/linked")).expect("link");
            paths.push(PathBuf::from("nested/linked/.mdkg/config.json"));
            paths.push(PathBuf::from("missing/.mdkg/config.json"));
            command(&root.0, &["init", "-qb", "main"]);
            command(&root.0, &["add", "."]);
            command(&root.0, &["commit", "-qm", "selected trees"]);
            let revision = command(&root.0, &["rev-parse", "HEAD^{tree}"]);
            let tree = config_tree(&root.0, &revision, &paths).expect("selective index");
            assert!(tree.keys().all(|p| !p.starts_with(b"unrelated")));
            assert_eq!(tree.len(), 303); // packages + 100 triples + nested/link
            for path in &paths[..100] {
                assert!(config_object(&tree, path).expect("config").is_some());
            }
            assert!(
                config_object(&tree, &paths[100])
                    .expect_err("link parent")
                    .message
                    .contains("directory")
            );
            assert!(
                config_object(&tree, &paths[101])
                    .expect("absent child")
                    .is_none()
            );
            // A selected directory stays one entry and cannot expand its children.
            let directory =
                config_tree(&root.0, &revision, &[PathBuf::from("unrelated")]).expect("directory");
            assert_eq!(directory.len(), 1);
            assert!(config_object(&directory, Path::new("unrelated")).is_err());
        });
    }

    #[test]
    fn config_path_limits_prevent_query_expansion() {
        for paths in [
            vec![
                PathBuf::from("a/config.json"),
                PathBuf::from("a/config.json/child"),
            ],
            vec![PathBuf::from("x".repeat(CONFIG_PATH_CAP + 1))],
            vec![PathBuf::from(format!(
                "{}config.json",
                "x/".repeat(CONFIG_DEPTH_CAP)
            ))],
        ] {
            assert!(config_paths(&paths).is_err(), "{paths:?}");
        }
        assert_eq!(
            config_paths(&[
                PathBuf::from("a/config.json"),
                PathBuf::from("a/config.json")
            ])
            .expect("deduplicated")
            .len(),
            1
        );
    }

    #[test]
    fn graph_declaration_limit_counts_aliases_before_deduplication() {
        let mut config: serde_json::Value = serde_json::from_str(CONFIG).expect("config");
        let workspaces = (0..MAX_DECLARATIONS - 1)
            .map(|i| (format!("alias{i}"), serde_json::json!({"path":"same"})))
            .collect::<serde_json::Map<_, _>>();
        config["workspaces"] = workspaces.into();
        assert_eq!(declared_configs(&config).expect("at limit").len(), 2);
        config["subgraphs"] = serde_json::json!({"external": {}});
        assert!(
            declared_configs(&config)
                .expect_err("over limit")
                .message
                .contains("1024")
        );
    }

    #[test]
    fn graph_config_batches_preserve_literal_paths_and_shared_objects() {
        git::with_worktree_trust(&Default::default(), || {
            let root = Scratch(
                std::env::temp_dir().join(format!("mdkg-config-batches-{}", uuid::Uuid::now_v7())),
            );
            fs::create_dir_all(root.0.join(".mdkg")).expect("root");
            let mut config: serde_json::Value = serde_json::from_str(CONFIG).expect("config");
            let mut workspaces = serde_json::Map::new();
            // Five distinct near-cap objects span multiple bounded content batches.
            for i in 0..5 {
                let path = format!("child[{i}]\nline");
                let folder = root.0.join(&path).join(".mdkg");
                fs::create_dir_all(&folder).expect("child");
                let child = serde_json::json!({"schema_version":3,"format_version":"0.7","graph_id":uuid::Uuid::now_v7().to_string(),"padding":" ".repeat(900_000)});
                fs::write(folder.join("config.json"), child.to_string()).expect("config");
                workspaces.insert(format!("child{i}"), serde_json::json!({"path":path}));
            }
            fs::create_dir_all(root.0.join("copy/.mdkg")).expect("copy");
            fs::copy(
                root.0.join("child[0]\nline/.mdkg/config.json"),
                root.0.join("copy/.mdkg/config.json"),
            )
            .expect("duplicate object");
            workspaces.insert("copy".into(), serde_json::json!({"path":"copy"}));
            config["workspaces"] = workspaces.into();
            fs::write(root.0.join(".mdkg/config.json"), config.to_string()).expect("root config");
            command(&root.0, &["init", "-qb", "main"]);
            command(&root.0, &["add", "."]);
            command(&root.0, &["commit", "-qm", "batched graphs"]);
            assert_eq!(graph_ids(&root.0).expect("bounded config batches").len(), 6);
        });
    }

    #[test]
    fn writer_lock_wait_is_bounded_and_retries_after_release() {
        let root =
            Scratch(std::env::temp_dir().join(format!("mdkg-lock-wait-{}", uuid::Uuid::now_v7())));
        let holder = graph_lock_file(&root.0).expect("holder");
        holder.lock().expect("lock");
        let contender = graph_lock_file(&root.0).expect("contender");
        let error =
            wait_graph_lock(contender, &root.0, std::time::Duration::ZERO).expect_err("busy");
        assert_eq!(error.kind, crate::ErrorKind::Failure);
        assert_eq!(error.code, "run.io_failed");
        assert_eq!(error.kind.exit_code(), 4);
        assert!(error.message.contains("graph checkout busy"));
        assert!(
            error.message.contains(
                root.0
                    .join("mdkg/graph-worktree.lock")
                    .to_str()
                    .expect("path")
            )
        );
        assert!(
            error
                .hint
                .expect("retry guidance")
                .contains("Do not remove")
        );
        drop(holder);
        let contender = graph_lock_file(&root.0).expect("contender");
        wait_graph_lock(contender, &root.0, std::time::Duration::ZERO)
            .expect("available immediately");
    }

    #[test]
    fn refresh_skips_a_writer_and_resumes_after_the_guard_drops() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, old, target) = setup();
            assert!(
                lock_graph_writer(&root.0)
                    .expect("ordinary checkout")
                    .is_none()
            );
            let guard = lock_graph_writer(&graph)
                .expect("writer guard")
                .expect("graph");
            let skipped = refresh_graph(&graph, "main", false);
            assert!(!skipped.updated);
            assert!(
                skipped
                    .note()
                    .expect("busy note")
                    .contains("holds the checkout")
            );
            assert_eq!(command(&graph, &["rev-parse", "HEAD"]), old);
            drop(guard);
            let synced = refresh_graph(&graph, "main", false);
            assert!(synced.updated, "{synced:?}");
            assert_eq!(synced.head.as_deref(), Some(target.as_str()));
        });
    }

    #[test]
    fn a_record_for_another_checkout_guides_the_caller_without_removing_it() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, _, _) = setup();
            let sibling = root.0.join("ordinary");
            command(
                &root.0,
                &["worktree", "add", sibling.to_str().expect("path"), "other"],
            );
            let records = identities(&sibling).expect("records");
            let error = check_identities(&sibling, &records, false).expect_err("wrong caller");
            assert!(error.message.contains("names worktree admin graph"));
            let hint = error.hint.expect("hint");
            assert!(hint.contains(graph.to_str().expect("graph path")));
            assert!(hint.contains("Run from that checkout instead"));
            assert!(!hint.contains("unset"));
        });
    }

    #[test]
    fn a_branch_switch_before_merge_is_refused_without_moving_either_branch() {
        git::with_worktree_trust(&Default::default(), || {
            let (_root, graph, old, target) = setup();
            let error = advance_graph(&graph, &old, &target, |phase| {
                if phase == AdvancePhase::BeforeMerge {
                    command(&graph, &["switch", "-q", "other"]);
                }
            })
            .expect_err("wrong branch freezes writes");
            assert!(error.message.contains("HEAD is not"));
            assert_eq!(command(&graph, &["rev-parse", "mdkg-graph"]), old);
            assert_eq!(command(&graph, &["rev-parse", "other"]), old);
            assert!(!graph.join("new-main").exists());
        });
    }

    #[test]
    fn a_branch_switch_after_merge_is_reported_without_rollback() {
        git::with_worktree_trust(&Default::default(), || {
            let (_root, graph, old, target) = setup();
            let error = advance_graph(&graph, &old, &target, |phase| {
                if phase == AdvancePhase::AfterMerge {
                    command(&graph, &["switch", "-q", "other"]);
                }
            })
            .expect_err("postflight freezes writes");
            assert!(error.message.contains("HEAD is not"));
            assert_eq!(command(&graph, &["rev-parse", "mdkg-graph"]), target);
            assert_eq!(command(&graph, &["rev-parse", "other"]), old);
            let status = repair_status(&graph).expect("status").expect("frozen");
            assert!(
                status
                    .guidance
                    .iter()
                    .any(|g| g.contains("switch --no-overwrite-ignore mdkg-graph"))
            );
            command(&graph, &["switch", "mdkg-graph"]);
            assert!(repair_status(&graph).expect("status").is_none());
        });
    }

    #[test]
    fn a_branch_switch_in_the_final_window_is_detected_and_loses_no_commits() {
        git::with_worktree_trust(&Default::default(), || {
            let (_root, graph, old, target) = setup();
            let error = advance_with(
                &graph,
                &old,
                &target,
                |_| (),
                |root, target| {
                    command(root, &["switch", "-q", "other"]);
                    merge(root, target)
                },
            )
            .expect_err("postflight detects documented race");
            assert!(error.message.contains("HEAD is not"));
            assert_eq!(command(&graph, &["rev-parse", "mdkg-graph"]), old);
            assert_eq!(command(&graph, &["rev-parse", "other"]), target);
            assert!(repair_status(&graph).expect("status").is_some());
        });
    }

    #[test]
    fn a_concurrent_graph_commit_is_never_rewound() {
        git::with_worktree_trust(&Default::default(), || {
            let (_root, graph, old, target) = setup();
            let reason = advance_graph(&graph, &old, &target, |phase| {
                if phase == AdvancePhase::BeforeMerge {
                    command(&graph, &["commit", "--allow-empty", "-qm", "other writer"]);
                }
            })
            .expect("safe skip")
            .expect("reason");
            assert!(reason.contains("HEAD changed"));
            let concurrent = command(&graph, &["rev-parse", "HEAD"]);
            assert_ne!(concurrent, old);
            assert_ne!(concurrent, target);
            assert_eq!(command(&graph, &["rev-parse", "HEAD^"]), old);
            assert!(repair_status(&graph).expect("status").is_none());
        });
    }

    #[test]
    fn a_refused_or_failed_merge_that_changed_nothing_does_not_freeze() {
        git::with_worktree_trust(&Default::default(), || {
            let (_root, graph, old, target) = setup();
            let result = advance_with(&graph, &old, &target, |_| (), |_, _| Ok(false))
                .expect("safe refusal");
            assert!(result.expect("reason").contains("checkout unchanged"));
            assert!(
                advance_with(
                    &graph,
                    &old,
                    &target,
                    |_| (),
                    |_, _| Err(RunError::failure(
                        "io.write_failed",
                        "injected process failure"
                    ))
                )
                .is_err()
            );
            assert!(repair_status(&graph).expect("status").is_none());
            assert_eq!(command(&graph, &["rev-parse", "HEAD"]), old);
            assert!(sync_graph(&graph, "main", false).expect("retry").updated);
        });
    }

    #[test]
    fn an_ignored_parent_symlink_is_never_replaced_by_checkout() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, old, _) = setup();
            fs::create_dir_all(root.0.join("ignored")).expect("directory");
            fs::write(root.0.join("ignored/incoming"), "incoming").expect("file");
            command(&root.0, &["add", "ignored/incoming"]);
            command(&root.0, &["commit", "-qm", "add nested path"]);
            fs::OpenOptions::new()
                .append(true)
                .open(root.0.join(".git/info/exclude"))
                .expect("exclude")
                .write_all(b"\nignored\n")
                .expect("ignore");
            let outside = root.0.join("outside");
            fs::create_dir(&outside).expect("outside");
            fs::write(outside.join("keep"), "keep").expect("unrelated");
            std::os::unix::fs::symlink(&outside, graph.join("ignored")).expect("link");
            assert!(command(&graph, &["status", "--porcelain"]).is_empty());
            let result = sync_graph(&graph, "main", false).expect("safe skip");
            assert!(
                result
                    .reason
                    .expect("reason")
                    .contains("not a plain directory")
            );
            assert_eq!(
                fs::read_link(graph.join("ignored")).expect("link preserved"),
                outside
            );
            assert_eq!(
                fs::read_to_string(outside.join("keep")).expect("outside"),
                "keep"
            );
            assert_eq!(command(&graph, &["rev-parse", "HEAD"]), old);
        });
    }

    #[test]
    fn an_existing_ignored_parent_directory_or_file_blocks_an_absent_incoming_leaf() {
        git::with_worktree_trust(&Default::default(), || {
            for directory in [true, false] {
                let (root, graph, old, _) = setup();
                fs::create_dir_all(root.0.join("ignored")).expect("directory");
                fs::write(root.0.join("ignored/incoming"), "incoming").expect("file");
                command(&root.0, &["add", "ignored/incoming"]);
                command(&root.0, &["commit", "-qm", "incoming path"]);
                fs::OpenOptions::new()
                    .append(true)
                    .open(root.0.join(".git/info/exclude"))
                    .expect("exclude")
                    .write_all(b"\nignored\n")
                    .expect("ignore");
                if directory {
                    fs::create_dir(graph.join("ignored")).expect("directory");
                } else {
                    fs::write(graph.join("ignored"), "keep").expect("file");
                }
                let result = sync_graph(&graph, "main", false).expect("skip");
                assert!(result.reason.expect("reason").contains("incoming parent"));
                assert_eq!(command(&graph, &["rev-parse", "HEAD"]), old);
                if directory {
                    assert!(graph.join("ignored").is_dir());
                } else {
                    assert_eq!(
                        fs::read_to_string(graph.join("ignored")).expect("file"),
                        "keep"
                    );
                }
                assert!(repair_status(&graph).expect("status").is_none());
            }
        });
    }

    #[test]
    fn an_untracked_collision_is_skipped_without_freezing_or_overwriting() {
        git::with_worktree_trust(&Default::default(), || {
            let (_root, graph, old, _) = setup();
            fs::write(graph.join("new-main"), "local content").expect("untracked");
            let result = sync_graph(&graph, "main", false).expect("skip");
            assert!(result.reason.expect("reason").contains("dirty"));
            assert_eq!(command(&graph, &["rev-parse", "HEAD"]), old);
            assert_eq!(
                fs::read_to_string(graph.join("new-main")).expect("local"),
                "local content"
            );
            assert!(repair_status(&graph).expect("status").is_none());
        });
    }

    #[test]
    fn a_modified_path_does_not_follow_a_symlink_parent() {
        git::with_worktree_trust(&Default::default(), || {
            let (fixture, graph, old, _) = setup();
            fs::write(fixture.0.join(".mdkg/config.json"), "new config").expect("main edit");
            command(&fixture.0, &["add", ".mdkg/config.json"]);
            command(&fixture.0, &["commit", "-qm", "modify nested path"]);
            let target = command(&fixture.0, &["rev-parse", "HEAD"]);
            let outside = fixture.0.join("outside");
            fs::create_dir(&outside).expect("outside");
            fs::write(outside.join("config.json"), "{}").expect("outside file");
            fs::rename(graph.join(".mdkg"), graph.join("saved-mdkg")).expect("move");
            std::os::unix::fs::symlink(&outside, graph.join(".mdkg")).expect("link");
            let changes = changed_paths(&graph, &old, &target).expect("paths");
            assert!(
                paths_clear(&graph, &changes)
                    .expect_err("reject parent")
                    .contains("parent .mdkg")
            );
            assert!(sync_graph(&graph, "main", false).is_err());
            assert_eq!(fs::read_link(graph.join(".mdkg")).expect("link"), outside);
            assert_eq!(
                fs::read_to_string(outside.join("config.json")).expect("outside"),
                "{}"
            );
        });
    }

    #[test]
    fn a_leftover_git_index_lock_freezes_without_a_marker() {
        git::with_worktree_trust(&Default::default(), || {
            let (fixture, graph, _old, _target) = setup();
            let lock = git::check_worktree(&graph)
                .expect("checkout")
                .git_dir
                .join("index.lock");
            fs::write(&lock, b"interrupted Git write").expect("lock");
            let before = fs::read(&lock).expect("lock bytes");
            let status = repair_status(&graph).expect("status").expect("frozen");
            assert!(status.reason.contains("index.lock"));
            assert!(status.guidance[0].ends_with(" status"));
            assert!(
                status
                    .guidance
                    .iter()
                    .any(|g| g.contains("no Git process is running"))
            );
            assert!(
                repair_status(&fixture.0)
                    .expect("primary diagnostic")
                    .is_some()
            );
            assert!(ensure_graph_writable(&fixture.0).is_ok());
            assert!(ensure_graph_writable(&graph).is_err());
            assert!(sync_graph(&graph, "main", true).is_err());
            assert!(GraphPlan::inspect(&fixture.0, "main").is_err());
            assert_eq!(fs::read(&lock).expect("unchanged"), before);
            fs::remove_file(lock).expect("operator removed stale lock");
            assert!(repair_status(&graph).expect("status").is_none());
            assert!(sync_graph(&graph, "main", false).expect("retry").updated);
        });
    }

    #[test]
    fn staged_and_unstaged_changes_that_cancel_still_freeze_the_index() {
        git::with_worktree_trust(&Default::default(), || {
            let (_root, graph, _, _) = setup();
            fs::write(graph.join(".mdkg/config.json"), "staged").expect("change");
            command(&graph, &["add", ".mdkg/config.json"]);
            fs::write(graph.join(".mdkg/config.json"), CONFIG).expect("restore only worktree");
            let status = repair_status(&graph).expect("status").expect("frozen");
            assert_eq!(status.paths, vec![PathBuf::from(".mdkg/config.json")]);
        });
    }

    #[test]
    fn sparse_checkouts_are_skipped_without_checkout_writes() {
        git::with_worktree_trust(&Default::default(), || {
            let (_fixture, graph, old, _) = setup();
            command(&graph, &["config", "core.sparseCheckout", "true"]);
            let result = sync_graph(&graph, "main", false).expect("skip");
            assert!(
                result
                    .reason
                    .expect("reason")
                    .contains("sparse graph checkout")
            );
            assert_eq!(command(&graph, &["rev-parse", "HEAD"]), old);
            assert!(!graph.join("new-main").exists());
        });
    }

    fn partial_checkout(root: &Path, target: &str) {
        // Exercise actual Git index and checkout writes, then fail before the
        // remaining path is written. HEAD has not advanced yet.
        command(root, &["read-tree", target]);
        command(
            root,
            &["checkout-index", "--force", "--", ".mdkg/config.json"],
        );
    }

    fn partial_setup() -> (Scratch, PathBuf, String, String) {
        let (root, graph, old, _) = setup();
        fs::write(root.0.join(".mdkg/config.json"), "{\"new\":true}").expect("new config");
        command(&root.0, &["add", ".mdkg/config.json"]);
        command(&root.0, &["commit", "-qm", "change config"]);
        let target = command(&root.0, &["rev-parse", "HEAD"]);
        (root, graph, old, target)
    }

    fn assert_frozen_and_repair(fixture: &Path, graph: &Path, old: &str) {
        assert_eq!(command(graph, &["rev-parse", "HEAD"]), old);
        assert_eq!(
            fs::read_to_string(graph.join(".mdkg/config.json")).expect("partial"),
            "{\"new\":true}"
        );
        assert!(!graph.join("new-main").exists(), "checkout did not finish");
        let status = repair_status(fixture).expect("read-only").expect("frozen");
        assert!(status.paths.contains(&PathBuf::from(".mdkg/config.json")));
        assert!(status.paths.contains(&PathBuf::from("new-main")));
        let before = command(graph, &["diff", "--cached", "--raw"]);
        assert!(ensure_graph_writable(graph).is_err());
        assert!(sync_graph(graph, "main", false).is_err());
        assert!(GraphPlan::inspect(fixture, "main").is_err());
        assert_eq!(command(graph, &["diff", "--cached", "--raw"]), before);
        // Later local files must survive both repeated diagnostics and the
        // documented, explicitly selected restore of tracked paths.
        command(graph, &["config", "core.excludesFile", "/dev/null"]);
        fs::OpenOptions::new()
            .append(true)
            .open(fixture.join(".git/info/exclude"))
            .expect("exclude")
            .write_all(b"\nlocal-cache\n")
            .expect("ignore");
        fs::write(graph.join("local-cache"), "ignored").expect("ignored");
        fs::write(graph.join("local-notes"), "untracked").expect("untracked");
        for guidance in status
            .guidance
            .iter()
            .filter(|g| g.contains("restore --source=HEAD"))
        {
            let cmd = guidance.split_once(": git ").expect("command").1;
            let restored = std::process::Command::new("/bin/sh")
                .args(["-c", &format!("git {cmd}")])
                .status()
                .expect("operator restore");
            assert!(restored.success());
        }
        assert!(repair_status(fixture).expect("repaired").is_none());
        assert_eq!(
            fs::read_to_string(graph.join("local-cache")).expect("ignored"),
            "ignored"
        );
        assert_eq!(
            fs::read_to_string(graph.join("local-notes")).expect("untracked"),
            "untracked"
        );
        assert_eq!(
            fs::read_to_string(graph.join(".mdkg/config.json")).expect("restored"),
            CONFIG
        );
    }

    #[test]
    fn a_failed_partial_git_checkout_freezes_and_printed_commands_repair_it() {
        git::with_worktree_trust(&Default::default(), || {
            let (fixture, graph, old, target) = partial_setup();
            let error = advance_with(
                &graph,
                &old,
                &target,
                |_| (),
                |root, target| {
                    partial_checkout(root, target);
                    Ok(false)
                },
            )
            .expect_err("partial checkout freezes");
            assert!(error.message.contains("graph worktree needs repair"));
            assert_frozen_and_repair(&fixture.0, &graph, &old);
        });
    }

    #[test]
    fn recovery_guidance_leaves_newly_staged_content_in_place() {
        git::with_worktree_trust(&Default::default(), || {
            let (fixture, graph, _, _) = setup();
            fs::write(graph.join("added-file"), "wanted new data").expect("new file");
            command(&graph, &["add", "added-file"]);
            fs::write(graph.join(".mdkg/config.json"), "changed config").expect("tracked edit");
            let status = repair_status(&graph).expect("status").expect("frozen");
            for guidance in status
                .guidance
                .iter()
                .filter(|g| g.contains("restore --source=HEAD"))
            {
                let cmd = guidance.split_once(": git ").expect("command").1;
                assert!(
                    std::process::Command::new("/bin/sh")
                        .args(["-c", &format!("git {cmd}")])
                        .status()
                        .expect("restore")
                        .success()
                );
            }
            assert_eq!(
                fs::read_to_string(graph.join("added-file")).expect("new data preserved"),
                "wanted new data"
            );
            assert!(repair_status(&fixture.0).expect("status").is_none());
        });
    }

    #[test]
    fn an_unreadable_index_is_frozen_instead_of_assumed_clean() {
        git::with_worktree_trust(&Default::default(), || {
            let (_fixture, graph, _, _) = setup();
            let index = git::check_worktree(&graph)
                .expect("checkout")
                .git_dir
                .join("index");
            fs::write(&index, b"broken index").expect("simulate interrupted index write");
            let status = repair_status(&graph).expect("status").expect("frozen");
            assert!(status.reason.contains("Git cannot read tracked changes"));
            assert!(ensure_graph_writable(&graph).is_err());
            assert_eq!(fs::read(index).expect("untouched"), b"broken index");
        });
    }

    #[test]
    fn a_staged_deletion_and_untracked_recreation_never_get_worktree_restore_guidance() {
        git::with_worktree_trust(&Default::default(), || {
            let (_root, graph, _, _) = setup();
            fs::write(graph.join("f"), "committed").expect("file");
            command(&graph, &["add", "f"]);
            command(&graph, &["commit", "-qm", "tracked fixture path"]);
            command(&graph, &["rm", "-q", "f"]);
            fs::write(graph.join("f"), "wanted untracked replacement").expect("replacement");
            let status = repair_status(&graph).expect("status").expect("frozen");
            assert!(
                !status
                    .guidance
                    .iter()
                    .any(|g| g.contains("--worktree -- f")),
                "{status:?}"
            );
            assert!(
                status
                    .guidance
                    .iter()
                    .any(|g| g.contains("move this collision aside")),
                "{status:?}"
            );
            assert_eq!(
                fs::read_to_string(graph.join("f")).expect("replacement"),
                "wanted untracked replacement"
            );
        });
    }

    #[test]
    fn a_pinned_sibling_can_read_graph_repair_without_losing_the_common_pin() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, old, _) = setup();
            let worker = root.0.join("sibling");
            command(
                &root.0,
                &[
                    "worktree",
                    "add",
                    "-qb",
                    "sibling",
                    worker.to_str().expect("path"),
                    &old,
                ],
            );
            fs::write(graph.join(".mdkg/config.json"), "partial config").expect("partial");
            for caller in [&root.0, &worker] {
                let pin = git::WorktreeTrust {
                    common: Some(root.0.join(".git")),
                    worktree: Some(caller.clone()),
                };
                git::with_worktree_trust(&pin, || {
                    let status = repair_status(caller)
                        .expect("pinned repository diagnostics")
                        .expect("frozen");
                    assert!(status.paths.contains(&PathBuf::from(".mdkg/config.json")));
                    assert_eq!(git::WorktreeTrust::current(), pin);
                });
            }
            let wrong = git::WorktreeTrust {
                common: Some(root.0.join("foreign.git")),
                worktree: Some(worker.clone()),
            };
            git::with_worktree_trust(&wrong, || {
                assert!(
                    repair_status(&worker).is_err(),
                    "the caller's common pin stays authoritative"
                );
                assert_eq!(git::WorktreeTrust::current(), wrong);
            });
        });
    }

    #[test]
    fn a_post_preflight_ignored_collision_is_preserved_by_git() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, old, target) = setup();
            fs::OpenOptions::new()
                .append(true)
                .open(root.0.join(".git/info/exclude"))
                .expect("exclude")
                .write_all(b"\nnew-main\n")
                .expect("ignore");
            let result = advance_with(
                &graph,
                &old,
                &target,
                |_| (),
                |root, target| {
                    fs::write(root.join("new-main"), "late ignored content")
                        .expect("racing writer");
                    merge(root, target)
                },
            )
            .expect("safe refusal");
            assert!(result.expect("reason").contains("checkout unchanged"));
            assert_eq!(
                fs::read_to_string(graph.join("new-main")).expect("preserved"),
                "late ignored content"
            );
            assert_eq!(command(&graph, &["rev-parse", "HEAD"]), old);
            assert!(repair_status(&graph).expect("status").is_none());
        });
    }

    #[test]
    fn the_printed_branch_repair_preserves_ignored_content() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, old, target) = setup();
            assert!(merge(&graph, &target).expect("advance graph"));
            command(&graph, &["switch", "-q", "other"]);
            fs::OpenOptions::new()
                .append(true)
                .open(root.0.join(".git/info/exclude"))
                .expect("exclude")
                .write_all(b"\nnew-main\n")
                .expect("ignore");
            fs::write(graph.join("new-main"), "wanted ignored content").expect("ignored");
            let status = repair_status(&root.0)
                .expect("status")
                .expect("wrong branch");
            let guidance = status
                .guidance
                .iter()
                .find(|g| g.contains(" switch "))
                .expect("switch command");
            let cmd = guidance
                .split_once(": git ")
                .expect("command")
                .1
                .split(';')
                .next()
                .expect("switch");
            let run = || {
                std::process::Command::new("/bin/sh")
                    .args(["-c", &format!("git {cmd}")])
                    .output()
                    .expect("operator switch")
            };
            assert!(
                !run().status.success(),
                "switch must refuse the ignored collision"
            );
            assert_eq!(
                fs::read_to_string(graph.join("new-main")).expect("preserved"),
                "wanted ignored content"
            );
            assert_eq!(command(&graph, &["rev-parse", "HEAD"]), old);
            fs::rename(graph.join("new-main"), root.0.join("saved-content")).expect("move aside");
            assert!(run().status.success());
            assert_eq!(command(&graph, &["rev-parse", "HEAD"]), target);
            assert!(repair_status(&root.0).expect("status").is_none());
            assert_eq!(
                fs::read_to_string(root.0.join("saved-content")).expect("saved"),
                "wanted ignored content"
            );
        });
    }

    #[test]
    fn a_symlinked_worktrees_parent_is_refused_before_inspecting_its_graph() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, _, _) = setup();
            let saved = root.0.join("saved-worktrees");
            fs::rename(root.0.join(".worktrees"), &saved).expect("move worktrees");
            std::os::unix::fs::symlink(&saved, root.0.join(".worktrees")).expect("parent link");
            fs::write(graph.join(".mdkg/config.json"), "partial config").expect("damage");
            let error = repair_status(&root.0).expect_err("never traverse parent link");
            assert!(
                error.message.contains(".worktrees must be a plain folder"),
                "{error:?}"
            );
            assert_eq!(
                fs::read_to_string(saved.join("graph/.mdkg/config.json")).expect("untouched"),
                "partial config"
            );
        });
    }

    #[test]
    fn graph_write_and_sync_guards_identify_the_checkout_through_parent_or_leaf_links() {
        git::with_worktree_trust(&Default::default(), || {
            for parent in [true, false] {
                let (root, graph, old, _) = setup();
                let (entry, moved, actual) = if parent {
                    (
                        root.0.join(".worktrees"),
                        root.0.join("saved-worktrees"),
                        root.0.join("saved-worktrees/graph"),
                    )
                } else {
                    (
                        graph.clone(),
                        root.0.join("saved-graph"),
                        root.0.join("saved-graph"),
                    )
                };
                fs::rename(&entry, &moved).expect("move graph");
                std::os::unix::fs::symlink(&moved, &entry).expect("link");
                for caller in [&graph, &actual] {
                    assert!(
                        ensure_graph_writable(caller).is_err(),
                        "writer through {caller:?}"
                    );
                    assert!(
                        sync_graph(caller, "main", false).is_err(),
                        "sync through {caller:?}"
                    );
                }
                assert!(
                    ensure_graph_writable(&root.0).is_ok(),
                    "primary checkout remains usable"
                );
                assert_eq!(command(&actual, &["rev-parse", "HEAD"]), old);
                assert!(!actual.join("new-main").exists());
            }
        });
    }

    #[test]
    fn an_unresolvable_graph_link_does_not_block_independent_checkouts() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, old, _) = setup();
            let worker = root.0.join("worker");
            command(
                &root.0,
                &[
                    "worktree",
                    "add",
                    "-qb",
                    "worker",
                    worker.to_str().expect("path"),
                    &old,
                ],
            );
            let moved = root.0.join("saved-graph");
            fs::rename(&graph, &moved).expect("move graph");
            std::os::unix::fs::symlink(&graph, &graph).expect("unresolvable link");
            for caller in [&root.0, &worker] {
                ensure_graph_writable(caller).expect("independent checkout");
                assert!(
                    !sync_graph(caller, "main", false)
                        .expect("no graph sync")
                        .updated
                );
            }
            assert!(
                ensure_graph_writable(&moved).is_err(),
                "broken graph backlink must refuse"
            );
            assert!(sync_graph(&moved, "main", false).is_err());
            assert_eq!(
                fs::read_to_string(moved.join(".mdkg/config.json")).expect("untouched"),
                CONFIG
            );
        });
    }

    #[test]
    fn a_relative_graph_registration_keeps_the_same_write_guard() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, _, _) = setup();
            let checkout = git::check_worktree(&graph).expect("checkout");
            fs::write(
                checkout.git_dir.join("gitdir"),
                "../../../.worktrees/graph/.git\n",
            )
            .expect("relative backlink");
            ensure_graph_writable(&graph).expect("clean graph");
            fs::write(graph.join(".mdkg/config.json"), "partial config").expect("damage");
            assert!(ensure_graph_writable(&graph).is_err());
            assert!(ensure_graph_writable(&root.0).is_ok());
        });
    }

    #[test]
    fn a_case_only_git_move_cannot_skip_freeze_on_a_case_insensitive_filesystem() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, _, _) = setup();
            let upper = root.0.join(".worktrees/Graph");
            if !upper.exists() {
                eprintln!("not run: filesystem is case-sensitive");
                return;
            }
            let temporary = root.0.join(".worktrees/moving-graph");
            command(
                &root.0,
                &[
                    "worktree",
                    "move",
                    graph.to_str().expect("path"),
                    temporary.to_str().expect("path"),
                ],
            );
            command(
                &root.0,
                &[
                    "worktree",
                    "move",
                    temporary.to_str().expect("path"),
                    upper.to_str().expect("path"),
                ],
            );
            ensure_graph_writable(&upper).expect("clean case variant");
            let admin = git::check_worktree(&upper).expect("checkout").git_dir;
            fs::write(admin.join("index.lock"), "interrupted").expect("lock");
            assert!(
                ensure_graph_writable(&upper).is_err(),
                "case variant must freeze"
            );
            assert!(sync_graph(&upper, "main", false).is_err());
            let status = repair_status(&root.0).expect("status").expect("frozen");
            assert!(status.reason.contains("index.lock"));
            fs::remove_file(admin.join("index.lock")).expect("repair");
            ensure_graph_writable(&upper).expect("case variant thaws");
            GraphPlan::inspect(&root.0, "main").expect("install keeps case variant");
        });
    }

    #[test]
    fn an_alias_registration_cannot_hide_a_frozen_or_relocated_graph() {
        git::with_worktree_trust(&Default::default(), || {
            for moved in [false, true] {
                let (root, graph, _, _) = setup();
                let checkout = git::check_worktree(&graph).expect("checkout");
                let alias = root.0.join("graph-alias");
                std::os::unix::fs::symlink(&graph, &alias).expect("alias");
                fs::write(
                    checkout.git_dir.join("gitdir"),
                    format!("{}\n", alias.join(".git").display()),
                )
                .expect("alias registration");
                fs::write(checkout.git_dir.join("index.lock"), "interrupted").expect("lock");
                let actual = if moved {
                    let actual = root.0.join("saved-graph");
                    fs::rename(&graph, &actual).expect("move");
                    std::os::unix::fs::symlink(&actual, &graph).expect("slot link");
                    actual
                } else {
                    graph.clone()
                };
                assert!(ensure_graph_writable(&actual).is_err());
                assert!(sync_graph(&actual, "main", false).is_err());
                ensure_graph_writable(&root.0).expect("independent primary");
            }
        });
    }

    #[test]
    fn a_graph_git_entry_retargeted_to_the_common_directory_cannot_skip_freeze() {
        git::with_worktree_trust(&Default::default(), || {
            for link in [false, true] {
                let (root, graph, _, _) = setup();
                let entry = graph.join(".git");
                if link {
                    fs::remove_file(&entry).expect("remove pointer");
                    std::os::unix::fs::symlink(root.0.join(".git"), &entry).expect("linked common");
                } else {
                    fs::write(
                        &entry,
                        format!("gitdir: {}\n", root.0.join(".git").display()),
                    )
                    .expect("retargeted pointer");
                }
                let index = fs::read(root.0.join(".git/index")).expect("primary index");
                assert!(ensure_graph_writable(&graph).is_err(), "retargeted graph");
                assert!(sync_graph(&graph, "main", false).is_err());
                assert_eq!(
                    fs::read(root.0.join(".git/index")).expect("untouched primary index"),
                    index
                );
                ensure_graph_writable(&root.0).expect("actual primary still usable");
            }
        });
    }

    #[test]
    fn a_foreign_checkout_under_a_symlinked_parent_is_never_reported_as_our_graph() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, _, _, _) = setup();
            let (foreign, foreign_graph, _, _) = setup();
            fs::rename(root.0.join(".worktrees"), root.0.join("saved-worktrees"))
                .expect("move worktrees");
            std::os::unix::fs::symlink(foreign.0.join(".worktrees"), root.0.join(".worktrees"))
                .expect("foreign parent link");
            fs::write(foreign_graph.join(".mdkg/config.json"), "foreign edits").expect("damage");
            let error = repair_status(&root.0).expect_err("foreign graph");
            assert!(
                error.message.contains(".worktrees must be a plain folder"),
                "{error:?}"
            );
            assert_eq!(
                fs::read_to_string(foreign_graph.join(".mdkg/config.json")).expect("untouched"),
                "foreign edits"
            );
        });
    }

    #[test]
    fn a_plain_foreign_graph_checkout_is_refused_by_the_common_directory_pin() {
        git::with_worktree_trust(&Default::default(), || {
            let (root, graph, _, _) = setup();
            fs::rename(&graph, root.0.join("saved-graph")).expect("move graph");
            fs::create_dir(&graph).expect("replacement");
            command(&graph, &["init", "-qb", "mdkg-graph"]);
            fs::write(graph.join("foreign"), "foreign content").expect("foreign");
            command(&graph, &["add", "foreign"]);
            command(&graph, &["commit", "-qm", "foreign repository"]);
            let error = repair_status(&root.0).expect_err("wrong common directory");
            assert!(error.message.contains("isn't the trusted one"), "{error:?}");
            assert_eq!(
                fs::read_to_string(graph.join("foreign")).expect("untouched"),
                "foreign content"
            );
        });
    }

    #[test]
    fn index_flags_freeze_hidden_changes_until_the_printed_flag_repair() {
        git::with_worktree_trust(&Default::default(), || {
            for flag in ["--assume-unchanged", "--skip-worktree"] {
                let (root, graph, old, _) = setup();
                let relative = ".mdkg/config.json";
                command(&graph, &["update-index", flag, "--", relative]);
                if flag == "--assume-unchanged" {
                    fs::write(graph.join(relative), "wanted hidden edit").expect("modify");
                } else {
                    fs::remove_file(graph.join(relative)).expect("missing skip-worktree file");
                }
                assert!(
                    command(&graph, &["diff", "--name-only"]).is_empty(),
                    "ordinary diff hides {flag}"
                );
                let index = git::check_worktree(&graph)
                    .expect("checkout")
                    .git_dir
                    .join("index");
                let before = fs::read(&index).expect("index");
                let status = repair_status(&root.0)
                    .expect("status")
                    .expect("index flags freeze writes");
                assert!(status.reason.contains("index flags"), "{status:?}");
                assert_eq!(status.paths, vec![PathBuf::from(relative)]);
                assert!(ensure_graph_writable(&graph).is_err());
                assert!(sync_graph(&graph, "main", false).is_err());
                assert!(GraphPlan::inspect(&root.0, "main").is_err());
                assert_eq!(command(&graph, &["rev-parse", "HEAD"]), old);
                assert_eq!(fs::read(&index).expect("read-only diagnostics"), before);
                assert!(!graph.join("new-main").exists());
                assert!(
                    !status.guidance.iter().any(|g| g.contains(" restore ")),
                    "unhide paths and rerun doctor before any restore"
                );
                let commands: Vec<_> = status
                    .guidance
                    .iter()
                    .filter(|g| g.contains(" update-index "))
                    .collect();
                assert_eq!(commands.len(), 2);
                for guidance in commands {
                    let cmd = guidance.split_once(": git ").expect("command").1;
                    assert!(
                        std::process::Command::new("/bin/sh")
                            .args(["-c", &format!("git {cmd}")])
                            .status()
                            .expect("operator flag repair")
                            .success()
                    );
                }
                let revealed = repair_status(&root.0)
                    .expect("status")
                    .expect("ordinary diff now sees the damage");
                assert!(!revealed.reason.contains("index flags"), "{revealed:?}");
                assert!(revealed.paths.contains(&PathBuf::from(relative)));
                if flag == "--assume-unchanged" {
                    assert_eq!(
                        fs::read_to_string(graph.join(relative)).expect("preserved"),
                        "wanted hidden edit"
                    );
                } else {
                    assert!(!graph.join(relative).exists());
                }
            }
        });
    }

    #[test]
    fn index_flags_also_freeze_clean_files() {
        git::with_worktree_trust(&Default::default(), || {
            for flag in ["--assume-unchanged", "--skip-worktree"] {
                let (root, graph, _, _) = setup();
                command(&graph, &["update-index", flag, "--", ".mdkg/config.json"]);
                let status = repair_status(&root.0)
                    .expect("status")
                    .expect("flags alone freeze writes");
                assert!(status.reason.contains("index flags"), "{status:?}");
                assert!(
                    !status
                        .reason
                        .contains("tracked files or index do not match HEAD"),
                    "{status:?}"
                );
            }
        });
    }
    const CRASH_TEST: &str = "MDKG_GRAPH_SYNC_CRASH_TEST";
    #[test]
    fn interrupted_sync_child() {
        let Some(data) = std::env::var_os(CRASH_TEST) else {
            return;
        };
        let (root, old, target): (PathBuf, String, String) =
            serde_json::from_str(&data.to_string_lossy()).expect("fixture");
        git::with_worktree_trust(&Default::default(), || {
            let _ = advance_with(
                &root,
                &old,
                &target,
                |_| (),
                |root, target| {
                    partial_checkout(root, target);
                    std::process::exit(77);
                },
            );
        });
        panic!("crash point not reached");
    }

    #[test]
    fn a_process_exit_mid_checkout_is_observed_without_any_journal() {
        git::with_worktree_trust(&Default::default(), || {
            let (fixture, graph, old, target) = partial_setup();
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args([
                        "--exact",
                        "install::worktree::tests::interrupted_sync_child",
                        "--nocapture",
                    ])
                    .env(
                        CRASH_TEST,
                        serde_json::to_string(&(&graph, &old, &target)).expect("fixture"),
                    )
                    .spawn()
                    .expect("child");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
            let exit = loop {
                if let Some(exit) = child.try_wait().expect("wait") {
                    break exit;
                }
                if std::time::Instant::now() > deadline {
                    child.kill().ok();
                    child.wait().ok();
                    panic!("crash child timed out");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            assert_eq!(exit.code(), Some(77));
            assert_frozen_and_repair(&fixture.0, &graph, &old);
            assert!(!fixture.0.join(".git/mdkg/graph-sync.json").exists());
        });
    }
}
