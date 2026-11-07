//! The guards before every graph push (edd-01a18867, Sync, "Guards before
//! every push" and "Destination guard"):
//!
//! - [`destination`] and [`destinations`]: a remote's effective fetch and
//!   push destinations, normalized, so two spellings of one repository
//!   compare equal. Approval pins them, and every network write compares
//!   them again (G23).
//! - [`check_remote_ref`]: `remoteRef` is never a code ref.
//! - [`check_destination`]: the graph remote is the code remote (the
//!   personal setup) or shares no destination with any other remote.
//! - [`graph_only`]: every outgoing commit's tree holds graph-owned paths
//!   alone (G7), unless the remote is the code remote.
//! - [`unrelated_to_code`]: an existing remote ref that shares history with
//!   `mainRef` is refused, except the personal full-shape graph (G20).
//! - [`scan`]: the secret scan of every outgoing commit, which fails
//!   closed, and [`size`]: the 64 MiB cap.
//!
//! Every git call here is local plumbing on the common folder, through
//! `mdkg-git`'s hardened runner.

use std::path::Path;

use crate::graphref::{Destination, Error};

/// The largest push, in bytes of the objects it sends (as git stores
/// them): 64 MiB.
pub const PUSH_MAX: u64 = 64 << 20;

/// The secret scanner a push runs over its outgoing commits. The CLI
/// passes `secret.scan`'s, so graph pushes and git's hooks use one
/// detector. A scanner that can't run returns `Err`, which refuses the
/// push.
pub trait Scanner {
    /// The first commit and path in `commits` (in the common folder
    /// `common`) that adds a credential-shaped string, and the paths of
    /// blobs it couldn't read for their size.
    fn scan(&self, common: &Path, commits: &[String]) -> Result<ScanResult, String>;
}

/// What a [`Scanner`] found. Never holds what matched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanResult {
    /// `(commit, path)` of the first finding.
    pub finding: Option<(String, String)>,
    /// Blobs too large to read: a push with any is refused, since what it
    /// holds is unknown.
    pub unscanned: Vec<String>,
}

pub(crate) fn git(common: &Path, args: &[&str]) -> Result<std::process::Output, Error> {
    mdkg_git::common_command(common)
        .args(args)
        .run(None)
        .map_err(|e| Error::new("graph.git", format!("git {}: {e}", args.join(" "))))
}

pub(crate) fn git_ok(common: &Path, args: &[&str]) -> Result<Vec<u8>, Error> {
    let out = git(common, args)?;
    if !out.status.success() {
        return Err(Error::new(
            "graph.git",
            format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(out.stdout)
}

fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A URL as a destination: host, port and path, with the scheme, user,
/// a trailing `.git` and slashes dropped, so `git@host:org/app.git` and
/// `https://host/org/app` are one destination. A port is kept unless it's
/// the scheme's default (`https` 443, `http` 80, `ssh` 22, `git` 9418),
/// so `https://host:8443/x` is another endpoint than `https://host/x`
/// (Terra #17687). A local path keeps its path, with an empty host. The
/// path is compared without case.
pub fn destination(url: &str) -> String {
    let url = url.trim();
    let (rest, scheme) = match url.split_once("://") {
        Some((scheme, rest)) => (rest, Some(scheme.to_ascii_lowercase())),
        None => (url, None),
    };
    let default_port = match scheme.as_deref() {
        Some("https") => Some("443"),
        Some("http") => Some("80"),
        Some("ssh" | "git+ssh" | "ssh+git") => Some("22"),
        Some("git") => Some("9418"),
        _ => None,
    };
    let scheme = scheme.is_some();
    let local = !scheme && (rest.starts_with('/') || rest.starts_with('.'));
    let (authority, path) = if local {
        ("", rest)
    } else if scheme {
        rest.split_once('/').unwrap_or((rest, ""))
    } else {
        // scp-like `[user@]host:path`.
        rest.split_once(':').unwrap_or(("", rest))
    };
    // A user is dropped from the authority only, so an `@` in the path
    // (`org/app@v2.git`) stays part of the path.
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.rsplit_once(':') {
        // An IPv6 literal's colons are inside its brackets.
        Some((h, port)) if !port.contains(']') => {
            if port.is_empty() || Some(port) == default_port {
                h.to_ascii_lowercase()
            } else {
                format!("{}:{port}", h.to_ascii_lowercase())
            }
        }
        _ => host.to_ascii_lowercase(),
    };
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    // Without case: hosts like GitHub fold it, and folding can only add
    // refusals to the sharing check, never remove one (design review).
    format!("{host}/{}", path.to_lowercase())
}

/// A URL's exact endpoint, for the approval pin and the one-repository
/// check (Terra #17986): the URL as `git remote get-url` gives it, local
/// paths (plain or `file://`) resolved against `base` and canonical, and
/// otherwise only what can't name another repository folded: the scheme
/// and host's case, the scheme's default port, a password (never kept).
/// An ssh user stays (`alice@host:g` and `bob@host:g` are two home
/// folders), and so does every byte of the path (`repo` isn't
/// `repo.git`). Any other scheme's user can be an access token
/// (`https://<token>@host/x`), so it's kept only as a digest, `~<hex>@`:
/// a changed user still changes the endpoint, and the pin, `sync.json`
/// and every message never hold it (Terra #18102).
pub fn endpoint(url: &str, base: &Path) -> String {
    endpoint_exact(url, base).unwrap_or_else(|| "\u{FFFD}not-utf8".to_owned())
}

/// [`endpoint`], or `None` when `url` is a local path whose resolved form
/// isn't UTF-8 and so can't be pinned exactly. The one resolver both the
/// check and the pin use (Terra #18527).
pub fn endpoint_exact(url: &str, base: &Path) -> Option<String> {
    // Exact: [`url_problem`] refuses what trimming would change.
    let url = local_resolved(url, base)?;
    if url.starts_with('/') {
        return Some(url);
    }
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme.to_ascii_lowercase()), rest),
        None => (None, url.as_str()),
    };
    let (authority, path, sep) = match &scheme {
        Some(_) => rest
            .split_once('/')
            .map_or((rest, "", "/"), |(a, p)| (a, p, "/")),
        None => rest
            .split_once(':')
            .map_or(("", rest, ":"), |(a, p)| (a, p, ":")),
    };
    let (user, host) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user.split_once(':').map_or(user, |(u, _)| u)), host),
        None => (None, authority),
    };
    let default_port = match scheme.as_deref() {
        Some("https") => Some("443"),
        Some("http") => Some("80"),
        Some("ssh" | "git+ssh" | "ssh+git") => Some("22"),
        Some("git") => Some("9418"),
        _ => None,
    };
    let host = match host.rsplit_once(':') {
        Some((h, port))
            if !port.contains(']') && (port.is_empty() || Some(port) == default_port) =>
        {
            h.to_ascii_lowercase()
        }
        _ => host.to_ascii_lowercase(),
    };
    let ssh = matches!(
        scheme.as_deref(),
        None | Some("ssh" | "git+ssh" | "ssh+git")
    );
    let user = match user {
        None => String::new(),
        Some(u) if ssh => format!("{u}@"),
        Some(u) => {
            let d = mdkg_format::hash::Digest::of_bytes(u.as_bytes());
            format!("~{}@", &d.hex()[..16])
        }
    };
    Some(match scheme {
        Some(scheme) => format!("{scheme}://{user}{host}{sep}{path}"),
        None => format!("{user}{host}{sep}{path}"),
    })
}

