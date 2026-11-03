//! Validated graph snapshots read directly from immutable Git objects.
//!
//! Paths are repository-relative, even for a nested graph. No checkout, local
//! work store or status overlay participates. Selected roots must be ordinary
//! trees/files; symlinks and gitlinks inside document walks are skipped, like
//! filesystem discovery. Regular files in place of discovery directories are
//! treated as absent. Non-UTF-8 or backslash paths are unsupported.
//!
//! Configured discovery limits still apply. Independent ceilings bound hostile
//! config, templates and tree metadata before asking Git to emit object bodies.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use mdkg_format::{Code, node::FormatVersion};

use crate::{Config, GraphError, Index, config, discovery::Documents, index, templates};

/// Include this, the normalized graph root and the normalized graph folder
/// name (`.mdkg` unless recorded otherwise) in a derived index's reader key.
// v3: a custom graph folder's root workspace is read from that folder
// (Terra #10216, Opus #10511).
pub const READER_FINGERPRINT: &str = "mdkg-committed-reader-v3";

const MAX_BLOB_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_TREE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 200_000;
const MAX_OBJECTS: usize = 100_000;
const MAX_DEPTH: u64 = 128;
const MAX_PATH_BYTES: usize = 4096;

/// Committed parsing inputs and the validated graph they produced.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub config: Config,
    pub index: Index,
}

/// Read the repository-root graph at a full immutable commit OID (SHA-1 or SHA-256).
pub fn build(repo: &Path, commit: &str) -> Result<Index, GraphError> {
    build_at(repo, commit, "")
}

/// Read `<graph_root>/.mdkg/config.json` and its declared graph sources.
/// `graph_root` is a relative graph directory: `""`/`"."` means repository root.
/// Only committed data is used; machine-local work is deliberately absent.
/// Git uses the shared hardened runner and its worktree trust policy.
pub fn build_at(repo: &Path, commit: &str, graph_root: &str) -> Result<Index, GraphError> {
    Ok(load_at(repo, commit, graph_root)?.index)
}

/// [`build_at`] for a graph whose folder under its root is `dir`
/// (`mdkg.graph.<id>.dir`, orchestrator #8321).
pub fn build_at_dir(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
) -> Result<Index, GraphError> {
    Ok(load_at_dir(repo, commit, graph_root, dir)?.index)
}

/// Like [`build_at`], also retaining the committed config for filters and
/// workspace validation. It has no machine-local store directory or overrides.
pub fn load_at(repo: &Path, commit: &str, graph_root: &str) -> Result<Snapshot, GraphError> {
    load(repo, commit, graph_root, DEFAULT_DIR, None)
}

/// [`load_at`] for a graph whose folder under its root is `dir`.
pub fn load_at_dir(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
) -> Result<Snapshot, GraphError> {
    load(repo, commit, graph_root, dir, None)
}

/// Like [`load_at`], for a candidate that may become local authority (a
/// sync's fast-forward or merge, an install adoption, a split): tolerant
/// mode is off, so a malformed or duplicate node fails instead of being
/// skipped; a symlink or gitlink where discovery would skip it fails too;
/// and the committed `graph_id` must be `graph_id`.
///
/// The agent definitions and committed config are admitted too, as
/// [`crate::graphref::authority_at`] reads them (links, oversized files,
/// duplicate keys and malformed JSON are refused), so a candidate that
/// passes can't make authority reads fail once it's the tip.
pub fn load_strict(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    graph_id: &str,
) -> Result<Snapshot, GraphError> {
    load_strict_with(repo, commit, graph_root, graph_id, &|_, _| Ok(()))
}

/// A parsed agent definition's check, as the consumer applies it
/// (`Definition::parse` in mdkg-run, which this crate can't depend on).
pub type DefinitionCheck<'a> = dyn Fn(&str, &serde_json::Value) -> Result<(), String> + 'a;

/// [`load_strict`] that also runs `check` on every agent definition, so a
/// caller holding the consumers' definition parser admits only candidates
/// that parser accepts (Terra #7807). Its refusal names the definition.
pub fn load_strict_with(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    graph_id: &str,
    check: &DefinitionCheck<'_>,
) -> Result<Snapshot, GraphError> {
    load_strict_dir_with(repo, commit, graph_root, DEFAULT_DIR, graph_id, check)
}

