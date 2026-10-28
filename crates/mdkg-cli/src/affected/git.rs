//! Read-only git for `affected`: resolving revisions, ancestry, and the
//! changed paths between two commits. Nothing here reads the checkout, so
//! its state (uncommitted edits, filters, sparse or assume-unchanged
//! entries, a different branch) can't change the answer.
//!
//! Every call runs with replace refs and grafts off (so no replacement can
//! stand in for a commit, or for a commit's parents), `GIT_OPTIONAL_LOCKS=0`,
//! `core.fsmonitor` off and `core.quotepath` off, and never runs an external
//! diff, a textconv filter or a hook. A partial clone, which would fetch missing objects from the
//! network, is detected first and makes the run full instead.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A git command in `dir` with the read-only settings above.
fn command(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .args([
            "--no-replace-objects",
            "--no-pager",
            // git 2.19 to 2.41 let a repository's `core.useReplaceRefs=true`
            // turn replace refs back on after `--no-replace-objects`;
            // command-line config is read last, so this wins.
            "-c",
            "core.useReplaceRefs=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=off",
        ])
        // `git config` alone reads GIT_CONFIG, as if `--file` were given,
        // which would hide the partial-clone settings from the check.
        .env_remove("GIT_CONFIG")
        // No grafts: `--no-replace-objects` doesn't turn them off, and one
        // would redirect `HEAD~1`. A path under a file can't be opened, so
        // git reads none.
        .env("GIT_GRAFT_FILE", "/dev/null/no-grafts")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        // Newer git also honours these; `--no-replace-objects` and the
        // partial-clone check cover older git.
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1");
    c
}

fn failure(what: &str, out: &Output) -> io::Error {
    io::Error::other(format!(
        "git {what}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// The repository `affected` reads.
#[derive(Debug)]
pub struct Repo {
    /// The worktree's top folder.
    pub top: PathBuf,
}

impl Repo {
    /// Opens the repository holding `dir`.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let out = command(dir)
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !out.status.success() {
            return Err(failure("rev-parse --show-toplevel", &out));
        }
        let text = String::from_utf8(out.stdout).map_err(|e| io::Error::other(e.to_string()))?;
        let top = std::fs::canonicalize(text.trim_end())?;
        Ok(Self { top })
    }

    fn run<I, S>(&self, args: I) -> io::Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        command(&self.top).args(args).output()
    }

    /// The commit `rev` names, or `None` when it doesn't name one.
    pub fn resolve_commit(&self, rev: &str) -> io::Result<Option<String>> {
        // `--end-of-options` keeps a revision that starts with `-` from
        // being read as an option.
        let spec = format!("{rev}^{{commit}}");
        let out = self.run([
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ])?;
        if !out.status.success() {
            return Ok(None);
        }
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(is_sha(&sha).then_some(sha))
    }

    /// Whether `ancestor` is an ancestor of `descendant` (or the same
    /// commit). `Err` when git can't tell.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> io::Result<bool> {
        let out = self.run([
            "merge-base",
            "--is-ancestor",
            "--end-of-options",
            ancestor,
            descendant,
        ])?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failure("merge-base --is-ancestor", &out)),
        }
    }

    /// Whether the repository is a shallow clone.
    pub fn is_shallow(&self) -> io::Result<bool> {
        let out = self.run(["rev-parse", "--is-shallow-repository"])?;
        if !out.status.success() {
            return Err(failure("rev-parse --is-shallow-repository", &out));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim() == "true")
    }

    /// Whether the repository is a partial clone (a promisor remote or the
    /// `partialClone` extension): reading a missing object would fetch it.
    pub fn is_partial_clone(&self) -> io::Result<bool> {
        let out = self.run([
            "config",
            "--name-only",
            "--get-regexp",
            r"^(extensions\.partialclone|remote\..*\.(promisor|partialclonefilter))$",
        ])?;
        match out.status.code() {
            // Exit 1: no such key.
            Some(0) => Ok(!out.stdout.is_empty()),
            Some(1) => Ok(false),
            _ => Err(failure("config --get-regexp", &out)),
        }
    }

    /// Every entry of `commit`'s tree, recursively (files, symlinks and
    /// submodules; folders are implied by their entries), with blob sizes.
    pub fn tree(&self, commit: &str) -> io::Result<Vec<TreeEntry>> {
        let out = self.run([
            "ls-tree",
            "-r",
            "-z",
            "-l",
            "--full-tree",
            "--end-of-options",
            commit,
        ])?;
        if !out.status.success() {
            return Err(failure("ls-tree", &out));
        }
        parse_ls_tree(&out.stdout)
    }

    /// Streams the raw contents of these blobs, in order, to `each`.
    /// `cat-file --batch` writes objects as stored: no filter, textconv or
    /// line-ending conversion runs.
    pub fn each_blob(
        &self,
        oids: &[String],
        mut each: impl FnMut(usize, &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        use std::io::{BufRead, Read, Write};
        let mut child = command(&self.top)
            .args(["cat-file", "--batch"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("no stdin"))?;
        let request: Vec<u8> = oids
            .iter()
            .flat_map(|o| format!("{o}\n").into_bytes())
            .collect();
        // Write from another thread, so a full output pipe can't deadlock.
        let writer = std::thread::spawn(move || {
            let r = stdin.write_all(&request);
            drop(stdin);
            r
        });
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("no stdout"))?;
        let mut reader = std::io::BufReader::new(stdout);
        let bad = |m: String| io::Error::other(format!("unexpected cat-file output: {m}"));
        let mut result = Ok(());
        let mut blob = Vec::new();
        for (i, oid) in oids.iter().enumerate() {
            let mut header = String::new();
            if let Err(e) = reader.read_line(&mut header) {
                result = Err(e);
                break;
            }
            let size = match parse_batch_header(&header, oid) {
                Ok(size) => size,
                Err(m) => {
                    result = Err(bad(m));
                    break;
                }
            };
            blob.clear();
            let read = (&mut reader).take(size + 1).read_to_end(&mut blob);
            if read.is_err() || blob.len() as u64 != size + 1 || blob.pop() != Some(b'\n') {
                result = Err(bad(format!("{oid} is truncated")));
                break;
            }
            if let Err(e) = each(i, &blob) {
                result = Err(e);
                break;
            }
        }
        if result.is_ok() {
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest)?;
            if !rest.is_empty() {
                result = Err(bad("extra output".into()));
            }
        } else {
            let _ = child.kill();
        }
        let status = child.wait()?;
        let wrote = writer
            .join()
            .map_err(|_| io::Error::other("cat-file writer panicked"))?;
        result?;
        wrote?;
        if !status.success() {
            return Err(io::Error::other("git cat-file --batch failed"));
        }
        Ok(())
    }

    /// The paths that differ between `base` and `head`, in git's order.
    pub fn changes(&self, base: &str, head: &str) -> io::Result<Vec<Change>> {
        let out = self.run([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--ignore-submodules=none",
            "--name-status",
            "-z",
            "-M",
            "--end-of-options",
            base,
            head,
            "--",
        ])?;
        if !out.status.success() {
            return Err(failure("diff --name-status", &out));
        }
        parse_name_status(&out.stdout)
    }
}

fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One changed path between two commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// `A`, `C`, `D`, `M`, `R`, `T`, or any other letter git reports.
    pub status: char,
    /// The path at `head` (for a deletion, the deleted path).
    pub path: Vec<u8>,
    /// For a rename or copy, the path at `base`.
    pub old_path: Option<Vec<u8>>,
}

/// One entry of a commit's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// `100644`, `100755`, `120000` (symlink) or `160000` (submodule).
    pub mode: String,
    pub oid: String,
    /// The blob's size; `None` for a submodule.
    pub size: Option<u64>,
    pub path: Vec<u8>,
}

/// Parses `ls-tree -r -z -l`:
/// `<mode> SP <type> SP <oid> SP+ <size or -> TAB <path> NUL`.
pub fn parse_ls_tree(bytes: &[u8]) -> io::Result<Vec<TreeEntry>> {
    let bad = |m: &str| io::Error::other(format!("unexpected ls-tree output: {m}"));
    let mut entries = Vec::new();
    for record in bytes.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| bad("no tab"))?;
        let head = std::str::from_utf8(&record[..tab]).map_err(|_| bad("header isn't UTF-8"))?;
        let mut parts = head.split(' ').filter(|p| !p.is_empty());
        let (Some(mode), Some(kind), Some(oid), Some(size), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(bad("header has the wrong number of fields"));
        };
        if !is_sha(oid) {
            return Err(bad("an object id isn't a SHA"));
        }
        let size = match (kind, size) {
            ("blob", n) => Some(n.parse().map_err(|_| bad("a blob size isn't a number"))?),
            ("commit", "-") => None,
            _ => return Err(bad("an entry is neither a blob nor a submodule")),
        };
        let path = record[tab + 1..].to_vec();
        if path.is_empty() {
            return Err(bad("empty path"));
        }
        entries.push(TreeEntry {
            mode: mode.to_string(),
            oid: oid.to_string(),
            size,
            path,
        });
    }
    Ok(entries)
}

/// The size in a `cat-file --batch` header, which must be `<oid> blob <size>`.
fn parse_batch_header(header: &str, oid: &str) -> Result<u64, String> {
    let line = header
        .strip_suffix('\n')
        .ok_or_else(|| format!("the header for {oid} has no end"))?;
    match line.split(' ').collect::<Vec<_>>().as_slice() {
        [id, "blob", size] if *id == oid => {
            size.parse().map_err(|_| format!("bad size in `{line}`"))
        }
        _ => Err(format!("expected blob {oid}, got `{line}`")),
    }
}

