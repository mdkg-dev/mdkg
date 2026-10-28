# Migrating from mdkg 0.6

mdkg 0.7.0 replaces the TypeScript mdkg 0.6.x under the same name. It reads
a 0.6.x graph as it is. `mdkg migrate` then moves the graph to the 0.7
format, in one reviewed commit that you can roll back.

## Before you start

- **One workspace only, for now.** `mdkg migrate` in 0.7.0 converts a
  graph of one workspace, `.mdkg` at the graph's root. It refuses a graph
  with several enabled workspaces, or one kept elsewhere
  (`migrate.unsupported`). Such a graph needs nothing to keep working:
  0.7.0 reads and writes it in its 0.6 form, so stay on that until a
  migration that keeps workspaces lands (planned for 0.7.1). One in
  identity format v2 (below) is the exception: 0.7.0 refuses it outside
  `migrate`, so keep it on mdkg 0.6.x until then. Don't merge
  the workspaces by hand to get past the refusal: their nodes can share
  old ids (`root:task-1` and `aux:task-1`), and their refs name each
  other's.
- **The whole repository moves at once.** After the migration, 0.6.x
  refuses the graph rather than misread it (its config is
  `schema_version: 3`). Upgrade every machine, CI job and agent that runs
  `mdkg` in this repository together.
- **Commit or stash your work.** Migrate refuses a dirty worktree,
  untracked files included (`git stash -u` takes those too), and anything
  staged, a submodule pointer included. Ignored files and changes inside a
  submodule's own checkout don't count.
- **Not a shallow clone.** The `graph_id` comes from the repository's first
  commit, which a shallow clone doesn't have, so migrate refuses one:
  `git fetch --unshallow` first. CI checkouts are often shallow.
- **Merge what you can first.** Every branch that will still merge needs
  its own migration, before it merges ("Branches that merge later",
  below).
- **Install 0.7.0** (see the README) and check the setup:

  ```sh
  mdkg --version      # mdkg 0.7.0 (commit …)
  mdkg doctor         # warns "no graph_id: a 0.6.0-era graph", as expected
  mdkg validate       # 0.7.0 reads the 0.6 graph as it is
  ```

  If `mdkg --version` prints a 0.6 version, the old CLI comes first on
  PATH: `npm uninstall -g mdkg`, then reinstall, or fix PATH. `mdkg doctor`
  says which `mdkg` runs.
- **Back up, outside the repository.** Migrate takes its own backups (step
  2), but keep copies a lost `.git` folder can't take with it. The
  coordinator store is `.git/mdkg/coord.sqlite`; copy it with SQLite's
  online backup, opened read-only so the copy can't change the store:

  ```sh
  mkdir -p ~/mdkg-backup
  sqlite3 -readonly .git/mdkg/coord.sqlite ".backup '$HOME/mdkg-backup/coord.sqlite'"
  cp -R .mdkg ~/mdkg-backup/mdkg
  git bundle create ~/mdkg-backup/repo.bundle --all
  ```

  A repository that has never run 0.7 has no store yet: skip that line.

## 1. Plan (a dry run)

```sh
mdkg migrate --dry-run
```

This changes nothing in the graph. It prints what the migration does and
the plan's hash. For review, it writes the full plan (`plan.json`) and a
patch for files outside `.mdkg` (`rewrite-refs.patch`) into the
repository's git folder, under `mdkg/migrate/`, and prints where. That
folder is shared by all worktrees, with one folder per worktree and plan.

Read the plan. For each node it says:
- **A new id:** every node gets `<type>-<uuidv7>`. The UUID's time is the
  node's old `created` date, so ids keep the old order. The old id stays in
  the node as a read-only `legacy_id`, and commands still resolve it.
  Each node that stays a file keeps its name and path (`task-19-parser.md`
  stays where it is); only its contents change.
- **Its plane:** what stays a knowledge file, and what becomes coordination
  records:
  - checkpoints become log records, each leaving a one-line summary;
  - finished test nodes become check results;
  - `events.jsonl` lines become legacy log records;
  - progress and review-ledger sections become review records.
- **Rewritten refs:** every ref field in the graph moves to the new ids. A
  mention of an old id in a body's prose becomes a link to the node,
  `[task-19](task-<uuid>)`, and in code the new id. Every other value keeps
  its mentions as written: titles, `goal_condition`, links, artifacts and
  extension values. Old ids there still resolve through `legacy_id`.
- **Renamed keys:** 0.6.0's `x_` extension keys become
  `x-<namespace>.<field>`. A key a local template added, which the 0.7 node
  schema doesn't have, becomes `x-local.<key>` with its value unchanged, and
  a warning lists each one. Two keys that would end up with one name (say
  `colour` beside `x_local_colour`) stop the plan.
