# mdkg API v1: a reader's guide

**Status: draft.** This guide describes the API mdkg is building towards. The
CLI's `--json` output is the contract today. The CLI seams and local
`mdkg serve` ship in 0.7.1; the networked serve design is deferred to a later release.
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

## Mail across mailboxes (0.7.2)

A person's view of the work, such as the faults or review requests still
open anywhere in a repository, reads `mdkg msg list --as human:<name>
--kind fault,review_request --json`. It lists mail delivered to every
mailbox, to `--to` mailboxes, or to a `--channel`'s current members (and
the posts to it), filtered by `--from` and `--state unacked|acked|any`
(default `unacked`: a delivery in scope is still unacked). Pages are
oldest first, at most 200 messages (`--limit`, default 50) and under
1 MiB of bodies; pass `cursor` back as `--after` for the next.

- It changes nothing: the store file is opened read-only, so no ack, no
  shown mark and no purge. A body whose retention is up reads as purged.
- To ack what it listed, a client first records what it showed the
  person: `msg inbox --as human:<name> --mark-shown
  <seq>/<digest>/<id>,...`, each from the row's `seq`, `digest` and
  `messageId`, for mail delivered to that person's mailbox. Then `msg ack
  --as human:<name> <id>`. An ack never skips that step: mail not marked
  shown, or marked with a digest that isn't the message's, is refused
  with `msg.not_shown`, and nothing is acked. Only that person marks
  their mail: an agent may read a person's mailbox (`msg inbox --mailbox
  human:<name>`, or `msg wait`), but its read marks nothing shown, and
  its `--mark-shown` there is refused with `msg.not_holder`.
- Only a person runs it (`--as human:<name>` or `$MDKG_ACTOR`;
  `api.actor_not_allowed` otherwise), outside an agent's turn: not under
  the turn mark, not as a live turn's actor and not as a descendant of a
  live turn's launcher. A turn some other process launched doesn't refuse
  it, even one working in the same checkout. The live-turn guard's refusal
  is `run.turn_active`, for `msg list` and a person's `msg send` alike:
  retry it from the person's own shell, or once the turn ends. Which person may
  see which channel is for software above mdkg to decide. It has no MCP
  tool and isn't served by `mdkg serve` until person-signed requests are.
- Its rows are `msg inbox`'s message records, and it returns
  `event_cursor`. To keep a view current, use the event stream as a
  signal to list again, never as data to patch the view with: an event
  doesn't carry every row a filter (`--state acked`, `--from`) can bring
  into view. Read every page, then follow `mdkg msg watch --after <first
  page's event_cursor> --types 'mdkg.message.*,mdkg.channel.*' --json`,
  and on any `mdkg.message.*` or `mdkg.channel.*` event, list again
  (several events in a burst need one list). The stream also sends an
  `mdkg.stream.heartbeat` line after 15 seconds without an event; it
  only says the stream is alive, so don't list again for it. Also list
  again on a timer, every few minutes: a body whose
  retention runs out sends no event until some command purges it, but
  the next list already reports it as purged.
- With no store yet, a list returns no `event_cursor` and no mail, and a
  list with `--channel` is refused with `channel.unknown`, since there
  are no channels either. List again later: the first list that returns
  a cursor is the snapshot to watch from, and nothing is missed in
  between.
- Mail isn't confidential between local actors: `msg watch` already
  shows every message to any of them. The person check limits who gets
  this query, not the data.

## Agents from outside (0.7.1)

