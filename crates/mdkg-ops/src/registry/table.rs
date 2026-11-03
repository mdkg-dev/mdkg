//! The table itself: every CLI `--json` command that is an operation, and
//! every one that isn't, with why.
//!
//! In 0.7.1 each served operation runs as one CLI subprocess (dec-01a18dd9),
//! so an entry is the CLI command as it is: its words, its output schema,
//! whether it writes. Routes follow A2's resources and S4's forms: a read
//! is `GET`, a create `POST {collection}`, any other write
//! `POST {resource}:{verb}`, and a collection operation puts its `:verb` on
//! the collection segment. `{graph_id}` names the served graph; every
//! other `{name}` is the CLI argument of that name.
//!
//! Not served over HTTP (`http: None`):
//! - operator, decision and person-only operations (class `operator`):
//!   goals, `abandon`, agent definitions, `use-key`, `resume`, `agents
//!   stop`, decision reads and the ledger's `decide` and `residual`;
//! - `agents logs`, until the stream adapter takes its `seq` positions;
//! - the launcher's `msg import`, which reads a file the launcher wrote;
//! - host operations that rewrite this checkout's derived files or read
//!   and write files by path: `format`, `node edit` and `save`, `index
//!   rebuild`, `skills add` and `sync`, `anchor`, `export`, `verify`.

use super::{Class, Keyed, Kind, LongPoll, Method, NotOp, OpSpec, Person, Route};

const fn spec(
    name: &'static str,
    kind: Kind,
    cli: &'static [&'static str],
    http: Option<Route>,
    output_schema: &'static str,
) -> OpSpec {
    OpSpec {
        name,
        kind,
        capability: "api.v1",
        cli,
        mcp: &[],
        http,
        input_schema: "",
        output_schema,
        keyed: match kind {
            Kind::Write => Keyed::Accepted,
            Kind::Read | Kind::Stream => Keyed::Meaningless,
        },
        class: Class::Any,
        person: Person::None,
        remote_plan: true,
    }
}

const fn read(
    name: &'static str,
    cli: &'static [&'static str],
    http: Option<Route>,
    output_schema: &'static str,
) -> OpSpec {
    spec(name, Kind::Read, cli, http, output_schema)
}

const fn write(
    name: &'static str,
    cli: &'static [&'static str],
    http: Option<Route>,
    output_schema: &'static str,
) -> OpSpec {
    spec(name, Kind::Write, cli, http, output_schema)
}

impl OpSpec {
    const fn tools(mut self, mcp: &'static [&'static str]) -> Self {
        self.mcp = mcp;
        self
    }

    const fn class(mut self, class: Class) -> Self {
        self.class = class;
        self
    }

    const fn capability(mut self, capability: &'static str) -> Self {
        self.capability = capability;
        self
    }

    /// Only the coordinator: no checkout needed to plan it.
    const fn coordinator(mut self) -> Self {
        self.remote_plan = false;
        self
    }
}

const fn get(path: &'static str) -> Option<Route> {
    Some(Route {
        method: Method::Get,
        path,
    })
}

const fn post(path: &'static str) -> Option<Route> {
    Some(Route {
        method: Method::Post,
        path,
    })
}

/// `msg wait --timeout`: 600 seconds by default, and at most.
pub(super) static LONG_POLLS: &[(&str, LongPoll)] = &[(
    "msg.wait",
    LongPoll {
        arg: "timeout",
        default_secs: 600,
        max_secs: 600,
    },
)];

