//! `mdkg git-hook pre-receive` and `mdkg git-hook update`: the receiving
//! hooks `git-serve` installs (the serve edd's S38 and R86).
//!
//! For a push through `git-serve` they refuse it unless the fence is
//! held and the machine is still approved, and every ref is the branch,
//! or a `.g<k>` generation, of exactly one defined agent placed on the
//! pushing machine and not retired. They refuse a delete or a
//! non-fast-forward, and an added commit that changes a path under any
//! `.mdkg` folder, since workers never commit graph files (S42; Opus r5
//! m9). `pre-receive` runs the secret scan over the commits the push
//! adds. git moves refs only after both hooks pass, and the fence stays
//! held by git until then (`git_serve`'s module note).
//!
//! The repository's own hooks don't run for these pushes (the edd's
//! stated cost, kept by orchestrator on 2026-10-29: running hooks a
//! repository's config names is code execution as the owner).
//! `pre-receive` says which it skipped, in the push's output.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use mdkg_format::Code;

use super::git_serve::{self, Agents, Book, Deps, GIT_VAR, MACHINE_VAR, PlacementState, Refusal};
use crate::hook::printable;

/// The two hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    PreReceive,
    Update,
}

impl Hook {
    pub fn as_str(self) -> &'static str {
        match self {
            Hook::PreReceive => "pre-receive",
            Hook::Update => "update",
        }
    }
}

/// One ref update as git hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub refname: String,
    pub old: String,
    pub new: String,
}

/// An object id: 40 or 64 lowercase hex digits.
fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_zero(oid: &str) -> bool {
    oid.bytes().all(|b| b == b'0')
}

fn malformed() -> Refusal {
    Refusal::new(
        Code::UsageInvalidArguments,
        "git handed the hook a malformed ref update",
    )
}

fn update(refname: &str, old: &str, new: &str) -> Result<Update, Refusal> {
    if !is_oid(old) || !is_oid(new) || old.len() != new.len() || refname.is_empty() {
        return Err(malformed());
    }
    Ok(Update {
        refname: refname.to_owned(),
        old: old.to_owned(),
        new: new.to_owned(),
    })
}

/// `pre-receive`'s stdin: one `<old> <new> <ref>` line per update.
pub fn parse_pre_receive(input: &str) -> Result<Vec<Update>, Refusal> {
    input
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut parts = line.splitn(3, ' ');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(old), Some(new), Some(refname)) => update(refname, old, new),
                _ => Err(malformed()),
            }
        })
        .collect()
}

/// `update`'s arguments: `<ref> <old> <new>`.
pub fn parse_update(args: &[String]) -> Result<Vec<Update>, Refusal> {
    match args {
        [refname, old, new] => Ok(vec![update(refname, old, new)?]),
        _ => Err(malformed()),
    }
}

/// The agent whose branch, or `.g<k>` generation, `refname` is (S38).
/// Refuses anything but a branch, a protected ref, a ref no agent owns,
/// and a ref two agents could own, naming both.
pub fn owner(agents: &Agents, refname: &str) -> Result<String, Refusal> {
    let not_pushable = |why: String| Refusal::new(Code::CoordBranchNotPushable, why);
    if agents.protected.iter().any(|p| p == refname) {
        return Err(not_pushable(format!(
            "{refname} is never pushed from a machine"
        )));
    }
    let Some(branch) = refname.strip_prefix("refs/heads/") else {
        return Err(not_pushable(format!(
            "{refname} isn't a branch; machines push only their agents' branches"
        )));
    };
    let owners: Vec<&String> = agents
        .defined
        .keys()
        .filter(|name| {
            let own = agents.names.branch_for(name);
            branch == own
                || branch
                    .strip_prefix(own.as_str())
                    .and_then(|rest| rest.strip_prefix(".g"))
                    .is_some_and(is_generation)
        })
        .collect();
    match owners.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(not_pushable(format!(
            "{refname} isn't the branch of an agent defined at the graph ref's tip"
        ))),
        many => Err(not_pushable(format!(
            "{refname} could belong to {}, so it's refused",
            many.iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(" and ")
        ))),
    }
}

/// A positive integer with no leading zero, so each generation has one
/// spelling.
fn is_generation(k: &str) -> bool {
    !k.is_empty() && !k.starts_with('0') && k.bytes().all(|b| b.is_ascii_digit())
}

/// Refuses unless the agent is not retired and is placed on `machine`.
pub fn check_placed(
    agents: &Agents,
    book: &dyn Book,
    agent: &str,
    machine: &str,
    refname: &str,
) -> Result<(), Refusal> {
    let refuse = |why: &str| {
        Refusal::new(
            Code::CoordBranchNotPushable,
            format!("{refname} is {agent}'s branch, and {agent} {why}"),
        )
    };
    if agents.defined.get(agent) == Some(&true) {
        return Err(refuse("is retired"));
    }
    match book.placement(agent)? {
        None => Err(refuse("isn't placed on any machine")),
        Some(p) if p.machine != machine => Err(refuse("is placed on another machine")),
        Some(p) => match p.state {
            PlacementState::Placed => Ok(()),
            PlacementState::Retiring => Err(refuse("is being retired")),
            PlacementState::Released => Err(refuse("is retired")),
        },
    }
}

/// The git the hooks run, in the folder git runs them in, under the
/// environment receive-pack gave them (its quarantine included).
pub struct Repo {
    pub git: PathBuf,
    pub git_dir: PathBuf,
}

