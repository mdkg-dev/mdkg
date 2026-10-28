# Concepts

mdkg keeps design knowledge in markdown files in git. Work definitions are
also markdown, local by default or committed with shared work; coordination
lives in a local store. This page explains the few ideas
everything else builds on. The examples are real output from a fresh
repository with a little code in it (`src/parser.rs`), where Ada plans an
importer, and, for migration, a small 0.6 graph. Ids and times will differ
in yours; `info` lines and some hints are left out, and long lines are cut
short with `…`.

## Two planes

mdkg splits what it keeps by how long it lasts.

| | Knowledge | Coordination |
|---|---|---|
| What | Goals, decisions, designs, rules, skills, task definitions and their final states | Claims and leases, temporary statuses, messages, the log of every change |
| Where | Markdown with frontmatter: design in `.mdkg/` and git; work beside the store by default, or in git with shared work | A SQLite file in the repository's git folder (`.git/mdkg/`), never committed |
| Changes | File edits; committed knowledge goes through commits and reviews | By the minute |

There's no server or daemon: every command opens the files and the store
itself. Search indexes aren't a plane: they're a cache rebuilt from the
files, never committed ([Search](#search-and-the-derived-index)).

### Where work lives: local or shared

`mdkg init --work local|shared` chooses where work definitions live and records
`work.location` in `.mdkg/config.json`:

- **Local (the default, `local`):** tasks, bugs, spikes, tests and checkpoints
  stay beside the coordinator store on this machine, shared by all worktrees.
  Git keeps design and shared knowledge: goals, decisions, designs, rules and skills.
- **Shared (`git`):** work definitions and final states are committed with
  the graph. Claims, leases and temporary statuses stay in the local store.

The older `work.committed` config values remain readable aliases; conflicting
values refuse. Existing graphs without either setting keep their git storage.

```console
$ mdkg init --work local --graph-only
$ mdkg new task "Local task" --status todo
created task-01a173c3-6eaf-73a7-bcd2-4f8ffdbd73a1 (.git/mdkg/work/root/task-local-task.md)
```

Every command reads both places as one graph.

With local work, work refers to design, never the other way around. A
private task names the committed epic, goal or decision it serves (its
`epic`, `parent` or `refs`), but a committed node never names a private
one: a fresh clone couldn't find it. `validate` refuses such a ref
(`edge.private_target`), and so do the commands that would write one. A
goal follows its private work by scoping the committed epic or feat that
holds it.

## Identity

### Ids

Every node has an id: its type, then a UUID.

```console
$ mdkg new goal "Ship the importer"
created goal-01a173c2-bb84-775d-acd0-c6947a52038a (.mdkg/work/goal-ship-the-importer.md)
$ mdkg new task "Parse the CSV header" --status todo --parent importer
created task-01a173c2-bbe6-7550-b12d-9f33d09ccfbe (.mdkg/work/task-parse-the-csv-header.md)
$ mdkg new task "Write the CSV reader" --status todo --parent importer --blocked-by "csv header"
created task-01a173c2-bc3f-718a-af20-3adfa30a85b3 (.mdkg/work/task-write-the-csv-reader.md)
$ mdkg list
count: 3
goal-01a173c2-bb84-775d-acd0-c6947a52038a | goal | todo/p9 | Ship the importer
task-01a173c2-bbe6-7550-b12d-9f33d09ccfbe | task | todo/p9 | Parse the CSV header
task-01a173c2-bc3f-718a-af20-3adfa30a85b3 | task | todo/p9 | Write the CSV reader
```

The UUID is a UUIDv7: its first 48 bits are the time it was made, and the
rest is random. Any machine or agent mints one without asking anyone, so
two branches, or two agents on one branch, never hand out the same id. Ids
sort in the order they were made.

The type in front is a hint for people. The UUID alone is the identity:
`mdkg retype <node> bug` turns a task into a bug, and rewrites the id
everywhere it's mentioned.

### File names

A node's file is named after its type and title,
`.mdkg/work/task-write-the-csv-reader.md`. The id lives only inside the
file (here with its empty keys left out):

```markdown
---
id: task-01a173c2-bc3f-718a-af20-3adfa30a85b3
type: task
title: Write the CSV reader
status: todo
priority: 9
parent: goal-01a173c2-bb84-775d-acd0-c6947a52038a
blocked_by: [task-01a173c2-bbe6-7550-b12d-9f33d09ccfbe]
created: 2026-10-25T15:51:44.447Z
updated: 2026-10-25T15:51:44.447Z
---
```

Since nothing refers to a file by its name, you can rename files at any
time. If two machines create the same name, git shows an add/add conflict;
renaming one of the files resolves it, and no reference changes.

### Handles

Commands that take a node accept:
- the full id;
- a unique prefix of the UUID, with or without the type, as git takes a
  short commit SHA (`01a173c2-bc3f`, or `task-01a173c2-bc3f`);
- words from the title (`"csv header"`);
- on a migrated graph, the node's old 0.6 id (`task-4`).

A handle that names more than one node fails with exit code 7 and lists
the candidates (up to five):

```console
$ mdkg show 01a173c2-bb
error node.ambiguous: that handle names 2 nodes
  hint: matches: goal-01a173c2-bb84-775d-acd0-c6947a52038a  Ship the importer; task-01a173c2-bbe6-7550-b12d-9f33d09ccfbe  Parse the CSV header; give more of the id, or the full id
```

Machine-readable output (`--json` and the MCP server) always carries full
ids.

### Migrated nodes: `legacy_id`

`mdkg migrate` gives every node of a 0.6 graph a new id, and keeps its old
one in a read-only `legacy_id` key. A node that stays a file keeps its file
name, and a mention of another node in its body becomes a link. In a small
migrated 0.6 graph (a separate example):

```markdown
---
id: task-01a09d36-a804-7497-ac85-094a3552b4cb
legacy_id: task-4
type: task
title: Map the old fields
status: todo
priority: 2
created: 2026-09-14T00:00:00.004Z
updated: 2026-09-14T00:00:00.004Z
---
# Overview

See [task-3](task-01a09810-4c03-7c78-b939-6123eade2b25).
```

The old id still works as a handle, and listings show it beside the new
one:

```console
$ mdkg list
count: 3
task-01a09810-4c03-7c78-b939-6123eade2b25 (task-3) | task | todo/p2 | Import the old data
task-01a09d36-a804-7497-ac85-094a3552b4cb (task-4) | task | todo/p2 | Map the old fields
task-01a0a25d-0405-75fa-a9e9-f50a93143568 (task-5) | task | todo/p2 | Check the import
$ mdkg show Import
error node.ambiguous: that handle names 2 nodes
  hint: matches: task-01a09810-4c03-7c78-b939-6123eade2b25 (task-3)  Import the old data; task-01a0a25d-0405-75fa-a9e9-f50a93143568 (task-5)  Check the import; give more of the id, or the full id; a legacy id like task-3 works too
```

A migrated id's time is its node's old `created` date plus its old number
in milliseconds, so nodes created on the same day with the same number
(`task-4` and `bug-4`) share the first 12 hex digits of their UUIDs. On a
migrated graph, give a prefix its type (`task-01a09d36-a804`) or use the
legacy id (`task-4`).

Every ref in the frontmatter uses the new ids; titles keep their mentions
as written. See [migrating from 0.6](migrating-from-0.6.md).

### Recommended headings

`mdkg new` writes each node from its type's template: the graph's own,
`<templates.root_path>/<templates.default_set>/<type>.md` in the config
(by default `.mdkg/templates/default/<type>.md`; `chk.md` for a
checkpoint), else the built-in one. Its headings are the type's
recommended headings, and `validate` warns (`node.heading_missing`) about
each one a node's body lacks, in any order and at any level, case
ignored. Edit a template and the recommendation follows it. The built-in
templates' headings:

<!-- headings:start -->
| Type | Recommended headings |
|---|---|
| `task` | Overview, Acceptance Criteria, Files Affected, Implementation Notes, Test Plan, Links / Artifacts |
| `bug` | Overview, Reproduction Steps, Expected vs Actual, Suspected Cause, Fix Plan, Test Plan, Links / Artifacts |
| `feat` | Overview, Acceptance Criteria, Notes |
| `spike` | Research Question, Context And Constraints, Search Plan, Findings, Options And Tradeoffs, Recommendation, Follow-Up Nodes To Create, Skill Candidates, Data Structures And Algorithms Notes, UX Notes, Security Notes, mdkg.dev Launch Implications, Evidence And Sources |
| `test` | Overview, Target / Scope, Preconditions / Environment, Test Cases, Results / Evidence, Notes / Follow-ups |
| `epic` | Goal, Scope, Milestones, Out of Scope, Risks, Links / Artifacts |
| `goal` | Objective, End Condition, Non-Goals, Recursive Algorithm, Required Skills, Required Checks, Acceptance Criteria, Definition Of Done, Stop Conditions, Current State, Iteration Log, Skill Improvement Candidates, Completion Evidence |
| `loop` | Operating Model, Scope, Definition Of Done, Required Evidence Lanes, Pre-Run Questions, Pre-Approved Read-Only Actions, Approval-Gated Actions, Child Node Materialization, Template Lineage, Run And Evidence Model, Blocker Continuation Policy, Waiver And Decision Policy, Closeout Matrix, Validation Plan, Current State |
| `checkpoint` | Summary, Scope Covered, Decisions Captured, Implementation Summary, Verification / Testing, Known Issues / Follow-ups, Links / Artifacts |
| `edd` | Overview, Architecture, Data model, APIs / interfaces, Failure modes, Observability, Security / privacy, Testing strategy, Rollout plan |
| `dec` | Context, Decision, Alternatives considered, Consequences, Links / references |
| `prd` | Problem, Goals, Non-goals, Requirements, Functional, Non-functional, Acceptance Criteria, Metrics / Success, Risks, Open Questions |
| `prop` | Summary, Motivation, Proposal, Impact, Risks, Alternatives, Next Steps |
| `rule` | Purpose, Scope, Requirements, Notes |
| `archive` | Archive Entry, Provenance |
| `manifest` | Purpose, Runtime, Work Contracts, Capabilities |
| `spec` | Purpose, Runtime, Work Contracts, Capabilities |
| `work` | Capability, Inputs, Outputs, Receipt |
| `work_order` | Request, Inputs, Queue refs, Requested Outputs, Constraints |
| `receipt` | Outcome, Artifacts, Proof, Redaction, Notes |
| `feedback` | Feedback, Evidence |
| `dispute` | Dispute, Evidence, Resolution |
| `proposal` | Summary, Evidence, Proposed Change, Review |
<!-- headings:end -->

A 0.6 graph keeps 0.6's shorter list, for the types 0.6 checked.

## Coordination

### Actors

Every command acts as an *actor*: `--as <name>`, else `$MDKG_ACTOR`, else
the worktree's folder name. Leases and mail belong to actors. Here Ada's
actor is `ada`, and a reviewer later reads mail with `--as reviewer`.

### Claims and leases

A claim takes a lease on a node, a file or a folder, so nobody else edits
it at the same time:

```console
$ mdkg claim "csv header"
claimed .mdkg/work/task-parse-the-csv-header.md as ada until 2026-10-25T17:51:52Z (lease-0)
$ mdkg claim src/parser.rs
claimed src/parser.rs as ada until 2026-10-25T17:51:53Z (lease-1)
$ mdkg --as bob claim src/parser.rs
src/parser.rs: error coord.lease_held: src/parser.rs is held by another lease until 2026-10-25T17:51:53Z (lease-1)
  hint: pick other work, or wait for the lease to end
  holder: ada
$ mdkg status
acting as ada; settling on main
lease lease-0 .mdkg/work/task-parse-the-csv-header.md by ada until 2026-10-25T17:51:52Z (2h 0m left)
lease lease-1 src/parser.rs by ada until 2026-10-25T17:51:53Z (2h 0m left)
nothing waiting to merge
```

Bob's claim exits with code 5: someone else holds it. A lease lasts two
hours; `mdkg renew` extends yours, and an expired one blocks nobody.

`mdkg release <path>` gives one lease up early, and `mdkg done` (below)
releases its node's lease. Later, once Ada's branch has merged and she
has committed everything, `mdkg release --all` gives up every lease she
still holds, so none of them blocks anyone until it expires:

```console
$ mdkg release --all
released src/parser.rs (lease-1)
```

Each lease is released on its own, so if a run stops part way, running
it again finishes the job. If you've configured a lease authority (an
outside command that holds file and node leases, so they hold across
machines), `--all` releases only this repository's own leases, such as
roles and turns, and warns about the rest. In another repository, with
an authority:

```console
$ mdkg release --all
released role:reviewer (lease-4)
warning coord.authority_remote: only this repository's own leases were released; the lease authority still holds yours there
  hint: release those by path: `mdkg release <path>`
```

### Statuses, done and settlement

The lasting statuses (`backlog`, `todo`, `done`) live in the files.
Temporary ones never touch them, so they never become commits: a claimed
task shows as `progress`, and `mdkg update --status review` (or
`blocked`) marks the rest.

Commit the implementation before finishing its task. Workers make their own
commits; the launcher never stages or commits their files. `mdkg done` writes
the final status into the node and, with shared work (`work.location: "git"`),
commits that node:

```console
$ mdkg done "csv header"
done task-01a173c2-bbe6-7550-b12d-9f33d09ccfbe in 0f887ac5f4f4 (cs-1)
```

That commit is recorded as a changeset (`cs-1`). Work settles when it
reaches the main branch (`main`, or the branch `git config mdkg.mainRef`
names). `mdkg status` lists changesets still waiting, in the order their
branches must merge, and `mdkg abandon --branch <branch>` drops a branch
that won't. When a claim warns `coord.sync_needed`, another branch has a
newer version of that node: run `mdkg sync <node>` before editing.

### The log, anchors and bundles

Every change the store makes appends a record to a hash-linked log: each
record holds the hash of the one before, so a changed or missing record
breaks every hash after it. `mdkg verify` checks the chain:

```console
$ mdkg verify
verified store c41df1afb5e26f5068d3b79a30990a10: 8 records, head sha256:8a3eebc5b3f390706ea65323a1c718c0ad627aa184158c4ae4afc06c36146934
warning log.anchor_missing: no anchor to check against: the chain itself verifies
```

An anchor puts the log's head into git, so the repository can prove what
the store held at that point. `mdkg anchor` writes one for you to commit
(`mdkg goal done` writes one too):

```console
$ mdkg anchor
checkpoint 9: Records 1 to 8 (8) by ada, bob. Kinds: lease.claim 3, main.observe 2, changeset.commit 1, log.genesis 1, work.status 1. …
anchored store c41df1afb5e26f5068d3b79a30990a10 at seq 9 (sha256:3c80779a5838f08a002cd02ebacd27c2259a20b2a457bfffab838ee4d16097b1) in .mdkg/anchors/c41df1afb5e26f5068d3b79a30990a10.json; commit it
```

`mdkg export --work` writes the whole log as a bundle (one record per
line), which `mdkg verify <bundle>` checks anywhere, with no store.

## Agents

Claude Code, Codex and OpenCode reach mdkg through the CLI, or through
`mdkg mcp`, an MCP server that `mdkg install --enable mcp:<harness>` sets
up for each of them.

- **Claims** work for agents as they do for people: an agent claims a task
  and the files it will edit before it starts, so two agents never take
  the same one.
- **Rooms and mail** let agents and people talk. A message goes to a room
  or straight to mailboxes; each recipient acks what it has read.

  ```console
  $ mdkg room create importer --member reviewer
  room importer: ada, reviewer
  $ mdkg msg send --room importer --kind question --text "Should the reader accept a BOM?"
  sent 01a173c2-ed05-7409-9717-ee6226075f4d #1 to reviewer
  $ mdkg --as reviewer msg inbox
  #1 01a173c2-ed05-7409-9717-ee6226075f4d question from ada to room importer (reviewer) at 2026-10-25T15:51:56Z
    unacked by reviewer
    untrusted body from ada between mdkg-body-2cd19af6c10496ac lines:
  mdkg-body-2cd19af6c10496ac
  Should the reader accept a BOM?
  mdkg-body-2cd19af6c10496ac
  cursor 1
  ```

  Message bodies come from other agents, so mdkg always fences them as
  untrusted data, between random marker lines a body can't forge.
- **Agent definitions** are committed `.mdkg/agents/<name>.json` files
  (`mdkg.agent.v1`), managed with `mdkg agents add`, `edit` and `rm`.
  Deprecated inline `.mdkg/config.json` entries remain readable.
- **Graph checkout registration** is local to each clone. Install maps
  committed graph IDs to the managed checkout's Git admin name; writes
  through that checkout require a matching registration. See
  [the graph checkout](agents.md#the-graph-checkout) for installation,
  synchronization and recovery.
- **Configuration inspection:** `mdkg config show` displays merged values
  and their sources without changing files. Some displayed user and local
  settings are not yet used by runtime commands; see the
  [transitional notice](agents.md#inspecting-configuration).
- **`mdkg agents up`** runs from the graph checkout, each worker in its own
  git worktree and branch, and wakes one whenever mail reaches it or its
  rooms. Sessions and routes stay local. A session you start yourself can
  wait for mail with `msg wait`; registered interactive Codex sessions use
  the [mail receiver](agents.md#interactive-codex-mail-receiver).
- **The turn policy** decides how an agent is woken. By default one turn
  reads up to 20 unread messages of one conversation and replies once; the
  agent resumes its last session; and a loop guard pauses a thread after 10
  automatic turns in an hour, until the hour rolls over or you run `mdkg
  agents resume`.
- **Delegated approvals** let a locally configured delegate relay one
  permitted action on an exact committed node version. The launcher
  revalidates the approval before placing its fixed statement in the trusted
  instructions. Ordinary message bodies stay untrusted, and the checks are
  advisory against the same OS user. See [delegated decisions](delegated-decisions.md).
- **Subscription first:** turns bill each tool's own login (your Claude,
  ChatGPT or OpenCode subscription). API keys in your shell never reach an
  agent; a key is used only if your config says so.
- **mdkg is the system of record:** the AGENTS.md block, the mdkg-workflow
  skill and the MCP server's instructions tell every agent, whatever its
  harness, to use mdkg's nodes, claims and mail for this project's work,
  not its own task list or messaging. Each turn starts and ends with
  `mdkg msg inbox`; every edit is claimed first.

Claude Code, Codex and OpenCode are first-class harnesses. Workers use
`mode: "auto"` by default: Claude Code and Codex use native automatic
approval modes; OpenCode keeps your own permissions. Reviewers remain
read-only. mdkg supplies worktrees and coordination; sandboxing and approvals
come from the harness and your settings. [Running agents](agents.md) explains
setup and overrides, with links to the launch and turn regression tests.

## Search and the derived index

`mdkg search` ranks nodes by full-text relevance (BM25) over their ids,
titles, fields and bodies. Every word must match the start of a word:

```console
$ mdkg search "csv reader"
count: 1
task-01a173c2-bc3f-718a-af20-3adfa30a85b3 | task | todo/p9 | Write the CSV reader | .mdkg/work/task-write-the-csv-reader.md
```

The index behind it is a SQLite file in the git folder, one per worktree.
It's derived: every read brings it up to date from the files, by their
hashes, and `mdkg index rebuild` makes it again from scratch. It's never
committed, so it can't conflict.

The same index answers graph questions:
- `mdkg graph ready` lists work whose blockers are done;
- `mdkg graph refs` and `graph backrefs` follow references;
- `mdkg graph critical-path` finds a goal's longest chain;
- `mdkg pack` prints the most relevant nodes for a question as one
  markdown context, within a token budget.

## Where to go next

- [The command reference](commands.md), generated from `--help`.
- [The file format](format-0.7.md), versioned and frozen for 0.7.
- [Running agents](agents.md).
- [Migrating from 0.6](migrating-from-0.6.md).
