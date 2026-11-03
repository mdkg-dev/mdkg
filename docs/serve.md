# `mdkg serve`: the local API (0.7.1)

`mdkg serve` exposes the CLI's `--json` operations over HTTP/1.1 on this
machine, for programs that would rather speak HTTP than run `mdkg` as a
child process. Each request is the same operation the CLI runs, and its
response is the same envelope the CLI prints (API v1, A12, A14).

It is local only. There is no network mode in 0.7.1: no TLS, no signed
requests, no machine enrolment, no remote clients. Those are the later
networked design (the serve edd's networked sections), not this release.

## Starting and stopping

```sh
mdkg serve --json            # the local socket only
mdkg serve status --json     # is one running for this repository?
```

`mdkg serve` runs in the foreground. It prints one `mdkg.serve.v1`
envelope once it listens (the socket, the graph, its `capabilities`),
then JSON log lines on stderr.

- **Ref mode serves the checkout top's graph.** With `mdkg.graph.ref` set,
  serve runs against the checkout and reads the graph from the ref, the
  checkout top's graph. A nested graph isn't served in 0.7.1: `--root`
  naming one is refused with `api.unsupported` (without `--root`, serve
  uses the checkout top). When the checkout top can't be determined,
  serve refuses rather than start (`api.unsupported`, or git's own
  `io.git_failed` when an earlier lookup fails first).
- **One per repository.** A second `mdkg serve` for the same repository
  is refused with `api.serve_running` (the `serve.lock` in the git common
  folder). `mdkg serve status` reads `serve.json` there and tells
  `running`, `stale` (a crashed serve's file, replaced at the next start),
  `starting`, `absent` or `unreadable`.
- **SIGTERM or SIGINT stops it gracefully.** Once any turn grace (below)
  ends, a long poll (`:wait`) under way is answered `api.draining` (503)
  at once; other requests under way get up to 10 s to finish; an open
  event stream gets a last `mdkg.stream.error` frame (`api.draining`)
  and closes; then the socket and `serve.json` are removed and the lock
  released. While an agent turn runs on the
  repository, serve keeps serving for up to `serve.drain_grace_s`
  (default 120 s) first; a second signal ends that wait. It exits 0 after
  a clean stop, and non-zero after a failed start or when the 10 s limit
  cut a request off. Every `mdkg` process a request or stream started is
  stopped before serve exits, and as soon as a long poll's or a stream's
  client goes away.
- **Under a service** (a LaunchAgent, a `systemd --user` unit), allow at
  least `serve.drain_grace_s` plus 20 s before a kill: the 10 s for
  requests, then a few seconds to stop their processes and end streams.
- An upgrade is a restart. serve runs its own program for each request,
  and checks that the file hasn't been replaced before each one and every
  tenth of a second besides, so an idle serve notices too. Once it has, requests are refused with
  `api.draining` and serve drains and exits 0, so it never runs a
  different build than itself against the store. Run it under a service
  that restarts it whenever it exits, even with 0: `Restart=always` for a
  `systemd --user` unit, `KeepAlive` set to `true` for a LaunchAgent (the
  common `Restart=on-failure` and `SuccessfulExit = false` don't restart
  it). Or start it again yourself. The check reads the file's metadata,
  so a `chmod`, an attribute a backup tool writes, or a copy caught half
  way restarts serve the same way. Off Linux, a file renamed over the
  program between that check and a request's child starting runs as the
  new build once; serve drains at its next check.

## The socket

`<git common dir>/mdkg/local.sock`, mode 0600, in a 0700 folder. When that
path is too long for a socket (104 bytes on macOS, 108 on Linux), serve
puts it under `${XDG_RUNTIME_DIR:-$TMPDIR}/mdkg-<uid>/` instead;
`serve.json` and the start envelope name the path in use. Find it there,
never by building the path yourself.

The socket's file mode (0600, in a 0700 folder) is the authentication:
any process of your OS user may connect, and it acts in your
repository's single trust domain. serve also refuses a connection from
another user. A request names its actor with `Mdkg-Actor`, trusted exactly
as `--as` is.

Its capability is `serve.local`: a later signed socket (`api.sock`, the
networked design) will be a different socket with a different capability,
so clients of this one keep working.

**Never forward the local socket** over SSH or anything else. Whoever
reaches it acts as you, with no signature, no turn check and no record of
where they came from.

## Loopback TCP (optional)

```sh
mdkg serve --listen 127.0.0.1:7461
```

For a program that can't open a Unix socket. It's opt-in only:

- the address must be `127.0.0.1:<port>` or `[::1]:<port>`, with an
  explicit port. Any other address, an unspecified one (`0.0.0.0`, `::`)
  or port 0 is refused, and nothing is bound. A port that's taken fails
  the start; no other port is tried. On macOS, a port another program
  holds on every address (`0.0.0.0:<port>`) may not count as taken, so
  serve then shares it on loopback: pick a port nothing else uses;
- `serve.listen` may set it, but only from your user config
  (`~/.config/mdkg/config.json`); from the repository's machine-local
  config it's ignored with a warning, since every agent can write that
  file;
- every request needs `Authorization: Bearer <token>`;
- a request with an `Origin` or `Sec-Fetch-*` header (a browser page) is
  refused, as is one whose `Host` isn't `localhost`, `127.0.0.1` or
  `[::1]`.

**The token file** is
`<state>/mdkg/serve/tokens/<16 hex digits>.token`, where `<state>` is
`$XDG_STATE_HOME`, or `~/.local/state`. It's created on the first loopback
start: 32 random bytes as hex, mode 0600, in a 0700 folder. The start
envelope's `token_file` names its path, as do `serve.json` and `mdkg serve
status`; the token itself never appears in an envelope, a log line, a
diagnostic or `serve.json`. Remove the file to
get a new token at the next start. A file readable by others is refused,
never repaired.

The token stands for "outside an agent turn" on loopback, where serve
can't see the caller's process; serve still refuses a request that names
a running turn's actor. It's outside the repository and its
`.git`, and a turn's sandbox lists neither your state folder nor the
token, so a sandboxed agent can't read it. Only a launch wrapper
(`run.wrapper`, off by default) sandboxes a turn's reads: without one,
an agent runs as you and can read the token file, so serve warns when it
starts on loopback with no `run.wrapper` configured. Don't put the token,
or the path to it, in an agent's environment, prompt or wrapper spec.

## Agents in a turn don't use serve

On the socket, serve reads the connecting process's id from the kernel
and refuses a request from inside an agent turn: one whose process
descends from a running launcher turn, or that names a running turn's
actor (`api.actor_not_allowed`). A caller it can't identify is refused
too. Only `GET /` is answered regardless. Agents in a turn use their mdkg
CLI or MCP tools, whose own checks apply.

This is a guard rail, as the CLI's turn checks are, not a boundary: a
process an agent detaches from its turn (`setsid`, a double fork) no
longer descends from the launcher, and passes when it names an actor
with no running turn. A sandbox (`run.wrapper`) is what confines an
agent.

Operator, host and decision operations are never served over HTTP, whoever
calls: a decision comes only from your human's channel, as for MCP.

## Requests

- `GET /` names the serve: `serve_id`, `graph_id`, `api` and
  `capabilities` (`mdkg.api.serve.v1`).
- The CLI's operations, at their API v1 paths (A2), routed from the
  operation registry: `GET /v1/graphs/<graph_id>/nodes` is `mdkg list`,
  `POST /v1/leases` is `mdkg claim`, and so on. serve runs each as one
  `mdkg` call of its own build, as MCP does, and answers with the
  envelope that call printed, so the socket, the CLI and MCP give the
  same answer. A refusal is the CLI's own envelope (`ok: false`, its
  diagnostics), with an HTTP status derived from the exit code (A12).
  - **Arguments:** the path's values, then for a `GET` its query
    (a repeated name gives a list), for a `POST` a JSON object body. A
    flag goes by its long name (`_` for `-`); a positional by its
    argument's name, which `--help`'s placeholder doesn't always show:
    `new`'s `<TYPE>` is `node_type`, so `POST /v1/graphs/<graph_id>/nodes`
    takes `{"node_type": "task", "title": "…"}`. Two file flags take
    the JSON itself instead of a path: `--op-file` is `op` and
    `--data-file` is `data`, each a JSON value, which serve hands the
    command on its stdin; no other `--…-file` flag is served. For
    `entries:update`, `op` is one operation object (`{"op": {…}}`), never
    an array, and each operation's outcome is in `data.results`: the
    envelope's `ok` can be true while a result there failed, so read
    them. A name the operation doesn't take is
    refused with the list of those it does.
  - **`Mdkg-Actor`** is required: the actor, as `--as` gives it.
  - **Not served:** operator, host and decision operations, a decision
    post, a ledger `decide` or `residual`, a turn's or a role's lease;
    streams other than `/v1/events`. On loopback, a `human:` actor's
    ledger writes (`entry` operations, `done --residual`) are refused
    too: make them over the socket or with the CLI.
- **HTTP status** (A12), from the CLI's exit code; read `ok` and the
  diagnostic codes, not the status:

  | Answer | Status |
  |---|---|
  | success | 200; 201 for a create: `POST /v1/graphs/<graph_id>/nodes` (`new`), `POST /v1/leases` (`claim`; 200 when it renews a lease you hold), `POST /v1/messages` (`msg send`), `POST /v1/channels` (`channel create`) |
  | usage or ambiguous (exit 1, 7) | 400 |
  | validation (exit 2) | 422 |
  | not found (exit 3) | 404 |
  | conflict (exit 5) | 409 |
  | timeout (exit 6) | 408 |
  | failure (exit 4) | 500 |
  | `api.unsupported`, and an unknown route or method (A12: clients re-read `capabilities` on it) | 501 |
  | a `HEAD` request whose head parses, answered before routing, with no body (one over the head limits is 413 first) | 405 |
  | `api.draining`, a busy store | 503 |
  | `api.actor_not_allowed`, `api.origin_refused` | 403 |
  | `api.unauthenticated` (loopback token) | 401 |
  | `api.too_large` (body or head over serve's limits) | 413 |
  | `api.output_gone` (a purged turn's output, once `agents logs` is served; it isn't in 0.7.1) | 410 |

  A create is a `POST` to a collection, with no `:verb`.
- `msg wait` is a long poll: `GET /v1/mailboxes/<mailbox>:wait`, with
  `timeout` (seconds, 600 when omitted) in the query, answered once, with
  `msg wait`'s own envelope. A `timeout` over 600 is refused with
  `usage.invalid_arguments` (400), never cut short. A stop answers one
  under way `api.draining` (503) as soon as its drain starts (after any
  turn grace), and a client that disconnects
  ends it; a client sends it again once serve is back. serve treats the
  end of a client's request stream as a disconnect: a client that shuts
  down its sending half gets its long poll cancelled (503,
  `api.draining`), so keep the connection open until the response ends.
  While a long poll waits, serve reads and drops what its client sends,
  and if it dropped any, the connection closes after the answer. A
  client that sends 1 MiB or more meanwhile has its long poll cancelled,
  as a disconnect. A request that arrives after serve last looked, just
  before the answer, is buffered and runs as any pipelined request: don't
  send on a connection while its long poll waits.
- `GET /v1/events` streams events as server-sent events. Each `data:`
  line is exactly the line `mdkg msg watch --json` prints for the event;
  a durable event's `id:` is its cursor. Resume with
  `?after=<the last id you saw>` (`after=start` replays from the first
  record); `Last-Event-ID` is never read. Filters: `types`, `channel`
  (repeatable) and `mailbox`, as `msg watch` takes them.
- Idempotency keys are honoured only where the CLI has them (`msg send
  --id`, claims); there is no request table in 0.7.1.

## Settings

From your user config, and the repository's machine-local config
(`<git common dir>/mdkg/config.local.json`); never the committed
`.mdkg/config.json`, where a `serve` value is ignored with a warning:

```json
{"serve": {"listen": "127.0.0.1:7461", "drain_grace_s": 120,
           "limits": {"body_bytes": 4194304, "connections": 256}}}
```

The machine-local file can only tighten: `listen` from it is ignored, and
a limit applies only where it's lower. `drain_grace_s` is 0 to 3600; 0
still gives requests under way their 10 s.

## Not in 0.7.1

- TCP beyond loopback, TLS, signed requests, machine enrolment, person
  keys, and remote clients (`mdkg remote`, `mdkg machine`, `mdkg person`).
- A request table for idempotency keys on every write.
- Turn output streams (`/v1/agents/{name}/turns/{turn_id}/output`).