impl Repo {
    fn git(&self, args: &[&str]) -> Result<std::process::Output, Refusal> {
        Command::new(&self.git)
            .args(args)
            .current_dir(&self.git_dir)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Refusal::new(Code::IoGitFailed, format!("can't run git: {e}")))
    }

    /// Whether `old` is an ancestor of `new`.
    fn fast_forward(&self, old: &str, new: &str) -> Result<bool, Refusal> {
        let out = self.git(&["merge-base", "--is-ancestor", old, new])?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Refusal::new(
                Code::IoGitFailed,
                "git couldn't compare the pushed commits",
            )),
        }
    }

    /// The commits the updates add: reachable from a new value and from
    /// no ref the repository has yet.
    fn added(&self, updates: &[Update]) -> Result<Vec<String>, Refusal> {
        let mut args = vec!["rev-list"];
        args.extend(
            updates
                .iter()
                .filter(|u| !is_zero(&u.new))
                .map(|u| u.new.as_str()),
        );
        if args.len() == 1 {
            return Ok(Vec::new());
        }
        args.extend(["--not", "--all"]);
        let out = self.git(&args)?;
        if !out.status.success() {
            return Err(Refusal::new(
                Code::IoGitFailed,
                "git couldn't list the pushed commits",
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect())
    }
}

/// The receive-side hooks git would run from a repository's own hooks
/// folder.
const OWN_HOOKS: [&str; 7] = [
    "pre-receive",
    "update",
    "post-receive",
    "post-update",
    "reference-transaction",
    "push-to-checkout",
    "proc-receive",
];

impl Repo {
    /// The repository's own receive-side hooks, which these pushes skip:
    /// those executable in its `core.hooksPath` (its own config only, not
    /// git-serve's `-c`), else in `<git dir>/hooks`.
    pub fn own_hooks(&self) -> Vec<&'static str> {
        use std::os::unix::fs::PermissionsExt;
        let configured = self
            .git(&["config", "--local", "--get", "core.hooksPath"])
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty());
        let dir = match configured {
            Some(p) if Path::new(&p).is_absolute() => PathBuf::from(p),
            Some(p) => {
                // Relative to where git runs hooks: the checkout's top, or
                // the repository itself when bare.
                let top = match self.git_dir.file_name() {
                    Some(n) if n == ".git" => self.git_dir.parent().unwrap_or(&self.git_dir),
                    _ => &self.git_dir,
                };
                top.join(p)
            }
            None => self.git_dir.join("hooks"),
        };
        OWN_HOOKS
            .into_iter()
            .filter(|h| {
                std::fs::metadata(dir.join(h))
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .collect()
    }

    /// The first graph path `commit` changes (see [`is_graph_path`]),
    /// against every parent for a merge (so a merge that only brings in
    /// main's graph files passes, and one that resolves them differently
    /// doesn't).
    fn graph_path(&self, commit: &str, folders: &[String]) -> Result<Option<String>, Refusal> {
        let out = self.git(&[
            "diff-tree",
            "-r",
            "-c",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-z",
            commit,
        ])?;
        if !out.status.success() {
            return Err(Refusal::new(
                Code::IoGitFailed,
                "git couldn't list a pushed commit's changes",
            ));
        }
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .map(String::from_utf8_lossy)
            .find(|p| is_graph_path(p, folders))
            .map(|p| p.into_owned()))
    }
}

/// What a graph-file refusal adds for the case it can't tell apart: work
/// merged on GitHub that the host's main doesn't have yet. Its commits
/// arrive with the push, so both graph checks see them as the worker's
/// (Terra #11089). Worded for the host's operator, as a condition, so a
/// worker that did commit graph files isn't told to blame the host; and
/// the remedy is only updating main, never moving `mdkg.mainRef`, which
/// other parts of mdkg read as a local branch (Terra #11168).
fn behind_hint(main: &str) -> String {
    format!(
        ". If this branch only carries graph changes merged on GitHub, the host's {main} is behind: update it"
    )
}

impl Repo {
    /// The first graph path the branch at `new` changes as a whole, against
    /// its merge base with `main` (#11064). The per-commit check sees only
    /// commits no ref reaches, so this one catches a branch pointed at a
    /// commit some ref already holds (the graph ref's tip) and a merge that
    /// takes graph files from one, which `diff-tree -c` doesn't list. With
    /// no main ref, or no merge base, the whole tree counts.
    fn net_graph_path(
        &self,
        main: &str,
        new: &str,
        folders: &[String],
    ) -> Result<Option<String>, Refusal> {
        let failed = |what: &str| {
            Refusal::new(
                Code::IoGitFailed,
                format!("git couldn't {what} for the pushed branch"),
            )
        };
        let main_commit = format!("{main}^{{commit}}");
        let has_main = self
            .git(&["rev-parse", "--verify", "-q", &main_commit])?
            .status
            .success();
        let base = if has_main {
            let out = self.git(&["merge-base", main, new])?;
            match out.status.code() {
                Some(0) => Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()),
                Some(1) => None,
                _ => return Err(failed("find the merge base with main")),
            }
        } else {
            None
        };
        let base = match base {
            Some(b) => b,
            None => {
                let out = self.git(&["hash-object", "-t", "tree", "/dev/null"])?;
                if !out.status.success() {
                    return Err(failed("name the empty tree"));
                }
                String::from_utf8_lossy(&out.stdout).trim().to_owned()
            }
        };
        let out = self.git(&[
            "diff-tree",
            "-r",
            "--no-renames",
            "--name-only",
            "-z",
            &base,
            new,
        ])?;
        if !out.status.success() {
            return Err(failed("list the changes"));
        }
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .map(String::from_utf8_lossy)
            .find(|p| !p.is_empty() && is_graph_path(p, folders))
            .map(|p| p.into_owned()))
    }
}

