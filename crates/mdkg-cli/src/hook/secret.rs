//! `secret.scan`: refuses a commit whose staged changes add a
//! credential-shaped string, and a push whose outgoing commits add one
//! (the hooks edd's rules table; serve S34 on the receiving side). The
//! only detector is [`crate::credentials::looks_secret`], the one the
//! mail filter uses: there's no second pattern list. A finding names the
//! commit and path, never what matched. A scan that can't run is an
//! error, which the callers refuse (orchestrator, 2026-10-30).
//!
//! Every blob is read as text, whatever git or its attributes call it
//! (`--text`), so a key in a binary file or a `-diff` one is found. A blob
//! larger than [`BLOB_MAX`] isn't read: the scan names it as unscanned,
//! which the callers turn into a warning (orchestrator #11600).

use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::credentials::looks_secret;

/// Where a scan found a credential-shaped string: never the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The commit that adds it, or empty for the staged changes.
    pub commit: String,
    /// The path it's added to, as git names it.
    pub path: String,
}

/// What a scan found, and what it left unread for its size.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scanned {
    pub finding: Option<Finding>,
    /// The paths of blobs larger than [`BLOB_MAX`], which weren't read.
    pub skipped: Vec<String>,
    /// The push's commits weren't bounded by anything the remote holds
    /// (no tracking refs, no remote tip): the scan read the whole history.
    pub full_history: bool,
}

/// The largest blob a scan reads: 16 MiB, fixed in 0.7.1.
pub const BLOB_MAX: u64 = 16 << 20;

/// Why a scan couldn't run. Never holds scanned content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError(pub String);

/// The diff options every scan uses, whatever the repository's config
/// says: no colour, external diff or textconv (which run configured
/// programs), every blob as text, fixed prefixes, the whole tree, renames
/// read as renames (a moved file adds nothing), and no context.
const DIFF: &[&str] = &[
    "--text",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--no-relative",
    "-M",
    "-U0",
];

/// The staged changes at `top`, in the index git commits from
/// (`GIT_INDEX_FILE` for a partial commit, inherited from git's hook).
pub fn scan_staged(top: &Path) -> Result<Scanned, ScanError> {
    let git = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(top).args(args);
        cmd
    };
    scan_diff(git, &["diff", "--cached"], &[], None, "")
}

/// One diff, read in two passes: its raw listing for each new blob's size
/// (`git` builds each command, `base` names the diff, `paths` its
/// pathspecs; `worktree`, the top the working-tree side's files are read
/// under, for a diff whose new side isn't in git), then its patch with
/// the blobs over [`BLOB_MAX`] excluded. `commit` names the finding's
/// commit.
fn scan_diff(
    git: impl Fn(&[&str]) -> Command,
    base: &[&str],
    paths: &[String],
    worktree: Option<&Path>,
    commit: &str,
) -> Result<Scanned, ScanError> {
    let big = oversized(&git, base, paths, worktree)?;
    let mut cmd = git(base);
    cmd.arg("-p").args(DIFF).arg("--");
    // An exclusion needs a positive pathspec beside it: the whole tree,
    // even from a subfolder (`:/`).
    if !paths.is_empty() {
        cmd.args(paths);
    } else if !big.is_empty() {
        cmd.arg(":/");
    }
    // The path's own bytes, so a name that isn't UTF-8 is excluded as
    // itself, never as a lossy look-alike (review of cc6e651a).
    for raw in &big {
        let mut spec = std::ffi::OsString::from(":(top,exclude,literal)");
        spec.push(std::ffi::OsStr::from_bytes(raw));
        cmd.arg(spec);
    }
    let skipped = big
        .iter()
        .map(|raw| String::from_utf8_lossy(raw).into_owned())
        .collect();
    let finding = first_added(cmd)?.map(|path| Finding {
        commit: commit.to_string(),
        path,
    });
    Ok(Scanned {
        finding,
        skipped,
        full_history: false,
    })
}

/// The paths, as git's bytes, of a diff's new blobs larger than
/// [`BLOB_MAX`]: sizes from one `cat-file --batch-check`, or the file's own
/// size for the working tree's side (`worktree`), whose blobs git hasn't
/// written.
fn oversized(
    git: &impl Fn(&[&str]) -> Command,
    base: &[&str],
    paths: &[String],
    worktree: Option<&Path>,
) -> Result<Vec<Vec<u8>>, ScanError> {
    let mut cmd = git(base);
    cmd.args([
        "--raw",
        "-z",
        "--no-abbrev",
        "--no-renames",
        "--no-relative",
        "--",
    ]);
    cmd.args(paths);
    let out = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| ScanError(format!("can't run git: {e}")))?;
    if !out.status.success() {
        return Err(ScanError("git couldn't list the changes".into()));
    }
    // `:<modes> <shas> <status>\0<path>\0`, one or more parents' columns;
    // the new blob's id is the last before the status. A deletion's is
    // zero and adds nothing.
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut fields = out.stdout.split(|b| *b == 0);
    while let Some(meta) = fields.next() {
        if meta.is_empty() {
            continue;
        }
        let meta = String::from_utf8_lossy(meta);
        let Some(path) = fields.next() else {
            return Err(ScanError("git's change listing is cut short".into()));
        };
        let words: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let (Some(status), Some(oid)) =
            (words.last(), words.len().checked_sub(2).map(|i| words[i]))
        else {
            return Err(ScanError("git's change listing is unreadable".into()));
        };
        if status.starts_with('D') {
            continue;
        }
        entries.push((oid.to_string(), path.to_vec()));
    }
    let zero = |o: &str| o.bytes().all(|b| b == b'0');
    let mut big = Vec::new();
    let in_git: Vec<&(String, Vec<u8>)> = entries.iter().filter(|(o, _)| !zero(o)).collect();
    if !in_git.is_empty() {
        let sizes = blob_sizes(git, in_git.iter().map(|(o, _)| o.as_str()))?;
        for (oid, path) in &in_git {
            if sizes.get(oid.as_str()).is_some_and(|&n| n > BLOB_MAX) {
                big.push(path.clone());
            }
        }
    }
    for (_, path) in entries.iter().filter(|(o, _)| zero(o)) {
        let Some(top) = worktree else { continue };
        let size = std::fs::symlink_metadata(top.join(std::ffi::OsStr::from_bytes(path)))
            .map_or(0, |m| m.len());
        if size > BLOB_MAX {
            big.push(path.clone());
        }
    }
    big.sort();
    big.dedup();
    Ok(big)
}