- **Goal gates:** gates that call `capability search` are rewritten to
  `mdkg search`.

Warnings name what migrate leaves for you: tier C content kept as plain
nodes, a ref it couldn't resolve, a key it couldn't split.

**The ids are deterministic.** The `graph_id` comes from the repository's
first commit, and a node's new id from its old date, number and file, never
from what else a branch holds. So the dry run and the apply agree on the
plan hash, and every branch of the repository gives a node the id main
gives it. The [format](format-0.7.md#101-migration-identity) has the exact
derivation.

## 2. Apply the plan you reviewed

```sh
mdkg migrate --apply <plan-hash>
```

It applies exactly the plan with that hash. It refuses if anything changed
since the dry run (`migrate.plan_mismatch`), or if a different plan is
still unfinished (`migrate.in_progress`). Before it changes anything, it backs up:
- the base commit, tagged `mdkg-pre-0.7` (or `mdkg-pre-0.7-<commit>` when
  that tag is taken);
- the coordinator store, when the repository has one, as a copy beside it
  (`coord.sqlite.pre-<schema>-to-migrate-<plan12>.sqlite`, where `<plan12>`
  is the plan hash's first 12 characters), which `mdkg store snapshots`
  lists and `mdkg store restore` takes;
- a full copy of `.mdkg`, in the migration's folder.

It then swaps the graph in at once and writes the migration as one commit.
If it stops part way, run the same `--apply <plan-hash>` again to finish,
or `--rollback` to undo it from any stage. A rollback that stops part way
is finished by `--rollback` again; `--apply` refuses it. `mdkg migrate
--status` shows how far it got.

### Instruction files, skills and scripts

Files outside `.mdkg` that mention old ids or 0.6.0 commands are covered
by `rewrite-refs.patch`: `AGENTS.md`, `CLAUDE.md`, skills and scripts.
Migrate never changes them unless you ask. To apply the patch in the same
commit:

```sh
mdkg migrate --apply <plan-hash> --rewrite-refs
```

`--rewrite-refs` is opt-in: a plain `--apply` never changes your files
outside `.mdkg`. If you use it, close the files it patches in your editor
first, or the editor may save its old copy over the patch. If it stops
part way, a `.<file>.migrate-<pid>` file (say `.AGENTS.md.migrate-4821`)
can be left beside a patched file. It holds that file's old content.
Check the patched file is right, then delete the `.migrate-<pid>` file.

Or apply it yourself later with `git apply`, after reading it.

## A trial run in a separate worktree

To see the result before main moves, migrate a throwaway branch in its own
worktree, check it, and only then bring main to it:

```sh
git worktree add ../my-project-migrate -b migrate-0.7 main
cd ../my-project-migrate
mdkg migrate --dry-run
mdkg migrate --apply <plan-hash>
mdkg validate && mdkg verify && mdkg doctor
```

If a check fails, run `mdkg migrate --rollback` in the trial worktree,
**before** main moves: once it has, the next command records that, and
rollback refuses (`migrate.store_changed`). If every check passes,
fast-forward main to the migration commit from the main checkout, then
remove the trial worktree:

```sh
cd ../my-project
git merge --ff-only migrate-0.7
git worktree remove ../my-project-migrate && git branch -d migrate-0.7
```

The coordinator store is shared by all worktrees of a repository, so run
the trial at a quiet point, with no agents working, and don't make claims
or send messages until main has moved.

## 3. Check, and refresh the agents' setup

```sh
mdkg doctor          # config: graph <graph_id>; migrated work stays in git
mdkg validate
mdkg list --status todo
mdkg show <old id>   # a legacy_id still resolves
```

Migration preserves work definitions in git. New projects default to local
work with `mdkg init --work local`; use `mdkg init --work shared` to commit
work definitions. Config uses `work.location: "local"|"git"`; the older
`work.committed: "none"|"definitions"` spellings remain readable aliases.
Changing init's choice on an existing graph refuses rather than moving nodes.

Temporary files can use the repository's local scratch area:
`mdkg scratch path` prints `<git common dir>/mdkg/scratch/<actor>`, and
`mdkg scratch path --shared` prints the `shared` folder beside it. The
`scratch_path` MCP tool provides the same paths. The query creates nothing;
create the folder when needed. Scratch is shared across this machine's
worktrees, outside the graph and git history, with no migration required.
The [scratch tests](../crates/mdkg-cli/tests/scratch.rs) check this behavior.

If agents use this repository, refresh what 0.6.0 set up for them:
- `mdkg install` refreshes the enabled projections recorded by migration:
  MCP configs point at the `mdkg mcp` server, and the managed `AGENTS.md`
  block and skill mirrors are refreshed when enabled;
- or, for the skill mirrors alone, `mdkg skills sync`.

`mdkg doctor` warns when an MCP config starts a program that isn't on
PATH.

### Earlier 0.7 launcher configurations

If you tried an earlier 0.7 development build, `mdkg run` has been removed
without an alias. Use `mdkg agents add`, `edit` and `rm` to manage committed
`.mdkg/agents/<name>.json` definitions, then `mdkg --root .worktrees/graph
agents up` to run them. Definition fields such as `--harness` and `--role`
belong on `agents add` or `agents edit`, not on `agents up`. Deprecated
inline agent definitions remain readable. Definition management commands
now commit their changes; omit the manual definition commits used by earlier
development builds. For tracked changes left by an older or interrupted
edit, follow [graph checkout recovery](agents.md#the-graph-checkout).
The [agent CLI tests](../crates/mdkg-cli/tests/agents.rs)
cover these compatibility boundaries.

Older 0.7 development checkouts without clone-local graph registration need
`mdkg install` from the primary checkout. Install records mappings in this
clone's common Git configuration for the committed root graph and any separate
0.7 graph identities in committed workspace and local subgraph configurations
directly declared by the root. Each clone installs independently. Missing or
conflicting records stop writes through the managed graph checkout with repair
guidance; install does not
overwrite conflicts or prune stale records automatically. See
[the graph checkout](agents.md#the-graph-checkout).

`mdkg config show --effective --json` now reports merged settings and their
sources. User and machine-local agent, orchestration and projection settings
are displayed with a transitional notice; runtime commands do not yet apply
those overrides. See [configuration inspection](agents.md#inspecting-configuration).

Workers now commit their own work; the launcher never stages or commits it.
Claude Code, Codex and OpenCode are equally supported, using the harness's
permissions and your settings. The default worker mode is `auto`; reviewers
remain read-only. Follow [the current setup sequence](agents.md) for native
overrides and [graph checkout updates and recovery](agents.md#the-graph-checkout).

OpenCode workers now require version 2.1.0 and an enabled `build` agent.
Each actual launch checks OpenCode's resolved configuration before starting
the worker. The check uses ordinary OpenCode state and can have native
startup side effects. See [OpenCode worker checks](agents.md#opencode-worker-checks).

Delegated approval is an explicit local opt-in. Grants belong in user or
machine-local settings; project configuration and ordinary mail cannot grant
them. Approval binds one permitted action to an exact committed node version.
See [delegated decisions](delegated-decisions.md) for configuration and the
advisory limits.

Turn-end receipts sent by mail, durable headless turn progress and crash
recovery, runtime configuration layering for `agents up`, and `config set`
are planned for 0.7.1. The existing loop-guard `agents resume`, paused-thread
status and interactive Codex routing recovery remain available; see the
[turn policy](agents.md#waking-agents-agents-up-and-the-turn-policy).

## Making shared work local

After migration, shared work can be moved beside the store while keeping
its ids, contents and references to design. Stop agents and other graph
writers, commit changes inside `.mdkg`, and use a feature branch:

```sh
mdkg work localize --dry-run
mdkg work localize --apply <full-plan-hash>
mdkg validate
```

Review the listed node moves and use the full `sha256:…` hash from the dry
run. The command refuses committed references into the prospective local
work set. Replace them with references to a committed goal, epic or other
container, letting work point back to that container. It also refuses
conflicting leases, imported workspaces, existing private work from another
checkout, or Git metadata on another filesystem. These checks leave the
definitions in place. Unrelated staged code is preserved.

Apply copies tasks, bugs, spikes, tests and checkpoints into the local work
folder, switches the graph atomically, and commits the config change and
node removals. Merge this commit before restarting writers in other
worktrees. Each machine that needs these work definitions must retain its
own local copy before merging the removals; a fresh clone gets design only.
No data is copied to another machine by the conversion.

The receipt names a retained `backup.mdkg` under the git common directory's
`mdkg/localize/` folder. If apply is interrupted, keep writers stopped and
repeat the same command with the same hash. Source edits or changed local
copies are refused rather than overwritten; reconcile those edits first.
The backup and displaced source tree stay outside git history.

To undo a completed conversion, first preserve a copy of the current local
work folder, including work created or edited since conversion. Revert the
conversion commit with your normal Git workflow, then validate. Its config
and committed definitions return; local files remain intact beside the
store and are no longer read by shared-work config. Reconcile newer local
work before adding any of it back to git. For an interruption before a
commit, preserve both local work and any later graph edits, then restore
only the config and listed node paths from the retained backup if resuming
is not possible. Do not restore the whole backup over subsequent edits.

The [localize CLI tests](../crates/mdkg-cli/tests/localize.rs) cover plan
staleness, reference and lease refusals, exact commit scope and idempotence;
[filesystem tests](../crates/mdkg-cli/src/work/apply.rs) cover interruption
and non-overwrite behavior.

## Branches that merge later

A branch cut before the migration still has 0.6 files. **Migrate it on its
own, before it merges**, not after merging main into it (a branch that has
main's migrated config is refused as already migrated):

1. In the branch's worktree, with a clean `git status`, run `mdkg migrate
   --dry-run`, then `mdkg migrate --apply <plan-hash>`.
2. Merge it as usual. The ids are deterministic, so the branch rewrote
   every node it shares with main exactly as main's migration did: git sees
   the same change on both sides, and only the branch's own edits can
   conflict. Use the same mdkg release main migrated with, so both sides
   rewrite alike. Records main's migration already put in the coordinator
   store aren't imported twice.

A branch that will never merge needs nothing.

## Rolling back

```sh
mdkg migrate --rollback
```

This puts the old graph, the base commit and the coordinator store back
from the backups. Roll back only before the migration commit is pushed.
Once others may have it, revert it with `git revert` and restore the
store with `mdkg store restore <snapshot>` instead.

Rollback refuses, saying why:
- **The store changed since the migration** (new claims, messages):
  restoring it would lose those changes (`migrate.store_changed`, naming
  what changed).
- **The worktree has changes**, after a finished migration, or the
  migrated graph was edited after one that stopped part way
  (`migrate.dirty`).
- **A commit landed on top of the migration commit** (`migrate.moved_on`):
  reset the branch to the migration commit first, or revert instead.

`--rollback --force` overrides the store check and the edits to a graph
that stopped part way: the store changes and those edits are lost. It
never overrides a commit on top of the migration commit, or a dirty
worktree when a rollback of a finished migration starts. That check runs
only as a rollback starts: if a rollback stops part way, edits made to the
graph after that are lost by a `--rollback --force` retry, so commit or
copy them first.

## Commands that changed

For one release, the busiest 0.6.0 spellings still run their new
equivalents, each printing a `usage.deprecated` notice with the new name,
in JSON output too (`skill` is simply accepted). They go away in 0.8.

| 0.6.0 | 0.7.0 |
|---|---|
| `mdkg task start <id>` | `mdkg claim <id>` |
| `mdkg task update <id> --status …` | `mdkg update <id> --status review\|blocked`; the alias claims for `progress` and finishes for `done`, and refuses field edits. `backlog` and `todo` are edits to the file's `status:` line |
| `mdkg task done <id>` | `mdkg done <id>` (a `--checkpoint` title is accepted and ignored) |
| `mdkg goal claim [goal] [work]` | `mdkg goal next`, then `mdkg claim <id>` |
| `mdkg checkpoint new …` | `mdkg new checkpoint …` |
| `mdkg skill …` | `mdkg skills …` (accepted, with no notice) |

Two more changed, with no alias:
- **`mdkg capability search …`** is now `mdkg search …`. Migrate rewrites
  the goal gates that call it.
- **`mdkg upgrade`** is replaced by `mdkg init` for new graphs, `mdkg
  doctor` for checks, and `mdkg migrate` for 0.6 graphs.

Status changes:
- **Progress, review and blocked** are no longer written into files. They
  live in the coordinator: `claim` marks work in progress, and `update
  --status review|blocked` sets the others.
- **Lasting statuses** (`backlog`, `todo`, `done`) stay in the files.

Gone, with no replacement: `fix`, `handoff`, the workspace commands, TOON
and XML output, the `spec` alias, `graph clone/fork/reconcile/recover` and
`bundle import`. See the [changelog](../CHANGELOG.md).

## Identity format v2 graphs

A graph that 0.6.0's `mdkg graph migrate --apply` moved to identity format
v2 (config `schema_version: 2` and `.mdkg/graph.json`) migrates the same
way, and keeps its UUIDs: the graph keeps its `graph_id`, and each node's
`node_id` becomes its new id (`task-4` with `node_id: e3d2…` becomes
`task-e3d2…`), with the old id as its `legacy_id`. The per-node
`graph_id` and `node_id` keys and `.mdkg/graph.json` go. An
`mdkg://<graph>/<node>` ref to one of the graph's own nodes becomes its
plain id. A ref to another graph's node becomes
`mdkg://<graph>/<type>-<uuid>` where that graph is a configured subgraph;
otherwise its type is unknown, so a ref field moves it to the node's
"Migrated records" with a warning, and a body keeps it as written. Every
other 0.7 command refuses a v2 graph until it's migrated.