pub(super) static OPS: &[OpSpec] = &[
    // Nodes and the graph (A4).
    read(
        "node.list",
        &["list"],
        get("/v1/graphs/{graph_id}/nodes"),
        "mdkg.list.v1",
    )
    .tools(&["list"]),
    read(
        "node.show",
        &["show"],
        get("/v1/graphs/{graph_id}/nodes/{id}"),
        "mdkg.show.v1",
    )
    .tools(&["show"]),
    read(
        "node.search",
        &["search"],
        get("/v1/graphs/{graph_id}/nodes:search"),
        "mdkg.search.v1",
    )
    .tools(&["search"]),
    read(
        "node.next",
        &["next"],
        get("/v1/graphs/{graph_id}/nodes:next"),
        "mdkg.next.v1",
    )
    .tools(&["next"]),
    write(
        "node.new",
        &["new"],
        post("/v1/graphs/{graph_id}/nodes"),
        "mdkg.new.v1",
    ),
    write(
        "node.done",
        &["done"],
        post("/v1/graphs/{graph_id}/nodes/{target}:done"),
        "mdkg.done.v1",
    ),
    write(
        "node.update",
        &["update"],
        post("/v1/graphs/{graph_id}/nodes/{target}:update"),
        "mdkg.update.v1",
    ),
    write(
        "node.retype",
        &["retype"],
        post("/v1/graphs/{graph_id}/nodes/{target}:retype"),
        "mdkg.retype.v1",
    ),
    write("node.format", &["format"], None, "mdkg.format.v1").class(Class::Host),
    // `node edit` writes an editable copy into a folder and `node save`
    // reads it back from there: files by path.
    write("node.edit", &["node", "edit"], None, "mdkg.node.edit.v1").class(Class::Host),
    write("node.save", &["node", "save"], None, "mdkg.node.save.v1").class(Class::Host),
    read(
        "graph.refs",
        &["graph", "refs"],
        get("/v1/graphs/{graph_id}/nodes/{node}/refs"),
        "mdkg.graph.refs.v1",
    ),
    read(
        "graph.backrefs",
        &["graph", "backrefs"],
        get("/v1/graphs/{graph_id}/nodes/{node}/backrefs"),
        "mdkg.graph.backrefs.v1",
    ),
    read(
        "graph.ready",
        &["graph", "ready"],
        get("/v1/graphs/{graph_id}/nodes:ready"),
        "mdkg.graph.ready.v1",
    ),
    read(
        "graph.critical_path",
        &["graph", "critical-path"],
        get("/v1/graphs/{graph_id}/goals/{goal}/critical-path"),
        "mdkg.graph.critical-path.v1",
    ),
    read(
        "graph.validate",
        &["validate"],
        get("/v1/graphs/{graph_id}:validate"),
        "mdkg.validate.v1",
    ),
    read(
        "goal.current",
        &["goal", "current"],
        get("/v1/graphs/{graph_id}/goals:current"),
        "mdkg.goal.current.v1",
    ),
    read(
        "goal.next",
        &["goal", "next"],
        get("/v1/graphs/{graph_id}/goals:next"),
        "mdkg.goal.next.v1",
    ),
    write(
        "goal.activate",
        &["goal", "activate"],
        None,
        "mdkg.goal.activate.v1",
    )
    .class(Class::Operator),
    write("goal.pause", &["goal", "pause"], None, "mdkg.goal.pause.v1").class(Class::Operator),
    write("goal.done", &["goal", "done"], None, "mdkg.goal.done.v1").class(Class::Operator),
    read(
        "pack.build",
        &["pack"],
        get("/v1/graphs/{graph_id}/pack"),
        "mdkg.pack.v1",
    )
    .tools(&["pack"]),
    read(
        "index.status",
        &["index", "status"],
        get("/v1/graphs/{graph_id}/index"),
        "mdkg.index.status.v1",
    ),
    write(
        "index.rebuild",
        &["index", "rebuild"],
        None,
        "mdkg.index.rebuild.v1",
    )
    .class(Class::Host),
    // The ledger (A27).
    // Requirements (A28).
    read(
        "req.list",
        &["req", "list"],
        get("/v1/graphs/{graph_id}/nodes/{node}/requirements"),
        "mdkg.req.list.v1",
    ),
    read(
        "entry.list",
        &["entry", "list"],
        get("/v1/graphs/{graph_id}/nodes/{node}/entries"),
        "mdkg.entry.list.v1",
    )
    .tools(&["entry_list"]),
    read(
        "entry.show",
        &["entry", "show"],
        get("/v1/graphs/{graph_id}/nodes/{node}/entries/{entry}"),
        "mdkg.entry.show.v1",
    )
    .tools(&["entry_show"]),
    read(
        "entry.gate",
        &["entry", "gate"],
        get("/v1/graphs/{graph_id}/nodes/{node}/gate"),
        "mdkg.entry.gate.v1",
    ),
    write(
        "entry.update",
        &["entry", "update"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:update"),
        "mdkg.entry.update.v1",
    )
    .tools(&["entry_update"]),
    write(
        "entry.add",
        &["entry", "add"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:add"),
        "mdkg.entry.add.v1",
    ),
    write(
        "entry.ask",
        &["entry", "ask"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:ask"),
        "mdkg.entry.ask.v1",
    ),
    write(
        "entry.answer",
        &["entry", "answer"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:answer"),
        "mdkg.entry.answer.v1",
    ),
    write(
        "entry.decide",
        &["entry", "decide"],
        None,
        "mdkg.entry.decide.v1",
    )
    .class(Class::Operator),
    write(
        "entry.propose",
        &["entry", "propose"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:propose"),
        "mdkg.entry.propose.v1",
    ),
    write(
        "entry.complete",
        &["entry", "complete"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:complete"),
        "mdkg.entry.complete.v1",
    ),
    write(
        "entry.fix",
        &["entry", "fix"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:fix"),
        "mdkg.entry.fix.v1",
    ),
    write(
        "entry.verify",
        &["entry", "verify"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:verify"),
        "mdkg.entry.verify.v1",
    ),
    write(
        "entry.reopen",
        &["entry", "reopen"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:reopen"),
        "mdkg.entry.reopen.v1",
    ),
    write(
        "entry.reject",
        &["entry", "reject"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:reject"),
        "mdkg.entry.reject.v1",
    ),
    write(
        "entry.dispute",
        &["entry", "dispute"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:dispute"),
        "mdkg.entry.dispute.v1",
    ),
    write(
        "entry.residual",
        &["entry", "residual"],
        None,
        "mdkg.entry.residual.v1",
    )
    .class(Class::Operator),
    write(
        "entry.duplicate",
        &["entry", "duplicate"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:duplicate"),
        "mdkg.entry.duplicate.v1",
    ),
    write(
        "entry.outdated",
        &["entry", "outdated"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:outdated"),
        "mdkg.entry.outdated.v1",
    ),
    write(
        "entry.withdraw",
        &["entry", "withdraw"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:withdraw"),
        "mdkg.entry.withdraw.v1",
    ),
    write(
        "entry.close",
        &["entry", "close"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:close"),
        "mdkg.entry.close.v1",
    ),
    write(
        "entry.resolve",
        &["entry", "resolve"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:resolve"),
        "mdkg.entry.resolve.v1",
    ),
    write(
        "entry.move",
        &["entry", "move"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:move"),
        "mdkg.entry.move.v1",
    ),
    write(
        "entry.req",
        &["entry", "req"],
        post("/v1/graphs/{graph_id}/nodes/{node}/entries:req"),
        "mdkg.entry.req.v1",
    ),
    // Leases and changesets (A6).
    write(
        "lease.claim",
        &["claim"],
        post("/v1/leases"),
        "mdkg.claim.v1",
    )
    .tools(&["lease_claim"]),
    read(
        "lease.holds",
        &["holds"],
        get("/v1/leases"),
        "mdkg.holds.v1",
    ),
    write(
        "lease.renew",
        &["renew"],
        post("/v1/leases:renew"),
        "mdkg.renew.v1",
    )
    .coordinator(),
    write(
        "lease.release",
        &["release"],
        post("/v1/leases:release"),
        "mdkg.release.v1",
    )
    .tools(&["lease_release"]),
    write(
        "lease.request",
        &["lease", "request"],
        post("/v1/leases:request"),
        "mdkg.lease.request.v1",
    )
    .tools(&["lease_request"]),
    read(
        "coordination.status",
        &["status"],
        get("/v1/status"),
        "mdkg.status.v1",
    ),
    write(
        "changeset.sync",
        &["sync"],
        post("/v1/changesets:sync"),
        "mdkg.sync.v1",
    ),
    write("changeset.abandon", &["abandon"], None, "mdkg.abandon.v1").class(Class::Operator),
    read(
        "scratch.path",
        &["scratch", "path"],
        get("/v1/scratch"),
        "mdkg.scratch.path.v1",
    )
    .tools(&["scratch_path"])
    .coordinator(),
    // Mail and channels (A5, A21). An inbox and a thread mark what they
    // show, so each is a write (S4). A wait is A2's stream that ends at
    // its first match, served as a long poll: a GET answered once with
    // `msg wait`'s envelope (orchestrator #11923); the mark it makes is
    // the command's own, as on the CLI and MCP.
    write(
        "msg.send",
        &["msg", "send"],
        post("/v1/messages"),
        "mdkg.msg.send.v1",
    )
    .tools(&["channel_post", "room_post"])
    .coordinator(),
    write(
        "msg.forward",
        &["msg", "forward"],
        post("/v1/messages:forward"),
        "mdkg.msg.forward.v1",
    )
    .coordinator(),
    write(
        "msg.thread",
        &["msg", "thread"],
        post("/v1/contexts:read"),
        "mdkg.msg.thread.v1",
    )
    .tools(&["channel_read", "room_read"])
    .coordinator(),
    write(
        "msg.inbox",
        &["msg", "inbox"],
        post("/v1/mailboxes/{mailbox}:read"),
        "mdkg.msg.inbox.v1",
    )
    .tools(&["inbox"])
    .coordinator(),
    read(
        "msg.wait",
        &["msg", "wait"],
        get("/v1/mailboxes/{mailbox}:wait"),
        "mdkg.msg.wait.v1",
    )
    .tools(&["inbox_wait"])
    .coordinator(),
    write(
        "msg.ack",
        &["msg", "ack"],
        post("/v1/mailboxes/{mailbox}:ack"),
        "mdkg.msg.ack.v1",
    )
    .tools(&["ack"])
    .coordinator(),
    spec(
        "msg.watch",
        Kind::Stream,
        &["msg", "watch"],
        get("/v1/events"),
        "mdkg.msg.watch.v1",
    )
    .capability("msg.watch")
    .coordinator(),
    write("msg.import", &["msg", "import"], None, "mdkg.msg.import.v1").class(Class::Launcher),
    read(
        "msg.decisions",
        &["msg", "decisions"],
        None,
        "mdkg.msg.decisions.v1",
    )
    .class(Class::Operator),
    write(
        "channel.create",
        &["channel", "create"],
        post("/v1/channels"),
        "mdkg.channel.create.v1",
    )
    .capability("channels")
    .coordinator(),
    write(
        "channel.join",
        &["channel", "join"],
        post("/v1/channels/{name}:join"),
        "mdkg.channel.join.v1",
    )
    .capability("channels")
    .coordinator(),
    write(
        "channel.leave",
        &["channel", "leave"],
        post("/v1/channels/{name}:leave"),
        "mdkg.channel.leave.v1",
    )
    .capability("channels")
    .coordinator(),
    read(
        "channel.list",
        &["channel", "list"],
        get("/v1/channels"),
        "mdkg.channel.list.v1",
    )
    .capability("channels")
    .coordinator(),
    // Agents (A7).
    read(
        "agents.status",
        &["agents", "status"],
        get("/v1/agents"),
        "mdkg.agents.status.v1",
    ),
    // A turn's output (A11, A18). Not routed yet: its events' position is
    // `seq` within the turn, and the stream adapter's SSE ids are a
    // `cursor`.
    spec(
        "agents.logs",
        Kind::Stream,
        &["agents", "logs"],
        None,
        "mdkg.agents.logs.v1",
    )
    .capability("turn.output"),
    // Stopping agents is about the system, not one's own work (A18).
    write(
        "agents.stop",
        &["agents", "stop"],
        None,
        "mdkg.agents.stop.v1",
    )
    .capability("agents.stop.drain")
    .class(Class::Operator),
    write("agents.add", &["agents", "add"], None, "mdkg.agents.add.v1").class(Class::Operator),
    write(
        "agents.edit",
        &["agents", "edit"],
        None,
        "mdkg.agents.edit.v1",
    )
    .class(Class::Operator),
    write("agents.rm", &["agents", "rm"], None, "mdkg.agents.rm.v1").class(Class::Operator),
    write(
        "agents.resume",
        &["agents", "resume"],
        None,
        "mdkg.agents.resume.v1",
    )
    .class(Class::Operator),
    write(
        "agents.use_key",
        &["agents", "use-key"],
        None,
        "mdkg.agents.use-key.v1",
    )
    .tools(&["agents_use_key"])
    .class(Class::Operator),
    // Skills, and the checkout's own tools.
    read(
        "skills.list",
        &["skills", "list"],
        get("/v1/skills"),
        "mdkg.skills.list.v1",
    ),
    read(
        "skills.show",
        &["skills", "show"],
        get("/v1/skills/{slug}"),
        "mdkg.skills.show.v1",
    ),
    read(
        "skills.validate",
        &["skills", "validate"],
        get("/v1/skills:validate"),
        "mdkg.skills.validate.v1",
    ),
    write("skills.add", &["skills", "add"], None, "mdkg.skills.add.v1").class(Class::Host),
    write(
        "skills.sync",
        &["skills", "sync"],
        None,
        "mdkg.skills.sync.v1",
    )
    .class(Class::Host),
    read(
        "affected.select",
        &["affected"],
        get("/v1/affected"),
        "mdkg.affected.v1",
    ),
    write("record.anchor", &["anchor"], None, "mdkg.anchor.v1").class(Class::Host),
    write("work.export", &["export"], None, "mdkg.export.v1").class(Class::Host),
    read("work.verify", &["verify"], None, "mdkg.verify.v1").class(Class::Host),
];

const SETUP: &str = "setup or maintenance of this checkout and its store, run where it is";

pub(super) static NOT_OPS: &[NotOp] = &[
    NotOp::new(&["init"], SETUP),
    NotOp::new(&["install"], SETUP),
    NotOp::new(&["doctor"], SETUP),
    NotOp::new(&["config"], SETUP),
    NotOp::new(&["schema"], SETUP),
    NotOp::new(&["version"], SETUP),
    NotOp::new(&["migrate"], SETUP),
    NotOp::new(&["store"], SETUP),
    NotOp::new(&["work"], SETUP),
    NotOp::new(&["auth"], "the API key helper, which prints a secret"),
    NotOp::new(&["hooks"], "what a harness's hooks run, inside its turn"),
    NotOp::new(&["mcp"], "the MCP server's own lifecycle"),
    NotOp::new(&["serve"], "the HTTP server's own lifecycle"),
    NotOp::new(
        &["agents", "up"],
        "the supervisor's lifecycle: a long-running process, not a request",
    ),
    NotOp::new(
        &["task"],
        "0.6.0 aliases of claim, update and done, served as those",
    ),
    NotOp::new(&["checkpoint"], "the 0.6.0 alias of `new checkpoint`"),
    NotOp::new(
        &["goal", "claim"],
        "the 0.6.0 alias of `goal next`, then `claim`",
    ),
    NotOp::new(&["room"], "the deprecated spelling of `channel` (A21)"),
    NotOp::new(
        &["machine"],
        "networked serve, parked unwired until the later design (dec-01a18dd9)",
    ),
    NotOp::new(
        &["person"],
        "networked serve, parked unwired until the later design (dec-01a18dd9)",
    ),
    NotOp::new(
        &["remote"],
        "networked serve, parked unwired until the later design (dec-01a18dd9)",
    ),
    NotOp::new(&["git-serve"], "sshd's forced command between machines"),
    NotOp::new(&["git-hook"], "what the hooks git-serve installs run"),
    NotOp::new(
        &["__receiver-queue-guard"],
        "an internal helper of the launcher",
    ),
];