/// [`load_strict_with`] for a graph whose folder under its root is `dir`.
pub fn load_strict_dir_with(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
    graph_id: &str,
    check: &DefinitionCheck<'_>,
) -> Result<Snapshot, GraphError> {
    let snapshot = load(repo, commit, graph_root, dir, Some(graph_id))?;
    let authority =
        crate::graphref::authority_at_dir(repo, commit, graph_root, dir).map_err(|e| {
            GraphError::failure(Code::ConfigInvalid, format!("{}: {}", e.code, e.message))
        })?;
    for (name, def) in &authority.definitions {
        check(name, &def.value).map_err(|why| {
            GraphError::failure(
                Code::ConfigInvalid,
                format!("graph.authority_invalid: agent definition {name}: {why}"),
            )
        })?;
    }
    Ok(snapshot)
}

fn load(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
    strict: Option<&str>,
) -> Result<Snapshot, GraphError> {
    if !valid_oid(commit) {
        return Err(invalid("expected a full lowercase immutable commit OID"));
    }
    let root = normalize(graph_root)?;
    let mut objects = Objects::new(repo);
    objects.strict = strict.is_some();
    let tree = commit_tree(&mut objects, commit)?;
    let folder = graph_folder(&root, dir)?;
    let graph_v2 = join(&folder, "graph.json")?;
    if objects.lookup(&tree, &graph_v2)?.is_some() {
        return Err(GraphError::failure(
            Code::ConfigGraphV2Unsupported,
            "graph format v2 (.mdkg/graph.json) needs migrating to format 0.7",
        )
        .at(graph_v2, None)
        .with_hint(config::identity_v2_hint()));
    }
    let config_path = join(&folder, "config.json")?;
    let config_entry = objects
        .lookup(&tree, &config_path)?
        .ok_or_else(|| invalid("committed graph config is missing").at(&config_path, None))?;
    let bytes = objects.file(&config_entry, MAX_CONFIG_BYTES)?;
    let value = serde_json::from_slice(&bytes).map_err(|e| {
        GraphError::failure(
            Code::ConfigUnreadable,
            format!("invalid committed config: {e}"),
        )
        .at(&config_path, None)
    })?;
    let mut config = config::from_snapshot(value)?;
    // The root workspace's folder is the graph folder its config was read
    // from (Terra #10216): a custom folder's nodes are discovered there,
    // never in an unrelated `.mdkg`.
    let folder_name = normalize(dir)?;
    for ws in &mut config.workspaces {
        if normalize(&ws.path)?.is_empty() {
            ws.mdkg_dir = folder_name.clone();
        }
    }
    if let Some(expected) = strict {
        config.tolerant = false;
        let found = config.graph_id.as_ref().map(ToString::to_string);
        if found.as_deref() != Some(expected) {
            return Err(GraphError::failure(
                Code::ConfigInvalid,
                format!(
                    "graph.identity_mismatch: the committed graph_id is {}, not this clone's {expected}",
                    found.as_deref().unwrap_or("absent")
                ),
            )
            .at(&config_path, None));
        }
    }
    let schemas = if config.format == FormatVersion::V07 {
        templates::from_documents(&config, std::iter::empty())?
    } else {
        let dir = join(&join(&root, &config.templates_root)?, &config.templates_set)?;
        let files = objects.markdown(&tree, &dir, config.limits.max_depth)?;
        let bodies = objects.file_batch(files.iter().map(|(_, entry)| entry))?;
        templates::from_documents(
            &config,
            files.into_iter().map(|(path, entry)| {
                let text = std::str::from_utf8(bodies.get(&entry.oid)?).map_err(|_| {
                    GraphError::failure(Code::IoReadFailed, "committed template is not UTF-8")
                        .at(&path, None)
                })?;
                Ok((path, text.to_owned()))
            }),
        )?
    }
    .schemas;
    let (documents, files) = discover(&mut objects, &tree, &root, &config)?;
    // Keep core/core.md in discovery limits, but index::build_from skips its
    // contents. Do not request or charge those bodies during prefetch.
    let bodies = objects.file_batch(
        documents
            .by_workspace
            .iter()
            .flat_map(|(_, paths)| paths)
            .filter(|path| !crate::discovery::is_core_page(path))
            .map(|path| &files[path]),
    )?;
    let index = index::build_from(Path::new(""), &config, &schemas, &documents, |path| {
        let entry = files
            .get(path)
            .ok_or_else(|| invalid("missing captured document"))?;
        Ok(String::from_utf8_lossy(bodies.get(&entry.oid)?).into_owned())
    })?;
    Ok(Snapshot { config, index })
}

/// One regular file read from a committed tree, for authority reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedFile {
    /// Repository-relative path.
    pub path: String,
    /// The blob's object id.
    pub blob: String,
    pub bytes: Vec<u8>,
}

