//! Map a hook's worktree to its actor without a process environment or a
//! basename guess. Saved agent locations take precedence over their config;
//! the configured worktree pattern also names actors with no launcher spec.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use mdkg_coord_sqlite::git;
use serde_json::Value;

use crate::RunError;
use crate::orchestration::{AGENT, Names, load_agent_config as load_config};
use crate::state::{self, AgentState};

fn bad(message: &str) -> RunError {
    RunError::failure(crate::error::CODE_UNKNOWN_AGENT, message)
}

/// Resolve exactly one actor for `cwd` in the repository containing `root`.
/// Missing identity is `None`; conflicting or unreadable evidence is an
/// error. Callers must not turn either case into an environment/basename
/// fallback. This does not select a session or determine headless eligibility.
pub fn actor_for_cwd(root: &Path, cwd: &Path) -> Result<Option<String>, RunError> {
    let repo = git::check_worktree(root)
        .map_err(|e| RunError::io("can't check the hook's repository", e))?;
    let here =
        git::check_worktree(cwd).map_err(|e| RunError::io("can't check the hook's worktree", e))?;
    if here.common != repo.common {
        return Err(bad("the hook's cwd belongs to another repository"));
    }
    let cfg = load_config(root)?;
    let saved = saved_agents(&repo.common.join("mdkg/run"))?;
    // This is also the launcher's pattern anchor. Separate git dirs and bare
    // repositories need saved locations or explicit absolute config paths.
    let main = (repo.common.file_name().is_some_and(|s| s == ".git"))
        .then(|| repo.common.parent())
        .flatten();
    resolve(&here.top, &here.git_dir, main, &cfg, &saved)
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
    let mut matches = BTreeSet::new();
    for (name, agent) in saved {
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
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("mdkg-actor-{}", uuid::Uuid::now_v7()));
            fs::create_dir_all(root.join(".git/objects")).expect("identity test fixture succeeds");
            fs::create_dir_all(root.join(".git/refs")).expect("identity test fixture succeeds");
            fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")
                .expect("identity test fixture succeeds");
            fs::write(
                root.join(".git/config"),
                "[core]\nrepositoryformatversion = 0\nbare = false\n",
            )
            .expect("identity test fixture succeeds");
            fs::create_dir_all(root.join(".mdkg")).expect("identity test fixture succeeds");
            Self(fs::canonicalize(root).expect("identity test fixture succeeds"))
        }
        fn config(&self, cfg: Value) {
            fs::write(self.0.join(".mdkg/config.json"), cfg.to_string())
                .expect("identity test fixture succeeds");
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
    fn custom_pattern_and_subdirectory_resolve_the_actor_not_the_basename() {
        let f = Fixture::new();
        f.config(json!({"orchestration":{"worktree":"lanes/wt-{agent}-code"}}));
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
        f.config(json!({"orchestration":{"worktree":"lanes/wt-{agent}"}}));
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
        f.config(json!({"agents":{"bob":{"cwd":top}}}));
        assert!(
            f.actor(&top)
                .expect_err("identity must be refused")
                .message
                .contains("more than one")
        );
        f.config(json!({}));
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
    fn pattern_and_another_actors_saved_location_are_ambiguous() {
        let f = Fixture::new();
        let top = f.worktree(".worktrees/alice");
        f.saved("bob", &top);
        assert!(f.actor(&top).is_err());
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
}
