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
  list      List nodes
  show      Show one node
  search    Search ids, titles, fields and bodies, best match first
  index     Rebuild the derived index, or show its state
  graph     Query the graph: refs, backrefs, the ready set, a goal's critical path
  scratch   Locate local scratch shared by the repository's worktrees
  work      Change where work definitions live
  config    Inspect configuration layers and their sources
  pack      Build a markdown context pack around a node or a query, in a token budget
  validate  Validate the graph
  new       Create a node
  done      Finish a work item: record its version and write `status: done`
  update    Set a temporary status (review or blocked), or clear it
  next      Show the next work item to pick up (the full record)
  goal      Show and change goals: current, next, activate, pause, done
  format    Report files whose frontmatter isn't canonical; `--write` fixes them
  retype    Change a node's type: its id and every ref to it (format 0.7)
  claim     Claim a node, file, folder, role or turn
  holds     Check whether you hold a lease covering a scope
  renew     Renew every active lease you hold
  release   Give up your lease without finishing; a lease that already ended is fine
  status    Show leases, the settlement queue and your unacked mail
  sync      Stack this worktree on unmerged versions it lacks
  abandon   Drop a changeset whose branch won't merge, and work built on it
  mcp       Serve the graph and leases to coding agents over stdio MCP
  msg       Send and read messages: send, inbox, wait, ack, thread, import
  room      Manage rooms: create, join, leave, list
  agents    Manage definitions and run agents: add, edit, rm, up, status, resume
  auth      The API key fallback's helper: `auth key <provider>`
  hook      What an agent harness's hook runs: mail at turn start, a reminder before stopping with unread mail, an idle wake, and the edit guard
  install   Set up Claude Code, Codex and OpenCode for this project
  store     List or restore copies of the coordinator store
  anchor    Write the record chain's head hash to an anchor file
  export    Export work state as a bundle anyone can verify
  verify    Verify a work bundle, or this repository's store, against an anchor
  migrate   Bring a 0.6.0 graph onto 0.7.0
  version   Show which build this is: version, commit, dirty tree, build date
  init      Start a graph here: config, setup README and the mdkg-workflow skill
  doctor    Check this setup: build, PATH, git, config, graph, store, mirrors, MCP, agents, hooks
  skills    List, show, validate, add and sync skills in .mdkg/skills
  affected  The tests a change needs: a selective run, or a full one when in doubt
  schema    Print the JSON Schema of a command's output
  help      Print this message or the help of the given subcommand(s)

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

Examples:
  mdkg graph ready
  mdkg graph ready --limit 5 --json
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

Examples:
  mdkg work localize --dry-run
  mdkg work localize --apply sha256:<plan-hash>
```

### `mdkg config`

```text
Inspect configuration layers and their sources

Usage: mdkg config [OPTIONS] <COMMAND>

Commands:
  show  Show merged settings and each value's source, without changing files
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

Examples:
  mdkg config show
  mdkg config show --effective --json
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
          Node id, file or folder path, `role:<name>` or `run:<name>` (names: lowercase a-z, 0-9, `.`, `_`, `-` and `:`, up to 120)

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

Examples:
  mdkg claim <id>
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

Examples:
  mdkg holds src/lib.rs
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

Examples:
  mdkg renew
```

### `mdkg release`

```text
Give up your lease without finishing; a lease that already ended is fine.

`--all` releases every active lease you hold: run it once your head has merged and you've committed everything. Each lease is released on its own, so running it again finishes a run that stopped part way.

Usage: mdkg release [OPTIONS] [TARGET]

Arguments:
  [TARGET]
          Node id, file or folder path, `role:<name>`, `run:<name>`, or lease id

Options:
      --all
          Release every active lease you hold

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

Examples:
  mdkg release <id>
  mdkg release src/lib.rs
  mdkg release --all
```

### `mdkg status`

```text
Show leases, the settlement queue and your unacked mail.