/// Only the committed config (`<graph_root>/.mdkg/config.json`) at
/// `commit`, with the same checks as [`authority_files`]: for picking a
/// candidate by its graph id before any definition is read.
pub fn config_file(
    repo: &Path,
    commit: &str,
    graph_root: &str,
) -> Result<CommittedFile, GraphError> {
    config_file_dir(repo, commit, graph_root, DEFAULT_DIR)
}

/// The folder name a graph keeps under its root unless its record says
/// otherwise (`mdkg.graph.<id>.dir`, orchestrator #8321).
pub const DEFAULT_DIR: &str = ".mdkg";

/// A graph folder name under the committed reader's rules, normalized
/// (creation uses it, so what's created can be read).
pub fn check_dir(dir: &str) -> Result<String, GraphError> {
    let dir = normalize(dir)?;
    if dir.is_empty() {
        return Err(invalid("a graph folder name is empty"));
    }
    Ok(dir)
}

/// `<graph_root>/<dir>`, both checked: the graph folder itself.
fn graph_folder(graph_root: &str, dir: &str) -> Result<String, GraphError> {
    let dir = normalize(dir)?;
    if dir.is_empty() {
        return Err(invalid("a graph folder name is empty"));
    }
    join(&normalize(graph_root)?, &dir)
}

/// [`config_file`] for a graph whose folder under its root is `dir`.
pub fn config_file_dir(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
) -> Result<CommittedFile, GraphError> {
    if !valid_oid(commit) {
        return Err(invalid("expected a full lowercase immutable commit OID"));
    }
    let folder = graph_folder(graph_root, dir)?;
    let mut objects = Objects::new(repo);
    let tree = commit_tree(&mut objects, commit)?;
    config_in(&mut objects, &tree, &folder, MAX_CONFIG_BYTES)
}

/// [`config_file`], or `None` when `<graph_root>/.mdkg/config.json` isn't
/// committed at all (any other failure is still an error).
pub fn config_file_if_present(
    repo: &Path,
    commit: &str,
    graph_root: &str,
) -> Result<Option<CommittedFile>, GraphError> {
    config_file_if_present_dir(repo, commit, graph_root, DEFAULT_DIR)
}

/// [`config_file_if_present`] for a graph whose folder is `dir`.
pub fn config_file_if_present_dir(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
) -> Result<Option<CommittedFile>, GraphError> {
    if !valid_oid(commit) {
        return Err(invalid("expected a full lowercase immutable commit OID"));
    }
    let folder = graph_folder(graph_root, dir)?;
    let mut objects = Objects::new(repo);
    let tree = commit_tree(&mut objects, commit)?;
    let config_path = join(&folder, "config.json")?;
    if objects.lookup(&tree, &config_path)?.is_none()
        && objects
            .lookup(&tree, &join(&folder, "graph.json")?)?
            .is_none()
    {
        return Ok(None);
    }
    config_in(&mut objects, &tree, &folder, MAX_CONFIG_BYTES).map(Some)
}

fn config_in(
    objects: &mut Objects<'_>,
    tree: &str,
    folder: &str,
    max_bytes: u64,
) -> Result<CommittedFile, GraphError> {
    let graph_v2 = join(folder, "graph.json")?;
    if objects.lookup(tree, &graph_v2)?.is_some() {
        return Err(GraphError::failure(
            Code::ConfigGraphV2Unsupported,
            "graph format v2 (.mdkg/graph.json) needs migrating to format 0.7",
        )
        .at(graph_v2, None));
    }
    let config_path = join(folder, "config.json")?;
    let entry = objects
        .lookup(tree, &config_path)?
        .filter(|e| e.kind == Kind::File)
        .ok_or_else(|| invalid("committed graph config is missing").at(&config_path, None))?;
    Ok(CommittedFile {
        blob: entry.oid.clone(),
        bytes: objects.file(&entry, MAX_CONFIG_BYTES.min(max_bytes))?,
        path: config_path,
    })
}

/// The committed config (`<graph_root>/.mdkg/config.json`) and every
/// `*.json` directly under `<graph_root>/.mdkg/agents/` at `commit`, as git
/// objects only. A symlink, gitlink, folder or file over `max_bytes` where
/// a definition belongs is an error, never skipped.
pub fn authority_files(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    max_bytes: u64,
) -> Result<(CommittedFile, Vec<CommittedFile>), GraphError> {
    authority_files_dir(repo, commit, graph_root, DEFAULT_DIR, max_bytes)
}

