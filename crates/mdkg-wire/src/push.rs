//! The push runner's private side (S38, Opus M6): a push never runs git in
//! the worker's repository, whose config a turn can write. It goes from a
//! private bare repository mdkg makes for that one push, to the binding's
//! git destination only.
//!
//! **One repository per push** (orchestrator on #10729, closing Terra 2
//! #6271's "the private push repository still reads its own config"):
//! git reads a repository's own config whatever else is switched off, so
//! a persistent repository that a same-user process edited could rewrite
//! the push's URL. Instead each push gets a fresh folder under
//! `<state>/mdkg/push/`, which mdkg writes itself with fixed bytes:
//! `HEAD`, `config`, empty `refs/`, and `objects/info/alternates` naming
//! the worker's common object folder, so the worker's commits can be sent
//! without copying them or reading its config. Just before git runs, the
//! whole tree is checked byte for byte, every entry, with nothing extra
//! (a `commondir` file, say, would point git elsewhere). The folder is
//! deleted when the push ends.
//!
//! **No git in the worker's repository** (Terra 2 #6271's major on commit
//! resolution): the commit is taken as a full hex SHA and checked in the
//! private repository through its alternates, so the worker's config,
//! promisor transport included, is never read.
//!
//! **The destination** is the binding's `git: {url, host_key, identity}`,
//! never a remote name: an ssh URL only, the host key written as the only
//! line of the push's own known-hosts file, looked up by a fixed alias,
//! the identity named outright, and the system's ssh by absolute path, so
//! no `PATH` picks it ([`PrivateRepo::ssh_command`]; Terra 2 #10743).
//!
//! **The runner** ([`BoundPush`], L0's `PushRunner`) runs git only through
//! `mdkg-git`'s `bare_command` and `ssh_push` (S38's settings): it checks
//! the commit through the alternates, then pushes `<sha>:refs/heads/<branch>`
//! with no force. A refusal by the receiving side, its hooks or a
//! non-fast-forward, is `run.push_refused`. That the push reached the
//! enrolled server is the caller's check, at the coordinator (S28).
//!
//! **Residuals.** A same-user process could edit the push's folder in the
//! moment between [`PrivateRepo::check`] and git reading it; the folder is
//! 0700, randomly named and lives for one push, and a turn's sandbox
//! doesn't write to the state folder. Git also follows the worker's own
//! `objects/info/alternates`, which a turn can write: that changes only
//! where objects are found, never config, and the commit pushed is the
//! one named (Terra 2 #10743).

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use mdkg_format::{Code, Diagnostic};
use mdkg_ops::wire::PushRunner;

use crate::keys;

/// The binding's git destination (S38): where pushes go, and nowhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDestination {
    /// `ssh://[user@]host[:port]/path` or `[user@]host:path`.
    pub url: String,
    /// The host's public key, `<type> <base64>`, as `ssh-keyscan` prints
    /// it without the host name.
    pub host_key: String,
    /// The private key file ssh authenticates with, an absolute path. It
    /// must work without an agent and without a passphrase: a push runs
    /// ssh with every `SSH_*` variable removed, `SSH_AUTH_SOCK` included
    /// (S38), and in `BatchMode`, so a key held only in ssh-agent or an
    /// agent-backed store, or a passphrase-protected file, fails every
    /// push (Terra 2 #10767).
    pub identity: PathBuf,
}

/// The name the host key is filed under in a push's known-hosts file, so
/// ssh checks the key whatever host or port the URL names.
pub const HOST_KEY_ALIAS: &str = "mdkg-push";

/// The folder pushes are made from: `<state>/mdkg/push`.
pub fn push_root(state: &Path) -> PathBuf {
    state.join("mdkg").join("push")
}

/// A repository's object format, from the commit's SHA length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

fn invalid(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why.into())
}

fn tampered(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} {why}; the push was refused", path.display()),
    )
}

/// A commit named by its full SHA, lowercase hex: 40 digits for SHA-1, 64
/// for SHA-256. Nothing a ref name or revision expression could resolve.
pub fn check_commit(sha: &str) -> io::Result<ObjectFormat> {
    let hex = sha
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    match sha.len() {
        40 if hex => Ok(ObjectFormat::Sha1),
        64 if hex => Ok(ObjectFormat::Sha256),
        _ => Err(invalid(
            "a commit to push is named by its full lowercase SHA",
        )),
    }
}

