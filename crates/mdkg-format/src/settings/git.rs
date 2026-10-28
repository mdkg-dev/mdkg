//! Locate a checkout from physical metadata, without Git or ambient settings.
//!
//! Discovery deliberately ignores Git environment overrides, just as the work
//! store does. A bare repository or a metadata directory is an error, including
//! inside an enclosing checkout. Callers pass captured overrides to
//! [`warnings`] to report disagreements without changing the selected store.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;

pub(super) const CHECKOUT_REQUIRED: &str = "mdkg needs a checkout with .mdkg/";

/// Captured by the caller; relative Git paths are relative to its actual cwd,
/// which can differ from an explicit graph root. No process environment reads.
#[derive(Debug, Clone, Copy)]
pub struct Environment<'a> {
    pub cwd: &'a Path,
    pub dir: Option<&'a Path>,
    pub common_dir: Option<&'a Path>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Variable {
    #[serde(rename = "GIT_DIR")]
    Dir,
    #[serde(rename = "GIT_COMMON_DIR")]
    CommonDir,
}

impl fmt::Display for Variable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dir => "GIT_DIR",
            Self::CommonDir => "GIT_COMMON_DIR",
        })
    }
}

/// A visible advisory value for CLI/doctor consumers, never a routing change.
/// `resolved` is absent when the override cannot be resolved safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub variable: Variable,
    pub resolved: Option<PathBuf>,
    pub selected: Option<PathBuf>,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let why = if self.resolved.is_some() {
            "points elsewhere"
        } else {
            "could not be resolved"
        };
        write!(f, "{} {why}; mdkg uses ", self.variable)?;
        match &self.selected {
            Some(path) => write!(f, "{path:?}"),
            None => f.write_str("no git common directory"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub top: PathBuf,
    pub common: PathBuf,
    pub prefix: PathBuf,
}

/// The nearest checkout boundary. Malformed metadata is an error, never
/// an absent repository or a reason to continue searching its ancestors.
pub fn discover(root: &Path) -> Result<Option<Repository>, String> {
    let root = std::fs::canonicalize(root).map_err(|e| format!("{}: {e}", root.display()))?;
    for dir in root.ancestors() {
        let dot_git = dir.join(".git");
        match std::fs::symlink_metadata(&dot_git) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if metadata_directory(dir)? {
                    return Err(format!("{}: {CHECKOUT_REQUIRED}", dir.display()));
                }
                continue;
            }
            Err(e) => return Err(format!("{}: {e}", dot_git.display())),
        }
        let (gitdir, common) =
            directories(&dot_git).map_err(|e| format!("{CHECKOUT_REQUIRED}: {e}"))?;
        // A gitdir file can point at a bare repository. A linked worktree of
        // a bare repository is different: it has its own HEAD and commondir.
        if root.starts_with(&gitdir)
            || root.starts_with(&common)
            || (gitdir == common && bare(&common, dir)?)
        {
            return Err(format!("{}: {CHECKOUT_REQUIRED}", dot_git.display()));
        }
        return Ok(Some(Repository {
            top: dir.into(),
            common,
            prefix: root.strip_prefix(dir).map_err(|e| e.to_string())?.into(),
        }));
    }
    Ok(None)
}