/// One path part as the host's file system may read it: with the code
/// points HFS+ ignores dropped, as git's `is_hfs_dot_generic` does, and
/// lowercased, since APFS and HFS+ ignore case (the Kelvin sign U+212A
/// lowercases to `k`).
fn fold(part: &str) -> String {
    part.chars()
        .filter(|c| {
            !matches!(
                c,
                '\u{200C}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{206A}'..='\u{206F}' | '\u{FEFF}'
            )
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether a pushed path is the graph's (S42): a part named `.mdkg`
/// anywhere, or a path in one of the graph's folders, each compared as
/// [`fold`] reads it (Terra #10408).
pub fn is_graph_path(path: &str, folders: &[String]) -> bool {
    let parts: Vec<String> = path.split('/').map(fold).collect();
    parts.iter().any(|p| p == ".mdkg")
        || folders.iter().any(|f| {
            let folder: Vec<String> = f.split('/').map(fold).collect();
            parts.starts_with(&folder)
        })
}

/// Every check one hook makes, in order: the fence, the machine, each
/// ref's owner and placement, no delete or force, and (for `pre-receive`)
/// the scan.
pub fn check(
    hook: Hook,
    machine: &str,
    repo: &Repo,
    common: &Path,
    updates: &[Update],
    deps: &Deps,
) -> Result<(), Refusal> {
    git_serve::check_machine_id(machine)?;
    if !git_serve::fence_held(&git_serve::fence_path(common, machine))? {
        return Err(Refusal::new(
            Code::ApiActorNotAllowed,
            "the push didn't come through git-serve: the machine's git fence isn't held",
        ));
    }
    let book = (deps.book)(common)?;
    git_serve::check_approved(book.as_ref(), machine)?;
    let agents = deps.authority.agents(&repo.git_dir)?;
    for u in updates {
        let agent = owner(&agents, &u.refname)?;
        check_placed(&agents, book.as_ref(), &agent, machine, &u.refname)?;
        if is_zero(&u.new) {
            return Err(Refusal::new(
                Code::CoordBranchNotPushable,
                format!(
                    "{} would be deleted; agent branches are cleaned up on the host",
                    u.refname
                ),
            ));
        }
        if !is_zero(&u.old) && !repo.fast_forward(&u.old, &u.new)? {
            return Err(Refusal::new(
                Code::CoordBranchNotPushable,
                format!(
                    "{} isn't a fast-forward; push a rebase to the agent's next .g<k> generation",
                    u.refname
                ),
            ));
        }
        for commit in repo.added(std::slice::from_ref(u))? {
            if let Some(path) = repo.graph_path(&commit, &agents.graph_folders)? {
                return Err(Refusal::new(
                    Code::CoordBranchNotPushable,
                    format!(
                        "commit {commit} on {} changes {path}; workers never commit graph files, the host publishes them{}",
                        u.refname,
                        behind_hint(&agents.main_ref)
                    ),
                ));
            }
        }
        if let Some(path) = repo.net_graph_path(&agents.main_ref, &u.new, &agents.graph_folders)? {
            return Err(Refusal::new(
                Code::CoordBranchNotPushable,
                format!(
                    "{} changes {} against {}; workers never bring graph files in, the host publishes them{}",
                    printable(&u.refname),
                    printable(&path),
                    printable(&agents.main_ref),
                    behind_hint(&agents.main_ref)
                ),
            ));
        }
    }
    if hook == Hook::PreReceive {
        let commits = repo.added(updates)?;
        if !commits.is_empty()
            && let Some(f) = deps.scanner.scan(&repo.git, &repo.git_dir, &commits)?
        {
            return Err(Refusal::new(
                Code::RunPushRefused,
                format!(
                    "the secret scan matched in commit {} at {}; nothing in this push lands",
                    printable(&f.commit),
                    printable(&f.path)
                ),
            ));
        }
    }
    Ok(())
}

fn env_var(env: &[(OsString, OsString)], name: &str) -> Option<String> {
    env.iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, v)| v.to_str())
        .map(str::to_owned)
}

/// Runs one hook: 0 lets git go on, 1 refuses the whole push. `git_dir`
/// is the folder git runs hooks in.
pub fn run(
    hook: Hook,
    args: &[String],
    stdin: impl Read,
    env: &[(OsString, OsString)],
    git_dir: &Path,
    deps: &Deps,
) -> u8 {
    let machine = env_var(env, MACHINE_VAR).unwrap_or_default();
    let mut refs = Vec::new();
    let result = (|| -> Result<(), Refusal> {
        let git = env_var(env, GIT_VAR)
            .map(PathBuf::from)
            .filter(|g| g.is_absolute())
            .ok_or_else(|| {
                Refusal::new(
                    Code::ApiActorNotAllowed,
                    "the push didn't come through git-serve",
                )
            })?;
        let git_dir = std::fs::canonicalize(git_dir).map_err(|e| {
            Refusal::new(
                Code::IoReadFailed,
                format!("can't find the repository: {e}"),
            )
        })?;
        let updates = match hook {
            Hook::PreReceive => {
                let mut input = String::new();
                stdin
                    .take(16 << 20)
                    .read_to_string(&mut input)
                    .map_err(|_| malformed())?;
                parse_pre_receive(&input)?
            }
            Hook::Update => parse_update(args)?,
        };
        refs = updates.iter().map(|u| u.refname.clone()).collect();
        let repo = Repo {
            git,
            git_dir: git_dir.clone(),
        };
        if hook == Hook::PreReceive {
            let skipped = repo.own_hooks();
            if !skipped.is_empty() {
                eprintln!(
                    "mdkg git-hook: the repository's own hooks don't run for pushes through git-serve: {}",
                    printable(&skipped.join(", "))
                );
            }
        }
        check(hook, &machine, &repo, &git_dir, &updates, deps)
    })();
    let outcome = match &result {
        Ok(()) => "ok",
        Err(r) => {
            // Messages carry client-supplied names and paths: none reaches
            // the pushing client's terminal with a control character in it.
            eprintln!(
                "mdkg git-hook {}: {} ({})",
                hook.as_str(),
                printable(&r.message),
                r.code.as_str()
            );
            r.code.as_str()
        }
    };
    if hook == Hook::PreReceive
        && let Some(home) = env_var(env, "HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())
    {
        git_serve::log(
            &home,
            &machine,
            Some(hook.as_str()),
            Some(git_dir),
            &refs,
            outcome,
        );
    }
    u8::from(result.is_err())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::serve_cli::git_serve::fakes::{Fixture, Tmp, commit, git, run_git, tempdir};
    use crate::serve_cli::git_serve::{MachineRow, Placement, take_fence};

    const M: &str = "01a188d0-835c-77be-b7fc-5bd4eb5d46d7";
    const OTHER: &str = "01a188d0-835c-77be-b7fc-5bd4eb5d46d8";

    fn agents(names: &[&str], retired: &[&str]) -> Agents {
        let defs: BTreeMap<String, serde_json::Value> = names
            .iter()
            .map(|n| ((*n).to_owned(), json!({"retired": retired.contains(n)})))
            .collect();
        Agents::from_authority(
            &json!({}),
            defs.iter(),
            vec!["refs/heads/main".into(), "refs/heads/mdkg/graph".into()],
        )
        .expect("agents")
    }

    #[test]
    fn refs_map_to_one_agent_through_the_branch_pattern() {
        let a = agents(&["a", "codex:pr19", "alice", "alice.g2", "main"], &[]);
        assert_eq!(owner(&a, "refs/heads/agent/a").expect("own"), "a");
        assert_eq!(owner(&a, "refs/heads/agent/a.g2").expect("gen"), "a");
        assert_eq!(owner(&a, "refs/heads/agent/a.g10").expect("gen"), "a");
        assert_eq!(
            owner(&a, "refs/heads/agent/codex%3Apr19").expect("encoded"),
            "codex:pr19"
        );
        assert_eq!(
            owner(&a, "refs/heads/agent/codex%3Apr19.g3").expect("encoded gen"),
            "codex:pr19"
        );
        assert_eq!(
            owner(&a, "refs/heads/agent/main").expect("an agent named main"),
            "main"
        );
        let ambiguous = owner(&a, "refs/heads/agent/alice.g2").expect_err("two owners");
        assert!(
            ambiguous.message.contains("alice and alice.g2"),
            "{}",
            ambiguous.message
        );
        for bad in [
            "refs/heads/main",
            "refs/heads/mdkg/graph",
            "refs/heads/agent/b",
            "refs/heads/agent/a.g0",
            "refs/heads/agent/a.g01",
            "refs/heads/agent/a.g",
            "refs/heads/agent/a.gx",
            "refs/heads/agent/a/x",
            "refs/heads/agent/codex:pr19",
            "refs/tags/agent/a",
            "refs/mdkg/graph",
            "HEAD",
        ] {
            assert_eq!(
                owner(&a, bad).expect_err(bad).code,
                Code::CoordBranchNotPushable,
                "{bad}"
            );
        }
        let custom = Agents::from_authority(
            &json!({"orchestration": {"branch": "lanes/{agent}/work"}}),
            BTreeMap::from([("a".to_owned(), json!({}))]).iter(),
            vec![],
        )
        .expect("agents");
        assert_eq!(owner(&custom, "refs/heads/lanes/a/work").expect("own"), "a");
        assert_eq!(
            owner(&custom, "refs/heads/lanes/a/work.g2").expect("gen"),
            "a"
        );
        assert!(owner(&custom, "refs/heads/agent/a").is_err());
    }

    #[test]
    fn hook_input_is_parsed_strictly() {
        let (z, a) = ("0".repeat(40), "a".repeat(40));
        let ups = parse_pre_receive(&format!(
            "{z} {a} refs/heads/agent/a\n{a} {z} refs/heads/x\n"
        ))
        .expect("ok");
        assert_eq!(ups.len(), 2);
        assert_eq!(ups[0].refname, "refs/heads/agent/a");
        assert!(parse_pre_receive("x y z\n").is_err());
        assert!(parse_pre_receive(&format!("{z} {} refs/heads/a\n", "b".repeat(64))).is_err());
        assert!(parse_update(&["refs/heads/a".into(), z.clone(), a.clone()]).is_ok());
        assert!(parse_update(&["refs/heads/a".into(), z]).is_err());
    }

    struct Host {
        _tmp: Tmp,
        git_dir: std::path::PathBuf,
        work: std::path::PathBuf,
        main: String,
    }

    /// A host repository with main, and a worker clone whose commits are
    /// fetched into the host without a ref, as a push's are before git
    /// moves the refs.
    fn host() -> Host {
        let tmp = tempdir();
        let host = tmp.path().join("host");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&host).expect("mkdir");
        run_git(&host, &["init", "-q", "-b", "main"]);
        let main = commit(
            &host,
            &[(".mdkg/config.json", "{}\n"), ("src/a.rs", "1\n")],
            "main",
        );
        run_git(
            tmp.path(),
            &["clone", "-q", host.to_str().expect("utf-8"), "work"],
        );
        Host {
            git_dir: host.join(".git"),
            _tmp: tmp,
            work,
            main,
        }
    }

    impl Host {
        /// Commits in the worker on `branch` and brings the objects over.
        fn push_objects(&self, branch: &str, files: &[(&str, &str)]) -> String {
            run_git(&self.work, &["checkout", "-q", "-B", branch]);
            let sha = commit(&self.work, files, branch);
            run_git(
                self.git_dir.parent().expect("top"),
                &["fetch", "-q", self.work.to_str().expect("utf-8"), &sha],
            );
            sha
        }

        fn repo(&self) -> Repo {
            Repo {
                git: git(),
                git_dir: self.git_dir.clone(),
            }
        }
    }

    fn fixture(placed_on: &str, state: PlacementState) -> Fixture {
        Fixture {
            machines: BTreeMap::from([(M.to_owned(), MachineRow::Approved)]),
            machine_after: BTreeMap::new(),
            placements: BTreeMap::from([(
                "a".to_owned(),
                Placement {
                    machine: placed_on.to_owned(),
                    state,
                },
            )]),
            config: json!({}),
            definitions: BTreeMap::from([("a".to_owned(), json!({})), ("b".to_owned(), json!({}))]),
            protected: vec!["refs/heads/main".into()],
            marker: Some(concat!("FAKE-", "SECRET-", "MARKER").to_owned()),
            graph_folders: vec![],
        }
    }

    fn up(refname: &str, old: &str, new: &str) -> Update {
        Update {
            refname: refname.into(),
            old: old.into(),
            new: new.into(),
        }
    }

    fn run_check(h: &Host, f: &Fixture, hook: Hook, updates: &[Update]) -> Result<(), Refusal> {
        check(hook, M, &h.repo(), &h.git_dir, updates, &f.deps())
    }

    #[test]
    fn the_hooks_accept_only_this_machines_agents_fast_forwards() {
        let h = host();
        let fence = take_fence(
            &git_serve::fence_path(&h.git_dir, M),
            Duration::from_millis(10),
        )
        .expect("fence");
        let zero = "0".repeat(40);
        let f = fixture(M, PlacementState::Placed);
        let first = h.push_objects("agent/a", &[("src/a.rs", "2\n")]);
        let push = up("refs/heads/agent/a", &zero, &first);
        for hook in [Hook::PreReceive, Hook::Update] {
            run_check(&h, &f, hook, std::slice::from_ref(&push))
                .expect("a's branch from its machine");
        }
        run_check(
            &h,
            &f,
            Hook::PreReceive,
            &[up("refs/heads/agent/a.g2", &zero, &first)],
        )
        .expect("a generation");

        let code = |r: Result<(), Refusal>| r.expect_err("refused").code;
        // Another agent's branch, main, a delete, a force push.
        assert_eq!(
            code(run_check(
                &h,
                &f,
                Hook::Update,
                &[up("refs/heads/agent/b", &zero, &first)]
            )),
            Code::CoordBranchNotPushable
        );
        assert_eq!(
            code(run_check(
                &h,
                &f,
                Hook::Update,
                &[up("refs/heads/main", &h.main, &first)]
            )),
            Code::CoordBranchNotPushable
        );
        assert_eq!(
            code(run_check(
                &h,
                &f,
                Hook::Update,
                &[up("refs/heads/agent/a", &first, &zero)]
            )),
            Code::CoordBranchNotPushable
        );
        run_git(&h.work, &["checkout", "-q", "origin/main"]);
        let sideways = h.push_objects("side", &[("src/b.rs", "x\n")]);
        let r = run_check(
            &h,
            &f,
            Hook::Update,
            &[up("refs/heads/agent/a", &first, &sideways)],
        )
        .expect_err("force");
        assert!(r.message.contains("fast-forward"), "{}", r.message);
        // Placed elsewhere, retiring, unplaced, retired.
        for f in [
            fixture(OTHER, PlacementState::Placed),
            fixture(M, PlacementState::Retiring),
            Fixture {
                placements: BTreeMap::new(),
                ..fixture(M, PlacementState::Placed)
            },
            Fixture {
                definitions: BTreeMap::from([("a".to_owned(), json!({"retired": true}))]),
                ..fixture(M, PlacementState::Placed)
            },
        ] {
            assert_eq!(
                code(run_check(&h, &f, Hook::Update, std::slice::from_ref(&push))),
                Code::CoordBranchNotPushable
            );
        }
        // The machine revoked after git-serve's checks.
        for row in [MachineRow::Revoking, MachineRow::Revoked] {
            let f = Fixture {
                machines: BTreeMap::from([(M.to_owned(), row)]),
                ..fixture(M, PlacementState::Placed)
            };
            assert_eq!(
                code(run_check(
                    &h,
                    &f,
                    Hook::PreReceive,
                    std::slice::from_ref(&push)
                )),
                Code::ApiMachineRevoked
            );
        }
        drop(fence);
        // Without the fence the push didn't come through git-serve.
        assert_eq!(
            code(run_check(
                &h,
                &f,
                Hook::PreReceive,
                std::slice::from_ref(&push)
            )),
            Code::ApiActorNotAllowed
        );
    }

    #[test]
    fn the_scan_refuses_the_whole_push_naming_commit_and_path_only() {
        let h = host();
        let _fence = take_fence(
            &git_serve::fence_path(&h.git_dir, M),
            Duration::from_millis(10),
        )
        .expect("fence");
        let zero = "0".repeat(40);
        let f = fixture(M, PlacementState::Placed);
        let marker = f.marker.clone().expect("marker");
        let clean = h.push_objects("agent/a", &[("src/a.rs", "2\n")]);
        let dirty = h.push_objects("agent/a", &[("src/key.txt", &format!("token={marker}\n"))]);
        let r = run_check(
            &h,
            &f,
            Hook::PreReceive,
            &[up("refs/heads/agent/a", &zero, &dirty)],
        )
        .expect_err("scan");
        assert_eq!(r.code, Code::RunPushRefused);
        assert!(
            r.message.contains(&dirty) && r.message.contains("src/key.txt"),
            "{}",
            r.message
        );
        assert!(!r.message.contains(&marker), "never the matched text");
        // The production scanner reads paths unquoted: a pushed name's
        // control characters (ESC, CR, BEL) are dropped from the refusal,
        // the rest of it kept (#11552).
        let token = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");
        let hostile = h.push_objects(
            "agent/a",
            &[("src/k\u{1b}[2J\r\u{7}ey.txt", &format!("token={token}\n"))],
        );
        let mut deps = f.deps();
        deps.scanner = git_serve::production().scanner;
        let r = check(
            Hook::PreReceive,
            M,
            &h.repo(),
            &h.git_dir,
            &[up("refs/heads/agent/a", &zero, &hostile)],
            &deps,
        )
        .expect_err("scan");
        assert_eq!(r.code, Code::RunPushRefused);
        assert!(
            !r.message.contains(['\u{1b}', '\r', '\u{7}']),
            "{:?}",
            r.message
        );
        assert!(r.message.contains("src/k[2Jey.txt"), "{:?}", r.message);
        assert!(!r.message.contains(token), "never the matched text");
        // The update hook leaves the scan to pre-receive, which sees it all.
        run_check(
            &h,
            &f,
            Hook::Update,
            &[up("refs/heads/agent/a", &zero, &dirty)],
        )
        .expect("update");
        run_check(
            &h,
            &f,
            Hook::PreReceive,
            &[up("refs/heads/agent/a", &zero, &clean)],
        )
        .expect("clean");
    }

    #[test]
    fn workers_never_push_graph_files() {
        let h = host();
        let _fence = take_fence(
            &git_serve::fence_path(&h.git_dir, M),
            Duration::from_millis(10),
        )
        .expect("fence");
        let zero = "0".repeat(40);
        let f = fixture(M, PlacementState::Placed);
        let node = h.push_objects("agent/a", &[("sub/.mdkg/work/task-1.md", "x\n")]);
        let r = run_check(
            &h,
            &f,
            Hook::Update,
            &[up("refs/heads/agent/a", &zero, &node)],
        )
        .expect_err("graph file");
        assert!(
            r.message.contains(&node) && r.message.contains("sub/.mdkg/work/task-1.md"),
            "{}",
            r.message
        );

        // A merge that only brings in main's graph files passes.
        let host_top = h.git_dir.parent().expect("top");
        commit(
            host_top,
            &[(".mdkg/config.json", "{\"x\":1}\n")],
            "main moves",
        );
        run_git(&h.work, &["fetch", "-q", "origin"]);
        run_git(
            &h.work,
            &["checkout", "-q", "-B", "agent/a", "origin/main~1"],
        );
        commit(&h.work, &[("src/c.rs", "c\n")], "work");
        run_git(&h.work, &["merge", "-q", "--no-edit", "origin/main"]);
        let merge = run_git(&h.work, &["rev-parse", "HEAD"]);
        run_git(
            host_top,
            &["fetch", "-q", h.work.to_str().expect("utf-8"), &merge],
        );
        run_check(
            &h,
            &f,
            Hook::Update,
            &[up("refs/heads/agent/a", &zero, &merge)],
        )
        .expect("a merge of main");
    }

    /// #11064: a branch's net change against main is checked too. A ref
    /// pointed at the graph ref's tip adds no commit, and a merge that
    /// takes graph files from it shows none in `diff-tree -c`; both are
    /// refused, and a merge of main still passes.
    #[test]
    fn a_branchs_net_change_never_brings_graph_files_in() {
        let h = host();
        let _fence = take_fence(
            &git_serve::fence_path(&h.git_dir, M),
            Duration::from_millis(10),
        )
        .expect("fence");
        let zero = "0".repeat(40);
        let f = fixture(M, PlacementState::Placed);
        let host_top = h.git_dir.parent().expect("top").to_path_buf();
        // The host's graph ref: its own history, holding graph files.
        run_git(&host_top, &["checkout", "-q", "--orphan", "seed"]);
        run_git(&host_top, &["rm", "-rfq", "--cached", "."]);
        let graph = commit(&host_top, &[(".mdkg/work/task-g.md", "g\n")], "graph");
        run_git(&host_top, &["update-ref", "refs/heads/mdkg-graph", &graph]);
        run_git(&host_top, &["checkout", "-qf", "main"]);
        run_git(&host_top, &["branch", "-D", "-q", "seed"]);
        run_git(&host_top, &["clean", "-fdxq"]);

        // A generation pointed at the graph ref's tip.
        let r = run_check(
            &h,
            &f,
            Hook::Update,
            &[up("refs/heads/agent/a.g1", &zero, &graph)],
        )
        .expect_err("the graph ref's tip");
        assert!(
            r.message.contains(".mdkg/work/task-g.md") && r.message.contains("against"),
            "{}",
            r.message
        );

        // A merge that takes the graph files from it.
        run_git(&h.work, &["fetch", "-q", "origin", "refs/heads/mdkg-graph"]);
        run_git(&h.work, &["checkout", "-q", "-B", "agent/a", &h.main]);
        commit(&h.work, &[("src/x.rs", "x\n")], "work");
        run_git(
            &h.work,
            &[
                "merge",
                "-q",
                "--no-edit",
                "--allow-unrelated-histories",
                "FETCH_HEAD",
            ],
        );
        let merge = run_git(&h.work, &["rev-parse", "HEAD"]);
        run_git(
            &host_top,
            &["fetch", "-q", h.work.to_str().expect("utf-8"), &merge],
        );
        for hook in [Hook::PreReceive, Hook::Update] {
            let r = run_check(&h, &f, hook, &[up("refs/heads/agent/a", &zero, &merge)])
                .expect_err("a merge of the graph");
            assert!(
                r.message.contains(".mdkg/work/task-g.md") && r.message.contains("against"),
                "{}",
                r.message
            );
        }
    }

    /// Terra #11089: main moved on GitHub (here, a migration that drops
    /// main's `.mdkg`) and the host hasn't pulled. A branch made from the
    /// new main is refused, naming the remedy; once the host's main has
    /// those commits, the same push passes.
    #[test]
    fn a_host_main_behind_github_is_named_and_then_passes() {
        let h = host();
        let _fence = take_fence(
            &git_serve::fence_path(&h.git_dir, M),
            Duration::from_millis(10),
        )
        .expect("fence");
        let zero = "0".repeat(40);
        let f = fixture(M, PlacementState::Placed);
        run_git(&h.work, &["checkout", "-q", "-B", "upstream", &h.main]);
        run_git(&h.work, &["rm", "-q", ".mdkg/config.json"]);
        run_git(
            &h.work,
            &["commit", "-q", "-m", "migrate: the graph moves to its ref"],
        );
        let upstream = run_git(&h.work, &["rev-parse", "HEAD"]);
        let tip = h.push_objects("agent/a", &[("src/b.rs", "b\n")]);
        let push = [up("refs/heads/agent/a", &zero, &tip)];
        for hook in [Hook::PreReceive, Hook::Update] {
            let r = run_check(&h, &f, hook, &push).expect_err("main behind");
            assert!(
                r.message.contains(".mdkg/config.json")
                    && r.message.contains("merged on GitHub")
                    && r.message.contains("is behind: update it")
                    && !r.message.contains("mdkg.mainRef"),
                "{}",
                r.message
            );
        }
        let host_top = h.git_dir.parent().expect("top");
        run_git(host_top, &["update-ref", "refs/heads/main", &upstream]);
        for hook in [Hook::PreReceive, Hook::Update] {
            run_check(&h, &f, hook, &push).expect("main caught up");
        }
    }

    #[test]
    fn graph_folders_match_as_the_host_reads_them() {
        for part in [
            ".mdkg",
            ".MDKG",
            ".Mdkg",
            ".md\u{212A}g",
            ".md\u{200C}kg",
            "\u{FEFF}.mdkg",
            ".m\u{202E}DKG\u{206F}",
        ] {
            assert!(is_graph_path(part, &[]), "{part:?}");
        }
        for part in ["mdkg", ".mdkg2", ".mdkg.", "x.mdkg", ".md kg", ".mdg"] {
            assert!(!is_graph_path(part, &[]), "{part:?}");
        }
        // A configured folder, a workspace's included, folds the same way.
        let folders = [".knowledge".to_owned(), "apps/web/graph".to_owned()];
        for path in [
            ".knowledge/work/t.md",
            ".KNOWLEDGE/config.json",
            "Apps/Web/\u{200D}Graph/x",
            "apps/web/graph",
        ] {
            assert!(is_graph_path(path, &folders), "{path:?}");
        }
        for path in [
            "apps/web/graphs/x",
            "apps/graph/x",
            "knowledge/x",
            "src/a.rs",
        ] {
            assert!(!is_graph_path(path, &folders), "{path:?}");
        }
    }

    /// Terra #10408: a case or HFS spelling of `.mdkg` is refused too.
    /// The commits are built from the index, so the case-insensitive
    /// disk a test may run on never folds the names first.
    #[test]
    fn graph_files_in_any_spelling_are_refused() {
        let h = host();
        let _fence = take_fence(
            &git_serve::fence_path(&h.git_dir, M),
            Duration::from_millis(10),
        )
        .expect("fence");
        let zero = "0".repeat(40);
        let f = fixture(M, PlacementState::Placed);
        run_git(&h.work, &["checkout", "-q", "-B", "agent/a"]);
        let host_top = h.git_dir.parent().expect("top");
        for path in [
            ".MDKG/work/task-x.md",
            "a/.Mdkg/config.json",
            ".md\u{200C}kg/config.json",
        ] {
            // Each commit sits right on main, so it adds only `path`.
            run_git(&h.work, &["reset", "-q", "--hard", &h.main]);
            let blob = run_git(&h.work, &["hash-object", "-w", "--stdin"]);
            run_git(
                &h.work,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{blob},{path}"),
                ],
            );
            run_git(&h.work, &["commit", "-q", "-m", path]);
            let sha = run_git(&h.work, &["rev-parse", "HEAD"]);
            run_git(
                host_top,
                &["fetch", "-q", h.work.to_str().expect("utf-8"), &sha],
            );
            for hook in [Hook::PreReceive, Hook::Update] {
                let r = run_check(&h, &f, hook, &[up("refs/heads/agent/a", &zero, &sha)])
                    .expect_err(path);
                assert_eq!(r.code, Code::CoordBranchNotPushable, "{path}");
                assert!(r.message.contains(path), "{}", r.message);
            }
        }
    }

    #[test]
    fn the_repositorys_own_hooks_are_named_as_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let h = host();
        assert!(
            h.repo().own_hooks().is_empty(),
            "git's samples aren't executable hooks"
        );
        let hook = h.git_dir.join("hooks/post-receive");
        std::fs::write(&hook, "#!/bin/sh\n").expect("hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(h.repo().own_hooks(), ["post-receive"]);
        let own = h.git_dir.parent().expect("top").join("own-hooks");
        std::fs::create_dir_all(&own).expect("mkdir");
        std::fs::copy(&hook, own.join("update")).expect("copy");
        run_git(&h.git_dir, &["config", "core.hooksPath", "own-hooks"]);
        assert_eq!(h.repo().own_hooks(), ["update"]);
        let _ = Path::new("");
    }
}

