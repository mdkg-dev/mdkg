//! Embeds what `mdkg --version` reports: the commit this binary was
//! built from, whether that worktree had uncommitted changes to tracked
//! files, and when it was built. A reinstall can then confirm which binary
//! it installed (orchestrator, 2026-10-22).
//!
//! Git is read as the CLI reads it (`hardened` in mdkg-coord-sqlite's
//! git.rs): read-only, found on the absolute entries of `$PATH`, with every
//! `GIT_*` variable cleared and no hook, fsmonitor, replace object, graft,
//! commit-graph or attribute filter able to run or steer it. Without git
//! (a source tree that isn't this workspace's checkout), each value is
//! "unknown"; so is git older than 2.31 (`--path-format`), and "dirty"
//! with git older than 2.41 (`--attr-source`). A crate `cargo package`
//! made (one from crates.io) has no git, but its `.cargo_vcs_info.json`
//! names the commit, and whether that tree was dirty.
//!
//! "Dirty" covers what builds the binary: `crates/`, the workspace manifest,
//! lock file and toolchain file.
//!
//! A shared `CARGO_TARGET_DIR` across checkouts: Cargo keys neither this
//! script's run nor a crate's fingerprint by where the checkout sits. A
//! compile that reuses another checkout's script output reports "unknown"
//! (by `MDKG_BUILD_ROOT`); a binary Cargo reuses whole from another
//! checkout reports that checkout's commit, which is the code it holds.
//! Build an install in a target folder of its own.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// What "dirty" covers, relative to the workspace root.
const SOURCES: [&str; 4] = ["crates", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml"];

fn main() {
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let (commit, dirty) = match where_git_is(&dir) {
        Some(repo) => {
            // Rebuilt when HEAD, its branch, the index or the sources move.
            for path in repo.watch() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
            (repo.commit, repo.dirty)
        }
        // A crate from crates.io has no git, but cargo recorded the commit
        // it was packaged from.
        None => vcs_info(&dir),
    };
    let commit = commit.unwrap_or_else(|| "unknown".into());
    let dirty = match dirty {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    };
    let built = built();
    let line = |commit: &str, dirty: &str| {
        format!(
            "{version} (commit {commit}{}, built {built})",
            match dirty {
                "true" => ", dirty",
                "unknown" => ", dirty unknown",
                _ => "",
            }
        )
    };
    // The checkout this ran for, as rustc sees it: a compile in another
    // checkout that reuses this output reports "unknown".
    println!(
        "cargo:rustc-env=MDKG_BUILD_ROOT={}",
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()
    );
    println!("cargo:rustc-env=MDKG_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=MDKG_BUILD_DIRTY={dirty}");
    println!("cargo:rustc-env=MDKG_BUILD_DATE={built}");
    println!(
        "cargo:rustc-env=MDKG_BUILD_VERSION={}",
        line(&commit, dirty)
    );
    println!(
        "cargo:rustc-env=MDKG_BUILD_VERSION_ELSEWHERE={}",
        line("unknown", "unknown")
    );
}

/// The worktree the crate is built in, as git places it.
struct Repo {
    git_dir: PathBuf,
    common: PathBuf,
    top: PathBuf,
    head_ref: Option<String>,
    commit: Option<String>,
    dirty: Option<bool>,
}

impl Repo {
    /// HEAD and every way its commit moves (a branch ref, loose or packed,
    /// and HEAD's log, which every move appends to), and the sources. Not
    /// the index: any `git status` refreshes it.
    fn watch(&self) -> Vec<PathBuf> {
        let mut paths = vec![
            self.git_dir.join("HEAD"),
            self.git_dir.join("logs").join("HEAD"),
            self.common.join("packed-refs"),
            self.common.join("refs").join("heads"),
        ];
        paths.extend(SOURCES.iter().map(|s| self.top.join(s)));
        if let Some(r) = &self.head_ref {
            paths.push(self.common.join(r));
        }
        // A path that doesn't exist reruns the script on every build.
        paths.retain(|p| p.exists());
        paths
    }
}

fn where_git_is(dir: &Path) -> Option<Repo> {
    let fields = git(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
            "--show-toplevel",
            "--show-object-format",
        ],
        None,
    )?;
    let fields: Vec<&str> = fields.lines().collect();
    let [git_dir, common, top, format] = fields[..] else {
        return None;
    };
    // Only this workspace's own checkout: a source tree without .git inside
    // another repository (a dotfiles repo at ~) isn't that repository's
    // commit.
    let workspace = dir.join("../..").canonicalize().ok()?;
    if Path::new(top).canonicalize().ok()? != workspace {
        return None;
    }
    let empty_tree = match format {
        "sha1" => EMPTY_TREE_SHA1,
        "sha256" => EMPTY_TREE_SHA256,
        _ => return None,
    };
    let commit = git(dir, &["rev-parse", "--verify", "-q", "HEAD"], None)
        .filter(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_hexdigit()));
    let head_ref = git(dir, &["symbolic-ref", "-q", "HEAD"], None)
        .filter(|r| r.starts_with("refs/heads/") && !r.contains(".."));
    // Tracked files only: a scratch file nobody added isn't a change to the
    // build's source.
    let dirty = commit.as_ref().and_then(|_| {
        git(
            dir,
            &[
                "status",
                "--porcelain",
                "--untracked-files=no",
                "--ignore-submodules=all",
                "--",
                // From the workspace root, not this crate's folder.
                ":(top)crates",
                ":(top)Cargo.toml",
                ":(top)Cargo.lock",
                ":(top)rust-toolchain.toml",
            ],
            Some(empty_tree),
        )
        .map(|out| !out.is_empty())
    });
    Some(Repo {
        git_dir: PathBuf::from(git_dir),
        common: PathBuf::from(common),
        top: PathBuf::from(top),
        head_ref,
        commit,
        dirty,
    })
}

