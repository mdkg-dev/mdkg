//! The turn policy (docs/agents.md): when `mdkg agents up` wakes an
//! agent, and what bounds its turns. It lives in `.mdkg/config.json`, as
//! defaults under `agents.defaults` and per agent under
//! `agents.<name>.policy`, field by field over the 0.7.0 defaults:
//!
//! ```json
//! {"agents": {
//!   "defaults": {"loop_guard": {"turns": 10, "window_secs": 3600}},
//!   "reviewer": {"harness": "codex", "role": "reviewer",
//!                "policy": {"timeout": 900}}
//! }}
//! ```
//!
//! | Field | 0.7.0 | Later (accepted now, refused until built) |
//! |---|---|---|
//! | `trigger` | `mail`, or `manual` (a named `agents up --once` wakes it) | `schedule`, `claim`, `file-change` |
//! | `shape` | `batch`: one run shows the unacked mail, up to `max` (20), and replies once | `until-done`, `per-message` |
//! | `session` | `resume` (`--fresh` starts over) | `fresh-each-turn`, `resume-until` |
//! | `loop_guard` | on: 10 automatic turns per thread per hour, then the thread pauses until a person runs the agent or the hour cools down; `false` turns it off | per-thread budgets |
//! | `concurrency` | unset: no cap; a number caps this supervisor's turns at once | per-role, per-harness caps |
//! | `timeout` | unset: none; seconds per turn | per role |
//! | `retry` | `none`: the mail stays unacked, the next trigger retries | `backoff`, `escalate` |
//! | `budget` | unset: none | tokens or cost per turn or day |
//!
//! Every field takes a string (its kind) or an object with a `kind` and
//! that kind's settings, so a later option adds settings without a new
//! field. A kind no release knows is refused when the config is read; one
//! planned for later is read, and refused only by `agents up`.

use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::RunError;

/// The code a bad policy is refused with.
pub const CODE_POLICY: &str = "usage.invalid_arguments";

/// A kind a later release adds, read now so the config doesn't break.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Later {
    pub field: String,
    /// The kind as written (`schedule`); the field's own `kind` says
    /// `later`.
    #[serde(rename = "later_kind")]
    pub kind: String,
    /// Its settings, as written.
    #[schemars(with = "serde_json::Value")]
    pub settings: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Trigger {
    /// Mail to the agent or its rooms wakes it.
    Mail,
    /// Run explicitly with `mdkg agents up <name> --once`.
    Manual,
    Later(Later),
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Shape {
    /// One run shows the agent's unacked mail (one conversation, up to
    /// `max` messages) and replies once; mail left over wakes the next.
    Batch {
        max: usize,
    },
    Later(Later),
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Session {
    /// Each turn resumes the agent's session.
    Resume,
    Later(Later),
}

/// At most `turns` automatic turns per thread in any `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct LoopGuard {
    pub turns: usize,
    #[serde(with = "secs")]
    #[schemars(with = "u64")]
    pub window: Duration,
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Retry {
    /// A failed turn leaves its mail unacked; the next trigger retries.
    None,
    Later(Later),
}

/// Which account an agent's turns bill: its harness's subscription login
/// first, and an API key from the Keychain only when `api_key_fallback` is
/// on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, schemars::JsonSchema)]
pub struct Auth {
    /// Always `subscription` in 0.7.0.
    pub prefer: Prefer,
    /// Use the key when the harness isn't logged in, or once when the
    /// subscription hits its usage limit.
    pub api_key_fallback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Prefer {
    #[default]
    Subscription,
}

/// An agent's policy, every field resolved.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Policy {
    pub trigger: Trigger,
    pub shape: Shape,
    pub session: Session,
    /// `None` is off.
    pub loop_guard: Option<LoopGuard>,
    /// The most turns this supervisor runs at once; `None` is no cap.
    pub concurrency: Option<usize>,
    /// How long one turn may run; `None` is no limit.
    #[serde(with = "opt_secs")]
    #[schemars(with = "Option<u64>")]
    pub timeout: Option<Duration>,
    pub retry: Retry,
    /// Planned: tokens or cost. 0.7.0 has none, so any value is a later
    /// kind.
    pub budget: Option<Later>,
    /// Which account turns bill ([`crate::auth`]).
    pub auth: Auth,
    /// Later kinds of `loop_guard`, `concurrency` and `timeout`, whose
    /// 0.7.0 values have no room for one: kept here, so `agents status`
    /// shows them and an agent's own value can replace a default's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub later: Vec<Later>,
}

/// The 0.7.0 loop guard: 10 automatic turns per thread per hour.
pub const LOOP_GUARD: LoopGuard = LoopGuard {
    turns: 10,
    window: Duration::from_secs(3600),
};

/// The most messages a batch shows by default.
pub const BATCH_MAX: usize = 20;

impl Default for Policy {
    /// 0.7.0's defaults: only the loop guard is on.
    fn default() -> Self {
        Self {
            trigger: Trigger::Mail,
            shape: Shape::Batch { max: BATCH_MAX },
            session: Session::Resume,
            loop_guard: Some(LOOP_GUARD),
            concurrency: None,
            timeout: None,
            retry: Retry::None,
            budget: None,
            auth: Auth::default(),
            later: Vec::new(),
        }
    }
}

mod secs {
    use std::time::Duration;
    pub fn serialize<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }
}

mod opt_secs {
    use std::time::Duration;
    pub fn serialize<S: serde::Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_some(&d.as_secs()),
            None => s.serialize_none(),
        }
    }
}

