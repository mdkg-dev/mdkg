# Changelog

## 0.7.4

### Upgrading from 0.7.3

- **No store change.** The coordinator store stays at schema 5 (`doctor`
  shows `schema 5+msg5`), so 0.7.3 and 0.7.4 open the same store, and
  going back needs no restore.
- **Replace every mdkg binary that runs a supervisor.** A running
  `agents up` re-execs when its binary is replaced in place. One still on
  0.7.3 keeps its per-second idle polling and wakes on the new `notice`
  mail.
- **Set `wake_kinds` only once every machine runs 0.7.4.** 0.7.3 refuses
  an agent definition that has it.
- **An agent's turn can no longer change `env`, `sandbox` or
  `approvals`.** Make those changes from your own terminal (see Security).

### Security

- **Authority fields are refused inside an agent's turn.** `config set`,
  `config unset` and `agents add`, `edit` and `rm` refuse a change to
  `env`, `sandbox` or `approvals` from inside a turn with
  `authority.field_in_turn`, including defaults, nested keys and whole
  object replacements. A person makes these changes outside a turn, and
  orchestrators get no exception. Identical values, other edits and
  config dry runs still work.

### New

- **`wake_kinds` on agent definitions.** `agents add` and `agents edit
  --wake-kinds question,decision,handoff` list the mail kinds that start
  an automatic turn, for Claude, Codex and OpenCode alike. Other mail
  stays unacked and readable until the next woken turn, a manual prompt
  or digest delivery. An empty list starts no mail-triggered turns, and
  `--unset wake_kinds` restores the default. `agents list --json` and the
  MCP `agents_list` tool show it. See
  [agent definitions](docs/agent-definitions.md).
- **`notice` mail.** A notice is readable in inboxes, threads and digests
  but never starts a turn. A turn's final reply is sent as a notice when
  everything it answered was a resolution, decision, note or notice. An
  explicit `--reply-kind` still wins.
- **`msg pending`** lists a mailbox's unanswered questions and decisions,
  oldest first, with their age (`ageMs` in JSON). Only a reply sent with
  `--reply-to` counts as an answer.
- **`pack --for-message <id> --budget <tokens>`** packs one message's
  thread, the nodes it names and the accepted decisions that point at
  them, within pack's usual budget. The MCP `pack` tool takes
  `for_message`.
- **`agents prepare <name>`** creates or verifies a worker's managed
  worktree as `agents up` would and prints its path. It runs no turn and
  starts no supervisor, so settings files can go into the worktree before
  the first turn.
- **Reviewers record their own requirement verdicts.** The `req_verdict`
  MCP tool, offered only to reviewers, records a requirement `met` or
  `unmet` in the reviewer's name, for a review round that names the
  reviewer at the reviewed commit. When a reviewer couldn't, a
  coordinator outside a turn can relay the reviewer's imported verdict
  with `entry update`'s `review.receipt` op, attributed to the reviewer.
  See [review receipts](docs/review-receipts.md).

### Changed

- **A lease request asks only its holder.** The holder gets a question,
  and the requester's `reports_to` gets a separate notice in the same
  thread. `lease request --json` adds optional `requests[].copied`.
