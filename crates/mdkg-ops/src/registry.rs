//! The operation registry: one static table, one [`OpSpec`] per operation.
//!
//! The adapters are generated from or checked against it: HTTP routes come
//! from [`OpSpec::http`], MCP's tools must each map to an entry, and every
//! CLI `--json` command is listed here or in [`NOT_OPS`], with why it isn't
//! an operation. [`check`] holds the rules every entry keeps.

mod table;

/// What an operation does to state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// No effects.
    Read,
    /// Changes state in one store transaction, with its log record.
    Write,
    /// A cursor-driven event stream (SSE over HTTP).
    Stream,
}

/// Whether an operation takes an idempotency key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyed {
    /// Locally accepted, not required (A13); every write served over HTTP
    /// requires one instead (S10).
    Accepted,
    /// Required on every interface.
    Required,
    /// Reads and streams: a key means nothing.
    Meaningless,
}

/// Who may call an operation (the edd's Authority section).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Reads, and the writes of an actor's own work.
    Any,
    /// About the system rather than a node; names the acting person.
    Operator,
    /// Only the machine's own bound launcher actor (S27).
    Launcher,
    /// Only on the coordinator's host, never over HTTP.
    Host,
}

/// Whether a `human:*` actor may call an operation over the network (S20,
/// S41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Person {
    /// Never over the network.
    None,
    /// Without a person signature: reads, acks of their own mailbox,
    /// leaving a channel.
    Allowlist,
    /// Only with a person signature from one of their enrolled keys.
    Signed,
}

/// An HTTP method. Reads are `GET`, writes are `POST` (S4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// An operation's HTTP route: A2's path template, with `{name}` segments,
/// and a write's `:verb` on its last segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub method: Method,
    pub path: &'static str,
}

/// One operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpSpec {
    /// Dotted and stable, such as `lease.claim`.
    pub name: &'static str,
    pub kind: Kind,
    /// The capability a coordinator lists when it serves this (A17).
    pub capability: &'static str,
    /// The CLI's command words (`["claim"]`, `["msg", "send"]`).
    pub cli: &'static [&'static str],
    /// MCP tool names; empty when there's no tool.
    pub mcp: &'static [&'static str],
    /// The HTTP route; `None` when it isn't served.
    pub http: Option<Route>,
    /// Schema ids of the canonical request and of the result. The request's
    /// is empty while the operation runs as its CLI command, whose
    /// arguments are the request (0.7.1).
    pub input_schema: &'static str,
    pub output_schema: &'static str,
    pub keyed: Keyed,
    pub class: Class,
    pub person: Person,
    /// Whether `plan` needs the caller's checkout.
    pub remote_plan: bool,
}

/// A long poll: a read that holds its request until it has something to
/// answer or its own wait ends, then answers once. `msg wait` is A2's
/// stream that ends at its first match, served as one response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongPoll {
    /// The argument that gives the wait, in seconds, as the CLI's flag.
    pub arg: &'static str,
    /// The wait when the request gives none, and the longest it may ask
    /// for: the command's own.
    pub default_secs: u64,
    pub max_secs: u64,
}

/// Every long poll, by operation name. Beside [`OPS`] rather than an
/// [`OpSpec`] field, so the `OpSpec` literals other crates' tests build
/// keep compiling.
pub static LONG_POLLS: &[(&str, LongPoll)] = table::LONG_POLLS;