/// The common folder a `.git` entry leads to: the folder itself, or for a
/// worktree's `gitdir:` file, its git folder's `commondir` (the git folder
/// itself when there's no `commondir`).
fn directories(dot_git: &Path) -> Result<(PathBuf, PathBuf), String> {
    let canonical =
        |p: &Path| std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()));
    let meta = std::fs::metadata(dot_git).map_err(|e| format!("{}: {e}", dot_git.display()))?;
    let gitdir = if meta.is_dir() {
        canonical(dot_git)?
    } else {
        let text = read_small(dot_git)?;
        // Exactly git's form: `gitdir: <path>` and a line end, with any other
        // whitespace part of the path.
        let gitdir = line(&text)
            .strip_prefix("gitdir: ")
            .ok_or_else(|| format!("{} isn't a gitdir file", dot_git.display()))?;
        canonical(&dot_git.parent().unwrap_or(Path::new("/")).join(gitdir))?
    };
    let commondir = gitdir.join("commondir");
    let common = match std::fs::symlink_metadata(&commondir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => gitdir.clone(),
        Err(e) => return Err(format!("{}: {e}", commondir.display())),
        Ok(_) => canonical(&gitdir.join(line(&read_small(&commondir)?)))?,
    };
    // An empty/damaged .git must not count as a checkout or select an outer
    // store. These are the metadata entries Git requires, including unborn
    // repositories and linked worktrees (objects/refs live in the common dir).
    let head = read_small(&gitdir.join("HEAD")).map_err(|e| format!("invalid HEAD: {e}"))?;
    if !valid_head(&head) {
        return Err(
            "invalid HEAD: expected a refs/ symbolic ref or a 40/64-digit lowercase object id"
                .into(),
        );
    }
    for name in ["objects", "refs"] {
        let path = common.join(name);
        if !std::fs::metadata(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .is_dir()
        {
            return Err(format!("{} isn't a directory", path.display()));
        }
    }
    Ok((gitdir, common))
}

/// Git's written HEAD forms, without resolving refs or requiring an object.
fn valid_head(text: &str) -> bool {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if let Some(reference) = text.strip_prefix("ref: refs/") {
        !reference.ends_with('.')
            && !reference.contains("..")
            && !reference.contains("@{")
            && !reference
                .bytes()
                .any(|b| b <= b' ' || b == 0x7f || b"~^:?*[\\".contains(&b))
            && reference
                .split('/')
                .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
    } else {
        matches!(text.len(), 40 | 64)
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
}

/// With no .git entry, only actual git-dir evidence establishes a boundary.
/// Ordinary working files named HEAD or commondir never establish one.
fn metadata_directory(path: &Path) -> Result<bool, String> {
    let directory = |name| match std::fs::metadata(path.join(name)) {
        Ok(meta) => Ok(meta.is_dir()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.join(name).display())),
    };
    if !directory("objects")? || !directory("refs")? {
        return Ok(false);
    }
    Ok(read_small(&path.join("HEAD")).is_ok_and(|head| valid_head(&head)))
}

/// Inspect the common dir's physical topology flags; never load user config
/// or includes. Unrecognized bare syntax refuses rather than guessing a checkout.
fn bare(common: &Path, top: &Path) -> Result<bool, String> {
    let path = common.join("config");
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("{CHECKOUT_REQUIRED}: {}: {e}", path.display())),
        Ok(_) => (),
    }
    let text = read_bounded(&path, super::MAX_BYTES, true)
        .map_err(|e| format!("{CHECKOUT_REQUIRED}: {e}"))?;
    let text = text.replace("\\\r\n", "").replace("\\\n", "");
    let mut core = false;
    let mut result = false;
    let mut worktree = None;
    for line in text.lines() {
        let mut line = line.trim();
        if line.starts_with('[') {
            let Some((section, rest)) = line[1..].split_once(']') else {
                return Err(format!("{CHECKOUT_REQUIRED}: invalid git config section"));
            };
            core = section.trim().eq_ignore_ascii_case("core");
            line = rest.trim();
        }
        if !core {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .map_or((line, None), |(k, v)| (k, Some(v)));
        // Whitespace or a comment after a valueless boolean is legal.
        let key = key.split([' ', '\t', '#', ';']).next().unwrap_or_default();
        if !key.eq_ignore_ascii_case("bare") && !key.eq_ignore_ascii_case("worktree") {
            continue;
        }
        let value = value.map(config_value).transpose()?;
        if key.eq_ignore_ascii_case("worktree") {
            worktree =
                Some(value.ok_or_else(|| format!("{CHECKOUT_REQUIRED}: invalid core.worktree"))?);
            continue;
        }
        result = match value.as_ref().map(|v| v.to_ascii_lowercase()).as_deref() {
            None | Some("true" | "yes" | "on") => true,
            Some("" | "false" | "no" | "off") => false,
            Some(v) => v
                .parse::<i64>()
                .map(|v| v != 0)
                .map_err(|_| format!("{CHECKOUT_REQUIRED}: cannot interpret core.bare"))?,
        };
    }
    if let Some(worktree) = worktree {
        let worktree = std::fs::canonicalize(common.join(worktree))
            .map_err(|_| format!("{CHECKOUT_REQUIRED}: core.worktree is unavailable"))?;
        if worktree != top {
            return Err(format!(
                "{CHECKOUT_REQUIRED}: core.worktree names another folder"
            ));
        }
    }
    Ok(result)
}