/// [`authority_files`] for a graph whose folder under its root is `dir`.
pub fn authority_files_dir(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
    max_bytes: u64,
) -> Result<(CommittedFile, Vec<CommittedFile>), GraphError> {
    if !valid_oid(commit) {
        return Err(invalid("expected a full lowercase immutable commit OID"));
    }
    let folder = graph_folder(graph_root, dir)?;
    let mut objects = Objects::new(repo);
    let tree = commit_tree(&mut objects, commit)?;
    let config = config_in(&mut objects, &tree, &folder, max_bytes)?;
    let dir = join(&folder, "agents")?;
    let mut definitions = Vec::new();
    match objects.lookup(&tree, &dir)? {
        None => {}
        Some(e) if e.kind == Kind::Tree => {
            let mut wanted = Vec::new();
            let children = objects.tree(&e.oid)?;
            // One metadata batch for every definition, before any check.
            objects.preload_infos(
                children
                    .iter()
                    .filter(|c| c.kind == Kind::File && c.name.ends_with(b".json"))
                    .map(|c| c.oid.as_str()),
            )?;
            for child in children {
                let name = std::str::from_utf8(&child.name)
                    .map_err(|_| invalid("definition path is not UTF-8"))?;
                if !name.ends_with(".json") {
                    continue;
                }
                let path = join(&dir, name)?;
                if child.kind != Kind::File {
                    return Err(invalid(
                        "graph.authority_invalid: a definition must be a regular file",
                    )
                    .at(path, None));
                }
                if objects.info(&child.oid)?.size > max_bytes {
                    return Err(limit(
                        "graph.authority_invalid: a definition is over the size limit",
                    )
                    .at(path, None));
                }
                wanted.push((path, child));
            }
            let bodies = objects.file_batch(wanted.iter().map(|(_, e)| e))?;
            for (path, entry) in wanted {
                definitions.push(CommittedFile {
                    bytes: bodies.get(&entry.oid)?.to_vec(),
                    blob: entry.oid,
                    path,
                });
            }
        }
        Some(_) => {
            return Err(
                invalid("graph.authority_invalid: .mdkg/agents must be a folder").at(dir, None),
            );
        }
    }
    Ok((config, definitions))
}

fn commit_tree(objects: &mut Objects<'_>, commit: &str) -> Result<String, GraphError> {
    let commit_bytes = objects.body(commit, "commit", MAX_CONFIG_BYTES)?;
    let first_line = commit_bytes
        .split(|b| *b == b'\n')
        .next()
        .unwrap_or_default();
    std::str::from_utf8(first_line)
        .ok()
        .and_then(|s| s.strip_prefix("tree "))
        .filter(|s| valid_oid(s))
        .map(str::to_owned)
        .ok_or_else(|| invalid("commit has no valid root tree"))
}

fn discover(
    objects: &mut Objects<'_>,
    tree: &str,
    root: &str,
    config: &Config,
) -> Result<(Documents, HashMap<PathBuf, Entry>), GraphError> {
    let mut by_workspace = Vec::new();
    let mut files = HashMap::new();
    let (mut count, mut bytes) = (0u64, 0u64);
    for ws in config.workspaces.iter().filter(|w| w.enabled) {
        let base = join(&join(root, &ws.path)?, &ws.mdkg_dir)?;
        let mut paths = Vec::new();
        for folder in ["core", "design", "work"] {
            let dir = join(&base, folder)?;
            for (path, entry) in objects.markdown(tree, &dir, config.limits.max_depth)? {
                count += 1;
                if count > config.limits.max_files || count > MAX_OBJECTS as u64 {
                    return Err(limit("committed graph exceeds file count limit"));
                }
                let path = PathBuf::from(path);
                paths.push(path.clone());
                files.insert(path, entry);
            }
        }
        by_workspace.push((ws.clone(), paths));
    }
    objects.preload_infos(files.values().map(|entry| entry.oid.as_str()))?;
    for (_, paths) in &mut by_workspace {
        for path in paths.iter() {
            let info = objects.info(&files[path].oid)?;
            let size = info.size;
            if info.kind != "blob" {
                return Err(invalid("expected document blob").at(path.display().to_string(), None));
            }
            bytes = bytes
                .checked_add(size)
                .ok_or_else(|| limit("document bytes overflow"))?;
            if size > config.limits.max_file_bytes || size > MAX_BLOB_BYTES {
                return Err(limit("committed document exceeds file byte limit")
                    .at(path.display().to_string(), None));
            }
            if bytes > config.limits.max_total_bytes || bytes > MAX_TOTAL_BYTES {
                return Err(limit("committed graph exceeds total byte limit"));
            }
        }
        paths.sort_by(|a, b| {
            a.to_string_lossy()
                .as_bytes()
                .cmp(b.to_string_lossy().as_bytes())
        });
    }
    Ok((Documents { by_workspace }, files))
}

