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
  or tool restriction for workers; its own `auth` and `abandon` commands
  remain refused. Workers own their commits, using their native harness
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

`mdkg install` requires a committed root graph at the Git worktree's top.
Installing a checkout with only `sub/.mdkg/` and no root `.mdkg/` is unsupported
in 0.7, as the [installation layout test](../crates/mdkg-cli/tests/run.rs) verifies.

Lay the checkouts out like this:

- Run `mdkg agents up` from the graph worktree, which no worker uses.
  After committing the graph, run `mdkg install` from the main checkout to
  create `.worktrees/graph` on `mdkg-graph`, then launch with
  `mdkg --root .worktrees/graph agents up`. It refuses a checkout of main,
  which changes only through a reviewed merge: merge `mdkg-graph` into
  main through a pull request when you choose.
- Give every worker a worktree of its own that holds no other worktree. The
  easy way is `mdkg --root .worktrees/graph agents add <agent> --harness
  claude --role worker --worktree`. It commits the definition there; then run
  `mdkg --root .worktrees/graph agents up <agent>`. Substitute `codex` or
  `opencode` to choose either of the other first-class harnesses. The first
  pass makes
  `.worktrees/<agent>` in the main checkout on branch `agent/<agent>` and
  later passes reuse it. The names come from
  `.mdkg/config.json`, where `{agent}` is the agent's name:
  `{"orchestration": {"branch": "agent/{agent}", "worktree":
  ".worktrees/{agent}"}}`. After adding the definition,
  `mdkg --root .worktrees/graph agents up <agent> --once --dry-run` plans its
  worktree with the same checks and makes no folder, branch or saved session.
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

The launcher reads definitions and graph content from its own checkout.
Changes committed in another checkout are not visible there until merged.
Install and `agents up` startup attempt a guarded fast-forward of the managed
graph checkout from the configured main branch. They report divergence and
leave the graph unchanged when the histories have diverged.
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

`mdkg init` enables no projections; the agents' files are projections you
enable (`mdkg install --enable agents-md,mcp:claude,…`), listed in
`.mdkg/config.json`, and install writes exactly those. Hooks are
projections too (`hooks:claude`, `hooks:codex`, `hooks:opencode`).

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
`abandon` refusals to both global and selected-agent permissions. Reviewers
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

## OpenCode worker checks

OpenCode workers require **OpenCode 2.1.0** and an enabled `build` agent.
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

A delegate configured in user or machine-local settings can relay approval
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
across the three harnesses. These turn-report fields are separate from the
mailed turn-end receipts planned for 0.7.1.

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
project. The catalog list is OpenCode 2.1.0's, kept in
`crates/mdkg-run/src/opencode-catalog-env.txt`. A newer OpenCode can name
providers this list lacks, and their keys would reach agents until it's
refreshed with `scripts/opencode-catalog-env.py` (a test checks every
name on it is withheld or kept on purpose).

A harness that isn't logged in refuses the turn, naming its login
command.

An API key is used only where the policy says so:

```json
{"agents": {"defaults": {"auth": {"prefer": "subscription", "api_key_fallback": true}}}}
```

With the fallback on, a Claude or Codex turn bills the key when the
harness isn't logged in, and retries once on it when the subscription
hits its usage limit. The key lives in the macOS Keychain, nowhere else:

```bash
security add-generic-password -s mdkg-anthropic -a mdkg -w
security add-generic-password -s mdkg-openai -a mdkg -w
```

Claude gets it through an `apiKeyHelper` (`mdkg auth key
anthropic`, which refuses a terminal); Codex gets it in its own process
only, excluded from the shells it runs for the agent. It's never logged
or printed, and it's taken out of the turn's files and reply. OpenCode
has no key fallback: its own config names its providers. `agents status`
shows each agent's account: `subscription`, `api_key`, `own_config` or
`none`.

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

This display does not validate every setting for execution. In this build,
user or machine-local values for `agents`, `orchestration` and `projections`
appear in the display but are not applied by runtime commands. The output
warns when those layers contribute such values. Runtime layering for
`agents up` and `config set` remain planned for 0.7.1.

Both formats hide recognized credential values as `[redacted]`; detection
is incomplete, and property names and source paths remain visible. The
[configuration display tests](../crates/mdkg-cli/tests/config_commands.rs)
cover source precedence, transitional notices, graphless discovery and
redaction without modifying files.

## Waking agents: `agents up` and the turn policy

With no names given, `mdkg agents up` selects the headless agents named in
`.mdkg/agents/*.json` (or deprecated inline config entries); only when none
are configured does it select all saved agents. It also adds interactive receiver registrations that
have no headless spec. A retained registration cannot add a saved worker that
the config left out. Explicit names select only those actors. The
[discovery regression](../crates/mdkg-cli/tests/agents.rs) covers both defaults
and the retired-worker case.

The supervisor wakes selected agents when mail reaches them or their rooms.
Codex sessions registered by trusted hooks use the receiver described below;
other sessions you start yourself can wait with `msg wait` through MCP.

```sh
mdkg --root .worktrees/graph agents add coder --harness claude --role worker --model sonnet
mdkg --root .worktrees/graph agents edit coder --instructions "Work on the assigned tasks."
mdkg --root .worktrees/graph agents add reviewer --harness codex --role reviewer
mdkg --root .worktrees/graph agents up
mdkg --root .worktrees/graph agents up reviewer --once --fresh
```

When Git content changes, `agents add`, `edit` and `rm` commit only the
affected definition and record its changeset. Otherwise they report the
current HEAD without a new commit. The [definition command tests](../crates/mdkg-cli/tests/agents.rs)
cover clean checkouts, exact commit scope and no-op edits.

