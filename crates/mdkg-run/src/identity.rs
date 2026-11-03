//! Map a hook's worktree to its actor without a process environment or a
//! basename guess, from the graph authority only (R1, orchestrator #10414,
//! #11198). A saved location counts only for an actor the authority defines,
//! and takes precedence over its configured one. The committed worktree
//! pattern also names an actor with no definition (an interactive
//! receiver), but never one with saved state: saved state whose definition
//! is gone (never defined, or removed at the tip) is retired and names
//! nobody.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use mdkg_coord_sqlite::git;
use serde_json::Value;

use crate::RunError;
use crate::orchestration::{AGENT, Names, agent, authority_or_absent};
use crate::state::{self, AgentState};

fn bad(message: &str) -> RunError {
    RunError::failure(crate::error::CODE_UNKNOWN_AGENT, message)
}

/// The actor and graph authority captured for one operation. Callers that
/// need identity more than once must pass the same snapshot through the whole
/// operation, including its already-resolved actor.
#[derive(Debug, Clone)]
pub struct IdentitySnapshot {
    pub actor: Option<String>,
    /// None is reserved for a positively identified absent checkout graph.
    pub authority: Option<mdkg_graph::graphref::Authority>,
}

/// Capture one authority and resolve exactly one actor for `cwd` in the
/// repository containing `root`. Conflicting or unreadable evidence is an
/// error; callers must not guess from an environment or basename fallback.
pub fn capture_for_cwd(root: &Path, cwd: &Path) -> Result<IdentitySnapshot, RunError> {
    capture(root, cwd, None)
}

/// [`capture_for_cwd`] with an authority the operation already pinned for
/// `root`, so its identity can't come from a later graph tip.
pub fn capture_with(
    root: &Path,
    cwd: &Path,
    authority: mdkg_graph::graphref::Authority,
) -> Result<IdentitySnapshot, RunError> {
    capture(root, cwd, Some(authority))
}

fn capture(
    root: &Path,
    cwd: &Path,
    pinned: Option<mdkg_graph::graphref::Authority>,
) -> Result<IdentitySnapshot, RunError> {
    let repo = git::check_worktree(root)
        .map_err(|e| RunError::io("can't check the hook's repository", e))?;
    let here =
        git::check_worktree(cwd).map_err(|e| RunError::io("can't check the hook's worktree", e))?;
    if here.common != repo.common {
        return Err(bad("the hook's cwd belongs to another repository"));
    }
    let authority = match pinned {
        Some(authority) => authority,
        None => match authority_or_absent(root)? {
            Some(authority) => authority,
            // A positively absent graph names no actor, even when saved
            // state maps this checkout (orchestrator #10414).
            None => {
                return Ok(IdentitySnapshot {
                    actor: None,
                    authority: None,
                });
            }
        },
    };
    let cfg = agent::authority_config(&authority)?;
    let saved = saved_agents(&repo.common.join("mdkg/run"))?;
    // This is also the launcher's pattern anchor. Separate git dirs and bare
    // repositories need saved locations or explicit absolute config paths.
    let main = (repo.common.file_name().is_some_and(|s| s == ".git"))
        .then(|| repo.common.parent())
        .flatten();
    let actor = resolve(&here.top, &here.git_dir, main, &cfg, &saved)?;
    Ok(IdentitySnapshot {
        actor,
        authority: Some(authority),
    })
}

/// Resolve one actor without keeping the snapshot. Missing identity is None;
/// this does not select a session or determine headless eligibility.
pub fn actor_for_cwd(root: &Path, cwd: &Path) -> Result<Option<String>, RunError> {
    Ok(capture_for_cwd(root, cwd)?.actor)
}

fn saved_agents(dir: &Path) -> Result<BTreeMap<String, AgentState>, RunError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(RunError::io("can't list agent identity records", e)),
    };
    let mut agents = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| RunError::io("can't list agent identity records", e))?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        // No blocking on a FIFO, no unbounded reads, no link to another
        // record. A broken record must not disappear from the candidate set.
        if !entry
            .file_type()
            .map_err(|e| RunError::io("can't inspect agent identity", e))?
            .is_file()
        {
            return Err(bad("an agent identity record isn't a regular file"));
        }
        const CAP: u64 = 1 << 20;
        let bytes = git::read_capped(&path, CAP + 1)
            .map_err(|e| RunError::io("can't read agent identity", e))?;
        if bytes.len() as u64 > CAP {
            return Err(bad("an agent identity record is too large"));
        }
        let agent: AgentState = serde_json::from_slice(&bytes)
            .map_err(|_| bad("an agent identity record isn't valid launcher state"))?;
        state::check_name(&agent.spec.name)?;
        if path != state::state_path(dir, &agent.spec.name) {
            return Err(bad(
                "an agent identity record's name disagrees with its file",
            ));
        }
        agents.insert(agent.spec.name.clone(), agent);
    }
    Ok(agents)
}

