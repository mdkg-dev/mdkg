# Changelog

## 0.7.0

mdkg 0.7.0 is a rewrite in Rust: one binary, the same `mdkg` name on every
package manager, and a new file format (0.7) that 0.6.x graphs migrate to.
It is MIT-licensed, like 0.6.x. Requires Git 2.41 or later.

### Upgrading from 0.6.x

Install 0.7.0 from any channel (see the README), then follow
[docs/migrating-from-0.6.md](docs/migrating-from-0.6.md):
1. `mdkg migrate` shows a dry run and a plan hash.
2. Applying that plan backs up the graph first, rewrites it in one commit,
   and can be rolled back. The config lists the projections the
   repository already has (its `AGENTS.md` block, MCP configs and skill
   mirrors), so `install` keeps writing just those.

A migrated repository needs 0.7.0 everywhere: 0.6.x refuses a 0.7 graph
rather than misread it. For one release, the busiest 0.6.0 commands keep
working under their old names, each printing its new one. They go away in
0.8.

### The file format (0.7)

- **Ids:** every node's id is its type plus a UUIDv7 (RFC 9562), such as
  `task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b`.
  - Any machine or agent mints one locally, so ids never collide across
    branches, and there is no counter or `fix ids`.
  - Migrated nodes keep their old id as a read-only `legacy_id`, which
    commands still resolve.
- **Handles:** commands take a full id, a unique id prefix (like a git
  short SHA), a `legacy_id`, or words from the title. An ambiguous handle
  lists the matches and exits 7. `mdkg retype` changes a node's type and
  every ref to it.
- **Refs:** within a graph, refs are plain ids. Across graphs they are
  `mdkg://<graph_id>/<id>`, optionally pinned to a revision with
  `@sha256:<hash>`. Workspace-qualified ids (`root:task-19`) are gone.
- **Files:** file names are `<type>-<slug>.md`, with the id only in the
  frontmatter, so a file can be renamed for cleanup at any time.
- **Timestamps:** `created` and `updated` are RFC 3339 UTC timestamps to
  the millisecond.
- **Config:** `.mdkg/config.json` carries the graph's own `graph_id`,
  `format_version: "0.7"` and `schema_version: 3`.
- **Extension keys:** they are `x-<namespace>.<field>`. 0.6.0's `x_` keys
  are read as legacy keys and rewritten by migrate.
- **The spec:** [docs/format-0.7.md](docs/format-0.7.md) is versioned and
  frozen for 0.7.

### Knowledge and coordination

- **Knowledge in git:** goals, decisions, designs, rules and skills. Work
  definitions and their final states are local by default, or committed
  when shared work is selected.
- **Coordination in a local store**, in the repository's git folder and
  shared by all of its worktrees, never committed: claims and leases
  (`claim`, `release`, `renew`), temporary statuses (`update --status
  review|blocked`), `done`, `sync` and the merge queue in `status`. 0.6.0
  wrote these into tracked files.
- **Work location:** `mdkg init --work local|shared` chooses where work lives
  (`work.location: "local"|"git"`). Local is the default for new graphs,
  keeping tasks, bugs, spikes, tests and checkpoints beside the store in
  the same file format. Shared work commits those definitions. The older
  `work.committed: "none"|"definitions"` config values remain readable
  aliases; existing graphs without either setting keep work in git.
- **Local scratch:** `mdkg scratch path` returns an actor's scratch folder;
  `--shared` returns the folder shared by the clone's worktrees. Both live
  beside the coordinator, outside committed graph files.
- **A hash-linked log:** every change to the coordinator is a record in
  it.
  - `mdkg anchor` writes the log's head to `.mdkg/anchors/` for you to
    commit (`goal done` writes one too).
  - `mdkg export --work` writes the whole chain as a JSONL bundle. Message
    bodies stay out; the bundle holds only their digests.
  - `mdkg verify` checks a bundle offline. With no bundle, it checks the
    repository's chain, and its committed anchor when there is one.