Leases are yours when the actor is named (--as or $MDKG_ACTOR), everyone's with --all; JSON always lists everyone's, marking yours.

Usage: mdkg status [OPTIONS]

Options:
      --all
          Show everyone's leases and expired leases

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

Examples:
  mdkg status
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

Examples:
  mdkg mcp --as <agent>
```

### `mdkg msg`

```text
Send and read messages: send, inbox, wait, ack, thread, import

Usage: mdkg msg [OPTIONS] <COMMAND>

Commands:
  send       Send a message to a room or to mailboxes
  inbox      Show your unacked mail (your mailbox and the roles you hold)
  wait       Wait for mail; exits 6 if none arrives before the timeout
  ack        Ack messages delivered to you
  thread     Show a conversation's messages, oldest first
  import     Record a read-only sender's output (a file) as its message
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

Examples:
  mdkg msg inbox
  mdkg msg send --room dev --kind note --text "Tests pass on main"
```

#### `mdkg msg send`

```text
Send a message to a room or to mailboxes

Usage: mdkg msg send [OPTIONS] --kind <KIND>

Options:
      --room <ROOM>
          Post to this room's members

      --to <TO>
          Send to these mailboxes (comma-separated or repeated)

      --kind <KIND>
          review_request, findings, resolution, question, decision, handoff or note

      --action <ACTION>
          Decision scope: implement, test, commit, design or review. Fixed limits always apply

      --ref <NODE_REF>
          A committed node holding the decision content; required for decision mail

      --id <ID>
          The message id (minted if left out; pass your own to retry safely)

      --context <CONTEXT>
          The conversation (default: the replied-to message's, else a new one).

          A reply joins the replied-to message's context when it goes to the same place, room to room or direct to direct, and a direct reply's conversation holds no room post; otherwise a room post's context is `room:<name>`, and a direct message's its own id.

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

Examples:
  mdkg msg send --room dev --kind note --text "Tests pass on main"
  mdkg msg send --to role:reviewer --kind review_request --text "Please review cs-3"
```

#### `mdkg msg inbox`

```text
Show your unacked mail (your mailbox and the roles you hold)

Usage: mdkg msg inbox [OPTIONS]

Options:
      --mailbox <MAILBOX>
          Read this mailbox instead (yours, a role you hold, or `human:*`)

      --room <ROOM>
          Only mail posted to this room

      --kind <KIND>
          Only these kinds: repeat the flag or separate them with commas (`--kind question,handoff`)

      --room-kind <ROOM_KIND>
          Room mail only of these kinds, while direct mail of every kind still counts (`--room-kind question,handoff`): a wake hook's filter

      --after <AFTER>
          Only mail after this cursor (the `cursor` a previous call returned). A cursor covers the mailboxes it was issued for: after taking a role, read once without `--after` to see the role's backlog

      --limit <LIMIT>
          At most this many messages (1 or more)

      --all
          Acked mail too

      --no-mark
          List without counting as shown, for a reminder that shows headers only (a hook, the MCP server): an ack of mail you were never shown is refused, so read it without this first

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

Examples:
  mdkg msg inbox
  mdkg msg inbox --room dev --json
```

#### `mdkg msg wait`

```text
Wait for mail; exits 6 if none arrives before the timeout

Usage: mdkg msg wait [OPTIONS]

Options:
      --mailbox <MAILBOX>
          Read this mailbox instead (yours, a role you hold, or `human:*`)

      --room <ROOM>
          Only mail posted to this room

      --kind <KIND>
          Only these kinds: repeat the flag or separate them with commas (`--kind question,handoff`)

      --room-kind <ROOM_KIND>
          Room mail only of these kinds, while direct mail of every kind still counts (`--room-kind question,handoff`): a wake hook's filter

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

Examples:
  mdkg msg wait --timeout 300
  mdkg msg wait --after 42 --json
```

#### `mdkg msg ack`

```text
Ack messages delivered to you

Usage: mdkg msg ack [OPTIONS] <IDS>...

Arguments:
  <IDS>...
          Message ids, or the `#N` (or `N`) a listing shows

Options:
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

Examples:
  mdkg msg ack <message-id>
  mdkg msg ack <message-id> --outcome merged
```

#### `mdkg msg thread`

```text
Show a conversation's messages, oldest first

Usage: mdkg msg thread [OPTIONS] [TARGET]

Arguments:
  [TARGET]
          A message id (its conversation), the `#N` a listing shows, or a context id

Options:
      --room <ROOM>
          Every message posted to this room instead, whatever its context

      --after <AFTER>
          Only messages after this cursor (the `cursor` a previous call on this thread or room returned), to page through a long one

      --limit <LIMIT>
          At most this many messages (1 or more), the oldest first

      --tail <TAIL>
          The newest this many messages (1 or more), listed oldest first.

          For catching up on a thread's or a room's recent messages. Not with --limit or --after.

      --member
          With --room: read it only as a member (you or a role you hold), refused with room.not_member before anything is read

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

Examples:
  mdkg msg thread <message-id>
  mdkg msg thread --room dev --tail 20
```

#### `mdkg msg import`

```text
Record a read-only sender's output (a file) as its message

Usage: mdkg msg import [OPTIONS] --kind <KIND> --from <FROM> <FILE>

Arguments:
  <FILE>
          The file: JSON becomes a data part, anything else a text part

Options:
      --room <ROOM>
          Post to this room's members

      --to <TO>
          Send to these mailboxes (comma-separated or repeated)

      --kind <KIND>
          The message kind

      --from <FROM>
          The sender the output belongs to (for example `codex:<run>`)

      --reply-to <REPLY_TO>
          The id of the message this answers

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

Examples:
  mdkg msg import --from codex:run-7 --room dev --kind findings review.json
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
```

### `mdkg room`

```text
Manage rooms: create, join, leave, list

Usage: mdkg room [OPTIONS] <COMMAND>

Commands:
  create  Create a room, with you as a member
  join    Add you (or the given mailboxes) to a room
  leave   Remove you (or the given mailboxes) from a room
  list    List every room, with its members and your unacked counts
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

Examples:
  mdkg room list
  mdkg room join dev
```

#### `mdkg room create`

```text
Create a room, with you as a member

Usage: mdkg room create [OPTIONS] <NAME>

Arguments:
  <NAME>
          The room's name: lowercase letters, digits, `.`, `_` and `-`

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

Examples:
  mdkg room create dev
```

#### `mdkg room join`

```text
Add you (or the given mailboxes) to a room

Usage: mdkg room join [OPTIONS] <NAME>

Arguments:
  <NAME>
          The room's name: lowercase letters, digits, `.`, `_` and `-`

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

Examples:
  mdkg room join dev
```

#### `mdkg room leave`

```text
Remove you (or the given mailboxes) from a room

Usage: mdkg room leave [OPTIONS] <NAME>

Arguments:
  <NAME>
          The room's name: lowercase letters, digits, `.`, `_` and `-`

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

Examples:
  mdkg room leave dev
```

#### `mdkg room list`

```text
List every room, with its members and your unacked counts

Usage: mdkg room list [OPTIONS]

Options:
      --mine
          Only rooms you're a member of

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

Examples:
  mdkg room list
  mdkg room list --mine --json
```

### `mdkg agents`

```text
Manage definitions and run agents: add, edit, rm, up, status, resume

Usage: mdkg agents [OPTIONS] <COMMAND>

Commands:
  add     Create a committed agent definition in .mdkg/agents/
  edit    Update an agent definition; fields not given stay as they are
  rm      Remove a committed definition, keeping local sessions and history
  up      Wake agents on their mail and run their turns under their policies, until stopped
  status  Show the agents, their policies, and the threads the loop guard paused
  resume  Let a thread the loop guard paused run again, or all of an agent's
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

      --reports-to <REPORTS_TO>
          The actor receiving turn receipts (default: orchestrator)

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

          [possible values: extends, cwd, worktree, model, effort, base, instructions, policy, allow, protect, reports_to, mode, sandbox, approvals]

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

      --reports-to <REPORTS_TO>
          The actor receiving turn receipts (default: orchestrator)

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

          [possible values: extends, cwd, worktree, model, effort, base, instructions, policy, allow, protect, reports_to, mode, sandbox, approvals]

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

Examples:
  mdkg agents up
  mdkg agents up --once
  mdkg agents up worker-1 --max-turns 3
```

#### `mdkg agents status`

```text
Show the agents, their policies, and the threads the loop guard paused

Usage: mdkg agents status [OPTIONS]

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

Examples:
  mdkg agents status
  mdkg agents status --json
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

Examples:
  mdkg auth key anthropic
```

### `mdkg hook`

```text
What an agent harness's hook runs: mail at turn start, a reminder before stopping with unread mail, an idle wake, and the edit guard.

Reads the harness's hook input on stdin and prints its output. Says nothing outside a graph; never shows a message's body.

Usage: mdkg hook [OPTIONS] --harness <HARNESS> <EVENT>

Arguments:
  <EVENT>
          The event: start, stop, wait or edit

          Possible values:
          - start: Turn or session start, and each user prompt: the mail line
          - stop:  Before the agent stops: block once on unread wake-worthy mail
          - wait:  An idle session's wait: wake it when wake-worthy mail arrives
          - edit:  Before a tool call: refuse an edit to a path the agent doesn't hold

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

Examples:
  mdkg hook start --harness claude
  mdkg hook edit --harness opencode
```

### `mdkg install`

```text
Set up Claude Code, Codex and OpenCode for this project.

Writes exactly the projections .mdkg/config.json lists: MCP configs, the AGENTS.md block, skill mirrors and hooks. --enable and --disable change the list, then sync.

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

Examples:
  mdkg install --enable agents-md,mcp:claude,skills:.claude/skills --dry-run
  mdkg install
  mdkg install --disable mcp:opencode
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

Examples:
  mdkg verify work.jsonl
  mdkg verify
```

### `mdkg migrate`

```text
Bring a 0.6.0 graph onto 0.7.0.

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

Examples:
  mdkg init
  mdkg init --dry-run
```

### `mdkg doctor`

```text
Check this setup: build, PATH, git, config, graph, store, mirrors, MCP, agents, hooks.

Changes nothing.

Usage: mdkg doctor [OPTIONS]

Options:
      --strict
          Fail on warnings too

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
  add       Add a skill the binary ships (`mdkg-workflow`) to `.mdkg/skills/`, and mirror it
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

Examples:
  mdkg skills validate
  mdkg skills validate mdkg-workflow --json
```

#### `mdkg skills add`

```text
Add a skill the binary ships (`mdkg-workflow`) to `.mdkg/skills/`, and mirror it

Usage: mdkg skills add [OPTIONS] <SLUG>

Arguments:
  <SLUG>
          The shipped skill's slug

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

Examples:
  mdkg skills add mdkg-workflow
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

Examples:
  mdkg schema
  mdkg schema next
```

## Exit codes

Every command exits with one of these. With `--json`, the envelope's
`diagnostics` carry the stable code that says why.

| Exit | Means |
|---|---|
| 0 | Success, warnings included |
| 1 | Bad arguments: an unknown command or flag, a missing or bad value |
| 2 | A check found errors: `validate`, `verify`, `doctor`, `skills validate` or `export`'s anchor check, or a write refused because the graph it would leave doesn't validate |
| 3 | Something named doesn't exist (a node, workspace, file, lease, changeset, room, message or agent), or there's none to act on, such as no active goal |
| 4 | The graph, store or Keychain can't be read or written, or another failure |
| 5 | A coordination conflict, such as a lease someone else holds: sync or pick other work, then retry |
| 6 | Waited and nothing happened: `msg wait` ran out of time |
| 7 | A handle names several nodes; the diagnostic lists them |
