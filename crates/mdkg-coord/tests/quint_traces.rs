//! Model-based tests: replay traces of `spec/lease.qnt` against the
//! coordinator and compare state after every step.
//!
//! - `spec/traces/*.itf.json` come from the correct spec. The implementation
//!   must take every step and end in the same state.
//! - `spec/traces/bug/*.itf.json` are counterexamples from deliberately
//!   buggy specs. The implementation must refuse to follow them.
//! - `spec/traces/apalache/*.itf.json` are the shortest counterexamples; they
//!   carry no action metadata, so each step is found by search.
//!
//! The spec models snapshot hashes as version counters; here version `n`
//! is the hash string `v{n}`, and a commit produces `v{n+1}`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use mdkg_coord::{
    Agent, Changes, ConflictReason, Coordinator, Error, Lease, LeaseId, Ms, Scope, Status,
};
use serde_json::Value;

fn var<'a>(state: &'a Value, name: &str) -> &'a Value {
    let suffix = format!("::{name}");
    state
        .as_object()
        .expect("state object")
        .iter()
        .find(|(k, _)| k.ends_with(&suffix) && !k.starts_with("mbt::"))
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("trace has no variable {name}"))
}

fn int(v: &Value) -> i64 {
    v["#bigint"]
        .as_str()
        .expect("#bigint")
        .parse()
        .expect("integer")
}

fn entries(v: &Value) -> impl Iterator<Item = (&Value, &Value)> {
    v["#map"]
        .as_array()
        .expect("#map")
        .iter()
        .map(|p| (&p[0], &p[1]))
}

fn pick<'a>(state: &'a Value, name: &str) -> Option<&'a Value> {
    let p = &state["mbt::nondetPicks"][name];
    (p["tag"] == "Some").then(|| &p["value"])
}

fn version(hash: &str) -> u64 {
    hash.trim_start_matches('v').parse().expect("v<n> hash")
}

#[derive(Debug, PartialEq)]
struct View {
    owner: String,
    node: String,
    base: u64,
    deadline: u64,
    status: Status,
    confirmed: bool,
    offline: bool,
    committed_at: Option<u64>,
    result: Option<u64>,
    reason: Option<ConflictReason>,
}

fn spec_view(l: &Value) -> View {
    let opt = |v: i64| u64::try_from(v).ok();
    View {
        owner: l["owner"].as_str().expect("owner").into(),
        node: l["node"].as_str().expect("node").into(),
        base: int(&l["base"]) as u64,
        deadline: int(&l["deadline"]) as u64,
        status: match l["status"]["tag"].as_str().expect("status") {
            "Active" => Status::Active,
            "Committed" => Status::Committed,
            "Released" => Status::Released,
            "Expired" => Status::Expired,
            "Conflict" => Status::Conflict,
            other => panic!("unknown status {other}"),
        },
        confirmed: l["confirmed"].as_bool().expect("confirmed"),
        offline: l["offline"].as_bool().expect("offline"),
        committed_at: opt(int(&l["committedAt"])),
        result: opt(int(&l["result"])),
        reason: match l["reason"]["tag"].as_str().expect("reason") {
            "NoReason" => None,
            "Held" => Some(ConflictReason::Held),
            "Stale" => Some(ConflictReason::Stale),
            "CasFailed" => Some(ConflictReason::CasFailed),
            other => panic!("unknown reason {other}"),
        },
    }
}

fn impl_view(l: &Lease) -> View {
    View {
        owner: l.owner.clone(),
        node: l.scope.to_string(),
        base: version(&l.base),
        deadline: l.deadline,
        status: l.status,
        confirmed: l.confirmed,
        offline: l.offline,
        committed_at: l.committed_at,
        result: l.result.as_deref().map(version),
        reason: l.reason,
    }
}

fn agent_mut<'a>(
    agents: &'a mut BTreeMap<String, Agent>,
    a: Option<&str>,
) -> Result<&'a mut Agent, String> {
    agents
        .get_mut(a.ok_or("step picked no agent")?)
        .ok_or_else(|| "unknown agent".to_string())
}

#[derive(Clone)]
struct Harness {
    coord: Coordinator,
    agents: BTreeMap<String, Agent>,
    now: Ms,
    /// spec lease id (index) -> implementation lease id
    ids: Vec<LeaseId>,
}

impl Harness {
    fn from_initial(s0: &Value, ttl: Ms) -> Self {
        let mut coord = Coordinator::new(ttl);
        for (node, _) in entries(var(s0, "nodeHash")) {
            coord
                .register(Scope::new(node.as_str().expect("node")), "v0")
                .expect("fresh scope");
        }
        let agents = entries(var(s0, "online"))
            .map(|(k, _)| {
                let name = k.as_str().expect("agent");
                (name.to_string(), Agent::new(name, &coord))
            })
            .collect();
        Harness {
            coord,
            agents,
            now: 0,
            ids: Vec::new(),
        }
    }