/// Each blob's size, from one `git cat-file --batch-check`.
fn blob_sizes<'a>(
    git: &impl Fn(&[&str]) -> Command,
    oids: impl Iterator<Item = &'a str>,
) -> Result<std::collections::HashMap<String, u64>, ScanError> {
    use std::io::Write as _;
    let mut child = git(&["cat-file", "--batch-check=%(objectname) %(objectsize)"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ScanError(format!("can't run git cat-file: {e}")))?;
    let input: String = oids.map(|o| format!("{o}\n")).collect();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child
        .wait_with_output()
        .map_err(|e| ScanError(format!("can't read git cat-file: {e}")))?;
    if !matches!(writer.join(), Ok(Ok(()))) || !out.status.success() {
        return Err(ScanError("git cat-file failed".into()));
    }
    let mut sizes = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((oid, size)) = line.split_once(' ') else {
            return Err(ScanError("an object to scan is missing".into()));
        };
        let size = size
            .parse()
            .map_err(|_| ScanError("an object to scan is missing".into()))?;
        sizes.insert(oid.to_string(), size);
    }
    Ok(sizes)
}

/// What a commit an agent's shell is about to make at `dir` would add
/// (`tool.before`, before git stages anything): its whole index; the
/// working-tree changes of its pathspecs, or of every tracked file for
/// `-a` (`stages`, read from its words and the line's `git add`s); and
/// the whole of each untracked, unignored file its pathspecs name, which
/// a `git add` there would bring in.
pub fn scan_commit(
    dir: &Path,
    stages: &mdkg_run::harness::hook::event::Stages,
) -> Result<Scanned, ScanError> {
    let mut scanned = scan_staged(dir)?;
    if scanned.finding.is_some() || (!stages.all && stages.paths.is_empty()) {
        return Ok(scanned);
    }
    let top = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .ok_or_else(|| ScanError("can't find the worktree's top".into()))?;
    let git = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        cmd
    };
    let paths: &[String] = if stages.all { &[] } else { &stages.paths };
    let worktree = scan_diff(git, &["diff"], paths, Some(&top), "")?;
    scanned.skipped.extend(worktree.skipped);
    scanned.finding = worktree.finding;
    if scanned.finding.is_some() || stages.paths.is_empty() {
        return Ok(scanned);
    }
    // Untracked files the pathspecs name, unignored; ignored ones too only
    // under the paths a `git add -f` named (review of 161901be).
    let list = |paths: &[String], exclude: bool| -> Result<Vec<u8>, ScanError> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["ls-files", "-z", "--others", "--full-name"])
            .args(exclude.then_some("--exclude-standard"))
            .arg("--")
            .args(paths)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| ScanError(format!("can't run git ls-files: {e}")))?;
        if !out.status.success() {
            return Err(ScanError("git ls-files failed".into()));
        }
        Ok(out.stdout)
    };
    let mut names = list(&stages.paths, true)?;
    if !stages.forced.is_empty() {
        names.push(0);
        names.extend(list(&stages.forced, false)?);
    }
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let path = String::from_utf8_lossy(name).into_owned();
        match untracked_secret(&top.join(&path))? {
            Untracked::Secret => {
                scanned.finding = Some(Finding {
                    commit: String::new(),
                    path,
                });
                return Ok(scanned);
            }
            Untracked::TooLarge => scanned.skipped.push(path),
            Untracked::Clean => {}
        }
    }
    Ok(scanned)
}

/// What an untracked file holds, for the scan.
enum Untracked {
    Secret,
    Clean,
    /// Larger than [`BLOB_MAX`], so not read.
    TooLarge,
}

/// Whether an untracked file's lines hold a credential-shaped string. A
/// link isn't followed (git adds the link, not its target); every other
/// file is read as text, as the diffs are.
fn untracked_secret(path: &Path) -> Result<Untracked, ScanError> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| ScanError(format!("can't read an untracked file: {}", e.kind())))?;
    if !meta.is_file() {
        return Ok(Untracked::Clean);
    }
    if meta.len() > BLOB_MAX {
        return Ok(Untracked::TooLarge);
    }
    let bytes = std::fs::read(path)
        .map_err(|e| ScanError(format!("can't read an untracked file: {}", e.kind())))?;
    let secret = bytes
        .split(|b| *b == b'\n')
        .any(|line| looks_secret(&String::from_utf8_lossy(line)));
    Ok(if secret {
        Untracked::Secret
    } else {
        Untracked::Clean
    })
}