impl OpSpec {
    /// Whether this operation is a long poll, and how it waits.
    pub fn long_poll(&self) -> Option<&'static LongPoll> {
        LONG_POLLS
            .iter()
            .find(|(name, _)| *name == self.name)
            .map(|(_, poll)| poll)
    }

    /// How long a request with these arguments may wait before the
    /// command answers, for a long poll: its wait argument (a whole number
    /// of seconds, or a string of one as a query gives it), at most the
    /// longest, or the default when it gives none or none readable. The
    /// request itself takes this plus the command's own run time, so a
    /// limit on it allows more.
    pub fn wait(&self, args: &serde_json::Value) -> Option<std::time::Duration> {
        let poll = self.long_poll()?;
        let given = args.get(poll.arg).and_then(|v| match v {
            serde_json::Value::Number(n) => n.as_u64(),
            serde_json::Value::String(s) => s.trim().parse().ok(),
            _ => None,
        });
        let secs = given.map_or(poll.default_secs, |n| n.min(poll.max_secs));
        Some(std::time::Duration::from_secs(secs))
    }

    /// A create: a write that is `POST {collection}`, with no `:verb`
    /// (S4), answered with 201.
    pub fn creates(&self) -> bool {
        self.kind == Kind::Write
            && self.http.is_some_and(|r| {
                r.method == Method::Post && !r.path.rsplit('/').next().unwrap_or("").contains(':')
            })
    }
}

impl Route {
    /// The `{name}` placeholders of the path, in order: what the router's
    /// values are, one each.
    pub fn names(&self) -> impl Iterator<Item = &'static str> {
        let path: &'static str = self.path;
        path.split('/').filter_map(|seg| {
            let open = seg.find('{')?;
            let close = seg.find('}')?;
            seg.get(open + 1..close)
        })
    }

    /// A request's arguments: the JSON `body` (an object, or none), with
    /// each path value, decoded once by the router, under its placeholder's
    /// name. A body that names a path value too is refused, never merged;
    /// the message names the argument, never a value.
    pub fn bind(
        &self,
        params: Vec<String>,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        use serde_json::{Map, Value};
        let mut args = match body {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m,
            Some(_) => return Err("the request body isn't a JSON object".into()),
        };
        let names: Vec<&str> = self.names().collect();
        if names.len() != params.len() {
            return Err(format!(
                "the path has {} values, its route {}",
                params.len(),
                names.len()
            ));
        }
        for (name, value) in names.into_iter().zip(params) {
            if args.contains_key(name) {
                return Err(format!("`{name}` is in the path, so not in the body"));
            }
            args.insert(name.to_string(), Value::String(value));
        }
        Ok(Value::Object(args))
    }
}

/// Every operation: each CLI `--json` command that is one (dec-01a18dd9).
pub static OPS: &[OpSpec] = table::OPS;

/// Every capability an [`OpSpec`] may name: A17's, and those serve adds.
/// One is added when its feature ships, and never removed within v1.
pub static CAPABILITIES: &[&str] = &[
    "api.v1",
    "events",
    "events.agents",
    "msg.watch",
    "turn.output",
    "agents.status.reattach",
    "agents.stop.drain",
    "run.wrapper",
    "channels",
    "serve",
    "serve.tcp",
    "machines.enroll",
    "api.check",
    "serve.socket",
    "machines.report",
    "machine.show",
    "placement",
    "client.remote",
    "turn.provenance",
    "turn.proxy",
    "git.serve",
    "person.keys",
];

/// A CLI command that is never an operation, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotOp {
    /// Command words, as a prefix: `["store"]` covers every `store …`
    /// subcommand.
    pub cli: &'static [&'static str],
    pub reason: &'static str,
}

impl NotOp {
    pub const fn new(cli: &'static [&'static str], reason: &'static str) -> Self {
        Self { cli, reason }
    }
}

/// CLI commands that are never operations: setup and maintenance, the
/// servers' and the supervisor's own lifecycles, aliases of operations,
/// and the networked serve parked until its later design.
pub static NOT_OPS: &[NotOp] = table::NOT_OPS;

/// Why the command `words` isn't an operation, when it isn't one.
pub fn not_an_op(words: &[&str]) -> Option<&'static str> {
    NOT_OPS
        .iter()
        .find(|n| words.starts_with(n.cli))
        .map(|n| n.reason)
}

/// The operation with this name.
pub fn find(name: &str) -> Option<&'static OpSpec> {
    OPS.iter().find(|op| op.name == name)
}

/// The served operations, in table order: those with an HTTP route.
pub fn served() -> impl Iterator<Item = &'static OpSpec> {
    OPS.iter().filter(|op| op.http.is_some())
}

