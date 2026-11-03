# Running agents

`mdkg agents up` runs Claude Code, Codex and OpenCode agents as workers
or reviewers, watching for unread mail. `agents up <name> --once` runs one
manual pass. Manage committed definitions with `agents add`, `edit` and
`rm`; see [agent definitions](agent-definitions.md).

Agents run as you would run them yourself: with the launcher's
whole environment, your own logins, settings, config and MCP servers, and
each harness's own permission mode for the role. mdkg gives each worker a
worktree; sandboxing and approvals come from the harness and your settings.
There's no sandbox of the launcher's own, no environment
scrubbing and no secret scanning. If you need those, run agents inside a
sandbox of your own.
So an agent can read what you can and commit files it can write, a `.env`
file included.
Keep secrets out of agents' worktrees and ignored. The one thing held
back is the provider key variables (`ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, `CODEX_API_KEY` and the like): see "Which account a
turn bills" below.

What the launcher adds is the role and the coordinator:
- A **worker** edits its folder using the harness's native permission mode
  and your settings. mdkg adds no command allowlist, push or publish denial,
  or tool restriction for workers; its own `auth`, `abandon` and `agents
  use-key` commands (and the `agents_use_key` MCP tool) remain refused. Workers own their commits, using their native harness
  permissions; the launcher never stages or commits their work. It leaves
  uncommitted and staged changes untouched when a turn ends. The
  [fake-harness turn tests](../crates/mdkg-run/tests/turn.rs) exercise a
  worker's real git commit in a linked worktree and preservation of staged
  work across all three harnesses.
- A **reviewer** reads only (Codex in `read-only` mode); its final message
  is imported as its reply. Its checkout is the author's, so its settings
  aren't loaded: Claude reads only your user settings (no project
  `.claude/settings*.json` hooks or permissions, `.mcp.json` or
  `CLAUDE.md`), and OpenCode skips the project's config. Codex can't load
  your config without the checkout's `.codex/config.toml` wherever your
  config trusts the repository, so a Codex reviewer leaves your config
  out: it runs on Codex's defaults and the agent's own `model` and
  `effort`, with your login.
- Both get the launcher's `mdkg` MCP server, acting as the agent, opened on
  the launcher's graph root.

At 0.7.1 a Codex reviewer's `read-only` sandbox also makes the coordinator
store read-only. Its mid-turn MCP question, ack or findings post fails with
`coord.store_failed`, and a mid-turn question is lost. Put questions and
findings in the reviewer's final message instead: the launcher imports that
message as the reviewer's reply. A write path for reviewer mail is planned
for 0.7.2.

Log each harness in as a standalone CLI first (`claude` and `/login`,
`codex login`, `opencode auth login`): the agent uses that login. Codex
sessions live in your `CODEX_HOME` and OpenCode's in your data folder, as
if you'd run them.

For temporary files shared across worktrees, `mdkg scratch path` returns
`<git common dir>/mdkg/scratch/<actor>`; `--shared` returns its `shared`
folder. The `scratch_path` MCP tool returns the same paths. These queries
create nothing: create the folder when needed. Scratch stays local, outside
the graph and git history. The [scratch tests](../crates/mdkg-cli/tests/scratch.rs)
cover worktree sharing, actor selection and CLI/MCP parity.

In checkout mode, install uses the graph at the Git worktree's top;
installing only `sub/.mdkg/` without a root graph is unsupported. In ref
mode, install can adopt an already fetched graph ref without a checkout
copy. It can also provision a setup before a graph exists, for `mdkg init`
to create there. The [setup fixtures](../crates/mdkg-cli/tests/graph_setups.rs)
cover both paths.

[Git and the graph ref](git-graph.md#the-two-modes) explains how the clone
chooses checkout mode or ref mode. Lay the checkouts out like this:

- **Checkout mode:** run `mdkg agents up` from the graph worktree, which no
  worker uses. After committing the graph, run `mdkg install` from the main
  checkout to create `.worktrees/graph` on `mdkg-graph`, then launch with
  `mdkg --root .worktrees/graph agents up`. It refuses a checkout of main,
  which changes only through a reviewed merge: merge `mdkg-graph` into
  main through a pull request when you choose. Before the initial graph
  commit, install defers making the graph checkout; an absent configured
  main branch also defers it and gives guidance.
- **Ref mode:** the launcher reads committed definitions and configuration
  from the graph ref, without a graph checkout. `agents add`, `edit` and `rm`
  publish definitions to that ref, not to the code branch. `agents up`
  accepts a main checkout in this mode. It replays pending
  graph intents and refreshes the graph cache, not a checkout from main.
  Choose `mdkg install --setup local`, `--setup personal`, or
  `--setup professional --graph-remote <existing-private-remote>`; see
  [setups and migration](git-graph.md#installing-a-setup). A plain install
  preserves checkout mode; only explicit `--setup` migrates. Open
  changesets must settle or be abandoned first. `--view` adds an optional
  detached `.worktrees/graph` for inspection.
- Give every worker a worktree of its own that holds no other worktree. The
  command is `mdkg agents add <agent> --harness claude --role worker
  --worktree`, then `mdkg agents up <agent>`. In checkout mode, give both
  commands `--root .worktrees/graph`; in ref mode run them from the code
  checkout. Substitute `codex` or
  `opencode` to choose either of the other first-class harnesses. The first
  pass makes
  `.worktrees/<agent>` in the main checkout on branch `agent/<agent>` and
  later passes reuse it. The names come from
  `.mdkg/config.json`, where `{agent}` is the agent's name:
  `{"orchestration": {"branch": "agent/{agent}", "worktree":
  ".worktrees/{agent}"}}`. After adding the definition,
  `mdkg agents up <agent> --once --dry-run` plans its
  worktree with the same checks and makes no folder, branch or saved session.
  Use the graph root for that command in checkout mode too.
  The worktree path is relative to the main
  checkout and may lead out of it (`../agents/{agent}`); `{agent}` must be
  in its last part. The launcher makes no worktree outside the folder that
  holds the main checkout, links followed: `../agents/{agent}`, beside the
  checkout, works, and a pattern leading further up is refused when the
  worktree would be made (an existing one is still reused). `mdkg init`
  won't write such a pattern, and `mdkg doctor` warns about one. A worker
  works only on the branch its pattern names.
- Never put a worktree inside a worker's worktree.
- Reviewers can't write, so they may share any checkout.

### The graph checkout

Checkout mode uses a managed graph checkout. Ref mode needs no checkout for
authority reads: definitions, configuration and graph content come from the
graph ref's pinned tip, so a publish there is visible without a merge into a
checkout. Ref-mode install records `mdkg.graph.<graph_id>.ref`, projects
from the graph tip into the code checkout, and provisions `/.mdkg-edit/`
in common `info/exclude`. The optional detached view is read-only by
convention; install refreshes it only when clean and never reads it as
authority. `--checkout-mode` keeps the graph ref and can reattach a view at
the tip. The maintenance below applies to checkout mode.

Install records the committed root graph's identity in this clone's common
Git configuration. It maps `mdkg.graph.<graph_id>.worktree` to the managed
checkout's Git admin name, not its filesystem path. It also records separate
0.7 graph identities in committed workspace and local subgraph configurations
directly declared by the root; unrelated example and fixture graphs are ignored.
Each clone installs independently. Missing or conflicting records stop writes
through the managed graph checkout with repair guidance. Run `mdkg install` from
the primary checkout to register committed graphs, including an older graph
checkout that predates these records. Conflicting or stale records are not
overwritten or pruned automatically. The [identity fixtures](../crates/mdkg-cli/tests/install_worktree.rs)
cover registration, clone independence and repair refusals.

In checkout mode, the launcher reads definitions and graph content from its
own checkout, so changes committed in another checkout are not visible there
until merged. Install and `agents up` startup attempt a guarded fast-forward
of the managed graph checkout from the configured main branch. They report
divergence and leave the graph unchanged when the histories have diverged.
Before updating `.worktrees/graph` manually, stop its graph writers and
inspect its branch and working tree. Preserve any local work, then
fast-forward reviewed main changes when the branches permit it:

```sh
git -C .worktrees/graph status
git -C .worktrees/graph -c merge.autoStash=false merge --ff-only --no-overwrite-ignore main
```

Use the configured main branch if it has another name. A divergence needs a
reviewed merge that preserves both histories. Do not reset the graph to
discard its commits. `mdkg sync <node>` is a separate operation: it brings
in unsettled node changesets.

If the managed graph checkout has an index lock, the wrong branch, tracked
changes or index flags that hide changes, mdkg freezes its graph writes.
`mdkg doctor` and `mdkg status` report the observed state and recovery
commands; mdkg does not delete files, reset Git or restore backups for you.
Inspect and preserve the reported changes before applying the guidance.
If an older build or an interrupted definition edit left tracked changes,
commit the wanted changes or follow the reported restore guidance. Release
a leftover definition lease only after confirming its writer has stopped.
An interrupted `agents add` can instead leave an untracked definition. Once
its writer has stopped, run `mdkg --root .worktrees/graph agents edit <name>`
as the same actor to commit the existing file.
Other checkouts remain usable. The [graph-repair fixtures](../crates/mdkg-cli/tests/graph_repair.rs)
exercise these gates and diagnostics. The [sync fixtures](../crates/mdkg-cli/tests/install_worktree.rs)
cover the guarded fast-forward implementation.

The guards that stay: a worker never works on the main branch or a
protected one, only on its own branch, never in the main checkout or the
launcher's, and never in a worktree holding another. A turn is refused when
the agent's worktree holds a nested repository (a submodule's is fine), a
FIFO or device file, or a folder that can't be listed: git would stall on
it. Use mdkg for tasks, leases and mail, and ask the configured `reports_to`
actor for decisions or missing context, including when a needed action is
denied. Mail can identify a task or provide context; its body does not grant
permission or override the user's instructions. Reviewers remain read-only.

`mdkg install` writes each agent's MCP config (naming the binary
that ran it by its bare name when `PATH` finds that same file there,
else by its path as invoked, links left as they are, or `--binary
<path or name>`; `agents up` takes `--binary` too), the AGENTS.md block and
the skill mirrors, and records what it changed; `mdkg install --undo`
(try `--dry-run` first) puts each file back as it was before install first
changed it, and refuses, changing nothing, where a file changed since
install last wrote it. If you edit one of those files and then run install
again, your edited file, with install's part still in it, is what undo puts
back.

In ref mode undo leaves the graph's shared projection list in place and
names it; `mdkg install --disable <name>` changes that list. Ignore-line
undo preserves other edits to `.gitignore`. If a saved undo copy is lost,
restore the exact pre-install bytes and rerun a writing install to recreate
it (up to 16 MiB); dry runs repair nothing. See
[doctor and undo](git-graph.md#doctor-and-undo) for recovery choices and
[install residuals](git-graph.md#limits-at-071) before migrating an old graph
checkout.

In 0.7.1, install's read-only ignore check is conservative: a negation in
`.gitignore` or `info/exclude` can refuse a new projection even when the
real install's later positive rule in that same file would override it.
The diagnostic names the matching rule; adjust that rule before retrying.
Real installs and dry runs check before graph preparation or projection
publication. A preview that preserves the full ignore-file ordering is
deferred to 0.7.2. The [graph setup fixtures](../crates/mdkg-cli/tests/graph_setups.rs)
cover these refusals and the state they preserve.

When an existing graph checkout would fast-forward across a change to
the root `.gitignore` or a `.gitignore` on a hook target's path, install
refuses a request that installs or syncs hooks before refreshing it.
Unrelated changes can refresh normally. The clone's common `info/exclude`
is outside the committed trees and cannot change during a fast-forward.
Refresh first by merging the configured main branch into the graph
checkout, then run install again. Omitting `--hooks` does not disable
previously installed hooks: even a plain install keeps their opt-in,
including when a config edit temporarily omits their projection. Only an
explicit `--disable hooks:<name>` excludes that hook from this check.
This keeps the ignore check and the hook write on the same checkout
contents. Dry runs make the same refusal; checking prospective checkout
contents is deferred to 0.7.2.

`mdkg init` enables no projections; the agents' files are projections you
enable (`mdkg install --enable agents-md,mcp:claude,…`), listed in
`.mdkg/config.json`, and install writes exactly those. Hooks are
projections too (`hooks:claude`, `hooks:codex`, `hooks:opencode`, `hooks:git`).
They need `--hooks` or `--enable hooks:<name>` on their first install in
each checkout, even when the shared list already enables them. Claude's
hook file is `.claude/settings.local.json`; Codex's is `.codex/hooks.json`;
OpenCode's is `.opencode/plugins/mdkg/server.ts`. See
[hook opt-in and ignore rules](git-graph.md#git-hooks).

The AGENTS.md block and the mdkg-workflow skill that init and install
write make mdkg the required baseline for every agent, whatever its
harness: start each turn with `mdkg msg inbox`, claim before any edit, and
coordinate through mdkg mail rather than a harness's own task list or
messaging. `mdkg doctor`'s agents check says when either is older than the
build, with the fix (`mdkg install`, or `mdkg skills add mdkg-workflow`).

After every turn the launcher stops the agent's process tree, its process
group, and every process carrying the turn's mark in its environment
(`MDKG_RUN_TURN`), so a `nohup … &` the agent left in a session of its own
is stopped too, as best it can (see "Limits").

OpenCode's Zen models (`--model opencode/…`) send prompts to OpenCode's
servers, so they need `--allow-zen`; with no `--model`, OpenCode uses the
model you configured.

Claude workers keep your project and user settings and use native tools.
OpenCode workers explicitly select `build` and keep its native command,
edit, web, directory and delegation permissions. mdkg adds its `auth` and
`abandon` refusals to both global and selected-agent permissions, and
refuses the harness commands that print a stored credential (below).
Reviewers
use a dedicated agent for each turn, with read-only command rules and no
subagent delegation. The [profile regressions](../crates/mdkg-run/src/profile.rs)
cover both roles on fresh and resumed launches; the
[fake OpenCode turn test](../crates/mdkg-run/tests/turn.rs) checks the config
that reaches a worker.

An explicit `--allow` adds your own Claude rule; mdkg supplies no default
worker grants. To give a worker mdkg commands, prefer its mdkg MCP server.
To allow the binary itself, name each subcommand, as in `Bash(mdkg validate *)`.

Some explicit rules are never passed on, since they could admit mdkg's
`abandon` or `auth` past a direct command refusal:

- an mdkg rule with open arguments (`Bash(mdkg *)`, `Bash(mdkg:*)`,
  `Bash(mdkg --root . *)`);
- `Bash` alone, or a pattern as the command (`Bash(*)`);
- a command the rule reader can't read (an unbalanced quote, a backslash,
  a letter outside ASCII, `$'…'`, a backtick, `X=1 mdkg`);
- several rules in one `--allow`.

The binary named by `--binary` counts as mdkg by its file name, in any
case. The turn and `agents status` name each dropped rule and say what to
write instead.

In launcher turns, `MDKG_BIN` names the absolute executable selected for the
mdkg MCP server before PATH changes. Use the mdkg MCP tools or `"$MDKG_BIN"`
for every mdkg shell command. This keeps a login profile that prepends an
older installation from selecting that build. Symlink and npm launcher
spellings are preserved. If the variable is empty or unavailable, use MCP
instead of falling back to a bare `mdkg` command.

Codex receives this value through its native `shell_environment_policy.set`
setting too. User environment filters remain in effect and can remove it;
see [Codex shell environment policy](https://learn.chatgpt.com/docs/config-file/config-advanced#shell-environment-policy).
The [launch tests](../crates/mdkg-run/src/profile.rs) exercise a fixture login
profile across fresh/resumed workers and reviewers in all three harnesses;
[turn tests](../crates/mdkg-run/tests/turn.rs) check that an unresolved binary
cannot reuse another launcher's inherited value.

## Native execution settings

Agent definitions use `mode: "auto"`; other presets are refused. A Codex
worker starts with `--approve-for-me`, which selects automatic approval
review and `workspace-write`. Resumed turns use the equivalent config keys.
A Claude worker uses `--permission-mode auto`.

Optional `sandbox` and `approvals` fields override the worker preset using
the harness's own values:

| Harness | `sandbox` | `approvals` |
|---|---|---|
| Codex | `sandbox_mode` config value | `approval_policy` config value |
| Claude Code | Refused | `--permission-mode` value |
| OpenCode | Refused | Refused |

Codex overrides use config keys instead of combining `--approve-for-me`
with `--sandbox`, which conflict. Automatic approval review stays selected;
the approval policy decides when a request is made. Changing either override
starts a new session. Removing an override restores the preset. Reviewers
always remain read-only; a reviewer definition with either override is
refused.

See the [launch-argument tests](../crates/mdkg-run/src/profile.rs),
[saved-state tests](../crates/mdkg-run/src/state.rs), and
[fake Codex turn regression](../crates/mdkg-run/tests/turn.rs).

## The launch wrapper: `run.wrapper`

An outside isolation layer (a container, a sandbox runner, a remote
shell) can run each turn's harness for mdkg. Off by default. Set it in
your user config, `$XDG_CONFIG_HOME/mdkg/config.json` (else
`~/.config/mdkg/config.json`), and only there:

```json
{"run": {"wrapper": {"command": ["/abs/path/wrap", "--profile", "turn"], "timeout_grace_ms": 5000}}}
```

A value anywhere in the repository is ignored with a warning. In the
committed `.mdkg/config.json` or an agent definition, a committed argv
would run on every clone that pulls it. In the machine-local
`<git common dir>/mdkg/config.local.json`, a worker turn, which may write
the git common folder, could turn off or replace the wrapper that
isolates it. No turn's spec grants write on your user config file or its
folder, or on the wrapper's program or its folder. The program must be an
absolute path outside the repository and its worktrees, and outside every
harness's config folder (`~/.claude` or `$CLAUDE_CONFIG_DIR`, `~/.codex`
or `$CODEX_HOME`, OpenCode's `~/.config/opencode` or
`$XDG_CONFIG_HOME/opencode`), which a worker's turn may write, to an
executable owned by you (or root) that no one else can write. A wrapper
that's configured but fails these checks refuses the turn, before
admission, rather than run it without its isolation. The turn's own
harness folder, as the environment mdkg gives the harness sets it, is
known only at the spawn: a wrapper inside it refuses the turn there, as
`run.wrapper_refused`, before the harness starts.

For every turn, after its preflight and just before the spawn, mdkg
writes a turn spec (`mdkg.run.wrapper.v1`, mode 0600) beside the turn's
logs and runs

```
<command…> <spec path> -- <harness program> <harness args…>
```

with the harness's own working folder, environment, stdin, stdout and
stderr, in the turn's process group, under the turn's own timeout. The
spec gives `turn_id`, `agent`, `harness`, `role`, `mode`, `argv` (the
harness's, exactly as after `--`), `cwd`, `env_keys` (the names, never the
values, of the variables mdkg set), `paths` (`worktree`, `git_common_dir`,
`graph_checkout`, `run_dir`, `scratch_shared`, `scratch_actor` and
`harness_config`, each with `access`: `write` for a worker, `read` for
every path of a reviewer), `read_only` (true for reviewers) and
`timeout_secs`. OpenCode takes its prompt as an argument, so its spec's
`argv` holds the prompt; Claude's and Codex's go on stdin and stay out of
the spec. A strict wrapper that exposes `harness_config` to a reviewer
only as read breaks a Claude reviewer's resume, since Claude writes its
session files there.

The wrapper may run the argv inside its own isolation, expose only the
listed paths at their access or less, add variables of its own, restrict
the network, and refuse the turn by exiting 125 before starting the
harness: the turn fails with `run.wrapper_refused`, and the agent isn't
run again. It may not change the argv or the cwd, touch the prompt on
stdin, write mdkg's store, run files, receipts or mail, leave the process
group, or outlive the turn; it passes through stdin, stdout, stderr and
the harness's exit status. mdkg keeps the timeout, the process-group stop
(`timeout_grace_ms` between the gentle stop and the kill, 2 s without a
wrapper), output parsing and receipts. The receipt records that a wrapper
ran: each attempt's `process_exits[]` entry carries
`wrapper: {command: [program], spec_digest}`.

The harness's own sandbox and approval flags are the same with a wrapper
as without one. A wrapper that isolates a turn fully and wants the
harness's sandbox off says so through the agent's `sandbox` and
`approvals` overrides, set by the operator; mdkg never infers it.

Codex workers in linked worktrees receive native `--add-dir` options for
their own verified Git administration folder and the clone's `objects`,
`refs` and `logs` folders. The common Git directory itself is never added:
its config, hooks, coordinator store and other lanes' administration folders
remain outside these roots. Workers in the main checkout get no added roots;
config, packed-refs, garbage collection and other rare Git writes escalate
under the operator's authority. These parent `exec` options
precede `resume` on resumed turns too; they add to the user's writable
roots without reading or replacing that configuration. Reviewers receive
no added roots. A sandbox that still refuses a Git write needs escalation
for that write; the live qualification checks cover fresh and resumed
turns with a preconfigured user root. Argument coverage is in
`codex_workers_can_write_only_their_clones_git_directories` in
`crates/mdkg-run/src/profile.rs`.

## OpenCode worker checks

OpenCode workers require a qualified OpenCode release, **2.1.0** or
**2.2.5** exactly, and an enabled `build` agent. Any other release,
another 2.2 patch included, is refused with a message naming the list:
a release that stopped reading mdkg's inline config would drop its
refusals silently, and only this gate catches that. 2.1.0 was checked
against its tagged source and the live hook check; 2.2.5's qualification
rests on its source (8bca5329) and on the live checks below. The CLI's `--version` and the
running server's must both be qualified and the same release.
Before each actual worker launch, including resumed attempts, mdkg checks
the version and asks OpenCode's native configuration
API to resolve `build` without mdkg's inline overlay. A disabled or absent
`build`, failed plugin activation, unsupported response, error or timeout
refuses the turn before the worker starts. Refused attempts leave unread
mail available. An enabled `build` marked as a subagent is still selected
explicitly; mdkg does not fall back to another agent.

The check uses the worker's normal `HOME`, `XDG_*` and `OPENCODE_DB` state.
OpenCode can perform ordinary initialization, logging and database writes,
including during the version check. mdkg does not parse or copy the user's
configuration or credentials. The probe starts no model turn. Configuration
or plugins can still change between the check and the worker launch.
Reviewers and dry runs skip this check, so a successful preview does not
prove that an actual worker will pass it.

The [profile tests](../crates/mdkg-run/src/profile.rs) cover version checks,
plugin activation, native state and probe cleanup.
`an_opencode_worker_turn_runs_only_after_its_build_preflight` in the
[CLI turn tests](../crates/mdkg-cli/tests/run.rs) checks refused launches,
retained unread mail and a successful worker launch with a fake OpenCode.

## What the permission rules do and don't do

- **They do** refuse `mdkg auth` and `mdkg abandon` in a marked agent turn,
  including calls with global options before the subcommand. The CLI checks
  the parsed command; ordinary command text mentioning these words still
  works. The launcher's fixed credential helper clears its own turn mark.
  The [CLI semantic regression](../crates/mdkg-cli/tests/worker_semantics.rs)
  checks refusal and the helper against a fake credential source.
- Claude and OpenCode workers receive pattern guardrails against direct
  invocation of credential-printing harness commands: `opencode auth`,
  `service get`, `pair`, `api`, `serve`, `debug`, and `claude setup-token`.
  The patterns cover the desktop app's `opencode-cli`, bare and absolute
  names, `./`, `../`, `~/` and `$HOME/` paths, quoted executable names,
  global options, and cheap `env`, `command`, `exec`, `nohup`, `timeout`
  or leading `VAR=value` prefixes. They also refuse those words later in
  a matching CLI invocation, such as `opencode run "fix the api"`.
  These literal, case-sensitive rules are a guardrail: case variants,
  other wrappers, `sh -c`, renamed binaries, and reading the credential
  stores directly can bypass them. Codex workers have no command rules.
  Reviewers retain their read-command allowlist. The stores remain files
  and a Keychain item the user's shell can read; these rules do not provide
  credential isolation.
- **They do** stop git's read commands from writing files: `--output`
  (`git diff --output=<file>`, and the same on `log` and `show`) is denied
  for reviewers in Claude and OpenCode, wherever it appears. Like the
  others, it matches the command as written, so a spelling the shell puts
  back together (`--"output"`) gets past it. Codex reviewers are also held
  by Codex's own `read-only` sandbox.
- **They aren't isolation.** A worker that your harness lets edit files and
  run commands can run code as you. That code can
  run mdkg or `security` itself. So can a command you allow that runs
  other code (`env`, `sh -c`, `xargs`). An agent can also remove its turn mark;
  the semantic refusal is a guardrail, not a security boundary.
  Turn records and receipts describe what mdkg observed. They are advisory,
  not a security boundary against an agent running under the same user whose
  native permissions let it rewrite those records. Isolation belongs to the
  harness sandbox or a separate OS account.
- **The fallback key is within reach.** With `auth.api_key_fallback` on,
  treat the Keychain key as reachable by workers. Keep it off unless you
  need it.
- **Isolation needs a sandbox,** which 0.7.0 doesn't provide.

## Delegated approvals

A delegate configured in the user settings file can relay approval
for one action: `implement`, `test`, `commit`, `design` or `review`. The
approval binds an exact committed node version. Project settings and message
bodies cannot grant delegation, and the send gate refuses detected agent
turns. Before each turn, the launcher revalidates the grant and committed-node
proof, then places a fixed approval statement and its limits in the trusted
instructions. Reviewer turns accept only `review` approvals.

Turn reports list promoted IDs in `approved_decisions`. Decisions omitted
because the trusted section does not fit appear in `dropped_decisions`;
their message bodies remain untrusted. `mdkg msg decisions` records
send-time metadata, which does not prove launcher promotion. Delegation
never authorizes push, publication, tags, credentials, data deletion,
security-setting changes or wider permissions. These checks are advisory
against processes running as the same OS user. See
[delegated decisions](delegated-decisions.md) for configuration, sending,
size limits, acknowledgment behavior and source-repository discovery limits.

The [decision fixtures](../crates/mdkg-cli/tests/decisions.rs) cover trusted
prompt placement, revocation, reviewer restrictions and dropped decisions
across the three harnesses. Mailed [turn receipts](#turn-receipts) describe
observations and do not list the promoted or dropped approval IDs.

## Which account a turn bills

Turns bill your subscription first: Claude Code's
login (`claude auth login`, kept in the keychain), Codex's ChatGPT login
(`codex login`), OpenCode's own configured auth. The provider key
variables never reach an agent, so a key left in your shell can't switch
a turn to API billing. Nor do the variables that move a turn to another
provider's billing, withheld by family so a new one is too:
- every `ANTHROPIC_*` (keys, tokens, `ANTHROPIC_CUSTOM_HEADERS`,
  `ANTHROPIC_BASE_URL` and the other endpoints, cloud projects, identity
  tokens), except the model choices `ANTHROPIC_MODEL`,
  `ANTHROPIC_SMALL_FAST_MODEL`, `ANTHROPIC_DEFAULT_*` and
  `ANTHROPIC_CUSTOM_MODEL_OPTION*`;
- every `CLAUDE_CODE_USE_*`: Bedrock, Vertex, Foundry, Mantle, Anthropic on
  AWS or Google Cloud, a gateway, and with them a few of Claude Code's
  feature switches;
- every `OPENAI_*`, and every provider key OpenCode's catalog names
  (Azure, Gemini, OpenRouter, Groq, Mistral, xAI, Z.ai, Zhipu, NVIDIA and
  some two hundred more), with its provider-only families `GOOGLE_VERTEX_*`
  and `SNOWFLAKE_CORTEX_*`.

These general cloud and service credentials do reach agents, since their
work may need them: `AWS_*` (but `AWS_BEARER_TOKEN_BEDROCK`),
`GOOGLE_APPLICATION_CREDENTIALS`, `GITHUB_TOKEN`, `GITLAB_TOKEN`,
`CLOUDFLARE_*`, `DATABRICKS_*`, `DIGITALOCEAN_ACCESS_TOKEN`,
`SNOWFLAKE_ACCOUNT`, `HF_TOKEN` and `WANDB_API_KEY`, and any variable
outside the families above. Without a provider switch, they bill nothing
for a Claude or Codex turn.

An OpenCode agent bills what OpenCode's own stored auth resolves
(`opencode auth login`), or one of the general credentials above that
OpenCode also accepts (`DATABRICKS_TOKEN`, `CLOUDFLARE_*`, `AWS_*`,
`GOOGLE_APPLICATION_CREDENTIALS`) when its model names that provider. No
other key in your environment reaches it: a provider key you relied on in
your shell must move to OpenCode's auth. It bills whatever
provider its model names, so an OpenCode turn on a Vertex model, with
`GOOGLE_CLOUD_PROJECT` and Google credentials in your shell, bills that
project. The catalog list is the union of the qualified releases'
catalogs (2.1.0 and 2.2.5), kept in
`crates/mdkg-run/src/opencode-catalog-env.txt`. Another OpenCode can name
providers this list lacks, and their keys would reach agents, so before a
release is qualified its `scripts/opencode-catalog-env.py` output joins
the union (a test checks every name on it is withheld or kept on purpose).

A harness that isn't logged in refuses the turn, naming its login
command and `mdkg agents use-key <agent>`.

An API key bills a turn only when you choose it, with `agents use-key`,
and only where the policy allows that:

```json
{"agents": {"defaults": {"auth": {"prefer": "subscription", "api_key_fallback": true}}}}
```

The fallback setting only allows `use-key`; it bills nothing by
itself. Nothing charges the key on its own: 0.7.0's automatic key turns,
after a usage limit and for a harness that isn't logged in, are gone in
0.7.1, and no setting bills the key on every turn. When the
subscription hits its usage limit (the harness's own final error line),
the turn ends with that reason, and the mail stays unread. The turn's
hint and `agents status` say `subscription limit reached; mdkg agents
use-key <agent> bills your API key for its next turn`. A turn is never
re-run within itself.

`mdkg agents use-key <agent>` bills that agent's next turn, one, on the
key; `agents_use_key` is its MCP twin. It's your command, never an
agent's: it refuses for an agent's actor, under a live turn of this
repository anywhere in its process chain (an agent's MCP server
included; `env -i` can't hide it), and with a turn's mark in its
environment. It fails closed: a process chain it can't read (`ps`
refused, an unknown parent, an ancestor of yours whose environment can't
be read) counts as inside a turn, and the refusal names why. A
non-agent actor you name is accepted only when no turn is found.

`use-key` also refuses unless the agent's policy has
`auth.api_key_fallback` on, and for OpenCode, which has no key fallback.
It sends the agent a note, which wakes a running, mail-triggered
supervisor to run that turn and take up the mail the limit left unread.
A stopped or manually triggered agent runs the key turn when it next
runs. The turn knows the note by the message id use-key records for it
(`billing/<agent>.wake`), never by its text: sent as the launcher's own
actor, that note is set aside unseen. Any other note, whatever it says,
is ordinary mail.

**A stated residual** (the rule for person actions, 2026-10-29): a
person's action is refused inside any launcher turn; forging one from
outside a turn is a stated residual until mdkg can prove a person is
present. Agents run unsandboxed as your OS user, so an agent outside any
turn, a process reparented away from its turn, or one acting on another
repository's turn could run `use-key`, and any process running as you can
write the mark file directly. The checks above refuse an agent's own
turns, not a forger running as you. With `auth.api_key_fallback` off (the
default), no mark bills at all.

The billing folder (`billing/` in the run folder) is made owner-only, and
every use checks it's a folder, not a link, owned by you and writable by no
one else, or refuses. Its files are read only when they're regular files
of at most 4 KiB, checked to be the file looked at once opened, and never
written through a link. A same-user process that swaps a link in between
a check and its use is the same residual.

The mark names the harness and Keychain service it was written for, so it
bills only that harness's key: after a definition change to another
harness, the next turn removes it instead. The turn consumes it as the
last step before the harness spawns (after admission, its files, context
and logs, the preflight and its arguments), by moving exactly the mark it
read aside and syncing that. A turn that fails before then leaves the
mark for the next one. A missing or replaced mark is never consumed, and a
consume that fails refuses the turn rather than run it on another account
(the harness's own key included). So a crash can't bill the key twice. A
crash between the consume and the spawn, or a harness that then fails to
spawn (moved or not executable), uses it up: run `use-key` again. That
fails safe: nothing is charged twice, and nothing is charged without a
mark. A mark that
can't be read fails closed: no turn bills on it, and turns are refused
until it's removed. A mark that can't be written durably is removed
again, and use-key promises the subscription only once that removal is
synced. If it isn't, the mark is held (`billing/<agent>.unsure`): the
next turn clears it instead of billing the key, and is refused until it
can. use-key settles an earlier hold before it reports a new mark. With
the fallback turned off, the mark is revoked before the turn does
anything else; one that can't be removed is held, and one that can't be
held either refuses the turn. The key lives in the macOS Keychain, nowhere else:

```bash
security add-generic-password -s mdkg-anthropic -a mdkg -w
security add-generic-password -s mdkg-openai -a mdkg -w
```

Claude gets it through an `apiKeyHelper` (`mdkg auth key
anthropic`, which refuses a terminal); Codex gets it in its own process
only, excluded from the shells it runs for the agent. It's never logged
or printed, and it's taken out of the turn's files and reply. OpenCode
has no key fallback: its own config names its providers. `agents status`
shows each agent's account: `subscription`, `own_config` or `none`, and
its `billing` line (the offer after a limit, a waiting mark, or a hold).

The limit, honestly: with no sandbox, an agent runs as you, so it could
reach the Keychain itself. Worker access follows your native permissions;
mdkg refuses its own helper, and Codex's tool shells don't see the key,
but neither is a boundary.
For real isolation, run agents in a sandbox or a separate account.

## Inspecting configuration

`mdkg config show` displays merged settings and each value's source without
changing files or opening the coordinator. `--effective` selects the same
view and is also the default. For JSON with versioned defaults, source paths,
per-value origins, warnings and redacted-value pointers, use:

```sh
mdkg config show --effective --json
```

The display combines built-in defaults, the selected user file, project
configuration and agent definitions, then machine-local settings. The user
file is `$XDG_CONFIG_HOME/mdkg/config.json` when that variable is nonempty
and absolute, otherwise `$HOME/.config/mdkg/config.json`. A selected but
absent XDG file does not fall back to HOME. Project settings come from
`.mdkg/config.json`, and machine-local settings from
`<git common dir>/mdkg/config.local.json`. A definition file replaces that
agent's inline project entry.

Run from the graph root or select it with `mdkg --root <graph-root> config show`.
If the selected root has no `.mdkg` directory, the command shows defaults and
user settings only. It warns about an ancestor project rather than importing
it; an incomplete or unreadable `.mdkg` directory is an error.

This display does not validate every setting for execution. In 0.7.0,
`agents up` and `agents status` apply user and machine-local overrides to
named `.mdkg/agents/<name>.json` definitions and orchestration settings.
Optional `agents.defaults` and overrides for names without a definition file
are ignored; project defaults still apply. Projections remain project-only.
The display warns about these limits. Migrate a legacy inline agent to a
definition file with `mdkg agents edit <name>` before using named overrides.

Definition management edits project-authored values. Status shows current
effective definitions and their origins separately from saved execution
fields, without updating that saved state. Execution saves the effective
spec; removing a definition can leave its last overrides in saved-only
operation. Removing the file is not a way to clear those saved settings.

`agents up` captures settings at startup. A long-running watch restarts
itself when a selected agent's committed definition or the committed
`.mdkg/config.json` changes ([reloads and restarts](#reloads-restarts-and-stopping)).
Edits to local or user configuration apply the next time the watch
restarts, whether you restart it or a reload or reinstall does. A changed
unfinished invocation can require
[explicit recovery](#durable-supervisor-progress) before it starts.
`config set` is deferred beyond 0.7.1. The
[runtime layering tests](../crates/mdkg-cli/tests/agent_layers.rs) cover
effective values, origins and authored edits across all three harnesses.

Both formats hide recognized credential values as `[redacted]`; detection
is incomplete, and property names and source paths remain visible. The
[configuration display tests](../crates/mdkg-cli/tests/config_commands.rs)
cover source precedence, transitional notices, graphless discovery and
redaction without modifying files.

## Reads that write

A read-shaped command can maintain local state:

- `status` reads a short read-only coordinator snapshot and closes that
  connection before Git work. In ref mode it pins one graph tip. Lease
  expiry and the displayed settlement queue are projected in memory.
  Settlement observed from the configured settlement branch in checkout
  mode, or the pinned graph tip in ref mode, is recorded through a separate
  zero-wait write. If recording is deferred, it prints `observation deferred`:
  the store may be busy or on a non-current schema, ancestry may be unknown
  or a Git probe may fail or report stderr, the bound graph tip may no longer
  validate against the snapshot, or the coordinator generation may have
  changed. An empty displayed queue does not prove the observation was saved.
  After `observation deferred`, retry `status` until the warning clears,
  before the settlement branch advances where possible. A squash or rebase
  landing overwritten before a successful observation can leave a false
  conflict. Status can also refresh its derived graph cache. It reports
  pending graph intents without replaying or publishing them.
- `holds` still calls `Repo::observe`: it persists settlement observations
  and due lease expiry. It can therefore change the coordinator without
  editing a node.
- `search`, `pack --query` and `index status` sync the derived cache in
  `<git common dir>/mdkg/index/`; `index rebuild` rebuilds it deliberately.
  When that cache can't be written, these reads build it in memory for the
  command and warn. At 0.7.1 `list` and `show` build their graph in memory
  instead of updating this derived cache, but when a coordinator store exists
  they still open it read/write, as other coordinator-backed reads do.
- `msg inbox`, `msg wait` and `msg thread` mark the messages they list as
  **shown**, not acked. Pass `--no-mark` to leave that state
  unchanged; mail listed only this way can't be acked until it is listed
  normally. The MCP `inbox`, `inbox_wait` and `channel_read` tools also mark
  the messages their replies deliver, after the reply goes out. Those tools
  have no `no-mark` input at 0.7.1.

After a crash, SQLite's read-only open can briefly take a WAL-index
recovery lock. This remains a residual: a lock-wait timeout does not bound
how long a connection holding that recovery lock takes to finish.

For a viewer who must not change mail state, use the CLI's `--no-mark` and
don't ack. For a viewer with no filesystem write access, `config show` does
not open the coordinator, and direct Git or file reads are safe. The derived
index's in-memory fallback does not make every other cache or coordinator
read write-free; status's cache refresh and observation write also remain
possible. In particular, don't rely on MCP mail from the Codex
read-only sandbox described above.

## Waiting for mail outside `agents up`

A session or script that has no supervisor can wait for its own actor's mail:

```sh
mdkg msg wait --as <actor> --timeout <seconds>
```

It returns as soon as matching mail arrives. If none arrives before the
timeout it exits 6 with `msg.wait_timeout`; `--timeout` defaults to 600
seconds. Add `--no-mark` when the caller is only watching for a wake and
must leave the messages unshown. The command works with no `agents up`
process running.

## Waking agents: `agents up` and the turn policy

With no names given, `mdkg agents up` selects the headless agents named in
`.mdkg/agents/*.json` (or deprecated inline config entries); only when none
are configured does it select all saved agents. It also adds interactive receiver registrations that
have no headless spec. A retained registration cannot add a saved worker that
the config left out. Explicit names select only those actors. The
[discovery regression](../crates/mdkg-cli/tests/agents.rs) covers both defaults
and the retired-worker case.

The supervisor wakes selected agents when mail reaches them or their channels.
Codex sessions registered by trusted hooks use the receiver described below;
other sessions you start yourself can wait with `msg wait` through MCP.

A turn answers one conversation. From the page of unacked mail it loads,
it chooses the oldest conversation from its `reports_to` actor first; if
none is on that page, it chooses the oldest conversation on the page.
The page limit defaults to 20 and can be set by the turn policy or `--limit`.
Mail from `reports_to` behind a full page waits for a later turn; priority
across the entire inbox is deferred to 0.7.2.

```sh
mdkg --root .worktrees/graph agents add coder --harness claude --role worker --model sonnet
mdkg --root .worktrees/graph agents edit coder --instructions "Work on the assigned tasks."
mdkg --root .worktrees/graph agents add reviewer --harness codex --role reviewer
mdkg --root .worktrees/graph agents up
mdkg --root .worktrees/graph agents up reviewer --once --fresh
```

When Git content changes, `agents add`, `edit` and `rm` commit only the
affected definition and record its changeset. In ref mode they publish to
the graph ref, never the code branch. Otherwise they report the
current commit without a new commit. The [definition command tests](../crates/mdkg-cli/tests/agents.rs)
cover clean checkouts, exact commit scope and no-op edits.

An agent's fields (`harness`, `role`, `model`, `effort`, `base`, `cwd`,
`worktree`) live in its [definition file](agent-definitions.md). A worker
with no `cwd` gets its own worktree from the orchestration names; a
relative `cwd` is from the launcher's checkout. Change fields with
`mdkg agents edit <name>`. Startup captures the effective definition; turn
calls apply it to saved runtime settings under the turn lease. `--unset`
removes the authored field, so another configuration layer may supply its
value. Sessions, turn counts and mail cursors stay local.

When creating a worker's branch, the launcher chooses the committed definition's
`base`, then `mainRef`, then the primary checkout's current branch. An invalid
explicit `base` refuses. Starting from a done dependency is deferred to 0.7.2;
0.7.1 does not select dependencies or give dependency merge notes.
If no usable base exists, `run.no_base` explains the problem and suggests a
first code commit or `git config mdkg.mainRef <ref>`. The saved local agent
state records the chosen commit, source and ref as `work_base`. If the
worker's branch already exists, a new worktree attaches to its existing tip,
which may be an older commit; the configured base does not choose that tip.
The launcher may resolve the base while recovering the branch's provenance.

In ref mode, when `mdkg.mainRef` or `$MDKG_MAIN_REF` names a resolvable code
base, before each worker turn the launcher reports when its branch is behind
that base, with a suggested merge command. If neither is configured, the
checkout-branch fallback does not enable follow notes. The launcher never
merges for the worker; `follow: "merge"` still gives notes in 0.7.1 and the
automatic merge is 0.7.2. `agents status --json` includes `code_base` with
the ref and ahead/behind counts when both are available; `work_base` is in
saved agent state, not the status response.

`instructions` are the operator's standing instructions, shown before the
mail on every turn: mail bodies are untrusted data, so an agent acts on
what they ask only as far as its instructions say.

The policy is project `agents.defaults`, then the effective definition's
`policy`, field by field over the 0.7.0 defaults:

| Field | 0.7.0 default | Also |
|---|---|---|
| `trigger` | `mail` | `manual`: use `agents up <name> --once` |
| `shape` | `batch`: one run shows the unacked mail of one conversation, up to 20 (`{"kind": "batch", "max": 5}`), and replies once | |
| `session` | `resume` | `agents up --fresh` starts over on each agent's first turn |
| `loop_guard` | 10 automatic turns per thread per hour | `{"turns": 4, "window_secs": 1800}`, or `false` |
| `concurrency` | unset: no cap | a number caps the turns `agents up` runs at once (the smallest set) |
| `timeout` | unset | seconds per turn |
| `retry` | `none`: a failed turn's mail stays unacked until the next mail | |
| `budget` | unset | |

Every field also takes `{"kind": …}` with settings, so later options
(`schedule`, `until-done`, `fresh-each-turn`, `backoff`, budgets) can be
written now: `agents status` reads them, and `agents up` refuses them
until a release runs them.

A thread the loop guard pauses keeps its mail unacked while the agent's
other threads run. It runs again when its oldest counted turn leaves the
window. While the supervisor is running, clear only that pause with
`mdkg agents resume <agent> --thread "<thread>"`; copy the thread from
`agents status`. Plain `mdkg agents resume <agent>` also recovers durable
progress and needs exclusive ownership: stop the agent's supervisor first.
A separate manual `mdkg agents up <agent> --once` also needs that ownership.
`agents status` lists agents, policies and paused threads. With no names,
`agents up --once` runs what automatic discovery finds and stops. Explicit names with
`--once` select one manual pass, which also runs manual-policy agents.
`--prompt`, `--reply-kind` and `--dry-run` require names and `--once`.
`--limit`, `--timeout`, `--keep-logs`, `--fresh` and `--binary` apply to
turn execution. JSON output is one `mdkg.agents.up.v1` event per line;
turn details are under `data.turn`.

### Durable supervisor progress

`agents up` saves each agent's watch count, mail cursor and admitted work
under `<git common dir>/mdkg/run/progress/`. Restarting the same unfinished
invocation preserves its accounting. `--max-turns` caps each agent's watch
count; `--limit` caps the messages shown in each turn. A finished watch permits
a new watch. Changing an unfinished invocation requires explicit recovery;
after recovery, a changed invocation is intentional new work.

A claimed, non-idle headless call counts toward the watch limit even when
preflight or execution fails. Idle checks and failed turn-lease acquisition
do not count. Ordinary completion is saved before releasing the lease.
Interactive receiver counts represent queue dispatches, not completed model
turns: queued or delivery-unknown attempts count, while proven not-sent
attempts do not.

If a crash or durable-state failure leaves an unfinished turn or receiver
dispatch, automatic execution refuses to replay it. Inspect
`mdkg agents status --json` and stop the actor's supervisor before running
`mdkg agents resume <agent>`. Recovery retains the interruption metadata and
conservatively charges the interrupted work once. That metadata provides
context for later work, not approval. Reopening the same invocation after
recovery preserves the count and cursor. Thread-only resume clears a loop
guard, preserves progress and routing, and cannot recover an unresolved
admission.

Status includes `progress` entries with the record, current store identity
(`<uuid>:<inode>` once `.store-id` exists), archive, problem and repair
guidance. Its `active` flag means the progress lock was held when inspected;
an admission under that lock is not by itself
evidence of a crash. `mdkg doctor` also reports progress needing recovery.
Follow the reported repair guidance for foreign or malformed records; keep
the records and recovery archives intact. The
[durable CLI tests](../crates/mdkg-cli/tests/agents.rs) and
[receiver recovery test](../crates/mdkg-cli/tests/receiver.rs) cover restart,
interruption and thread-only recovery.

### Reloads, restarts and stopping

A long-running `agents up` (without `--once`) checks every two seconds for
three things, and on any of them stops starting new turns, lets every
running turn or queue dispatch finish, then re-execs itself with the same
arguments:

- **A committed definition change:** a selected agent's
  `.mdkg/agents/<name>.json`, or `.mdkg/config.json`, changed at the
  checkout's HEAD or the main branch. A change counts once it is in a
  local commit, however it got there (`agents edit`, a merge, a pulled
  squash or rebase). Uncommitted edits never trigger a reload.
- **A replaced binary:** the executable it runs, or the path that started
  it, was replaced, as an install does. It re-execs that path, so the new
  build runs.
- **A moved store:** a newer build upgraded the coordinator store to
  versions this build doesn't use. If the binary was also replaced, it
  re-execs the new one. Otherwise it stops once with `coord.store_failed`
  and the hint to start the newer build, instead of failing every turn.

For a supervisor already running 0.7.1, a reinstall that upgrades the
store needs no manual restart: the install replaces the binary, and the
supervisor drains and re-execs into the new build. A supervisor still
running 0.7.0 lacks that behavior and must be restarted once after upgrade.
Restart a long-running supervisor to include newly added agents: only its
originally selected definitions and config are watched. In ref mode these
changes must reach the graph ref. Each agent's watch continues across the re-exec with the
same count and cursor, so no mail gets a second turn. A re-exec reads
`.mdkg/config.json` and `.mdkg/agents/*.json` from the commit at HEAD,
never from the worktree, so an uncommitted edit doesn't take effect until
it's committed; the re-exec names it in a warning. The user and
machine-local configuration layers aren't committed, and are read as
usual. A watch the re-exec can't continue (a definition `agents up`
refuses, or an agent no longer selected) is finished, so starting again
needs no `agents resume`. Claude, Codex and OpenCode agents restart the
same way.

For a headless agent with a definition file, each turn prompt names the
commit that last changed it ("last committed in <sha>"), so you can match
a turn to the definition it ran.

A turn the model turns away as at capacity fails with a hint saying so,
and its mail stays unread. Nothing is retried within a turn, for any
reason, so nothing it did runs twice. It counts only when the harness
itself exits (not a timeout or a signal) reporting capacity in its own
error form. Claude and OpenCode run in a structured protocol, so their
JSON error event, the last JSON line of stdout, is read first, whatever
stderr says. Codex runs without `--json`, so its output is text, and only
its `ERROR:` line counts. Then, for every harness, the last line of
stderr (or of stdout when stderr is empty) is read.
Quoted text before those lines doesn't count. A quote that happens to be
that very line would, which only starts the next session early. After two such failures in a row, the
agent's next turn starts in a fresh session. The count is saved in the run
folder, so a re-exec, a restart or a series of `--once` passes keeps it.
The wording matched is Codex's (and an OpenAI model's through OpenCode);
Claude's provider-wide "Overloaded" isn't counted, since a fresh session
wouldn't help.

To stop a long-running watch, send SIGTERM to its process (`kill <pid>`):
it lets running turns finish, finishes each watch and exits 0, so
restarting needs no `agents resume`. A SIGTERM to its whole process group
(`kill -- -<pid>`) also reaches each running turn's guard, which stops
that turn at once; that's the hard stop. Under systemd, whose default
`KillMode=control-group` signals every process in the unit, set
`KillMode=mixed` (or `process`) so that `systemctl stop` drains, and a
`TimeoutStopSec` longer than a turn. A second SIGTERM stops it at once.
SIGTERM also wins over a pending restart. In the moment between a re-exec
and the new process taking over, a SIGTERM stops it at once and leaves the
watch unfinished. Starting it again picks the watch up, unless its
definitions changed; then it needs `agents resume`. Prefer TERM over INT
for supervisors started in the background: a shell without job control
starts background commands with SIGINT ignored, and the supervisor keeps
that, so INT does nothing to them. An explicit `--once` pass keeps the
default SIGTERM action.

To stop one agent, or all of them, without signalling anything, use
`agents stop <agent>… --drain` (or `--all --drain`) from any shell in the
repository. It's the same drain as SIGTERM's, for each agent named: its
supervisor finds the request at its next loop boundary, or within about a
second while the agent waits for mail, lets a turn in flight run to its
end (its own `timeout` still applies), finishes the agent's watch, gives up
its progress lock, and marks it stopped. The supervisor keeps running its
other agents, and exits 0 once every agent it runs has stopped. A stopped
agent can be started again with `agents up`, without `agents resume`,
even while that supervisor still runs others, and a re-exec for a reload
leaves it stopped. `--drain` is the only mode; `--now` is reserved and
refused.

Inside an agent's turn, `agents stop` may stop only that agent: naming
another, or `--all`, is refused (`run.unsupported`), so a worker can't
drain the other lanes or the orchestrator. The turn is found as for
`use-key`: by its mark in the environment and by a live turn of this
repository anywhere in the process chain, and when that chain can't be
read the stop is refused too. There, `--wait` is refused as well, since
it would wait for the caller's own turn to end; the drain takes effect
when that turn ends. Stop other agents from your own terminal. As with
`use-key`, the rule covers the target repository's turns: a turn of
another repository that strips its environment and passes `--root`
isn't one of them, a stated residual.

`agents stop --json` (`mdkg.agents.stop.v1`) gives each agent's
`supervisor_id`, its `state` (`draining`, `stopped` or `not_running`) and,
while draining, its `in_flight_turn`. Without `--wait` it returns once the
requests are written; with `--wait <secs>` it waits that long for
`stopped`, and exits 6 (`msg.wait_timeout`) if a turn is still running.
Nothing is killed either way. Stopping an agent that has already
stopped, or that no supervisor runs, gives `not_running` and succeeds, so
it's safe to repeat; a name no definition, saved state or supervisor
knows is `run.unknown_agent`.

Each `agents up` names itself with a supervisor id, kept across a
re-exec, in `<git common dir>/mdkg/run/supervisors/` (internal; read it
through `agents status`). The request is
`<git common dir>/mdkg/run/stop/<agent>.json` and names that id. If the
supervisor died before acting on it, the request stays, and the next
`agents up` for the agent removes it, says so, and starts.

When an agent's progress is already held, `agents up` and
`agents resume` name the last recorded holder's process ID, and say so
when no such process is running.

### Re-attaching: `agents status --json`

A client that restarts finds its agents still running and picks their
streams back up from `agents status --json` (API v1 A18). It gives the
event stream's head, `event_cursor`, read with the snapshot, and for
each agent:

- `state`: `stopped` (no supervisor runs it), `idle`, `running`,
  `draining` (asked to stop, its turn still finishing), or `paused` (idle,
  with threads the loop guard paused); an open set;
- `supervisor`: `{supervisor_id, pid, started_at, build, state}`, where
  `state` is `running`, or `stale` for the last supervisor that ran it
  when that process is gone; null when none ever did;
- `current_turn`: `{turn_id, number, started_at, output_after}`, the turn
  running now, where `output_after` is the latest `seq` in its output
  journal; null when none;
- `last_turn`: `{turn_id, number, outcome, ended_at}`, the last turn that
  isn't running; null when none;
- `follow_from`: `<current_turn.turn_id>:<output_after>` while a turn
  runs, else `<last_turn.turn_id>:end`, else `turn:<N>` for its next turn
  (`turn:1` for an agent that has never run).

To re-attach: read `agents status --json`; start
`msg watch --after <event_cursor>`; and for each agent, start
`agents logs <agent> --follow --after <follow_from>`, passing the
position as it is. A position is never "from now", so a turn that starts,
or even finishes, in between is still delivered.

Behind those fields, from 0.7.1 each admitted turn attempt gets a turn
id, a number from the agent's turn index (one more than the highest it
holds, so a retry never reuses one) and an output journal of its own,
written in a fixed order: the index's start line, the journal with
`turn.started`, the D6 admission, `harness.starting` just before the
spawn; after the harness, `turn.ended`, the receipt, D6 completion and
the index's end line. A turn's state is read from that evidence, never
from a missing line: ended once any terminal record exists (the receipt,
the end line or the journal's `turn.ended`), running while the launcher
process that started it is alive, interrupted when the harness may have
started, and otherwise not started. The next turn of the agent writes the
missing end line of any attempt left unfinished, with the outcome its
evidence gives, and never runs it again. Journals are kept for the last 3
turns, and for any turn without a terminal record; the index keeps the
newest 1000 turns once it passes 2000. All of it lives under
`<git common dir>/mdkg/run/<agent>/` and is internal: read it through
`agents status` and `agents logs`.

### Turn output: `agents logs`

`agents logs <agent> [--turn <turn_id>] [--follow] [--after <position>]
[--native] --json` prints a turn's live output, normalized across
harnesses, as NDJSON: one `mdkg.api.turn_output.v1` event per line,
flushed per line. Each event has the turn's path, a `seq` from 1 within
the turn, a `time`, a `type` and its `data`:

| `type` | `data` |
|---|---|
| `turn.started` | `{turn_id, number}`, the journal's first event |
| `harness.starting` | `{harness}`, just before the spawn |
| `text` | `{text}`, the agent's own output |
| `tool.started` | `{class, name, summary}`; `class` is `edit`, `shell`, `read`, `mcp` or `other` |
| `tool.ended` | `{class, name, ok}` |
| `usage` | `{input_tokens, output_tokens, cache_read_tokens, total_tokens, cost_usd}`, as the harness reports them |
| `status`, `error` | `{text}`, harness notices; a stderr line is a `status` |
| `turn.ended` | `{outcome}`, plus `output_gone: true` when its journal was purged first |
| `agent.state` | `{state}`, on an agent follow only; `turn` and `seq` are null |
| `native` | `{harness, event}`, a native event no mapping covers; only with `--native` |

The set is open: a client skips a type it doesn't know. Claude's
`stream-json` gives text, tools and usage; OpenCode's `--format json`
gives text, tool parts and usage summed over its steps; Codex runs without
`--json`, so its stderr progress comes as `status`, its `tokens used`
footer as `usage`, and its `-o` reply as `text` once the harness exits,
with no tool events. Output text is the agent's, untrusted, and on an
API-key turn the redacted text the launcher wrote.

- **One turn** (`--turn`): its journal; with `--follow` it closes after
  the turn's `turn.ended`.
- **An agent** (`--follow` alone): its turns in number order, each from
  `turn.started` to `turn.ended`, never closing at a turn's end, with an
  `agent.state` frame when it starts and whenever the state changes. A
  turn whose journal is gone is still sent, as `turn.started` then
  `turn.ended` with `output_gone: true`.
- **Neither**: the current turn, else the last one, and exit.

`--after` takes a position: `<turn_id>:<seq>` resumes after that event,
`<turn_id>:end` starts with the turn after that one, and `turn:<N>` with
turn number N, whether or not it has started (with `--turn`, a plain seq
too). Without it, an agent follow starts with the running turn, or else
the next one. Every reader sees the same events with the same `seq`, in
the order the launcher read them; `--native` changes only which events
appear, never their `seq`. SIGINT or SIGTERM ends a follow with exit 0.

A turn just starting has no journal for a moment, since its index line
comes first: `--follow` waits for it, and without `--follow` nothing is
printed and the exit is 0. A turn whose journal is gone is refused with
`api.output_gone` (exit 3) and `data.receipt`, its receipt's path; a turn
the agent never had, or an attempt that ended before it started, with
`api.unknown_resource`; a position past the index's trim with
`api.cursor_expired` and `data.earliest`; a malformed one with
`api.bad_cursor`; an unknown agent with `run.unknown_agent`. A refusal is
one `mdkg.agents.logs.v1` envelope, and nothing else is printed.

### Turn receipts

After an admitted headless turn ends, the launcher retains an advisory
receipt and attempts to mail it as `mdkg:receipts` to the agent's `reports_to`
actor, which defaults to `orchestrator`. Successful turns and harness failures
can produce receipts; idle polls, dry runs and refusals before admission do
not. This receipt is separate from the agent's ordinary reply.
The recipient must be a valid, non-reserved actor different from the agent;
an agent named `orchestrator` needs a different `reports_to` value.

The receipt combines the result and final summary with Git and coordinator
observations: commits newly reachable between the observed HEADs, including
merged history; the actor's claims and leases still held; versioned native
completion facts; and committed task evidence. Task evidence uses committed
graph configuration and node contents, rather than dirty files or current
local settings. Added or retyped done nodes may already have been done.
Unknown metadata stays unknown, and repository titles remain untrusted.
These observations do not prove authorship, approval or task acceptance.

Collection has separate time and size limits for its phases. Missing history,
large graphs or spans, malformed content and other collection failures are
reported as unavailable or partial evidence, not empty completion lists.
The full rendered receipt is retained separately from disposable turn logs
under `<git common dir>/mdkg/run/receipts/` before delivery is attempted.

Only the mailed final summary is capped at 8 KiB of escaped JSON. A shortened
summary is marked as truncated, with its original byte count and the local
receipt path; the retained receipt keeps the full summary. Other evidence is
not shortened. Mail can still refuse an oversized body or recognized
credential content. Receipt failures are non-fatal: they do not change the
turn result, rerun the agent or automatically resend mail. Interrupted delivery
can leave an unknown outcome. Retention failure may leave no local receipt
path; inspect the diagnostic and retained turn output.

`agents status --json` exposes the latest `receipt` delivery record, including
`path`, `message_id` and `error`, or `receipt_problem` if it cannot read the
record. These issues produce warnings without making the agent unrunnable.
The [receipt fixtures](../crates/mdkg-cli/tests/receipts.rs) cover all three
harnesses in both roles, failures, local retention and mailed summary previews.

### Deferred beyond 0.7.1

`config set`, interactive receiver policy refresh, bounded mail previews
of receipt evidence beyond the final summary, native Windows, macOS
notarization, graph visualization and derived/CocoIndex search do not ship
in 0.7.1. Networked serve is deferred to 0.7.2. Ref-based graph writes and
the `mdkg-git` crate do ship; see [Git and the graph ref](git-graph.md).

## Agent hooks: one router, built-in rules, harness adapters

Hooks check mail, leases and git at the harness's lifecycle events,
including sessions you run yourself. Every native entry (Claude's and
Codex's command hooks, OpenCode's plugin, git's own hooks) calls one
router, `mdkg hooks run --harness <claude|codex|opencode|git> --native
<event>`. It reads the harness's hook input, turns it into a canonical
event (`mdkg.hook.event.v1`: the event, the harness, the session, the
actor and its kind, and the tool, classified as `edit`, `shell`, `read`,
`mcp` or `other`, with what a shell command does with git), runs the rules
for that event, and answers in the harness's format. `hook` is an alias of
`hooks`, and 0.7.0's `hook <start|stop|wait|edit> --harness <h>` commands
are aliases of the router for one release, with the same output; the
[router fixtures](../crates/mdkg-cli/tests/hook.rs) compare the two on all
three harnesses.

| Native event | Claude Code | Codex | OpenCode | Canonical event |
|---|---|---|---|---|
| start, prompt | `SessionStart`, `UserPromptSubmit` | `SessionStart`, `UserPromptSubmit` | `context` | `session.start`, `prompt.submit` |
| before a tool | `PreToolUse` | `PreToolUse` | `tool.execute.before` | `tool.before` |
| stop | `Stop` | `Stop` | `session.execution.succeeded` | `turn.stop` |
| idle wait | `idle` (the async Stop entry) | none | `idle` | `session.idle` |
| git | | | | `pre-commit`, `pre-push` (`--harness git`) |

The built-in rules, and who they apply to:

| Rule | Event | Who | Does |
|---|---|---|---|
| `mail.notice` | start, prompt | agents | names unread mail |
| `mail.stop` | `turn.stop` | agents | blocks once on new wake-worthy mail |
| `mail.wake` | `session.idle` | agents | wakes an idle session on mail |
| `lease.edit` | `tool.before`, an edit | agents | refuses an edit to an unleased file |
| `lease.audit` | `turn.stop` | agents | blocks once per set of files changed without a lease |
| `lease.done` | `turn.stop` | agents | blocks once while path leases are held after `done` |
| `lease.staged` | `git.pre-commit` | agents; people | refuses an agent's commit of an unleased staged file (a link by its own path); warns a person |
| `git.protect` | `tool.before` (a git push), `git.pre-push` | agents | refuses a push to a protected branch, and any force push |
| `secret.scan` | `tool.before` (a git commit or push), `git.pre-commit`, `git.pre-push` | everyone | refuses a credential-shaped string in what's committed or pushed, naming the commit and path but never the match, and refuses when the scan can't run. It uses the mail filter's detector, and reads every file as text, binary ones included. A file larger than 16 MiB isn't scanned: it passes with a warning that names it |

Every rule is on by default, and the lease rules stay silent where the
session has no mdkg actor's leases to check. The 0.7.0 projections still
call the alias commands, which route through the same rules; the Claude
and Codex PreToolUse entries now match the shell tool too (`Bash`), and
OpenCode's plugin routes its shell tool (`bash` in 2.1; `shell` in 2.2,
whose built-in input repair renames a model's `bash` call to `shell`
before any plugin sees it), so `git.protect` sees an
agent's `git push` before it runs. A shell command the plugin can't check
still runs; an edit it can't check is refused.

When several rules answer, the most severe wins (deny, then block, then
context), and their texts join in the order above. A refusal says what to
do next and carries a stable code (`hook.git_protect.denied`). The rule
format (`on`, `match`, `use`, `who`, `mode`, `timeout`, `on_error`) is the
one config will use to tune these rules by id.

Each adapter declares the events its harness delivers. A rule none of
whose events a harness delivers is unsupported there, and `mdkg doctor`'s
hooks check names it (`codex lacks mail.wake (session.idle)`), never
skipping it silently. The [doctor fixture](../crates/mdkg-cli/tests/hook.rs)
checks the list.

**Actors.** A harness hook's actor is an agent. git's hooks see people
too, and decide by the worktree first (orchestrator, 2026-10-29): a
worktree that maps to an agent (a lane's own, or one the launcher saved)
is that agent whatever the shell's environment says, so `env -i`, `env
-C`, `builtin cd` or a `git` function can't make its push a person's,
and a person working in a lane's worktree is treated as that agent.
Identity records that can't be read, or disagree about the worktree,
refuse the commit or push rather than check another actor's rules.
Identity and rules come from the committed graph, read as git objects,
never from the checkout's files: a worktree whose `.mdkg` is missing
from disk is still read from what its HEAD commits, and in ref mode a
checkout with no `.mdkg` at all is read from the graph ref. A branch or
worktree whose HEAD commits no graph (an orphan `gh-pages` branch, a
release branch from before the graph, a rebase of commits older than
it) reads the graph the main ref commits: `mdkg.mainRef`, else `main`,
as a local branch or else a remote-tracking ref of that name (a clone
with only `origin/main` sets `mdkg.mainRef origin/main`),
so its actors, protected branches and lease checks still apply. Only a
clone with no graph at all, where neither HEAD nor the main ref commits
one, is an absent graph: its commits and pushes go through, with
`git.protect` and `secret.scan` still applied. Graph settings, a graph
or a repository that can't be read refuse every commit and push (a
person can run git with `--no-verify`); where the graph settings are at
fault, the refusal names `mdkg doctor` and `mdkg install`. A push
from a repository with no worktree (a bare one) is refused inside a
live turn.
Second,
a push or commit whose process ancestors include a launcher's live turn
(the turn's mark in an ancestor's environment, with this repository's
record of that turn still held by its launcher) is that turn's agent,
wherever it runs: the harness process keeps its mark when a command runs
under `env -i` in another folder. If the process chain can't be read,
the worktree and the environment decide. Anywhere else, git's hooks
count the shell an agent when one of these is set:
`MDKG_RUN_TURN` (mdkg's launcher), `CLAUDECODE` (Claude Code),
`CODEX_THREAD_ID` (Codex), `OPENCODE` or `OPENCODE_SESSION_ID`
(OpenCode), or `AI_AGENT`; otherwise it's a person, who meets only
`secret.scan` and gets a warning wherever an agent is refused.

**The boundary.** mdkg's hooks guard one trust domain against an
agent's mistakes, not against deliberate evasion. A process that
detaches itself from the process tree and works outside a lane's
worktree, or work done under another user's account, is outside what
they can see. Containing an agent that means to get round them is the
sandbox's job, outside mdkg, with branch protection on the hosting
service as the backstop.

**pre-push is the guarantee; the harness check is advisory.** git's own
`pre-push` hook sees the exact refs a push updates, with no shell to
read, so it's what keeps an agent off a protected branch (with
`hooks:git` installed). Before a shell tool runs, the harness hooks also
read the agent's command for a `git push` and refuse early what they can
see: past `env`, `env -S`, `sudo`, `nice`, `timeout` and the like, one
level into a `sh -c`, `bash -c`, `zsh -c` or `dash -c` string, and every
push in the command. That reading is an allowlist: a push passes it only
when the whole command is read with certainty, and anything else (an
expanded remote or branch, a refspec pattern, an option mdkg doesn't
know, config or a repository set for the command) is refused with the
explicit-refspec hint. It's best effort and won't be extended further;
it can't see `eval`, scripts or aliases, which is why the git hook is the
guarantee. One route skips git's hooks themselves: `--no-verify`. The
harness check refuses it where it can read it; where it can't, GitHub's
branch protection is the backstop.

**Protected branches** are `main`, `master`, the main ref
(`mdkg.mainRef`), the agent definition's `protect` list, and the branches
in GitHub's protection cache when it's there
(`<git common dir>/mdkg/hooks/github-protection.json`). In ref mode the graph
ref (`mdkg.graph.ref`) and its configured remote ref (`graph.remoteRef`) are
protected too. The configured ref names come from the clone's common config,
never `$MDKG_MAIN_REF`; the agent's list comes from committed authority. A
hook never calls the network. Branch names compare
without case. A push naming no branch,
or `HEAD`, is checked against the branch checked out and its push target
(`@{push}`). `--all` or `--mirror` is refused, as is an agent's push that
skips git's hooks (`--no-verify`, or `-c core.hooksPath=...`).

Before a shell tool runs, `git.protect` checks a plain push exactly and
fails closed on the rest: a push whose branches it
can't map with certainty is refused for an agent, with a hint to push an
explicit refspec to a named branch (`git push origin work:work`). That
covers a remote whose own push refspecs (`remote.<name>.push`) decide
where a push naming no destination goes (globs, deletions, a tag's or a
commit's refspec), a mirror remote, a `push.default` other than `simple`,
`current` or `upstream` (`matching`), a push with nothing checked out,
`push.default=upstream` with an upstream that doesn't resolve, push,
remote, branch or url config set for the one command (`git -c
push.default=matching push`; any `-c` or `--config-env` on a push,
`include.path` among them) or assigned in the
command's environment (`GIT_CONFIG_COUNT`, `GIT_CONFIG_PARAMETERS`,
`GIT_DIR`, `HOME` and the like), `--git-dir` or `--work-tree`, a push
option mdkg doesn't know (git also takes abbreviations such as `--rep`),
and a `-C` folder, or a `cd` or `pushd` earlier in the command, that the
shell expands or that isn't in this repository. A mirror is read as git reads
the boolean (`yes`, `on`, `1`). `--repo` counts as the remote, and
`git -C <dir>` is read in that folder. `--prune` is a force push. A `HEAD` the push
names is the branch checked out, by name. git's own `pre-push` hook sees the exact
refs, so it needs no such rule, and GitHub's branch protection stays the
backstop. A force push of any ref is refused: a deletion
(`:branch`, `--delete`), and moving a tag the remote has, even forward.
Pushing a feature branch for a pull request
passes, and so does a person's push of `main`, so nothing here assumes
work lands by a local merge.

The 0.7.0 behaviours, now the mail and lease rules:

- **Turn start** (`mail.notice`): the agent is told its unread mail, as
  "You have mdkg mail: #212 handoff from worker". Hooks show
  number, kind and sender, never a body: what a harness injects is trusted
  more than tool output, so bodies stay behind `msg inbox`.
- **Turn end** (`mail.stop`): unread, wake-worthy mail newer than the session
  has already been told about blocks a stop once. Mail delivered at start
  does not block the same session again just because it remains unacked.
- **Idle wake**: Claude and OpenCode use `wait` after a turn ends; Codex
  needs its separate interactive mail receiver. Direct mail of any kind
  wakes a session, as does a
  channel's question, handoff, decision, findings, review_request or
  resolution; channel notes don't, and neither does a note replying to the
  agent's own message, in the hooks and in Codex's receiver. (Past 40
  pages of such notes, the receiver wakes on the newest one rather than
  read on, so later mail is never hidden.) A session's reminder record
  prevents a stop and a waiter from delivering the same new mail twice.
- **Edit guard** (`lease.edit`): an edit to a file the agent holds no lease on is
  refused, with the claim command to run, as is any edit inside git's own
  folder. A file in another worktree of the repository is checked against
  the repository's lease. Like the permission rules, it's a guardrail, not
  isolation: a worker that runs code can still write anything. It costs
  one `mdkg` process per edit. Legacy commit-message files require leases
  like other files; there is no launcher control-file exemption.
- **Leases after `done`** (`lease.done`): a stop is blocked once per `done` when the
  agent ran `mdkg done` in this session, holds no node's lease (no new
  task underway), and still holds leases on paths. It names how many and
  asks the agent to release what it's finished with (`mdkg status` lists
  them); paths aren't echoed.
- **Turn-end audit** (`lease.audit`): agents write through their shells too
  (Bash, OpenCode's `shell` and `execute`), which no edit hook sees. So at
  each stop, `git status` identifies files changed without a lease of the
  agent's. Feedback reports their count and asks the agent to inspect
  `git status --short`, then claim or revert them. Filenames are not
  injected as instructions. The stop is blocked once per changed-file set.
- **Agent threads**: the start and stop hooks keep a central record of
  each session (actor, harness, session, folder, working or idle, last
  seen) under the git folder, and `agents status` lists them. Records retain
  the producing event and separate first-seen, last-seen and state-change
  times. A failed observation produces a diagnostic; the last stored state
  is not proof that a process is still alive. A valid session/worktree
  binding supplies its actor instead of an inherited daemon environment.

Hooks are projections: their canonical definitions are in `.mdkg/hooks/`,
and `.mdkg/config.json` declares which are enabled. `mdkg init` enables
no projections.

| Projection | Canonical definition | Harness target |
|---|---|---|
| `hooks:claude` | `.mdkg/hooks/claude/settings.json` | `.claude/settings.local.json` |
| `hooks:codex` | `.mdkg/hooks/codex/hooks.json` | `.codex/hooks.json` |
| `hooks:opencode` | `.mdkg/hooks/opencode/mdkg/server.ts` | `.opencode/plugins/mdkg/server.ts` |
| `hooks:git` | none: written by the build | `core.hooksPath` naming `<git common dir>/mdkg/hooks/git/` |

`hooks:git` changes every commit and push in the checkout, so it's its own
step (`mdkg install --enable hooks:git`). Install points `core.hooksPath`
at mdkg's folder in the git common dir, shared by every worktree. Its
`pre-commit` and `pre-push` run the rules and then the hook of the folder
`core.hooksPath` named before (git's default `hooks/` folder, or husky's),
and every other hook found there gets a shim that runs it, so no hook stops
running and none is moved or replaced. A hook added to the old folder later
needs another `mdkg install`. The folder it chains to is the
`core.hooksPath` git uses, from any config scope and with `~` expanded
(a global `~/.githooks` keeps running); install follows a folder another
tool has set since. A worktree's own config (`config.worktree`) beats the
repository's, so install refuses while any worktree's own config sets
`core.hooksPath` (naming the files), and `mdkg doctor` reports it. Only a
refusal (exit 1) stops git: an mdkg that's missing or fails warns and
lets the commit through. A copy of git's input that fails partway stops
it too, since no hook would see all of it. Disabling the projection, or
`mdkg install --undo`, puts the repository's own `core.hooksPath` back as
it was; undo does so even when install changed no file. The folder is the
repository's, shared by its worktrees, so only an undo in the worktree
whose install wrote it last removes it. The
[git adapter fixtures](../crates/mdkg-cli/tests/hook.rs) cover an agent's
refused commit and pushes, a person's commit and push of `main`, a feature
branch, an existing hook and husky's folder, and install and undo.

When a canonical definition is absent, install writes the build's default
before projecting it. Defaults ship for all three harnesses. Existing
canonical definitions remain the source of truth, except exact previous
OpenCode defaults shipped by mdkg: `mdkg install --hooks` replaces those
with the current default while retaining their binary path. Customized
plugins are preserved. Doctor reports verified shell routing only for an
exact current default; every customized plugin gets an unverified-routing
warning. Compare its `execute.before` handler with the shipped plugin:
adding `SHELL_TOOLS` alone is insufficient. In ref mode doctor reads the
canonical from the graph tip and compares the checkout's projection.
`--dry-run` previews the projected file as well as any canonical update.
Enable the harnesses you use and inspect the plan first:

```sh
mdkg install --enable hooks:claude,hooks:codex,hooks:opencode --dry-run
mdkg install --enable hooks:claude,hooks:codex,hooks:opencode
mdkg doctor
```

The [fresh-init setup test](../crates/mdkg-cli/tests/setup.rs) checks that
enabling all three writes their canonical definitions and projections.
Install merges mdkg's command entries beside the user's hooks; OpenCode gets
a separate plugin directory. Doctor
checks enabled projections against their canonical definitions. Hook
projections do not implicitly enable MCP, the AGENTS.md block or skill
mirrors; those have their own opt-ins. OpenCode executes project plugins
without a trust prompt, so review the canonical plugin before enabling it.

When a hook matches its session and worktree to a live launcher turn
(`mdkg agents up`), it suppresses duplicate mail notices and waits:
the launcher delivers and acknowledges that turn's mail and schedules the
next turn. An inherited launcher marker alone is insufficient. The edit
guard, stop audit and thread observations still apply. Outside a graph,
the hooks do nothing. The [shared hook tests](../crates/mdkg-cli/src/hook/mod.rs)
and [integration fixtures](../crates/mdkg-cli/tests/hook.rs) cover these rules,
including failed reminder writes, malformed edits and unrelated sessions.

Harness behavior and qualification:

- **Claude Code** gets SessionStart and UserPromptSubmit context, a Stop
  hook that blocks once (Claude's `stop_hook_active`), an async Stop hook
  with `asyncRewake` for the idle wake, and a PreToolUse deny on Edit,
  Write, MultiEdit and NotebookEdit, and on a Bash `git push` that
  `git.protect` refuses. The 2.1.309 spike confirmed a stop
  reason reaching the model, a forced Write denial and a second turn with
  streaming input. In plain `claude -p`, `asyncRewake` ran synchronously as
  a blocking stop; the TUI and desktop Code tab were not tested. Do not
  treat streaming-input evidence as qualification of those surfaces.
  Bare mode (`CLAUDE_CODE_SIMPLE`) skips hooks. Read mail through the mdkg
  MCP inbox tool, or `mdkg msg inbox` when the harness permits it.
- **Codex** 0.164.0 stable confirmed a stop continuation and an
  `apply_patch` denial in the spike with an invocation-only trust bypass.
  The mdkg adapter uses SessionStart/UserPromptSubmit for context, Stop for
  reminders and the audit, and PreToolUse for every file named by
  `apply_patch` and for Bash's git pushes (`apply_patch|Bash`; that Codex
  names its shell tool `Bash` there is still to be checked on a real
  turn). In a session with no binding, only an edit is refused; reads and
  shell commands run. Codex has no idle hook that starts a later turn; a Stop
  hook alone cannot provide idle wake. The spike did not qualify persisted
  per-hook trust. This build includes the default hook definition and
  adapter. Receiver qualification is described [below](#interactive-codex-mail-receiver).
- **OpenCode**'s plugin (2.1.0 and 2.2.5) adds the mail line to each model request's
  context. When a session's run ends (`session.execution.succeeded`), it
  queues a reminder for unread mail, else waits and prompts the idle
  session when mail arrives, which needs a server that outlives the turn
  (`opencode serve` or the TUI; `opencode run` ends at the first idle).
  `execute.before` throws for an unleased edit, including each file an
  `apply_patch` names, and also when it can't run mdkg to check; it also
  routes the shell tool (`bash` or `shell`), throwing for a push
  `git.protect` refuses. The spike
  confirmed plugin loading, a `write` denial and a queued wake on a
  long-lived server. `run --standalone` cut off the queued turn at shutdown.
  OpenCode fell back to its shell after a denial in that probe.

These observations came from isolated harness probes using small test
hooks rather than the installed mdkg workflow.
The [Claude adapter](../crates/mdkg-run/src/harness/hook/claude.rs) and
[OpenCode plugin mock check](../scripts/check-opencode-plugin.mjs) cover the
generated adapters; neither substitutes for a real harness turn.

### Codex hook trust

Interactive Codex requires project trust and a review in `/hooks` once for
each unchanged hook definition. A new or changed definition must be reviewed
again; mdkg writes no Codex trust hashes. See the official
[hook-trust documentation](https://learn.chatgpt.com/docs/hooks#review-and-trust-hooks).
mdkg withholds `--dangerously-bypass-hook-trust` for every launch, resume,
retry and preview. A project-file check cannot authorize invocation-wide
trust; verification of every source Codex would load remains unqualified.
Reviewers also receive no bypass. The turn report records the decision.

Ordinary trust applies: unchanged, enabled, trusted hooks can run; non-managed
hooks needing trust are skipped until reviewed in `/hooks`. Untrusted mdkg
hooks therefore supply no mail context, edit guard or stop audit until their
definitions are reviewed and trusted. An untrusted project's configuration,
hooks and rules are skipped, while user and system sources still load; see
[configuration precedence](https://learn.chatgpt.com/docs/config-file/config-basic#configuration-precedence).

### Checking OpenCode hooks on a real model

From a source checkout, build the candidate and canonical-plugin exporter,
then run the offline checks (Python 3.9+, Node 24+):

```sh
cargo build --locked -p mdkg --bin mdkg
cargo build --locked -p mdkg-run --example opencode-hook-plugin
cargo build --locked -p mdkg-run --example opencode-worker-config
python3 -I scripts/check-opencode-hooks.py --self-test
node scripts/check-opencode-safety.mjs --self-test
node scripts/check-opencode-plugin.mjs target/debug/examples/opencode-hook-plugin
```

For the live checks, use a qualified OpenCode (**2.1.0** or **2.2.5**) on
macOS or Linux. The permission check starts no session and calls no
model: it starts a loopback server with this build's worker inline config
and checks that OpenCode's resolved `build` agent denies mdkg's refusals
and the credential commands, taking the last matching rule:

```sh
python3 -I scripts/check-opencode-hooks.py --live-permissions \
  > /tmp/opencode-permissions-result.json
```

For the hook check, use network
access and name an available Zen free model explicitly. Replace the example
model if needed. This creates and commits a temporary Git repository, so run
it only where Git writes and real model calls are permitted:

```sh
python3 -I scripts/check-opencode-hooks.py --live \
  --model opencode/deepseek-v4.1-flash-free > /tmp/opencode-hooks-result.json
```

The [driver](../scripts/check-opencode-hooks.py) starts `opencode serve`,
runs four sessions with `run --server`, and reads their stored messages to
verify start context, an unleased write denial, a stop reminder and an idle
wake. `run` stdout alone cannot prove the queued replies. It uses temporary
HOME/XDG/TMP directories and a throwaway server password, without inherited
provider credentials. Preflight refuses external system, managed or ancestor
configuration. A separate fixture plugin permits only the intended guarded
write; reads, shell fallbacks and other tools are denied before any session.
That fixture restriction is specific to this check, not mdkg's normal guard.
The JSON result records evidence and failures; process groups and temporary
files are cleaned up on completion, error or interruption. Offline passes
are not a live qualification receipt.

### Checking Claude hooks under the normal login

The [Claude check](../scripts/check-claude-hooks.py) targets Claude Code
**2.1.309** and **claude-haiku-5-5**. Its offline checks use no Git, Claude,
mdkg, network or model calls:

```sh
python3 -I scripts/check-claude-hooks.py --self-test
```

For a live run, supply absolute paths to the candidate mdkg build and Claude
executable. `--person-reviewed` permits preparation for a supervised `/hooks`
review; omitting it refuses before fixture setup:

```sh
python3 -I scripts/check-claude-hooks.py --live \
  --mdkg /absolute/path/to/candidate/mdkg \
  --claude /absolute/path/to/claude \
  --model claude-haiku-5-5 --person-reviewed > /tmp/claude-hooks-result.json
```

The script prints a review helper for the generated fixture. It establishes
the same allowlisted environment, repeats preflight and subscription checks,
and opens interactive Claude with project/local sources and strict fixture
MCP. In another terminal, run it, inspect `/hooks`, then exit that review
session. Only then write the one-use confirmation marker with the printed command. The script
checks that the settings stayed unchanged before starting acceptance sessions.
Review times out after 15 minutes (`--review-timeout` changes it), cleaning up
the fixture. The interactive review session is operator-owned: exit it before
confirming or interrupting the check.

For a person-approved inventory instead of interactive review, add
`--review-mode inventory --inventory-sha256 <report-sha256>`; this keeps the
private confirmation barrier, records the report hash and approval attestation,
and permits catalog metadata while refusing
registered plugin entries, nonempty plugin payload caches, plugin manifests,
and hook declarations in skill or legacy command headers.

This supervised check requires the person's normal Claude subscription login,
backed by their macOS Keychain on macOS. The environment sets `USER` and
`LOGNAME` from the current OS account rather than inherited values. By default, every receipt says
**with unverified effects: managed/server policy**; a successful result is
**pass with unverified effects**, never fixture-only qualification. Claude's
[setting-source controls](https://code.claude.com/docs/en/cli-reference) do
not exclude managed policy. Preflight still refuses detectable local managed
configuration and external plugin sources, including links and unreadable
entries. Invocation-scoped settings disable account plugin/skill sync;
the script does not change the person's settings or read authentication files.
Claude's own auth-status command must confirm a first-party subscription
login before review and each model invocation. Saved Console/API credentials
are refused; inherited provider keys are not passed to the fixture.

The optional `--allow-builtin-plugins` accepts only init plugins whose path is
`builtin` and source ends with `@builtin`, records the flag, per-session names
and `claude_sha256`, and expands the scope to **with unverified effects:
managed/server policy and Claude built-in plugins** while still refusing other plugins.

The candidate installs its canonical hooks in a temporary repository. The
check verifies start headers without bodies, denied and allowed writes,
stop mail inserted after the start read plus an unclaimed-change audit, and
a real headless worker's inbox call and delivered reply. The finite check
omits the asynchronous idle-wake entry; it does not qualify idle wake.
Only reading/writing the exact test file or calling the worker inbox tool is
permitted by its fixture guard. Git writes and coordinator operations stay in
the temporary repository and its worktrees.

The JSON receipt includes fixture paths, session IDs, hook-reported transcript
paths, and possible Claude cleanup locations. Claude can write transcripts,
project entries and other state under the normal home; see
[Claude's data directory](https://code.claude.com/docs/en/claude-directory).
The person must inspect and remove only this check's data afterward. The
script drains the candidate MCP server's calls, cleans up its own process
groups and temporary files, and never deletes data from the person's Claude
home. An offline pass is not a live qualification receipt.

## Troubleshooting

- **Hooks do not run:** check `mdkg doctor` and whether the corresponding
  `hooks:<harness>` projection is enabled. Inspect `mdkg install --dry-run`
  before refreshing it. In Codex, check project trust and `/hooks`; installing
  the projection does not itself trust it. In Claude, check for bare mode.
- **An edit is denied:** claim the attempted paths as the session's actor
  (`mdkg claim <path>`, or `--new` for a new file). A malformed edit payload or
  failed check can also deny it; inspect the user-facing hook diagnostics.
  At stop, use `git status --short` to inspect the files counted by the audit.
- **A thread record is stale or mail is quiet:** inspect hook stderr or the
  plugin log for recording failures, verify the session's actor, and inspect
  `mdkg msg inbox`. Start context can repeat unread headers on later prompts
  or model requests until acknowledgment. Stop and wake issue no new mail
  notice without newer wake-worthy mail; a Stop notice may also include
  older unread headers. An unreadable reminder record suppresses new reminders;
  repair the storage/access problem rather than deleting state to force a wake.
- **The OpenCode live check refuses external configuration:** its temporary
  directory may be below a configured project, or system/managed configuration
  may be present. Use a clean test environment with a temporary parent outside
  projects. Do not disable the preflight or remove user configuration to make
  the test pass. An inactive plugin or missing readiness receipt also prevents
  sessions from starting.
- **The live model returns `429 FreeUsageLimitError`:** the Zen free quota
  is exhausted. Retry when it is available; a provider error remains a
  nonzero check result and qualifies none of the uncompleted scenarios.
- **"isn't logged in"** means the harness has no login to bill the turn
  to. Run its login command once: `claude auth login`, `codex login` or
  `opencode auth login`. For Claude or Codex, you can instead bill one
  turn on your API key with `mdkg agents use-key <agent>`, once
  `auth.api_key_fallback` is on in the agent's policy and a key is in the
  Keychain (see "Which account a turn bills"). OpenCode has no key
  fallback.
- **`run.branch_refused`** can mean a worker isn't on its own branch,
  named by `orchestration.branch` (by default, `agent/<agent>`).
  `mdkg --root .worktrees/graph agents edit <agent> --worktree` commits the
  updated definition. Then `mdkg --root .worktrees/graph agents up <agent>`
  creates that branch for a new checkout, but reuses an existing worktree without switching its branch.
  If that checkout is on the wrong branch, switch it to the configured
  branch by hand, or point the agent at a correct checkout with
  `agents edit <agent> --cwd <folder>`. If the refusal names the launcher on main, run the
  launcher from the graph worktree as described above.
- **"would overlap the worktree"** means the proposed worker worktree
  would sit inside another worktree or contain one. Change
  `orchestration.worktree` so agents' worktrees sit side by side.
- **"loop guard paused"** means a thread reached its automatic turn
  limit; its mail stays waiting. Inspect it with `mdkg agents status`,
  then use `mdkg agents resume <agent> --thread "<thread>"` (copy the
  whole thread key from status and quote it, since it contains a space),
  or wait for enough counted turns to leave the window, which is one hour
  by default.
- **`coord.lease_held` on `run:<agent>`** means another launcher or
  `install` holds that agent's turn. Wait for it to finish before running
  the agent again; the turn lease prevents simultaneous turns or an
  install changing the worktree during a turn.

## Limits

- The Codex receiver's fault bar applies from the hook's first durable write:
  its pending marker. A failure before that marker exists is equivalent to
  the hook never firing for the new session. The old idle route can remain
  eligible, and status can still show it without pending evidence. A reminder
  may then be queued to the previous conversation, which can read and act on
  the mail under its ordinary permissions. The next recorded conflicting
  observation suspends future wakes; it cannot retract a queued reminder.
- The final marker check, after registry and projection reads, is the
  receiver's admission point. A marker arriving later cannot cancel an
  already admitted spawn, so the previous conversation may still receive
  that reminder. Persisted pending evidence blocks subsequent admissions.
  Both this boundary and the pre-marker boundary are accepted for 0.7.0.
- Queue admission is not fenced at the Codex daemon. The receiver verifies
  ownership after executable discovery; its guard checks the same absolute
  lease expiry immediately before starting Codex. A pause before that check
  cannot restart the budget. The local deadline is the earlier of that lease
  expiry and command preparation plus the policy timeout (at most 30 seconds).
  The guard checks every 10 ms while the child runs and kills its process group
  on expiry; OS scheduling, process creation and signal delivery add latency.
  A pause after the guard's last check can still let Codex send after expiry,
  and a request already in transit can be accepted after cancellation. There
  is no hard upper bound on daemon acceptance without daemon support. The
  receiver records expiry after a possible Codex launch, or a late success
  receipt, as **delivery unknown**, consumes the reminder and never retries it
  automatically. A proven refusal before Codex starts is **not sent** (`NotSent`),
  releases the reservation and can be retried. A stale holder
  can still deliver a duplicate or wake the previous conversation during
  this interval. Stable IDs in the reminder identify each mail/thread pair;
  they do not enforce deduplication. This is an accepted limitation of 0.7.0.
  The [startup-pause and expiry fixtures](../crates/mdkg-cli/tests/receiver.rs)
  measure local cancellation only, not daemon acceptance.
- Abandoned Codex observations remain blocking across restarts, including a
  first registration with no route file; `agents status` reports an incomplete
  observation. Each actor has 64 pending slots, with at most one retained
  witness per slot. If all slots are occupied, another observation leaves an
  overflow latch; completing active hooks does not clear that latch. Full
  `mdkg agents resume <actor>`, after stopping its supervisor, clears abandoned
  evidence and overflow, then waits for a primary prompt with SessionStart
  provenance. The `--thread`
  form only clears the loop guard and cannot recover this state. Reset sends
  nothing and does not retry consumed mail. The [fault/concurrency tests](../crates/mdkg-run/src/receiver/dispatch.rs)
  and [receiver fixtures](../crates/mdkg-cli/tests/receiver.rs) cover these limits.
- Edit-tool hooks do not intercept shell writes. The stop audit is a
  once-per-file-set reminder after the change, not prevention or rollback;
  it can be skipped when the worktree cannot be read. Hooks are guardrails,
  and ordinary agent turns still need your own sandbox for isolation.
- Stop and wake reminders require readable, writable coordinator state.
  Failure to persist a reminder suppresses delivery rather than claiming it
  was delivered; failed thread observations leave the previous record in
  place and emit diagnostics. A listed idle thread is an observation, not a
  guarantee of a live or selected interactive session.
- A process an agent leaves behind can outlive its turn if it drops the
  turn's mark and leaves the process group, or on macOS if it's one of
  Apple's own programs (`sh`, `zsh`, `perl`, `sleep`), which hide their
  environment. If the launcher is killed mid-turn, only the agent's group
  is stopped.
- An `install` stopped with Ctrl-C leaves its leases and marker; the next
  turn or install takes them back once their holder is gone.
- One actor holds at most 32 leases. `install` holds one turn per agent
  working in or around its worktree, so it fails with 33 or more there.
  This limits simultaneous claims, not the files a worker's own Git commit
  can contain; the launcher performs no commit step.

## Interactive Codex mail receiver

An actor with a saved agent spec, a committed definition or a deprecated
inline entry is headless. An actor with none is interactive. This is the mode-selection
seam; missing, unreadable or busy session evidence never permits a headless
fallback. `agents.defaults` alone does not make an actor headless.

With the project's `hooks:codex` projection enabled and its hooks installed
and trusted, run Codex in the actor's existing
worktree and keep `mdkg agents up <actor>` running beside it. The worktree
must match the repository's configured worktree pattern, including its
prefix/suffix and colon escaping; mdkg refuses ambiguous actor mappings.
The first primary SessionStart registers its conversation. Subagent
SessionStart records do not select a route. An observation from another
primary or unclassified session suspends wakes; a late Stop from the old
session cannot clear that suspension. `agents status` shows the durable
receiver route, its suspension and the last delivery attempt. The default
`agents up` list includes registered actors, but use the explicit actor
argument to wait before its first registration. Registration and each dispatch
check the opt-in; legacy configs and a disabled projection do not enable wakes.

Each receiver observation first creates a pending marker, before route locks
or reads. Any retained evidence blocks admission, even if no route was written.
Successful status and route writes complete the observation; a crashed or
failed observation remains blocking across restarts. An old-format route waits
for a completed fresh observation. Partial route writes are unreadable and
refuse admission. Independent markers are not subject to status pruning; the
receiver also checks the thread registry under its lock. See the failure and
admission boundaries in [Limits](#limits).

The receiver verifies and renews its existing run lease after admission waits
and immediately before queueing. It never reacquires an expired lease to use
an old routing decision. While the queue process runs, it renews every 100 ms;
loss or renewal failure stops that process and leaves delivery uncertain.
The ownership read, renewal and verification share a five-second budget;
after the ownership read, renewal and verification are also bounded by half
the remaining lease time.
They renew only the local turn and never start an external lease authority.
The independent queue guard receives that absolute epoch-millisecond lease
deadline, using the same clock as renewal. It checks again after its own
startup and before starting Codex; renewals never extend this dispatch's cap.
A blocked receiver renewal cannot restart the guard's budget. Expiry stops
the child when the guard is scheduled; the check-to-daemon-acceptance interval
and scheduling limits are described in [Limits](#limits).
Cleanup removes only its own holder record under the lock shared by receiver,
headless launch and install. The [lease-expiry and takeover fixtures](../crates/mdkg-cli/tests/receiver.rs)
cover these checks, including expiry while the registry is locked.

The receiver checks admission under the actor's run lease, reads unread
headers with `msg inbox --no-mark` / `msg wait --no-mark`, and queues only
headers newer than the conversation's shared reminder watermark. It never
acknowledges mail or marks its body shown. Busy sessions do not reserve mail;
the stop hook can remind them. Final generation/route validation and queue-process
creation share the observation lock, which is released before waiting for the
child. Lock-free markers are rechecked after registry and projection reads,
immediately before admission. Evidence present at that check refuses dispatch.
On an idle observation it runs:

```text
codex queue --remote unix:// --thread <registered-session-id> --message <header-reminder>
```

Each reserved mail number has a deterministic `mdkg-…` ID derived from that
number and the thread UUID, independent of attempt or batch. These IDs travel
as reminder text. Codex 0.164.0's installed `queue --help` exposes no client-ID
option; its CLI generates a fresh `clientUserMessageId` for each invocation.
Consequently repeated invocations do not reuse a transport deduplication key.
Whether `thread/queue/add` rejects or coalesces a repeated client ID has not
been qualified; mdkg relies on neither behavior and adds no automatic retry.

There is no sender fast path. The supervisor supplies no prompt to a model
while waiting, and never answers an approval request. A hook's idle state
can become stale between checks; queue submission is used so the reminder
does not steer a person's active turn. A successful CLI exit means queued,
not that the model received or completed it. The queue subprocess has a
30-second deadline from command preparation (a shorter policy timeout or
lease expiry applies to the subprocess, not to the resulting model turn).
A definite refusal before Codex starts or failure to start that subprocess
releases the reservation for another attempt. A timeout, nonzero exit,
crash or interrupted supervisor leaves uncertain delivery consumed; new
mail excludes those older headers. Automatic attempts share the existing
conversation loop guard. A supervisor containing interactive receivers refuses
a model-turn concurrency cap, because a queue receipt cannot enforce it. No raw queue output or transcripts are retained. An independent process-group
guard owns the queue child from birth and checks parent identity and the
absolute deadline independently of the supervisor. It attempts to stop the
process group on parent death or expiry; cancellation cannot retract an
accepted request. Automatic Codex discovery searches only
absolute PATH directories. Empty, dot and relative entries are ignored;
`MDKG_RUN_CODEX` can separately supply an explicit executable path.

The registration outlives the TUI. **A registered conversation may be woken
after its TUI closes.** Codex has no qualified TUI-liveness signal in this
path; stop `agents up` to stop its future attempts. An already queued
submission remains Codex's responsibility. Plain Codex without the running
receiver does not wake on mdkg mail.

Stop the actor's running supervisor before plain `mdkg agents resume <actor>`:
it needs exclusive progress ownership. To clear one loop-guard pause while
the watch stays active, use `--thread codex:<session-id>`. This preserves
durable progress, routing and any suspension; it cannot recover an unfinished
turn or receiver dispatch. Interactive resume also serializes with receiver
activity, so inspect a reported busy state before retrying.

For an interactive actor, plain `mdkg agents resume <actor>` clears its loop
guard and resets routing to wait for a new primary UserPromptSubmit; it sends
nothing itself. A delayed Start/Stop cannot select the replacement. A
session already classified by SessionStart may supply that next prompt;
an unknown session needs SessionStart provenance first. Reset discards old
provenance except for the previous bound primary, so the bounded record can
recover from a full history. Discarded sessions require fresh SessionStart
provenance; an unclassified observation still suspends a selected route. The optional
`--thread codex:<session-id>` clears only that conversation's loop guard
and preserves the route and any suspension. It never retries consumed mail.
A complete resume can recover a malformed regular route file after an interrupted
write: it preserves the damaged bytes privately beside that record and waits for
fresh primary SessionStart provenance followed by a UserPromptSubmit. It does not
repair links, directories, oversized or unreadable evidence. Ordinary hooks and
dispatch never repair malformed authority automatically.
These mode, closed-TUI lifetime and resume semantics are the accepted
behavior for 0.7.0.

Qualification: the adapter targets Homebrew Codex 0.164.0 stable; the
0.164.0-alpha.11.1 adapter was also tested earlier. The dedicated-daemon
probe confirmed equal hook session/thread ids and queue-after-completion
behavior. This receiver integration is tested offline with a fake Codex
process. A supervised check on Codex
0.164.0 confirmed the default `unix://` endpoint and normal idle queue delivery
after a completed turn: the queued header started a new turn without another
user prompt, and that turn completed normally. This is one successful trial,
not a reliability guarantee. Interrupted conversations remain paused, and
reminder IDs do not enforce deduplication.

A read-only Codex session can be woken but cannot yet read its mdkg mail.
In that check, the model attempted to read its inbox,
which failed with `coord.store_failed`; the wake succeeded without a successful
mail read. Plugin MCP startup effects were not verified.
Project and individual hook trust must be reviewed in Codex; an untrusted
checkout cannot register through its hooks. mdkg never writes Codex's trust
hashes.

Enable the Codex projection with `mdkg install --enable hooks:codex` (use
`--dry-run` to review the plan first). When the project has no canonical
file, install creates `.mdkg/hooks/codex/hooks.json` using its stable mdkg
binary spelling and merges its four entries into `.codex/hooks.json`.
SessionStart and UserPromptSubmit call `hook start`, Stop calls `hook stop`,
and PreToolUse matches `apply_patch` and calls `hook edit`, all with
`--harness codex`. A path containing spaces or quotes remains one executable
word. Keeping the command spelling stable avoids changing hook trust hashes
merely because a package upgrade moved its versioned executable.

Sync preserves the user's other hooks. Disable removes mdkg's entries and
keeps the canonical file; undo restores the user's previous bytes and removes
a canonical file created by that install. Doctor reports a missing or drifted
projection. These operations never write `config.toml` hook-trust entries.
Review the project and its individual hooks in Codex's `/hooks` interface.

Codex workers follow the [ordinary hook-trust policy](#codex-hook-trust) above:
automatic bypass is unconditionally withheld. The local mdkg-only project
predicate does not grant trust. The [launcher](../crates/mdkg-run/src/profile.rs)
and [launch/retry tests](../crates/mdkg-run/tests/turn.rs) cover that policy.

`agents up` turn reports include `codex_hook_trust` receipts with
an attempt number, typed decision, fixed reason and `preview` flag. Current
decisions are always `required`; `bypassed` is retained only for historical
receipts. Dry-run command arguments and receipts are previews, never
authority for a later launch. Text output labels previews and actual spawn
attempts separately; failure diagnostics retain each attempted launch's fixed
explanation. Hook contents and commands are never copied into these reasons.
Idle turns and other roles/harnesses have no such receipt. Interactive users
still review project and hook trust in Codex; mdkg writes no trust hashes.