- **`affected` doesn't force a full run** for a change confined to
  `.mdkg/config.json`'s `affected` rules, or for a version-only change to
  a `Cargo.toml` or `Cargo.lock`. See
  [affected](docs/affected.md#narrowed-global-files).
- **The shipped `mdkg-workflow` skill** covers ref mode (node bodies
  through `node edit` and `node save`), launcher turns (the mail in the
  prompt, the final reply and `NO_REPLY`) and review mail, and runs mdkg
  as `$MDKG_BIN`.

### Fixed

- **Idle supervisors no longer start a CLI child every second,** plus its
  git reads. Enough idle agents on one Mac could crash it. An idle
  `agents up` now keeps one mail wait open for its timeout (600 seconds
  by default), skips git while refs and config are unchanged, and reads
  definitions in full at most every 30 seconds. A missing sign-in or
  repeated turn failures back off 30, 60, 120, 240, then 300 seconds.
  Interactive receivers wait the same way.
- **A turn no longer starts from an old agent definition** when mail
  arrives just after `agents edit`. Before each turn, the supervisor
  checks the committed definition. On a change, it lets running turns
  finish and restarts, and the mail starts its turn on the new
  definition, including a new harness.
- **`init --setup local` checks the git identity before writing
  anything.** With no `user.name` or `user.email` it refuses with
  `graph.identity_unset` and leaves nothing behind.

### Not in 0.7.4

- The projections default is unchanged. A config with no `projections`
  key still gets the legacy list from `install`. The change to none is
  planned for 0.7.5.

## 0.7.3

### Upgrading from 0.7.2

- **The coordinator store moves from schema 4 to 5.** The first 0.7.3
  command that opens a repository's store upgrades it, after copying it
  beside it. After that, 0.7.2 and older builds refuse the store, so
  replace every mdkg binary that uses the repository first. `store
  restore <copy>` brings the copy back.
- **`hooks:git` adds `post-commit`.** An existing install gets it at its
  next `mdkg install`.

### New

- **Path leases can be scoped by branch (opt-in).** Set `git config
  mdkg.leaseScope branch` for one clone, or `"scope": "branch"` under
  `coordination.leases` in the committed config. Leases on two different
  branches then never block each other. The second claim warns
  `coord.lease_other_branch`, and `status` shows each lease's branch.
  `repository` stays the default. Lease records gain `branch`, which is
  null when the lease holds on every branch. See
  [concepts](docs/concepts.md).
- **A commit gives back the path leases it settled.** With `hooks:git`,
  git's `post-commit` runs the new `lease.committed` rule, which
  releases leases as `release --committed` does. It never stops a commit.
- **`msg thread --json` gives each body as one `text` string,** so a
  reader no longer parses the boundary lines. It's absent once the body
  is purged. The message API schema declares it.

### Changed

- **A turn refused for a missing login is recorded and reported.** It
  fails with `run.login_required`, and `agents status` shows it as the
  last turn, `failed`. The agent's `reports_to` gets a fault that names
  the harness and its login command. The mail stays unacked, so the first
  turn after the login takes it.
- **Only a turn this process descends from refuses a person's `msg
  list`, as for `msg send`.** A turn that another process launched no
  longer refuses it, even in the same checkout. The live-turn guard now
  refuses both commands with its own code, `run.turn_active`, instead of
  `api.actor_not_allowed`.
- **A turn's mail comes from its `reports_to` first.** The launcher
  delivers that sender's mail first. Each other sender follows, ordered by
  its newest message, and each sender's mail comes newest first. It used
  to come oldest first. Digest delivery puts `reports_to`'s groups first.
- **In ref mode, worktrees keep `.worktrees/` out of git through
  `.git/info/exclude`,** not the tracked `.gitignore`. Adding a worker
  or running `install --view` changes no tracked file. Checkout mode is
  unchanged.

### Fixed

- A turn lease whose end-of-turn release failed (`coord.store_failed`)
  no longer blocks the agent's turns until it expires. The supervisor
  gives it back between turns, and the next turn releases it first. Until
  then, `agents status` shows `turn_lease_unreleased` and warns.
- Path leases that the end-of-turn `release --clean` couldn't release on
  a busy store are retried between turns and at the next turn's end.
- A turn lease whose process has exited is reclaimed after a reboot.
  Renaming the machine no longer keeps it held, unless the machine also
  restarted since. The lease's record now carries the boot's id.

### Internal

- `affected` selects the tests that read the repository's own graph for
  changes under `.mdkg/`. check.sh's affected mode allows git's local
  `file` transport. Load-sensitive test binaries get more room, and the
  agents tests leave no supervisor running.

### Not in 0.7.3

- The projections default is unchanged. A config with no `projections`
  key still gets the legacy list from `install`. The change to none is
  planned for 0.7.4.
- Writes to authority fields from inside an agent's turn aren't refused
  yet. That's planned for 0.7.4.

## 0.7.2

### Security

- **`config show` hides keys that look like credentials.** A key that
  itself matches the credential rules is shown as `[redacted key <n>]`,
  with everything under it hidden, in both formats and in `config set`'s
  `old`, `new` and diff. `origins` and `redacted` name the renamed
  pointers. docs/format-0.7.md states the guarantee and its limits.
- **Harness turns start with an allowlisted environment.** A turn, its
  login check and OpenCode's worker preflight no longer inherit the
  launcher's whole environment: only what a harness needs (`PATH`, `HOME`,
  `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `TERM`, `LANG`, `LC_*`, `TZ`, the
  XDG folders, git identity and config paths), mdkg's own `MDKG_*`, each
  harness's own folders (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `OPENCODE_*`,
  never their token variables) and the names a new policy field,
  `env.allow`, adds. A supervisor started from a shell no longer hands
  that shell's credentials to agents. Opt out per agent or for all with
  `"env": {"inherit": true}`, which warns on every turn; `agents status`
  (`env_mode`) and `doctor`'s `env` check name inherited-mode agents and
  agents a supervisor from 0.7.0 or 0.7.1 runs. A shell that rereads your
  startup files still gets what they export. See
  [the turn's environment](docs/agents.md#the-turns-environment).
- **OpenCode turns no longer read Claude Code's files.** Every OpenCode
  turn mdkg launches sets `OPENCODE_DISABLE_CLAUDE_CODE=1` and
  `OPENCODE_DISABLE_CLAUDE_CODE_SKILLS=1`, so it no longer finds
  `CLAUDE.md`, `~/.claude/CLAUDE.md` or `.claude/skills` by itself. A file
  your own OpenCode config lists in `instructions` still loads. A harness
  finds only its own memory and instruction files by itself. Claude Code and Codex
  need no switch. See
  [harness memory](docs/agents.md#harness-memory).

### New

- **Digest delivery.** Agents can opt in with `--delivery digest`; batch
  remains the default. Digest turns group unacknowledged mail by node or
  conversation, putting faults first and otherwise showing newest first
  within each group. Turns reply to the configured coordinator. `msg digest`
  computes a view without marking mail shown. `version --json` advertises
  `agents.delivery.digest`.
- **A person sends mail as themselves.** `msg send` runs as `human:<name>`
  (the exact lowercase form, by `--as` or `$MDKG_ACTOR`), of any kind but
  `decision`, `receipt` and `fault`, from the CLI and serve's local socket,
  only outside agent turns and only while no person key is enforced in the
  coordinator (`person.proof_required` once one is). It's unsigned, and its
  log record carries `person_proof: null`. Loopback and MCP refuse it. Until
  enforcement is on, any process run outside a turn can send as the person,
  including one an agent detaches from its turn. See
  [a person's own mail](docs/delegated-decisions.md#a-persons-own-mail).
- **`init --setup <personal|professional|local>`.** A repository with no
  graph can start one on `refs/heads/mdkg-graph` in ref mode in one step,
  with no commit on any branch and the checkout unchanged. It reuses
  `install --setup`'s setup rules and keys (`--graph-remote` for
  professional), and refuses before writing anything when the checkout
  already has a graph, another worktree of the clone has one, the graph
  ref exists, or the clone is in ref mode.
  A failure before the graph ref exists puts the keys back. `init --json`
  adds `data.setup`.
- **`install` and `doctor` warn about the legacy projections default.**
  A `.mdkg/config.json` with no `projections` key gets the legacy default
  from `install`: the AGENTS.md block, every harness's MCP config and the
  skill mirrors. In 0.7.3 a config without the key gets none of them. Both
  commands now warn (`install.legacy_projections`) and name the key: `mdkg
  config set projections '<today's list>'`, then `install`, keeps them.
  In ref mode, an `install --enable` or `--disable` that leaves the list as
  it was now publishes the key too, as checkout mode already wrote it.

- **Graph sync:** in ref mode, the graph ref syncs with `mdkg.graph.remote`.
  `mdkg graph remote approve`, run by a person outside any agent turn, pins
  the remote's destination and the git setup a push runs with (ssh
  command, hooks, credential helpers, as digests, from the system,
  global, local and worktree config; not `-c` or `GIT_CONFIG_*` settings);
  nothing is pulled or pushed before that, or after either changes. `mdkg graph sync` pulls and
  pushes by hand, and `agents up` pulls before its first turn and every 15
  minutes, and pushes 2 minutes after the last local change. Only
  `remoteRef` crosses; pushes refuse code destinations, non-graph history
  on a private remote, and anything the secret scan flags. A conflicting
  path takes the remote's version and is listed by `mdkg graph conflicts`;
  `mdkg graph resolve` settles it (`--merge` names which, when a path has
  several). `--accept-rewrite` adopts a rewritten
  remote, keeping the local graph under a backup ref. See
  [syncing with graph.remote](docs/git-graph.md#syncing-with-graphremote).
- **Secret scan:** some `password`, `passwd` and `token` assignments are
  refused as credentials, in mail and in graph pushes: those whose value
  is at least 20 characters with both a letter and a digit (placeholders,
  expressions and UUIDs excepted). A letters-only value, a quoted value of
  several words and some structured tokens aren't caught: the scan is a
  backstop. The same rule applies in git's secret hooks on code commits
  and pushes, so a code commit holding such an assignment is now refused
  too.
- **`agents list`** lists the agents the settings define, each with the
  layer that defines it, whether it has its own file, every layer whose
  settings have an entry for it, and its harness and role. It reads settings
  only, redacted as `config show` redacts them.
- **`config set` and `config unset`** change one key of the committed
  project config, in either mode: in ref mode a publish on the graph ref
  through mdkg's graph writer (pending intent, compare-and-swap, replay),
  in checkout mode a commit where the committed config is read. They
  refuse unknown keys, the identity keys, `work.location` (which
  `work localize` changes) and any value that `validate` and `doctor`
  would reject, writing nothing. `--dry-run` shows the
  diff; `--json` gives the old and new values, the commit and the graph
  ref. This replaces the hand-written git recipe in
  [changing the committed config](docs/git-graph.md#changing-the-committed-config),
  which stays only for repairing a config mdkg can't read.
- **A read-only mail list across mailboxes:** `msg list --json` lists mail
  of the kinds you name (`--kind fault,review_request`) delivered to any
  mailbox, to `--to` mailboxes, or to a `--channel`'s current members,
  with `--from`, `--state unacked|acked|any`, `--after` and `--limit`
  (at most 200, and under 1 MiB of bodies a page). It opens the store
  read-only: nothing is acked, marked shown or purged, and a body whose
  retention is up reads as purged. Only a person runs it
  (`--as human:<name>`), outside an agent's turn; it has no MCP tool and
  isn't served. Mail isn't confidential between local actors:
  `msg watch` already shows every message.
- **Tool-call ids in turn output:** `tool.started` and `tool.ended` carry
  `call_id`, the harness's own id for the call (Claude and OpenCode), so
  overlapping calls of one tool pair up. Null when the harness gives none;
  Codex turns report no tools. See `mdkg.api.turn_output.v1`.
- **`agents turns <agent> --json`:** an agent's turns, newest first, with
  each one's `turn_id`, `number`, `started`, `ended`, `state`, `outcome`
  and receipt path; `--limit`, `--before` and `--turn <N>`
  (`mdkg.agents.turns.v1`). Each `agents logs --json` line carries its
  `position`, which `--after` resumes from.
- **Graph head events:** `msg watch --json` and `serve`'s `/v1/events`
  send `mdkg.graph.head_changed` (`{graph, head}`, ephemeral) when they
  start, naming the graph ref's tip (in checkout mode the worktree's
  `HEAD`), and each time it changes; `head` is null while it names no
  commit. The stream reads it about once a second. `status --json` reports the same
  head in checkout mode too: `graph` is `{ref: null, tip: <HEAD>}` (it
  had no `graph` there before). See
  [the API guide](docs/api.md#graph-head-events).
- **`doctor --only store`:** runs the `store` check alone: the coordinator
  store's schema and mail schema versions (`store_schema`, `mail_schema`)
  and whether this build would upgrade it (`upgrade_needed`), read-only,
  without upgrading the store or reading any config, projection or harness
  file. It reads the store of the git repository it runs in. A store newer
  than this build is a warning with its own versions, not an error, in
  full `doctor` too (`--strict` still fails on it).
- **`agents status`'s `last_exit`:** how an agent's last `agents up` ended
  (`cap`, `once`, `manual`, `mode_changed`, `drained`, `signal`, or
  `failed` with its `code`) stays readable after the process is gone, per
  agent, for as long as its watch is the agent's latest. It's kept
  in the run folder (no store change), absent while a supervisor runs the
  agent, and absent when the supervisor was killed outright, so the reason
  is never guessed. See
  [the API guide](docs/api.md).
- **`agents status --no-probe`:** reports the agents, their effective
  definitions, which are file-backed (the new `file_backed`), their
  supervisors and progress, without running any harness login check,
  Keychain listing or billing read, so a client can poll it. It leaves
  `auth` and `billing` out. See [the API guide](docs/api.md).
- **A stable store id:** `status --json` and `config show --json` carry
  `store_id`, the coordinator store's own id (as `anchor` names it): the
  same from every worktree and for the store's life; null before the store
  exists.
- **A person's client acks the mail it showed:** a client that reads mail
  itself (`msg list`, or `msg inbox --no-mark`) records what it put in
  front of the person with `msg inbox --mark-shown <seq>/<digest>/<id>`,
  then acks by id. Acking by id still refuses mail never marked shown
  (`msg.not_shown`, whose hint now names the mark). Only that person
  marks their mail: an agent's read of a person's mailbox no longer
  counts as shown, and its `--mark-shown` there is refused
  (`msg.not_holder`). See
  [the API guide](docs/api.md#mail-across-mailboxes-072).

### Changed

- **`skills add` commits in a git checkout.** In checkout mode it now
  writes the skill under an exact lease on its `SKILL.md` and commits it on
  the checkout's branch, as `agents add` commits a definition. A refresh
  inside the managed `.worktrees/graph` lands on the graph branch instead
  of leaving that checkout dirty, which froze graph writers and install
  there. Outside git, or before a branch's first commit, it writes in
  place as before.
- **Ref-mode skills.** `skills add` publishes a shipped skill on the graph
  ref (0.7.1 refused it), and `install` mirrors it from there. The new
  `skills add --replace` takes the shipped copy over one that differs, in
  either mode. `doctor`'s `agents` check judges the graph ref's skill in
  ref mode, never a checkout copy, and its hint names that refresh.
- **Rolling back from a custom graph ref.** `install --checkout-mode` on a
  clone whose graph ref isn't `refs/heads/mdkg-graph` now creates
  `mdkg-graph` at its tip, retargets the records naming the old ref, and
  goes on as for the default ref, so install's own path makes and
  registers the graph checkout. If a record it keeps leaves the checkout
  unregistered, checkout-mode reads refuse with an identity diagnostic and
  never read it unguarded. 0.7.1 refused this (its Known limits). It
  refuses before changing anything while `mdkg-graph` holds another commit
  or is checked out in another worktree, and a rerun finishes a move that
  stopped part way. See [git-graph](docs/git-graph.md).
- **Ref mode keeps the repository-root graph at `.mdkg`.** This is now a
  rule, not a limit that a later version lifts: no mdkg writer records a
  root `mdkg.graph.<id>.dir`, and a hand-set one is refused with a hint
  that gives the `git config --unset` command for each key.

### Faster

- **Fewer git processes per command:** a command keeps some of what it
  reads from git: repository config values (unless the config includes
  another file), a ref found to name a commit (a missing one is asked
  again each time), the graph ref's tip, and parsed config snapshots. A
  kept read is read again when a file it was read from changes, at the
  next turn or request, or before mdkg writes. Measured on a graph with 50 agents (git processes
  per command, before and after): `status` 19 and 14, `msg send` 12 and 11,
  `agents status` 20 and 16, `claim` 21 and 19; with a graph ref, `msg
  send` 44 and 31.
- **Fault bookkeeping no longer decodes every stored fault:** on a store
  holding 2,000 faults from 50 agents, the previously stored faults a
  fault send looks up and decodes fell from 200 to 1, a resend's from
  2,202 to 2, and a clear's from 2,004 to 6. SQLite still scans the
  stored fault bodies to find them.

### Changed from 0.7.1

- **`no_reply` outcome (contract change):** a turn answering mail whose
  final message is exactly `NO_REPLY` now ends with outcome `no_reply`, not
  `replied`, in `agents up --json`, the turn index, the journal's
  `turn.ended` and `agents status`'s `last_turn`. Its report no longer
  carries `reply: "NO_REPLY"`. For turns run from 0.7.2 on, `replied`
  always means an imported reply with a `reply_id`; turns recorded earlier
  keep their `replied`. A task-only turn still ends `ran`. Clients that
  treated `replied` as "the turn answered" should accept both.
- **Fewer duplicate replies:** when a turn sends a `handoff` or `findings`
  answering its newest shown message, its final message is not also imported
  as a reply. Questions and notes do not count as answers. See the release
  notes for limits and [the API guide](docs/api.md).

- **Finer `affected` selection for Rust tests:** a changed `.rs` file that
  only integration tests reach, such as a new `tests/<name>.rs` or a
  shared helper under `tests/common/`, now selects just the test targets
  whose module trees reach it, read from the head's `mod`, `#[path]` and
  `include!` lines, instead of the whole crate and its dependents. What
  can't be derived soundly still falls back to the whole crate, and the
  warning says why. A test that reads `.rs` files at run time still needs
  a rule. See [affected](docs/affected.md).
- **`affected --targets` and older heads:** a rule naming a Rust test
  target that the planned head doesn't have (an older head than the shared
  config) no longer fails with `config.invalid`. The target is skipped with
  a warning and the rule's other targets still run; with none left, its
  packages run whole. A package the head lacks still fails closed.
- **Exit codes (contract):** `entry gate` exits 2 when the gate fails,
  with the failures in its data and an `entry.gate_failed` error, like the
  other checks; it exited 0. Its `mdkg serve` read answers 422 instead of
  200 to match. A ref-mode write the graph moved under
  (`graph.write_stale`, `graph.write_contended`, such as a stale
  `node save`) exits 5, a conflict to re-read and retry, instead of 4.
- **Validation:** `req.missing` warns for a `feat` with no requirement
  items too, as for a task, bug or spike. A duplicate requirement id's
  message names the first use by its file line, like the diagnostic's own
  line.
- **Doctor:** `graph_worktree` warns when a checkout-mode graph checkout
  can no longer follow main by fast-forward although main already has its
  work, as a squash or rebase merge of the graph branch leaves it. `graph.shape`
  warns, instead of failing, when a personal graph's full shape was found
  but the tree passed a walk limit before the rest was checked. Check
  names line up in one column however long they are.
- **Store creation:** on a file system that refuses the full sync barrier
  (an SMB mount), creating the coordinator store is refused with the
  `MDKG_STORE_PLAIN_FSYNC=1` override named; with it, the store is created
  with a plain fsync and the command warns `coord.store_weak_sync`.

### Fixed

- **Usage limits in their current wording:** Claude's "You've hit your
  weekly limit" and "<n>-hour limit" (the text of its error result, or
  an error result after a rejected `rate_limit_event`) and OpenCode's
  `provider.quota` errors and 429 usage-limit bodies, in both of their forms, are now read as a usage
  limit, so the turn ends with that reason (Claude's hint also offers
  `agents use-key`, and names the limit and when it resets). A Claude 429
  that names no limit is provider throttling: like capacity, it never
  sets mail aside, and its hint offers no key.

- **Signals reach git:** a SIGTERM or SIGINT to a one-shot command is
  forwarded to the git processes it started. mdkg waits up to 2 s, kills
  what's left, and exits by the same signal within 3 s, so none of its
  git writes lands after mdkg has exited. A running agent turn and
  `git-serve`'s `git receive-pack` aren't covered and can still write
  after it (see the release notes' Known limits). See
  [the API guide](docs/api.md).
- `doctor` no longer shows an MCP server command it reads from
  `.mcp.json`, `opencode.json` or `.codex/config.toml` when the command
  looks like a credential (an inline token, a URL with a password) or
  holds whitespace (a command line with arguments, which may carry a
  token in no known shape): it shows `<redacted>` in text and JSON, and
  still checks the real command.
- `msg watch` and `msg wait` in ref mode no longer hold the graph lock for
  as long as they stream or wait: graph writes beside them went on
  waiting, and a `status` after the graph ref moved failed with
  `graph.lock_required`.
- `.git/mdkg` is created 0700 when the search index or `work localize`
  makes it first: `search` or `pack --query` in a fresh checkout-mode
  clone made it with the umask's mode.
- A delegated decision whose grant the machine-local file revokes says so,
  rather than "has no delegation". A decision sent from a plain shell no
  longer fails now and then with "cannot inspect launcher turn" when a
  turn ends during the check.
- `holds` names the scope when it isn't held (exit 0, as before).
- A stale `node save` from a copy says to move the edits out first, since
  `node edit --discard` replaces the copy. `new` labels a local work path.
- `anchor` in ref mode publishes past a stale earlier anchor at the same
  record, instead of refusing every rerun.
- The credential filter no longer refuses a message that quotes a task
  id's tail on its own; identifiers such as `my-sk-…` pass, while a
  recognised key shape standing on its own is still refused.
- `install`'s refresh check counts a `.gitignore` that changes between a
  file and a symlink as an ignore change.

- **The Linux PyPI wheels don't include FIDO2:** pip wheels on Linux
  build without FIDO2 person keys (the musl ones never had them), so no
  wheel needs libudev or bundles a system library (the gnu wheels still
  link glibc, as manylinux allows), and the PyPI build fails if a wheel
  bundles one. Their `--version` says there's no FIDO2. Person keys
  aren't usable from the CLI yet; once they are, FIDO2 keys on Linux need
  a gnu release archive, Homebrew or `cargo install mdkg`. The other
  install methods are unchanged.

### Internal

- The agent turn is split into the harness seam's kernel operations
  (`admit`, `starting`, `spawned`, `output`, `import`, `end`) in
  `mdkg-run`'s new `seam` module, and the turn's harness-specific choices
  move behind the harness adapter. No change in behaviour.

## 0.7.1

### New

- **A local API:** `mdkg serve` exposes CLI JSON operations on a private
  Unix socket, with resumable server-sent events. Optional loopback HTTP
  uses a token file; it refuses browser requests and non-loopback hosts.
  Operator, host and delegated-decision operations are not served. See
  [serve](docs/serve.md).
- **Events and requirements:** `msg watch --json` follows local coordinator
  events with resumable cursors. `req list --json` and `pack --json` expose
  requirement statements, verification text, relations and ledger state;
  `validate` adds advisory requirement warnings.
- **An optional graph ref:** clone-local `mdkg.graph.ref` selects committed
  graph authority independent of the code branch or a main checkout.
  Committed/shared node and goal writes and their settlement use that ref,
  as do agent definitions and shared-node `node edit` / `node save`.
  `install --setup local|personal|professional`
  explicitly moves an existing checkout graph; plain install preserves
  checkout mode, which remains the default. See [setup and migration
  limits](docs/git-graph.md) before moving a graph.
- **Recovery and diagnostics:** durable supervisor faults, orphaned-launcher
  lease recovery, bounded startup retries and `release --clean` help recover
  stalled lanes. Long-running 0.7.1 supervisors detect a replaced binary or
  change to an originally selected agent's committed definition or config,
  drain active turns and re-execute with watch progress intact on Claude,
  Codex and OpenCode. In ref mode the change must reach the graph ref;
  restart a supervisor to include newly added agents.
  `status` reads through a read-only coordinator connection,
  releases it before Git and makes a separate zero-wait observation write.
  `affected` skips unchanged changes brought in from main.

### Changed from 0.7.0

- **Mail retention:** an acknowledged message body remains readable for
  seven days by default. Set `coordination.messages.ack_grace_days` to `0`
  for the old purge-at-ack behavior; accepted values are whole days from
  0 to 3650. Invalid values refuse sends and acknowledgements.
- **Delegation:** grants are accepted only from the user settings file.
  Machine-local grants are ignored; machine-local revocations still apply.
- **Billing:** mdkg no longer automatically retries on an API key after a
  subscription limit, login failure or an in-turn limit. A person must use
  `agents use-key <agent>` for one turn, with `auth.api_key_fallback`
  enabled. OpenCode continues to use its own configured authentication.
- **Channels and replies:** named groups use `channel` / `--channel`;
  deprecated aliases remain through 0.7.1. JSON adds `channel` beside the
  compatibility field. Message ids and explicit contexts starting with
  `channel:` are refused. Channel notes no longer wake headless lanes;
  other channel posts use the interactive wake-kind policy. A channel reply
  goes directly to the sender, with one sender answered per turn.
- **Receipts and faults:** receipt mail has kind `receipt`, is hidden from
  default inbox/wait results and `status`'s unacked count, and never wakes
  an agent. `msg ack --through` skips faults unless `--kind fault` is
  explicit. Message `kind` is now an open string in the v1 schemas, so old
  strict schema copies reject the new `receipt` and `fault` kinds.
- **Turns:** an exact final `NO_REPLY` sends nothing and acknowledges shown
  mail on Claude, Codex and OpenCode. After three failed shows, the next
  turn sets the message aside and reports a fault; capacity, usage-limit,
  timeout and coordinator failures do not count. Configured protected
  branches now govern worker launch; turn options and `MDKG_MAIN_REF`
  cannot override that protection.
- **Migration:** `migrate` keeps committed 0.6 work history in Git by default;
  `--work-history store` opts into the store and `--restore-history` brings
  it back. Migrating a 0.6 branch against an already-migrated main is refused;
  merge main and redo branch-only nodes with `mdkg new`. `--against` is gone.
- **Harnesses and safety:** OpenCode workers require exactly 2.1.0 or 2.2.5;
  the default plugin guards 2.2's `shell` tool too. Claude and OpenCode
  workers receive pattern guardrails for common direct invocations of
  credential-printing harness commands. Wrappers and direct store reads can
  bypass them; they provide no credential isolation. Mail filtering and
  config redaction catch credentials after escaped separators.
  Plain install adds no new hooks; explicitly requested hooks and previously
  installed hooks are synchronized.

### Fixed by the 0.7.1 end-to-end run

- **Ref mode:** `mdkg serve` and `serve status` work in ref mode, from
  linked worktrees too. `status` and `agents status` work with or without
  a `.worktrees/graph` view and after the move from checkout mode.
  `install --checkout-mode` rolls back after the view is gone, registering
  a new graph checkout; when a leftover record prevents that, reads refuse
  rather than fall back to a stale HEAD. A clone that has fetched a graph
  is pointed to `mdkg install` (`--setup personal` for
  `origin/mdkg-graph`), and `init` refuses to start a second graph there
  (`--new-graph` overrides).
  Ref-mode `init` commits a README that describes ref mode and excludes
  `.mdkg-edit/`. `node edit` and `node save` refuse a local work node
  before taking a lease, and release only a lease they took.
- **Committed config authority:** mail settings and the lease policy come
  from the committed config in either mode, never from an uncommitted
  edit. `config show` names the graph and commit they came from, and
  `doctor` and `validate` report a committed value that would refuse every
  send and ack.
- **serve's child processes:** a long poll's or event stream's process
  stops when its client disconnects, and before serve exits; a SIGTERM
  answers open long polls `api.draining`. A `:wait` timeout over 600
  seconds is refused instead of cut short.
- **Reboots:** saved supervisor progress survives a reboot or remount.
  A zero-byte store reads as no store; newer and foreign stores are refused
  before any schema change; lock waits are bounded.
- **Text output and hints:** hints print real line breaks and name the
  program as invoked; `entry show`, `entry gate`, `agents status`, `show`
  and `next` print fields, not JSON; `validate --changed-only` prints
  `graph.no_code_base` in text too; an unknown agent exits 3; a `NO_REPLY`
  turn's summary says so; `install --dry-run` says what it would change.
- **Safety and setup:** the credential filter reads separators under
  several layers of URL encoding and escapes. Graph commits take the
  identity in `GIT_CONFIG_GLOBAL`. New stores are 0600, and project files
  `init` writes keep the umask's mode. `doctor --strict` passes after
  `install --setup personal`, unless a kept view is behind the tip (see
  Known limits).

### Upgrading from 0.7.0

- Restart supervisors still running 0.7.0 after replacing the binary once;
  they cannot adopt 0.7.1 automatically. Supervisors already on 0.7.1 drain
  and re-execute when their binary is replaced or an originally selected
  agent's committed definition or config changes in the graph authority.
  Restart to include newly added agents. Restart after user/machine-local policy edits when no
  such reload is triggered; interactive receiver policy refresh is deferred.
- Move machine-local delegation grants to `$XDG_CONFIG_HOME/mdkg/config.json`
  (normally `~/.config/mdkg/config.json`). Review the retention and billing
  changes above. Refresh strict validators from the 0.7.1 `schemas/` copies.
- Refresh `mdkg-workflow` in the **ordinary checkout, in checkout mode**.
  Commit the old skill and your project additions first, so Git preserves
  them. Run `mdkg doctor` and follow its
  hint: remove `.mdkg/skills/mdkg-workflow`, then run
  `mdkg skills add mdkg-workflow`. Use `git diff` against that commit to
  reapply your project additions, run `mdkg install` to synchronize mirrors,
  and commit the refreshed skill. Finish the refresh there; immediate
  graph-ref migration has the known limit below.
- If you use or want the OpenCode plugin, review
  `mdkg install --enable hooks:opencode --dry-run` before applying it.
  Preserve edits made only to `.opencode/plugins/mdkg/server.ts`: sync
  overwrites that projection from `.mdkg/hooks/opencode/mdkg/server.ts`.
  Customized canonical plugins are preserved and doctor warns when their
  shell routing is unverified. Ordinary 0.7 graphs need no format migration. Coordinator/mail
  upgrades copy the old store first; do not reopen an upgraded store with
  an older binary.

### Known limits

- Local work files stay beside each clone's coordinator; leases, mail and
  settlement history stay inside its database. None are on the graph ref.
  Keep one clone writing a shared graph ref; `graph.remote` does not sync it. Ref-mode
  migration has preservation checks and documented residuals.
- Ref mode reads the repository-root graph from `.mdkg` only. A root graph
  registered in another folder (`mdkg.graph.<id>.dir`) is refused by
  `install --setup`, doctor, `config show` and every command, until 0.7.2;
  keep such a repository in checkout mode.
- `install --checkout-mode` refuses a graph ref other than
  `refs/heads/mdkg-graph`, changing no mode or registration. Move the
  graph there first: `git branch -m -- <name> mdkg-graph` for a branch (as
  the hint says), or `git update-ref refs/heads/mdkg-graph '<ref>^{commit}'
  ''` for a ref outside `refs/heads/` (the empty old value makes it refuse
  if `mdkg-graph` already exists: then inspect that branch and keep or
  merge its commits before you replace it). Then set `mdkg.graph.ref`, and each `mdkg.graph.<id>.ref`
  naming the old ref, to `refs/heads/mdkg-graph`, and run it again.
- There is no `mdkg config set` until 0.7.2; in ref mode, change the
  committed config with the git recipe in [git-graph](docs/git-graph.md#changing-the-committed-config).
- The credential filter is a guard rail: a password hidden by
  deliberately mixed URL encodings can get through.
- After `install --setup` moves a clone that kept a `.worktrees/graph`
  view, each graph write leaves `doctor` warning `graph.view: the view is
  behind the tip`, and `doctor --strict` fails. Refresh it with `mdkg
  install --view`, which keeps ignored drafts (doctor's own hint, a plain
  `git switch`, can overwrite one). If it refuses because a draft sits
  where the tip now has a file, move the draft aside and rerun; to remove
  the view instead, preserve its drafts first.
- `init` looks for an already fetched graph only on remote-tracking refs,
  at the repository top or the selected root. A graph on a local branch,
  or in another folder, isn't seen, and `init` starts a second one: check
  `git branch -a` first in a clone that might have one.
- In checkout mode, a squash or rebase merge of `mdkg-graph` into `main`
  leaves `install` reporting `graph not synced: diverged`, and `doctor`
  doesn't warn. Merge the graph branch with a merge commit.
- When `node save` is refused as stale (`graph.write_stale`), its hint
  suggests `node edit --discard`, which overwrites your copy: save your
  edits elsewhere first.
- With a lease authority configured, the node lease `node edit` takes
  is local. A save that publishes a change ends it; one that changes
  nothing or is refused leaves it held, and then `release <node>` asks the
  authority and `release <lease-id>` refuses it, so it lapses after two
  hours. `release --all` frees it too, but also every other local lease
  you hold, nodes and roles included: use it only once your other work is
  saved.
- `entry gate` exits 0 when the gate fails, since the query itself worked;
  check `data.ok` in `--json` (or the text), not the exit code.
- A command that builds the search index first (`search`, `pack --query`)
  in a fresh checkout-mode clone creates `.git/mdkg` with the umask's mode
  (0755 under 022) instead of 0700; `chmod 700 "$(git rev-parse
  --git-common-dir)/mdkg"` fixes it. The store files are 0600 either way.
- Ref-mode doctor does not yet compare the graph ref's workflow skill with
  this build's, and refreshing that skill in ref mode is not supported.
  Refresh in checkout mode first. A subsequent graph-ref migration can
  require graph-branch or view reconciliation (`graph.diverged` or
  `graph.view`); the immediate migration procedure and ref-mode skill
  refresh are deferred to 0.7.2.
- Agents run as the same OS user; mdkg adds no sandbox. The serve turn guard
  and `use-key` guard are advisory against detached same-user processes.
  Never forward the local API socket.
- Receipt summaries are bounded to 8 KiB of escaped JSON; other evidence can
  still exceed ordinary mail limits. Full local receipts are limited to
  4 MiB; rendering above that cap fails before a local receipt is saved.
  See [agent operation](docs/agents.md) for behavior details.
- Linux musl builds (the static binaries, and musllinux wheels) have no
  FIDO2 person keys; use a gnu (glibc) build, Homebrew or the macOS build
  for them.

### Not in 0.7.1

- Automatically downloading/installing binary updates; `config set`;
  policy refresh for running interactive receivers; broader bounded receipt
  previews.
- Native Windows builds and the PowerShell installer; macOS notarization.
- `mdkg viz`, derived semantic/CocoIndex search.
- Networked `serve`: TLS, signed requests, machine enrolment, person keys
  and remote clients (deferred to 0.7.2; code in the tree is not wired in).
- Graph-remote sync, remote approval, `graph publish-design`, automatic
  following of the code base and worker dependency selection (0.7.2).

Ref-based graph writes, the `mdkg-git` crate and supervisor re-execution,
planned in 0.7.0, **do ship** in 0.7.1.

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
  - Supervisor progress survives restarts. An interrupted turn is not
    replayed automatically: `agents status` and `doctor` report it, and
    `agents resume <name>` records explicit recovery before work continues.
    Recovery context can be archived with `--archive-context`.
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
- **Turn-end receipts:** the launcher retains a local receipt and sends
  advisory mail from `mdkg:receipts` to the agent's `reports_to` actor.
  Receipts report the outcome, observed commits, claims and completion
  evidence, reply and acknowledgement status, and final summary. They
  report observations, not commit authorship or approval. Delivery failures
  are reported without changing the turn outcome or rerunning the agent.
  The recipient must be a different, non-reserved actor.
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
- **Runtime settings:** `agents up` applies layered orchestration settings
  and user or machine-local overrides to agents with definition files in
  `.mdkg/agents/`. Optional settings cannot create agents; optional
  `agents.defaults` values are ignored, while project defaults still apply.
  Projections stay project-only. An active watch keeps its startup settings;
  restart it to apply changes. `config show` identifies these limits.
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

- Automatic binary replacement and re-execution for running supervisors.
- `config set` and policy refresh for running interactive receivers.
- Broader bounded previews for receipt mail.
- Native Windows builds.
- `mdkg viz` and derived/CocoIndex search.
- Ref-based graph writes and the `mdkg-git` crate.

### Known limits

- After upgrading mdkg, restart running `mdkg agents up` supervisors so
  subsequent turns use the new build.
- **A hard link between the two skill mirrors:** when a file in one mirror
  is a hard link to its copy in the other, `install --disable` of either
  mirror is refused while the other stays enabled: install can't tell the
  two names apart, so it won't remove one. Disable both together, or
  replace the link with a copy and then disable the mirror.