const FIELDS: &[&str] = &[
    "auth",
    "trigger",
    "shape",
    "session",
    "loop_guard",
    "concurrency",
    "timeout",
    "retry",
    "budget",
];

/// The later kinds the design names for each field.
fn planned(field: &str) -> &'static [&'static str] {
    match field {
        "trigger" => &["schedule", "claim", "file-change"],
        "shape" => &["until-done", "per-message"],
        "session" => &["fresh-each-turn", "resume-until"],
        "loop_guard" => &["per-thread-budget"],
        "concurrency" => &["per-role", "per-harness"],
        "timeout" => &["per-role"],
        "retry" => &["backoff", "escalate"],
        "budget" => &["tokens", "cost"],
        _ => &[],
    }
}

fn bad(field: &str, what: impl std::fmt::Display) -> RunError {
    RunError::usage(CODE_POLICY, format!("policy {field}: {what}"))
        .with_hint("see the turn policy in docs/agents.md for the policy object's fields")
}

/// A field's kind, and its settings when written as an object.
type Kind<'a> = (&'a str, Option<&'a Map<String, Value>>);

/// A field's kind and settings: `"mail"` or `{"kind": "mail", …}`.
fn kind_of<'a>(field: &str, v: &'a Value) -> Result<Kind<'a>, RunError> {
    match v {
        Value::String(s) => Ok((s.as_str(), None)),
        Value::Object(o) => match o.get("kind") {
            Some(Value::String(k)) => Ok((k.as_str(), Some(o))),
            _ => Err(bad(field, "an object needs a string `kind`")),
        },
        _ => Err(bad(field, "must be a string or an object with a `kind`")),
    }
}

/// A kind this release doesn't run: a later one, or an error.
fn later(field: &str, kind: &str, v: &Value) -> Result<Later, RunError> {
    if planned(field).contains(&kind) {
        Ok(Later {
            field: field.into(),
            kind: kind.into(),
            settings: v.clone(),
        })
    } else {
        Err(bad(field, format!("unknown kind {kind:?}")))
    }
}

/// Settings a 0.7.0 kind doesn't take are refused, so a typo isn't
/// silently ignored.
fn only(field: &str, o: Option<&Map<String, Value>>, keys: &[&str]) -> Result<(), RunError> {
    if let Some(k) = o.and_then(|o| {
        o.keys()
            .find(|k| *k != "kind" && !keys.contains(&k.as_str()))
    }) {
        return Err(bad(field, format!("no setting {k:?}")));
    }
    Ok(())
}

fn count(field: &str, v: &Value) -> Result<usize, RunError> {
    v.as_u64()
        .filter(|n| *n > 0)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| bad(field, "must be a whole number above 0"))
}

