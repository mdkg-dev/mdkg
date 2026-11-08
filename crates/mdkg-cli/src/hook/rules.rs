//! Hook rules: what runs on a canonical event, for whom, and how strictly.
//! mdkg's own behaviours are built-in rules ([`defaults`]); config keyed by
//! rule id (a-hooks-sync) tunes or adds rules in this same format.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::event::{
    ActorKind, Decision, EventName, GitCall, GitVerb, HarnessName, HookEvent, ToolClass, Verdict,
};

/// One rule, as config writes it under `hooks.rules.<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Its key in `hooks.rules`; not written inside the entry.
    #[serde(skip)]
    pub id: String,
    /// The canonical events it runs on: one, or a list.
    #[serde(deserialize_with = "one_or_many")]
    #[schemars(with = "OneOrMany<EventName>")]
    pub on: Vec<EventName>,
    /// What else must hold, all of it; nothing means always.
    #[serde(default, rename = "match", skip_serializing_if = "Match::is_empty")]
    pub matches: Match,
    /// What runs: `builtin:<name>` in 0.7.1 (`run:` is reserved for 0.7.2).
    #[serde(rename = "use")]
    pub uses: String,
    /// The actor kinds it applies to.
    pub who: Vec<ActorKind>,
    #[serde(default)]
    pub mode: Mode,
    /// Seconds a rule may take (a `run:` rule's process, from 0.7.2).
    #[serde(default = "timeout_default")]
    pub timeout: u64,
    #[serde(default)]
    pub on_error: OnError,
}

fn timeout_default() -> u64 {
    5
}

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

fn one_or_many<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(t) => vec![t],
        OneOrMany::Many(v) => v,
    })
}

/// A rule's conditions beyond its event. All optional, combined with AND.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Match {
    /// The tool's class (`tool.before`, `tool.after`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolClass>,
    /// What the event does with git: a shell command running it, or the
    /// git adapter's own events. One verb, or a list.
    #[serde(
        default,
        deserialize_with = "some_one_or_many",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<OneOrMany<GitVerb>>")]
    pub git: Option<Vec<GitVerb>>,
    /// The branches a push updates: globs, or `"protected"` for the
    /// protected set ([`super::protect`]).
    #[serde(
        default,
        deserialize_with = "some_one_or_many",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<OneOrMany<String>>")]
    pub branch: Option<Vec<String>>,
    /// Globs over the paths an edit writes, relative to the worktree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// The harnesses it runs in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harness: Vec<HarnessName>,
}

fn some_one_or_many<'de, D, T>(d: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    one_or_many(d).map(Some)
}

