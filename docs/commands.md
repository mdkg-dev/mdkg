# Command reference

Every command's `--help`, generated from the CLI itself by
`scripts/commands-doc.sh`; a test fails when this file is stale.
[Concepts](concepts.md) explains the ideas behind them. Add `--json`
to a command for one machine-readable envelope, and see its shape
with `mdkg schema <command>`.

## `mdkg`

```text
Local-first project memory for humans and agents

Usage: mdkg [OPTIONS] <COMMAND>

Commands:
  list       List nodes
  show       Show one node
  search     Search ids, titles, fields and bodies, best match first
  index      Rebuild the derived index, or show its state
  graph      Query the graph: refs, backrefs, the ready set, a goal's critical path
  scratch    Locate local scratch shared by the repository's worktrees
  work       Change where work definitions live
  node       Edit a node's body when the graph lives on the graph ref
  config     Inspect configuration layers and their sources
  pack       Build a markdown context pack around a node or a query, in a token budget
  validate   Validate the graph
  new        Create a node
  done       Finish a work item: record its version and write `status: done`
  update     Set a temporary status (review or blocked), or clear it
  next       Show the next work item to pick up (the full record)
  goal       Show and change goals: current, next, activate, pause, done
  format     Report files whose frontmatter isn't canonical; `--write` fixes them
  retype     Change a node's type: its id and every ref to it (format 0.7)
  claim      Claim a node, file, folder, role or turn
  holds      Check whether you hold a lease covering a scope
  lease      Ask about leases others hold: request
  renew      Renew every active lease you hold
  release    Give up your lease without finishing; a lease that already ended is fine
  status     Show leases, the settlement queue and your unacked mail
  sync       Stack this worktree on unmerged versions it lacks
  abandon    Drop a changeset whose branch won't merge, and work built on it
  mcp        Serve the graph and leases to coding agents over stdio MCP
  entry      The ledger: entries on a node (findings, questions, decisions, issues, reviews, requirements) and its gate
  req        The requirements in a node's body, with their ledger states
  msg        Send and read messages: send, inbox, wait, ack, thread, import
  channel    Manage channels: create, join, leave, list
  agents     Manage definitions and run agents: add, edit, rm, up, status, resume
  auth       The API key fallback's helper: `auth key <provider>`
  hooks      What an agent harness's hooks run: one router (`hooks run`) for the mail notice, the stop reminders, the idle wake and the lease guards
  install    Set up Claude Code, Codex and OpenCode for this project
  store      List or restore copies of the coordinator store
  anchor     Write the record chain's head hash to an anchor file
  export     Export work state as a bundle anyone can verify
  verify     Verify a work bundle, or this repository's store, against an anchor
  migrate    Bring a 0.6 graph onto format 0.7
  version    Show which build this is: version, commit, dirty tree, build date
  init       Start a graph here: config, setup README and the mdkg-workflow skill
  doctor     Check this setup: build, PATH, git, config, graph, store, mirrors, MCP, agents, hooks
  skills     List, show, validate, add and sync skills in .mdkg/skills
  affected   The tests a change needs: a selective run, or a full one when in doubt
  schema     Print the JSON Schema of a command's output
  serve      Serve this graph's coordinator over its socket and, when asked, TLS; `serve status` tells whether it runs
  git-serve  The receiving side of git between machines: the ssh forced command `authorized_keys` names for a machine's key, run by sshd as the repository owner
  help       Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

### `mdkg list`

```text
List nodes

Usage: mdkg list [OPTIONS]

Options:
      --ws <WS>
          Workspace alias (`all` means every workspace)

      --type <NODE_TYPE>
          Comma-separated node types; nodes of any of them match

      --status <STATUS>
          Comma-separated statuses; nodes with any of them match

      --tags <TAGS>
          Comma-separated tags

      --tags-mode <TAGS_MODE>
          Whether nodes need any or all of the tags

          [possible values: any, all]

      --fields <FIELDS>
          Comma-separated node fields, or `all` (default: id, type, title, status, priority, path)

      --json
          Print JSON

      --epic <EPIC>
          Only nodes in this epic (an id, id prefix or title fragment)

      --priority <PRIORITY>
          Only nodes with this priority

      --blocked
          Only blocked nodes

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg list --type task --status todo
  mdkg list --blocked --json
```

### `mdkg show`

```text
Show one node

Usage: mdkg show [OPTIONS] <ID>

Arguments:
  <ID>
          The node: a full id, a unique id prefix or a title fragment

Options:
      --ws <WS>
          Workspace hint

      --meta
          Omit the body

      --fields <FIELDS>
          Comma-separated node fields, or `all` (the default)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg show <id>
  mdkg show <id> --meta --json
```

### `mdkg search`

```text
Search ids, titles, fields and bodies, best match first

Usage: mdkg search [OPTIONS] <QUERY>...

Arguments:
  <QUERY>...
          Words; every word must match. Unquoted words are joined

Options:
      --ws <WS>
          Workspace alias (`all` means every workspace)

      --type <NODE_TYPE>
          Comma-separated node types; nodes of any of them match

      --status <STATUS>
          Comma-separated statuses; nodes with any of them match

      --tags <TAGS>
          Comma-separated tags

      --tags-mode <TAGS_MODE>
          Whether nodes need any or all of the tags

          [possible values: any, all]

      --fields <FIELDS>
          Comma-separated node fields, or `all` (default: id, type, title, status, priority, path)

      --json
          Print JSON

      --owners <OWNERS>
          Comma-separated owners; nodes with any of them match

      --limit <LIMIT>
          Maximum results (default 50)

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg search lease renew
  mdkg search lease renew --limit 10 --json
```

### `mdkg index`

```text
Rebuild the derived index, or show its state

Usage: mdkg index [OPTIONS] <COMMAND>

Commands:
  rebuild  Drop the derived index and build it again from the graph
  status   Bring the index up to date and say where it is and what it holds
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg index status
  mdkg index rebuild
```

#### `mdkg index rebuild`

```text
Drop the derived index and build it again from the graph

Usage: mdkg index rebuild [OPTIONS]

Options:
      --json
          Print JSON

      --verbose
          Also print how long it took, on stderr

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg index rebuild
  mdkg index rebuild --verbose
```

#### `mdkg index status`

```text
Bring the index up to date and say where it is and what it holds

Usage: mdkg index status [OPTIONS]

Options:
      --json
          Print JSON

      --verbose
          Also print how long it took, on stderr

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg index status
  mdkg index status --json
```

### `mdkg graph`

```text
Query the graph: refs, backrefs, the ready set, a goal's critical path

Usage: mdkg graph [OPTIONS] <COMMAND>

Commands:
  refs           List the nodes a node points at (its edge fields and refs)
  backrefs       List the nodes that point at a node
  ready          List work items ready to start: a ready status and nothing open blocking them, in the order `next` picks
  remote         Approve graph.remote's destination for graph pushes (a person, outside any agent turn)
  sync           Pull the graph ref from graph.remote and, once the destination is approved, push it: one explicit refspec for remoteRef, never a code branch. Unapproved, it neither pulls nor pushes, and says how to approve (`graph remote approve`). A person runs it, outside any agent turn; `agents up` pulls and pushes on its own timers
  conflicts      List the paths a graph sync merge took the remote's version of
  resolve        Settle sync conflicts: publish the local or the remote version of each path on the graph ref, in one commit, and clear their entries
  critical-path  Show the longest chain of unfinished work a goal waits on
  help           Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg graph ready --limit 5
  mdkg graph backrefs <id>
  mdkg graph critical-path <goal-id>
```

#### `mdkg graph refs`

```text
List the nodes a node points at (its edge fields and refs)

Usage: mdkg graph refs [OPTIONS] <NODE>

Arguments:
  <NODE>
          A full id, a unique id prefix or a title fragment

Options:
      --ws <WS>
          Workspace hint

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg graph refs <id>
  mdkg graph refs <id> --json
```

#### `mdkg graph backrefs`

```text
List the nodes that point at a node

Usage: mdkg graph backrefs [OPTIONS] <NODE>

Arguments:
  <NODE>
          A full id, a unique id prefix or a title fragment

Options:
      --ws <WS>
          Workspace hint

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg graph backrefs <id>
```

#### `mdkg graph ready`

```text
List work items ready to start: a ready status and nothing open blocking them, in the order `next` picks

Usage: mdkg graph ready [OPTIONS]

Options:
      --limit <LIMIT>
          At most this many (default: all)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg graph ready
  mdkg graph ready --limit 5 --json
```

#### `mdkg graph remote`

```text
Approve graph.remote's destination for graph pushes (a person, outside any agent turn)

Usage: mdkg graph remote [OPTIONS] <COMMAND>

Commands:
  approve  Approve graph.remote's destination, so graph pushes may go there (G23). Shows the remote, its remoteRef and its effective fetch and push destinations, and pins them in <git common dir>/mdkg/graph/remote.json. Only a person runs it, outside any agent turn; configuration alone never pushes. Run it again after changing the remote's URL or remoteRef
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg graph remote approve`

```text
Approve graph.remote's destination, so graph pushes may go there (G23). Shows the remote, its remoteRef and its effective fetch and push destinations, and pins them in <git common dir>/mdkg/graph/remote.json. Only a person runs it, outside any agent turn; configuration alone never pushes. Run it again after changing the remote's URL or remoteRef

Usage: mdkg graph remote approve [OPTIONS]

Options:
      --dry-run
          Show what would be pinned; pin nothing

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg graph remote approve
  mdkg graph remote approve --dry-run --json
```

#### `mdkg graph sync`

```text
Pull the graph ref from graph.remote and, once the destination is approved, push it: one explicit refspec for remoteRef, never a code branch. Unapproved, it neither pulls nor pushes, and says how to approve (`graph remote approve`). A person runs it, outside any agent turn; `agents up` pulls and pushes on its own timers

Usage: mdkg graph sync [OPTIONS]

Options:
      --no-push
          Pull only

      --accept-rewrite
          Accept a remote graph ref that was rewritten (it no longer descends from the tip this clone last accepted)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg graph sync
  mdkg graph sync --no-push --json
  mdkg graph sync --accept-rewrite