impl Policy {
    /// This policy with the fields `o` sets put over it.
    pub fn apply(mut self, o: &Map<String, Value>) -> Result<Self, RunError> {
        if let Some(k) = o.keys().find(|k| !FIELDS.contains(&k.as_str())) {
            return Err(bad(k, "no such field"));
        }
        for (field, v) in o {
            let field = field.as_str();
            // This field's value replaces a later kind set before it.
            self.later.retain(|l| l.field != field);
            // null puts the 0.7.0 default back.
            if v.is_null() {
                let d = Policy::default();
                match field {
                    "trigger" => self.trigger = d.trigger,
                    "shape" => self.shape = d.shape,
                    "session" => self.session = d.session,
                    "loop_guard" => self.loop_guard = d.loop_guard,
                    "concurrency" => self.concurrency = d.concurrency,
                    "timeout" => self.timeout = d.timeout,
                    "retry" => self.retry = d.retry,
                    "auth" => self.auth = d.auth,
                    _ => self.budget = d.budget,
                }
                continue;
            }
            match field {
                "trigger" => {
                    let (kind, s) = kind_of(field, v)?;
                    self.trigger = match kind {
                        "mail" => only(field, s, &[]).map(|()| Trigger::Mail)?,
                        "manual" => only(field, s, &[]).map(|()| Trigger::Manual)?,
                        k => Trigger::Later(later(field, k, v)?),
                    };
                }
                "shape" => {
                    let (kind, s) = kind_of(field, v)?;
                    self.shape = match kind {
                        "batch" => {
                            only(field, s, &["max"])?;
                            let max = match s.and_then(|s| s.get("max")) {
                                Some(m) => count("shape.max", m)?,
                                None => BATCH_MAX,
                            };
                            Shape::Batch { max }
                        }
                        k => Shape::Later(later(field, k, v)?),
                    };
                }
                "session" => {
                    let (kind, s) = kind_of(field, v)?;
                    self.session = match kind {
                        "resume" => only(field, s, &[]).map(|()| Session::Resume)?,
                        k => Session::Later(later(field, k, v)?),
                    };
                }
                "loop_guard" => {
                    self.loop_guard = match v {
                        Value::Bool(false) => None,
                        Value::Bool(true) => Some(LOOP_GUARD),
                        Value::String(s) if s == "off" => None,
                        Value::String(s) if s == "on" => Some(LOOP_GUARD),
                        Value::Object(o) if !o.contains_key("kind") || o["kind"] == "on" => {
                            only(field, Some(o), &["turns", "window_secs"])?;
                            Some(LoopGuard {
                                turns: match o.get("turns") {
                                    Some(t) => count("loop_guard.turns", t)?,
                                    None => LOOP_GUARD.turns,
                                },
                                window: match o.get("window_secs") {
                                    Some(w) => {
                                        Duration::from_secs(
                                            count("loop_guard.window_secs", w)? as u64
                                        )
                                    }
                                    None => LOOP_GUARD.window,
                                },
                            })
                        }
                        _ => {
                            let (kind, _) = kind_of(field, v)?;
                            // Kept as the field's later kind; the guard
                            // stays as it was, and `agents up` refuses the
                            // policy.
                            self.later.push(later(field, kind, v)?);
                            continue;
                        }
                    };
                }
                "concurrency" => {
                    self.concurrency = match v {
                        Value::Number(_) => Some(count(field, v)?),
                        _ => {
                            let (kind, _) = kind_of(field, v)?;
                            self.later.push(later(field, kind, v)?);
                            continue;
                        }
                    };
                }
                "timeout" => {
                    self.timeout = match v {
                        Value::Number(_) => Some(Duration::from_secs(count(field, v)? as u64)),
                        _ => {
                            let (kind, _) = kind_of(field, v)?;
                            self.later.push(later(field, kind, v)?);
                            continue;
                        }
                    };
                }
                "auth" => {
                    let Value::Object(o) = v else {
                        return Err(bad(
                            field,
                            "must be an object: {\"prefer\": \"subscription\", \"api_key_fallback\": false}",
                        ));
                    };
                    if let Some(k) = o
                        .keys()
                        .find(|k| *k != "prefer" && *k != "api_key_fallback")
                    {
                        return Err(bad(field, format!("no setting {k:?}")));
                    }
                    match o.get("prefer") {
                        None | Some(Value::Null) => {}
                        Some(Value::String(p)) if p == "subscription" => {}
                        Some(p) => {
                            return Err(bad(
                                field,
                                format!(
                                    "prefer {p} isn't one 0.7.0 runs; it takes \"subscription\""
                                ),
                            ));
                        }
                    }
                    self.auth.prefer = Prefer::Subscription;
                    self.auth.api_key_fallback = match o.get("api_key_fallback") {
                        None | Some(Value::Null) => false,
                        Some(Value::Bool(b)) => *b,
                        Some(_) => {
                            return Err(bad(field, "api_key_fallback must be true or false"));
                        }
                    };
                }
                "retry" => {
                    let (kind, s) = kind_of(field, v)?;
                    self.retry = match kind {
                        "none" => only(field, s, &[]).map(|()| Retry::None)?,
                        k => Retry::Later(later(field, k, v)?),
                    };
                }
                _ => {
                    let (kind, _) = kind_of(field, v)?;
                    self.budget = if kind == "none" {
                        None
                    } else {
                        Some(later(field, kind, v)?)
                    };
                }
            }
        }
        Ok(self)
    }