/// Find the nearest physical checkout boundary, never a parent worktree just
/// because it contains this directory. Only a candidate equal to the already
/// checked target top can contribute an actor. Other checkouts are not run.
fn location_top(cwd: &Path) -> Result<Option<PathBuf>, RunError> {
    if !cwd.is_absolute() {
        return Err(bad(
            "an unsaved relative agent cwd has no unique launcher origin",
        ));
    }
    let cwd = match fs::canonicalize(cwd) {
        Ok(cwd) => cwd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RunError::io("can't resolve agent cwd", e)),
    };
    for p in cwd.ancestors() {
        match fs::symlink_metadata(p.join(".git")) {
            Ok(_) => return Ok(Some(p.into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(RunError::io("can't inspect agent checkout boundary", e)),
        }
    }
    Ok(None)
}

/// A generated worktree path names that checkout itself, never a directory
/// within another checkout. As in the launcher, resolve its parents but
/// refuse a link in the final component (it could name another actor's tree).
fn exact_worktree(path: &Path) -> Result<Option<PathBuf>, RunError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RunError::io("can't inspect configured worktree", e)),
    };
    if !meta.is_dir() {
        return Err(bad(
            "a configured worktree must be a directory, not a link or file",
        ));
    }
    let (Some(parent), Some(last)) = (path.parent(), path.file_name()) else {
        return Err(bad("a configured worktree has no final path component"));
    };
    let real = fs::canonicalize(parent)
        .map_err(|e| RunError::io("can't resolve configured worktree parent", e))?
        .join(last);
    Ok((location_top(&real)?.as_ref() == Some(&real)).then_some(real))
}

fn resolve(
    top: &Path,
    git_dir: &Path,
    main: Option<&Path>,
    cfg: &Value,
    saved: &BTreeMap<String, AgentState>,
) -> Result<Option<String>, RunError> {
    let names = Names::from_config(cfg)?;
    let configured = match cfg.get("agents") {
        None | Some(Value::Null) => None,
        Some(Value::Object(o)) => Some(o),
        Some(_) => return Err(bad("agents must be an object")),
    };
    let defined =
        |name: &str| name != "defaults" && configured.is_some_and(|c| c.contains_key(name));
    let mut matches = BTreeSet::new();
    for (name, agent) in saved {
        // Saved state only chooses among actors the authority defines.
        if !defined(name) {
            continue;
        }
        if location_top(&agent.spec.cwd)?.as_deref() == Some(top) {
            if let Some(pin) = &agent.spec.pinned
                && (fs::canonicalize(&pin.top).ok().as_deref() != Some(top)
                    || fs::canonicalize(&pin.git_dir).ok().as_deref() != Some(git_dir))
            {
                return Err(bad(
                    "a saved agent's cwd disagrees with its pinned worktree",
                ));
            }
            matches.insert(name.clone());
        }
    }
    if let Some(configured) = configured {
        for (name, entry) in configured {
            if name == "defaults" || saved.contains_key(name) {
                continue;
            }
            state::check_name(name)?;
            let entry = entry
                .as_object()
                .ok_or_else(|| bad("an agent config isn't an object"))?;
            let cwd = match entry.get("cwd") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(PathBuf::from(s)),
                Some(_) => return Err(bad("an agent cwd must be a string")),
            };
            let worktree = match entry.get("worktree") {
                None | Some(Value::Null) => {
                    cwd.is_none() && entry.get("role").and_then(Value::as_str) == Some("worker")
                }
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(bad("an agent worktree must be a boolean")),
            };
            if worktree && cwd.is_some() {
                return Err(bad("an agent config takes cwd or worktree, not both"));
            }
            let location = match (cwd, worktree, main) {
                (Some(cwd), _, _) => location_top(&cwd)?,
                (None, true, Some(main)) => exact_worktree(&main.join(names.worktree_for(name)))?,
                _ => None,
            };
            if location.as_deref() == Some(top) {
                matches.insert(name.clone());
            }
        }
    }
    // Decode only the exact configured pattern, round-trip through the
    // launcher's filename encoder, and compare canonical parents. This is
    // not a basename default: wt-alice maps to alice, codex%3Apr1 to codex:pr1.
    // A name with saved state is launcher-managed: undefined, it's retired.
    if let Some(main) = main
        && let Some(name) = pattern_actor(top, main, &names)?
        && !saved.contains_key(&name)
        && configured.is_none_or(|c| !c.contains_key(&name))
    {
        matches.insert(name);
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => Err(bad(
            "more than one actor maps to this worktree; repair the agent locations",
        )),
    }
}