/// The rules every entry of `ops` keeps; each broken one, as text.
pub fn check(ops: &[OpSpec]) -> Vec<String> {
    let mut broken = Vec::new();
    let mut names = std::collections::HashSet::new();
    let mut routes = std::collections::HashSet::new();
    for op in ops {
        let n = op.name;
        if !names.insert(n) {
            broken.push(format!("{n}: the name is listed twice"));
        }
        if !dotted(n) {
            broken.push(format!("{n}: the name isn't dotted snake_case"));
        }
        if !CAPABILITIES.contains(&op.capability) {
            broken.push(format!("{n}: the capability isn't listed"));
        }
        if op.cli.is_empty() {
            broken.push(format!("{n}: no CLI command words"));
        }
        if not_an_op(op.cli).is_some() {
            broken.push(format!("{n}: its CLI command isn't an operation"));
        }
        // An MCP tool runs the CLI command (S16).
        if !op.mcp.is_empty() && op.cli.is_empty() {
            broken.push(format!("{n}: an MCP tool without a CLI command"));
        }
        match op.kind {
            Kind::Read | Kind::Stream if op.keyed != Keyed::Meaningless => {
                broken.push(format!("{n}: a read or stream takes no key"));
            }
            Kind::Write if op.keyed == Keyed::Meaningless => {
                broken.push(format!("{n}: a write takes a key"));
            }
            _ => {}
        }
        if op.person != Person::None && op.http.is_none() {
            broken.push(format!("{n}: a person reaches only a served operation"));
        }
        if op.person == Person::Allowlist && op.class != Class::Any {
            broken.push(format!(
                "{n}: only an `any` operation is open to a person without a signature"
            ));
        }
        if let Some(route) = op.http {
            if op.class == Class::Host {
                broken.push(format!("{n}: a host operation is never served"));
            }
            // `{id}` and `{lease_id}` match the same requests.
            if !routes.insert((route.method, placeholders_blank(route.path))) {
                broken.push(format!("{n}: the route is listed twice"));
            }
            if let Err(why) = template(route.path) {
                broken.push(format!("{n}: {why}"));
            }
            let verb = route
                .path
                .rsplit('/')
                .next()
                .is_some_and(|last| last.contains(':'));
            match (op.kind, route.method) {
                (Kind::Write, Method::Post) => {}
                (Kind::Write, Method::Get) => {
                    broken.push(format!("{n}: a write is a POST"));
                }
                // A long read may be `POST {path}:read` (S4).
                (Kind::Read, Method::Post) if !route.path.ends_with(":read") => {
                    broken.push(format!("{n}: a read is a GET, or a POST to `:read`"));
                }
                (Kind::Stream, Method::Post) => {
                    broken.push(format!("{n}: a stream is a GET"));
                }
                _ => {}
            }
            if verb && op.kind == Kind::Stream {
                broken.push(format!("{n}: a stream's path has no verb"));
            }
            let waits = route.path.ends_with(":wait");
            if waits != op.long_poll().is_some() {
                broken.push(format!("{n}: a long poll, and only one, is a `:wait`"));
            }
        }
        if let Some(poll) = op.long_poll() {
            if op.kind != Kind::Read || op.http.is_none_or(|r| r.method != Method::Get) {
                broken.push(format!("{n}: a long poll is a served GET read"));
            }
            if poll.max_secs == 0 || poll.default_secs > poll.max_secs {
                broken.push(format!(
                    "{n}: a long poll's default wait is within its longest"
                ));
            }
        }
    }
    broken
}

/// The path with every `{name}` as `{}`.
fn placeholders_blank(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut inside = false;
    for c in path.chars() {
        match c {
            '{' => {
                inside = true;
                out.push('{');
            }
            '}' => {
                inside = false;
                out.push('}');
            }
            _ if inside => {}
            c => out.push(c),
        }
    }
    out
}