- **Store safety:** a store upgrade copies the store first; `store
  snapshots` and `store restore` bring a copy back.
- **Graph checkout identity:** clone-local records tie the root graph and
  its declared local workspaces/subgraphs to the dedicated checkout's Git
  worktree identity. Moving that checkout preserves its identity; missing
  or conflicting records cause a refusal with repair guidance.

### Agents

- **MCP:** `mdkg mcp` serves the graph, leases and mail to coding agents
  over stdio MCP. Message bodies are labelled as untrusted data.
- **Rooms and mail:** `msg send`, `inbox`, `wait`, `ack` and `thread`, and
  `room create`, `join`, `leave` and `list`.
- **Install:** every file agents read from the project's root is an
  opt-in projection, listed in `.mdkg/config.json` under `projections`:
  the `AGENTS.md` block, the MCP config for each of Claude Code, Codex
  and OpenCode, each skill mirror, and each harness's hooks (canonical in
  `.mdkg/hooks/`). `mdkg install` writes exactly those; `--enable` and
  `--disable` change the list, and disabling removes only mdkg's part.
  `install --undo` reverts it all, the list included.
- **Waking agents:**
  - `mdkg agents up --once` wakes an agent for one turn with its unread mail.
  - `mdkg agents up` keeps agents woken on mail. Each wake is one batch
    turn, and a loop guard pauses a thread after 10 automatic turns in an
    hour. `agents status` shows the agents, their policies and paused
    threads, and `agents resume` lifts a pause.
  - Every harness takes either role, worker or reviewer. Agents run in
    your own environment: mdkg adds no sandbox, environment clearing or
    secret scanning of its own, and leaves containment to each harness.
  - Worktree and branch names are configurable.
  - Workers own their Git changes and commits; the launcher does not
    stage or commit their work.
  - `agents add`, `edit` and `rm` commit their own definition change and
    report its commit, preserving unrelated staged work. A no-op keeps HEAD.
  - Codex and Claude workers default to the harness's native automatic
    approval handling. OpenCode workers retain the user's permission
    defaults. Supported `sandbox` and `approvals` overrides remain
    harness-specific; reviewer definitions cannot widen their read-only role.
  - OpenCode workers check the native configuration before every actual
    launch, including resumes and retries. The preflight requires OpenCode 2.1.0
    and an enabled `build` agent; an unsupported or failed probe refuses
    the turn and leaves its mail unread.
- **Delegated approval:** a delegate configured in user or machine-local
  settings can relay approval for a fixed action on an exact committed
  node version. The launcher revalidates it before each turn; message text
  stays untrusted. `msg decisions` lists the durable audit. This is advisory
  against processes running as the same OS user, and cannot authorize
  pushes, publication, tags, credentials, data deletion, security-setting
  changes or wider permissions.
  See [delegated decisions](docs/delegated-decisions.md) for the action list,
  configuration and limits.

### Setup and skills

- **`mdkg config show [--effective] [--json]`** displays merged settings,
  their source layers and paths, redacting recognized credential fields
  and patterns. It does not write files or open the coordinator; effective
  display is the default.
  Displayed user or machine-local values for agents, orchestration and
  projections are not yet applied by runtime commands; the display's
  notice identifies this limit.
- **`mdkg init`** starts a graph and writes only inside `.mdkg`:
  - `.mdkg/config.json`: a new `graph_id`, explicit local/shared work defaults, each agent's
    branch and worktree patterns, and an empty `projections` list;
  - `.mdkg/README.md`: the setup commands, opt-in projections and diagrams
    of the design, work and coordination layers;
  - one skill, `mdkg-workflow` (ground, claim, work, verify, close).

  It says how to opt into the agents' files (`install --enable`).
  `--graph-only` skips the skill, keeping the config and setup README.
  Init leaves `.gitignore` alone; creating an agent worktree adds its
  ignore entry. In `init --json`, `mcp_command` is
  always null and `agents` always false (install writes those files
  now), and no change is `removed`.
  Templates are built in (`.mdkg/templates/` holds optional overrides),
  and init seeds no nodes or event log.