    fn find(&self, id: &LeaseId) -> Option<&Lease> {
        self.coord.lease(id).or_else(|| {
            self.agents
                .values()
                .flat_map(Agent::provisional)
                .find(|l| &l.id == id)
        })
    }

    /// Applies the step the spec took. Outcomes the spec also allows (a lost
    /// claim race, a failed CAS) are fine; any other refusal is divergence.
    fn apply(
        &mut self,
        action: &str,
        a: Option<&str>,
        n: Option<&str>,
        id: Option<usize>,
    ) -> Result<(), String> {
        let now = self.now;
        let ttl = self.coord.ttl();
        let Harness {
            coord, agents, ids, ..
        } = self;
        let lease = id.and_then(|i| ids.get(i).cloned());
        let lease = || {
            lease
                .clone()
                .ok_or_else(|| "unknown spec lease id".to_string())
        };
        let scope = || Scope::new(n.unwrap_or_default());
        let r: Result<(), Error> = match action {
            "tick" => {
                self.now += 1;
                return Ok(());
            }
            "disconnect" => {
                let snapshot = coord.clone();
                agent_mut(agents, a)?.disconnect(&snapshot);
                Ok(())
            }
            "reconnect" => {
                agent_mut(agents, a)?.reconnect();
                Ok(())
            }
            "claim" => match agent_mut(agents, a)?.claim(coord, &scope(), now) {
                Ok(lid) | Err(Error::Held(lid)) => {
                    ids.push(lid);
                    Ok(())
                }
                Err(e) => Err(e),
            },
            "claimOffline" => agent_mut(agents, a)?
                .claim_offline(&scope(), now, ttl)
                .map(|lid| ids.push(lid)),
            "heartbeat" => {
                let l = lease()?;
                agent_mut(agents, a)?.heartbeat(coord, &l, now).map(drop)
            }
            "release" => {
                let l = lease()?;
                agent_mut(agents, a)?.release(coord, &l, now)
            }
            "commit" => {
                let l = lease()?;
                // A lease covers one node: the commit writes that node's next version.
                let changes: Changes = coord
                    .lease(&l)
                    .map(|x| {
                        let next = format!("v{}", version(&coord.snapshot(&x.scope)) + 1);
                        (x.scope.clone(), Some(next))
                    })
                    .into_iter()
                    .collect();
                match agent_mut(agents, a)?.commit(coord, &l, &changes, None, now) {
                    Ok(_) | Err(Error::CasFailed) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            "reconcile" => {
                let l = lease()?;
                agent_mut(agents, a)?
                    .reconcile(coord, &l, now)
                    .map(|(confirmed_id, _)| {
                        // The coordinator assigns its own id; keep the spec's id mapped.
                        if let Some(i) = id {
                            ids[i] = confirmed_id;
                        }
                    })
            }
            "expire" => {
                let l = lease()?;
                coord.expire(&l, now)
            }
            other => return Err(format!("unknown action {other}")),
        };
        r.map_err(|e| format!("implementation refused: {e:?}"))
    }

    fn check(&self, s: &Value) -> Result<(), String> {
        let now = int(var(s, "now")) as u64;
        if self.now != now {
            return Err(format!("now: impl {} spec {now}", self.now));
        }
        for (node, h) in entries(var(s, "nodeHash")) {
            let node = node.as_str().expect("node");
            let have = Some(version(&self.coord.snapshot(&Scope::new(node))));
            if have != Some(int(h) as u64) {
                return Err(format!("hash of {node}: impl {have:?} spec {}", int(h)));
            }
        }
        for (agent, on) in entries(var(s, "online")) {
            let agent = agent.as_str().expect("agent");
            if self.agents[agent].online() != on.as_bool().expect("bool") {
                return Err(format!("online({agent}) differs"));
            }
        }
        let spec_leases: Vec<_> = entries(var(s, "leases")).collect();
        if spec_leases.len() != self.ids.len() {
            return Err(format!(
                "lease count: impl {} spec {}",
                self.ids.len(),
                spec_leases.len()
            ));
        }
        for (sid, sl) in spec_leases {
            let lid = &self.ids[int(sid) as usize];
            let got = self.find(lid).map(impl_view);
            let want = spec_view(sl);
            if got.as_ref() != Some(&want) {
                return Err(format!(
                    "lease {} ({lid}):\n  impl {got:?}\n  spec {want:?}",
                    int(sid)
                ));
            }
        }
        Ok(())
    }
}

/// TTL from the first lease a trace creates (constants are not in ITF).
fn ttl_of(states: &[Value]) -> Ms {
    for w in states.windows(2) {
        let before = entries(var(&w[0], "leases")).count();
        if let Some((_, l)) = entries(var(&w[1], "leases")).nth(before) {
            return (int(&l["deadline"]) - int(var(&w[0], "now"))) as Ms;
        }
    }
    1
}

struct Divergence {
    step: usize,
    action: String,
    reason: String,
}

fn replay(trace: &Value, actions: &mut BTreeMap<String, usize>) -> Result<usize, Divergence> {
    let states = trace["states"].as_array().expect("states");
    let mut h = Harness::from_initial(&states[0], ttl_of(states));
    h.check(&states[0]).map_err(|reason| Divergence {
        step: 0,
        action: "init".into(),
        reason,
    })?;
    for (i, s) in states.iter().enumerate().skip(1) {
        let action = s["mbt::actionTaken"]
            .as_str()
            .expect("actionTaken")
            .to_string();
        let a = pick(s, "a").and_then(Value::as_str);
        let n = pick(s, "n").and_then(Value::as_str);
        let id = pick(s, "id").map(|v| int(v) as usize);
        h.apply(&action, a, n, id)
            .and_then(|_| h.check(s))
            .map_err(|reason| Divergence {
                step: i,
                action: action.clone(),
                reason: format!("{reason} [a={a:?} n={n:?} id={id:?}]"),
            })?;
        *actions.entry(action).or_default() += 1;
    }
    Ok(states.len() - 1)
}

const ACTIONS: [&str; 10] = [
    "tick",
    "disconnect",
    "reconnect",
    "claim",
    "claimOffline",
    "heartbeat",
    "release",
    "expire",
    "commit",
    "reconcile",
];

fn replay_by_search(trace: &Value) -> Result<usize, Divergence> {
    let states = trace["states"].as_array().expect("states");
    let mut h = Harness::from_initial(&states[0], ttl_of(states));
    h.check(&states[0]).map_err(|reason| Divergence {
        step: 0,
        action: "init".into(),
        reason,
    })?;
    let agents: Vec<String> = h.agents.keys().cloned().collect();
    let nodes: Vec<String> = h.coord.leaves().keys().map(ToString::to_string).collect();
    for (i, s) in states.iter().enumerate().skip(1) {
        let mut found = None;
        'search: for action in ACTIONS {
            for a in &agents {
                for n in &nodes {
                    for id in 0..h.ids.len().max(1) {
                        let mut next = h.clone();
                        if next.apply(action, Some(a), Some(n), Some(id)).is_ok()
                            && next.check(s).is_ok()
                        {
                            found = Some(next);
                            break 'search;
                        }
                    }
                }
            }
        }
        h = found.ok_or_else(|| Divergence {
            step: i,
            action: "?".into(),
            reason: "no implementation step reproduces this spec state".into(),
        })?;
    }
    Ok(states.len() - 1)
}

fn traces(dir: &str) -> Vec<(PathBuf, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(dir);
    let mut out: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.expect("entry").path())
        .filter(|p| p.to_string_lossy().ends_with(".itf.json"))
        .map(|p| {
            let v = serde_json::from_str(&fs::read_to_string(&p).expect("read")).expect("json");
            (p, v)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn coordinator_follows_spec_traces() {
    let all = traces("traces");
    assert!(!all.is_empty(), "no traces in spec/traces");
    let mut actions = BTreeMap::new();
    let mut steps = 0;
    for (path, t) in &all {
        match replay(t, &mut actions) {
            Ok(n) => steps += n,
            Err(d) => panic!(
                "{}: step {} ({}): {}",
                path.display(),
                d.step,
                d.action,
                d.reason
            ),
        }
    }
    eprintln!(
        "replayed {} traces, {steps} steps; actions: {actions:?}",
        all.len()
    );
    for a in ACTIONS {
        assert!(actions.contains_key(a), "traces never exercise {a}");
    }
}

#[test]
fn coordinator_refuses_buggy_spec_counterexamples() {
    let mut all: Vec<_> = traces("traces/bug")
        .into_iter()
        .map(|t| (t, false))
        .collect();
    all.extend(traces("traces/apalache").into_iter().map(|t| (t, true)));
    assert!(!all.is_empty(), "no counterexample traces");
    for ((path, t), search) in &all {
        let last = t["states"].as_array().expect("states").len() - 1;
        let r = if *search {
            replay_by_search(t)
        } else {
            replay(t, &mut BTreeMap::new())
        };
        match r {
            Ok(_) => panic!(
                "{}: coordinator followed a buggy counterexample to the end",
                path.display()
            ),
            Err(d) => assert!(d.step <= last, "diverged after the violation"),
        }
    }
}