/// `url` as git reads it from `base`: a local path (no `://`, and no
/// scp-like `host:` before the first `/`) made absolute against `base`,
/// and canonical when it exists; anything else unchanged.
fn local_resolved(url: &str, base: &Path) -> Option<String> {
    // `file://` is a local path too (Terra #17986): `file:///srv/x` and a
    // link to it are one repository. Its path is percent-decoded, as git
    // opens it (Terra #18102); one with an authority is refused by
    // [`file_url_problem`] and kept as written here.
    let decoded;
    let url = match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("file") => {
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            if !rest.starts_with('/') {
                return Some(url.to_owned());
            }
            match percent_decoded(rest) {
                Some(path) => {
                    decoded = path;
                    decoded.as_str()
                }
                None => return Some(url.to_owned()),
            }
        }
        Some(_) => return Some(url.to_owned()),
        None => url,
    };
    let scp = url
        .find(':')
        .is_some_and(|colon| !url[..colon].contains('/'));
    if scp {
        return Some(url.to_owned());
    }
    let path = base.join(url);
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    // Exact or nothing: a path that isn't UTF-8 is `None`, never folded
    // (Terra #18503).
    path.into_os_string().into_string().ok()
}

/// The raw configuration a remote's URLs are made from, checked byte for
/// byte before any is read through `git remote get-url` (Terra #18470,
/// #18503): `remote.<name>.url` and `pushurl`, and every
/// `url.<base>.insteadOf` / `pushInsteadOf` base and value, read
/// NUL-separated (`git config -z`), must be UTF-8 with no control
/// character. So no newline or invalid byte can make two repositories
/// print as one URL, and the one-URL-per-line output that follows is
/// unambiguous; the full URL rules ([`url_problem`]) then apply to each
/// URL git prints, after rewriting (Terra #18527).
fn raw_url_problem(common: &Path, base: &Path, remote: &str) -> Result<Option<String>, Error> {
    let records = |args: &[&str]| -> Result<Vec<Vec<u8>>, Error> {
        let out = git_at(common, base, args)?;
        // Exit 1: no such key.
        if !out.status.success() && out.status.code() != Some(1) {
            return Err(Error::new(
                "graph.git",
                format!("git {} failed", args.join(" ")),
            ));
        }
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(<[u8]>::to_vec)
            .collect())
    };
    // Byte safety only: a rewrite base or value is a prefix, not a URL,
    // so full URL rules apply to the rewritten URL git prints, below
    // (Terra #18527). Edge whitespace in a part shows up there too.
    let check = |bytes: &[u8]| -> Option<String> {
        match std::str::from_utf8(bytes) {
            Ok(text) if text.chars().any(char::is_control) => Some(format!(
                "a remote URL setting ({}) has a control character; spell the URL exactly",
                redacted(text).escape_debug()
            )),
            Ok(_) => None,
            Err(_) => Some("a remote URL setting isn't UTF-8; spell the URL exactly".to_owned()),
        }
    };
    for key in ["url", "pushurl"] {
        let name = format!("remote.{remote}.{key}");
        for value in records(&["config", "-z", "--get-all", &name])? {
            if let Some(why) = check(&value) {
                return Ok(Some(why));
            }
        }
    }
    let rewrites = records(&[
        "config",
        "-z",
        "--get-regexp",
        r"^url\..*\.(insteadof|pushinsteadof)$",
    ])?;
    for record in rewrites {
        // `url.<base>.insteadof\n<value>`.
        let (key, value) = match record.iter().position(|b| *b == b'\n') {
            Some(i) => (&record[..i], &record[i + 1..]),
            None => (&record[..], &[][..]),
        };
        let base_part = key
            .strip_prefix(b"url.")
            .and_then(|k| k.iter().rposition(|b| *b == b'.').map(|i| &k[..i]))
            .unwrap_or(key);
        if let Some(why) = check(base_part).or_else(|| check(value)) {
            return Ok(Some(why));
        }
    }
    Ok(None)
}