/// The commits a push an agent's shell is about to run from `dir` would
/// send (`tool.before`): what its sources (`HEAD` when it names none, every
/// branch for `all`) have that `remote`'s tracking refs don't.
pub fn outgoing_from(
    dir: &Path,
    remote: Option<&str>,
    sources: &[String],
    all: bool,
) -> Result<(Vec<String>, bool), ScanError> {
    let remote = remote.and_then(|r| tracking_remote(dir, r));
    let full = !remote.as_deref().is_some_and(|r| has_tracking(dir, r));
    let mut args = vec![
        "rev-list".to_string(),
        "--topo-order".into(),
        "--reverse".into(),
    ];
    if all {
        args.push("--branches".into());
    } else if sources.is_empty() {
        args.push("HEAD".into());
    } else {
        if sources.iter().any(|s| s.starts_with('-')) {
            return Err(ScanError("a push source reads as an option".into()));
        }
        args.extend(sources.iter().cloned());
    }
    args.push("--not".into());
    if let Some(r) = remote.as_deref() {
        args.push(format!("--remotes={r}"));
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(&args)
        .arg("--")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| ScanError(format!("can't run git rev-list: {e}")))?;
    if !out.status.success() {
        return Err(ScanError("git rev-list failed".into()));
    }
    Ok((
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(String::from)
            .collect(),
        full,
    ))
}

/// What `commits` add, each against its parents (a merge only what it
/// adds beyond them, as `--cc` shows it), read with the `git` binary given
/// in the repository whose git folder is `git_dir`. The first finding, in
/// the order given. This is git-serve's `Scanner::scan` shape (S34), so
/// its receiving hook runs the same scan.
pub fn scan(git: &Path, git_dir: &Path, commits: &[String]) -> Result<Scanned, ScanError> {
    if commits
        .iter()
        .any(|c| c.is_empty() || !c.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(ScanError("a commit to scan isn't a commit id".into()));
    }
    let mut scanned = Scanned::default();
    if commits.is_empty() {
        return Ok(scanned);
    }
    let run = |args: &[&str]| {
        let mut cmd = Command::new(git);
        cmd.arg("--git-dir").arg(git_dir).args(args);
        cmd
    };
    // One git for all the commits (review of 8cf3c619); one with a blob
    // over the cap gets a diff of its own, which excludes that blob.
    let big = oversized_commits(&run, commits)?;
    let batch: Vec<String> = commits
        .iter()
        .filter(|c| !big.contains_key(c.as_str()))
        .cloned()
        .collect();
    if !batch.is_empty() {
        let mut cmd = run(&["diff-tree", "--stdin", "-p", "-r", "--root", "--cc"]);
        cmd.args(DIFF);
        if let Some((commit, path)) = first_added_in(cmd, &batch)? {
            scanned.finding = Some(Finding { commit, path });
            return Ok(scanned);
        }
    }
    for commit in commits.iter().filter(|c| big.contains_key(c.as_str())) {
        let base = [
            "diff-tree",
            "-r",
            "--root",
            "--cc",
            "--no-commit-id",
            commit.as_str(),
        ];
        let one = scan_diff(run, &base, &[], None, commit)?;
        scanned.skipped.extend(
            one.skipped
                .into_iter()
                .map(|p| format!("{}:{p}", &commit[..commit.len().min(12)])),
        );
        if one.finding.is_some() {
            scanned.finding = one.finding;
            return Ok(scanned);
        }
    }
    Ok(scanned)
}