fn pattern_actor(top: &Path, main: &Path, names: &Names) -> Result<Option<String>, RunError> {
    let pattern = Path::new(names.worktree_pattern());
    let parent = main.join(pattern.parent().unwrap_or(Path::new("")));
    let parent = match fs::canonicalize(parent) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RunError::io("can't resolve worktree pattern", e)),
    };
    if top.parent() != Some(parent.as_path()) {
        return Ok(None);
    }
    let (Some(last), Some(template)) = (
        top.file_name().and_then(|s| s.to_str()),
        pattern.file_name().and_then(|s| s.to_str()),
    ) else {
        return Ok(None);
    };
    let (prefix, suffix) = template
        .split_once(AGENT)
        .expect("Names validated the placeholder");
    let Some(encoded) = last
        .strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(suffix))
    else {
        return Ok(None);
    };
    let actor = encoded.replace("%3A", ":");
    if state::check_name(&actor).is_err() || state::file_name(&actor) != encoded {
        return Ok(None);
    }
    Ok(Some(actor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture(PathBuf);
    impl Fixture {
        fn unborn() -> Self {
            let root = std::env::temp_dir().join(format!("mdkg-actor-{}", uuid::Uuid::now_v7()));
            fs::create_dir_all(&root).expect("identity test fixture succeeds");
            fs::create_dir_all(root.join(".mdkg")).expect("identity test fixture succeeds");
            let fixture = Self(fs::canonicalize(root).expect("identity test fixture succeeds"));
            fixture.git(&["init", "-q", "-b", "main"]);
            fixture
        }
        fn new() -> Self {
            let fixture = Self::unborn();
            fixture.config(json!({}));
            fixture
        }
        fn git(&self, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(&self.0)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &self.0)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_ATTR_NOSYSTEM", "1")
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "init.templateDir=",
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(args)
                .output()
                .expect("run fixture git");
            assert!(
                output.status.success(),
                "fixture git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fn config(&self, cfg: Value) {
            fs::write(self.0.join(".mdkg/config.json"), cfg.to_string())
                .expect("identity test fixture succeeds");
            self.git(&["add", ".mdkg/config.json"]);
            self.git(&["commit", "-q", "--allow-empty", "-m", "fixture"]);
        }
        fn worktree(&self, rel: &str) -> PathBuf {
            let top = self.0.join(rel);
            fs::create_dir_all(&top).expect("identity test fixture succeeds");
            let admin = self
                .0
                .join(".git/worktrees")
                .join(uuid::Uuid::now_v7().to_string());
            fs::create_dir_all(&admin).expect("identity test fixture succeeds");
            fs::write(top.join(".git"), format!("gitdir: {}\n", admin.display()))
                .expect("identity test fixture succeeds");
            fs::write(
                admin.join("gitdir"),
                format!("{}\n", top.join(".git").display()),
            )
            .expect("identity test fixture succeeds");
            fs::write(admin.join("commondir"), "../..\n").expect("identity test fixture succeeds");
            fs::write(admin.join("HEAD"), "ref: refs/heads/work\n")
                .expect("identity test fixture succeeds");
            top
        }
        fn saved(&self, name: &str, cwd: &Path) {
            let agent: AgentState = serde_json::from_value(json!({
                "spec": {"name":name,"cwd":cwd,"harness":"codex","role":"worker"}
            }))
            .expect("identity test fixture succeeds");
            state::save(&self.0.join(".git/mdkg/run"), &agent)
                .expect("identity test fixture succeeds");
        }
        fn actor(&self, cwd: &Path) -> Result<Option<String>, RunError> {
            actor_for_cwd(&self.0, cwd)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn fixture_git_ignores_inherited_repository_and_index_overrides() {
        const CHILD: &str = "MDKG_IDENTITY_FIXTURE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let f = Fixture::new();
            f.config(json!({"orchestration":{"worktree":"lanes/{agent}"}}));
            return;
        }
        let dir = std::env::temp_dir().join(format!("mdkg-identity-env-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        let index = dir.join("external-index");
        fs::write(&index, b"untouched").unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "identity::tests::fixture_git_ignores_inherited_repository_and_index_overrides",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("GIT_DIR", dir.join("other-git"))
            .env("GIT_WORK_TREE", dir.join("other-worktree"))
            .env("GIT_INDEX_FILE", &index)
            .output()
            .unwrap();
        let after = fs::read(&index).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(after, b"untouched");
    }

    #[test]
    fn custom_pattern_and_subdirectory_resolve_the_actor_not_the_basename() {
        let f = Fixture::new();
        f.config(json!({"orchestration":{"worktree":"lanes/wt-{agent}-code"},
            "agents":{"alice":{"role":"worker"}}}));
        let top = f.worktree("lanes/wt-alice-code");
        let sub = top.join("src/deep");
        fs::create_dir_all(&sub).expect("identity test fixture succeeds");
        assert_eq!(
            f.actor(&sub).expect("identity test fixture succeeds"),
            Some("alice".into())
        );
        assert_eq!(f.actor(&f.0).expect("identity test fixture succeeds"), None);
    }

    #[test]
    fn colon_names_round_trip_and_other_percent_spellings_do_not() {
        let f = Fixture::new();
        f.config(json!({"orchestration":{"worktree":"lanes/wt-{agent}"},
            "agents":{"codex:pr19":{"role":"worker"}}}));
        let top = f.worktree("lanes/wt-codex%3Apr19");
        assert_eq!(
            f.actor(&top).expect("identity test fixture succeeds"),
            Some("codex:pr19".into())
        );
        // Separate tree: case-insensitive filesystems otherwise resolve the
        // lowercase spelling to the already existing uppercase path.
        let invalid = Fixture::new();
        invalid.config(json!({"orchestration":{"worktree":"lanes/wt-{agent}"}}));
        let other = invalid.worktree("lanes/wt-codex%3apr19");
        assert_eq!(
            invalid
                .actor(&other)
                .expect("identity test fixture succeeds"),
            None
        );
    }

    #[test]
    fn nested_worktree_never_inherits_the_containing_actors_identity() {
        let f = Fixture::new();
        f.config(json!({"agents":{"alice":{"harness":"codex"},"bob":{"harness":"codex"}}}));
        let outer = f.worktree(".worktrees/alice");
        let inner = f.worktree(".worktrees/alice/nested/bob");
        f.saved("alice", &outer);
        assert_eq!(
            f.actor(&inner).expect("identity test fixture succeeds"),
            None
        );
        f.saved("bob", &inner);
        assert_eq!(
            f.actor(&inner).expect("identity test fixture succeeds"),
            Some("bob".into())
        );
        assert_eq!(
            f.actor(&outer).expect("identity test fixture succeeds"),
            Some("alice".into())
        );
    }

    #[test]
    fn explicit_locations_and_symlinked_cwds_use_canonical_worktree_identity() {
        let f = Fixture::new();
        f.config(json!({"agents":{"codex:alice":{"harness":"codex"}}}));
        let top = f.worktree("custom/checkout");
        let sub = top.join("src");
        fs::create_dir_all(&sub).expect("identity test fixture succeeds");
        let link = f.0.join("alias");
        std::os::unix::fs::symlink(&sub, &link).expect("identity test fixture succeeds");
        f.saved("codex:alice", &link);
        assert_eq!(
            f.actor(&sub).expect("identity test fixture succeeds"),
            Some("codex:alice".into())
        );
        assert_eq!(
            f.actor(&link).expect("identity test fixture succeeds"),
            Some("codex:alice".into())
        );
    }

    #[test]
    fn conflicting_saved_or_configured_actors_are_refused() {
        let f = Fixture::new();
        let top = f.worktree("custom/checkout");
        f.saved("alice", &top);
        f.config(json!({"agents":{"alice":{"harness":"codex"},"bob":{"cwd":top}}}));
        assert!(
            f.actor(&top)
                .expect_err("identity must be refused")
                .message
                .contains("more than one")
        );
        f.config(json!({"agents":{"alice":{"harness":"codex"},"bob":{"harness":"codex"}}}));
        f.saved("bob", &top);
        assert!(
            f.actor(&top)
                .expect_err("identity must be refused")
                .message
                .contains("more than one")
        );
    }

    #[test]
    fn saved_location_overrides_config_and_does_not_revive_an_old_pattern_path() {
        let f = Fixture::new();
        let old = f.worktree(".worktrees/alice");
        let new = f.worktree("custom/new");
        f.config(json!({"agents":{"alice":{"cwd":old}}}));
        f.saved("alice", &new);
        assert_eq!(f.actor(&old).expect("identity test fixture succeeds"), None);
        assert_eq!(
            f.actor(&new).expect("identity test fixture succeeds"),
            Some("alice".into())
        );
    }

    #[test]
    fn a_replacement_checkout_cannot_reuse_a_saved_git_directory_pin() {
        let f = Fixture::new();
        f.config(json!({"agents":{"alice":{"harness":"codex"}}}));
        let top = f.worktree(".worktrees/alice");
        f.saved("alice", &top);
        let dir = f.0.join(".git/mdkg/run");
        let mut agent = state::load(&dir, "alice")
            .expect("read state")
            .expect("saved state");
        let checkout = git::check_worktree(&top).expect("checked checkout");
        agent.spec.pinned = Some(state::Pinned {
            top: top.clone(),
            git_dir: checkout.git_dir,
        });
        state::save(&dir, &agent).expect("save valid pin");
        assert_eq!(f.actor(&top).expect("valid pin"), Some("alice".into()));
        agent.spec.pinned.as_mut().expect("pin").git_dir =
            f.0.join(".git/worktrees/old-deleted-admin");
        state::save(&dir, &agent).expect("save stale pin");
        assert!(
            f.actor(&top)
                .expect_err("refuse stale pin")
                .message
                .contains("pinned worktree")
        );
    }

    #[test]
    fn a_configured_worktree_directory_cannot_assign_its_parent_checkout() {
        let f = Fixture::new();
        fs::create_dir_all(f.0.join(".worktrees/bob")).expect("ordinary directory");
        f.config(json!({"agents":{"bob":{"worktree":true}}}));
        assert_eq!(f.actor(&f.0).expect("no identity"), None);
    }

    #[test]
    fn a_configured_worktree_cannot_redirect_through_its_last_component() {
        let f = Fixture::new();
        fs::create_dir_all(f.0.join(".worktrees")).expect("worktree parent");
        std::os::unix::fs::symlink(&f.0, f.0.join(".worktrees/bob")).expect("link");
        f.config(json!({"agents":{"bob":{"worktree":true}}}));
        assert!(
            f.actor(&f.0).is_err(),
            "a configured worktree link must be refused"
        );
    }

    #[test]
    fn an_explicit_configured_cwd_can_be_inside_a_checkout() {
        let f = Fixture::new();
        let top = f.worktree("custom/checkout");
        let sub = top.join("src/deep");
        fs::create_dir_all(&sub).expect("subdirectory");
        f.config(json!({"agents":{"bob":{"cwd":sub}}}));
        assert_eq!(f.actor(&sub).expect("explicit cwd"), Some("bob".into()));
        assert_eq!(f.actor(&top).expect("same worktree"), Some("bob".into()));
    }

    #[test]
    fn configured_workers_map_before_their_first_saved_state() {
        let f = Fixture::new();
        let top = f.worktree(".worktrees/alice");
        f.config(json!({"agents":{"alice":{"role":"worker","harness":"codex"}}}));
        assert_eq!(
            f.actor(&top).expect("identity test fixture succeeds"),
            Some("alice".into())
        );
        f.config(json!({"agents":{"alice":{"cwd":"relative/to/unknown/launcher"}}}));
        assert!(f.actor(&top).is_err());
    }

    #[test]
    fn malformed_inline_locations_do_not_assign_an_unsaved_actor() {
        for (field, invalid, expected) in [
            ("cwd", json!(42), "cwd must be a string"),
            ("worktree", json!("broken"), "worktree must be a boolean"),
        ] {
            let f = Fixture::new();
            f.config(json!({"agents":{"alice":{field:invalid}}}));
            let error = f.actor(&f.0).unwrap_err();
            assert_eq!(error.code, "usage.invalid_arguments");
            assert!(error.message.contains(expected), "{error}");
            f.saved("alice", &f.0);
            let error = f.actor(&f.0).unwrap_err();
            assert!(error.message.contains(expected), "{error}");
        }
    }

    #[test]
    fn pattern_and_another_actors_saved_location_are_ambiguous() {
        let f = Fixture::new();
        f.config(json!({"agents":{"alice":{"role":"worker"},"bob":{"harness":"codex"}}}));
        let top = f.worktree(".worktrees/alice");
        f.saved("bob", &top);
        assert!(f.actor(&top).is_err());
    }

    /// R1: saved state or a pattern-shaped folder never names an actor the
    /// authority doesn't define, whether it never did or the tip removed it.
    #[test]
    fn saved_state_naming_an_undefined_actor_gives_no_actor() {
        let mut failures = Vec::new();
        for harness in ["claude", "codex", "opencode"] {
            let f = Fixture::new();
            let top = f.worktree(".worktrees/alice");
            f.saved("alice", &top);
            f.saved("bob", &f.0);
            let mut check = |state: &str, expected: Option<&str>| match f.actor(&top) {
                Ok(actor) if actor.as_deref() == expected => (),
                other => failures.push(format!("{harness}, {state}: {other:?}")),
            };
            check("never defined", None);
            f.config(json!({"agents":{"alice":{"harness":harness,"role":"worker"}}}));
            check("defined", Some("alice"));
            f.config(json!({}));
            check("removed at the tip", None);
            assert_eq!(
                f.actor(&f.0).unwrap(),
                None,
                "{harness}: saved bob is undefined"
            );
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// An interactive receiver has no definition and no saved state: the
    /// committed worktree pattern still names it (orchestrator #11198).
    #[test]
    fn an_interactive_pattern_actor_with_no_saved_state_still_resolves() {
        let f = Fixture::new();
        let top = f.worktree(".worktrees/carol");
        assert_eq!(f.actor(&top).unwrap().as_deref(), Some("carol"));
        // Once a launcher saves it, only a definition keeps it named.
        f.saved("carol", &top);
        assert_eq!(f.actor(&top).unwrap(), None);
    }

    #[test]
    fn corrupt_or_misnamed_records_never_disappear_from_identity_evidence() {
        let f = Fixture::new();
        let top = f.worktree(".worktrees/alice");
        f.saved("alice", &top);
        let dir = f.0.join(".git/mdkg/run");
        fs::write(dir.join("broken.json"), "not JSON").expect("identity test fixture succeeds");
        assert!(f.actor(&top).is_err());
        fs::remove_file(dir.join("broken.json")).expect("identity test fixture succeeds");
        fs::rename(dir.join("alice.json"), dir.join("bob.json"))
            .expect("identity test fixture succeeds");
        assert!(f.actor(&top).is_err());
    }

    #[test]
    fn nonregular_identity_records_are_refused_without_reading_them() {
        let f = Fixture::new();
        let top = f.worktree(".worktrees/alice");
        let dir = f.0.join(".git/mdkg/run");
        fs::create_dir_all(dir.join("alice.json")).expect("identity test fixture succeeds");
        assert!(f.actor(&top).is_err());
        fs::remove_dir(dir.join("alice.json")).expect("identity test fixture succeeds");
        std::os::unix::fs::symlink(f.0.join("missing"), dir.join("alice.json"))
            .expect("identity test fixture succeeds");
        assert!(f.actor(&top).is_err());
    }

    #[test]
    fn a_hook_cannot_map_a_cwd_in_another_repository() {
        let f = Fixture::new();
        let other = Fixture::new();
        let top = other.worktree(".worktrees/alice");
        assert!(
            f.actor(&top)
                .expect_err("identity must be refused")
                .message
                .contains("another repository")
        );
    }

    #[test]
    fn graph_ref_definitions_identify_each_harness_without_checkout_files() {
        let f = Fixture::new();
        let graph_id = "0192f3a0-0000-7000-8000-000000000000";
        f.config(json!({"schema_version":3,"format_version":"0.7","graph_id":graph_id}));
        let dir = f.0.join(".mdkg/agents");
        fs::create_dir_all(&dir).unwrap();
        let mut checkouts = Vec::new();
        for harness in ["claude", "codex", "opencode"] {
            let top = f.worktree(&format!("custom/{harness}"));
            fs::write(
                dir.join(format!("{harness}.json")),
                json!({"schema":"mdkg.agent.v1", "harness":harness, "role":"worker", "cwd":top})
                    .to_string(),
            )
            .unwrap();
            checkouts.push((harness, top));
        }
        f.git(&["add", ".mdkg/agents"]);
        f.git(&["commit", "-q", "-m", "definitions"]);
        f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
        f.git(&["config", "mdkg.graph.ref", "refs/heads/mdkg-graph"]);
        f.git(&[
            "config",
            &format!("mdkg.graph.{graph_id}.ref"),
            "refs/heads/mdkg-graph",
        ]);

        // A code-branch commit, dirty files and absent files are all non-authority.
        let imposter = f.worktree("custom/imposter");
        for (harness, _) in &checkouts {
            fs::write(
                dir.join(format!("{harness}.json")),
                json!({"schema":"mdkg.agent.v1", "harness":harness, "role":"worker", "cwd":imposter})
                    .to_string(),
            )
            .unwrap();
        }
        f.git(&["add", ".mdkg/agents"]);
        f.git(&["commit", "-q", "-m", "different code branch"]);
        assert_eq!(f.actor(&imposter).unwrap(), None);
        for (harness, top) in &checkouts {
            assert_eq!(f.actor(top).unwrap().as_deref(), Some(*harness));
        }
        fs::write(f.0.join(".mdkg/config.json"), "{}").unwrap();
        for (harness, _) in &checkouts {
            fs::write(dir.join(format!("{harness}.json")), "not JSON").unwrap();
        }
        for (harness, top) in &checkouts {
            assert_eq!(f.actor(top).unwrap().as_deref(), Some(*harness));
        }
        fs::remove_dir_all(f.0.join(".mdkg")).unwrap();
        for (harness, top) in &checkouts {
            assert_eq!(f.actor(top).unwrap().as_deref(), Some(*harness));
        }
    }

    #[test]
    fn an_uncommitted_checkout_graph_is_absent_from_identity() {
        let mut failures = Vec::new();
        for harness in ["claude", "codex", "opencode"] {
            let f = Fixture::unborn();
            fs::write(
                f.0.join(".mdkg/config.json"),
                json!({"agents":{"alice":{"cwd":f.0,"harness":harness}}}).to_string(),
            )
            .unwrap();
            for state in ["unborn", "committed without graph"] {
                if state != "unborn" {
                    fs::write(f.0.join("code.txt"), "code").unwrap();
                    f.git(&["add", "code.txt"]);
                    f.git(&["commit", "-q", "-m", "code only"]);
                }
                match capture_for_cwd(&f.0, &f.0) {
                    Ok(snapshot) if snapshot.actor.is_none() && snapshot.authority.is_none() => (),
                    Ok(snapshot) => failures.push(format!(
                        "{harness}, {state}: expected no actor or authority, got {snapshot:?}"
                    )),
                    Err(error) => failures.push(format!(
                        "{harness}, {state}: expected absent authority, got {error}"
                    )),
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn an_unreadable_checkout_head_is_not_absent_authority() {
        let f = Fixture::unborn();
        // A syntactically valid ref pointing to a missing object is not an
        // unborn branch even though rev-parse cannot peel it to a commit.
        fs::write(
            f.0.join(".git/refs/heads/main"),
            format!("{}\n", "1".repeat(40)),
        )
        .unwrap();
        let error = capture_for_cwd(&f.0, &f.0).unwrap_err();
        assert!(error.message.contains("HEAD"), "{error}");
    }

    #[test]
    fn identity_snapshot_keeps_the_actor_and_authority_from_one_graph_tip() {
        for harness in ["claude", "codex", "opencode"] {
            let f = Fixture::new();
            let graph_id = "0192f3a0-0000-7000-8000-000000000000";
            let config = |actor: &str| {
                json!({
                    "schema_version":3, "format_version":"0.7", "graph_id":graph_id,
                    "agents":{actor:{"cwd":f.0,"harness":harness}},
                    "orchestration":{"branch":format!("{actor}/{{agent}}")}
                })
            };
            f.config(config("alice"));
            f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
            f.git(&["config", "mdkg.graph.ref", "refs/heads/mdkg-graph"]);
            f.git(&[
                "config",
                &format!("mdkg.graph.{graph_id}.ref"),
                "refs/heads/mdkg-graph",
            ]);
            let first = capture_for_cwd(&f.0, &f.0).unwrap();
            f.config(config("bob"));
            assert_eq!(
                crate::orchestration::Names::load(&f.0)
                    .unwrap()
                    .branch_for("worker"),
                "alice/worker"
            );
            assert_eq!(
                crate::orchestration::load_config(&f.0).unwrap()["orchestration"]["branch"],
                "bob/{agent}"
            );
            f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
            let second = capture_for_cwd(&f.0, &f.0).unwrap();
            assert_eq!(
                crate::orchestration::Names::load(&f.0)
                    .unwrap()
                    .branch_for("worker"),
                "bob/worker"
            );

            assert_eq!(first.actor.as_deref(), Some("alice"));
            assert_eq!(second.actor.as_deref(), Some("bob"));
            assert_ne!(
                first.authority.as_ref().unwrap().commit,
                second.authority.as_ref().unwrap().commit
            );
            assert!(
                first.authority.unwrap().config["agents"]
                    .get("alice")
                    .is_some()
            );
        }
    }

    #[test]
    fn saved_identity_cannot_mask_unrecorded_foreign_or_missing_graph_authority() {
        let f = Fixture::new();
        let graph_id = "0192f3a0-0000-7000-8000-000000000000";
        let other_id = "0192f3a0-0000-7000-8000-000000000001";
        f.config(json!({
            "schema_version":3, "format_version":"0.7", "graph_id":graph_id,
            "agents":{"alice":{"cwd":f.0}}
        }));
        f.saved("alice", &f.0);
        f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
        f.git(&["config", "mdkg.graph.ref", "refs/heads/mdkg-graph"]);
        assert_eq!(f.actor(&f.0).unwrap_err().code, "graph.identity_unrecorded");
        f.git(&[
            "config",
            &format!("mdkg.graph.{other_id}.ref"),
            "refs/heads/mdkg-graph",
        ]);
        assert_eq!(f.actor(&f.0).unwrap_err().code, "graph.identity_mismatch");
        f.git(&["config", "--unset", &format!("mdkg.graph.{other_id}.ref")]);
        f.git(&[
            "config",
            &format!("mdkg.graph.{graph_id}.ref"),
            "refs/heads/mdkg-graph",
        ]);
        assert_eq!(f.actor(&f.0).unwrap().as_deref(), Some("alice"));
        f.git(&["update-ref", "-d", "refs/heads/mdkg-graph"]);
        assert_eq!(f.actor(&f.0).unwrap_err().code, "graph.ref_missing");
    }

    #[test]
    fn checkout_mode_uses_committed_patterns() {
        let f = Fixture::new();
        f.config(json!({"orchestration":{"worktree":"lanes/graph-{agent}"},
            "agents":{"alice":{"role":"worker"},"bob":{"role":"worker"}}}));
        let expected = f.worktree("lanes/graph-alice");
        let forged = f.worktree("lanes/local-bob");
        fs::write(
            f.0.join(".mdkg/config.json"),
            json!({"orchestration":{"worktree":"lanes/local-{agent}"}}).to_string(),
        )
        .unwrap();
        assert_eq!(f.actor(&expected).unwrap().as_deref(), Some("alice"));
        assert_eq!(f.actor(&forged).unwrap(), None);
    }

    #[test]
    fn a_nested_graph_reads_its_own_committed_identity() {
        let f = Fixture::new();
        let nested = f.0.join("nested");
        fs::create_dir_all(nested.join(".mdkg")).unwrap();
        fs::write(
            nested.join(".mdkg/config.json"),
            json!({"agents":{"nested-agent":{"cwd":f.0}}}).to_string(),
        )
        .unwrap();
        f.git(&["add", "nested/.mdkg/config.json"]);
        f.git(&["commit", "-q", "-m", "nested graph"]);
        assert_eq!(
            actor_for_cwd(&nested, &f.0).unwrap().as_deref(),
            Some("nested-agent")
        );
        assert_eq!(f.actor(&f.0).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_backslash_in_a_nested_graph_root_reports_how_to_repair_it() {
        for harness in ["claude", "codex", "opencode"] {
            let f = Fixture::new();
            let nested = f.0.join(r"nested\literal");
            fs::create_dir_all(nested.join(".mdkg")).unwrap();
            fs::write(
                nested.join(".mdkg/config.json"),
                json!({"agents":{"worker":{"cwd":f.0,"harness":harness}}}).to_string(),
            )
            .unwrap();
            f.git(&["add", r"nested\literal/.mdkg/config.json"]);
            f.git(&["commit", "-q", "-m", "nested graph"]);
            let error = actor_for_cwd(&nested, &f.0).unwrap_err();
            assert_eq!(error.code, "graph.authority_invalid", "{harness}");
            assert!(error.message.contains("backslash"), "{harness}: {error}");
            assert!(
                error
                    .hint
                    .as_deref()
                    .is_some_and(|hint| hint.contains("rename")),
                "{harness}: {error}"
            );
        }
    }
}