/// `text` with each `%XX` decoded, or `None` when an escape is malformed
/// or the result isn't UTF-8.
fn percent_decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // Exactly two hex digits (`from_str_radix` alone takes `+0`),
            // and never NUL, which no path holds: either would let two
            // distinct spellings git keeps literally decode to one pin
            // (Terra #18602).
            let hex = text.get(i + 1..i + 3)?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let byte = u8::from_str_radix(hex, 16).ok()?;
            if byte == 0 {
                return None;
            }
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Why `url` can't be a graph destination: leading or trailing
/// whitespace, or any control character, which git would keep as part of
/// a different path or host than the one shown (Terra #18470), or a
/// [`file_url_problem`].
pub fn url_problem(url: &str) -> Option<String> {
    if url.trim() != url || url.chars().any(char::is_control) {
        return Some(format!(
            "{} has whitespace at an end or a control character; spell the URL exactly",
            redacted(url.trim()).escape_debug()
        ));
    }
    file_url_problem(url)
}

/// Why a `file://` URL can't be a graph destination, if it can't: one
/// with an authority other than `localhost` (git doesn't open it as the
/// path it seems to name), or a malformed escape (Terra #18102).
pub fn file_url_problem(url: &str) -> Option<String> {
    let (scheme, rest) = url.trim().split_once("://")?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return Some(format!(
            "{} names a host in a file:// URL; use a plain local path",
            redacted(url)
        ));
    }
    percent_decoded(rest)
        .is_none()
        .then(|| format!("{} has a malformed %-escape", redacted(url)))
}

/// `url` with any userinfo replaced by `***@`, for messages: mdkg never
/// prints a credential a URL holds.
pub fn redacted(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://***@{host}/{path}"),
        None => url.to_owned(),
    }
}

/// Git run as network git runs ([`super::net::config_command`]): `base`'s
/// own git folder (a linked worktree's `config.worktree`, Terra #18297)
/// and the network command's environment (`GIT_CONFIG_GLOBAL` and the
/// rest cleared, Terra #18368), so the URLs read are the URLs contacted.
fn git_at(common: &Path, base: &Path, args: &[&str]) -> Result<std::process::Output, Error> {
    super::net::config_command(common, base)?
        .args(args)
        .run(None)
        .map_err(|e| Error::new("graph.git", format!("git {}: {e}", args.join(" "))))
}

