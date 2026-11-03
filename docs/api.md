# mdkg API v1: a reader's guide

**Status: draft.** This guide describes the API mdkg is building towards. The
CLI's `--json` output is the contract today. The CLI seams and local
`mdkg serve` ship in 0.7.1; the networked serve design is deferred to 0.7.2.
The [local API guide](serve.md) names the operations available now; other
resource designs below remain a roadmap.

The full design is the design note *mdkg API v1: resources, URIs and the
event stream* in mdkg's development graph. Its decisions are numbered from
A1, and this guide cites them that way.

## The model in one paragraph

An mdkg **coordinator** serves one trust domain: one owner, or a small team
who trust each other. It hosts one or more **graphs** of nodes, and holds the
coordination around them: **leases** on files, nodes and turns; **mail**
between **actors** (agents and people) and **channels** that deliver to
several mailboxes; and the **agents** it runs, their **turns** and the
**supervisors** that run them, on one or more **machines**. Every change is a
record in one hash-chained log, and the API's **event stream** is that log, in
order, with a cursor to resume it. Actors have names and leave receipts; none
has a role that grants or withholds an operation.

## Resources and paths

Every resource has a path at its coordinator, beginning `/v1/`, carried in
its record's `uri` field. The same string appears in CLI output, in HTTP
requests and in events, so a client can key its cache on it (A2).

| Resource | Path |
|---|---|
| Coordinator | `/v1` |
| Graph or subgraph | `/v1/graphs/{graph_id}` |
| Node | `/v1/graphs/{graph_id}/nodes/{node_id}`, the bare id whatever its workspace; its graph ref `mdkg://{graph_id}/{node_id}` is in `ref` |
| Pack (computed) | `/v1/graphs/{graph_id}/pack?root=…&budget=…` |
| Actor, mailbox | `/v1/actors/{actor}`, `/v1/mailboxes/{mailbox}` |
| Message, thread | `/v1/messages/{message_id}`, `/v1/contexts/{context_id}` |
| Channel | `/v1/channels/{name}` |
| Lease, changeset | `/v1/leases/{lease_id}`, `/v1/changesets/{changeset_id}` |
| Agent, turn | `/v1/agents/{name}`, `/v1/agents/{name}/turns/{turn_id}` |
| Turn receipt, output | `…/turns/{turn_id}/receipt`, `…/turns/{turn_id}/output` (a stream) |
| Supervisor | `/v1/supervisors/{supervisor_id}` |
| Machine, harness, checkout | `/v1/machines/{machine_id}`, `…/harnesses/{harness}`, `…/checkouts/{checkout_id}` |
| Placement | `/v1/agents/{name}/placement` |
| Event stream | `/v1/events` |

## Responses, streams and errors

- **One-shot calls** return the envelope every mdkg command already returns:
  `{schema, ok, data, diagnostics}`. Read `ok` and the diagnostic `code`s;
  codes are stable, messages may improve (A12).
- **Streams** send one bare JSON event per line (CLI) or per SSE `data:` line
  (`mdkg serve`). A stream that has to stop sends a last
  `mdkg.stream.error` event with its diagnostics.
- **Unknown fields, event types and enum values:** ignore them. v1 grows by
  addition; anything that removes or renames is v2, served beside v1 (A1).

## Following changes: list, then watch

1. Read what you show (`agents status --json`, a channel, an inbox). The
   response carries `event_cursor`, the event stream's position at that
   moment. (A `cursor` field in `msg inbox` or `msg thread` output is a
   different thing: a mail position.)
2. Stream events after it: `mdkg msg watch --after <event_cursor> --json`
   (0.7.1), or `GET /v1/events?after=<event_cursor>` on the local API (0.7.1).
3. Save the cursor of each event you've handled. After a disconnect, stream
   again from the last one. You may see an event twice; dedupe on its `id`.
4. If the cursor is refused (`api.cursor_reset`, `api.cursor_expired`,
   `api.cursor_foreign`), drop your cache and go back to step 1 with the
   `head` the refusal returns (A10).

A cursor is opaque: store it, give it back, and never parse it. Long lists
come in pages (`limit`, then the `next` token); watch from the first page's
`event_cursor`, and keep the newest state of each `uri` (A10).

## Agents from outside (0.7.1)