```

#### `mdkg graph conflicts`

```text
List the paths a graph sync merge took the remote's version of

Usage: mdkg graph conflicts [OPTIONS]

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg graph conflicts
  mdkg graph conflicts --json
```

#### `mdkg graph resolve`

```text
Settle sync conflicts: publish the local or the remote version of each path on the graph ref, in one commit, and clear their entries

Usage: mdkg graph resolve [OPTIONS] --take <TAKE> <PATHS>...

Arguments:
  <PATHS>...
          The conflicted paths, as `graph conflicts` lists them. Paths of one change (a rename's old and new path) settle together

Options:
      --take <TAKE>
          Which version to keep

          [possible values: local, remote]

      --merge <MERGE>
          The merge commit whose conflicts to settle (at least 7 hex digits), as `graph conflicts` lists it. Needed when a path has more than one open conflict

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg graph resolve .mdkg/work/fix-login.md --take local
  mdkg graph resolve .mdkg/work/old.md .mdkg/work/new.md --take local
```

#### `mdkg graph critical-path`

```text
Show the longest chain of unfinished work a goal waits on

Usage: mdkg graph critical-path [OPTIONS] [GOAL]

Arguments:
  [GOAL]
          The goal (default: the active goal)

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg graph critical-path
  mdkg graph critical-path <goal-id>
```

### `mdkg scratch`

```text
Locate local scratch shared by the repository's worktrees

Usage: mdkg scratch [OPTIONS] <COMMAND>

Commands:
  path  Print the actor's local scratch path, without creating it
  help  Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg scratch path
  mdkg scratch path --shared
```

#### `mdkg scratch path`

```text
Print the actor's local scratch path, without creating it

Usage: mdkg scratch path [OPTIONS]

Options:
      --shared
          The repository-wide shared folder instead of the actor's folder

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg scratch path
  mdkg scratch path --shared --json
```

### `mdkg work`

```text
Change where work definitions live

Usage: mdkg work [OPTIONS] <COMMAND>

Commands:
  localize  Move work definitions beside the store, preserving their ids
  help      Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg work localize`

```text
Move work definitions beside the store, preserving their ids

Usage: mdkg work localize [OPTIONS] <--dry-run|--apply <PLAN_HASH>>

Options:
      --dry-run
          Check references and print the conversion plan without writing

      --apply <PLAN_HASH>
          Apply the exact plan from --dry-run; repeat to finish an interruption

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg work localize --dry-run
  mdkg work localize --apply sha256:<plan-hash>
```

### `mdkg node`

```text
Edit a node's body when the graph lives on the graph ref

Usage: mdkg node [OPTIONS] <COMMAND>

Commands:
  edit  Take the node's lease and write an editable copy outside `.git`: `--dir`, else `<worktree>/.mdkg-edit/<graph path>`. An unsaved copy is kept, never overwritten
  save  Publish the edited copy under the node's lease; refused as stale when the graph changed the node since `node edit`
  help  Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg node edit <id>
  mdkg node save <id>
```

#### `mdkg node edit`

```text
Take the node's lease and write an editable copy outside `.git`: `--dir`, else `<worktree>/.mdkg-edit/<graph path>`. An unsaved copy is kept, never overwritten

Usage: mdkg node edit [OPTIONS] <ID>

Arguments:
  <ID>
          The node: an id, a legacy id, a unique prefix or a title

Options:
      --dir <FOLDER>
          The folder to put the copy under

      --discard
          Start over from the graph's bytes, replacing an unsaved copy

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg node save`

```text
Publish the edited copy under the node's lease; refused as stale when the graph changed the node since `node edit`

Usage: mdkg node save [OPTIONS] <ID>

Arguments:
  <ID>
          The node: an id, a legacy id, a unique prefix or a title

Options:
      --dir <FOLDER>
          The folder `node edit --dir` put the copy under

      --stdin
          Read the node's new content from stdin instead of the copy

      --preimage <BLOB>
          With `--stdin`: the graph blob the content was based on (as `node edit` printed it); else the record of your `node edit`

      --request-id <ID>
          A retry passes the same id and gets the original receipt (one is minted otherwise, and printed)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

### `mdkg config`

```text
Inspect configuration layers and their sources

Usage: mdkg config [OPTIONS] <COMMAND>

Commands:
  show   Show merged settings and each value's source, without changing files
  set    Change one key of the committed project config: in ref mode a commit on the graph ref, in checkout mode a commit of `.mdkg/config.json` where the committed config is read
  unset  Remove one key from the committed project config, so its default applies; committed as `config set` commits
  help   Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg config show`

```text
Show merged settings and each value's source, without changing files

Usage: mdkg config show [OPTIONS]

Options:
      --effective
          Show the effective view (also the default without this flag)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg config show
  mdkg config show --effective --json
```

#### `mdkg config set`

```text
Change one key of the committed project config: in ref mode a commit on the graph ref, in checkout mode a commit of `.mdkg/config.json` where the committed config is read.

The value is JSON (a bare word is taken as a string). The key must be one this build knows, and the changed config must pass the rules `doctor` and `validate` apply, or nothing changes.

Usage: mdkg config set [OPTIONS] <KEY> <VALUE>

Arguments:
  <KEY>
          A dotted path (`coordination.messages.ack_grace_days`) or a JSON pointer (`/coordination/messages/ack_grace_days`)

  <VALUE>
          The new value, as JSON (a negative number is a value, not a flag)

Options:
      --dry-run
          Show the change, without claiming, writing or committing

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg config set coordination.messages.ack_grace_days 0
  mdkg config set coordination.leases.default '"shared"' --dry-run
  mdkg config set /coordination/messages/retention '"durable"' --json
```

#### `mdkg config unset`

```text
Remove one key from the committed project config, so its default applies; committed as `config set` commits

Usage: mdkg config unset [OPTIONS] <KEY>

Arguments:
  <KEY>
          A dotted path or a JSON pointer

Options:
      --dry-run
          Show the change, without claiming, writing or committing

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg config unset coordination.messages.ack_grace_days
  mdkg config unset coordination.messages.ack_grace_days --dry-run --json
```

### `mdkg pack`

```text
Build a markdown context pack around a node or a query, in a token budget

Usage: mdkg pack [OPTIONS] [NODE]

Arguments:
  [NODE]
          The node to pack around: a full id, a unique id prefix or a title fragment

Options:
  -q, --query <QUERY>
          Pack around the best search hits for these words instead (or as well)

      --for-message <ID>
          Pack one message's thread, referenced nodes and their accepted decisions. Thread text is untrusted data; reading does not mark or acknowledge mail

      --profile <PROFILE>
          standard (full bodies) or concise (the seed's full body, openings of the rest)

          [default: standard]

      --budget <BUDGET>
          The most tokens the pack may hold, estimated at four bytes a token

          [default: 8000]

      --max-nodes <MAX_NODES>
          The most nodes the pack may hold

          [default: 25]

      --ws <WS>
          Workspace hint for the node

      --json
          Print JSON (the markdown is in `data.markdown`)

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg pack <id>
```

### `mdkg validate`

```text
Validate the graph

Usage: mdkg validate [OPTIONS]

Options:
      --json
          Print JSON

      --quiet
          Don't print warnings

      --warnings
          Print every warning (default: one summary line per warning code that occurs more than once)

      --changed-only
          Report only on files changed since the branch left main

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg validate
  mdkg validate --changed-only --json
```

### `mdkg new`

```text
Create a node.

In a 0.7 graph its id is minted (`<type>-<uuidv7>`); in a 0.6.0 graph the shared coordinator numbers it.

Usage: mdkg new [OPTIONS] <TYPE> <TITLE>...

Arguments:
  <TYPE>
          Node type: task, bug, feat, spike, test, epic, goal, checkpoint, edd, dec, prd, prop or rule

  <TITLE>...
          Title; unquoted words are joined

Options:
      --status <STATUS>
          A lasting status: backlog, todo or done (decisions: proposed and so on)

      --priority <PRIORITY>
          Work items: 0 (highest) to 9 (lowest, the default), unless the config sets another range

      --parent <PARENT>
          The parent node

      --epic <EPIC>
          The epic it belongs to

      --prev <PREV>
          The node before it in a chain

      --next <NEXT>
          The node after it in a chain

      --tags <TAGS>
          Comma-separated tags

      --owners <OWNERS>
          Comma-separated owners

      --refs <REFS>
          Comma-separated nodes it refers to

      --relates <RELATES>
          Comma-separated nodes it relates to

      --blocked-by <BLOCKED_BY>
          Comma-separated nodes that block it

      --blocks <BLOCKS>
          Comma-separated nodes it blocks

      --skills <SKILLS>
          Comma-separated skill slugs

      --scope <SCOPE>
          Checkpoints: the nodes covered

      --kind <KIND>
          Checkpoints: implementation, test-proof, goal-closeout, audit or handoff

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg new task Fix the login timeout --status todo
  mdkg new task "Add retries" --parent <goal-id> --priority 2 --json
```

### `mdkg done`

```text
Finish a work item: record its version and write `status: done`

Usage: mdkg done [OPTIONS] <TARGET>

Arguments:
  <TARGET>
          Node id

Options:
      --checkpoint
          Also create a checkpoint node covering it

      --residual <REASON>
          Accept every gate failure a residual may record, for this reason, before finishing: a person's (outside turns), or a coordinator's on a node no live lease holds

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg done <id>
  mdkg done <id> --checkpoint
```

### `mdkg update`

```text
Set a temporary status (review or blocked), or clear it

Usage: mdkg update [OPTIONS] <TARGET>

Arguments:
  <TARGET>
          Node id; with --clear, also the path `status` lists for a state whose node isn't in this worktree (yours only)

Options:
      --status <STATUS>
          review or blocked

      --clear
          Clear the temporary status

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg update <id> --status review
  mdkg update <id> --clear
```

### `mdkg next`

```text
Show the next work item to pick up (the full record)

Usage: mdkg next [OPTIONS] [ID]

Arguments:
  [ID]
          Follow this node's `next` edge instead

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg next
  mdkg next --json
```

### `mdkg goal`

```text
Show and change goals: current, next, activate, pause, done

Usage: mdkg goal [OPTIONS] <COMMAND>

Commands:
  current   Show the active goal and any leases inside it
  next      Show the next work item in a goal (default: the active goal)
  activate  Make a goal the active one, pausing any other
  pause     Pause a goal
  done      Mark a goal achieved
  help      Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg goal current
  mdkg goal activate <goal-id>
```

#### `mdkg goal current`

```text
Show the active goal and any leases inside it

Usage: mdkg goal current [OPTIONS]

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg goal current
```

#### `mdkg goal next`

```text
Show the next work item in a goal (default: the active goal)

Usage: mdkg goal next [OPTIONS] [GOAL]

Arguments:
  [GOAL]
          The goal's id

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg goal next
  mdkg goal next <goal-id> --json
```

#### `mdkg goal activate`

```text
Make a goal the active one, pausing any other

Usage: mdkg goal activate [OPTIONS] <GOAL>

Arguments:
  <GOAL>
          The goal's id

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg goal activate <goal-id>
```

#### `mdkg goal pause`

```text
Pause a goal

Usage: mdkg goal pause [OPTIONS] <GOAL>

Arguments:
  <GOAL>
          The goal's id

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg goal pause <goal-id>
```

#### `mdkg goal done`

```text
Mark a goal achieved

Usage: mdkg goal done [OPTIONS] <GOAL>

Arguments:
  <GOAL>
          The goal's id

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg goal done <goal-id>
```

### `mdkg format`

```text
Report files whose frontmatter isn't canonical; `--write` fixes them

Usage: mdkg format [OPTIONS] [IDS]...

Arguments:
  [IDS]...
          Node ids (default: every node)

Options:
      --write
          Rewrite the files (skips files leased by others)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg format
  mdkg format --write
```

### `mdkg retype`

```text
Change a node's type: its id and every ref to it (format 0.7)

Usage: mdkg retype [OPTIONS] <TARGET> <NEW_TYPE>

Arguments:
  <TARGET>
          The node: an id, legacy id or handle

  <NEW_TYPE>
          The new type

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg retype <id> bug
```

### `mdkg claim`

```text
Claim a node, file, folder, role or turn.

A role is `role:<name>` (its mailbox), a turn `run:<name>`. Claiming again renews your lease, except a turn, which `renew` extends.

Usage: mdkg claim [OPTIONS] <TARGET>

Arguments:
  <TARGET>
          Node id, file or folder path, `role:<name>` or `run:<name>` (names: lowercase a-z, 0-9, `.`, `_`, `-` and `:`, up to 120). More targets may follow: all are claimed or none, one lease each, and the options apply to every one. Beside a lease authority, several targets in one call are refused (claim each on its own); one target then is still fine

Options:
      --new
          A path you're about to create: lease it as a path, without the missing-file warning.

          A path no worktree has yet is leased as a folder would be; your lease keeps that mode until you release it.

      --exclusive
          Keep a path the config shares to yourself for this claim

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg claim <id>
  mdkg claim src/a.rs src/b.rs
  mdkg claim --new src/new_module.rs
  mdkg claim role:reviewer
```

### `mdkg holds`

```text
Check whether you hold a lease covering a scope.

The scope is relative to the repository root. Read-only, as a lease authority asks it.

Usage: mdkg holds [OPTIONS] <SCOPE>

Arguments:
  <SCOPE>
          The scope

Options:
      --node <NODE>
          The node the scope is (its qualified id): held only if nobody holds the node under another path

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg holds src/lib.rs
```

### `mdkg lease`

```text
Ask about leases others hold: request

Usage: mdkg lease [OPTIONS] <COMMAND>

Commands:
  request  Ask the actor holding a node or path to give it up: direct mail of kind `question` to the holder, copied to whoever you report to
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg lease request`

```text
Ask the actor holding a node or path to give it up: direct mail of kind `question` to the holder, copied to whoever you report to.

The holder answers with a normal reply, and releasing stays its own act: nothing is released for it. The mail's context is `lease:<lease id>:<you>`, one conversation per lease and asker (asking again adds to it), and a data part names the path, you and the reason.

Usage: mdkg lease request [OPTIONS] --reason <REASON> <TARGET>

Arguments:
  <TARGET>
          The node id or path you need, as you'd claim it

Options:
      --reason <REASON>
          Why you need it, for the holder

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg lease request src/parser.rs --reason "a security fix needs one commit here"
```

### `mdkg renew`

```text
Renew every active lease you hold

Usage: mdkg renew [OPTIONS]

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg renew
```

### `mdkg release`

```text
Give up your lease without finishing; a lease that already ended is fine.

`--all` releases every active lease you hold: run it once your head has merged and you've committed everything. Each lease is released on its own, so running it again finishes a run that stopped part way. `--clean` releases only the path leases with nothing uncommitted in their paths (ignored files, hidden index entries and submodules count as open work) that weren't just claimed. The launcher runs it as each agent after every turn. `--committed` releases, right away, the path leases you've committed to since claiming them and left with nothing uncommitted, and lists the ones it kept and why: run it after a commit to let others in.

Usage: mdkg release [OPTIONS] [TARGET]

Arguments:
  [TARGET]
          Node id, file or folder path, `role:<name>`, `run:<name>`, or lease id. More targets may follow: all are released or none, in one transaction. Beside a lease authority, several targets in one call are refused (release each on its own)

Options:
      --all
          Release every active lease you hold

      --clean
          Release your path leases whose paths have no uncommitted or staged changes in this worktree and that you didn't claim in the last 30 minutes: the leases `renew` lets lapse. Never a node, role or turn lease

      --committed
          Release your path leases whose paths have no uncommitted, staged or ignored changes in this worktree and that a commit of yours here touched since you claimed them, with no 30-minute wait. Lists each path lease it kept, and why. Only in your own worktree, and only this repository's leases (not a lease authority's)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg release <id>
  mdkg release src/lib.rs
  mdkg release --all
  mdkg release --clean
  mdkg release --committed
```

### `mdkg status`

```text
Show leases, the settlement queue and your unacked mail.

Every active lease, yours first; --mine lists only yours. Expired leases are yours, everyone's with --all. JSON always lists everyone's active leases, marking yours.

Usage: mdkg status [OPTIONS]

Options:
      --all
          List everyone's expired leases too, not only yours

      --mine
          List only your own active leases (text; JSON lists everyone's)

      --no-observe
          Don't record what main holds: write nothing to the store (a live view's poll). Settlement shown is still current

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg status
  mdkg status --mine
  mdkg status --all
```

### `mdkg sync`

```text
Stack this worktree on unmerged versions it lacks

Usage: mdkg sync [OPTIONS] [TARGET]

Arguments:
  [TARGET]
          Node id or folder path (default: the whole graph)

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg sync
  mdkg sync <id>
```

### `mdkg abandon`

```text
Drop a changeset whose branch won't merge, and work built on it

Usage: mdkg abandon [OPTIONS] [CHANGESET]

Arguments:
  [CHANGESET]
          Changeset id (`cs-<n>`)

Options:
      --branch <BRANCH>
          Abandon every open changeset from this branch instead

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg abandon cs-3
  mdkg abandon --branch my-branch
```

### `mdkg mcp`

```text
Serve the graph and leases to coding agents over stdio MCP

Usage: mdkg mcp [OPTIONS]

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg mcp --as <agent>
```

### `mdkg entry`

```text
The ledger: entries on a node (findings, questions, decisions, issues, reviews, requirements) and its gate

Usage: mdkg entry [OPTIONS] <COMMAND>

Commands:
  list       List a node's entries
  show       Show one entry and its history
  gate       Check the node's gate: what `done` would refuse for (A29)
  update     Apply op objects, one JSON object per line, from a file or `-`
  add        Open a finding, an issue, a question or a decision
  ask        Ask a question on a node
  answer     Answer a question asked of you, or your own
  decide     Record an accepted decision, or accept a proposed one (`<entry>`)
  propose    Propose a decision
  complete   Complete a review round you were asked for (`review.complete`)
  fix        Mark a finding fixed at a SHA
  verify     Verify a finding's fix at a reviewed SHA
  reopen     Reopen a fixed finding (`--at` the reviewed SHA), or a disputed one
  reject     Reject a finding, or a proposed decision, with a reason
  dispute    Dispute a rejected finding
  residual   Accept a failure as residual (a person or a coordinator)
  duplicate  Mark a finding a duplicate of another (`--of`)
  outdated   Mark a finding outdated
  withdraw   Withdraw your own open question
  close      Close an answered question you asked, or a rejected finding
  resolve    Resolve an issue
  move       Move an open question or issue to another node (`--to`)
  req        Set a requirement's state: met, unmet or waived (`--state`)
  help       Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg entry list <node>
  mdkg entry fix <node> F2 --sha <sha>
  mdkg entry gate <node>
```

#### `mdkg entry list`

```text
List a node's entries

Usage: mdkg entry list [OPTIONS] <NODE>

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

Options:
      --kind <KIND>


      --state <STATE>


      --severity <SEVERITY>


      --asked-of <ASKED_OF>


      --blocking <BLOCKING>
          [possible values: true, false]

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry show`

```text
Show one entry and its history

Usage: mdkg entry show [OPTIONS] <NODE> <ENTRY>

Arguments:
  <NODE>


  <ENTRY>
          The entry: `F3`

Options:
      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry gate`

```text
Check the node's gate: what `done` would refuse for (A29)

Usage: mdkg entry gate [OPTIONS] <NODE>

Arguments:
  <NODE>


Options:
      --at <AT>
          The head to check at (default: this checkout's HEAD)

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry update`

```text
Apply op objects, one JSON object per line, from a file or `-`

Usage: mdkg entry update [OPTIONS] --op-file <OP_FILE> <NODE>

Arguments:
  <NODE>
          The node ops act on when they name none

Options:
      --op-file <OP_FILE>
          Op objects, one per line; `-` reads stdin

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry add`

```text
Open a finding, an issue, a question or a decision

Usage: mdkg entry add [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry ask`

```text
Ask a question on a node

Usage: mdkg entry ask [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry answer`

```text
Answer a question asked of you, or your own

Usage: mdkg entry answer [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry decide`

```text
Record an accepted decision, or accept a proposed one (`<entry>`)

Usage: mdkg entry decide [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry propose`

```text
Propose a decision

Usage: mdkg entry propose [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry complete`

```text
Complete a review round you were asked for (`review.complete`)

Usage: mdkg entry complete [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry fix`

```text
Mark a finding fixed at a SHA

Usage: mdkg entry fix [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry verify`

```text
Verify a finding's fix at a reviewed SHA

Usage: mdkg entry verify [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry reopen`

```text
Reopen a fixed finding (`--at` the reviewed SHA), or a disputed one

Usage: mdkg entry reopen [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry reject`

```text
Reject a finding, or a proposed decision, with a reason

Usage: mdkg entry reject [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry dispute`

```text
Dispute a rejected finding

Usage: mdkg entry dispute [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry residual`

```text
Accept a failure as residual (a person or a coordinator)

Usage: mdkg entry residual [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry duplicate`

```text
Mark a finding a duplicate of another (`--of`)

Usage: mdkg entry duplicate [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry outdated`

```text
Mark a finding outdated

Usage: mdkg entry outdated [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry withdraw`

```text
Withdraw your own open question

Usage: mdkg entry withdraw [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry close`

```text
Close an answered question you asked, or a rejected finding

Usage: mdkg entry close [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry resolve`

```text
Resolve an issue

Usage: mdkg entry resolve [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry move`

```text
Move an open question or issue to another node (`--to`)

Usage: mdkg entry move [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg entry req`

```text
Set a requirement's state: met, unmet or waived (`--state`)

Usage: mdkg entry req [OPTIONS] <NODE> [ENTRY]

Arguments:
  <NODE>
          The node: an id, a qid or a unique prefix

  [ENTRY]
          The entry the op acts on (`F3`; `R2` for `req`; `V1` for `complete`)

Options:
      --kind <KIND>


      --title <TITLE>


      --text <TEXT>
          The text: a reason, an answer, a note

      --severity <SEVERITY>


      --anchor <ANCHOR>
          `path` or `path:line`

      --requirement <REQUIREMENT>


      --asked-of <ASKED_OF>


      --blocking


      --slot <SLOT>


      --ref <REFS>


      --supersedes <SUPERSEDES>


      --sha <SHA>


      --commit <COMMITS>
          For `fix`: another commit of the fix, besides `--sha` (repeat it)

      --supersede
          For `fix` on a reopened finding, by one of its fixers: these commits replace every one declared before (recorded as `supersede`)

      --at <AT>


      --of <OF>


      --to <TO>


      --state <STATE>
          For `req`: met, unmet or waived

      --range <RANGE>
          For `complete`: `<base>..<sha>`

      --verdict <VERDICT>
          For `complete`: clean or findings

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

### `mdkg req`

```text
The requirements in a node's body, with their ledger states

Usage: mdkg req [OPTIONS] <COMMAND>

Commands:
  list  List requirements in a node body, with their ledger states
  help  Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg req list <node> --json
```

#### `mdkg req list`

```text
List requirements in a node body, with their ledger states

Usage: mdkg req list [OPTIONS] <NODE>

Arguments:
  <NODE>
          A node id, qualified id, unique prefix, or title fragment

Options:
      --json
          Print one JSON envelope

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

### `mdkg msg`

```text
Send and read messages: send, inbox, wait, ack, thread, import

Usage: mdkg msg [OPTIONS] <COMMAND>

Commands:
  watch      Follow the local coordinator event stream as one JSON event per line
  send       Send a message to a channel or to mailboxes
  inbox      Show your unacked mail (your mailbox and the roles you hold)
  pending    List unanswered questions and decisions, oldest first, with age. Acknowledgement does not count as an answer; a --reply-to does
  digest     Compute the current mail digest without marking any message shown
  wait       Wait for mail; exits 6 if none arrives before the timeout
  ack        Ack messages delivered to you
  forward    Re-send messages delivered to you, verbatim with their senders, as one new message; then ack them
  thread     Show a conversation's messages, oldest first
  import     Record a read-only sender's output (a file) as its message
  list       List mail across mailboxes by kind, read-only: no ack, no shown mark, no purge. Only a person (`--as human:<name>`) runs it, outside an agent's turn. A body whose retention is up reads as purged
  decisions  List send-time decision metadata without acknowledging mail. A row does not prove human approval or launcher promotion: the launcher revalidates against its own current configuration before using a decision
  help       Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg inbox
  mdkg msg send --channel dev --kind note --text "Tests pass on main"
```

#### `mdkg msg watch`

```text
Follow the local coordinator event stream as one JSON event per line

Usage: mdkg msg watch [OPTIONS]

Options:
      --after <AFTER>
          Resume just after this opaque event cursor (`start` replays history)

      --from-start
          Replay from the first record, then follow new events

      --types <TYPES>
          Event types, repeated or comma-separated; a trailing `*` matches a prefix

      --channel <NAME>
          Events of these channels, including messages posted to them (repeatable)

      --mailbox <NAME>
          Events for messages delivered to this mailbox

      --json
          Required: print one bare API event per line

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg watch --json
  mdkg msg watch --from-start --types 'mdkg.message.*' --json
```

#### `mdkg msg send`

```text
Send a message to a channel or to mailboxes

Usage: mdkg msg send [OPTIONS] --kind <KIND>

Options:
      --channel <NAME>
          Post to this channel's members

      --to <TO>
          Send to these mailboxes (comma-separated or repeated)

      --kind <KIND>
          review_request, findings, resolution, question, decision, handoff or note; notice is readable information that never wakes an agent

      --action <ACTION>
          Decision scope: implement, test, commit, design or review. Fixed limits always apply

      --ref <NODE_REF>
          A committed node holding the decision content; required for decision mail

      --id <ID>
          The message id (minted if left out; pass your own to retry safely)

      --context <CONTEXT>
          The conversation (default: the replied-to message's, else a new one).

          A reply joins the replied-to message's context when it goes to the same place, channel to channel or direct to direct, and a direct reply's conversation holds no channel post; otherwise a channel post's context is the channel's default context, and a direct message's its own id.

      --reply-to <REPLY_TO>
          The id of a message this replies to (repeatable)

      --text <TEXT>
          A text part (repeatable)

      --text-file <TEXT_FILE>
          A text part read from a file (`-` for stdin)

      --data-file <DATA_FILE>
          A data part read from a JSON file

      --url <URL>
          A URL part

      --node <NODE>
          A node this message is about (repeatable): an id, a qid or a unique prefix, bound by its UUID. A review_request names exactly one, and opens a review round on it with --range

      --range <RANGE>
          With --node on a review_request: the range under review, `<base>..<sha>`, which the review round records

      --supersedes <SUPERSEDES>
          A message of yours this one replaces (repeatable): its unacked deliveries to this message's recipients are acked by mdkg as superseded, so they leave inboxes and wake no one

      --slot <SLOT>
          Replaces your earlier unacked messages with this slot and the same first --node (or none), as --supersedes does: `[a-z0-9._:-]`, up to 200 characters, such as `brief.a-mail`

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg send --channel dev --kind note --text "Tests pass on main"
  mdkg msg send --to role:reviewer --kind review_request --text "Please review cs-3"
```

#### `mdkg msg inbox`

```text
Show your unacked mail (your mailbox and the roles you hold)

Usage: mdkg msg inbox [OPTIONS]

Options:
      --mailbox <MAILBOX>
          Read this mailbox instead (yours, a role you hold, or `human:*`)

      --channel <NAME>
          Only mail posted to this channel

      --kind <KIND>
          Only these kinds: repeat the flag or separate them with commas (`--kind question,handoff`). Receipts are left out unless this names `receipt`

      --channel-kind <ROOM_KIND>
          Channel mail only of these kinds, while direct mail of every kind still counts (`--channel-kind question,handoff`): a wake hook's filter

      --from <FROM>
          Only mail from these senders: repeat the flag or separate them with commas (`--from a-mail,orchestrator`)

      --after <AFTER>
          Only mail after this cursor (the `cursor` a previous call returned). A cursor covers the mailboxes it was issued for: after taking a role, read once without `--after` to see the role's backlog

      --limit <LIMIT>
          At most this many messages (1 or more)

      --all
          Acked mail too

      --no-mark
          List without counting as shown or purging, for a reminder that shows headers only (a hook): an ack of mail you were never shown is refused, so read it without this first. The store may still be created or upgraded; --read-only never writes it

      --mark-shown <MARK_SHOWN>
          Record that exactly these messages reached you, each as `<seq>/<digest>/<id>` from a `--no-mark` listing, so you may ack them; list nothing. For a reader that delivers only some of what it read: the MCP server once its reply is out, or a turn's lead read (`--from <reports_to> --no-mark --limit 1`)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg inbox
  mdkg msg inbox --channel dev --json
  mdkg msg inbox --from orchestrator --kind question
```

#### `mdkg msg pending`

```text
List unanswered questions and decisions, oldest first, with age. Acknowledgement does not count as an answer; a --reply-to does

Usage: mdkg msg pending [OPTIONS]

Options:
      --after <AFTER>
          Continue after this store-wide message sequence

          [default: 0]

      --limit <LIMIT>
          The most unanswered messages to return

          [default: 100]

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg pending --as worker
  mdkg msg pending --after 42 --json
```

#### `mdkg msg digest`

```text
Compute the current mail digest without marking any message shown

Usage: mdkg msg digest [OPTIONS]

Options:
      --mailbox <MAILBOX>
          Read this held mailbox instead of all held mailboxes

      --budget <BUDGET>
          Target rendered size in bytes

          [default: 49152]

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg msg wait`

```text
Wait for mail; exits 6 if none arrives before the timeout

Usage: mdkg msg wait [OPTIONS]

Options:
      --mailbox <MAILBOX>
          Read this mailbox instead (yours, a role you hold, or `human:*`)

      --channel <NAME>
          Only mail posted to this channel

      --kind <KIND>
          Only these kinds: repeat the flag or separate them with commas (`--kind question,handoff`). Receipts are left out unless this names `receipt`

      --channel-kind <ROOM_KIND>
          Channel mail only of these kinds, while direct mail of every kind still counts (`--channel-kind question,handoff`): a wake hook's filter

      --from <FROM>
          Only mail from these senders: repeat the flag or separate them with commas (`--from a-mail,orchestrator`)

      --after <AFTER>
          Only mail after this cursor (the `cursor` a previous call returned). A cursor covers the mailboxes it was issued for: after taking a role, read once without `--after` to see the role's backlog

      --limit <LIMIT>
          At most this many messages (1 or more)

      --timeout <TIMEOUT>
          Give up after this many seconds (exit 6)

          [default: 600]

      --no-mark
          List without counting as shown (see `inbox --no-mark`)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg wait --timeout 300
  mdkg msg wait --after 42 --json
```

#### `mdkg msg ack`

```text
Ack messages delivered to you

Usage: mdkg msg ack [OPTIONS] [IDS]...

Arguments:
  [IDS]...
          Message ids, or the `#N` (or `N`) a listing shows

Options:
      --through <THROUGH>
          Ack all your unacked mail numbered this or lower instead (`N` or `#N`: a listing's number, or the `cursor` it returned).

          Findings and faults are skipped unless `--kind` names them, and so is mail never listed to you; the result names what it skipped and why.

      --kind <KIND>
          With --through: only these kinds (repeat the flag or separate them with commas). Name `findings` to ack findings

      --mailbox <MAILBOX>
          Ack only this mailbox's deliveries (yours or a role you hold)

      --outcome <OUTCOME>
          A short outcome to record with the ack (at most 500 bytes).

          Keep credentials out of it: it is metadata and stays after the body is purged. An empty or blank one records none.

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg ack <message-id>
  mdkg msg ack <message-id> --outcome merged
  mdkg msg ack --through 4400 --kind note,handoff
```

#### `mdkg msg forward`

```text
Re-send messages delivered to you, verbatim with their senders, as one new message; then ack them.

For a role that relays mail: an ack can purge a body, so forward it first. The send and the acks are one step: nothing is acked unless the forward is stored.

Usage: mdkg msg forward [OPTIONS] <IDS>...

Arguments:
  <IDS>...
          Message ids, or the `#N` (or `N`) a listing shows, in the order to forward them

Options:
      --channel <NAME>
          Post to this channel's members

      --to <TO>
          Send to these mailboxes (comma-separated or repeated)

      --kind <KIND>
          The new message's kind (default: the forwarded messages' kind, when they all have one). Never `decision`

      --text <TEXT>
          A text part to put before the forwarded messages (repeatable)

      --mailbox <MAILBOX>
          Forward and ack only this mailbox's deliveries (yours or a role you hold)

      --outcome <OUTCOME>
          The outcome recorded with the acks (default: `forwarded as <id>`)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg forward 4410 4411 --to a-mail --kind findings --text "Reviews of 1a2b3c4"
  mdkg msg forward <message-id> --channel dev
```

#### `mdkg msg thread`

```text
Show a conversation's messages, oldest first

Usage: mdkg msg thread [OPTIONS] [TARGET]

Arguments:
  [TARGET]
          A message id (its conversation), the `#N` a listing shows, or a context id

Options:
      --channel <NAME>
          Every message posted to this channel instead, whatever its context

      --after <AFTER>
          Only messages after this cursor (the `cursor` a previous call on this thread or channel returned), to page through a long one

      --limit <LIMIT>
          At most this many messages (1 or more), the oldest first

      --tail <TAIL>
          The newest this many messages (1 or more), listed oldest first.

          For catching up on a thread's or a channel's recent messages. Not with --limit or --after.

      --member
          With --channel: read it only as a member (you or a role you hold), refused with channel.not_member before anything is read

      --no-mark
          List without counting as shown (see `inbox --no-mark`)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg thread <message-id>
  mdkg msg thread --channel dev --tail 20
```

#### `mdkg msg import`

```text
Record a read-only sender's output (a file) as its message

Usage: mdkg msg import [OPTIONS] --kind <KIND> --from <FROM> <FILE>

Arguments:
  <FILE>
          The file: JSON becomes a data part, anything else a text part

Options:
      --channel <NAME>
          Post to this channel's members

      --to <TO>
          Send to these mailboxes (comma-separated or repeated)

      --kind <KIND>
          The message kind

      --from <FROM>
          The sender the output belongs to (for example `codex:<run>`)

      --reply-to <REPLY_TO>
          The ids of the messages this answers (repeatable)

      --node <NODE>
          Nodes the imported reply concerns (repeatable)

      --context <CONTEXT>
          The conversation (default as for `send`)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg import --from codex:run-7 --channel dev --kind findings review.json
```

#### `mdkg msg list`

```text
List mail across mailboxes by kind, read-only: no ack, no shown mark, no purge. Only a person (`--as human:<name>`) runs it, outside an agent's turn. A body whose retention is up reads as purged

Usage: mdkg msg list [OPTIONS]

Options:
      --kind <KIND>
          Only these kinds: repeat the flag or separate them with commas (`--kind fault,review_request`). Receipts are left out unless this names `receipt`

      --channel <NAME>
          Mail delivered to this channel's current members, and the posts to it (repeatable). With neither this nor `--to`, every mailbox

      --to <TO>
          Mail delivered to these mailboxes: repeat the flag or separate them with commas

      --from <FROM>
          Only mail from these senders: repeat the flag or separate them with commas

      --state <STATE>
          `unacked` (a delivery in scope is still unacked), `acked` (every delivery in scope is acked) or `any`

          [default: unacked]

      --after <AFTER>
          Only mail after this cursor (the `cursor` a previous call returned)

      --limit <LIMIT>
          At most this many messages: 1 to 200 (default 50). A page also stops before bodies past 1 MiB

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg msg list --as human:nick --kind fault,review_request --json
  mdkg msg list --as human:nick --channel dev --kind question --state any
```

#### `mdkg msg decisions`

```text
List send-time decision metadata without acknowledging mail. A row does not prove human approval or launcher promotion: the launcher revalidates against its own current configuration before using a decision

Usage: mdkg msg decisions [OPTIONS]

Options:
      --after <AFTER>
          [default: 0]

      --limit <LIMIT>
          [default: 100]

      --message <MESSAGE>
          Only these message IDs (repeatable)

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

### `mdkg channel`

```text
Manage channels: create, join, leave, list

Usage: mdkg channel [OPTIONS] <COMMAND>

Commands:
  create  Create a channel, with you as a member
  join    Add you (or the given mailboxes) to a channel
  leave   Remove you (or the given mailboxes) from a channel
  list    List every channel, with its members and your unacked counts
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg channel list
  mdkg channel join dev
```

#### `mdkg channel create`

```text
Create a channel, with you as a member

Usage: mdkg channel create [OPTIONS] <NAME>

Arguments:
  <NAME>
          The channel's name: lowercase letters, digits, `.`, `_` and `-`

Options:
      --member <MEMBER>
          Mailboxes to add or remove instead of (for create: besides) you

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg channel create dev
```

#### `mdkg channel join`

```text
Add you (or the given mailboxes) to a channel

Usage: mdkg channel join [OPTIONS] <NAME>

Arguments:
  <NAME>
          The channel's name: lowercase letters, digits, `.`, `_` and `-`

Options:
      --member <MEMBER>
          Mailboxes to add or remove instead of (for create: besides) you

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg channel join dev
```

#### `mdkg channel leave`

```text
Remove you (or the given mailboxes) from a channel

Usage: mdkg channel leave [OPTIONS] <NAME>

Arguments:
  <NAME>
          The channel's name: lowercase letters, digits, `.`, `_` and `-`

Options:
      --member <MEMBER>
          Mailboxes to add or remove instead of (for create: besides) you

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg channel leave dev
```

#### `mdkg channel list`

```text
List every channel, with its members and your unacked counts

Usage: mdkg channel list [OPTIONS]

Options:
      --mine
          Only channels you're a member of

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg channel list
  mdkg channel list --mine --json
```

### `mdkg agents`

```text
Manage definitions and run agents: add, edit, rm, up, status, resume

Usage: mdkg agents [OPTIONS] <COMMAND>

Commands:
  add      Create a committed agent definition in .mdkg/agents/
  edit     Update an agent definition; fields not given stay as they are
  rm       Remove a committed definition, keeping local sessions and history
  up       Wake agents on their mail and run their turns under their policies, until stopped
  prepare  Create or verify a worker's worktree without running a turn. Only outside an agent turn; prints the canonical working directory
  stop     Stop agents gracefully: a turn in flight finishes, then each agent's supervisor admits no more of its turns. Nothing is killed. Inside an agent's turn it may stop only that agent
  logs     Show a turn's output, normalized across harnesses, or follow an agent's turns as they run
  turns    List an agent's turns, newest first: each one's id, number, times, outcome and receipt, a page at a time
  status   Show the agents, their policies, and the threads the loop guard paused
  list     List the agents the settings define, each with the layer that defines it and every layer with an entry for it. Reads settings only: nothing is probed and no saved state is read
  use-key  Bill an agent's next turn, one, on your API key, after its subscription hit its usage limit. Nothing charges the key on its own; the agent's policy must allow this (`auth.api_key_fallback`). Refused inside an agent's turn, and for an agent's own actor: a person runs it from their own terminal
  resume   Let a thread the loop guard paused run again, or all of an agent's
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg agents status
  mdkg agents up --once
```

#### `mdkg agents add`

```text
Create a committed agent definition in .mdkg/agents/

Usage: mdkg agents add [OPTIONS] <AGENT>

Arguments:
  <AGENT>
          The actor name and definition filename, without .json

Options:
      --file <FILE>
          Read a complete definition from this JSON file before applying flags

      --harness <HARNESS>
          [possible values: claude, codex, opencode]

      --role <ROLE>
          [possible values: worker, reviewer]

      --cwd <CWD>


      --worktree
          Give a worker its own worktree (the default without --cwd)

      --model <MODEL>


      --effort <EFFORT>


      --base <BASE>


      --instructions <INSTRUCTIONS>


      --policy <POLICY>
          Per-agent turn policy, as a JSON object

      --delivery <DELIVERY>
          Mail delivery shape (batch by default)

          [possible values: batch, digest]

      --reports-to <REPORTS_TO>
          The actor receiving turn receipts and faults: turn errors and refused replies (default: orchestrator)

      --wake-kinds [<WAKE_KINDS>]
          Mail kinds that start a turn (comma-separated or repeated). With no values, disable mail-triggered wakes; --unset wake_kinds restores defaults

      --extends <EXTENDS>
          Reserved remote definition reference; this release does not run it

      --mode <MODE>
          Cross-harness preset (auto; other values are reserved)

      --sandbox <SANDBOX>
          Harness-native sandbox override

      --approvals <APPROVALS>
          Harness-native approvals override

      --unset <UNSET>
          Remove an optional field; repeat for several fields

          [possible values: extends, cwd, worktree, model, effort, base, instructions, policy, allow, protect, reports_to, wake_kinds, mode, sandbox, approvals, follow]

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg agents edit`

```text
Update an agent definition; fields not given stay as they are

Usage: mdkg agents edit [OPTIONS] <AGENT>

Arguments:
  <AGENT>
          The actor name and definition filename, without .json

Options:
      --file <FILE>
          Read a complete definition from this JSON file before applying flags

      --harness <HARNESS>
          [possible values: claude, codex, opencode]

      --role <ROLE>
          [possible values: worker, reviewer]

      --cwd <CWD>


      --worktree
          Give a worker its own worktree (the default without --cwd)

      --model <MODEL>


      --effort <EFFORT>


      --base <BASE>


      --instructions <INSTRUCTIONS>


      --policy <POLICY>
          Per-agent turn policy, as a JSON object

      --delivery <DELIVERY>
          Mail delivery shape (batch by default)

          [possible values: batch, digest]

      --reports-to <REPORTS_TO>
          The actor receiving turn receipts and faults: turn errors and refused replies (default: orchestrator)

      --wake-kinds [<WAKE_KINDS>]
          Mail kinds that start a turn (comma-separated or repeated). With no values, disable mail-triggered wakes; --unset wake_kinds restores defaults

      --extends <EXTENDS>
          Reserved remote definition reference; this release does not run it

      --mode <MODE>
          Cross-harness preset (auto; other values are reserved)

      --sandbox <SANDBOX>
          Harness-native sandbox override

      --approvals <APPROVALS>
          Harness-native approvals override

      --unset <UNSET>
          Remove an optional field; repeat for several fields

          [possible values: extends, cwd, worktree, model, effort, base, instructions, policy, allow, protect, reports_to, wake_kinds, mode, sandbox, approvals, follow]

      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg agents rm`

```text
Remove a committed definition, keeping local sessions and history

Usage: mdkg agents rm [OPTIONS] <AGENT>

Arguments:
  <AGENT>


Options:
      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg agents up`

```text
Wake agents on their mail and run their turns under their policies, until stopped

Usage: mdkg agents up [OPTIONS] [AGENTS]...

Arguments:
  [AGENTS]...
          The agents to run (default: configured agents, else saved agents, plus interactive registrations without a headless spec)

Options:
      --once
          Run what's waiting now, then stop

      --max-turns <MAX_TURNS>
          Stop each agent after this many turns

      --wait <WAIT>
          How long each wait lasts before starting again, in seconds

          [default: 600]

      --fresh
          Each agent's first turn starts a new session

      --prompt <PROMPT>
          Instructions for a manual pass. Requires names and --once

      --reply-kind <REPLY_KIND>
          The reply kind for a manual pass

      --limit <LIMIT>
          Override the most messages shown in each turn

      --timeout <TIMEOUT>
          Override the turn timeout in seconds

      --keep-logs
          Keep each turn's logs after importing its reply

      --dry-run
          Print the planned command and prompt. Requires names and --once

      --binary <BINARY>
          The mdkg binary the agents' MCP servers run (default: this one, by its bare name when `PATH` finds this same file, else by its path as invoked)

      --allow-zen
          OpenCode only: allow its Zen models (throwaway smoke turns)

      --json
          Print one JSON envelope per event, a line each

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg agents up
  mdkg agents up --once
  mdkg agents up worker-1 --max-turns 3
```

#### `mdkg agents prepare`

```text
Create or verify a worker's worktree without running a turn. Only outside an agent turn; prints the canonical working directory

Usage: mdkg agents prepare [OPTIONS] <AGENT>

Arguments:
  <AGENT>


Options:
      --json


  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg agents prepare worker-1
  mdkg agents prepare worker-1 --json
```

#### `mdkg agents stop`

```text
Stop agents gracefully: a turn in flight finishes, then each agent's supervisor admits no more of its turns. Nothing is killed. Inside an agent's turn it may stop only that agent

Usage: mdkg agents stop [OPTIONS] [AGENTS]...

Arguments:
  [AGENTS]...
          The agents to stop

Options:
      --all
          Every agent a running supervisor runs

      --drain
          Let a turn in flight finish, then admit no more (required: the only mode)

      --wait <WAIT>
          Wait up to this many seconds for each agent to stop; exits 6 if a turn is still running then

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg agents stop worker-1 --drain
  mdkg agents stop --all --drain --wait 600
```

#### `mdkg agents logs`

```text
Show a turn's output, normalized across harnesses, or follow an agent's turns as they run

Usage: mdkg agents logs [OPTIONS] <AGENT>

Arguments:
  <AGENT>
          The agent's name

Options:
      --turn <TURN>
          Only this turn (its turn id)

      --follow
          Keep following: a turn until it ends, or the agent's turns for good

      --after <AFTER>
          Start after this position: `<turn_id>:<seq>`, `<turn_id>:end` or `turn:<N>` (with --turn, a plain seq too)

      --native
          Also show native events no mapping covers

      --json
          Print NDJSON, one turn output event per line

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg agents logs worker-1
  mdkg agents logs worker-1 --follow --json
  mdkg agents logs worker-1 --follow --after turn:1 --json
```

#### `mdkg agents turns`

```text
List an agent's turns, newest first: each one's id, number, times, outcome and receipt, a page at a time

Usage: mdkg agents turns [OPTIONS] <AGENT>

Arguments:
  <AGENT>
          The agent's name

Options:
      --limit <LIMIT>
          At most this many turns, newest first (default 50, at most 500)

      --before <BEFORE>
          Only turns numbered below N: pass the last page's `next_before`

      --turn <TURN>
          Only turn number N

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg agents turns worker-1 --json
  mdkg agents turns worker-1 --limit 20 --before 41 --json
  mdkg agents turns worker-1 --turn 7 --json
```

#### `mdkg agents status`

```text
Show the agents, their policies, and the threads the loop guard paused

Usage: mdkg agents status [OPTIONS]

Options:
      --no-probe
          Run no probe: no harness login check, Keychain listing or billing read, so it's cheap to poll. `auth` and `billing` are left out

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg agents status
  mdkg agents status --json
  mdkg agents status --no-probe --json
```

#### `mdkg agents list`

```text
List the agents the settings define, each with the layer that defines it and every layer with an entry for it. Reads settings only: nothing is probed and no saved state is read

Usage: mdkg agents list [OPTIONS]

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg agents list
  mdkg agents list --json
```

#### `mdkg agents use-key`

```text
Bill an agent's next turn, one, on your API key, after its subscription hit its usage limit. Nothing charges the key on its own; the agent's policy must allow this (`auth.api_key_fallback`). Refused inside an agent's turn, and for an agent's own actor: a person runs it from their own terminal

Usage: mdkg agents use-key [OPTIONS] <AGENT>

Arguments:
  <AGENT>
          The agent's name (its actor id)

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg agents use-key worker-1
```

#### `mdkg agents resume`

```text
Let a thread the loop guard paused run again, or all of an agent's

Usage: mdkg agents resume [OPTIONS] <AGENT>

Arguments:
  <AGENT>
          The agent's name (its actor id)

Options:
      --thread <THREAD>
          Clear only this loop-guard thread. Without it, an interactive receiver also waits for a new primary prompt to select its route

      --store <STORE>
          Confirm the current physical store when retaining a foreign record

      --archive-context
          Preserve full recovery context in an immutable archive

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg agents resume worker-1
  mdkg agents resume worker-1 --thread <thread>
```

### `mdkg auth`

```text
The API key fallback's helper: `auth key <provider>`

Usage: mdkg auth [OPTIONS] <COMMAND>

Commands:
  key   Print the API key the fallback bills, for a harness's key helper (`anthropic` or `openai`); refuses a terminal
  help  Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg auth key anthropic
```

#### `mdkg auth key`

```text
Print the API key the fallback bills, for a harness's key helper (`anthropic` or `openai`); refuses a terminal

Usage: mdkg auth key [OPTIONS] <PROVIDER>

Arguments:
  <PROVIDER>
          `anthropic` (Claude) or `openai` (Codex)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg auth key anthropic
```

### `mdkg hooks`

```text
What an agent harness's hooks run: one router (`hooks run`) for the mail notice, the stop reminders, the idle wake and the lease guards.

Reads the harness's hook input on stdin and prints its output. Says nothing outside a graph; never shows a message's body. `hook` is an alias, and `start|stop|wait|edit` are 0.7.0's commands, kept for one release.

Usage: mdkg hooks [OPTIONS] <COMMAND>

Commands:
  run    The router each native hook entry calls: reads the harness's hook input on stdin, runs the rules for its event, prints the harness's output
  start  Turn or session start, and each prompt (alias of `run`, for one release)
  stop   Before the agent stops (alias of `run`, for one release)
  wait   An idle session's wait for mail (alias of `run`, for one release)
  edit   Before a tool call (alias of `run`, for one release)
  help   Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg hooks run --harness claude --native PreToolUse
  mdkg hooks run --harness opencode --native context
  mdkg hook start --harness claude
```

#### `mdkg hooks run`

```text
The router each native hook entry calls: reads the harness's hook input on stdin, runs the rules for its event, prints the harness's output

Usage: mdkg hooks run [OPTIONS] --harness <HARNESS> --native <NATIVE> [-- <ARGS>...]

Arguments:
  [ARGS]...
          git only: the hook's own arguments (pre-push's remote and URL)

Options:
      --harness <HARNESS>
          The adapter calling: claude, codex, opencode, or git (its own `pre-commit` and `pre-push` hooks)

          [possible values: claude, codex, opencode, git]

      --native <NATIVE>
          The harness's own name for the event: Claude's and Codex's `SessionStart`, `UserPromptSubmit`, `PreToolUse` and `Stop`, OpenCode's `context`, `tool.execute.before` and `session.execution.succeeded`, and `idle` for an idle session's wait

      --timeout <TIMEOUT>
          The idle wait only: how long to wait for mail, in seconds

          [default: 3000]

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg hooks start`

```text
Turn or session start, and each prompt (alias of `run`, for one release)

Usage: mdkg hooks start [OPTIONS] --harness <HARNESS>

Options:
      --harness <HARNESS>
          The harness calling: claude, codex or opencode

          [possible values: claude, codex, opencode]

      --timeout <TIMEOUT>
          `wait` only: how long to wait for mail, in seconds

          [default: 3000]

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg hooks stop`

```text
Before the agent stops (alias of `run`, for one release)

Usage: mdkg hooks stop [OPTIONS] --harness <HARNESS>

Options:
      --harness <HARNESS>
          The harness calling: claude, codex or opencode

          [possible values: claude, codex, opencode]

      --timeout <TIMEOUT>
          `wait` only: how long to wait for mail, in seconds

          [default: 3000]

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg hooks wait`

```text
An idle session's wait for mail (alias of `run`, for one release)

Usage: mdkg hooks wait [OPTIONS] --harness <HARNESS>

Options:
      --harness <HARNESS>
          The harness calling: claude, codex or opencode

          [possible values: claude, codex, opencode]

      --timeout <TIMEOUT>
          `wait` only: how long to wait for mail, in seconds

          [default: 3000]

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg hooks edit`

```text
Before a tool call (alias of `run`, for one release)

Usage: mdkg hooks edit [OPTIONS] --harness <HARNESS>

Options:
      --harness <HARNESS>
          The harness calling: claude, codex or opencode

          [possible values: claude, codex, opencode]

      --timeout <TIMEOUT>
          `wait` only: how long to wait for mail, in seconds

          [default: 3000]

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

### `mdkg install`

```text
Set up Claude Code, Codex and OpenCode for this project.

Writes exactly the projections .mdkg/config.json lists: MCP configs, the AGENTS.md block, skill mirrors and, with --hooks, hooks. --enable and --disable change the list, then sync. --setup moves the clone to the graph ref (personal, professional or local); --checkout-mode moves it back.

Usage: mdkg install [OPTIONS]

Options:
      --dry-run
          Show what would change and write nothing

      --undo
          Put back everything install changed here, exactly as it was; refuses where a file was edited since

      --binary <BINARY>
          The mdkg binary the MCP configs run (default: this one, by its bare name when `PATH` finds this same file, else by its path as invoked)

      --enable <LIST>
          Projections to add to the config's list, comma-separated: agents-md, mcp:claude, mcp:codex, mcp:opencode, skills:.agents/skills, skills:.claude/skills, hooks:claude, hooks:codex, hooks:opencode

      --disable <LIST>
          Projections to take off the list, comma-separated; mdkg's part of their files is removed, the rest kept

      --setup <SETUP>
          Move this clone to the graph ref, or set its setup again: `personal` (the graph syncs with origin), `professional` (with --graph-remote, a private repository of your own, so the company remote is never touched) or `local` (no remote). Nothing is pushed until a person approves the remote

          Possible values:
          - personal:     `graph.remote = origin`: the graph syncs with the code's own remote
          - professional: `graph.remote` is a private repository of your own, so the company remote is never touched
          - local:        No remote

      --graph-remote <REMOTE>
          The professional setup's graph remote: an existing remote that shares no URL with another

      --publish-design
          Allow publishing design to the code base for a professional or local setup (personal allows it already)

      --view
          Create or refresh .worktrees/graph, a detached, read-only view of the graph ref

      --hooks
          Install the hook projections the config lists (hooks:git, hooks:claude, hooks:codex, hooks:opencode). Hooks are opt-in: without this, install skips any not already installed in this checkout, and `--enable hooks:<name>` asks for that one. The shared hook files install writes are ignored: in .git/info/exclude for the professional setup, in .gitignore otherwise. Claude's per-user .claude/settings.local.json goes in .git/info/exclude when install creates it

      --checkout-mode
          Go back to checkout mode (0.7.0's layout) in this build: unsets mdkg.graph.ref and keeps the graph ref

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg install --enable agents-md,mcp:claude,skills:.claude/skills --dry-run
  mdkg install
  mdkg install --disable mcp:opencode
  mdkg install --setup local --dry-run
  mdkg install --setup professional --graph-remote mine
  mdkg install --hooks
```

### `mdkg store`

```text
List or restore copies of the coordinator store

Usage: mdkg store [OPTIONS] <COMMAND>

Commands:
  snapshots  List the copies of the coordinator store taken before upgrades and restores
  restore    Put a store copy back
  help       Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg store snapshots
```

#### `mdkg store snapshots`

```text
List the copies of the coordinator store taken before upgrades and restores

Usage: mdkg store snapshots [OPTIONS]

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg store snapshots
  mdkg store snapshots --json
```

#### `mdkg store restore`

```text
Put a store copy back.

Stop every process using the repository's store first: commands, the MCP server and the launcher.

Usage: mdkg store restore [OPTIONS] <SNAPSHOT>

Arguments:
  <SNAPSHOT>
          The copy: a path, or a file name beside the store

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg store restore <copy>
```

### `mdkg anchor`

```text
Write the record chain's head hash to an anchor file.

The file is .mdkg/anchors/<store-id>.json, for you to commit.

Usage: mdkg anchor [OPTIONS]

Options:
      --force
          Replace an anchor the chain no longer holds (after a restore)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg anchor
```

### `mdkg export`

```text
Export work state as a bundle anyone can verify.

`--work` writes the coordinator's record chain.

Usage: mdkg export [OPTIONS]

Options:
      --work
          The coordinator's record chain

      --out <OUT>
          Where to write it (default: mdkg-work-<store>-<seq>.jsonl here)

      --force
          Replace the file if it exists

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg export --work
  mdkg export --work --out work.jsonl
```

### `mdkg verify`

```text
Verify a work bundle, or this repository's store, against an anchor.

Verifying a bundle needs no repository.

Usage: mdkg verify [OPTIONS] [BUNDLE]

Arguments:
  [BUNDLE]
          The bundle; without one, the repository's store

Options:
      --anchor <ANCHOR>
          The anchor to check against (default for the store: the graph's own)

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg verify work.jsonl
  mdkg verify
```

### `mdkg migrate`

```text
Bring a 0.6 graph onto format 0.7.

Plan with --dry-run, then --apply <hash>; --status shows how far it got, and --rollback reverts it.

Usage: mdkg migrate [OPTIONS] <--dry-run|--apply <HASH>|--rollback|--status>

Options:
      --dry-run
          Print the plan and its hash, and write the full plan for review; change nothing in the graph

      --apply <HASH>
          Apply the plan with this hash (from --dry-run) in one commit, or finish one that stopped part way

      --rollback
          Revert the migration: the old graph, commit and store come back

      --status
          Show how far the last migration got

      --rewrite-refs
          With --apply: also apply the patch to AGENTS.md, CLAUDE.md, skills and scripts that --dry-run wrote for review

      --force
          With --rollback: restore the store even though it changed since the migration (the changes since are lost)

      --work-history <WHERE>
          With --dry-run or --apply: where the work history goes. `git` keeps checkpoints, finished tests, event logs and progress sections in git; `store` moves them to the coordinator store's log, as 0.7.0 did. Default: `git` when the graph commits its work (its config's `work.location`, which is `git` for every 0.6 graph)

          [possible values: git, store]

      --restore-history
          With --dry-run or --apply: on a graph already migrated, bring back into git, in one commit, the checkpoints, finished tests and event logs the migration moved to the store, read from the migration commit's parent

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg migrate --dry-run
  mdkg migrate --apply <hash>
  mdkg migrate --status
```

### `mdkg version`

```text
Show which build this is: version, commit, dirty tree, build date

Usage: mdkg version [OPTIONS]

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg version --json
```

### `mdkg init`

```text
Start a graph here: config, setup README and the mdkg-workflow skill.

Writes only inside .mdkg. Integrations are opt-in with `install --enable`; .mdkg/README.md explains the setup steps.

Usage: mdkg init [OPTIONS]

Options:
      --work <WORK>
          Where work lives: local beside the store (default), or shared in git. Asked for when run in a terminal

          Possible values:
          - local:  Work stays on this machine, beside the coordinator store (default)
          - shared: Work definitions are shared through git

      --branch-pattern <PATTERN>
          The branch each agent works on; must hold `{agent}` (default `agent/{agent}`)

      --worktree-pattern <PATTERN>
          Each agent's worktree, relative to the main checkout; must hold `{agent}` (default `.worktrees/{agent}`)

      --graph-only
          Write the config and setup README, without the mdkg-workflow skill. Integrations stay opt-in (`install --enable`)

      --dry-run
          Show what would change and write nothing

      --in-checkout
          In ref mode (`mdkg.graph.ref` set), write `.mdkg/` into this checkout as 0.7.0 did, instead of creating the graph ref

      --new-graph
          Start a new graph even though a fetched ref (any `refs/remotes/...` whose tip has a committed `.mdkg/config.json`) may hold this repository's graph, or git couldn't check. Without it, init refuses; for a graph it found, it names both ways to get it: merge the team's default branch (checkout mode) or `install` (ref mode)

      --setup <SETUP>
          Start the graph straight on the graph ref (refs/heads/mdkg-graph) in this setup, as `install --setup` names them: `personal` (the graph syncs with origin), `professional` (with --graph-remote) or `local` (no remote). No branch gets a commit and the checkout is unchanged; nothing is pushed. Refused, before anything is written, when the clone already has a graph: at the checkout's root (`install --setup` moves it), staged but missing, nested below the root, in any other worktree of the clone, on the graph ref, or `mdkg.graph.ref` set already; and when a fetched branch sits at the setup's own graph destination

          Possible values:
          - personal:     `graph.remote = origin`: the graph syncs with the code's own remote
          - professional: `graph.remote` is a private repository of your own, so the company remote is never touched
          - local:        No remote

      --graph-remote <REMOTE>
          With --setup professional: the graph remote, an existing remote that shares no URL with another

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg init
  mdkg init --dry-run
```

### `mdkg doctor`

```text
Check this setup: build, PATH, git, config, graph, store, mirrors, MCP, agents, hooks.

Changes nothing, except --repair adds missing clone-local graph identity records.

Usage: mdkg doctor [OPTIONS]

Options:
      --repair
          Add missing clone-local graph identity records only; never sync projections

      --strict
          Fail on warnings too

      --only <ONLY>
          Run one check only. `store` reads the coordinator store's schema and mail schema versions, and whether this build would upgrade it, read-only, without upgrading it or reading any other file (no config, projection or harness file), so it's cheap to poll

          Possible values:
          - store: The coordinator store's versions

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg doctor
  mdkg doctor --json
```

### `mdkg skills`

```text
List, show, validate, add and sync skills in .mdkg/skills

Usage: mdkg skills [OPTIONS] <COMMAND>

Commands:
  list      List the skills
  show      Show one skill: its frontmatter and its SKILL.md body
  validate  Check every skill (or one) and that the mirrors match it
  add       Add a skill the binary ships (`mdkg-workflow`) to `.mdkg/skills/`, and mirror it. In ref mode it's published on the graph ref, and `install` mirrors it
  sync      Copy `.mdkg/skills/` into the mirror folders agents read
  help      Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg skills list
  mdkg skills show <slug>
```

#### `mdkg skills list`

```text
List the skills

Usage: mdkg skills list [OPTIONS]

Options:
      --tags <TAGS>
          Comma-separated tags (such as `stage:plan`)

      --tags-mode <TAGS_MODE>
          Whether skills need any or all of the tags (default any)

          [possible values: any, all]

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg skills list
  mdkg skills list --tags stage:plan --json
```

#### `mdkg skills show`

```text
Show one skill: its frontmatter and its SKILL.md body

Usage: mdkg skills show [OPTIONS] <SLUG>

Arguments:
  <SLUG>
          The skill's slug (its folder name)

Options:
      --meta
          Omit the body

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg skills show mdkg-workflow
  mdkg skills show mdkg-workflow --meta
```

#### `mdkg skills validate`

```text
Check every skill (or one) and that the mirrors match it

Usage: mdkg skills validate [OPTIONS] [SLUG]

Arguments:
  [SLUG]
          Only this skill

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg skills validate
  mdkg skills validate mdkg-workflow --json
```

#### `mdkg skills add`

```text
Add a skill the binary ships (`mdkg-workflow`) to `.mdkg/skills/`, and mirror it. In ref mode it's published on the graph ref, and `install` mirrors it

Usage: mdkg skills add [OPTIONS] <SLUG>

Arguments:
  <SLUG>
          The shipped skill's slug

Options:
      --replace
          Replace a copy that differs from what the binary ships (an older version, or the project's own changes) with the shipped one

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg skills add mdkg-workflow
  mdkg skills add mdkg-workflow --replace
```

#### `mdkg skills sync`

```text
Copy `.mdkg/skills/` into the mirror folders agents read

Usage: mdkg skills sync [OPTIONS]

Options:
      --dry-run
          Show what would change and write nothing

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg skills sync --dry-run
  mdkg skills sync
```

### `mdkg affected`

```text
The tests a change needs: a selective run, or a full one when in doubt

Usage: mdkg affected [OPTIONS] --base <BASE>

Options:
      --base <BASE>
          The commit the change starts from (for a PR, its base)

      --head <HEAD>
          The commit the change ends at; any commit (the checkout isn't read)

          [default: HEAD]

      --main <MAIN>
          The main branch: paths the head has exactly as main had them where the two last met came from main, and aren't selected (default: `mdkg.mainRef`, else `main`; neither resolving skips nothing)

      --targets
          Require verified revisions and metadata before using the test-target plan

      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg affected --base main --json
```

### `mdkg schema`

```text
Print the JSON Schema of a command's output

Usage: mdkg schema [OPTIONS] [COMMAND]

Arguments:
  [COMMAND]
          Command name (default: list them)

Options:
      --out <OUT>
          Write every schema into this folder instead

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.

Examples:
  mdkg schema
  mdkg schema next
```

### `mdkg serve`

```text
Serve this graph's coordinator over its socket and, when asked, TLS; `serve status` tells whether it runs

Usage: mdkg serve [OPTIONS]
       mdkg serve <COMMAND>

Commands:
  status  Whether this repository's serve is running, from serve.json
  help    Print this message or the help of the given subcommand(s)

Options:
      --listen <ADDR:PORT>
          Also listen on loopback TCP, 127.0.0.1:<port> or [::1]:<port>, with a bearer token (overrides `serve.listen`)

      --json
          Print the start as one JSON envelope

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

#### `mdkg serve status`

```text
Whether this repository's serve is running, from serve.json

Usage: mdkg serve status [OPTIONS]

Options:
      --json
          Print JSON

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

### `mdkg git-serve`

```text
The receiving side of git between machines: the ssh forced command `authorized_keys` names for a machine's key, run by sshd as the repository owner

Usage: mdkg git-serve [OPTIONS] <MACHINE_ID>

Arguments:
  <MACHINE_ID>
          The machine whose key this is, as approved for the graph

Options:
  -h, --help
          Print help (see a summary with '-h')

Global options:
  -r, --root <ROOT>
          Run against this repository root (default: the current directory)

      --as <ACTOR>
          Act as this actor in coordination commands (default: $MDKG_ACTOR, then the worktree's folder name)

      --allow-store-upgrade
          Let a development build upgrade the coordinator store.

          The store is copied first; never use this on a live repository's store. Installed builds always may; only a build running from a Cargo build folder needs this.

      --read-only
          Never write the coordinator store.

          The store is opened read-only: never created, upgraded, recovered or checkpointed. Only `status`, which doesn't record its observation, and `msg inbox`, `msg wait` and `msg thread`, which read as with `--no-mark`, run with it. Every other command is refused with `store.read_only` (exit 1) before it runs.
```

## Anchor output

mdkg anchor leads with the anchor path to commit, then counts distinct
actors by kind (launcher, named and system). For example:

~~~text
anchored store <store> at seq 42 (<hash>) in .mdkg/anchors/<store>.json; commit it
checkpoint 42: records 1 to 41; actors: launcher 35, named 2 (full list: --json)
~~~

mdkg anchor --json includes the complete sorted list in
data.checkpoint.actors, including actors omitted from the bounded prose
summary. Verified by anchor_text_counts_actors_and_json_keeps_the_complete_list
in crates/mdkg-cli/tests/work.rs.

## Doctor build warnings

Doctor prints one builds row with the stale process count, current and
observed builds, and PIDs grouped by build and process kind. For example:

~~~text
warn builds   mixed mdkg builds: 2 stale launcher/MCP process(es); current 0.7.1 at <new> (clean, built <date>); 0.7.0 at <old> (clean, built <date>): launcher pids [123, 456]
hint: restart stale launcher and MCP processes after upgrading mdkg
~~~

JSON retains the builds check and its coded diagnostic. The same grouped
warnings appear in agents status and frozen-checkout diagnostics, for
Claude, Codex and OpenCode supervisors alike. Verified by
mixed_live_builds_warn_in_doctor_and_status_text_and_json in
crates/mdkg-cli/tests/process_builds.rs and
warnings_group_pids_by_full_build_and_preserve_independent_problems in
crates/mdkg-run/src/builds.rs.

## Graph identity repair

Plain doctor previews the exact clone-local Git identity records that
are missing for an existing, validated graph checkout. For example:

~~~text
warn graph_identity identity repair would write: git config --local --add mdkg.graph.<graph-id>.worktree graph
~~~

Run mdkg doctor --repair to add those records. Repeating it is a no-op.
It keeps conflicting records and refuses dirty graph checkouts, never
advances the checkout or syncs Claude, Codex or OpenCode projections.
This works from the primary, graph or ordinary sibling checkout.
A subgraph source_path under a Git submodule belongs to that separate
repository: parent install and repair skip its identity. Repair or install
the submodule's own graph checkout from that repository instead. A workspace
under a gitlink is still refused because workspaces belong to the parent.
Correct it to a subgraph declaration on the configured main ref, then rerun
install: its guarded fast-forward precedes identity validation. A preview
checks the pinned target tree when that update can proceed. If install
advances the checkout before refusing identity publication, its error names
the updated commit. Doctor never advances the checkout. Repair uses the
shared clone-local graph coordination lock, which may create its lock file.
Verified by doctor_identity_repair_previews_only_identity_writes_and_is_idempotent,
doctor_identity_repair_preserves_conflicts_and_dirty_checkouts,
doctor_identity_repair_adds_only_missing_declared_ids_without_advancing_head,
doctor_identity_repair_finds_moved_graph_from_an_ordinary_sibling,
concurrent_doctor_identity_repairs_do_not_duplicate_records,
doctor_identity_repair_explains_declared_submodule_workspace_boundary,
install_skips_submodule_subgraphs_and_records_only_the_parent,
install_heals_a_refused_gitlink_workspace_after_correction_on_main,
subgraph_symlink_and_blob_parents_refuse_and_preserve_identity_records and
install_identity_refusal_reports_an_already_completed_fast_forward in
crates/mdkg-cli/tests/install_worktree.rs.

## Exit codes

Every command exits with one of these. With `--json`, the envelope's
`diagnostics` carry the stable code that says why.

| Exit | Means |
|---|---|
| 0 | Success, warnings included |
| 1 | Bad arguments: an unknown command or flag, a missing or bad value |
| 2 | A check found errors: `validate`, `verify`, `doctor`, `skills validate`, `entry gate` or `export`'s anchor check, or a write refused because the graph it would leave doesn't validate |
| 3 | Something named doesn't exist (a node, workspace, file, lease, changeset, channel, message or agent), or there's none to act on, such as no active goal |
| 4 | The graph, store or Keychain can't be read or written, or another failure |
| 5 | A coordination conflict, such as a lease someone else holds: sync or pick other work, then retry |
| 6 | Waited and nothing happened: `msg wait` ran out of time |
| 7 | A handle names several nodes; the diagnostic lists them |