/// Parses `git diff --name-status -z`: `STATUS NUL path NUL`, or for a
/// rename or copy `STATUS NUL old NUL new NUL`.
pub fn parse_name_status(bytes: &[u8]) -> io::Result<Vec<Change>> {
    let mut fields = bytes.split(|b| *b == 0);
    let mut changes = Vec::new();
    let bad = |m: &str| io::Error::other(format!("unexpected git diff output: {m}"));
    while let Some(status) = fields.next() {
        if status.is_empty() {
            // The trailing NUL.
            continue;
        }
        let letter = *status.first().ok_or_else(|| bad("empty status"))? as char;
        if !letter.is_ascii_uppercase() {
            return Err(bad("status is not a letter"));
        }
        let first = fields
            .next()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| bad("missing path"))?
            .to_vec();
        if matches!(letter, 'R' | 'C') {
            let second = fields
                .next()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| bad("missing new path"))?
                .to_vec();
            changes.push(Change {
                status: letter,
                path: second,
                old_path: Some(first),
            });
        } else {
            changes.push(Change {
                status: letter,
                path: first,
                old_path: None,
            });
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_status_and_both_rename_paths() {
        let out = b"M\0a.go\0A\0b c.go\0D\0gone.go\0R087\0old/x.go\0new/x.go\0T\0link\0";
        let changes = parse_name_status(out).unwrap();
        assert_eq!(changes.len(), 5);
        assert_eq!(changes[1].path, b"b c.go");
        assert_eq!(changes[3].status, 'R');
        assert_eq!(changes[3].path, b"new/x.go");
        assert_eq!(changes[3].old_path.as_deref(), Some(&b"old/x.go"[..]));
        assert_eq!(changes[4].status, 'T');
    }

    #[test]
    fn rejects_truncated_output() {
        assert!(parse_name_status(b"M\0").is_err());
        assert!(parse_name_status(b"R100\0old\0").is_err());
        assert!(parse_name_status(b"m\0a\0").is_err());
    }

    #[test]
    fn a_copy_reports_both_paths_and_unknown_letters_pass_through() {
        let changes = parse_name_status(b"C075\0src.go\0dst.go\0X\0odd\0").unwrap();
        assert_eq!(changes[0].status, 'C');
        assert_eq!(changes[0].path, b"dst.go");
        assert_eq!(changes[0].old_path.as_deref(), Some(&b"src.go"[..]));
        // An unknown letter is kept; the command turns it into a full run.
        assert_eq!(changes[1].status, 'X');
    }

    #[test]
    fn every_command_turns_off_replace_objects_and_lazy_fetch() {
        let c = command(Path::new("/r"));
        let args: Vec<String> = c
            .get_args()
            .filter_map(|a| a.to_str().map(String::from))
            .collect();
        assert!(args.contains(&"--no-replace-objects".to_string()));
        assert!(args.contains(&"core.useReplaceRefs=false".to_string()));
        assert!(
            c.get_envs().any(|(k, v)| k == "GIT_CONFIG" && v.is_none()),
            "GIT_CONFIG is removed"
        );
        let envs: Vec<(String, String)> = c
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();
        for (k, v) in [
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_GRAFT_FILE", "/dev/null/no-grafts"),
        ] {
            assert!(envs.contains(&(k.to_string(), v.to_string())), "{k}");
        }
    }

    #[test]
    fn parses_ls_tree_and_batch_headers() {
        let oid = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let tree = format!(
            "100644 blob {oid}      12\ta b.txt\0120000 blob {oid}       4\tlink\0160000 commit {oid}       -\tsub\0"
        );
        let entries = parse_ls_tree(tree.as_bytes()).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, b"a b.txt");
        assert_eq!(entries[0].size, Some(12));
        assert_eq!(entries[1].mode, "120000");
        assert_eq!(entries[2].size, None);
        assert!(parse_ls_tree(b"100644 blob nothex 1\tx\0").is_err());
        assert!(parse_ls_tree(format!("040000 tree {oid} -\tx\0").as_bytes()).is_err());

        assert_eq!(parse_batch_header(&format!("{oid} blob 3\n"), oid), Ok(3));
        assert!(parse_batch_header(&format!("{oid} missing\n"), oid).is_err());
        assert!(parse_batch_header(&format!("{oid} blob 3"), oid).is_err());
        assert!(parse_batch_header("abc blob 3\n", oid).is_err());
    }

    #[test]
    fn empty_output_has_no_changes() {
        assert!(parse_name_status(b"").unwrap().is_empty());
    }
}
