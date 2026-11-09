# Agent definitions

Each committed agent has a JSON file at `.mdkg/agents/<name>.json`.
The filename names its actor and mailbox. Session ids, mail cursors and
loop-guard state remain machine-local, outside the committed definition.

```sh
mdkg agents add coder --harness codex --role worker --model gpt-6.1-sol
mdkg agents edit coder --instructions 'Work on the tasks assigned to you.'
mdkg agents edit coder --policy '{"timeout":900}'
mdkg agents edit coder --wake-kinds question,decision,handoff
mdkg agents edit coder --unset model
mdkg agents rm coder
```

`add` refuses an existing file. `edit` preserves fields you do not pass;
`--unset <field>` removes an optional field, and can be repeated. Use
`--file <path>` to read a complete definition before applying flags.
Both commands validate the complete result before writing it atomically.
`rm` removes only the definition, retaining sessions and history; stop
any running supervisor separately.

```json
{
  "schema": "mdkg.agent.v1",
  "harness": "codex",
  "role": "worker",
  "model": "gpt-6.1-sol",
  "instructions": "Use the project's tasks, leases and mail.",
  "reports_to": "orchestrator",
  "mode": "auto",
  "policy": {"timeout": 900}
}
```

The [JSON Schema](../schemas/mdkg.agent.v1.json) describes the fields.
`harness` is `claude`, `codex` or `opencode`; `role` is `worker` or
`reviewer`. A worker without `cwd` gets its own worktree. A relative
`cwd` is resolved from the launcher's checkout. `reports_to` defaults
to `orchestrator` and names the actor receiving turn receipts and faults,
including turn errors and refused replies.

`wake_kinds` optionally lists the mail kinds that start an automatic turn,
for Claude, Codex and OpenCode alike. Omit it to keep the default direct and
channel wake rules. An empty array disables mail-triggered turns; a bare
`--wake-kinds` writes that array. `--unset wake_kinds` restores the default.
Use comma-separated values or repeat the flag. Unknown kinds are refused.
An explicit list replaces the default direct/channel kind selection, so
including `note` also enables channel notes. Receipts remain non-waking;
hooks and the interactive receiver also leave notes answering the agent's
own mail non-waking. Other mail remains unacked and readable until a turn
starts for eligible mail, a manual prompt, or digest delivery.
Automatic batch turns choose an eligible conversation, paging past
deferred conversations. A conversation containing no waking message is not
delivered by a later automatic batch turn for another conversation; read it
manually or use digest delivery. Digest delivery includes deferred kinds in
its normal budget.

`agents list --json` and the read-only MCP `agents_list` tool expose the
optional list. Definition changes remain operator CLI operations; MCP
does not provide definition mutation tools. Existing supervisors reload
when their committed definition changes.

`mode` defaults to `auto`, the only supported preset in this release.
Other presets are reserved and refused. Optional `sandbox` and `approvals`
strings use each harness's native settings. Codex workers support both
overrides; Claude workers support `approvals` for their permission mode and
refuse `sandbox`. OpenCode keeps the user's defaults and refuses both
overrides. Reviewers are read-only and refuse both overrides for every harness.
See [native execution settings](agents.md#native-execution-settings) for the
mapping. The [state validation tests](../crates/mdkg-run/src/state.rs) cover
accepted and refused definitions, and the
[command tests](../crates/mdkg-run/src/profile.rs) cover native arguments for
fresh and resumed sessions.

The optional string `extends` is reserved for remote definitions. It is
accepted and preserved so a future release can resolve it; this release
explicitly refuses to run a definition using it.

Inline `.mdkg/config.json` agent entries still work with a deprecation
warning. A file replaces the same-name inline entry whole. Shared turn
policy defaults remain under `agents.defaults` in `.mdkg/config.json`.
To move an inline entry, run `mdkg agents edit <name>`, which writes and
commits the new file, then remove the old entry from config. Removing a definition while
its old inline entry remains is refused, so it cannot reactivate an older
configuration silently.

When the supervisor starts, it reads the current definition. Runtime
settings replace the saved spec under the turn lease, including fields
removed with `edit --unset`; local turn counts and mail cursors survive.
Changing harness, role or working folder starts a new session.

```sh
mdkg agents up coder
mdkg agents up coder --once
mdkg agents up coder --once --prompt 'Review the assigned change.' --dry-run
```

`agents up` watches for mail by default. Explicit names with `--once` run
one manual pass, including agents with a manual trigger. Without names,
`--once` uses automatic discovery and its loop guard. JSON output is one
`mdkg.agents.up.v1` event per line; a turn report is in `data.turn`.
See [running agents](agents.md) for launching and sessions.

In checkout mode, `mdkg install` prepares `.worktrees/graph` on `mdkg-graph`, leaving an
existing graph checkout and branch in place. Launch from it with
`mdkg --root .worktrees/graph agents up`. If the main branch has no committed
graph yet, install reports that setup is deferred: commit the initial graph and
run install again. Worktree creation adds `/.worktrees/` to the primary
checkout's `.gitignore`, preserving its existing lines. Projection undo
keeps this rule and the graph worktree, which may hold later work.

In ref mode, definition commands publish through the graph ref. The
launcher reads that ref's pinned authority without refreshing a graph
checkout from main; see [Git and the graph ref](git-graph.md).

`mdkg doctor` checks the builds of registered launchers and MCP servers.
`mdkg agents status` lists those processes and warns about mixed builds or
executables replaced on disk; JSON includes `processes` and `build_warnings`.
Restart stale processes after upgrading. These checks read local build
records without changing them. Only processes holding a record's lock appear;
records left by a killed process are ignored, and older builds may not register.
The registry hierarchy must be owned by the current user and must not be
writable by other users; unsafe directories produce a warning and their
records are not trusted. Receiver diagnostics use read-only snapshots;
when an existing lock prevents a snapshot, status reports a busy receiver.
The [integration tests](../crates/mdkg-cli/tests/process_builds.rs) cover these
diagnostics and the launcher and MCP registration lifetimes.

Graph sync delegates the update to `git merge --ff-only` after checking that
`.worktrees/graph` is on `mdkg-graph`, clean, and behind the target. Existing
ignored or untracked incoming paths, including their parent directories and
symlinks, cause a safe skip. Untracked work or divergent history also skips
the update without changing that work.

An index lock, an unexpected branch, or tracked files that differ from HEAD
freeze sync, install and graph writers through that checkout, including agent
startup. They leave the checkout untouched. Doctor and both status commands
return its state, affected paths and repair guidance even when the config is
unreadable. Frozen status returns no inventory and reads no coordinator store;
it still reports independently readable build warnings. Start with
`git -C .worktrees/graph status`. Preserve wanted edits outside the checkout
before restoring any tracked paths. Remove a leftover `index.lock` only after
confirming that no Git process is still using that worktree. Newly added,
untracked and ignored files are preserved; move any collisions aside yourself.
There is no automatic reset, journal replay or file deletion. Manual changes
to Git internals or worktree registration aliases need operator review beyond
these checkout-state checks. A branch switch
in the narrow interval between the final preflight and Git's merge may advance
that other branch without losing commits; the postflight detects the switch
and requires repair. The [sync tests](../crates/mdkg-run/src/install/worktree.rs)
exercise these boundaries, including a process exit during a partial checkout
and repair using the printed commands. The [CLI freeze tests](../crates/mdkg-cli/tests/graph_repair.rs)
cover writer refusal before config parsing, diagnostics while locks are held,
and graph writes after repair.