impl Match {
    pub fn is_empty(&self) -> bool {
        *self == Match::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Deny or block.
    #[default]
    Enforce,
    /// A deny or block becomes context.
    Warn,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OnError {
    /// A rule that fails says nothing (the router never breaks the harness).
    #[default]
    Allow,
    /// A rule that fails refuses.
    Deny,
}

/// Built-in rules this build lists but doesn't run yet: doctor names them.
pub const NOT_BUILT: &[&str] = &[];

/// The built-in rules, by the name `builtin:<name>` gives.
#[cfg(test)]
pub const BUILTINS: &[&str] = &[
    "mail.notice",
    "mail.stop",
    "mail.wake",
    "lease.edit",
    "lease.audit",
    "lease.done",
    "lease.staged",
    "lease.committed",
    "git.protect",
    "git.graph_path",
    "git.graph_ref",
    "secret.scan",
];

fn rule(id: &str, on: &[EventName], who: &[ActorKind], matches: Match) -> Rule {
    Rule {
        id: id.into(),
        on: on.to_vec(),
        matches,
        uses: format!("builtin:{id}"),
        who: who.to_vec(),
        mode: Mode::Enforce,
        timeout: timeout_default(),
        on_error: OnError::Allow,
    }
}

/// The default profile (edd "Defaults", option C), in the order rules run
/// and their texts join.
pub fn defaults() -> Vec<Rule> {
    use ActorKind::{Agent, Human, Orchestrator, Reviewer};
    use EventName::*;
    let agents = [Agent, Orchestrator];
    let git = |verbs: &[GitVerb]| Match {
        git: Some(verbs.to_vec()),
        ..Match::default()
    };
    vec![
        rule(
            "mail.notice",
            &[SessionStart, PromptSubmit],
            &agents,
            Match::default(),
        ),
        rule("mail.stop", &[TurnStop], &agents, Match::default()),
        rule("mail.wake", &[SessionIdle], &agents, Match::default()),
        rule(
            "lease.edit",
            &[ToolBefore],
            &agents,
            Match {
                tool: Some(ToolClass::Edit),
                ..Match::default()
            },
        ),
        rule("lease.audit", &[TurnStop], &agents, Match::default()),
        rule("lease.done", &[TurnStop], &agents, Match::default()),
        // Humans get a warning where an agent is refused (the rule's own).
        rule(
            "lease.staged",
            &[GitPreCommit],
            &[Agent, Orchestrator, Human],
            Match::default(),
        ),
        // A commit gives back the path leases it settled (R2 of the 0.7.3
        // lease work): `release --committed`, as the worktree's own actor.
        rule(
            "lease.committed",
            &[GitPostCommit],
            &[Agent, Orchestrator, Reviewer, Human],
            Match::default(),
        ),
        rule(
            "git.protect",
            &[ToolBefore, GitPrePush],
            &agents,
            git(&[GitVerb::Push]),
        ),
        // Ref mode's graph files on code branches (G17); people are warned.
        rule(
            "git.graph_path",
            &[GitPreCommit, GitPreMergeCommit],
            &[Agent, Orchestrator, Reviewer, Human],
            Match::default(),
        ),
        // A direct update of the graph ref (C20); people are warned.
        rule(
            "git.graph_ref",
            &[GitReferenceTransaction],
            &[Agent, Orchestrator, Reviewer, Human],
            Match::default(),
        ),
        rule(
            "secret.scan",
            &[ToolBefore, GitPreCommit, GitPrePush],
            &[Agent, Orchestrator, Reviewer, Human],
            git(&[GitVerb::Commit, GitVerb::Push]),
        ),
    ]
}

/// The events each adapter's harness delivers: the capability matrix. A
/// rule none of whose events a harness delivers is unsupported there.
pub fn supports(harness: HarnessName, event: EventName) -> bool {
    harness
        .harness()
        .map_or_else(|| event.is_git(), |h| h.adapter().hook_supports(event))
}

/// The rules (by id) that `harness` can't run, each with the events it
/// lacks for them; never silently skipped.
/// A rule for git's own hooks is no agent harness's to run, and an agent
/// harness's rule isn't git's: those aren't listed.
pub fn unsupported(rules: &[Rule], harness: HarnessName) -> BTreeMap<String, Vec<EventName>> {
    let git_event = |e: &EventName| e.is_git();
    rules
        .iter()
        .filter(|r| r.mode != Mode::Off)
        .filter(|r| r.matches.harness.is_empty() || r.matches.harness.contains(&harness))
        .filter(|r| {
            if harness == HarnessName::Git {
                r.on.iter().any(git_event)
            } else {
                !r.on.iter().all(git_event)
            }
        })
        .filter(|r| !r.on.iter().any(|e| supports(harness, *e)))
        .map(|r| (r.id.clone(), r.on.clone()))
        .collect()
}

/// Whether a rule runs on this event: its event, harness, actor kind and
/// match, before anything opens the store. `git` is the event's git call:
/// the tool's, or the git adapter's own.
pub fn applies(rule: &Rule, event: &HookEvent, git: Option<&GitCall>) -> bool {
    if rule.mode == Mode::Off
        || !rule.on.contains(&event.event)
        || !rule.who.contains(&event.actor.kind)
    {
        return false;
    }
    let m = &rule.matches;
    if !m.harness.is_empty() && !m.harness.contains(&event.harness) {
        return false;
    }
    if let Some(class) = m.tool
        && event.tool.as_ref().map(|t| t.class) != Some(class)
    {
        return false;
    }
    if let Some(verbs) = &m.git
        && !git.is_some_and(|g| verbs.contains(&g.verb))
    {
        return false;
    }
    if !m.paths.is_empty() {
        let paths = event
            .tool
            .as_ref()
            .map(|t| t.paths.as_slice())
            .unwrap_or_default();
        if !paths.iter().any(|p| m.paths.iter().any(|g| glob(g, p))) {
            return false;
        }
    }
    true
}

/// What one rule said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    pub rule: String,
    pub verdict: Verdict,
    pub text: String,
    /// The stable code of a refusal or warning.
    pub code: Option<String>,
}

/// Rules' sayings as one decision: the most severe verdict wins
/// (deny > block_stop > context > allow), and the texts of that verdict
/// join in rule order. A `warn` rule's refusal arrives here as context.
pub fn combine(said: Vec<Said>) -> Decision {
    let Some(top) = said.iter().map(|s| s.verdict).max() else {
        return Decision::allow();
    };
    if top == Verdict::Allow {
        return Decision::allow();
    }
    let chosen: Vec<&Said> = said.iter().filter(|s| s.verdict == top).collect();
    let mut d = Decision::new(
        top,
        Some(
            chosen
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
    );
    d.rules = chosen.iter().map(|s| s.rule.clone()).collect();
    d.codes = chosen.iter().filter_map(|s| s.code.clone()).collect();
    d
}

/// A rule's verdict under its mode: `warn` turns a refusal into context.
pub fn moded(rule: &Rule, verdict: Verdict) -> Verdict {
    match (rule.mode, verdict) {
        (Mode::Warn, Verdict::Deny | Verdict::BlockStop) => Verdict::Context,
        _ => verdict,
    }
}

/// `*` and `?` within a segment, `**` across segments; a glob without a
/// `/` matches a name in any folder, as `.gitignore` reads it.
pub fn glob(pattern: &str, path: &str) -> bool {
    let split = |s: &str| -> Vec<String> {
        s.split('/')
            .filter(|x| !x.is_empty() && *x != ".")
            .map(String::from)
            .collect()
    };
    let mut pat = split(pattern);
    if !pattern.contains('/') {
        pat.insert(0, "**".into());
    }
    glob_segments(&pat, &split(path))
}

/// `glob`, matched against the whole of `name` (a branch, say): a glob
/// without a `/` never matches inside a folder.
pub fn glob_whole(pattern: &str, name: &str) -> bool {
    let split = |s: &str| -> Vec<String> {
        s.split('/')
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect()
    };
    glob_segments(&split(pattern), &split(name))
}

fn glob_segments(pat: &[String], path: &[String]) -> bool {
    match (pat.first(), path.first()) {
        (Some(g), _) if g == "**" => {
            glob_segments(&pat[1..], path) || (!path.is_empty() && glob_segments(pat, &path[1..]))
        }
        (None, None) => true,
        (Some(g), Some(n)) => {
            let g: Vec<char> = g.chars().collect();
            let n: Vec<char> = n.chars().collect();
            segment(&g, &n) && glob_segments(&pat[1..], &path[1..])
        }
        _ => false,
    }
}

fn segment(g: &[char], n: &[char]) -> bool {
    match (g.first(), n.first()) {
        (None, None) => true,
        (Some('*'), _) => segment(&g[1..], n) || (!n.is_empty() && segment(g, &n[1..])),
        (Some('?'), Some(_)) => segment(&g[1..], &n[1..]),
        (Some(a), Some(b)) if a == b => segment(&g[1..], &n[1..]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::event::{Actor, Native, Tool};
    use serde_json::json;

    fn event(
        name: EventName,
        harness: HarnessName,
        kind: ActorKind,
        tool: Option<Tool>,
    ) -> HookEvent {
        HookEvent {
            schema: crate::hook::event::EVENT_SCHEMA.into(),
            event: name,
            harness,
            session: None,
            turn: None,
            cwd: None,
            actor: Actor { id: None, kind },
            tool,
            stop_active: false,
            native: Native::default(),
        }
    }

    #[test]
    fn a_rule_reads_as_the_design_writes_it() {
        let r: Rule = serde_json::from_value(json!({
            "on": "tool.before",
            "match": {"tool": "shell", "git": "push", "branch": ["docs/*"]},
            "use": "builtin:git.protect", "who": ["agent"], "mode": "enforce"
        }))
        .expect("rule");
        assert_eq!(r.on, [EventName::ToolBefore]);
        assert_eq!(r.matches.git, Some(vec![GitVerb::Push]));
        assert_eq!(r.matches.branch, Some(vec!["docs/*".into()]));
        assert_eq!((r.timeout, r.on_error), (5, OnError::Allow));
        let many: Rule = serde_json::from_value(json!({
            "on": ["git.pre-commit", "git.pre-push"], "use": "builtin:secret.scan",
            "who": ["human"], "mode": "warn", "timeout": 2, "on_error": "deny"
        }))
        .expect("rule");
        assert_eq!(many.on.len(), 2);
        assert_eq!((many.mode, many.on_error), (Mode::Warn, OnError::Deny));
        assert!(
            serde_json::from_value::<Rule>(
                json!({"on": "tool.before", "use": "x", "who": [], "extra": 1})
            )
            .is_err()
        );
    }

    #[test]
    fn every_default_is_a_builtin_and_unique() {
        let rules = defaults();
        let ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, BUILTINS);
        for r in &rules {
            assert_eq!(r.uses, format!("builtin:{}", r.id));
            assert_eq!(r.mode, Mode::Enforce);
        }
    }

    #[test]
    fn the_capability_matrix_names_what_each_harness_lacks() {
        let rules = defaults();
        let lacks = |h| unsupported(&rules, h).into_keys().collect::<Vec<_>>();
        assert!(lacks(HarnessName::Claude).is_empty());
        assert!(lacks(HarnessName::OpenCode).is_empty());
        assert_eq!(lacks(HarnessName::Codex), ["mail.wake"]);
        assert!(lacks(HarnessName::Git).is_empty());
        // A rule tuned to an event its harness lacks is listed there; a
        // rule with a git event isn't listed for git when it has one.
        let mut mixed = defaults().remove(0);
        mixed.on = vec![EventName::SessionIdle, EventName::GitPreCommit];
        assert_eq!(
            unsupported(std::slice::from_ref(&mixed), HarnessName::Codex)
                .into_keys()
                .collect::<Vec<_>>(),
            ["mail.notice"]
        );
        assert!(unsupported(&[mixed], HarnessName::Git).is_empty());
    }

    #[test]
    fn rules_apply_by_event_actor_tool_and_git() {
        let rules = defaults();
        let by = |id: &str| rules.iter().find(|r| r.id == id).expect("rule").clone();
        let shell = |command: &str| Tool {
            class: ToolClass::Shell,
            name: "Bash".into(),
            command: Some(command.into()),
            git: crate::hook::event::git_call(command),
            ..Tool::default()
        };
        let push = event(
            EventName::ToolBefore,
            HarnessName::Claude,
            ActorKind::Agent,
            Some(shell("git push origin main")),
        );
        let git = push.tool.as_ref().and_then(|t| t.git.as_ref());
        assert!(applies(&by("git.protect"), &push, git));
        assert!(applies(&by("secret.scan"), &push, git));
        assert!(!applies(&by("lease.edit"), &push, git));
        let human = HookEvent {
            actor: Actor {
                id: None,
                kind: ActorKind::Human,
            },
            ..push.clone()
        };
        assert!(!applies(&by("git.protect"), &human, git));
        assert!(applies(&by("secret.scan"), &human, git));
        let ls = event(
            EventName::ToolBefore,
            HarnessName::Claude,
            ActorKind::Agent,
            Some(shell("ls")),
        );
        assert!(!applies(&by("git.protect"), &ls, None));
        let mut off = by("git.protect");
        off.mode = Mode::Off;
        assert!(!applies(&off, &push, git));
        let mut docs = by("lease.edit");
        docs.matches.paths = vec!["docs/**".into()];
        let edit = |p: &str| {
            event(
                EventName::ToolBefore,
                HarnessName::Codex,
                ActorKind::Agent,
                Some(Tool {
                    class: ToolClass::Edit,
                    paths: vec![p.into()],
                    ..Tool::default()
                }),
            )
        };
        assert!(applies(&docs, &edit("docs/a/b.md"), None));
        assert!(!applies(&docs, &edit("src/a.rs"), None));
        docs.matches.harness = vec![HarnessName::Claude];
        assert!(!applies(&docs, &edit("docs/a.md"), None));
    }

    #[test]
    fn the_most_severe_verdict_wins_and_texts_join_in_rule_order() {
        let said = |rule: &str, verdict, text: &str| Said {
            rule: rule.into(),
            verdict,
            text: text.into(),
            code: None,
        };
        assert_eq!(combine(vec![]).decision, Verdict::Allow);
        let d = combine(vec![
            said("mail.stop", Verdict::BlockStop, "mail"),
            said("x", Verdict::Context, "ctx"),
            said("lease.audit", Verdict::BlockStop, "audit"),
        ]);
        assert_eq!(d.decision, Verdict::BlockStop);
        assert_eq!(d.text.as_deref(), Some("mail\n\naudit"));
        assert_eq!(d.rules, ["mail.stop", "lease.audit"]);
        let deny = combine(vec![
            said("a", Verdict::BlockStop, "b"),
            said("git.protect", Verdict::Deny, "no"),
        ]);
        assert_eq!(
            (deny.decision, deny.text.as_deref()),
            (Verdict::Deny, Some("no"))
        );
        let mut warn = defaults().remove(0);
        warn.mode = Mode::Warn;
        assert_eq!(moded(&warn, Verdict::Deny), Verdict::Context);
        assert_eq!(moded(&warn, Verdict::Wake), Verdict::Wake);
    }

    #[test]
    fn globs_match_segments_and_bare_names_anywhere() {
        assert!(glob("docs/*", "docs/a"));
        assert!(!glob("docs/*", "docs/a/b"));
        assert!(glob("docs/**", "docs/a/b"));
        assert!(glob("*.md", "x/y/z.md"));
        assert!(glob("release/*", "release/1.2"));
        assert!(!glob("release/*", "release"));
        assert!(glob("ma?n", "main"));
        assert!(glob_whole("release-*", "release-1"));
        assert!(!glob_whole("release-*", "x/release-1"));
        assert!(glob_whole("main", "main") && !glob_whole("main", "mainx"));
    }
}