/// SC56 end to end: real pushes and fetches through `git-serve`, its
/// hooks and git's receive-pack, with this test binary standing in for
/// sshd's forced command and for the `mdkg` the hooks name.
#[cfg(test)]
mod sc56 {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    use serde_json::json;

    use super::*;
    use crate::serve_cli::git_serve::fakes::{Fixture, Tmp, commit, git, run_git, tempdir};
    use crate::serve_cli::git_serve::{MachineRow, Placement};

    const M: &str = "01a188d0-835c-77be-b7fc-5bd4eb5d46d7";
    const OTHER: &str = "01a188d0-835c-77be-b7fc-5bd4eb5d46d8";
    const FIXTURE: &str = "MDKG_GIT_SERVE_TEST_FIXTURE";

    /// The hooks, as the test binary: runs only when the stand-in `mdkg`
    /// starts it, and exits with the hook's code.
    #[test]
    fn as_hook() {
        let Some(fixture) = std::env::var_os(FIXTURE) else {
            return;
        };
        let f = Fixture::from_json(
            &serde_json::from_slice(&std::fs::read(fixture).expect("fixture")).expect("json"),
        );
        let words: Vec<String> = std::env::var("MDKG_GIT_SERVE_TEST_HOOK")
            .expect("hook")
            .split(' ')
            .map(str::to_owned)
            .collect();
        let hook = if words[0] == "pre-receive" {
            Hook::PreReceive
        } else {
            Hook::Update
        };
        let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        let dir = std::env::current_dir().expect("cwd");
        let code = run(
            hook,
            &words[1..],
            std::io::stdin().lock(),
            &env,
            &dir,
            &f.deps(),
        );
        std::process::exit(i32::from(code));
    }