fn dotted(name: &str) -> bool {
    let mut parts = name.split('.');
    let snake = |p: &str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    parts.next().is_some_and(snake) && {
        let rest: Vec<&str> = parts.collect();
        !rest.is_empty() && rest.into_iter().all(snake)
    }
}

/// A2's path rules: under `/v1`, no empty segments, `{name}` placeholders
/// as whole segments, and a `:verb` only on the last segment.
fn template(path: &str) -> Result<(), String> {
    let Some(rest) = path.strip_prefix("/v1") else {
        return Err("the path isn't under /v1".into());
    };
    if rest.is_empty() {
        return Ok(());
    }
    let Some(rest) = rest.strip_prefix('/') else {
        return Err("the path isn't under /v1".into());
    };
    let segments: Vec<&str> = rest.split('/').collect();
    let last = segments.len() - 1;
    for (i, seg) in segments.iter().enumerate() {
        let (body, verb) = match seg.split_once(':') {
            Some((b, v)) if i == last => (b, Some(v)),
            Some(_) => return Err("a `:verb` only ends the path".into()),
            None => (*seg, None),
        };
        let placeholder = body
            .strip_prefix('{')
            .and_then(|b| b.strip_suffix('}'))
            .is_some_and(|b| {
                !b.is_empty() && b.chars().all(|c| c.is_ascii_lowercase() || c == '_')
            });
        let literal = !body.is_empty()
            && body
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !placeholder && !literal {
            return Err(format!(
                "the path segment `{seg}` isn't a name or a placeholder"
            ));
        }
        if let Some(v) = verb
            && (v.is_empty() || !v.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        {
            return Err(format!("the verb in `{seg}` isn't a name"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn op(name: &'static str, kind: Kind, keyed: Keyed, http: Option<Route>) -> OpSpec {
        OpSpec {
            name,
            kind,
            capability: "api.v1",
            cli: &["claim"],
            mcp: &[],
            http,
            input_schema: "mdkg.api.lease.claim.request.v1",
            output_schema: "mdkg.api.lease.claim.v1",
            keyed,
            class: Class::Any,
            person: Person::None,
            remote_plan: true,
        }
    }

    const fn post(path: &'static str) -> Option<Route> {
        Some(Route {
            method: Method::Post,
            path,
        })
    }

    const fn get(path: &'static str) -> Option<Route> {
        Some(Route {
            method: Method::Get,
            path,
        })
    }

    #[test]
    fn the_registry_keeps_its_rules() {
        assert_eq!(check(OPS), Vec::<String>::new());
        for op in OPS {
            assert_eq!(find(op.name), Some(op));
        }
        for (name, _) in LONG_POLLS {
            assert!(find(name).is_some(), "{name} isn't an operation");
        }
    }

    #[test]
    fn a_long_poll_waits_as_its_request_asks() {
        use serde_json::json;
        use std::time::Duration;
        let wait = find("msg.wait").expect("msg.wait");
        assert_eq!(
            wait.http,
            get("/v1/mailboxes/{mailbox}:wait"),
            "a GET that answers once"
        );
        let secs = |args| wait.wait(&args).map(|d: Duration| d.as_secs());
        assert_eq!(secs(json!({})), Some(600));
        assert_eq!(secs(json!({"timeout": 30})), Some(30));
        assert_eq!(secs(json!({"timeout": "45"})), Some(45));
        assert_eq!(secs(json!({"timeout": 86_400})), Some(600));
        assert_eq!(secs(json!({"timeout": "soon"})), Some(600));
        assert_eq!(secs(json!({"timeout": -5})), Some(600));
        let inbox = find("msg.inbox").expect("msg.inbox");
        assert_eq!(inbox.long_poll(), None);
        assert_eq!(inbox.wait(&json!({"timeout": 30})), None);
    }

    #[test]
    fn good_entries_pass() {
        let mut list = op(
            "lease.list",
            Kind::Read,
            Keyed::Meaningless,
            get("/v1/leases"),
        );
        list.person = Person::Allowlist;
        let mut revoke = op(
            "machine.revoke",
            Kind::Write,
            Keyed::Accepted,
            post("/v1/machines/{machine_id}:revoke"),
        );
        revoke.class = Class::Operator;
        revoke.person = Person::Signed;
        let ops = [
            list,
            revoke,
            op(
                "lease.claim",
                Kind::Write,
                Keyed::Accepted,
                post("/v1/leases"),
            ),
            op(
                "lease.renew",
                Kind::Write,
                Keyed::Accepted,
                post("/v1/leases/{lease_id}:renew"),
            ),
            op(
                "lease.search",
                Kind::Read,
                Keyed::Meaningless,
                post("/v1/leases:read"),
            ),
            op(
                "events.read",
                Kind::Stream,
                Keyed::Meaningless,
                get("/v1/events"),
            ),
            op("pack.build", Kind::Read, Keyed::Meaningless, None),
        ];
        assert_eq!(check(&ops), Vec::<String>::new());
    }

    #[test]
    fn broken_entries_are_named() {
        let mut host = op(
            "store.restore",
            Kind::Write,
            Keyed::Accepted,
            post("/v1/store"),
        );
        host.class = Class::Host;
        let mut human = op("lease.claim", Kind::Write, Keyed::Accepted, None);
        human.person = Person::Signed;
        let mut unsigned = op(
            "lease.claim",
            Kind::Write,
            Keyed::Accepted,
            post("/v1/leases"),
        );
        unsigned.person = Person::Allowlist;
        unsigned.class = Class::Operator;
        let mut local = op("store.restore", Kind::Write, Keyed::Accepted, None);
        local.cli = &["store", "restore"];
        let mut capability = op("lease.claim", Kind::Write, Keyed::Accepted, None);
        capability.capability = "leases";
        let cases = [
            (op("Lease", Kind::Read, Keyed::Meaningless, None), "dotted"),
            (
                op("lease.list", Kind::Read, Keyed::Accepted, None),
                "takes no key",
            ),
            (
                op("lease.claim", Kind::Write, Keyed::Meaningless, None),
                "takes a key",
            ),
            (
                op(
                    "lease.claim",
                    Kind::Write,
                    Keyed::Accepted,
                    get("/v1/leases"),
                ),
                "is a POST",
            ),
            (
                op(
                    "lease.list",
                    Kind::Read,
                    Keyed::Meaningless,
                    post("/v1/leases"),
                ),
                "`:read`",
            ),
            (
                op(
                    "events.read",
                    Kind::Stream,
                    Keyed::Meaningless,
                    post("/v1/events"),
                ),
                "is a GET",
            ),
            (
                op("lease.claim", Kind::Write, Keyed::Accepted, post("/leases")),
                "under /v1",
            ),
            (
                op(
                    "lease.claim",
                    Kind::Write,
                    Keyed::Accepted,
                    post("/v1/a:b/c"),
                ),
                "only ends",
            ),
            (
                op("lease.claim", Kind::Write, Keyed::Accepted, post("/v1//c")),
                "segment",
            ),
            (
                op("lease.claim", Kind::Write, Keyed::Accepted, post("/v1/{X}")),
                "segment",
            ),
            (
                op("lease.claim", Kind::Write, Keyed::Accepted, post("/v1/a:")),
                "verb",
            ),
            (host, "never served"),
            (human, "only a served operation"),
            (unsigned, "without a signature"),
            (capability, "capability"),
            (local, "isn't an operation"),
        ];
        for (entry, want) in cases {
            let broken = check(&[entry]);
            assert!(
                broken.iter().any(|b| b.contains(want)),
                "{}: wanted `{want}`, got {broken:?}",
                entry.name
            );
        }
        // `msg.wait` is the registry's long poll; any other name isn't.
        let not_a_poll = op(
            "lease.list",
            Kind::Read,
            Keyed::Meaningless,
            get("/v1/leases:wait"),
        );
        let posted = op(
            "msg.wait",
            Kind::Write,
            Keyed::Accepted,
            post("/v1/mailboxes/{mailbox}:wait"),
        );
        for entry in [not_a_poll, posted] {
            let broken = check(&[entry]);
            assert!(
                broken.iter().any(|b| b.contains("long poll")),
                "{}: {broken:?}",
                entry.name
            );
        }
        let twice = op(
            "lease.list",
            Kind::Read,
            Keyed::Meaningless,
            get("/v1/leases"),
        );
        let broken = check(&[twice, twice]);
        assert!(
            broken.iter().any(|b| b.contains("listed twice")),
            "{broken:?}"
        );
        // Routes that differ only in their placeholders' names collide.
        let a = op(
            "lease.show",
            Kind::Read,
            Keyed::Meaningless,
            get("/v1/leases/{id}"),
        );
        let b = op(
            "lease.get",
            Kind::Read,
            Keyed::Meaningless,
            get("/v1/leases/{lease_id}"),
        );
        let broken = check(&[a, b]);
        assert!(
            broken.iter().any(|b| b.contains("route is listed twice")),
            "{broken:?}"
        );
    }

    #[test]
    fn every_output_schema_has_its_file() {
        let schemas = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas");
        let mut missing = Vec::new();
        for op in OPS {
            let id = op.output_schema;
            assert!(
                id.starts_with("mdkg.") && id.ends_with(".v1"),
                "{}: {id}",
                op.name
            );
            if !schemas.join(format!("{id}.json")).is_file() {
                missing.push(id);
            }
        }
        assert_eq!(missing, Vec::<&str>::new(), "schemas/ lacks these");
    }

    #[test]
    fn served_operations_keep_the_0_7_1_rules() {
        for op in OPS {
            // An entry's CLI words name its schema, as the CLI's do.
            assert_eq!(
                op.output_schema,
                format!("mdkg.{}.v1", op.cli.join(".")),
                "{}",
                op.name
            );
            if op.http.is_some() {
                // Operator, decision, person-only, launcher and host
                // operations aren't served over HTTP (dec-01a18dd9).
                assert_eq!(op.class, Class::Any, "{} is served", op.name);
            }
        }
        // Every served operation's placeholders name its arguments; the
        // graph is the served one.
        let show = find("node.show").and_then(|op| op.http).expect("routed");
        assert_eq!(show.names().collect::<Vec<_>>(), ["graph_id", "id"]);
        let validate = find("graph.validate")
            .and_then(|op| op.http)
            .expect("routed");
        assert_eq!(validate.names().collect::<Vec<_>>(), ["graph_id"]);
        assert_eq!(
            find("lease.claim")
                .and_then(|op| op.http)
                .map(|r| r.names().count()),
            Some(0)
        );
    }

    #[test]
    fn a_route_binds_its_path_values_into_the_body() {
        use serde_json::json;
        let route = Route {
            method: Method::Get,
            path: "/v1/graphs/{graph_id}/nodes/{node}/entries/{entry}",
        };
        let params = || vec!["g1".to_string(), "task-1".to_string(), "F3".to_string()];
        assert_eq!(
            route.bind(params(), Some(json!({"kind": "finding"}))),
            Ok(json!({"graph_id": "g1", "node": "task-1", "entry": "F3", "kind": "finding"}))
        );
        assert_eq!(
            route.bind(params(), None),
            Ok(json!({"graph_id": "g1", "node": "task-1", "entry": "F3"}))
        );
        let twice = route
            .bind(params(), Some(json!({"node": "task-2"})))
            .expect_err("refused");
        assert!(
            twice.contains("`node`") && !twice.contains("task-2"),
            "{twice}"
        );
        assert!(route.bind(params(), Some(json!([1]))).is_err());
        assert!(route.bind(vec![], None).is_err());
    }

    #[test]
    fn not_ops_match_command_word_prefixes() {
        assert!(not_an_op(&["store", "restore"]).is_some());
        assert!(not_an_op(&["serve"]).is_some());
        assert!(not_an_op(&["agents", "up"]).is_some());
        assert!(not_an_op(&["agents", "status"]).is_none());
        assert!(not_an_op(&["claim"]).is_none());
        assert!(not_an_op(&[]).is_none());
    }
}