- **Safe to rerun:**
  - on its own graph, init changes nothing, not a byte;
  - it refuses a 0.6 graph (that's `migrate`), a conflicting profile or
    name, a config it can't read, and a `.mdkg/` holding files it didn't
    write;
  - an init that stopped picks up only its own output.
- **`mdkg doctor`** checks:
  - this build and whether `mdkg` on PATH is it, or a 0.6.x install;
  - git's version;
  - the config's 0.7 keys and the graph;
  - the coordinator store's version, read-only, never upgrading it;
  - the skill mirrors;
  - for the enabled projections only: the program each MCP config
    starts, the `AGENTS.md` block, and the hooks;
  - the `mdkg-workflow` skill's version.
  `--strict` also fails on warnings.
- **`mdkg skills`** lists, shows and validates skills:
  - `skills sync` writes their mirrors, including the mirrors'
    `.mdkg-managed.json`;
  - `skills add mdkg-workflow` installs the shipped skill into a graph
    that doesn't have it.

  `mdkg skill` is still accepted.

### Test selection

- **`mdkg affected`** recommends Go packages, Rust crates and configured
  suites from committed changes: `mdkg affected --base <rev> [--head HEAD]
  [--json]`. Global files, unmapped paths and uncertain analysis request a
  full run. It does not execute tests or include uncommitted changes. Run
  the full suite before merge; see [docs/affected.md](docs/affected.md)
  for rules and limits.

### Search and the derived index

- **Search:** `mdkg search` is full-text search (SQLite FTS5, ranked with
  BM25) over ids, titles, fields and bodies, with field filters, scores
  and snippets.
- **The index:** it is derived and rebuildable. Each worktree has its own,
  kept in the repository's git folder and never committed. `mdkg index
  rebuild|status` manages it.
- **Graph walks:** `mdkg graph refs`, `backrefs`, `ready` and
  `critical-path`. `next` skips items with unfinished blockers.
- **Packs:** `mdkg pack` writes a markdown context pack, ranked from its
  seed nodes (`standard` or `concise`, within a token budget). MCP has
  `search` and `pack` tools.

### Removed from 0.6.x

These had little or no use in 0.6.x graphs, and are not ported:
- `fix` and `handoff`, and the workspace commands;
- TOON and XML output; JSON (`--json`) is the one machine format;
- `manifest` and its `spec` alias;
- `graph clone`, `fork`, `reconcile` and `recover`, and `bundle import`;
- the project database, and the JSON plus SQLite index caches. The
  derived index replaces them.

Two more go with migrate's help:
- **`capability search`:** migrate rewrites goal gates that call it to
  `mdkg search`.
- **`upgrade`:** `init`, `doctor` and `migrate` replace it.

### Platforms

- **Prebuilt binaries:** macOS arm64 and x86_64, and Linux x86_64 and
  arm64, each with glibc and musl.
- **Channels:** Homebrew (`mdkg-dev/tap`), npm, crates.io, PyPI and a
  shell installer, with SHA-256 checksums and GitHub build attestations
  for every archive.
- **Windows** and macOS notarization are planned for 0.7.1. Until then,
  Windows users run the Linux build under WSL.
- **Git:** 2.41 or later.

### Planned for 0.7.1

- Deterministic turn-end receipts.
- Durable supervisor progress, with crash recovery through `agents resume`.
- Applying user and machine-local agent, orchestration and projection
  settings to runtime commands, including `agents up`, and `config set`.
- Native Windows builds.
- `mdkg viz` and derived/CocoIndex search.

### Known limits

- **A hard link between the two skill mirrors:** when a file in one mirror
  is a hard link to its copy in the other, `install --disable` of either
  mirror is refused while the other stays enabled: install can't tell the
  two names apart, so it won't remove one. Disable both together, or
  replace the link with a copy and then disable the mirror.