fn invalid(message: impl Into<String>) -> GraphError {
    GraphError::failure(Code::ConfigInvalid, message)
}

fn limit(message: impl Into<String>) -> GraphError {
    GraphError::failure(Code::IndexLimitExceeded, message)
}

/// The object read must be the one whose metadata was charged.
fn same(found: &mdkg_git::ObjectInfo, oid: &str, info: &Info) -> Result<(), GraphError> {
    if found.oid != oid || found.kind != info.kind || found.size != info.size {
        return Err(invalid("object metadata changed during read"));
    }
    Ok(())
}

/// Never echoes object contents or repository configuration.
fn read_failed(e: std::io::Error) -> GraphError {
    GraphError::failure(
        Code::IoReadFailed,
        format!("committed Git read failed: {e}"),
    )
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn normalize(path: &str) -> Result<String, GraphError> {
    if path.starts_with('/') || path.contains(['\\', '\0']) || path.as_bytes().get(1) == Some(&b':')
    {
        return Err(invalid("snapshot paths must be relative portable paths"));
    }
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => return Err(invalid("snapshot paths cannot contain parent traversal")),
            _ => components.push(component),
        }
    }
    let result = components.join("/");
    if result.len() > MAX_PATH_BYTES || components.len() as u64 > MAX_DEPTH {
        return Err(limit("snapshot path exceeds reader ceiling"));
    }
    Ok(result)
}

fn join(base: &str, path: &str) -> Result<String, GraphError> {
    let path = normalize(path)?;
    if base.is_empty() {
        Ok(path)
    } else if path.is_empty() {
        normalize(base)
    } else {
        normalize(&format!("{base}/{path}"))
    }
}

#[derive(Clone, Debug)]
struct Entry {
    name: Vec<u8>,
    oid: String,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Tree,
    File,
    Link,
    Gitlink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Info {
    kind: String,
    size: u64,
}

/// File bodies read for one discovery phase, by OID.
#[derive(Default)]
struct Bodies(HashMap<String, Vec<u8>>);

impl Bodies {
    fn get(&self, oid: &str) -> Result<&[u8], GraphError> {
        self.0
            .get(oid)
            .map(Vec::as_slice)
            .ok_or_else(|| invalid("missing batch object body"))
    }
}

/// A command's committed reads, all through one long-lived `cat-file`
/// ([`mdkg_git::ObjectReader`]), started at the first read. Objects can't
/// change, so nothing read here goes stale.
struct Objects<'a> {
    repo: &'a Path,
    reader: Option<mdkg_git::ObjectReader>,
    infos: HashMap<String, Info>,
    trees: HashMap<String, Vec<Entry>>,
    body_bytes: u64,
    metadata_bytes: u64,
    entries: usize,
    visits: usize,
    walked_entries: usize,
    /// Refuse what tolerant discovery skips (links, gitlinks).
    strict: bool,
}

impl<'a> Objects<'a> {
    fn new(repo: &'a Path) -> Self {
        Self {
            repo,
            reader: None,
            infos: HashMap::new(),
            trees: HashMap::new(),
            body_bytes: 0,
            metadata_bytes: 0,
            entries: 0,
            visits: 0,
            walked_entries: 0,
            strict: false,
        }
    }

    fn reader(&mut self) -> Result<&mut mdkg_git::ObjectReader, GraphError> {
        if self.reader.is_none() {
            self.reader = Some(mdkg_git::ObjectReader::open(self.repo).map_err(read_failed)?);
        }
        Ok(self.reader.as_mut().expect("reader just opened"))
    }

    /// An object's contents, which must match the metadata already read.
    fn contents(&mut self, oid: &str, info: &Info) -> Result<Vec<u8>, GraphError> {
        let (found, bytes) = self
            .reader()?
            .contents(oid, info.size)
            .map_err(read_failed)?
            .ok_or_else(|| invalid("object is missing or metadata is invalid"))?;
        same(&found, oid, info)?;
        Ok(bytes)
    }

    fn info(&mut self, oid: &str) -> Result<Info, GraphError> {
        self.preload_infos([oid])?;
        self.infos
            .get(oid)
            .cloned()
            .ok_or_else(|| invalid("missing object metadata"))
    }