/// The commit `cargo package` recorded in `.cargo_vcs_info.json`, and
/// whether the tree was dirty. Cargo writes `"dirty": true` only for a
/// package made from a dirty tree, so a whole file without it is clean.
/// A file that's cut short, or whose values aren't what cargo writes,
/// names no commit.
fn vcs_info(dir: &Path) -> (Option<String>, Option<bool>) {
    let path = dir.join(".cargo_vcs_info.json");
    // Watched only when it's there: a missing path reruns this every build.
    if !path.is_file() {
        return (None, None);
    }
    println!("cargo:rerun-if-changed={}", path.display());
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (None, None);
    };
    let text = text.trim();
    // A small file cargo writes, read without a JSON crate: it must be one
    // whole object (every brace outside a string closed, the last at the
    // end), its sha1 a string and its dirty, if any, a bare true or false.
    if !(text.starts_with('{') && balanced(text)) {
        return (None, None);
    }
    let Some(Token::Str(commit)) = value(text, "sha1").filter(
        |t| matches!(t, Token::Str(c) if c.len() >= 40 && c.bytes().all(|b| b.is_ascii_hexdigit())),
    ) else {
        return (None, None);
    };
    let dirty = match value(text, "dirty") {
        None if !text.contains("\"dirty\"") => false,
        Some(Token::Word(w)) if w == "true" => true,
        Some(Token::Word(w)) if w == "false" => false,
        _ => return (None, None),
    };
    (Some(commit), Some(dirty))
}

/// A JSON value as `vcs_info` reads one: a string, or a bare word.
#[derive(Debug, PartialEq)]
enum Token {
    Str(String),
    Word(String),
}

/// Whether every `{` and `}` outside strings pairs up, with the outermost
/// object closing at the very end.
fn balanced(text: &str) -> bool {
    let (mut depth, mut in_str, mut escaped) = (0i32, false, false);
    for (i, c) in text.char_indices() {
        if in_str {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 && i + 1 != text.len() {
                    return false;
                }
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !in_str
}

/// The value after `"key":`: a quoted string, or a bare word ended by `,`
/// or `}`.
fn value(text: &str, key: &str) -> Option<Token> {
    let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    if let Some(quoted) = rest.strip_prefix('"') {
        return Some(Token::Str(quoted[..quoted.find('"')?].to_string()));
    }
    let word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    let after = rest[word.len()..].trim_start();
    (!word.is_empty() && after.starts_with([',', '}'])).then_some(Token::Word(word))
}

/// git, found on the absolute entries of `$PATH`, and that `$PATH` cleaned
/// the same way for git's own children.
fn git_program() -> Option<(PathBuf, OsString)> {
    use std::os::unix::fs::PermissionsExt;
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .filter(|d| d.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let program = dirs.iter().map(|d| d.join("git")).find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })?;
    Some((program, std::env::join_paths(&dirs).ok()?))
}

/// One read-only git command in `dir`: its trimmed output, or `None` when
/// git isn't there or the command fails.
fn git(dir: &Path, args: &[&str], attr_source: Option<&str>) -> Option<String> {
    let (program, path) = git_program()?;
    let mut c = Command::new(program);
    c.env("PATH", path);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            c.env_remove(key);
        }
    }
    c.env("GIT_GRAFT_FILE", "/dev/null/none")
        .env("GIT_SHALLOW_FILE", "/dev/null/none")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "none")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .arg("--no-replace-objects")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.commitGraph=false",
            "-c",
            "core.attributesFile=/dev/null",
        ]);
    if let Some(tree) = attr_source {
        c.arg(format!("--attr-source={tree}"));
    }
    let out = c
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    Some(text.trim_end_matches(['\n', '\r']).to_string())
}

/// When this was built, in UTC (RFC 3339): `$SOURCE_DATE_EPOCH` when set,
/// for a reproducible build, else now.
fn built() -> String {
    let epoch = std::env::var("SOURCE_DATE_EPOCH").ok();
    let parsed = epoch.as_deref().and_then(|s| s.trim().parse::<i64>().ok());
    if epoch.is_some() && parsed.is_none() {
        println!("cargo:warning=SOURCE_DATE_EPOCH isn't a number of seconds; using now");
    }
    let secs = parsed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
    });
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}