/// The commits, of `commits`, that add a blob over [`BLOB_MAX`]: one
/// `diff-tree --stdin --raw` and one `cat-file --batch-check` for all.
fn oversized_commits(
    git: &impl Fn(&[&str]) -> Command,
    commits: &[String],
) -> Result<std::collections::HashMap<String, ()>, ScanError> {
    use std::io::Write as _;
    let mut child = git(&[
        "diff-tree",
        "--stdin",
        "--always",
        "-r",
        "--root",
        "--cc",
        "--raw",
        "-z",
        "--no-abbrev",
        "--no-renames",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .map_err(|e| ScanError(format!("can't run git: {e}")))?;
    let input: String = commits.iter().map(|c| format!("{c}\n")).collect();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child
        .wait_with_output()
        .map_err(|e| ScanError(format!("can't read git: {e}")))?;
    if !matches!(writer.join(), Ok(Ok(()))) || !out.status.success() {
        return Err(ScanError("git couldn't list the changes".into()));
    }
    // A field with no leading `:` is the next commit's id (`--always`
    // prints every commit it reads, so one git skipped isn't there and
    // refuses); then each change is `:<modes> <shas> <status>`, NUL, its
    // path.
    let mut seen = std::collections::HashSet::new();
    let mut current = String::new();
    let mut blobs: Vec<(String, String)> = Vec::new();
    let mut fields = out.stdout.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        let field = String::from_utf8_lossy(field);
        let Some(meta) = field.strip_prefix(':') else {
            current = field.trim().to_string();
            seen.insert(current.clone());
            continue;
        };
        if fields.next().is_none() {
            return Err(ScanError("git's change listing is cut short".into()));
        }
        let words: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let (Some(status), Some(oid)) =
            (words.last(), words.len().checked_sub(2).map(|i| words[i]))
        else {
            return Err(ScanError("git's change listing is unreadable".into()));
        };
        if !status.starts_with('D') && !oid.bytes().all(|b| b == b'0') {
            blobs.push((current.clone(), oid.to_string()));
        }
    }
    if commits.iter().any(|c| !seen.contains(c)) {
        return Err(ScanError("a commit to scan isn't here".into()));
    }
    let mut big = std::collections::HashMap::new();
    if !blobs.is_empty() {
        let sizes = blob_sizes(git, blobs.iter().map(|(_, o)| o.as_str()))?;
        for (commit, oid) in &blobs {
            if sizes.get(oid.as_str()).is_some_and(|&n| n > BLOB_MAX) {
                big.insert(commit.clone(), ());
            }
        }
    }
    Ok(big)
}

/// The commits a push sends: for each update, the commits its new tip has
/// that the remote's tip (when the remote has the ref) and the remote's
/// tracking refs don't. A deletion sends none. `remote` is pre-push's
/// first argument: a remote's name, whose tracking refs git keeps, or a
/// URL, which has none.
///
/// Also whether anything bounded them: a push URL names the configured
/// remote with that URL, and when that remote has no tracking refs here
/// and an update's remote tip isn't here either, every commit reachable
/// is sent as far as this clone can tell (review of 8cf3c619), so the
/// scan read the whole history.
pub fn outgoing(
    top: &Path,
    remote: Option<&str>,
    updates: &[super::git::Update],
) -> Result<(Vec<String>, bool), ScanError> {
    let zero = |s: &str| !s.is_empty() && s.bytes().all(|b| b == b'0');
    let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    let remote = remote.and_then(|r| tracking_remote(top, r));
    let tracked = remote.as_deref().is_some_and(|r| has_tracking(top, r));
    let mut full = false;
    let mut all: Vec<String> = Vec::new();
    for u in updates.iter().filter(|u| !zero(&u.local_sha)) {
        if !hex(&u.local_sha) || !hex(&u.remote_sha) {
            return Err(ScanError("pre-push's input names a commit oddly".into()));
        }
        let mut args = vec![
            "rev-list".to_string(),
            "--topo-order".into(),
            "--reverse".into(),
            u.local_sha.clone(),
            "--not".into(),
        ];
        // The remote's tip may not be here (a fetch never brought it): then
        // the tracking refs alone bound what's sent.
        let tip = !zero(&u.remote_sha) && has_commit(top, &u.remote_sha);
        if tip {
            args.push(u.remote_sha.clone());
        }
        full |= !tip && !tracked;
        if let Some(r) = remote.as_deref() {
            args.push(format!("--remotes={r}"));
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(top)
            .args(&args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| ScanError(format!("can't run git rev-list: {e}")))?;
        if !out.status.success() {
            return Err(ScanError("git rev-list failed".into()));
        }
        for c in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            if !all.iter().any(|x| x == c) {
                all.push(c.to_string());
            }
        }
    }
    Ok((all, full))
}

/// The configured remote a push names: the name itself, or the remote
/// whose `url` or `pushurl` is the URL given. `None` for a URL no remote
/// has, or a name that isn't safe in a ref pattern.
fn tracking_remote(top: &Path, remote: &str) -> Option<String> {
    if remote.is_empty() || remote.starts_with('-') || remote.contains(['*', '?', '[']) {
        return None;
    }
    let config = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(top)
            .arg("config")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    if config(&["--get", &format!("remote.{remote}.url")]).is_some() {
        return Some(remote.to_string());
    }
    let listed = config(&["--get-regexp", r"^remote\..*\.(push)?url$"])?;
    listed.lines().find_map(|l| {
        let (key, value) = l.split_once(' ')?;
        let name = key.strip_prefix("remote.")?.rsplit_once('.')?.0;
        (value == remote && !name.contains(['*', '?', '['])).then(|| name.to_string())
    })
}

/// Whether this clone has any tracking ref of `remote`.
fn has_tracking(top: &Path, remote: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(top)
        .args([
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            &format!("refs/remotes/{remote}/"),
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

fn has_commit(top: &Path, sha: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(top)
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs a diff command and reads its patch as it streams: the first path
/// an added line holding a credential-shaped string goes to. The command
/// is stopped at the first finding.
fn first_added(cmd: Command) -> Result<Option<String>, ScanError> {
    Ok(first_added_in(cmd, &[])?.map(|(_, path)| path))
}

/// [`first_added`] for `git diff-tree --stdin`, fed `commits`: each
/// commit's patch follows a line holding its id, so the finding names the
/// commit too (review of 8cf3c619: one git for every commit).
fn first_added_in(
    mut cmd: Command,
    commits: &[String],
) -> Result<Option<(String, String)>, ScanError> {
    use std::io::Write as _;
    let mut child = cmd
        .stdin(if commits.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ScanError(format!("can't run git: {e}")))?;
    let writer = child.stdin.take().map(|mut stdin| {
        let input: String = commits.iter().map(|c| format!("{c}\n")).collect();
        std::thread::spawn(move || stdin.write_all(input.as_bytes()))
    });
    let stdout = child.stdout.take().expect("piped stdout");
    let ids: std::collections::HashSet<&str> = commits.iter().map(String::as_str).collect();
    let found = added_secret(BufReader::new(stdout), &ids);
    // A finding stops git early, so the writer may see a closed pipe.
    let wrote = writer.map(|w| matches!(w.join(), Ok(Ok(()))));
    if matches!(found, Ok(Some(_))) {
        child.kill().ok();
        child.wait().ok();
        return found.map_err(|e| ScanError(format!("can't read git's diff: {e}")));
    }
    let status = child
        .wait()
        .map_err(|e| ScanError(format!("can't wait for git: {e}")))?;
    let found = found.map_err(|e| ScanError(format!("can't read git's diff: {e}")))?;
    if !status.success() || wrote == Some(false) {
        return Err(ScanError("git diff failed".into()));
    }
    Ok(found)
}

/// The first commit and path in a patch (`git diff -p`, or `--cc` for a
/// merge) one of whose added lines looks secret. An added line is one
/// every parent's column marks `+`: in a merge, a line from one side isn't
/// new. A line that is one of `ids` starts that commit's patch (`git
/// diff-tree --stdin`); with no ids the commit is empty.
fn added_secret(
    mut patch: impl BufRead,
    ids: &std::collections::HashSet<&str>,
) -> std::io::Result<Option<(String, String)>> {
    let mut commit = String::new();
    let mut path: Option<String> = None;
    // The parents' columns of the hunk being read; `None` between hunks.
    let mut columns: Option<usize> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if patch.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        if let Some(id) = std::str::from_utf8(text).ok().filter(|t| ids.contains(t)) {
            commit = id.to_string();
            path = None;
            columns = None;
        } else if text.starts_with(b"diff ") {
            path = None;
            columns = None;
        } else if text.starts_with(b"@@") {
            let ats = text.iter().take_while(|&&b| b == b'@').count();
            columns = Some(ats.saturating_sub(1).max(1));
        } else if let Some(n) = columns {
            let (prefix, rest) = text.split_at(n.min(text.len()));
            if prefix.len() == n
                && prefix.iter().all(|&b| b == b'+')
                && looks_secret(&String::from_utf8_lossy(rest))
            {
                return Ok(Some((commit, path.unwrap_or_default())));
            }
        } else if let Some(name) = text.strip_prefix(b"+++ ") {
            path = target(name);
        }
    }
}

/// A `+++` line's path: `b/<path>`, C-quoted when git quotes it;
/// `/dev/null` (a deletion) is none. git ends an unquoted name holding a
/// space with a tab, which isn't the path's (a tab in a path is quoted).
fn target(name: &[u8]) -> Option<String> {
    let name = if name.first() == Some(&b'"') {
        unquote(name)?
    } else {
        name.strip_suffix(b"\t").unwrap_or(name).to_vec()
    };
    let path = name.strip_prefix(b"b/")?;
    Some(String::from_utf8_lossy(path).into_owned())
}

/// A path git quoted: `"..."` with C escapes and octal bytes.
fn unquote(quoted: &[u8]) -> Option<Vec<u8>> {
    let inner = quoted.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        if inner[i] != b'\\' {
            out.push(inner[i]);
            i += 1;
            continue;
        }
        let c = *inner.get(i + 1)?;
        let byte = match c {
            b'a' => 7,
            b'b' => 8,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 11,
            b'f' => 12,
            b'r' => b'\r',
            b'"' | b'\\' => c,
            b'0'..=b'7' => {
                let digits = inner.get(i + 1..i + 4)?;
                let s = std::str::from_utf8(digits).ok()?;
                let byte = u8::from_str_radix(s, 8).ok()?;
                out.push(byte);
                i += 4;
                continue;
            }
            _ => return None,
        };
        out.push(byte);
        i += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");

    fn found(patch: &str) -> Option<String> {
        added_secret(patch.as_bytes(), &Default::default())
            .expect("read")
            .map(|(_, path)| path)
    }

    #[test]
    fn an_added_line_is_found_by_its_path_and_other_lines_are_not() {
        let patch = format!(
            "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old {TOKEN}\n+new\n\
             diff --git a/src/k.rs b/src/k.rs\n--- a/src/k.rs\n+++ b/src/k.rs\n@@ -0,0 +1,2 @@\n+let a = 1;\n+let k = \"{TOKEN}\";\n"
        );
        assert_eq!(found(&patch), Some("src/k.rs".into()));
        // A removed line, a context line, and a header that only looks
        // like an added line aren't additions.
        let patch =
            format!("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,1 @@\n-{TOKEN}\n {TOKEN}\n");
        assert_eq!(found(&patch), None);
        // An added line that itself starts `+++ ` is content, not a header.
        let patch = format!("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -0,0 +1 @@\n++++ {TOKEN}\n");
        assert_eq!(found(&patch), Some("x".into()));
    }

    #[test]
    fn a_merge_adds_only_lines_new_against_every_parent() {
        // `+ ` came from the second parent, ` +` from the first: neither
        // is new. `++` is the merge's own.
        let patch = format!(
            "diff --cc m.txt\nindex 1,2..3\n--- a/m.txt\n+++ b/m.txt\n@@@ -1,1 -1,1 +1,3 @@@\n+ {TOKEN}\n +{TOKEN}\n  same\n"
        );
        assert_eq!(found(&patch), None);
        let patch = format!(
            "diff --cc m.txt\n--- a/m.txt\n+++ b/m.txt\n@@@ -1,1 -1,1 +1,2 @@@\n++{TOKEN}\n"
        );
        assert_eq!(found(&patch), Some("m.txt".into()));
    }

    #[test]
    fn quoted_paths_are_read_back_and_a_deletion_names_none() {
        assert_eq!(target(b"b/plain.txt"), Some("plain.txt".into()));
        assert_eq!(target(b"b/with space.txt\t"), Some("with space.txt".into()));
        assert_eq!(
            target(br#""b/tab\there.txt""#),
            Some("tab\there.txt".into())
        );
        assert_eq!(target(br#""b/caf\303\251""#), Some("café".into()));
        assert_eq!(target(br#""b/q\"uote""#), Some("q\"uote".into()));
        assert_eq!(target(b"/dev/null"), None);
        assert_eq!(target(br#""b/bad\q""#), None);
    }

    /// A throwaway folder, removed when dropped.
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn git(top: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(top)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_DIR")
            .output()
            .expect("git");
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn staged_changes_and_commits_are_scanned_and_a_move_adds_nothing() {
        let dir = std::env::temp_dir().join(format!("mdkg-secret-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("temp");
        let _scratch = Scratch(dir.clone());
        let top = dir.canonicalize().expect("canonical");
        git(&top, &["init", "-q", "-b", "main"]);
        // Config that would hide or rewrite the patch changes nothing.
        git(&top, &["config", "diff.noprefix", "true"]);
        git(&top, &["config", "color.diff", "always"]);
        std::fs::write(top.join("a.txt"), "ordinary\n").expect("a");
        git(&top, &["add", "."]);
        assert_eq!(scan_staged(&top).map(|s| s.finding), Ok(None));
        git(&top, &["commit", "-qm", "a"]);
        let clean = git(&top, &["rev-parse", "HEAD"]);
        std::fs::write(top.join("key file.txt"), format!("x {TOKEN}\n")).expect("key");
        git(&top, &["add", "."]);
        assert_eq!(
            scan_staged(&top).map(|s| s.finding),
            Ok(Some(Finding {
                commit: String::new(),
                path: "key file.txt".into()
            }))
        );
        git(&top, &["commit", "-qm", "key"]);
        let keyed = git(&top, &["rev-parse", "HEAD"]);
        // Moving the file afterwards adds nothing new.
        git(&top, &["mv", "key file.txt", "moved.txt"]);
        git(&top, &["commit", "-qm", "move"]);
        let moved = git(&top, &["rev-parse", "HEAD"]);
        let git_dir = top.join(".git");
        let bin = Path::new("git");
        let found = |commits: &[String]| scan(bin, &git_dir, commits).map(|s| s.finding);
        assert_eq!(found(std::slice::from_ref(&clean)), Ok(None));
        assert_eq!(found(std::slice::from_ref(&moved)), Ok(None));
        assert_eq!(
            found(&[clean.clone(), keyed.clone(), moved]),
            Ok(Some(Finding {
                commit: keyed.clone(),
                path: "key file.txt".into()
            }))
        );
        // A commit that isn't one fails the scan, which callers refuse.
        assert!(
            scan(
                bin,
                &git_dir,
                &["0123456789abcdef0123456789abcdef01234567".into()]
            )
            .is_err()
        );
        assert!(scan(bin, &git_dir, &["HEAD --output=x".into()]).is_err());
        // A push of main to a remote that has the first commit sends the
        // other two.
        let update = |local: &str, remote: &str| super::super::git::Update {
            local_ref: "refs/heads/main".into(),
            local_sha: local.into(),
            remote_ref: "refs/heads/main".into(),
            remote_sha: remote.into(),
        };
        let head = git(&top, &["rev-parse", "HEAD"]);
        let sent = outgoing(&top, Some("origin"), &[update(&head, &clean)]).expect("outgoing");
        assert_eq!(sent, (vec![keyed.clone(), head.clone()], false));
        let zero = "0".repeat(40);
        // A first push to a remote with no tracking refs reads the whole
        // history, and says so (review of 8cf3c619).
        let new = outgoing(&top, Some("origin"), &[update(&head, &zero)]).expect("new branch");
        assert_eq!(
            new,
            (vec![clean.clone(), keyed.clone(), head.clone()], true)
        );
        // Through a URL, the configured remote with that URL bounds it.
        git(&top, &["remote", "add", "origin", "/nowhere/origin.git"]);
        git(&top, &["update-ref", "refs/remotes/origin/main", &keyed]);
        let url =
            outgoing(&top, Some("/nowhere/origin.git"), &[update(&head, &zero)]).expect("by url");
        assert_eq!(url, (vec![head.clone()], false));
        assert!(
            outgoing(&top, None, &[update(&zero, &head)])
                .expect("delete")
                .0
                .is_empty()
        );
    }

    #[test]
    fn a_commit_about_to_run_is_scanned_for_what_it_would_stage() {
        use mdkg_run::harness::hook::event::Stages;
        let dir = std::env::temp_dir().join(format!("mdkg-secret-tool-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("sub")).expect("temp");
        let _scratch = Scratch(dir.clone());
        let top = dir.canonicalize().expect("canonical");
        git(&top, &["init", "-q", "-b", "main"]);
        std::fs::write(top.join("tracked.txt"), "ordinary\n").expect("tracked");
        git(&top, &["add", "."]);
        git(&top, &["commit", "-qm", "a"]);
        let only = |paths: &[&str]| Stages {
            paths: paths.iter().map(|p| p.to_string()).collect(),
            ..Stages::default()
        };
        let found = |stages: &Stages, at: &Path| {
            scan_commit(at, stages)
                .expect("scan")
                .finding
                .map(|f| f.path)
        };
        // A tracked change: found for `-a` or its pathspec, not otherwise.
        std::fs::write(top.join("tracked.txt"), format!("{TOKEN}\n")).expect("change");
        assert_eq!(found(&Stages::default(), &top), None);
        let all = Stages {
            all: true,
            ..Stages::default()
        };
        assert_eq!(found(&all, &top), Some("tracked.txt".into()));
        assert_eq!(
            found(&only(&["tracked.txt"]), &top),
            Some("tracked.txt".into())
        );
        assert_eq!(found(&only(&["other.txt"]), &top), None);
        git(&top, &["checkout", "--", "tracked.txt"]);
        // An untracked file a pathspec names, from a subfolder too, and
        // `:/` for `git add -A`; an ignored one isn't added.
        std::fs::write(top.join("sub/new.txt"), format!("k {TOKEN}\n")).expect("new");
        assert_eq!(found(&all, &top), None, "-a takes no untracked file");
        assert_eq!(
            found(&only(&["new.txt"]), &top.join("sub")),
            Some("sub/new.txt".into())
        );
        assert_eq!(found(&only(&[":/"]), &top), Some("sub/new.txt".into()));
        std::fs::write(top.join(".gitignore"), "sub/\n").expect("ignore");
        assert_eq!(found(&only(&[":/"]), &top), None);
        // `git add -f` takes it anyway (review of 689b8498).
        let forced = Stages {
            forced: vec!["sub/new.txt".into()],
            ..only(&["sub/new.txt"])
        };
        assert_eq!(found(&forced, &top), Some("sub/new.txt".into()));
        // ...and only under the paths it forced (review of 161901be).
        let elsewhere = Stages {
            forced: vec!["other.txt".into()],
            ..only(&[":/"])
        };
        assert_eq!(found(&elsewhere, &top), None);
        // A binary untracked file is read as text, as the diffs are.
        std::fs::remove_file(top.join(".gitignore")).expect("unignore");
        std::fs::write(top.join("sub/new.txt"), format!("\0{TOKEN}\n")).expect("binary");
        assert_eq!(found(&only(&[":/"]), &top), Some("sub/new.txt".into()));
    }

    /// #11600: every blob is read as text, whatever git or its attributes
    /// call it, and one over the 16 MiB cap is named as unscanned, not
    /// read, so it passes with a warning.
    #[test]
    fn binary_and_diff_off_blobs_are_read_and_oversized_ones_are_named() {
        let dir = std::env::temp_dir().join(format!("mdkg-secret-text-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("temp");
        let _scratch = Scratch(dir.clone());
        let top = dir.canonicalize().expect("canonical");
        git(&top, &["init", "-q", "-b", "main"]);
        std::fs::write(top.join(".gitattributes"), "*.json -diff\n").expect("attributes");
        git(&top, &["add", "."]);
        git(&top, &["commit", "-qm", "attributes"]);
        let git_dir = top.join(".git");
        let bin = Path::new("git");
        for (path, text) in [
            ("cache.sqlite", format!("\0\0SQLite\0{TOKEN}\0")),
            ("keys.json", format!("{{\"k\":\"{TOKEN}\"}}\n")),
        ] {
            std::fs::write(top.join(path), text).expect("write");
            git(&top, &["add", path]);
            let staged = scan_staged(&top).expect("scan");
            assert_eq!(staged.finding.map(|f| f.path).as_deref(), Some(path));
            git(&top, &["commit", "-qm", path]);
            let commit = git(&top, &["rev-parse", "HEAD"]);
            let pushed = scan(bin, &git_dir, std::slice::from_ref(&commit)).expect("scan");
            assert_eq!(pushed.finding.map(|f| f.path).as_deref(), Some(path));
        }
        // Over the cap: not read, named, and nothing found.
        let mut big = vec![b'x'; (BLOB_MAX + 1) as usize];
        big.extend_from_slice(format!("\n{TOKEN}\n").as_bytes());
        std::fs::write(top.join("big.bin"), &big).expect("big");
        git(&top, &["add", "big.bin"]);
        let staged = scan_staged(&top).expect("scan");
        assert_eq!(
            (staged.finding, staged.skipped),
            (None, vec!["big.bin".to_string()])
        );
        git(&top, &["commit", "-qm", "big"]);
        let commit = git(&top, &["rev-parse", "HEAD"]);
        let pushed = scan(bin, &git_dir, std::slice::from_ref(&commit)).expect("scan");
        assert_eq!(pushed.finding, None);
        assert_eq!(pushed.skipped, [format!("{}:big.bin", &commit[..12])]);
        // A token beside the oversized blob, in the same commit, is found.
        std::fs::write(top.join("big.bin"), [&big[..], b"more\n"].concat()).expect("bigger");
        std::fs::write(top.join("small.txt"), format!("{TOKEN}\n")).expect("small");
        git(&top, &["add", "."]);
        let staged = scan_staged(&top).expect("scan");
        assert_eq!(staged.finding.map(|f| f.path).as_deref(), Some("small.txt"));
        assert_eq!(staged.skipped, ["big.bin"]);
    }

    /// Review of cc6e651a: an oversized blob whose name isn't UTF-8 is
    /// excluded by its own bytes, so its look-alike is still read.
    #[test]
    fn an_oversized_blob_with_a_non_utf8_name_is_excluded_as_itself() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("mdkg-secret-raw-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("temp");
        let _scratch = Scratch(dir.clone());
        let top = dir.canonicalize().expect("canonical");
        git(&top, &["init", "-q", "-b", "main"]);
        // Into the index by plumbing: a file system may refuse the name
        // (APFS does), but git holds any bytes but NUL and `/`.
        let blob = |bytes: &[u8]| {
            let mut child = Command::new("git")
                .arg("-C")
                .arg(&top)
                .args(["hash-object", "-w", "--stdin"])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("git");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(bytes)
                .expect("write");
            let out = child.wait_with_output().expect("wait");
            String::from_utf8(out.stdout)
                .expect("oid")
                .trim()
                .to_string()
        };
        let big = blob(&vec![b'x'; (BLOB_MAX + 1) as usize]);
        let keyed = blob(format!("{TOKEN}\n").as_bytes());
        let mut info = Vec::new();
        info.extend_from_slice(format!("100644 {big}\t").as_bytes());
        info.extend_from_slice(b"big-\xff.bin\n");
        // Its lossy look-alike holds a token, and must still be read.
        info.extend_from_slice(format!("100644 {keyed}\tbig-\u{FFFD}.bin\n").as_bytes());
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&top)
            .args(["update-index", "--index-info"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .spawn()
            .expect("git");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(&info)
            .expect("write");
        assert!(child.wait().expect("wait").success());
        let staged = scan_staged(&top).expect("scan");
        assert_eq!(staged.skipped, ["big-\u{FFFD}.bin"]);
        assert_eq!(
            staged.finding.map(|f| f.path).as_deref(),
            Some("big-\u{FFFD}.bin"),
            "the look-alike is scanned"
        );
    }

    /// Review of 8cf3c619: the commits are scanned by one `diff-tree
    /// --stdin`, which names the commit a finding is in; a commit with a
    /// blob over the cap is scanned on its own, with that blob excluded.
    #[test]
    fn many_commits_are_scanned_in_one_pass_and_name_the_right_commit() {
        let dir = std::env::temp_dir().join(format!("mdkg-secret-batch-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("temp");
        let _scratch = Scratch(dir.clone());
        let top = dir.canonicalize().expect("canonical");
        git(&top, &["init", "-q", "-b", "main"]);
        let commit = |path: &str, bytes: &[u8]| {
            std::fs::write(top.join(path), bytes).expect("write");
            git(&top, &["add", path]);
            git(&top, &["commit", "-qm", path]);
            git(&top, &["rev-parse", "HEAD"])
        };
        let a = commit("a.txt", b"ordinary\n");
        let b = commit("b.txt", b"also ordinary\n");
        let mut huge = vec![b'x'; (BLOB_MAX + 1) as usize];
        huge.extend_from_slice(format!("\n{TOKEN}\n").as_bytes());
        let big = commit("big.bin", &huge);
        let keyed = commit("k.txt", format!("{TOKEN}\n").as_bytes());
        let d = commit("d.txt", b"after\n");
        let git_dir = top.join(".git");
        let bin = Path::new("git");
        let all = [a.clone(), b.clone(), big.clone(), keyed.clone(), d.clone()];
        let scanned = scan(bin, &git_dir, &all).expect("scan");
        assert_eq!(
            scanned.finding,
            Some(Finding {
                commit: keyed,
                path: "k.txt".into()
            })
        );
        let clean = scan(bin, &git_dir, &[a, b, big.clone(), d.clone()]).expect("scan");
        assert_eq!(clean.finding, None);
        assert_eq!(clean.skipped, [format!("{}:big.bin", &big[..12])]);
        // A merge in the batch is named too: its own line, new against
        // both parents, is found under the merge's id (review of
        // bc2f5bd8).
        git(&top, &["checkout", "-q", "-b", "side", &d]);
        let side = commit("side.txt", b"side\n");
        git(&top, &["checkout", "-q", "main"]);
        let main = commit("main.txt", b"main\n");
        git(&top, &["merge", "-q", "--no-commit", "--no-ff", "side"]);
        std::fs::write(top.join("side.txt"), format!("side\n{TOKEN}\n")).expect("evil merge");
        git(&top, &["add", "side.txt"]);
        git(&top, &["commit", "-qm", "merge"]);
        let merge = git(&top, &["rev-parse", "HEAD"]);
        let scanned =
            scan(bin, &git_dir, &[side.clone(), main.clone(), merge.clone()]).expect("scan");
        assert_eq!(
            scanned.finding,
            Some(Finding {
                commit: merge,
                path: "side.txt".into()
            })
        );
        assert_eq!(
            scan(bin, &git_dir, &[side, main]).expect("scan").finding,
            None
        );
    }
}