/// An ssh URL git takes as such and nothing else: `ssh://[user@]host[:port]/path`
/// or the scp-like `[user@]host:path`. Every part is checked here, not
/// left to git's own guards (Terra 2 #10743): no `<transport>::` form
/// (git's remote helpers, `ext::` running a command among them), no user
/// or host starting with `-` (an option to ssh), a port of digits only,
/// a host of letters, digits, `.`, `-` and `_` or a bracketed IPv6
/// address, a non-empty path, and no whitespace or control character.
pub fn check_url(url: &str) -> io::Result<()> {
    let refuse = || invalid("the git destination's URL isn't a plain ssh URL");
    if url.is_empty() || !url.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(refuse());
    }
    // `<transport>::<address>`, which git reads before any other form.
    let before_bracket = url.split('[').next().unwrap_or(url);
    if before_bracket.contains("::") {
        return Err(refuse());
    }
    let (authority, path) = if let Some(rest) = url.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/').ok_or_else(refuse)?;
        let (host, port) = split_port(authority_host(authority)?).ok_or_else(refuse)?;
        if port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
            return Err(refuse());
        }
        check_host(host)?;
        (authority, path)
    } else {
        if url.contains("://") {
            return Err(refuse());
        }
        // scp-like: the host, then a colon before any slash.
        let (authority, path) = scp_split(url).ok_or_else(refuse)?;
        if path.starts_with(':') {
            return Err(refuse());
        }
        check_host(authority_host(authority)?)?;
        (authority, path)
    };
    if path.is_empty() || authority.is_empty() {
        return Err(refuse());
    }
    Ok(())
}

/// The host (and port) part of `[user@]host[:port]`, the user checked.
fn authority_host(authority: &str) -> io::Result<&str> {
    let refuse = || invalid("the git destination's URL has a bad user or host");
    match authority.split_once('@') {
        None => Ok(authority),
        Some((user, host)) => {
            let ok = !user.is_empty()
                && !user.starts_with('-')
                && user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
            if ok && !host.contains('@') {
                Ok(host)
            } else {
                Err(refuse())
            }
        }
    }
}

/// `host[:port]`, a bracketed IPv6 host kept whole.
fn split_port(hostport: &str) -> Option<(&str, Option<&str>)> {
    if hostport.starts_with('[') {
        let end = hostport.find(']')?;
        let (host, rest) = hostport.split_at(end + 1);
        match rest {
            "" => Some((host, None)),
            _ => rest.strip_prefix(':').map(|p| (host, Some(p))),
        }
    } else {
        Some(match hostport.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        })
    }
}

/// The scp-like `[user@]host:path` split at the colon ending the host.
fn scp_split(url: &str) -> Option<(&str, &str)> {
    let (user, rest) = match url.split_once('@') {
        Some((u, r)) if !u.contains(['/', ':']) => (Some(u), r),
        _ => (None, url),
    };
    let colon = if rest.starts_with('[') {
        rest.find(']')? + 1
    } else {
        rest.find(':')?
    };
    if rest.get(colon..colon + 1) != Some(":") || rest[..colon].contains('/') {
        return None;
    }
    let authority_len = user.map_or(0, |u| u.len() + 1) + colon;
    Some((&url[..authority_len], &url[authority_len + 1..]))
}

fn check_host(host: &str) -> io::Result<()> {
    let ok = if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        !inner.is_empty()
            && inner
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b":.".contains(&b))
    } else {
        !host.is_empty()
            && !host.starts_with('-')
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
    };
    if ok {
        Ok(())
    } else {
        Err(invalid("the git destination's URL has a bad host"))
    }
}

/// Key types ssh's known-hosts file takes for a host.
const HOST_KEY_TYPES: [&str; 7] = [
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "ssh-rsa",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// A host key: one known type, then base64, nothing else.
pub fn check_host_key(key: &str) -> io::Result<()> {
    let mut parts = key.split(' ');
    let (Some(kind), Some(data), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(invalid(
            "the git destination's host key isn't `<type> <base64>`",
        ));
    };
    let base64 = !data.is_empty()
        && data
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b));
    if !HOST_KEY_TYPES.contains(&kind) || !base64 {
        return Err(invalid(
            "the git destination's host key isn't `<type> <base64>`",
        ));
    }
    Ok(())
}

/// `s` for `sh -c`, which runs `GIT_SSH_COMMAND`: in single quotes, and
/// refused if it holds a quote or a control character.
fn sh_quote(s: &str) -> io::Result<String> {
    if s.bytes().any(|b| b == b'\'' || b.is_ascii_control()) {
        return Err(invalid(
            "a path for ssh holds a quote or a control character",
        ));
    }
    Ok(format!("'{s}'"))
}