/// The git setup a push to `remote` runs with (DECISION 10,
/// 2026-11-04): the system, global, local and worktree values (every
/// scope but `command`, DECISION 16 (A')) of `core.sshCommand`, `core.hooksPath`,
/// `remote.<remote>.receivepack` and `uploadpack`, every `credential.*`
/// and `hook.*` key, and `include.*`/`includeIf.*`; and the pre-push hook
/// the network command would run. Approval pins it, so any change is
/// `graph.remote_changed`.
///
/// Nothing in it can hold a credential in the clear (Terra #18946): each
/// entry is keyed `<scope> <section>.<name>`, with any subsection (a URL
/// in `credential.<url>.helper`) replaced by `<sha256 prefix>`, and its
/// value is the SHA-256 of every value's exact bytes, length-prefixed in
/// git's order, so `["a", "b"]` and `["a\nb"]` differ.
///
/// What's checked is one more entry, [`SEQUENCE`] (DECISION 16 (A),
/// Terra #19005): the SHA-256 of git's whole ordered record stream, each
/// record its scope, exact key bytes, whether it has a value and the
/// length-prefixed value. Git reads the order between keys (an empty
/// `credential.helper` resets the helpers listed before it), and a key
/// with no value isn't one with an empty value, so a reorder or either
/// change moves the pin; the per-key entries only name what moved.
pub fn setup(
    common: &Path,
    base: &Path,
    remote: &str,
) -> Result<std::collections::BTreeMap<String, String>, Error> {
    use sha2::{Digest, Sha256};
    let escaped: String = remote
        .chars()
        .flat_map(|c| {
            let special = !c.is_ascii_alphanumeric() && c != '_' && c != '-';
            special.then_some('\\').into_iter().chain([c])
        })
        .collect();
    let pattern = format!(
        "^(core\\.(sshcommand|hookspath)|remote\\.{escaped}\\.(receivepack|uploadpack)|\
         credential\\..*|hook\\..*|include\\..*|includeif\\..*)$"
    );
    let out = git_at(
        common,
        base,
        &["config", "-z", "--show-scope", "--get-regexp", &pattern],
    )?;
    // 1 is "none set"; anything else is a failure to read.
    if !out.status.success() && out.status.code() != Some(1) {
        return Err(Error::new(
            "graph.git",
            format!(
                "can't read the git setup of remote {remote}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    let hex = |bytes: &[u8]| -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let mut values = std::collections::BTreeMap::<String, Sha256>::new();
    let mut sequence = Sha256::new();
    let framed = |d: &mut Sha256, bytes: &[u8]| {
        d.update((bytes.len() as u64).to_be_bytes());
        d.update(bytes);
    };
    let mut fields = out.stdout.split(|&b| b == 0);
    while let (Some(scope), Some(entry)) = (fields.next(), fields.next()) {
        // Every scope git reads but `command` (DECISION 16 (A'), Terra
        // #19100): an included file takes its includer's scope, so a
        // global `includeIf` can point into a checkout an agent writes.
        if scope == b"command" {
            continue;
        }
        // `key\nvalue`, or the key alone for one with no value (`[x] y`),
        // which git reads differently from `y =` (Terra #19005).
        let (key, value) = match entry.iter().position(|&b| b == b'\n') {
            Some(at) => (&entry[..at], Some(&entry[at + 1..])),
            None => (entry, None),
        };
        framed(&mut sequence, scope);
        framed(&mut sequence, key);
        sequence.update([u8::from(value.is_some())]);
        framed(&mut sequence, value.unwrap_or_default());
        // `section.subsection.name`: the section and name are git's
        // lowercase ASCII; the subsection is anything, so only its digest
        // is kept.
        let first = key.iter().position(|&b| b == b'.').unwrap_or(key.len());
        let last = key.iter().rposition(|&b| b == b'.').unwrap_or(key.len());
        let section = String::from_utf8_lossy(&key[..first]);
        let label = if last > first {
            let sub = &key[first + 1..last];
            let name = String::from_utf8_lossy(&key[last + 1..]);
            format!("{section}.<{}>.{name}", &hex(sub)[..12])
        } else {
            String::from_utf8_lossy(key).into_owned()
        };
        let scope = String::from_utf8_lossy(scope);
        let digest = values.entry(format!("{scope} {label}")).or_default();
        digest.update([u8::from(value.is_some())]);
        framed(digest, value.unwrap_or_default());
    }
    let mut setup: std::collections::BTreeMap<String, String> = values
        .into_iter()
        .map(|(k, d)| {
            let v: String = d.finalize().iter().map(|b| format!("{b:02x}")).collect();
            (k, format!("sha256:{v}"))
        })
        .collect();
    let sequence: String = sequence
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    setup.insert(SEQUENCE.into(), format!("sha256:{sequence}"));
    setup.insert("pre-push".into(), pre_push(common, base)?);
    Ok(setup)
}

/// The [`setup`] entry holding the digest of git's ordered record stream,
/// named in `graph.remote_changed` when only the order or a value's
/// presence moved.
pub const SEQUENCE: &str = "the order of these settings";

/// The pre-push hook [`setup`] pins: its blob id and whether git would
/// run it, or `absent`. Git runs a hook when `access(path, X_OK)`
/// succeeds, so that same call is pinned (DECISION 16 (B), Terra #19005):
/// the permission bits, owner, an ACL, file flags or a noexec mount all
/// reach it there, and none needs pinning apart. A hooks path git expands (`~/`,
/// `~user/`, `%(prefix)/`) is expanded by git itself; one it can't expand
/// is refused, never pinned as text.
fn pre_push(common: &Path, base: &Path) -> Result<String, Error> {
    let mut hooks = super::net::hooks_path(common, base)?;
    if hooks.starts_with('~') || hooks.starts_with("%(") {
        let out = git_at(
            common,
            base,
            &[
                "config",
                "--type=path",
                "--show-scope",
                "--get-all",
                "core.hooksPath",
            ],
        )?;
        let expanded = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter(|(scope, _)| *scope != "command")
            .map(|(_, v)| v.trim().to_owned())
            .next_back()
            .filter(|p| out.status.success() && Path::new(p).is_absolute());
        hooks = expanded.ok_or_else(|| {
            Error::new(
                "graph.remote_invalid",
                format!(
                    "core.hooksPath {hooks} can't be resolved to a folder, so the pre-push hook \
                     a graph push would run can't be pinned"
                ),
            )
        })?;
    }
    let file = Path::new(&hooks).join("pre-push");
    let meta = match std::fs::metadata(&file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("absent".into()),
        Err(e) => {
            return Err(Error::new(
                "graph.git",
                format!("the pre-push hook {}: {e}", file.display()),
            ));
        }
    };
    let file_text = file.to_string_lossy().into_owned();
    let out = git_at(
        common,
        base,
        &["hash-object", "--no-filters", "--", &file_text],
    )?;
    if !out.status.success() {
        return Err(Error::new(
            "graph.git",
            format!("can't hash the pre-push hook {}", file.display()),
        ));
    }
    let runs = meta.is_file() && rustix::fs::access(&file, rustix::fs::Access::EXEC_OK).is_ok();
    Ok(format!(
        "blob {} {}",
        String::from_utf8_lossy(&out.stdout).trim(),
        if runs { "runs" } else { "not run" }
    ))
}

/// The remotes configured, as the checkout `base` reads them.
pub fn remotes(common: &Path, base: &Path) -> Result<Vec<String>, Error> {
    let out = git_at(common, base, &["remote"])?;
    if !out.status.success() {
        return Err(Error::new("graph.git", "git remote failed"));
    }
    Ok(lines(&out.stdout))
}

/// `remote`'s effective destinations: its fetch URLs and its push URLs,
/// with `insteadOf` and `pushInsteadOf` applied (`git remote get-url`),
/// each as its exact [`endpoint`] (relative and `file://` local paths
/// resolved against `base`, the checkout git runs in), sorted and without
/// repeats. A remote that doesn't exist has none.
pub fn destinations(common: &Path, base: &Path, remote: &str) -> Result<Destination, Error> {
    if let Some(why) = raw_url_problem(common, base, remote)? {
        return Err(Error::new("graph.remote_invalid", why));
    }
    let get = |args: &[&str]| -> Result<Vec<String>, Error> {
        let out = git_at(common, base, args)?;
        if !out.status.success() {
            return Ok(Vec::new());
        }
        // Each URL exactly as git prints it, one per line: never trimmed
        // (Terra #18470), never decoded lossily, and with the raw settings
        // already checked, any empty or non-UTF-8 record is refused as
        // ambiguous rather than skipped (Terra #18503).
        let ambiguous = || {
            Error::new(
                "graph.remote_invalid",
                format!("git's URLs for remote {remote} can't be read exactly"),
            )
        };
        let text = String::from_utf8(out.stdout).map_err(|_| ambiguous())?;
        let body = text.strip_suffix('\n').ok_or_else(ambiguous)?;
        let urls: Vec<String> = body.split('\n').map(str::to_owned).collect();
        if urls.iter().any(String::is_empty) {
            return Err(ambiguous());
        }
        if let Some(why) = urls.iter().find_map(|u| url_problem(u)) {
            return Err(Error::new("graph.remote_invalid", why));
        }

        let mut all = Vec::new();
        for u in &urls {
            all.push(endpoint_exact(u, base).ok_or_else(|| {
                Error::new(
                    "graph.remote_invalid",
                    format!("remote {remote} names a local path that isn't UTF-8"),
                )
            })?);
        }
        all.sort();
        all.dedup();
        Ok(all)
    };
    Ok(Destination {
        fetch: get(&["remote", "get-url", "--all", "--", remote])?,
        push: get(&["remote", "get-url", "--push", "--all", "--", remote])?,
        ..Destination::default()
    })
}

/// The code remote: `mainRef`'s remote (`refs/remotes/<remote>/...`), or
/// the upstream remote of a local `mainRef` branch, else `origin`.
pub fn code_remote(
    common: &Path,
    base: &Path,
    main_ref: Option<&str>,
    remotes: &[String],
) -> String {
    if let Some(rest) = main_ref.and_then(|m| m.strip_prefix("refs/remotes/")) {
        // The longest configured remote name the ref starts with, so a
        // remote named `a/b` is found.
        if let Some(r) = remotes
            .iter()
            .filter(|r| rest.starts_with(&format!("{r}/")))
            .max_by_key(|r| r.len())
        {
            return r.clone();
        }
    }
    if let Some(branch) = main_ref.and_then(|m| m.strip_prefix("refs/heads/"))
        && let Ok(out) = git_at(
            common,
            base,
            &["config", "--get", &format!("branch.{branch}.remote")],
        )
        && out.status.success()
    {
        let r = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !r.is_empty() && r != "." {
            return r;
        }
    }
    "origin".to_owned()
}

/// Whether the graph remote is the code remote (the personal setup), or
/// `Err(graph.remote_is_code)` when it shares a destination with another
/// remote, so a company remote is never pushed to by another name (Sync,
/// "Destination guard").
///
/// The graph remote has one destination: its fetch and push URLs name
/// one repository (`Err(graph.remote_invalid)` otherwise, Terra #17687),
/// since sync reads the tip it pushes against from where it fetches.
/// Another remote counts as the code's only when that one destination is
/// one the code remote pushes to; so full code history goes only where
/// the code already goes, and a remote that merely fetches from the code
/// repository is refused as sharing it.
pub fn check_destination(
    common: &Path,
    base: &Path,
    remote: &str,
    mine: &Destination,
    main_ref: Option<&str>,
) -> Result<bool, Error> {
    if mine.fetch.is_empty() && mine.push.is_empty() {
        return Err(Error::new(
            "graph.remote_invalid",
            format!("there's no remote named {remote}; mdkg never creates one"),
        ));
    }
    if mine.fetch.len() != 1 || mine.push != mine.fetch {
        let list = |l: &[String]| {
            if l.is_empty() {
                "none".to_owned()
            } else {
                l.join(", ")
            }
        };
        return Err(Error::new(
            "graph.remote_invalid",
            format!(
                "graph remote {remote} fetches from {} and pushes to {}; graph sync needs one \
                 repository for both: name a remote with a single URL and no other pushurl",
                list(&mine.fetch),
                list(&mine.push)
            ),
        ));
    }
    let all = remotes(common, base)?;
    let code = code_remote(common, base, main_ref, &all);
    let code_dest = if code == remote {
        mine.clone()
    } else {
        destinations(common, base, &code)?
    };
    // Sharing is judged by the loose [`destination`] key, so any spelling
    // of one repository counts (over-matching only refuses more); being
    // the code's needs the exact endpoint.
    let same = |a: &Destination, b: &Destination| {
        let theirs: Vec<String> = b
            .fetch
            .iter()
            .chain(&b.push)
            .map(|u| destination(u))
            .collect();
        a.fetch
            .iter()
            .chain(&a.push)
            .any(|u| theirs.contains(&destination(u)))
    };
    let is_code = code == remote || mine.push.iter().all(|u| code_dest.push.contains(u));
    if is_code {
        return Ok(true);
    }
    for other in all.iter().filter(|r| r.as_str() != remote) {
        if same(mine, &destinations(common, base, other)?) {
            return Err(Error::new(
                "graph.remote_is_code",
                format!(
                    "graph remote {remote} shares a destination with remote {other}; a graph \
                     remote other than the code's must be a repository of its own"
                ),
            ));
        }
    }
    Ok(false)
}

/// `remoteRef` is never a code ref: under `refs/heads/`, and its short
/// name isn't `main`, `master`, `mainRef`'s branch, a protected branch
/// (`protected`, short names), or the remote's default branch
/// (`refs/remotes/<remote>/HEAD`'s target). Compared without case.
pub fn check_remote_ref(
    common: &Path,
    remote: &str,
    remote_ref: &str,
    main_ref: Option<&str>,
    protected: &[String],
) -> Result<(), Error> {
    let Some(short) = remote_ref.strip_prefix("refs/heads/") else {
        return Err(Error::new(
            "graph.remote_invalid",
            format!("mdkg.graph.remoteRef {remote_ref} must be under refs/heads/"),
        ));
    };
    let short = short.to_lowercase();
    let mut code: Vec<String> = vec!["main".into(), "master".into()];
    if let Some(m) = main_ref {
        // `refs/heads/<b>`, or `refs/remotes/<r>/<b>`: the branch.
        let branch = match m.strip_prefix("refs/heads/") {
            Some(b) => b,
            None => m
                .strip_prefix("refs/remotes/")
                .and_then(|r| r.split_once('/'))
                .map_or(m, |(_, b)| b),
        };
        code.push(branch.to_owned());
    }
    code.extend(protected.iter().cloned());
    let head = git(
        common,
        &["symbolic-ref", "-q", &format!("refs/remotes/{remote}/HEAD")],
    )?;
    if head.status.success() {
        let target = String::from_utf8_lossy(&head.stdout).trim().to_owned();
        if let Some(b) = target.strip_prefix(&format!("refs/remotes/{remote}/")) {
            code.push(b.to_owned());
        }
    }
    if code.iter().any(|c| c.to_lowercase() == short) {
        return Err(Error::new(
            "graph.remote_is_code",
            format!(
                "mdkg.graph.remoteRef {remote_ref} is a code branch on {remote}; give the graph \
                 its own ref, such as refs/heads/mdkg-graph"
            ),
        ));
    }
    Ok(())
}

/// Every path in `commit`'s tree, NUL-separated as git lists them.
fn tree_paths(common: &Path, commit: &str) -> Result<Vec<String>, Error> {
    let out = git_ok(
        common,
        &["ls-tree", "-r", "-z", "--name-only", "--full-tree", commit],
    )?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

fn owned(path: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|p| path.starts_with(&format!("{}/", p.trim_end_matches('/'))))
}

/// Whether `commit`'s tree holds graph-owned paths alone.
pub fn graph_only_tree(common: &Path, commit: &str, prefixes: &[String]) -> Result<bool, Error> {
    Ok(tree_paths(common, commit)?
        .iter()
        .all(|p| owned(p, prefixes)))
}

/// The graph-only proof (G7): every commit in `outgoing` holds
/// graph-owned paths alone, and no graph folder holds a path `mainRef`'s
/// tree holds. `Err(graph.shape)` names the first commit and path that
/// fails, or `graph.overlapping_workspace` the folder.
pub fn graph_only(
    common: &Path,
    outgoing: &[String],
    prefixes: &[String],
    main_tip: Option<&str>,
) -> Result<(), Error> {
    if let Some(main) = main_tip {
        let code = tree_paths(common, main)?;
        if let Some(p) = prefixes
            .iter()
            .find(|p| code.iter().any(|c| owned(c, &[(*p).clone()])))
        {
            return Err(Error::new(
                "graph.overlapping_workspace",
                format!("graph folder {p} holds a path the code base also holds"),
            ));
        }
    }
    for commit in outgoing {
        if let Some(p) = tree_paths(common, commit)?
            .iter()
            .find(|p| !owned(p, prefixes))
        {
            return Err(Error::new(
                "graph.shape",
                format!(
                    "commit {} holds {}, outside the graph folders; a remote other than the \
                     code's receives only graph-only history, a ref whose commits hold \
                     nothing but the graph folders",
                    short(commit),
                    super::printable(p)
                ),
            ));
        }
    }
    Ok(())
}

/// Refuses an existing remote tip `remote` whose history shares a merge
/// base with `main_tip` (`graph.remote_is_code`).
pub fn unrelated_to_code(common: &Path, remote: &str, main_tip: &str) -> Result<(), Error> {
    let out = git(common, &["merge-base", remote, main_tip])?;
    if out.status.success() {
        return Err(Error::new(
            "graph.remote_is_code",
            format!(
                "the remote graph ref {} shares history with the code base; it isn't a graph ref",
                short(remote)
            ),
        ));
    }
    Ok(())
}

/// Those of `commits` this clone has as commits: the rest can't bound a
/// `rev-list` here. One `cat-file --batch-check` for all of them.
pub fn present(common: &Path, commits: &[String]) -> Result<Vec<String>, Error> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    let input: String = commits.iter().map(|c| format!("{c}\n")).collect();
    let out = mdkg_git::common_command(common)
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .run(Some(input.as_bytes()))
        .map_err(|e| Error::new("graph.git", format!("git cat-file: {e}")))?;
    if !out.status.success() {
        return Err(Error::new("graph.git", "git cat-file --batch-check failed"));
    }
    Ok(lines(&out.stdout)
        .into_iter()
        .filter_map(|l| {
            l.strip_suffix(" commit")
                .filter(|sha| commits.iter().any(|c| c == sha))
                .map(str::to_owned)
        })
        .collect())
}

/// The commits `tip` adds over `known` (`rev-list tip --not known`), oldest
/// first; all of `tip`'s history when `known` is empty.
pub fn outgoing(common: &Path, tip: &str, known: &[String]) -> Result<Vec<String>, Error> {
    let mut args = vec!["rev-list", "--reverse", tip];
    if !known.is_empty() {
        args.push("--not");
        args.extend(known.iter().map(String::as_str));
    }
    Ok(lines(&git_ok(common, &args)?))
}

/// The bytes the objects `tip` adds over `known` take, as git stores them;
/// over [`PUSH_MAX`] is refused (`graph.push_large`).
pub fn size(common: &Path, tip: &str, known: &[String]) -> Result<u64, Error> {
    let mut args = vec!["rev-list", "--objects", "--disk-usage", tip];
    if !known.is_empty() {
        args.push("--not");
        args.extend(known.iter().map(String::as_str));
    }
    let out = git_ok(common, &args)?;
    let bytes = String::from_utf8_lossy(&out)
        .trim()
        .parse::<u64>()
        .map_err(|_| Error::new("graph.git", "git rev-list --disk-usage printed no size"))?;
    if bytes > PUSH_MAX {
        return Err(Error::new(
            "graph.push_large",
            format!("the push is {bytes} bytes, over the {PUSH_MAX}-byte cap"),
        ));
    }
    Ok(bytes)
}

/// The secret scan of `outgoing`: a finding refuses (`graph.secret`,
/// naming the commit and path, never the match), and so does a scanner
/// that's missing or can't run, or a blob it couldn't read
/// (`graph.secret_scan_unavailable`).
/// What could substitute objects for the scan but not for the push:
/// a replace ref (git reads through `refs/replace/`, or
/// `GIT_REPLACE_REF_BASE`'s, for every command but a transfer) or a
/// grafts file. A scanner reads with the user's git, so a replacement of
/// a secret-bearing blob would hide it while pack transfer sends the
/// original (Terra #17687). `None` when there's neither.
fn substitutes(common: &Path) -> Result<Option<String>, Error> {
    let base = std::env::var("GIT_REPLACE_REF_BASE")
        .ok()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| "refs/replace/".to_owned());
    let found = git_ok(
        common,
        &["for-each-ref", "--count=1", "--format=%(refname)", &base],
    )?;
    if let Some(r) = lines(&found).first() {
        return Ok(Some(format!("the replace ref {}", super::printable(r))));
    }
    let grafts = std::env::var_os("GIT_GRAFT_FILE")
        .filter(|g| !g.is_empty())
        .map_or_else(|| common.join("info/grafts"), std::path::PathBuf::from);
    if let Ok(text) = std::fs::read_to_string(&grafts)
        && text
            .lines()
            .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
    {
        return Ok(Some(format!("the grafts file {}", grafts.display())));
    }
    Ok(None)
}

pub fn scan(
    common: &Path,
    scanner: Option<&dyn Scanner>,
    outgoing: &[String],
) -> Result<(), Error> {
    if outgoing.is_empty() {
        return Ok(());
    }
    let Some(scanner) = scanner else {
        return Err(Error::new(
            "graph.secret_scan_unavailable",
            "the secret scanner is unavailable, so graph pushes are refused",
        ));
    };
    if let Some(what) = substitutes(common)? {
        return Err(Error::new(
            "graph.secret_scan_unavailable",
            format!(
                "{what} can make the secret scan read other objects than the push sends, so \
                 the push is refused; remove it to sync"
            ),
        ));
    }
    let found = scanner.scan(common, outgoing).map_err(|why| {
        Error::new(
            "graph.secret_scan_unavailable",
            format!("the secret scan couldn't run ({why}), so the push is refused"),
        )
    })?;
    if let Some((commit, path)) = found.finding {
        return Err(Error::new(
            "graph.secret",
            format!(
                "commit {} adds a credential-shaped string to {}; the push is refused. Remove \
                 it from the graph's history, and rotate the credential if it was ever real",
                short(&commit),
                super::printable(&path)
            ),
        ));
    }
    if let Some(p) = found.unscanned.first() {
        return Err(Error::new(
            "graph.secret_scan_unavailable",
            format!(
                "{} is too large to scan, so the push is refused",
                super::printable(p)
            ),
        ));
    }
    Ok(())
}

pub(crate) fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::{destination, endpoint, file_url_problem, redacted};
    use std::path::Path;

    #[test]
    fn an_https_user_is_kept_only_as_a_digest() {
        let token = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");
        let url = format!("https://{token}:x-oauth-basic@example.invalid/g.git");
        let e = endpoint(&url, Path::new("/"));
        assert!(!e.contains(token) && !e.contains("x-oauth"), "{e}");
        assert!(
            e.starts_with("https://~") && e.ends_with("@example.invalid/g.git"),
            "{e}"
        );
        let other = endpoint("https://someone@example.invalid/g.git", Path::new("/"));
        assert_ne!(e, other, "a changed user changes the endpoint");
        assert_eq!(
            endpoint("ssh://alice@example.invalid/g", Path::new("/")),
            "ssh://alice@example.invalid/g"
        );
    }

    #[test]
    fn a_file_url_is_read_as_git_opens_it() {
        let base = Path::new("/nonexistent-base");
        assert_eq!(
            endpoint("file:///nonexistent/graph%20private.git", base),
            "/nonexistent/graph private.git"
        );
        assert_ne!(
            endpoint("file:///nonexistent/graph%20private.git", base),
            endpoint("/nonexistent/graph%20private.git", base)
        );
        assert!(file_url_problem("file://host/srv/g.git").is_some());
        // Terra #18602: a sign or a NUL is a malformed escape.
        assert!(file_url_problem("file://localhost/tmp/repo%+0.git").is_some());
        assert!(file_url_problem("file:///tmp/repo%00.git").is_some());
        assert!(file_url_problem("file:///tmp/repo%2B0.git").is_none());
        assert!(file_url_problem("file:///srv/g%zz.git").is_some());
        assert!(file_url_problem("file:///srv/g.git").is_none());
        assert!(file_url_problem("file://localhost/srv/g.git").is_none());
        assert!(file_url_problem("/srv/g.git").is_none());
    }

    #[test]
    fn a_url_with_space_at_an_end_or_a_control_character_is_refused() {
        assert!(super::url_problem("/srv/g.git ").is_some());
        assert!(super::url_problem(" /srv/g.git").is_some());
        assert!(super::url_problem("/srv/g\t.git").is_some());
        assert!(super::url_problem("/srv/my graphs/g.git").is_none());
    }

    #[test]
    fn sharing_is_judged_without_path_case() {
        assert_eq!(
            destination("https://example.invalid/Org/App.git"),
            destination("git@example.invalid:org/app")
        );
    }

    #[test]
    fn spellings_of_one_destination_agree() {
        for url in [
            "git@example.invalid:org/app.git",
            "ssh://git@example.invalid:22/org/app.git",
            "https://user@example.invalid/org/app/",
            "https://EXAMPLE.invalid/org/app",
        ] {
            assert_eq!(destination(url), "example.invalid/org/app", "{url}");
        }
        assert_eq!(destination("/srv/app.git"), "/srv/app");
        assert_eq!(destination("file:///srv/app.git"), "/srv/app");
    }

    #[test]
    fn a_port_other_than_the_default_is_another_endpoint() {
        assert_eq!(
            destination("https://example.invalid:443/g.git"),
            destination("https://example.invalid/g")
        );
        assert_eq!(
            destination("https://example.invalid:8443/g.git"),
            "example.invalid:8443/g"
        );
        assert_ne!(
            destination("https://example.invalid:8443/g.git"),
            destination("https://example.invalid:443/g.git")
        );
        assert_eq!(
            destination("ssh://git@example.invalid:2222/g.git"),
            "example.invalid:2222/g"
        );
        assert_eq!(
            destination("git://example.invalid:9418/g"),
            "example.invalid/g"
        );
        assert_eq!(destination("https://[::1]:8443/g"), "[::1]:8443/g");
        assert_eq!(destination("https://[::1]/g"), "[::1]/g");
    }

    #[test]
    fn an_endpoint_keeps_what_can_name_another_repository() {
        use super::endpoint;
        let base = std::path::Path::new("/nonexistent-base");
        assert_ne!(
            endpoint("alice@example.invalid:graph.git", base),
            endpoint("bob@example.invalid:graph.git", base)
        );
        assert_ne!(endpoint("/srv/repo", base), endpoint("/srv/repo.git", base));
        assert_eq!(
            endpoint("https://EXAMPLE.invalid:443/g.git", base),
            "https://example.invalid/g.git"
        );
        assert_eq!(
            endpoint(concat!("https://user:pw", "@example.invalid/g.git"), base),
            endpoint("https://user@example.invalid/g.git", base),
            "a password is never kept"
        );
        assert_eq!(
            endpoint("https://example.invalid:8443/g.git", base),
            "https://example.invalid:8443/g.git"
        );
        assert_eq!(endpoint("file:///srv/g.git", base), "/srv/g.git");
        assert_eq!(endpoint("../g.git", base), "/nonexistent-base/../g.git");
    }

    #[test]
    fn a_file_url_and_a_link_to_it_are_one_destination() {
        use super::endpoint;
        let dir = std::env::temp_dir().join(format!("mdkg-guard-{}", std::process::id()));
        let real = dir.join("company.git");
        std::fs::create_dir_all(&real).expect("mkdir");
        let link = dir.join("link.git");
        std::os::unix::fs::symlink(&real, &link).ok();
        let base = std::path::Path::new("/");
        let a = endpoint(&format!("file://{}", link.display()), base);
        let b = endpoint(&real.display().to_string(), base);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(destination(&a), destination(&b));
        assert_eq!(a, b);
    }

    #[test]
    fn an_at_sign_in_the_path_stays_in_the_path() {
        assert_eq!(
            destination("https://example.invalid/org/app@v2.git"),
            "example.invalid/org/app@v2"
        );
        assert_eq!(
            destination("git@example.invalid:org/app@v2.git"),
            "example.invalid/org/app@v2"
        );
    }

    #[test]
    fn userinfo_is_redacted() {
        assert_eq!(
            redacted(concat!("https://someone:pw", "@example.invalid/org/app")),
            "https://***@example.invalid/org/app"
        );
        assert_eq!(redacted("/srv/app.git"), "/srv/app.git");
    }
}