| Need | Command |
|---|---|
| What runs where, and where to pick up | `mdkg agents status --json`: per agent `state`, `supervisor`, `current_turn`, `last_turn` and `follow_from`; top-level `event_cursor` |
| The same, cheap enough to poll | `mdkg agents status --no-probe --json`: runs no harness login check, Keychain listing or billing read, and leaves those fields (`auth`, `billing`) out, never empty. Everything else is the same, including `file_backed` (defined by `.mdkg/agents/<name>.json`, whose layered definition is `effective`) and `progress` |
| Why `agents up` ended, after it's gone | `mdkg agents status --json`, per agent `last_exit`: `{supervisor_id, reason, code, at}`, with `reason` one of `cap` (`--max-turns` reached), `once`, `manual` (its trigger is `manual`, so a watch runs it no turns), `mode_changed` (its receiver changed between interactive and headless; start it again), `drained` (`agents stop`), `signal` (SIGTERM, its turns finished) or `failed` (`code` is the diagnostic code; also a reload whose new image refuses to start). Each agent's reason is its own, written by the process holding the agent's progress lock as it lets go, and it names that watch. It's shown only while the agent's progress names the same watch, finished, and no running supervisor holds the agent: a later run starts or resumes a watch, which hides an older reason. A supervisor killed outright (SIGKILL, SIGINT, a crash) writes none: an absent `last_exit` means the reason is unknown, never a clean stop |
| The store's versions as a doctor check | `mdkg doctor --only store --json`: one `store` check with `store_schema`, `mail_schema` and `upgrade_needed`, read through a read-only connection that never upgrades the store; it reads no config, projection or harness file. The store is that of the git repository the command runs in (or `--root`). Before any store exists the check is `ok`, its summary starts "no store yet", and it has none of the three fields. A store older than this build is a `warning` with `upgrade_needed: true`; one newer than this build is a `warning` too, with its own `store_schema` and `mail_schema` and `upgrade_needed: false` (this build's other commands refuse it with `coord.store_failed`). Neither fails the command, but `--strict` fails on warnings. Outside a git repository the check is `skip`; a repository git finds but can't open is `error`, with git's reason |
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

