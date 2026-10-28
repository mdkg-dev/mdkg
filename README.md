# mdkg

Shared memory and traffic control for coding agents and the people working
beside them, in one git repository.

When several agents (Claude Code, Codex, OpenCode) and people work on one
codebase, they need to know what was decided, what's next, and who is
already editing what. mdkg keeps goals, decisions, designs, rules and
skills as markdown files in `.mdkg/`, committed with your code. Tasks and
other work definitions are local by default; shared work keeps them in git.
It keeps coordination in a local SQLite store inside
`.git/`: claims on tasks and files that expire after two hours, temporary
statuses, and messages between agents. There's no server or daemon, and
nothing to host. Agents use it through the `mdkg` CLI or its MCP server.

mdkg is a single Rust binary. New here? [Concepts](docs/concepts.md)
explains ids, the two planes, agents and search in a few pages, and
[the command reference](docs/commands.md) lists every command's options.

## Two planes, plus a cache

| Plane | Holds | Lives in |
|---|---|---|
| Knowledge | Goals, decisions, designs, rules, skills, task definitions and their final states | Markdown with frontmatter: design in `.mdkg/` and git; work beside the store by default, or in git with shared work |
| Coordination | Claims and leases, temporary statuses, rooms and messages, and a hash-linked log of every change | A SQLite store in the repository's git folder, never committed |

Search indexes are a derived cache: rebuilt from the files, never
committed.

Every node has an id like `task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b`: its
type and a UUIDv7. Any machine or agent mints one without asking anyone,
so branches never collide. Commands also take a unique prefix of the id,
or words from the title. [docs/format-0.7.md](docs/format-0.7.md) is the
file format, versioned and frozen for 0.7.

Upgrading from the TypeScript mdkg 0.6.x? 0.7 replaces it under the same
name, and reads and migrates 0.6 graphs: see
[migrating from 0.6](docs/migrating-from-0.6.md).

## Install

| | |
|---|---|
| Homebrew | `brew install mdkg-dev/tap/mdkg` |
| npm | `npm install --global mdkg` |
| Shell (macOS, Linux) | `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/mdkg-dev/mdkg/releases/latest/download/mdkg-installer.sh \| sh` |
| Cargo | `cargo install mdkg --locked` |
| PyPI | `pipx install mdkg`, or `uv tool install mdkg` |

There are prebuilt binaries for macOS (Apple silicon and Intel) and Linux
(x86_64 and arm64, glibc and musl). Each release lists their SHA-256
checksums in `sha256.sum`. Windows is planned for 0.7.1; until then, use
WSL. Requires Git 2.41 or later.

## Quick start

```sh
cd my-project              # a git repository with at least one commit
mdkg init                  # asks where work lives (below); writes config, a setup guide and one skill inside .mdkg
mdkg doctor                # checks the binary, PATH, git, the main branch, config and store
mdkg new goal "Ship the importer"
mdkg new task "Parse the CSV header" --status todo
mdkg next                  # the next ready item, with its full record
mdkg claim <id>            # a lease, so no one else takes it
mdkg done <id>             # records the final state (and, with shared work, commits it)
```

Work settles on the repository's main branch, `main` unless you name
another with `git config mdkg.mainRef <branch>` (on `master`, say). In a
new repository, make the first commit before `mdkg new`: `git add -A &&
git commit -m "Start the graph"`. `mdkg init` and `mdkg doctor` say which
of the two you need.

Add `--json` to any command for one machine-readable envelope with stable
diagnostic codes. `mdkg schema <command>` prints its JSON Schema.

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


## Coding agents

`mdkg init` writes only inside `.mdkg/`; the first worktree creation adds
`/.worktrees/` to `.gitignore`.
Each file agents read from the project's root is a projection you opt
into; `.mdkg/config.json` lists the enabled ones under `projections`, and
`mdkg install` writes exactly those (a config without the key, from
before it, gets every one but the hooks):
- `agents-md`: a short block in `AGENTS.md`;
- `mcp:claude`, `mcp:codex`, `mcp:opencode`: each one's MCP config;
- `skills:.agents/skills`, `skills:.claude/skills`: a mirror of
  `.mdkg/skills/`;
- `hooks:claude`, `hooks:codex`, `hooks:opencode`: the hooks canonical in
  `.mdkg/hooks/`, in each harness's own file.

```sh
mdkg install --enable agents-md,mcp:claude,skills:.claude/skills
mdkg install --disable mcp:claude    # removes only mdkg's part
```

`mdkg install --undo` puts back every file install changed, the list
included. Review hooks before enabling them: Claude Code and Codex have
their own trust controls; OpenCode executes project plugins without a trust
prompt. A graph migrated from 0.6 lists what it already had.

To set up an agent by hand, `mdkg mcp` is a stdio MCP server. Its tools:
- reads: `show`, `list`, `search`, `next` and `pack`;
- leases: `lease_claim` and `lease_release`;
- mail: `room_post`, `room_read`, `inbox`, `inbox_wait` and `ack`.

Claude Code, in `.mcp.json`:

```json
{ "mcpServers": { "mdkg": { "type": "stdio", "command": "mdkg", "args": ["mcp"] } } }
```