    /// `git-serve`, as the test binary: runs only when the stand-in ssh
    /// starts it, with git's protocol on fd 3 (the harness prints to fd 1).
    #[test]
    fn as_git_serve() {
        let Some(fixture) = std::env::var_os(FIXTURE) else {
            return;
        };
        let f = Fixture::from_json(
            &serde_json::from_slice(&std::fs::read(&fixture).expect("fixture")).expect("json"),
        );
        let exe = PathBuf::from(std::env::var_os("MDKG_GIT_SERVE_TEST_EXE").expect("exe"));
        let out = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/fd/3")
            .expect("fd 3");
        let code = git_serve::serve_to(M, std::env::vars_os(), &exe, &f.deps(), out.into());
        std::process::exit(i32::from(code));
    }

    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("script");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    struct World {
        tmp: Tmp,
        home: PathBuf,
        repo: PathBuf,
        work: PathBuf,
        ssh: PathBuf,
    }

    fn world(f: &Fixture) -> World {
        let tmp = tempdir();
        let home = tmp.path().join("home");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        run_git(&repo, &["init", "-q", "-b", "main"]);
        commit(&repo, &[(".mdkg/config.json", "{}\n")], "main");
        run_git(
            tmp.path(),
            &["clone", "-q", repo.to_str().expect("utf-8"), "work"],
        );
        let config = git_serve::config_path(&home);
        std::fs::create_dir_all(config.parent().expect("dir")).expect("mkdir");
        std::fs::write(
            &config,
            json!({"git_serve": {"repositories": [repo], "push_timeout_s": 60, "git": git()}})
                .to_string(),
        )
        .expect("config");
        let fixture = tmp.path().join("fixture.json");
        std::fs::write(&fixture, f.to_json().to_string()).expect("fixture");
        let test_bin = std::env::current_exe().expect("test binary");
        let q = |p: &Path| format!("'{}'", p.display());
        // The `mdkg` the hooks name.
        let mdkg = tmp.path().join("mdkg");
        script(
            &mdkg,
            &format!(
                "shift\nMDKG_GIT_SERVE_TEST_HOOK=\"$*\" {FIXTURE}={} exec {} --exact serve_cli::git_hook::sc56::as_hook --nocapture -q --test-threads=1",
                q(&fixture),
                q(&test_bin)
            ),
        );
        // sshd running the forced command, as the owner, with HOME and a
        // shell startup's GIT_DIR and MDKG_ROOT.
        let ssh = tmp.path().join("ssh");
        script(
            &ssh,
            &format!(
                "for a; do last=$a; done\nexec 3>&1 1>&2\nSSH_ORIGINAL_COMMAND=$last HOME={} GIT_DIR=/nowhere MDKG_ROOT=/nowhere MDKG_GIT_SERVE_TEST_EXE={} {FIXTURE}={} exec {} --exact serve_cli::git_hook::sc56::as_git_serve --nocapture -q --test-threads=1",
                q(&home),
                q(&mdkg),
                q(&fixture),
                q(&test_bin)
            ),
        );
        World {
            work: tmp.path().join("work"),
            tmp,
            home,
            repo,
            ssh,
        }
    }