/// Git quoting for the two topology values above, not a general config loader.
/// Other sections/keys and external configuration sources are never evaluated.
fn config_value(raw: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut quoted = false;
    let mut keep = 0;
    let mut chars = raw.trim_start().chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                keep = out.len();
            }
            '#' | ';' if !quoted => break,
            '\\' => {
                let c = match chars.next() {
                    Some('n') => '\n',
                    Some('t') => '\t',
                    Some('b') => '\u{8}',
                    Some(c @ ('\\' | '"')) => c,
                    _ => return Err(format!("{CHECKOUT_REQUIRED}: invalid git config escape")),
                };
                out.push(c);
                keep = out.len();
            }
            _ => {
                out.push(c);
                if quoted || !c.is_ascii_whitespace() {
                    keep = out.len();
                }
            }
        }
    }
    if quoted {
        return Err(format!("{CHECKOUT_REQUIRED}: unclosed git config quote"));
    }
    out.truncate(keep);
    Ok(out)
}

/// Report captured overrides that disagree with physical discovery. Gitdir
/// overrides resolve commondir too; GIT_COMMON_DIR is already a common dir.
/// Invalid overrides also produce warnings, never a fallback or silent routing.
pub fn warnings(repo: Option<&Repository>, env: Environment<'_>) -> Vec<Warning> {
    let selected = repo.map(|r| &r.common);
    [
        (Variable::Dir, env.dir),
        (Variable::CommonDir, env.common_dir),
    ]
    .into_iter()
    .filter_map(|(variable, value)| {
        let value = value?;
        let path = env.cwd.join(value);
        let resolved = if !env.cwd.is_absolute() || value.as_os_str().is_empty() {
            None
        } else if variable == Variable::Dir {
            directories(&path).ok().map(|(_, common)| common)
        } else {
            std::fs::canonicalize(&path).ok().filter(|p| p.is_dir())
        };
        if resolved.as_ref().is_some_and(|p| Some(p) == selected) {
            return None;
        }
        Some(Warning {
            variable,
            resolved,
            selected: selected.cloned(),
        })
    })
    .collect()
}

/// The first line of `text`, without its line end.
fn line(text: &str) -> &str {
    let first = text.split('\n').next().unwrap_or_default();
    first.strip_suffix('\r').unwrap_or(first)
}

/// A small regular file's text: opened without blocking (a FIFO where a
/// file belongs can't hang a read), at most 4 KiB, and UTF-8.
fn read_small(path: &Path) -> Result<String, String> {
    read_bounded(path, 4096, false)
}

fn read_bounded(path: &Path, limit: u64, nofollow: bool) -> Result<String, String> {
    use std::io::Read;
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let mut flags =
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC;
    if nofollow {
        flags |= rustix::fs::OFlags::NOFOLLOW;
    }
    let fd = rustix::fs::open(path, flags, rustix::fs::Mode::empty()).map_err(|e| fail(&e))?;
    let file = std::fs::File::from(fd);
    if !file.metadata().map_err(|e| fail(&e))?.is_file() {
        return Err(format!("{} isn't a regular file", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| fail(&e))?;
    if bytes.len() as u64 > limit {
        return Err(format!("{} is too long for git metadata", path.display()));
    }
    String::from_utf8(bytes).map_err(|e| fail(&e))
}