Codex, in `.codex/config.toml` (the project must be trusted):

```toml
[mcp_servers.mdkg]
command = "mdkg"
args = ["mcp"]
```

OpenCode, in `opencode.json`:

```json
{ "mcp": { "mdkg": { "type": "local", "command": ["mdkg", "mcp"], "enabled": true } } }
```

Message bodies are written by other agents, so mdkg labels them as
untrusted data in every result.

`mdkg agents up` starts Claude Code, Codex and OpenCode agents in their own
worktrees and in your own environment, waking them on mail.
All three are first-class harnesses. Workers own their commits; the launcher
never stages or commits their work. The default mode is `auto`: Claude Code
and Codex use their native automatic approval modes, and OpenCode keeps your
own permissions. Reviewers remain read-only. See the
[native settings and their tests](docs/agents.md#native-execution-settings).

Commit the graph and enabled integrations through your normal Git workflow
first. From the main checkout, create the graph worktree with `mdkg install`,
then define the agents in that checkout. Choose the harnesses you use:

```sh
mdkg install
mdkg --root .worktrees/graph agents add claude-worker --harness claude --role worker
mdkg --root .worktrees/graph agents add codex-worker --harness codex --role worker
mdkg --root .worktrees/graph agents add opencode-worker --harness opencode --role worker
mdkg --root .worktrees/graph agents up
```

Definitions are committed `.mdkg/agents/<name>.json` files. `agents add`,
`agents edit` and `agents rm` commit their changes in the graph checkout
and report the commit. Each command commits only the affected definition.
Sessions and mail stay local. The
launcher reads the graph checkout's definitions, so keep that checkout
current using the [graph checkout guidance](docs/agents.md#the-graph-checkout).
`mdkg init --branch-pattern` and `--worktree-pattern` set the
branch and worktree each agent gets (default `agent/{agent}` in
`.worktrees/{agent}`). See [docs/agents.md](docs/agents.md).

## Commands

| Area | Commands |
|---|---|
| Setup | `init`, `doctor`, `install`, `migrate` |
| Read | `show`, `list`, `search`, `next`, `validate`, `pack`, `graph refs`, `graph backrefs`, `graph ready`, `graph critical-path` |
| Write | `new`, `update`, `done`, `format`, `goal`, `retype` |
| Coordinate | `claim`, `holds`, `release`, `renew`, `status`, `sync`, `abandon` |
| Talk | `msg send`, `msg inbox`, `msg wait`, `msg ack`, `msg thread`, `msg import`, `room` |
| Skills | `skills list`, `skills show`, `skills validate`, `skills sync`, `skills add` |
| Agents | `mcp`, `agents add`, `agents edit`, `agents rm`, `agents up`, `agents status`, `agents resume` |
| Log | `anchor`, `export --work`, `verify` |
| Store | `index rebuild`, `index status`, `store snapshots`, `store restore`, `version`, `schema` |

`mdkg <command> --help` describes each one, and
[docs/commands.md](docs/commands.md) collects them all. A handle that names several
nodes fails with exit code 7 and lists them. For one release, the busiest
0.6.0 spellings still work (`task start`, `task update`, `task done`,
`goal claim`, `checkpoint new`), each printing a `usage.deprecated` notice
with its new name, and `skill …` is accepted for `skills …`.

## Skills

`mdkg init` seeds one skill, `mdkg-workflow`, and enabling the
`agents-md` projection adds a block to AGENTS.md. Together they hold the
steps every agent follows, whatever its harness. mdkg is the system of
record, so each turn starts with `mdkg msg inbox` and `mdkg status`, every
edit is claimed first, questions and handoffs go through mdkg mail, and
work closes with `mdkg done`, not a harness's own task list or messaging.
A project's rules add to these steps. `mdkg skills add mdkg-workflow`
adds the skill to a graph that lacks it, and `mdkg doctor` says when the
skill or the block is older than the build. A skill is a folder in
`.mdkg/skills/` holding a `SKILL.md`. Its frontmatter needs a `name` and a
`description`. `tags`, `version`, `authors` and `links` are optional.
`mdkg skills validate` checks every skill, and checks that the enabled
mirrors (`skills:.agents/skills`, `skills:.claude/skills`) match.
`mdkg skills sync` copies them across. Edit the `.mdkg` copy; the mirrors
are overwritten.

## Documentation

- [Concepts](docs/concepts.md): the two planes and where work lives, ids
  and handles, coordination, agents, search.
- [Command reference](docs/commands.md), generated from `--help`.
- [The file format](docs/format-0.7.md), frozen for 0.7, with its
  [node schema](crates/mdkg-format/schema/node-0.7.json).
- [Running agents](docs/agents.md).
- [Migrating from 0.6](docs/migrating-from-0.6.md).

## Developing

```sh
cargo test --workspace
scripts/check.sh           # fmt, clippy, the tests, and 0.6.0 compatibility checks
scripts/commands-doc.sh    # regenerate docs/commands.md after changing a command's help
```

See [CONTRIBUTING.md](CONTRIBUTING.md). Releases are cut by
[docs/releasing.md](docs/releasing.md).

## License

MIT. See [LICENSE](LICENSE).