| Need | Command |
|---|---|
| What runs where, and where to pick up | `mdkg agents status --json`: per agent `state`, `supervisor`, `current_turn`, `last_turn` and `follow_from`; top-level `event_cursor` |
| An agent's turns and their live output, the same shape for Claude, Codex and OpenCode | `mdkg agents logs <agent> --follow --json` (`mdkg.api.turn_output.v1` lines): follows the agent from turn to turn; `--turn <id>` follows one turn |
| Mail and channel activity, live | `mdkg msg watch --json` (`mdkg.api.event.v1` lines) |
| A graceful stop | `mdkg agents stop <agent> --drain [--wait <secs>] --json`: no new turns, the running one finishes, nothing is killed |
| Which features this build has | `mdkg version --json`, field `capabilities` (A17) |
| Which build this is | `mdkg version --json`, fields `commit` (full SHA, or `unknown`) and `dirty` (true, false, or `null` when the build couldn't tell); two builds of one `version` differ here |
| A node's ledger, and one entry with its history | `mdkg entry list <node> --json` and `mdkg entry show <node> <entry> --json` (`mdkg.entry.list.v1`, `mdkg.entry.show.v1`; `history[].seq` marks a change) |

To re-attach after a restart:
1. Read `agents status`.
2. Start `msg watch` from its `event_cursor`.
3. For each agent, start `agents logs <agent> --follow --after
   <follow_from>`, passing the agent's `follow_from` as it is.

Positions count turns, not time, so a turn that starts, or even finishes,
while you're re-attaching still arrives, an agent's first turn included.
The follow also sends `agent.state` frames, the first one as soon as it
starts (A18).

Long-lived commands (`msg watch`, `agents logs --follow`) have no timeout of
their own. Don't run them under a one-shot call's timeout; stop them with
SIGINT or SIGTERM.

## Seams for software around mdkg

mdkg exposes three generic hooks for software that adds its own rules around
it. Each is configured only in the user or machine-local config layer,
never in a committed file, and each program must be an absolute path owned
by the user.

- **Launch wrapper** (`run.wrapper`, 0.7.1): an argv mdkg puts in front of
  each turn's harness. It gets a 0600 JSON turn spec (`mdkg.run.wrapper.v1`)
  naming the paths the turn needs, and must run the harness argv after `--`
  unchanged. It may isolate, restrict or refuse (exit 125), and mdkg's
  sandbox defaults stay as they are (A19).
- **Request check** (`api.check`, 0.8): an argv mdkg runs before each write,
  with the request as JSON on stdin (`mdkg.api.check.v1`). It answers with
  an envelope; `ok: false`, a failure or a timeout refuses the write (A16).
- **Lease authority** (shipped): an argv that grants leases across machines
  (see [agents.md](agents.md) and [commands.md](commands.md)).

## `mdkg serve` (local in 0.7.1)

CLI JSON operations are available over HTTP/1.1, with server-sent events.
The default private Unix socket uses local file permissions for
authentication. Optional loopback-only TCP uses a token file and refuses
browser requests. See the [local API](serve.md) for the supported paths
and limits. Networked TLS, signed requests and machine enrolment (A15, A16)
are deferred to 0.7.2; they are not the 0.7.1 loopback transport.

## Channels

From 0.7.1, `mdkg channel …` and `--channel` name a group of mailboxes.
The 0.7.0 spellings keep working as hidden aliases that warn, until 0.8.
Message records gain a `channel` field beside the 0.7.0 one, so existing
readers keep working; the new API records have `channel` only (A21).

## What you may rely on today

The `--json` envelope of every command with a schema under
`schemas/`, its diagnostic codes and exit codes; the agent
definition and turn receipt formats; format 0.7's files, records and
bundles; the lease-authority protocol; the MCP tools. Everything under the
repository's `<git common dir>/mdkg/` other than the store itself, text
output, and harness transcripts are internal: read them through the
commands above (A1, "Machine contract").

## Draft schemas

Under `docs/api/`, all marked `"x-mdkg-status": "draft"` until the
release that first emits them:

| Schema | What |
|---|---|
| [`mdkg.api.event.v1`](api/mdkg.api.event.v1.json) | An event on the stream |
| [`mdkg.api.turn_output.v1`](api/mdkg.api.turn_output.v1.json) | A turn output event, its `data` typed per `type` in `$defs` |
| [`mdkg.api.coordinator.v1`](api/mdkg.api.coordinator.v1.json) | The coordinator |
| [`mdkg.api.graph.v1`](api/mdkg.api.graph.v1.json) | A graph or subgraph |
| [`mdkg.api.channel.v1`](api/mdkg.api.channel.v1.json) | A channel |
| [`mdkg.api.agent.v1`](api/mdkg.api.agent.v1.json) | An agent |
| [`mdkg.api.turn.v1`](api/mdkg.api.turn.v1.json) | A turn |
| [`mdkg.api.supervisor.v1`](api/mdkg.api.supervisor.v1.json) | A supervisor |
| [`mdkg.api.machine.v1`](api/mdkg.api.machine.v1.json) | A machine, its harnesses and checkouts |
| [`mdkg.api.placement.v1`](api/mdkg.api.placement.v1.json) | Where an agent runs |
| [`mdkg.agents.stop.v1`](api/mdkg.agents.stop.v1.json) | `agents stop --drain --json` |
| [`mdkg.run.wrapper.v1`](api/mdkg.run.wrapper.v1.json) | The launch wrapper's turn spec |
| [`mdkg.api.check.v1`](api/mdkg.api.check.v1.json) | The request check's input |