A line of `agents logs --json` can carry a `position`, an opaque string:
to resume after that line, pass its position to `--after` as it is, and
keep the last one you saw. `agent.state` frames carry none, and neither
does the `turn.started` mdkg sends in place of a purged turn's journal: a
resume from the line before replays it. A `turn.ended` mdkg synthesizes
(the journal was purged, or the turn was interrupted before its own end)
gives a position that resumes with the next turn, so it never comes again.
mdkg synthesizes one only once the turn is provably over: a receipt, an
end line or a journal `turn.ended`, or a launcher proven gone. A turn whose
launcher still runs waits for its end, whatever its supervisor's record
says. A launcher is gone when its pid isn't running here, or runs with
another start time than it recorded; the host name isn't compared, so a
host renamed mid-turn keeps its turn running, and a `ps` that can't answer
never proves a launcher gone. `agents status` and `agents turns` use the
same rule. A journal that can't be read just now is read again, never
reported purged, including the first read of `--turn <id> --follow`.
`agents turns <agent> --json` (`mdkg.agents.turns.v1`) lists an agent's
turns newest first: each one's `turn_id`, `number`, `started`, `ended`,
`state`, `outcome` and `receipt` (its retained receipt's path, or null). It
returns at most `--limit` turns (default 50, at most 500); pass a page's
`next_before` as `--before` for the next, older page, which is null on the
last one. `--turn <N>` returns turn N alone.

`notice` mail stays readable in inboxes, threads and digests without starting
an agent turn. Default final replies use it when all delivered bodies are
resolutions, decisions, notes or notices. Header-only messages do not change
that classification. An explicit operator `--reply-kind` takes precedence;
mixed actionable mail retains the ordinary reply kind.

`lease request` sends its holder a question and copies the requester's
`reports_to` with a separate notice in the same context. Its existing
`requests[].sent` names the question; optional `requests[].copied` names the
notice. Failure to send the copy is a warning after the holder was asked.

A turn that answers mail ends `replied` when its reply was imported (the
report's `reply_id` names it), and `no_reply` when the agent's final message
was exactly `NO_REPLY`, or when the turn had already answered the newest
message itself, so its final message would have reached that sender a
second time. A message answers it when the agent sent it during the turn,
over MCP or the CLI, to the newest message's sender, as a `handoff` or
`findings`, and either replying to the newest message or in its context.
A question, a note or a lease request's copy doesn't count, and the final
message is imported as usual. A channel `handoff` or `findings` delivered
to that sender can count if it replies to the message or shares its context.
When the final message is skipped, the receipt's `reply_status` starts
`not imported: the turn already mailed its recipient` and its
`final_summary` is withheld. The launcher writes the final message to
`turn-<N>.reply`, but successful cleanup can delete that file unless logs
are kept. If acknowledgement or writing the reply file fails, the
unfiltered final message can still appear in the retained receipt.
In both cases the launcher sends no final reply and acks as
answered the shown mail that is still held and still unacked: mail the agent
already acked itself keeps its own outcome, and mail in a role it lost
during the turn stays unacked. The report then has no `reply` and no
`reply_id`. A launcher receipt may still go to `reports_to`, and the agent may have sent
mail itself. A task-only turn with no mail ends `ran`, whatever its final
message. `agents up --json`, the index, the journal's `turn.ended` and
`agents status`'s `last_turn` all use the same outcome. Turns recorded
before 0.7.2 keep the outcome they were written with: a `NO_REPLY` turn
then shows `replied` with no reply imported.

A tool call's `tool.started` and `tool.ended` carry the same `call_id`: the
harness's own id for that call (Claude's `tool_use` id; OpenCode's
`callID`, or its tool part's `id` when it gives no `callID`). Pair a start
and an end by `(turn, call_id)`, and only when `call_id` isn't null: never
by tool name, since calls with one name can overlap and end in either
order. With a null `call_id`, or none (output written before 0.7.2), the
pairing is unknown. Codex turns report no tool events (their
`output.tools` capability is false), so they carry no call ids.

Long-lived commands (`msg watch`, `agents logs --follow`) have no timeout of
their own. Don't run them under a one-shot call's timeout; stop them with
SIGINT or SIGTERM.

Stopping a one-shot command: mdkg runs each git in a process group of its
own, so a signal sent to mdkg alone wouldn't reach it. On a SIGTERM or
SIGINT, a one-shot command forwards that signal to every git it started
(the git mdkg's own commands run),
waits up to 2 seconds for them to exit, kills the rest, waits up to 1 more
second for them to be gone, and then ends by the same signal (SIGTERM, if
both have arrived by then). Once that starts, no new git starts. So within
3 seconds of the signal mdkg has exited, and none of the git it started
is still running: a git write either finished before mdkg exited or never
lands. The two kinds of process below are the exceptions. A
later signal during the wait changes nothing. SIGKILL can't be forwarded:
send SIGTERM, and wait at least 3 seconds before you resort to SIGKILL.
`agents up --once` is a one-shot command. A continuous `agents up` drains
on SIGTERM (its turns finish first, and no new turn starts once the signal
has arrived, startup's git included) and forwards SIGINT as above.
`agents logs`, `msg watch` and `serve` close at their next step on either
signal instead, after the git they're running finishes.

Two kinds of process aren't covered, and either can still write after
mdkg has exited. One is an agent turn that's running, under `agents up
--once` or a continuous `agents up` sent SIGINT: its harness and what that
starts. The turn's guard stops the harness's process group shortly after
mdkg exits, but not a process that left the group for a session of its
own, and there's no guard at all when mdkg can't start one (it found no
`sleep`). The other is the `git receive-pack` that `git-serve` runs for a
push, which can still move refs.

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
are deferred to a later release; they are not the loopback transport.

## Channels

From 0.7.1, `mdkg channel …` and `--channel` name a group of mailboxes.
The 0.7.0 spellings keep working as hidden aliases that warn, until 0.8.
Message records gain a `channel` field beside the 0.7.0 one, so existing
readers keep working; the new API records have `channel` only (A21).

## Graph head events

`msg watch --json`, and `mdkg serve`'s `GET /v1/events` (which runs it),
send `mdkg.graph.head_changed` naming the graph's head: the graph
ref's tip in ref mode, and otherwise the `HEAD` of the worktree the stream
runs in. Its `data` is `{graph, head}`: the graph's `graph_id` and the
commit the head names, or `head` null while it names none, and its `subjects` are `["/v1/graphs/<graph_id>"]`.
Like a heartbeat it's ephemeral (`durable: false`): its `cursor` repeats
the last durable one, serve sends it with no SSE `id:`, and it's never
replayed. Each stream sends exactly one when it starts, naming the head
it starts from, and then one each time the head changes. It
reads the head once each poll interval (about a second), so several moves
within one can arrive as one event naming the last. A client that keeps
graph content reads it again on each of these events, the first
included, so a move just before the stream started is never missed; a
move while no stream ran shows in the next stream's first event.
`status --json`'s `graph.tip` is the same head (with `graph.ref` the
graph ref in ref mode, null in checkout mode). In checkout mode, while
`HEAD` names no commit (unborn), `status` leaves `graph` out and the
event's `head` is null: at the start, or when a checkout switches to a
branch with no commits. In ref mode a missing graph ref is refused
before either reads the head: `status` and a starting stream fail with
`graph.ref_missing`. A stream already running when the graph ref is
deleted sends `head` null, and the ref's tip again once it's recreated.
A read of the head
that fails sends nothing and changes nothing (`status` warns
`io.git_failed`). Graph content is git's, so the event is a hint to read
again, never a history. `--types` selects it like any type; a `--channel`
or `--mailbox` filter leaves it out, and that stream never reads the
head. A watch holds no graph lock while it streams.

## Naming a coordinator

`status --json` and `config show --json` carry `store_id`: the
coordinator store's own id, the one `anchor` names its file by. It's the
same from every worktree of a repository and for the store's whole life,
including after `store restore` puts back one of its snapshots, so a client
can key what it keeps per coordinator by it rather than by a path. It's
null until something creates the store (the first claim, mail or other
write).

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

`msg pending --as <actor>` lists unanswered `question` and `decision` mail
in store send order, oldest first, including acknowledged deliveries. It reads
the actor's mailbox and currently held roles without marking shown or acking.
A stored reply counts as an answer only when it explicitly names the original
with `--reply-to` and its sender or importing writer (`via`) received that
original; another message in the same context does not. Reply evidence survives
body purging. Notices are excluded. JSON rows include `ageMs` (elapsed
milliseconds, saturated at zero), and `cursor`/`more` support `--after` and
`--limit`. This is a coordination query, not a claim that a reply resolved the
question or granted authority. Historical role-holder provenance is not
inferred from current leases.

`pack --for-message <id> --budget <tokens>` builds context for one stored
message without marking mail shown or acknowledging it. It includes its
thread (trigger first, then the rest in send order), nodes bound with
`--node`, explicit node IDs or Markdown links in the thread, and accepted
decisions that point at those nodes. It preserves node bindings when bodies
have expired; expired bodies are labelled. Other unrelated graph neighbors
are excluded. `--for-message` replaces the positional node and `--query`.
For a channel post, the thread is its explicit reply ancestors and descendants,
including replies across contexts; unrelated channel posts and sibling branches
are excluded. Direct mail retains its explicit context. Bodies are fetched in
bounded pages until enough text can fill the pack; `message.cut` also reports
this read-budget cutoff. The MCP `pack` tool exposes this as `for_message`.

Thread text and node sections share pack's existing four-bytes-per-token
budget and node cap. When there are referenced nodes, the thread may occupy
up to half the budget, leaving the remainder for the existing ranked node
selection; without nodes, it may use all remaining space. The trigger is
kept first under truncation. JSON's optional `message` metadata reports the
context, total message count and whether thread text was cut; `nodes[].cut`
and `omitted` retain their existing meanings. The concise profile also keeps
parsed requirement statements beyond a node's opening. As with other packs,
a small budget can truncate the seed or omit nodes and decisions; raise it
to see more. Packed text remains untrusted data, not authority.