    impl World {
        fn client(&self, args: &[&str]) -> Output {
            let url = format!("ssh://host{}", self.repo.display());
            let args: Vec<String> = args.iter().map(|a| a.replace("URL", &url)).collect();
            Command::new(git())
                .args(&args)
                .current_dir(&self.work)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_SSH_COMMAND", &self.ssh)
                .env("GIT_SSH_VARIANT", "simple")
                // The ssh here is the fixture's local command, so allow
                // exactly that transport, whatever the runner allows.
                .env("GIT_ALLOW_PROTOCOL", "ssh")
                .output()
                .expect("git client")
        }

        fn host_ref(&self, name: &str) -> Option<String> {
            let out = Command::new(git())
                .args(["rev-parse", "--verify", "--quiet", name])
                .current_dir(&self.repo)
                .output()
                .expect("git");
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        }

        fn hooks(&self) -> PathBuf {
            git_serve::hook_dir(&self.home, &self.tmp.path().join("mdkg"))
        }
    }

    fn refused(out: &Output, says: &str) {
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "pushed: {err}");
        assert!(err.contains(says), "{says:?} not in: {err}");
    }

    #[test]
    fn pushes_and_fetches_through_git_serve() {
        let marker = concat!("FAKE-", "SECRET-", "MARKER");
        let f = Fixture {
            machines: BTreeMap::from([(M.to_owned(), MachineRow::Approved)]),
            machine_after: BTreeMap::new(),
            placements: BTreeMap::from([
                (
                    "a".to_owned(),
                    Placement {
                        machine: M.into(),
                        state: PlacementState::Placed,
                    },
                ),
                (
                    "codex:pr19".to_owned(),
                    Placement {
                        machine: M.into(),
                        state: PlacementState::Placed,
                    },
                ),
                (
                    "b".to_owned(),
                    Placement {
                        machine: OTHER.into(),
                        state: PlacementState::Placed,
                    },
                ),
            ]),
            config: json!({}),
            definitions: BTreeMap::from([
                ("a".to_owned(), json!({})),
                ("b".to_owned(), json!({})),
                ("codex:pr19".to_owned(), json!({})),
            ]),
            protected: vec!["refs/heads/main".into()],
            marker: Some(marker.into()),
            graph_folders: vec![],
        };
        let w = world(&f);
        run_git(&w.work, &["checkout", "-q", "-b", "agent/a"]);
        let first = commit(&w.work, &[("src/a.rs", "1\n")], "a 1");

        // a's branch, a generation and an encoded name land; a fetch works.
        let out = w.client(&["push", "-q", "URL", "agent/a"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            w.host_ref("refs/heads/agent/a").as_deref(),
            Some(first.as_str())
        );
        assert!(
            w.client(&["push", "-q", "URL", "agent/a:agent/a.g2"])
                .status
                .success()
        );
        assert!(
            w.client(&["push", "-q", "URL", "agent/a:agent/codex%3Apr19"])
                .status
                .success()
        );
        let out = w.client(&["fetch", "-q", "URL", "main"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(w.hooks().join("pre-receive").is_file());

        // Each refused, with no ref moved.
        let main = w.host_ref("refs/heads/main");
        refused(
            &w.client(&["push", "-q", "URL", "agent/a:main"]),
            "never pushed from a machine",
        );
        assert_eq!(w.host_ref("refs/heads/main"), main);
        refused(
            &w.client(&["push", "-q", "URL", "agent/a:agent/b"]),
            "placed on another machine",
        );
        assert_eq!(w.host_ref("refs/heads/agent/b"), None);
        run_git(&w.work, &["commit", "-q", "--amend", "-m", "rewritten"]);
        refused(
            &w.client(&["push", "-q", "--force", "URL", "agent/a"]),
            "fast-forward",
        );
        refused(&w.client(&["push", "-q", "URL", ":agent/a"]), "deleted");
        assert_eq!(
            w.host_ref("refs/heads/agent/a").as_deref(),
            Some(first.as_str())
        );
        run_git(&w.work, &["reset", "-q", "--hard", &first]);
        let dirty = commit(
            &w.work,
            &[("src/key.txt", &format!("k={marker}\n"))],
            "oops",
        );
        let out = w.client(&["push", "-q", "URL", "agent/a"]);
        refused(&out, &format!("commit {dirty} at src/key.txt"));
        assert!(!String::from_utf8_lossy(&out.stderr).contains(marker));
        // Through git-serve, a hostile filename reaches the pushing client
        // with no control character in it (#11552).
        run_git(&w.work, &["reset", "-q", "--hard", &first]);
        let hostile = commit(
            &w.work,
            &[("src/k\u{1b}]0;x\u{7}\rey.txt", &format!("k={marker}\n"))],
            "hostile",
        );
        let out = w.client(&["push", "-q", "URL", "agent/a"]);
        refused(&out, &format!("commit {hostile} at"));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains(['\u{1b}', '\u{7}']), "{err:?}");
        assert!(
            err.lines()
                .filter(|l| l.contains("secret scan"))
                .all(|l| !l.contains('\r')),
            "{err:?}"
        );
        assert_eq!(
            w.host_ref("refs/heads/agent/a").as_deref(),
            Some(first.as_str())
        );

        // The hook folder deleted: git-serve puts it back, and main is
        // still refused.
        std::fs::remove_dir_all(w.hooks()).expect("rm hooks");
        refused(
            &w.client(&["push", "-q", "URL", "agent/a:main"]),
            "never pushed from a machine",
        );
        assert!(w.hooks().join("update").is_file(), "reinstalled");
        assert_eq!(w.host_ref("refs/heads/main"), main);
    }
}