/// One entry of a private repository: what it must be, exactly.
enum Entry {
    Dir(Vec<(&'static str, Entry)>),
    File(Vec<u8>),
}

/// A push's own folder: the private bare repository and the known-hosts
/// file, removed when dropped.
#[derive(Debug)]
pub struct PrivateRepo {
    /// `<root>/<random>`, holding `repo.git` and `known_hosts`.
    dir: PathBuf,
    format: ObjectFormat,
    alternates: Vec<u8>,
    known_hosts: Vec<u8>,
}

impl PrivateRepo {
    /// Makes a fresh one under `root` (made 0700 if missing), whose
    /// alternates name `common`'s object folder.
    pub fn create(
        root: &Path,
        common: &Path,
        format: ObjectFormat,
        host_key: &str,
    ) -> io::Result<Self> {
        check_host_key(host_key)?;
        let objects = common.join("objects");
        let objects = objects
            .to_str()
            .filter(|p| objects.is_absolute() && !p.bytes().any(|b| b.is_ascii_control()))
            .ok_or_else(|| invalid("the repository's object folder can't go in alternates"))?;
        ensure_root(root)?;
        let name: String = keys::random::<16>()?
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let dir = root.join(name);
        DirBuilder::new().mode(0o700).create(&dir)?;
        let repo = Self {
            dir,
            format,
            alternates: format!("{objects}\n").into_bytes(),
            known_hosts: format!("{HOST_KEY_ALIAS} {host_key}\n").into_bytes(),
        };
        // Dropped on any failure below, which removes what was made.
        write_tree(&repo.dir, &repo.tree())?;
        Ok(repo)
    }

    /// The bare repository git runs in.
    pub fn git_dir(&self) -> PathBuf {
        self.dir.join("repo.git")
    }

    fn known_hosts_path(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }

    fn config(&self) -> &'static [u8] {
        match self.format {
            ObjectFormat::Sha1 => b"[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
            ObjectFormat::Sha256 => {
                b"[core]\n\trepositoryformatversion = 1\n\tbare = true\n[extensions]\n\tobjectformat = sha256\n"
            }
        }
    }

    /// Everything the push's folder holds, exactly.
    fn tree(&self) -> Vec<(&'static str, Entry)> {
        use Entry::{Dir, File};
        vec![
            (
                "repo.git",
                Dir(vec![
                    ("HEAD", File(b"ref: refs/heads/mdkg-push\n".to_vec())),
                    ("config", File(self.config().to_vec())),
                    (
                        "objects",
                        Dir(vec![
                            (
                                "info",
                                Dir(vec![("alternates", File(self.alternates.clone()))]),
                            ),
                            ("pack", Dir(vec![])),
                        ]),
                    ),
                    (
                        "refs",
                        Dir(vec![("heads", Dir(vec![])), ("tags", Dir(vec![]))]),
                    ),
                ]),
            ),
            ("known_hosts", File(self.known_hosts.clone())),
        ]
    }

    /// Checks the folder is still exactly what [`create`](Self::create)
    /// wrote, every entry, byte for byte, ours and closed to others, with
    /// nothing added. Run just before git.
    pub fn check(&self) -> io::Result<()> {
        check_owned(&self.dir, true)?;
        check_tree(&self.dir, &self.tree())
    }

    /// `GIT_SSH_COMMAND` for this push: no ssh config, only `identity`,
    /// and only the binding's host key, filed under [`HOST_KEY_ALIAS`]
    /// in this push's known-hosts file; no prompt.
    pub fn ssh_command(&self, ssh: &Path, identity: &Path) -> io::Result<String> {
        if !ssh.is_absolute() {
            return Err(invalid("ssh is named by an absolute path"));
        }
        let ssh = ssh
            .to_str()
            .ok_or_else(|| invalid("ssh's path isn't UTF-8"))?;
        if !identity.is_absolute() {
            return Err(invalid("the git identity is named by an absolute path"));
        }
        let identity = identity
            .to_str()
            .ok_or_else(|| invalid("the git identity's path isn't UTF-8"))?;
        let known = self.known_hosts_path();
        let known = known
            .to_str()
            .ok_or_else(|| invalid("the push folder's path isn't UTF-8"))?;
        Ok(format!(
            "{} -F none -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityFile={} \
             -o StrictHostKeyChecking=yes -o UserKnownHostsFile={} \
             -o GlobalKnownHostsFile=/dev/null -o HostKeyAlias={HOST_KEY_ALIAS}",
            sh_quote(ssh)?,
            sh_quote(identity)?,
            sh_quote(known)?,
        ))
    }
}

impl Drop for PrivateRepo {
    fn drop(&mut self) {
        // Ours, made for this push only; nothing else lives there.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The push root: made 0700 if missing; a folder of ours, closed to the
/// group and others, and not a link.
fn ensure_root(root: &Path) -> io::Result<()> {
    match fs::symlink_metadata(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            DirBuilder::new().recursive(true).mode(0o700).create(root)?;
        }
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    check_owned(root, true).map(|_| ())
}

fn check_owned(path: &Path, dir: bool) -> io::Result<fs::Metadata> {
    let m = fs::symlink_metadata(path)?;
    let kind_ok = if dir { m.is_dir() } else { m.is_file() };
    if !kind_ok || m.file_type().is_symlink() {
        return Err(tampered(path, "isn't what mdkg made"));
    }
    if m.uid() != rustix::process::geteuid().as_raw() {
        return Err(tampered(path, "is owned by another user"));
    }
    if m.mode() & 0o077 != 0 {
        return Err(tampered(path, "is open to the group or others"));
    }
    Ok(m)
}

/// Opens a file for reading without following a link or blocking on a
/// FIFO swapped in after the check.
fn open_nofollow(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

fn write_tree(dir: &Path, entries: &[(&'static str, Entry)]) -> io::Result<()> {
    for (name, entry) in entries {
        let path = dir.join(name);
        match entry {
            Entry::Dir(inner) => {
                DirBuilder::new().mode(0o700).create(&path)?;
                write_tree(&path, inner)?;
            }
            Entry::File(bytes) => {
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                f.write_all(bytes)?;
            }
        }
    }
    Ok(())
}

fn check_tree(dir: &Path, entries: &[(&'static str, Entry)]) -> io::Result<()> {
    let mut found: Vec<String> = Vec::new();
    for e in fs::read_dir(dir)? {
        found.push(e?.file_name().to_string_lossy().into_owned());
    }
    found.sort();
    let mut want: Vec<&str> = entries.iter().map(|(n, _)| *n).collect();
    want.sort_unstable();
    if found != want {
        return Err(tampered(dir, "holds other entries than mdkg made"));
    }
    for (name, entry) in entries {
        let path = dir.join(name);
        match entry {
            Entry::Dir(inner) => {
                check_owned(&path, true)?;
                check_tree(&path, inner)?;
            }
            Entry::File(bytes) => {
                let m = check_owned(&path, false)?;
                if m.len() != bytes.len() as u64 {
                    return Err(tampered(&path, "differs from what mdkg wrote"));
                }
                let mut got = Vec::with_capacity(bytes.len());
                open_nofollow(&path)?
                    .take(bytes.len() as u64 + 1)
                    .read_to_end(&mut got)?;
                if &got != bytes {
                    return Err(tampered(&path, "differs from what mdkg wrote"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// The hint on a push that failed short of the receiving side.
const PUSH_FAILED_HINT: &str = "check that the bound host is reachable and presents the bound host \
     key, and that the bound identity is a key file usable without ssh-agent or a passphrase";

/// Where OpenSSH's client lives on macOS and Linux.
const SYSTEM_SSH: [&str; 2] = ["/usr/bin/ssh", "/bin/ssh"];

/// The system's ssh, by absolute path: never one a `PATH` names.
pub fn system_ssh() -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    SYSTEM_SSH
        .iter()
        .map(PathBuf::from)
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no ssh in /usr/bin or /bin"))
}

/// A branch git takes as a plain name under `refs/heads/`, by
/// `git check-ref-format`'s rules, checked here.
pub fn check_branch(branch: &str) -> io::Result<()> {
    let bad_byte = |b: u8| b.is_ascii_control() || b" ~^:?*[\\\x7f".contains(&b);
    let ok = !branch.is_empty()
        && !branch.starts_with(['-', '/', '.'])
        && !branch.ends_with(['/', '.'])
        && !branch.ends_with(".lock")
        && !branch.contains("..")
        && !branch.contains("//")
        && !branch.contains("@{")
        && !branch.contains("/.")
        && branch != "@"
        && !branch.bytes().any(bad_byte)
        && branch.split('/').all(|c| !c.ends_with(".lock"));
    if ok {
        Ok(())
    } else {
        Err(invalid("the branch to push isn't a plain branch name"))
    }
}

/// The push runner for a bound clone (S38): to its git destination only,
/// from a private repository made for each push.
///
/// A push that fails for any other reason than the receiving side's
/// refusal is `io.git_failed`, hinting at what it most often is: the host
/// unreachable, its key not the bound one, or an identity that needs an
/// agent or a passphrase ([`GitDestination::identity`]).
#[derive(Debug, Clone)]
pub struct BoundPush {
    destination: GitDestination,
    /// The folder pushes are made from ([`push_root`]).
    root: PathBuf,
    ssh: PathBuf,
}

impl BoundPush {
    /// Pushes to `destination` from `<state>/mdkg/push`, with the system's
    /// ssh. The destination is checked here, before any push.
    pub fn new(destination: GitDestination, state: &Path) -> io::Result<Self> {
        Self::with_ssh(destination, state, system_ssh()?)
    }

    /// As [`new`](Self::new), with `ssh` as the ssh program (tests).
    pub fn with_ssh(destination: GitDestination, state: &Path, ssh: PathBuf) -> io::Result<Self> {
        check_url(&destination.url)?;
        check_host_key(&destination.host_key)?;
        if !destination.identity.is_absolute() || !ssh.is_absolute() {
            return Err(invalid(
                "ssh and the git identity are named by absolute paths",
            ));
        }
        Ok(Self {
            destination,
            root: push_root(state),
            ssh,
        })
    }

    fn run(&self, checkout: &Path, branch: &str, commit: &str) -> Result<(), Box<Diagnostic>> {
        let usage = |e: io::Error| {
            Box::new(Diagnostic::error(
                Code::UsageInvalidArguments,
                e.to_string(),
            ))
        };
        let git_failed =
            |e: io::Error| Box::new(Diagnostic::error(Code::IoGitFailed, e.to_string()));
        check_branch(branch).map_err(usage)?;
        let format = check_commit(commit).map_err(usage)?;
        // The worktree's common folder, through mdkg-git's own check,
        // which runs no transport; nothing else is read from the worker.
        let common = mdkg_git::check_worktree(checkout)
            .map_err(git_failed)?
            .common;
        let repo = PrivateRepo::create(&self.root, &common, format, &self.destination.host_key)
            .map_err(git_failed)?;
        repo.check().map_err(git_failed)?;
        let found = mdkg_git::bare_command(&repo.git_dir())
            .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .run(None)
            .map_err(git_failed)?;
        if !found.status.success() {
            return Err(Box::new(
                Diagnostic::error(
                    Code::CoordCommitUnknown,
                    format!(
                        "the commit to push isn't in the repository of {}",
                        checkout.display()
                    ),
                )
                .with_hint("commit it on the branch first"),
            ));
        }
        let ssh = repo
            .ssh_command(&self.ssh, &self.destination.identity)
            .map_err(usage)?;
        let refspec = format!("{commit}:refs/heads/{branch}");
        // Checked again just before the push: nothing changed it since.
        repo.check().map_err(git_failed)?;
        let out = mdkg_git::bare_command(&repo.git_dir())
            .ssh_push(&ssh)
            .args(["push", "--porcelain", "--", &self.destination.url, &refspec])
            .run(None)
            .map_err(git_failed)?;
        if out.status.success() {
            return Ok(());
        }
        // `--porcelain` marks each refused ref with `!`: the receiving
        // side answered and said no (its hooks, or not a fast-forward).
        let refused = String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.starts_with('!'));
        if refused {
            return Err(Box::new(
                Diagnostic::error(
                    Code::RunPushRefused,
                    format!("the receiving side refused the push to refs/heads/{branch}"),
                )
                .with_hint("a rebased branch goes to its next generation; force is never used"),
            ));
        }
        Err(Box::new(
            Diagnostic::error(
                Code::IoGitFailed,
                format!(
                    "the push to the bound git destination failed (git exited with {})",
                    out.status
                        .code()
                        .map_or("a signal".to_owned(), |c| c.to_string())
                ),
            )
            .with_hint(PUSH_FAILED_HINT),
        ))
    }
}

impl PushRunner for BoundPush {
    fn push(&self, checkout: &Path, branch: &str, commit: &str) -> Result<(), Box<Diagnostic>> {
        self.run(checkout, branch, commit)
    }
}