An agent's fields (`harness`, `role`, `model`, `effort`, `base`, `cwd`,
`worktree`) live in its [definition file](agent-definitions.md). A worker
with no `cwd` gets its own worktree from the orchestration names; a
relative `cwd` is from the launcher's checkout. Change fields with
`mdkg agents edit <name>`. On supervisor startup, the current definition
replaces its saved runtime settings under the turn lease, including fields
removed with `--unset`. Sessions, turn counts and mail cursors stay local.

`instructions` are the operator's standing instructions, shown before the
mail on every turn: mail bodies are untrusted data, so an agent acts on
what they ask only as far as its instructions say.

The policy is config `agents.defaults`, then the definition's `policy`, field by
field over the 0.7.0 defaults:

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
window, when you run the agent yourself (`mdkg agents up <agent> --once`), or with
`mdkg agents resume <agent> [--thread …]`. `agents status` lists the
agents, their policies and paused threads. With no names, `agents up
--once` runs what automatic discovery finds and stops. Explicit names with
`--once` select one manual pass, which also runs manual-policy agents.
`--prompt`, `--reply-kind` and `--dry-run` require names and `--once`.
`--limit`, `--timeout`, `--keep-logs`, `--fresh` and `--binary` apply to
turn execution. JSON output is one `mdkg.agents.up.v1` event per line;
turn details are under `data.turn`.

### Planned for 0.7.1

Turn-end receipts sent by mail, durable headless turn progress and crash
recovery through `agents resume`, runtime configuration layering for
`agents up`, and `config set` are planned for 0.7.1. In 0.7.0,
`agents resume` already clears loop-guard pauses. The shipping interactive
Codex routing recovery is described [separately below](#interactive-codex-mail-receiver).

## Agent hooks: mail and the edit guard

Hooks check mail and leases at the harness's lifecycle events, including
sessions you run yourself. The command entries and OpenCode plugin call
`mdkg hook <event> --harness <claude|codex|opencode>`, which reads the
harness's hook input and answers in its format:

- **Turn start** (`start`): the agent is told its unread mail, as
  "You have mdkg mail: #212 handoff from worker". Hooks show
  number, kind and sender, never a body: what a harness injects is trusted
  more than tool output, so bodies stay behind `msg inbox`.
- **Turn end** (`stop`): unread, wake-worthy mail newer than the session
  has already been told about blocks a stop once. Mail delivered at start
  does not block the same session again just because it remains unacked.
- **Idle wake**: Claude and OpenCode use `wait` after a turn ends; Codex
  needs its separate interactive mail receiver. Direct mail of any kind
  wakes a session, as does a
  room's question, handoff, decision, findings, review_request or
  resolution; room notes don't. A session's reminder record prevents a
  stop and a waiter from delivering the same new mail twice.
- **Edit guard** (`edit`): an edit to a file the agent holds no lease on is
  refused, with the claim command to run, as is any edit inside git's own
  folder. A file in another worktree of the repository is checked against
  the repository's lease. Like the permission rules, it's a guardrail, not
  isolation: a worker that runs code can still write anything. It costs
  one `mdkg` process per edit. Legacy commit-message files require leases
  like other files; there is no launcher control-file exemption.
- **Turn-end audit** (in `stop`): agents write through their shells too
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
| `hooks:claude` | `.mdkg/hooks/claude/settings.json` | `.claude/settings.json` |
| `hooks:codex` | `.mdkg/hooks/codex/hooks.json` | `.codex/hooks.json` |
| `hooks:opencode` | `.mdkg/hooks/opencode/mdkg/server.ts` | `.opencode/plugins/mdkg/server.ts` |

When a canonical definition is absent, install writes the build's default
before projecting it. Defaults ship for all three harnesses. Existing
canonical definitions remain the source of truth. Enable the harnesses you
use and inspect the plan first with `--dry-run`:

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
  Write, MultiEdit and NotebookEdit. The 2.1.309 spike confirmed a stop
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
  `apply_patch`. Codex has no idle hook that starts a later turn; a Stop
  hook alone cannot provide idle wake. The spike did not qualify persisted
  per-hook trust. This build includes the default hook definition and
  adapter. Receiver qualification is described [below](#interactive-codex-mail-receiver).
- **OpenCode** 2.1.0's plugin adds the mail line to each model request's
  context. When a session's run ends (`session.execution.succeeded`), it
  queues a reminder for unread mail, else waits and prompts the idle
  session when mail arrives, which needs a server that outlives the turn
  (`opencode serve` or the TUI; `opencode run` ends at the first idle).
  `execute.before` throws for an unleased edit, including each file an
  `apply_patch` names, and also when it can't run mdkg to check. The spike
  confirmed plugin loading, a `write` denial and a queued wake on a
  long-lived server. `run --standalone` cut off the queued turn at shutdown.
  OpenCode fell back to its shell after a denial in that probe.

These observations came from isolated harness probes using small test
hooks rather than the installed mdkg workflow.
The [Claude adapter](../crates/mdkg-cli/src/hook/claude.rs) and
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
python3 -I scripts/check-opencode-hooks.py --self-test
node scripts/check-opencode-safety.mjs --self-test
node scripts/check-opencode-plugin.mjs target/debug/examples/opencode-hook-plugin
```

For the live check, use OpenCode **2.1.0** on macOS or Linux with network
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
  `opencode auth login`. For Claude or Codex, you can instead turn on
  `auth.api_key_fallback` in the agent's policy with a key in the Keychain
  (see "Which account a turn bills"). OpenCode has no key fallback.
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
  `mdkg agents resume <actor>` clears abandoned evidence and overflow, then
  waits for a primary prompt with SessionStart provenance. The `--thread`
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

For an interactive actor, `mdkg agents resume <actor>` clears its loop guard
and resets routing to wait for a new primary UserPromptSubmit; it sends
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