    /// One metadata request per discovery phase, deduplicated by immutable OID.
    fn preload_infos<'b>(
        &mut self,
        oids: impl IntoIterator<Item = &'b str>,
    ) -> Result<(), GraphError> {
        let mut missing = BTreeSet::new();
        for oid in oids {
            if !valid_oid(oid) {
                return Err(invalid("invalid object OID"));
            }
            if !self.infos.contains_key(oid) {
                missing.insert(oid);
            }
            if self.infos.len() + missing.len() > MAX_OBJECTS {
                return Err(limit("too many snapshot objects"));
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        for oid in missing {
            let info = self
                .reader()?
                .info(oid)
                .map_err(read_failed)?
                .filter(|info| info.oid == oid)
                .ok_or_else(|| invalid("object is missing or metadata is invalid"))?;
            self.infos.insert(
                oid.to_owned(),
                Info {
                    kind: info.kind,
                    size: info.size,
                },
            );
        }
        Ok(())
    }

    /// Preflight every logical file before issuing one bounded body batch.
    /// Reused blobs are fetched once, but every path still consumes the budget.
    fn file_batch<'b>(
        &mut self,
        entries: impl IntoIterator<Item = &'b Entry>,
    ) -> Result<Bodies, GraphError> {
        let entries: Vec<_> = entries.into_iter().collect();
        if entries.len() > MAX_OBJECTS {
            return Err(limit("too many snapshot files"));
        }
        self.preload_infos(entries.iter().map(|entry| entry.oid.as_str()))?;
        let mut requested = BTreeSet::new();
        for entry in entries {
            if entry.kind != Kind::File {
                return Err(invalid("selected snapshot source must be a regular file"));
            }
            let info = self.info(&entry.oid)?;
            self.charge(&info, "blob", MAX_BLOB_BYTES)?;
            requested.insert(entry.oid.as_str());
        }
        if requested.is_empty() {
            return Ok(Bodies::default());
        }
        let mut bodies = Bodies::default();
        for oid in requested {
            let info = self.infos[oid].clone();
            let bytes = self.contents(oid, &info)?;
            bodies.0.insert(oid.to_owned(), bytes);
        }
        Ok(bodies)
    }

    fn charge(&mut self, info: &Info, kind: &str, ceiling: u64) -> Result<(), GraphError> {
        if info.kind != kind {
            return Err(invalid(format!("expected {kind} object")));
        }
        if info.size > ceiling {
            return Err(limit(format!(
                "committed {kind} exceeds reader byte ceiling"
            )));
        }
        self.body_bytes = self
            .body_bytes
            .checked_add(info.size)
            .filter(|n| *n <= MAX_TOTAL_BYTES)
            .ok_or_else(|| limit("snapshot bytes exceed reader ceiling"))?;
        Ok(())
    }

    fn body(&mut self, oid: &str, kind: &str, ceiling: u64) -> Result<Vec<u8>, GraphError> {
        let info = self.info(oid)?;
        self.charge(&info, kind, ceiling)?;
        self.contents(oid, &info)
    }

    fn file(&mut self, entry: &Entry, ceiling: u64) -> Result<Vec<u8>, GraphError> {
        if entry.kind != Kind::File {
            return Err(invalid("selected snapshot source must be a regular file"));
        }
        self.body(&entry.oid, "blob", ceiling)
    }

    fn tree(&mut self, oid: &str) -> Result<Vec<Entry>, GraphError> {
        if let Some(entries) = self.trees.get(oid) {
            return Ok(entries.clone());
        }
        let info = self.info(oid)?;
        if info.kind != "tree" {
            return Err(invalid("selected snapshot directory must be a tree"));
        }
        if info.size > MAX_TREE_BYTES {
            return Err(limit("committed tree exceeds reader byte ceiling"));
        }
        self.metadata_bytes = self
            .metadata_bytes
            .checked_add(info.size)
            .filter(|n| *n <= MAX_METADATA_BYTES)
            .ok_or_else(|| limit("snapshot tree metadata exceeds reader ceiling"))?;
        // Walked by each entry's OID, never by name: a name may hold a
        // newline, which a batch request can't carry.
        let (found, listed) = self
            .reader()?
            .tree(oid, info.size)
            .map_err(read_failed)?
            .ok_or_else(|| invalid("selected snapshot directory must be a tree"))?;
        same(&found, oid, &info)?;
        let mut entries = Vec::new();
        for entry in listed {
            self.entries += 1;
            if self.entries > MAX_ENTRIES {
                return Err(limit("too many snapshot tree entries"));
            }
            if !valid_oid(&entry.oid) {
                return Err(invalid("invalid tree metadata"));
            }
            // By the mode's type bits, as git reads a tree (`canon_mode`):
            // an old tree may hold `040000` or a file mode like `100664`.
            let mode = u32::from_str_radix(&entry.mode, 8)
                .map_err(|_| invalid("unsupported tree entry mode"))?;
            let kind = match mode & 0o170000 {
                0o040000 => Kind::Tree,
                0o100000 => Kind::File,
                0o120000 => Kind::Link,
                0o160000 => Kind::Gitlink,
                _ => return Err(invalid("unsupported tree entry mode")),
            };
            let name = entry.name;
            if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
                return Err(invalid("invalid tree entry name"));
            }
            entries.push(Entry {
                name,
                oid: entry.oid,
                kind,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        if entries.windows(2).any(|pair| pair[0].name == pair[1].name) {
            return Err(invalid("duplicate tree entry name"));
        }
        self.trees.insert(oid.to_owned(), entries.clone());
        Ok(entries)
    }

    fn lookup(&mut self, root: &str, path: &str) -> Result<Option<Entry>, GraphError> {
        let path = normalize(path)?;
        if path.is_empty() {
            return Ok(Some(Entry {
                name: Vec::new(),
                oid: root.to_owned(),
                kind: Kind::Tree,
            }));
        }
        let mut oid = root.to_owned();
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            let Some(entry) = self
                .tree(&oid)?
                .into_iter()
                .find(|e| e.name == part.as_bytes())
            else {
                return Ok(None);
            };
            if parts.peek().is_none() {
                return Ok(Some(entry));
            }
            if entry.kind == Kind::File {
                return Ok(None); // ENOTDIR is absent to filesystem discovery.
            }
            if entry.kind != Kind::Tree {
                return Err(invalid("snapshot ancestor is not an ordinary tree").at(path, None));
            }
            oid = entry.oid;
        }
        unreachable!("nonempty normalized path")
    }

    fn markdown(
        &mut self,
        root: &str,
        path: &str,
        max_depth: u64,
    ) -> Result<Vec<(String, Entry)>, GraphError> {
        let mut result = Vec::new();
        if let Some(entry) = self.lookup(root, path)? {
            if entry.kind == Kind::File {
                return Ok(result); // is_dir() is false for an ordinary file.
            }
            if entry.kind != Kind::Tree {
                return Err(
                    invalid("selected snapshot directory must be an ordinary tree").at(path, None),
                );
            }
            self.walk(&entry.oid, path, 0, max_depth.min(MAX_DEPTH), &mut result)?;
        }
        Ok(result)
    }

    fn walk(
        &mut self,
        oid: &str,
        path: &str,
        depth: u64,
        max_depth: u64,
        result: &mut Vec<(String, Entry)>,
    ) -> Result<(), GraphError> {
        if depth > max_depth {
            return Err(limit("committed directory exceeds depth limit").at(path, None));
        }
        self.visits += 1;
        if self.visits > MAX_OBJECTS {
            return Err(limit("too many snapshot directory visits"));
        }
        for entry in self.tree(oid)? {
            self.walked_entries += 1;
            if self.walked_entries > MAX_ENTRIES {
                return Err(limit("too many snapshot entry visits"));
            }
            if matches!(entry.kind, Kind::Link | Kind::Gitlink) {
                if self.strict {
                    let name = String::from_utf8_lossy(&entry.name);
                    return Err(invalid("a symlink or gitlink in a graph folder")
                        .at(format!("{path}/{name}"), None));
                }
                continue;
            }
            let name = std::str::from_utf8(&entry.name)
                .map_err(|_| invalid("snapshot source path is not UTF-8"))?;
            let child = join(path, name)?;
            if entry.kind == Kind::Tree {
                self.walk(&entry.oid, &child, depth + 1, max_depth, result)?;
            } else if Path::new(name).extension().is_some_and(|ext| ext == "md") {
                if result.len() >= MAX_OBJECTS {
                    return Err(limit("too many snapshot documents"));
                }
                result.push((child, entry));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preflight_fixture_ignores_an_inherited_index_and_hooks() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-committed-decoy-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        let (index, marker) = (dir.join("index"), dir.join("hook-ran"));
        std::fs::write(&index, "decoy").unwrap();
        let hook = dir.join("hooks/pre-commit");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Each on its own: a bad index fails `git add` before any hook
        // could run, so the hook needs a run of its own.
        let hooks = dir.join("hooks");
        let runs: [&[(&str, &std::ffi::OsStr)]; 3] = [
            &[("GIT_INDEX_FILE", index.as_os_str())],
            // Config git can't parse (a count with no key): any git that
            // inherits it fails, whatever `-c` the fixture adds.
            &[("GIT_CONFIG_COUNT", "1".as_ref())],
            &[
                ("GIT_CONFIG_COUNT", "1".as_ref()),
                ("GIT_CONFIG_KEY_0", "core.hooksPath".as_ref()),
                ("GIT_CONFIG_VALUE_0", hooks.as_os_str()),
            ],
        ];
        for env in runs {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            // Only this run's variables: an inherited one (say
            // `GIT_CONFIG_PARAMETERS`) could outrank the injected one.
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("GIT_") {
                    child.env_remove(key);
                }
            }
            let output = child
                .args([
                    "--exact",
                    "committed::tests::a_tree_read_through_objects_checks_its_preflight",
                    "--test-threads=1",
                ])
                .envs(env.iter().copied())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{env:?}: {stdout}");
            assert!(stdout.contains("1 passed"), "the fixture ran: {stdout}");
            assert_eq!(std::fs::read_to_string(&index).unwrap(), "decoy");
            assert!(!marker.exists(), "an inherited hook ran");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_tree_read_through_objects_checks_its_preflight() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-committed-preflight-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(dir.join("d")).unwrap();
        let git = |args: &[&str]| {
            let mut cmd = std::process::Command::new("git");
            // Nothing inherited routes the fixture's git elsewhere (an
            // index, a git folder) or configures it (hooks).
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("GIT_") {
                    cmd.env_remove(key);
                }
            }
            let out = cmd
                .current_dir(&dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("d/a.md"), "a").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "t"]);
        let tree = git(&["rev-parse", "HEAD^{tree}"]);
        let size: u64 = git(&["cat-file", "-s", &tree]).parse().unwrap();
        mdkg_git::with_worktree_trust(&mdkg_git::WorktreeTrust::default(), || {
            // The answer matches its preflight: listed.
            let mut objects = Objects::new(&dir);
            assert_eq!(objects.tree(&tree).unwrap().len(), 1);
            // A preflight that doesn't match what git answers is refused
            // by the comparison, and nothing is kept for the tree.
            let mut objects = Objects::new(&dir);
            objects.infos.insert(
                tree.clone(),
                Info {
                    kind: "tree".into(),
                    size: size + 1,
                },
            );
            let refused = objects.tree(&tree).unwrap_err();
            assert_eq!(refused.code(), Code::ConfigInvalid, "{refused:?}");
            assert!(!objects.trees.contains_key(&tree));
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_read_must_match_its_preflight_metadata() {
        let oid = "a".repeat(40);
        let info = Info {
            kind: "tree".into(),
            size: 30,
        };
        let found = |oid: &str, kind: &str, size: u64| mdkg_git::ObjectInfo {
            oid: oid.into(),
            kind: kind.into(),
            size,
        };
        assert!(same(&found(&oid, "tree", 30), &oid, &info).is_ok());
        for wrong in [
            found(&"b".repeat(40), "tree", 30),
            found(&oid, "blob", 30),
            found(&oid, "tree", 31),
        ] {
            assert_eq!(
                same(&wrong, &oid, &info).unwrap_err().code(),
                Code::ConfigInvalid
            );
        }
    }

    #[test]
    fn batch_preflight_charges_duplicate_paths_and_rejects_before_git_body_read() {
        let oid = "a".repeat(40);
        let entry = Entry {
            name: b"a.md".to_vec(),
            oid: oid.clone(),
            kind: Kind::File,
        };
        // An invalid repository makes an accidental body call distinguishable
        // from the required limit error. Metadata is already captured.
        let mut objects = Objects::new(Path::new("/not-a-snapshot-repository"));
        objects.infos.insert(
            oid.clone(),
            Info {
                kind: "blob".into(),
                size: 1,
            },
        );
        objects.body_bytes = MAX_TOTAL_BYTES - 1;
        let error = objects
            .file_batch([&entry, &entry])
            .err()
            .expect("logical duplicate exceeds budget");
        assert_eq!(error.code(), Code::IndexLimitExceeded);
        objects.body_bytes = 0;
        objects.infos.insert(
            oid,
            Info {
                kind: "blob".into(),
                size: MAX_BLOB_BYTES + 1,
            },
        );
        let error = objects
            .file_batch([&entry])
            .err()
            .expect("oversized body blocked");
        assert_eq!(error.code(), Code::IndexLimitExceeded);
    }
}