    /// An agent's policy from a parsed `.mdkg/config.json`: the 0.7.0
    /// defaults, then `agents.defaults`, then `agents.<name>.policy`.
    pub fn for_agent(config: &Value, name: &str) -> Result<Self, RunError> {
        let agents = config.get("agents");
        let section =
            |v: Option<&Value>, what: &str| -> Result<Option<Map<String, Value>>, RunError> {
                match v {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::Object(o)) => Ok(Some(o.clone())),
                    Some(_) => Err(bad(what, "must be an object")),
                }
            };
        let mut p = Policy::default();
        if let Some(d) = section(agents.and_then(|a| a.get("defaults")), "agents.defaults")? {
            p = p.apply(&d)?;
        }
        if let Some(a) = section(agents.and_then(|a| a.get(name)), &format!("agents.{name}"))?
            && let Some(own) = section(a.get("policy"), &format!("agents.{name}.policy"))?
        {
            p = p.apply(&own)?;
        }
        Ok(p)
    }

    /// The later kinds this policy names, which `agents up` can't run yet.
    pub fn unsupported(&self) -> Vec<&Later> {
        let mut out = Vec::new();
        if let Trigger::Later(l) = &self.trigger {
            out.push(l);
        }
        if let Shape::Later(l) = &self.shape {
            out.push(l);
        }
        if let Session::Later(l) = &self.session {
            out.push(l);
        }
        if let Retry::Later(l) = &self.retry {
            out.push(l);
        }
        if let Some(l) = &self.budget {
            out.push(l);
        }
        out.extend(&self.later);
        out
    }

    /// Refuses a policy naming a later kind, naming each.
    pub fn check_supported(&self, agent: &str) -> Result<(), RunError> {
        let later = self.unsupported();
        if later.is_empty() {
            return Ok(());
        }
        let names: Vec<String> = later
            .iter()
            .map(|l| format!("{} {}", l.field, l.kind))
            .collect();
        Err(RunError::usage(
            crate::error::CODE_UNSUPPORTED,
            format!(
                "{agent}'s policy names {}, which comes in a later release",
                names.join(", ")
            ),
        )
        .with_hint("remove it from .mdkg/config.json, or use a kind 0.7.0 runs (the turn policy in docs/agents.md)"))
    }

    /// The most messages one turn shows.
    pub fn batch_max(&self) -> usize {
        match self.shape {
            Shape::Batch { max } => max,
            Shape::Later(_) => BATCH_MAX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_0_7_0_defaults_have_only_the_loop_guard_on() {
        let p = Policy::for_agent(&json!({}), "a").expect("defaults");
        assert_eq!(p, Policy::default());
        assert_eq!(p.trigger, Trigger::Mail);
        assert_eq!(p.batch_max(), 20);
        assert_eq!(p.session, Session::Resume);
        assert_eq!(
            p.loop_guard,
            Some(LoopGuard {
                turns: 10,
                window: Duration::from_secs(3600)
            })
        );
        assert_eq!(
            (p.concurrency, p.timeout, &p.retry, &p.budget),
            (None, None, &Retry::None, &None)
        );
        assert!(p.check_supported("a").is_ok());
    }

    #[test]
    fn an_agents_policy_goes_over_the_defaults_field_by_field() {
        let cfg = json!({"agents": {
            "defaults": {"timeout": 600, "loop_guard": {"turns": 3}},
            "rev": {"harness": "codex", "policy": {"trigger": "manual", "loop_guard": null}},
        }});
        let rev = Policy::for_agent(&cfg, "rev").expect("rev");
        assert_eq!(rev.trigger, Trigger::Manual);
        assert_eq!(rev.timeout, Some(Duration::from_secs(600)));
        assert_eq!(
            rev.loop_guard,
            Some(LOOP_GUARD),
            "null is the default again"
        );
        let other = Policy::for_agent(&cfg, "other").expect("other");
        assert_eq!(other.loop_guard.map(|g| g.turns), Some(3));
        assert_eq!(
            other.loop_guard.map(|g| g.window),
            Some(Duration::from_secs(3600))
        );
        let off = Policy::for_agent(&json!({"agents": {"defaults": {"loop_guard": false}}}), "a")
            .expect("off");
        assert_eq!(off.loop_guard, None);
        let objects = Policy::for_agent(
            &json!({"agents": {"defaults": {
                "trigger": {"kind": "mail"}, "shape": {"kind": "batch", "max": 5},
                "concurrency": 2, "retry": "none", "budget": "none",
            }}}),
            "a",
        )
        .expect("objects");
        assert_eq!(objects.batch_max(), 5);
        assert_eq!(objects.concurrency, Some(2));
    }

    #[test]
    fn later_kinds_are_read_now_and_refused_only_when_run() {
        let cfg = json!({"agents": {"defaults": {
            "trigger": {"kind": "schedule", "cron": "0 * * * *"},
            "shape": "until-done",
            "session": {"kind": "resume-until", "context_tokens": 100000},
            "retry": {"kind": "backoff", "max": 3},
            "budget": {"kind": "cost", "usd_per_day": 5},
        }}});
        let p = Policy::for_agent(&cfg, "a").expect("read");
        let later: Vec<String> = p
            .unsupported()
            .iter()
            .map(|l| format!("{} {}", l.field, l.kind))
            .collect();
        assert_eq!(
            later,
            [
                "trigger schedule",
                "shape until-done",
                "session resume-until",
                "retry backoff",
                "budget cost"
            ]
        );
        // Fields whose 0.7.0 value has no room for a later kind keep it
        // too, and an agent's own value replaces a default's.
        let cfg = json!({"agents": {
            "defaults": {"concurrency": {"kind": "per-role", "worker": 2},
                         "loop_guard": {"kind": "per-thread-budget"}},
            "b": {"policy": {"concurrency": 3}},
        }});
        let a = Policy::for_agent(&cfg, "a").expect("read");
        let later: Vec<&str> = a.unsupported().iter().map(|l| l.field.as_str()).collect();
        assert_eq!(later, ["concurrency", "loop_guard"]);
        assert_eq!(a.loop_guard, Some(LOOP_GUARD), "the guard stays on");
        let b = Policy::for_agent(&cfg, "b").expect("read");
        assert_eq!(b.concurrency, Some(3));
        assert_eq!(b.unsupported().len(), 1, "only the guard's later kind");
        let e = p.check_supported("a").expect_err("later");
        assert_eq!(e.code, crate::error::CODE_UNSUPPORTED);
        assert!(e.message.contains("trigger schedule"), "{}", e.message);
    }

    #[test]
    fn auth_is_subscription_only_unless_an_agent_turns_the_fallback_on() {
        assert!(!Policy::default().auth.api_key_fallback);
        let cfg = json!({"agents": {
            "defaults": {"auth": {"prefer": "subscription"}},
            "k": {"policy": {"auth": {"api_key_fallback": true}}},
        }});
        assert!(
            !Policy::for_agent(&cfg, "a")
                .expect("a")
                .auth
                .api_key_fallback
        );
        assert!(
            Policy::for_agent(&cfg, "k")
                .expect("k")
                .auth
                .api_key_fallback
        );
        for bad in [
            json!({"auth": "api-key"}),
            json!({"auth": {"prefer": "api_key"}}),
            json!({"auth": {"api_key_fallback": "yes"}}),
            json!({"auth": {"key": "sk-x"}}),
        ] {
            assert!(
                Policy::for_agent(&json!({"agents": {"defaults": bad}}), "a").is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn unknown_fields_kinds_settings_and_bad_numbers_are_refused() {
        for bad in [
            json!({"triger": "mail"}),
            json!({"trigger": "sometimes"}),
            json!({"trigger": {"kind": "mail", "when": "now"}}),
            json!({"shape": {"kind": "batch", "max": 0}}),
            json!({"timeout": -1}),
            json!({"timeout": "soon"}),
            json!({"concurrency": 1.5}),
            json!({"loop_guard": {"turns": 0}}),
            json!({"loop_guard": {"kind": "per-thread"}}),
            json!({"trigger": 3}),
        ] {
            let cfg = json!({"agents": {"defaults": bad}});
            assert!(Policy::for_agent(&cfg, "a").is_err(), "{cfg}");
        }
        assert!(Policy::for_agent(&json!({"agents": {"defaults": 3}}), "a").is_err());
    }

    #[test]
    fn loop_guard_takes_true_on_and_off_and_refuses_other_strings() {
        let guard = |v: Value| {
            Policy::for_agent(&json!({"agents": {"defaults": {"loop_guard": v}}}), "a")
                .map(|p| p.loop_guard)
        };
        assert_eq!(guard(json!(true)).expect("true"), Some(LOOP_GUARD));
        assert_eq!(guard(json!("on")).expect("on"), Some(LOOP_GUARD));
        assert_eq!(guard(json!("off")).expect("off"), None);
        assert!(guard(json!("sometimes")).is_err());
    }
}
