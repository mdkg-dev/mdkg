# API v1 conformance

The JSON cases in `cases/` describe canonical resource operations, throwaway
fixtures, resource and event assertions, and placeholders such as `$lease`.
`runner.rs` owns the shared assertions and captures each placeholder at its
first occurrence, requiring the same value thereafter. A transport implements
its `Driver` and `Events` traits; the CLI adapter lives in
`crates/mdkg-cli/tests/watch.rs`. It validates emitted frames against
`docs/api/` and the frozen command envelopes against `schemas/`. No case uses
the developer's graph or homes.

The ten cases cover concurrent list/watch (3), resume (4), cursor errors (5),
keyed sends (6), ordering and historical payloads (7), channels and CLI aliases
(8), forward compatibility (13), pages followed by events (17), canonical paths
(18), and imported history (20). Store-copy, log and imported-history setup are
fixture oracles, independent of the API transport. Event cursors are returned
unchanged. The CLI adapter presents its existing numeric mail position as the
runner's opaque page token; it changes no shipped envelope or paging behavior.

Expectations under `cli` apply only to the CLI: frozen envelope schemas, exit
codes, deprecated spellings and warnings. Case 8 runs the canonical operations
for every transport and additionally compares CLI aliases when the driver
supports them. HTTP starts with channel routes and never implements legacy
aliases. Shared assertions operate on normalized resources and API events;
the CLI checks its raw envelope before normalization. Producer schema checks
reject undeclared fields while clients accept extensions. Every frame consumed
by a shared case also gets unknown fields, an unknown event type and open-set
values injected. The adapter encodes these frames and passes them through the
same wire decoder used by its live follow; the sample client skips the unknown
type and continues reading. Case 13 supplies additional explicit mutations.
The paging oracle compares each late event's identity, order and payload with
the post-snapshot log sequence, even when later pages already contain its URI.
Mutant fixtures prove that closed decoders and stale streamed events fail the
respective oracles.

Stored message bodies are opaque JSON in API read schemas, including legacy
part objects and non-array values. The existing CLI keeps its known write
shapes; generic input validation is a 0.8 follow-up. API audit extensions are
accepted by readers while producer conformance requires declared audit fields.
Injected event types are always distinct from the source event's type.

Case 22 (`same_envelope`) is the transport-neutral envelope case
(dec-01a18dd9): each operation is the registry's (`mdkg_ops::OPS`), with its
arguments by their HTTP names, and runs on each transport the case names,
each on its own copy of the fixture: `cli-local` (argv), `mcp` (a stateless
`tools/call` to `mdkg mcp`, pinned to the operation's actor) and
`http-socket` (one request on serve-local's socket, the actor in
`Mdkg-Actor`). The runner checks each transport's own outcome (the exit
code, MCP's `isError`, the HTTP status) and that every transport's envelope
equals the first's, with the fixture's folder written `<fixture>` and each
`per_copy` pointer (a value a store has its own of, such as an inbox's
`event_cursor`) masked. The transports are in
`crates/mdkg-cli/tests/watch/transports.rs`; the networked drivers
(signed `http-tcp`, `cli-remote`) come with their design.

The HTTP driver joins the event and paging cases above when `serve` has
them in the same terms.

Run the CLI cases with:

```sh
CARGO_BUILD_JOBS=4 cargo test -p mdkg --test watch -- --test-threads=4
```

Case 22 alone runs with `same_envelope` as the filter.

Cases 9 to 12, 14 to 16 and 19 cover the supervisor and turn-output seams:
turn output per harness (9), drain (10), the launch wrapper (11), re-attach
(12), re-attach across turns (14), output order (15), the retention boundary
(16) and turn index recovery (19). They exercise the CLI only, until `serve`
has the matching operations. Each case file names its driver: case 9 is a
table of native lines and the events they normalize to, read by a unit test
in `crates/mdkg-run/src/turn/live.rs`; the others run fake harnesses in
`crates/mdkg-cli/tests/agents.rs`. Case 11's first driver runs Claude,
Codex and OpenCode. Cases 10, 12, 14 to 16 and 19 drive Claude's fake only:
drain, re-attach, the index and retention are supervisor paths every harness
shares, and case 9 covers each harness's own lines. So case 15 checks output
order across Claude's stdout and stderr, not Codex's all-stderr output. Run
them with:

```sh
CARGO_BUILD_JOBS=4 cargo test -p mdkg-run --lib conformance_09 -- --test-threads=4
CARGO_BUILD_JOBS=4 cargo test -p mdkg --test agents conformance_ -- --test-threads=4
```

The signal-ending case is temporarily ignored until the shared signal-handler
dependency lands; the lane is incomplete until that test is enabled and passes.
The read-only backend's cursor/replay and concurrent-snapshot checks live in
`mdkg-coord-sqlite`'s `events::tests`.
