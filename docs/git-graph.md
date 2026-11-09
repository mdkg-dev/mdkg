# Git and the graph ref

mdkg reads its authority — the committed `.mdkg/config.json`, agent
definitions, designs and shared work — from git objects at one commit. In
0.7.0 that commit was the head of a checkout: the graph was committed on a
branch, usually `main`. Since 0.7.1 the graph can live on a **graph ref** of
its own, so a repository doesn't need a code branch, or `main`, to carry it.

This page describes what 0.7.1 does, plus graph sync (`mdkg graph remote
approve`, `graph sync`, `graph conflicts`, `graph resolve`), new in 0.7.2.
Other commands that arrive later are marked where they'd change the
picture; they aren't available yet.

## The two modes

| | Checkout mode (default) | Ref mode |
|---|---|---|
| `mdkg.graph.ref` in the clone's common git config | unset | the graph ref's name |
| Where the graph is committed | a branch of the checkout, usually `main` | the graph ref, normally `refs/heads/mdkg-graph` |
| What mdkg reads as authority | the graph checkout's `HEAD`; with none registered, the invoking checkout's `HEAD` only in a clone that never used a graph ref | the graph ref's tip |
| What carries the code | the same branch | `$MDKG_MAIN_REF` when set, else `mdkg.mainRef`; mdkg only reads it |
| What work settles on | the main branch (`mdkg.mainRef`) | the graph ref |
| Where a write goes | the checkout's files and a commit on its branch | a commit on the graph ref |

A repository picks its mode with that one key: present, and every command
reads the graph ref; absent, and mdkg behaves as 0.7.0 did. mdkg reads the
key from the clone's common git config file only — never a worktree's own
config, the global or system files, or the environment. A short name is a
branch (`mdkg-graph` is `refs/heads/mdkg-graph`); `refs/mdkg/` is refused.
Where the code goes is the one thing taken from the environment:
`$MDKG_MAIN_REF`, when set and not empty, is the code base instead of
`mdkg.mainRef`.

### Checkout mode

Nothing has to be configured. mdkg reads the checkout `mdkg install`
registered for the graph (`mdkg.graph.<graph_id>.worktree`), or, with none,
the invoking checkout's `HEAD`, as 0.7.0 did. A clone that has used a graph
ref (an `mdkg.graph.<graph_id>.ref` record) and has no `.worktree` record
at all refuses that fallback until `mdkg install` registers a graph
checkout: `graph.checkout_missing`, or `graph.identity_mismatch` when a
`.root` record is left at the lookup root. Definitions and design come from that
commit, so an uncommitted edit never becomes authority.

The graph identity in `.mdkg/config.json` must match the clone's records, and
a graph id bound to another root is refused. A configuration that can't be
read, or an id that's present but malformed, refuses the lookup instead of
falling through to another checkout.

Work settles on the main branch and merges are branches into it. The
[concepts](concepts.md) page has the layout.

### Installing a setup

An existing checkout-mode clone stays in that mode until an explicit
`mdkg install --setup <setup>`. A plain `mdkg install` preserves it. In an
unborn repository, install defers making the managed graph checkout until
the initial graph is committed; it can also defer when the configured main
branch is absent. Ref mode needs neither that checkout nor a main branch.

Choose a setup for ref mode:

| Setup | Command | Graph remote | Defaults recorded by an explicit setup |
|---|---|---|---|
| Local | `mdkg install --setup local` | none | `mdkg.follow.fetch = off`, `mdkg.graph.publishDesign = off` |
| Personal | `mdkg install --setup personal` | existing `origin` | `mdkg.follow.fetch = on`, `mdkg.graph.publishDesign = on` |
| Professional | `mdkg install --setup professional --graph-remote mine` | existing private remote `mine` | `mdkg.follow.fetch = off`, `mdkg.graph.publishDesign = off` |

The professional remote must share no effective destination with another
remote; install creates no remote. Professional projections leave tracked
files and paths through links alone. `--publish-design` enables the design
copy preference for professional or local setups. Fetching the code base
and publishing design are preferences for later functionality, not network
operations performed by install in 0.7.1.

If `mdkg.mainRef` is unset, personal chooses a resolvable `origin/HEAD`,
then local `main` or `master`; professional chooses only `origin/HEAD`;
local chooses local `main` or `master`. With none, it stays unset. In an
already configured ref-mode clone, plain install keeps its keys and infers
the setup from the graph remote: `origin` is personal, another remote is
professional, and none is local.

Install keeps an existing graph ref, adopts an already fetched
`refs/remotes/<remote>/<graph-branch>`, or creates an orphan graph-only
commit from this checkout's committed graph. It fetches nothing. With no
graph yet it records the setup; `mdkg init` then creates the graph there.
`mdkg init --setup <personal|professional|local>` (with `--graph-remote` for
professional) does both in one step for a repository with no graph: it
checks install's setup rules and records the same keys, then commits the
graph to a new `refs/heads/mdkg-graph`, with no commit on any branch and
nothing written into the checkout. It refuses, before writing anything, a
checkout that already has a graph at its root (`install --setup` moves it)
or nested below it (which stays in checkout mode, or moves to the root
first: neither command moves a nested graph), a graph anywhere in another
worktree of the clone (the config it changes is shared: a linked worktree's
graph must leave it first, and one in the main checkout is that checkout's
to set up), a fetched branch at the setup's own graph
destination (its remote's `mdkg-graph`, or `mdkg.graph.remoteRef` when set,
even with `--new-graph`), an existing
graph ref (install adopts it), a clone already in ref mode, and a fetched
graph of this repository (`--new-graph` lifts that one only for a graph on
another remote; one on the setup's own graph remote is always install's to
adopt). If the graph's first commit
fails before the ref exists, the keys are put back and the clone stays in
checkout mode (if a key can't be put back, the error names the `git config`
command to run); a stop after the keys are written is finished by `mdkg init`
with the same `--work` and `--graph-only` options.
A professional setup refuses an existing graph ref whose tree holds code
alongside the graph. The clone's graph identity is recorded when the graph
is kept, adopted or created.

The move is judged against this checkout's own graph, including declared
workspace and subgraph folders. Changes that `git status` reports under
those graph paths refuse the move. Index flags (`assume-unchanged` or
`skip-worktree`) don't hide an edit from it: the move compares the graph
paths' contents through a fresh index, so a tracked file that differs from
its commit refuses the move too; a file a sparse checkout left absent
isn't an edit. The graph ref receives committed bytes.
Ignored untracked files also stay in the checkout and are not copied into
the graph ref, so ref-mode reads no longer see them. Before moving, inspect ignored graph
drafts too (`git status --short --ignored --untracked-files=all -- .mdkg`,
plus any other declared graph folders). Preserve them, and deliberately
commit any that should become graph authority before retrying.
An existing graph branch is adopted when one of its first 500 first-parent
commits, including the tip, holds that graph; otherwise install reports
`graph.diverged`. Open changesets refuse with `graph.open_changesets`:
settle or abandon them before retrying. 0.7.1 does not drain them. A
`--dry-run` skips the coordinator check and says so.

The `.worktrees/graph` checkout that `mdkg install` makes in checkout mode,
on the graph branch, is detached by the move and kept as a view, and the
moved graph's `mdkg.graph.<graph_id>.worktree` record is removed: ref mode
has no graph checkout. A 0.7.0 nested graph's record stays, as its identity.
The move inspects the registered graph checkout wherever it is, on the
branch or detached, in the slot or moved, and refuses while it holds
anything the graph branch doesn't: changes, untracked or ignored files, an
edit git's stat cache calls clean, or a commit off the branch. The
invoking checkout's graph folders are compared by content too. Ref mode
would never read any of it, so commit it to the graph branch or discard it
first. Records the move can't resolve refuse it: run `mdkg doctor
--repair`. A graph branch checked out anywhere but the slot must be
detached by hand, from that checkout, once `git -C <path> rev-parse
--absolute-git-dir` names its own folder under the git folder's
`worktrees/`; a checkout whose `.git` leads elsewhere needs its
registration repaired first, since a switch there moves another HEAD.
`--view` creates or
refreshes a detached `.worktrees/graph` from the graph tip; it leaves hand
edits alone. The view is read-only by convention and is never authority:
`status` and every other command work the same with it or without it, and
`mdkg migrate` refuses to run inside one: in ref mode it refuses any
linked worktree with a detached HEAD, a code checkout made with `git
worktree add --detach` included, so run it from the primary checkout or a
linked worktree on a branch.
It needs an existing graph ref. With no graph yet, run `mdkg init --setup
local` (or `install --setup local`, then `mdkg init`), then `mdkg install
--view` (choose personal or professional instead when appropriate). Asking for
`--view` before init fails after setup has already recorded the mode key.

`mdkg install --checkout-mode` unsets `mdkg.graph.ref` and keeps the graph
ref. It does not copy that ref's files into a checkout or reconcile newer
graph changes with an older checkout graph. Subsequent authority reads use
the registered graph checkout's committed `HEAD`. With none registered,
a clone that never used a graph ref (no `mdkg.graph.<id>.ref` record)
reads the invoking checkout's `HEAD`, as 0.7.0 did; one that did, with no
`.worktree` record at all, refuses (`graph.checkout_missing`) until `mdkg
install` registers one. A `.root` record at that root without its
`.worktree` refuses first with `graph.identity_mismatch`, and any
`.worktree` record, even another root's, goes to the identity checks
instead. With no checkout graph, graph
commands fail; an older checkout graph can instead become authority while
newer work remains on the preserved ref.

Before switching, inspect the graph ref and identify the checkout that will
be authority. For the default ref, `git ls-tree -r --name-only
refs/heads/mdkg-graph -- .mdkg` lists its committed graph files; when the
chosen checkout has a `HEAD`, run `git diff HEAD refs/heads/mdkg-graph --
.mdkg` there to compare them. Include any other declared graph folders, and
inspect working changes and ignored drafts as above. Preserve local work
and reconcile the wanted graph content through reviewed commits in that
graph checkout before continuing in checkout mode.

A ref-only clone can use `mdkg install --view` while still in ref mode to
inspect committed graph content, once the graph ref exists. This view is
for inspection until checkout registration and a settlement branch are
ready. The switch can reattach a detached view at the tip, but it registers
no graph checkout and creates no settlement branch; `mdkg install` then
registers a reattached `.worktrees/graph` as the graph checkout. When the
view was removed, the switch unsets the records of this graph whose
checkout is provably gone (another graph's stays, without a note; one it
can't read stays, with a `doctor --repair` note) and, when no `.worktree`
record remains and
the graph ref is the default `refs/heads/mdkg-graph`, makes and registers
a new graph checkout through install's own path, which records every graph
again. A custom graph ref (0.7.2) is moved first, since install's graph
checkout is made on `mdkg-graph` only: `refs/heads/mdkg-graph` is created
at its tip (create-only), the custom ref is kept and no longer moves, and
each `mdkg.graph.<id>.ref` record naming it is set to
`refs/heads/mdkg-graph`; the rest runs as for the default ref. The switch
refuses before changing anything (`graph.ref_invalid`) when `mdkg-graph`
already holds another commit (inspect it and keep or merge its commits;
the hint gives the compare-and-swap `git update-ref` that points it at the
graph, after freeing it in any worktree that has it checked out), when
`mdkg-graph` is checked out in a worktree other than the view or a
registered graph checkout, or when branches under `mdkg-graph/` keep git
from creating it. Running the switch again finishes one that stopped
part way: records still naming the custom ref are retargeted and a
detached view at the tip is reattached. To keep using the custom ref,
stay in ref mode. A record it kept (such as a nested graph's no longer
declared) leaves the checkout unregistered; checkout-mode reads then
refuse with an identity diagnostic such as `graph.ref_missing` or
`graph.identity_mismatch` (a `.root` identity left at that root refuses
with `graph.identity_mismatch` too; `graph.checkout_missing` when no
`.worktree`
record is left at all), never reading this checkout. Read the identity
diagnostic that `mdkg status` or `mdkg doctor` reports; `doctor --repair`
adds missing identities but doesn't remove stale or conflicting records,
so unset a record only once you've verified it's stale (`git config
--unset mdkg.graph.<id>.worktree`), then run `mdkg install`. Install's prerequisites
apply: with no committed `.mdkg/config.json` on the main branch, the
checkout is deferred, as plain install defers it.
Run again in checkout mode, the switch finishes an interrupted cleanup.
Successful `list` or
`config show` reads there do not establish checkout-mode readiness:
`status` and graph writes can still refuse the unregistered checkout, and
doctor can report its missing identity record and an absent settlement
branch. Establish the clone-local graph-checkout registration and the
branch work settles on, then resolve the layout and branch diagnostics
from `mdkg doctor` and `mdkg status` before normal checkout-mode operation.
Preserve the view's edits throughout. This switch is separate from
`--undo`, which restores installed files; neither deletes the graph ref,
a branch or a worktree.

Install pins no destination and prints that a person must approve it before
anything is pushed. Since 0.7.2 that's `mdkg graph remote approve`, then
`mdkg graph sync` (see [Syncing with graph.remote](#syncing-with-graphremote)).
Install itself never contacts a remote.

### Ref mode

Set the key once in the clone:

```console
$ git config mdkg.graph.ref refs/heads/mdkg-graph
```

A fresh `mdkg init` then publishes the graph's first commit to that ref as an
orphan, with a compare-and-swap that fails if the ref appeared meanwhile: the
committed config with its identity, the `.mdkg/README.md` and, unless
`--graph-only`, the workflow skill. It writes nothing to the checkout and
changes no branch; like `install --setup`, it adds `/.mdkg-edit/` to the
common `info/exclude`, so `node edit` copies stay out of git and doctor's
`graph.exclude` check passes. Init records the clone's identity in the common config
(`mdkg.graph.<graph_id>.ref`, plus `.root` for a graph below the repository
root and `.dir` for a folder that isn't `.mdkg`), and adopts nothing else: an
existing graph ref this init didn't create is install's to adopt.

Every command then runs against the graph ref's tip:

- `mdkg status` says `settling on refs/heads/mdkg-graph`, and its JSON `graph`
  block has the ref, the tip it read, the configured remote's name, and any
  graph edits found outside the ref. `main` in that output is the graph ref;
  `code_base` is `$MDKG_MAIN_REF` when set, else `mdkg.mainRef`.
- `mdkg node edit` writes a writable copy to the worktree's `.mdkg-edit/`
  folder, at the node's graph path, and `mdkg node save` publishes it through
  the graph ref. The copy folder is added to the clone's `info/exclude`, so
  `git add -A` doesn't commit a copy; if it can't be written, the command
  warns (`graph.edit_unexcluded`). Install also provisions this exclusion,
  and doctor checks it.
  Local work nodes (under `.git/mdkg/work`, ref-mode `init`'s default for
  tasks) aren't on the graph ref: both commands refuse them before taking a
  lease, and you edit their file in place, at the path `new` printed.
- `mdkg agents add`, `edit` and `rm` publish definition changes to the graph
  ref, without writing or committing the code checkout.
- `mdkg agents up` accepts a checkout of `main` in ref mode. It replays
  pending graph intents and refreshes its cache from the ref; it does not
  fast-forward a graph checkout from main. Checkout mode keeps the launcher
  refusal (`run.branch_refused`). Workers still use their own branches.
- `mdkg install` projects from the graph tip into the invoking code
  checkout. `mdkg doctor` checks that graph without needing a checkout copy.
- A second clone can adopt an already fetched graph ref through install,
  which validates it and records the clone's identity. Fetch it yourself
  before install; install makes no network call.

## Where authority comes from

One read pins one commit and reads everything from it: the committed config,
`.mdkg/agents/*.json`, and the inline `agents.<name>` entries no file
replaces. The read's source names the commit it used (`graph:<ref>@<sha>` in
ref mode, `checkout:<rev>@<sha>` otherwise), and the command uses that commit
for its whole run, so a publish in another process can't split a decision.

The clone's identity records are the second half. The tip's graph id must be
one this clone records for that ref, at that root; otherwise writes are
refused with `graph.identity_mismatch`. That catches a typo and a
`mdkg.graph.ref` pointed at another graph. A 0.7.0 record (`.worktree` alone)
stays readable wherever its checkout holds the id, and an explicit `.root`
always wins over it.

The launcher uses the same authority reader, including from a linked
checkout: it pins the graph ref's commit and reads definitions and
configuration from that commit, not the checkout's copy.

In ref mode the graph's files are materialized in a private cache,
`<git common dir>/mdkg/graph/cache/`, with a manifest beside it. It isn't a
git worktree and has no index. Outside a write session its files are
read-only. Edits that no mdkg write made are moved aside to
`<git common dir>/mdkg/graph/rescued/`, reported by `status` as
`graph.hand_edits`, and never read. `status`, the read that sets them aside
and `doctor` give the same advice. To keep a node's change, run `mdkg node
edit <id>`, make it in that copy, then `mdkg node save <id>`, which
publishes it. Only after the save succeeds, clean up where the edit was:
delete a rescued copy; put a changed checkout file back in both the index
and the worktree with `git restore --staged --worktree -- <path>` (delete
an untracked one); and take a branch commit's graph paths, and only those,
back to the code base in a new commit (`git checkout <code base> --
<graph paths>`, or `git rm` for paths it added), so its other changes
stay. Status keeps listing that commit while it's on the branch and not
in the local code base (`<code base>..HEAD`): until a merge commit reaches
the code base here, and after a squash or rebase merge until the branch is
rebased onto it. Keep
any other rescued file for a later release's importer. Files set aside as
*unclassified* (left by a write session that stopped, in a folder named
`-unclassified-`) aren't importable: compare them with the graph, and
remove them once nothing in them is wanted. `mdkg status --json` lists
every edit under `graph.hand_edits`, unclassified folders included. A
write that stopped before publishing leaves its own copies in an
`-unfinished-` folder: they aren't hand edits and aren't listed; doctor's
`graph.cache` names them, and the advice is to run that command again
(its `UNFINISHED.json` names the op), never to save its files by hand.
Remove a rescue folder once nothing in it is wanted: status and doctor
report it until it's gone. In
ref mode a graph path read from the
checkout is refused with `graph.checkout_read`, so a code branch's stale copy
can't stand in for the graph.

### Changing the committed config

Settings that runtime commands read come from this committed config, never
a working-tree copy or a user or machine-local layer. (In checkout mode,
the working copy is still checked before most commands run, mail
included: one that isn't valid JSON, or has a schema or format version
this build can't read, refuses them. A command that loads the whole
config, such as `claim`, also refuses fields its parser rejects. These
checks only validate the working copy; they never set behaviour: an
uncommitted `ack_grace_days`, even out of range, is ignored; an
uncommitted `coordination.leases` the parser rejects (such as
`default: "advisory"`) refuses `claim`, while a valid one changes
nothing, since claims use the committed policy; and `affected` doesn't
read the working copy at all.) These are the mail
settings under `coordination.messages` (`ack_grace_days`, `retention`,
`human_hold_days`), the lease policy under `coordination.leases`, and
`affected` rules. A change takes effect once it's committed where the
authority is read. `mdkg config show` names that commit in its project
layer's source, and warns when a user or machine-local layer sets
`coordination`, which changes nothing at run time.

Change it with `mdkg config set` and `mdkg config unset`, in either mode:

```console
$ mdkg config set coordination.messages.ack_grace_days 0 --dry-run
$ mdkg config set coordination.messages.ack_grace_days 0
$ mdkg config unset coordination.messages.ack_grace_days
```

The key is a dotted path or a JSON pointer
(`/coordination/messages/ack_grace_days`), and the value is JSON. A bare
word, such as `shared`, is taken as a string; quote a string that looks
like a number. The command refuses a key this build doesn't know (one
that isn't in the committed config, the built-in defaults or a section
mdkg reads), the graph's identity keys (`schema_version`, `format_version`,
`graph_id`), `work.location` and its `work.committed` alias (including a
whole `work` set or unset that would change them: `mdkg work localize`
moves work out of git together with its nodes), and any change after which the config would fail the
checks `validate` and `doctor` make, such as an `ack_grace_days` outside
0 to 3650. A refused change writes nothing. `--dry-run` prints the
change as a diff without claiming, writing or committing anything.
`--json` gives the key, its old and new values (credential-like values
hidden, as `config show` hides them), the diff, the commit it read
(`base`), the commit it made (`commit`), and in ref mode the graph ref.

The change holds an exclusive lease on `.mdkg/config.json` while it
runs. The command takes the lease and releases it. If you already hold
one, it uses that one: a change it commits uses the lease up, as `done`
does, and a change that changes nothing leaves it held.

- **Ref mode:** the change is published on the graph ref through mdkg's
  own graph writer, as `new` and `node save` publish. It is recorded as
  a pending intent first. The ref is moved by compare-and-swap, and
  rebuilt on a newer tip when another write moved the ref but left the
  config alone. A crash is replayed by a later graph command. If another
  write changed the config meanwhile, the change is reported stale and
  never published over it. No checkout is touched.
- **Checkout mode:** the change is committed in the checkout the
  committed config is read from. With a graph checkout registered, run
  it there (`.worktrees/graph`); elsewhere it refuses, because a commit
  on another branch isn't read. An uncommitted edit to the config refuses
  it too, so the commit never includes that edit.

A change takes effect for commands that start after it. `agents up`
keeps the settings it started with until it is restarted. This changes
only the local ref or branch. Publishing it is the same push as any
graph change, and it needs the same approval for the destination.

#### By hand, for recovery

`config set` reads the committed config with the same parser every
command uses. If a hand edit has left a config that parser refuses (an
invalid lease policy, say), `config set` refuses it as well, and a person
repairs it through git. In ref mode:

1. Take the target from `mdkg config show --json` (`data.graph`: the
   common git folder, the graph ref, the commit and the config's path),
   with `GIT_DIR`, `GIT_COMMON_DIR` and the other `GIT_*` location
   variables unset. If `config show` refuses too, take the ref from the
   repository's own `git config mdkg.graph.ref` and its tip from
   `git rev-parse`.
2. Check that no worktree has the graph branch checked out (`git
   --git-dir="$common" worktree list`).
3. Fix the file in a detached worktree of that commit (`git worktree add
   --detach`). Commit it there, with `--no-verify` if mdkg's hooks refuse
   because the current config is the broken one. Then move the ref by
   compare-and-swap against the commit you started from: `git update-ref
   "$ref" <new> <old>`. If the swap is refused, the ref moved: rebase
   onto the new tip and swap again. Never force it.
4. Run `mdkg doctor`, then `mdkg config show`.

Agents can't commit to graph files by hand: with the git hooks
installed, that commit is refused (`git.graph_path`). An agent can run
`mdkg config set` and `config unset`, as it can run `agents edit`: the
change goes through mdkg's own writer and takes effect at once.
Changes to authority fields (`env`, `sandbox`, `approvals`) are refused inside
an agent's turn with `authority.field_in_turn`. This covers `config set`,
`config unset`, and definition add, edit and removal, including whole-object
replacements and `agents edit --file`. Defaults and per-agent policies are
checked. The same fields may be changed by a person outside a turn, in their
own terminal; orchestrators have no turn-time exception. Turn markers, live
actor records and launcher ancestry are checked using the existing outside-turn
mechanism. Clearing a marker or changing the claimed actor does not grant
permission. An unrelated worker in the same checkout does not block a person's
terminal. Ordinary edits, identical values and config dry runs remain available.

## How work settles

A ref-mode write is prepared, recorded as an intent in the store, and
published to the graph ref; a crash between the steps is replayed by a later
graph command such as `mdkg renew`. `status` reports `graph.intents` and leaves
pending intents unchanged. A request id makes a retry return the first receipt
instead of writing twice. `new`, `done`, `node save` and the goal actions all publish
this way.

Because every reader reads the ref, there is nothing to bring into a
checkout:

- `mdkg sync <node>` reports `graph.nothing_to_sync`.
- `mdkg status`'s queue is empty once a change is published; what's left to
  merge is code.
- A changeset already published on the graph ref can't be abandoned
  (`coord.abandon_published`); change the node again, or write a revert.

The store (leases, mail, temporary statuses, the changesets) stays beside the
clone's git folder and is never shared by pushing the graph. That is the
[next section](#a-graph-ref-is-not-a-shared-ledger-yet)'s limit.

Work nodes follow `work.location`. With `shared` they're committed to the
graph ref like design. With `local` they stay beside the coordinator store,
outside git, and can be created and read in either mode. Ref mode maps the
private cache back to that same clone-local work folder.

In ref mode, `mdkg validate --changed-only` still validates the whole graph.
It reports all errors, but filters warnings to graph paths whose contents
differ from the code base's tree (`mdkg.mainRef`, or `$MDKG_MAIN_REF` when
set). If no code base resolves, it warns and counts no paths as changed.

The graph lock accepts local filesystems on an allowlist. On macOS, iOS and FreeBSD
the names are APFS, HFS, UFS, ZFS and tmpfs. On Linux they are ext2/3/4,
XFS, Btrfs, tmpfs, ZFS, F2FS, overlayfs, ReiserFS, JFS, bcachefs, ntfs3
and HFS+. Network, FUSE, host-shared and unknown filesystems are refused.

## Git hooks

mdkg's git hooks are opt-in, like skills and agent configs:

```console
$ mdkg install --enable hooks:git
```

`--hooks` opts into all hook projections the graph config lists, including
`hooks:claude`, `hooks:codex` and `hooks:opencode`; `--enable hooks:<name>`
opts into that one. Without either, install skips hooks not previously
installed in this checkout, even if another checkout enabled them in the
graph. Later plain installs keep already installed hooks in sync.

Claude's hook file is `.claude/settings.local.json`, Codex's is
`.codex/hooks.json`, and OpenCode's is `.opencode/plugins/mdkg/server.ts`.
Shared harness hook files install writes are ignored before being written:
in common `info/exclude` for professional setups, and in this checkout's
`.gitignore` otherwise. A newly created Claude per-user file goes in
`info/exclude` in every setup; an existing one keeps its ignore state.
Install never writes Claude's shared `.claude/settings.json`. A tracked
hook file cannot be ignored: install reports it, and professional setup
leaves it alone.

Git's effective-ignore answer is checked before a new hook file is made.
A negation that would include it again refuses the install and names the
rule. An existing shared hook file that remains included is reported.
Before publishing projection-list changes or missing canonical hook files
to the graph ref, install preflights the hook targets and the whole
projection sync. A merge or ignore refusal publishes neither change. The
setup itself can already have provisioned the ref and clone-local keys
before projection planning; this is not a rollback of that setup.

Install points the repository's `core.hooksPath` at
`<git common dir>/mdkg/hooks/git/` and writes `pre-commit` and `pre-push`
there, each running mdkg's rules first and then whatever folder
`core.hooksPath` named before, so the hooks you had keep running. It records
what it replaced, and `mdkg install --undo` puts it back.

With them installed, mdkg applies, for people and agents alike:

- **`git.protect`** — agent pushes to protected branches are refused. The
  protected list is `main`, `master`, `mdkg.mainRef`, the graph ref and the
  configured remote ref (`graph.remoteRef`), the agent definition's `protect`
  list, and the branches in GitHub's protection cache when present. Names
  compare without case, and each name comes from the config, never
  `$MDKG_MAIN_REF` (an agent's shell can set that).
- **`secret.scan`** — a commit's staged changes and a push's outgoing commits
  are scanned for credentials. A finding refuses the commit or push; a scan
  that can't run refuses too. (The installed wrapper warns and carries on if
  it can't make its temporary file at all, so the scan never sees the input.)
- **`git.graph_path`** — in ref mode an agent's commit on a code branch that
  changes a graph-owned file is refused, and a person's is a warning. A merge
  that keeps main's graph files as they are is exempt: its result must equal
  the blob of a parent that `mdkg.mainRef` holds (that ref or one of its
  ancestors). So is a design file equal to the graph tip's, which lets a
  design copy reach `main` through a pull request. A `.mdkg-edit/` copy is
  never exempt, in either mode, and an agent's commit on the graph ref's own
  branch is refused.

At 0.7.1 `hooks:git` routes mdkg's rules only for `pre-commit` and `pre-push`, so a merge is
checked when it's concluded with `git commit` (after a conflict, or with
`--no-commit`). A clean `git merge` that runs to completion is first checked
at push, through `git.protect` and `secret.scan`. Other pre-existing hooks
get forwarding wrappers, without mdkg's rules; install does not add a
`pre-merge-commit` guard.

An agent's direct update of the graph ref is refused by the
reference-transaction guard (`git.graph_ref`), and is a warning for a person.
mdkg's own writes run git with hooks off, so they never reach it. At 0.7.1
install doesn't route mdkg's rules for that hook, so its graph-ref guard
only applies where a hooks folder supplies it separately.

In checkout mode, a positively identified absent graph — an unborn `HEAD`, or
a clone whose main ref commits no graph — lets a commit through, with the
ancestor-turn, environment and secret rules still applied. A configured graph
ref that's missing, unreadable, foreign or invalid fails closed with install
guidance. Where the clone has no `.mdkg` at all, mdkg's hooks still apply
`git.protect` and `secret.scan`.

## Doctor and undo

In ref mode, doctor's `code_base` check reports a missing code base without
requiring main. `graph.ref` checks the ref and committed graph identity;
`graph.shape` checks graph-only versus full-repository shape; `graph.remote`
checks the configured remote, protected code refs, approval and the last
sync (see **Doctor** under [Syncing with graph.remote](#syncing-with-graphremote)). In
0.7.2 a never approved remote is a warning, so `doctor --strict` fails until a
person runs `mdkg graph remote approve`. `graph.secret_scan` says whether
the scanner graph pushes need can run. (In 0.7.1, with no approve
command, an unapproved remote is `ok`.) The remaining checks are
`graph.migrating`, when a migration marker exists; `graph.intents`,
`graph.cache`, `graph.view`, `graph.hand_edits`, `graph.design_drift` and
`graph.exclude`. The last checks for `/.mdkg-edit/` in common `info/exclude`.
Design drift is information, not a warning or permission to publish.
Checkout mode is reported as information with a setup hint. The separate
`hooks` check reports enabled hook projections that need syncing; it does
not provision the missing git guard routes.

`mdkg install --undo` restores this checkout's installed files, subject to
its checks for later edits. In ref mode it leaves the graph's shared
projection list as it is, and names its entries in the report; use
`mdkg install --disable <name>` to change that list. Undo removes only the
ignore lines install added, preserving other edits to `.gitignore`.

If an undo copy is missing or damaged, install refuses before its projection
writes. Restore the affected file's exact pre-install bytes, then rerun
install: a writing install can recreate the saved copy when its hash
matches, up to 16 MiB. A dry run checks but repairs nothing. Above that cap,
follow the diagnostic's manual recovery path. The diagnostic also names
the record folder that can be removed to keep the present files and give
up that undo history; preserve anything you need before choosing that path.
`--checkout-mode` remains available with an unusable undo record, warning
and leaving that record in place.

## Credentials and privacy

`mdkg.graph.remote` names an existing git remote; it never creates or edits
one. `graph.remoteRef` (the remote's ref, default the graph ref's own name)
must be a branch. Sync uses your own git setup for the network (credential
helpers, `insteadOf`, `core.sshCommand`, your `pre-push` hooks) and never
reads, stores or prints a credential: a URL's userinfo is shown as `***@`.
Nothing can prompt: git and ssh run with prompts off and a time limit, so a
remote that needs a typed password fails as `graph.remote_auth` instead.
The limits, as built: each network git command may run for 120 seconds,
then its process group is stopped; git doesn't get a session of its own,
so a `GIT_SSH_COMMAND` or `core.sshCommand` of yours without
`BatchMode=yes`, or a signing passphrase prompt (`push.gpgSign`), can hold
a sync (and the launcher's sync thread) for those 120 seconds. With no ssh
command of yours set, mdkg's own ssh default sets `BatchMode=yes` and a
15-second connect timeout.

The graph ref holds the graph: config, definitions, design and shared work. A
graph-only ref carries no code history; a ref in 0.7.0's full shape (a
`mdkg-graph` branch that carried the repository) holds the code too. The
clone's identity records live in its common git config, never in a commit.
Secrets are part of what the graph holds: with `hooks:git` the scanner
refuses a commit or push that carries one before it leaves.

## Syncing with graph.remote

Since 0.7.2 the graph ref syncs with `mdkg.graph.remote`: the code's own
remote in the personal setup, or a private repository of your own in the
professional one. Only `remoteRef` crosses, through one explicit refspec:
no tags, no other branch, no store, mail, leases or local work. mdkg never
pushes a code branch.

**Approve first.** Configuration alone never pushes. A person, in their own
terminal and outside any agent turn, runs:

```sh
mdkg graph remote approve --dry-run   # shows the remote, remoteRef, and where fetches and pushes go
mdkg graph remote approve             # pins them in <git common dir>/mdkg/graph/remote.json
```

The pin holds each URL's exact endpoint, with `insteadOf` and
`pushInsteadOf` applied: local paths (plain or `file://`) resolved against
the checkout and made canonical, and only what can't name another
repository folded (the scheme's and host's case, a default port; a
password is never kept). A user and every byte of the path stay, so
`alice@host:g` and `bob@host:g`, or `repo` and `repo.git`, are different
destinations. An https (or any non-ssh) URL's user can be an access token,
so it's kept only as a digest (`~<hex>@`): a changed user still reads as a
changed destination, and the token is never stored or printed. A `file://`
URL's path is percent-decoded, as git opens it; one naming a host other
than `localhost` is refused (`graph.remote_invalid`). A graph remote has one destination: its fetch and push URLs
must name one repository (`graph.remote_invalid` otherwise), since sync
pushes against the tip it fetched. The pin also holds the git setup a
push runs with that an agent could change: the system, global, local and
worktree values (every scope git reads but the command line's `-c` and
`GIT_CONFIG_*`) of `core.sshCommand`, `core.hooksPath`,
`remote.<name>.receivepack` and `uploadpack`, `credential.*`, `hook.*` and
`include.*`/`includeIf.*`, each as a digest (never the value), plus one
digest of all of them in git's order (git reads the order: an empty
`credential.helper` resets the helpers before it, and a key with no value
differs from an empty one), and the `pre-push` hook git would run: its
content and whether git would run it, by the same `access(X_OK)` check git
makes, so a mode, owner or ACL change that turns it on or off counts. A
hooks path git can't expand is refused. If the remote's URL, push URL,
name, `remoteRef` or that setup changes later, every sync stops with
`graph.remote_changed`, naming what changed (a URL inside a key only by
its digest), until a person approves again. A hook script that runs other
files is pinned by its own content only. Approval
is refused inside an agent's turn (`graph.remote_unapproved`); until person
keys arrive, "a person" means the same outside-a-turn check delegated
decisions use (no launcher turn mark, no live launcher turn in this clone,
not a descendant of one).

Approval and every push refuse a destination that is code:

- `remoteRef` must not be `main`, `master`, `mainRef`'s branch, a branch in
  a definition's `protect` list or GitHub's protection cache, or the
  remote's default branch (`graph.remote_is_code`);
- a graph remote other than the code's must not share a destination with
  any other remote, so a company remote is never pushed to by another name.
  This check compares loosely (scheme, user, a trailing `.git` and slashes
  dropped, local paths canonical), so an alias of a code repository is
  still caught; a remote counts as the code's only where the code remote
  itself pushes;
- an existing remote graph ref that shares history with `mainRef` is
  refused, except the personal full-shape graph on the code's own remote;
- a remote other than the code's receives graph-only history: every
  outgoing commit may hold only graph folders (`graph.shape`).

Network git runs from the checkout you run mdkg in, with that checkout's
own git folder, so your relative hooks (`core.hooksPath`), a linked
worktree's `config.worktree` and `includeIf.onbranch`, and relative local
remote paths apply as they do for your own `git push` there. The
destination checks and the approval read the remote's URLs in that same
checkout, so a `pushurl` set only in a worktree is seen. The fetch
writes only its one ref (`--refmap=`, `--no-prune`): a remote's configured
fetch refspecs move nothing else. It runs in a process group of its own,
not a new session, so it keeps your terminal: nothing can prompt
(`GIT_TERMINAL_PROMPT=0`, no askpass, and ssh in batch mode unless you set
an ssh command of your own), a read of the
terminal stops it, and its time limit ends it.

**Sync by hand.** `mdkg graph sync` fetches `remoteRef`, takes a
fast-forward or merges, checks the result strictly, moves the local graph
ref, and pushes with a lease (a compare-and-swap on the remote, never a
force). Unapproved, it neither pulls nor pushes and says how to approve:
an unapproved remote could hold definitions or protect lists an agent
seeded, so nothing is read from it. `--no-push` pulls only. It's refused inside an agent's turn (`graph.sync_in_turn`).

**Background sync.** `agents up` pulls when it starts, before it lets any
turn begin, and every
`mdkg.graph.sync.pullInterval` (900 s) after the last successful pull
from the approved destination (`0` means only the first; a newly approved
destination is due a pull at once, after any backoff below), and pushes
`mdkg.graph.sync.pushDelay` (120 s) after the last local graph change,
coalescing changes in that window into one push. Never per turn, and never
inline after a write: writes publish locally and return. A due pull only pulls;
one that leaves the graph ahead (after a merge) starts the push delay. The
settings are read again every second, so a changed delay, interval or
remote applies without a restart. The timers keep running until every turn
has finished; when `agents up` stops, a push still waiting for its delay is
dropped: your `mdkg graph sync` sends it, or the next `agents up`, whose
first pull finds the graph ahead and starts the push delay. Before
a sync moves the local graph, any graph write a crash left pending is
replayed first. Background sync goes only to an approved, unchanged
destination: for an unapproved or changed one the launcher neither pulls nor
pushes. A failed or refused sync counts as no pull (`sync.json` records
most failures; an unapproved or changed destination is refused before
anything is recorded): the launcher waits 30 s, doubling up to 15 minutes,
and tries again (a push stays due). Changing the
interval applies it to the last successful pull. `agents up --once` runs no
timers, and an `agents up` started inside an agent's turn runs without sync
and says so once: sync starts only after the outside-turn check `mdkg graph
sync` makes. Bad agent names fail before the first pull. A graph remote
configured while `agents up` runs is picked up on the next tick, and
pulled from once a person approves it. Another device sees a change about
2 minutes plus up to one pull interval later.

Each sync, approval and resolution holds `<git common dir>/mdkg/graph/sync.lock`
while it reads and writes `sync.json`, `conflicts.json` and `remote.json`,
so a background sync and yours take turns. A pulled remote tip is recorded
as accepted before the local graph ref moves, so a crash between the two
still treats a later reset of the remote as a rewrite. An adopted rewrite
is the exception: it's accepted only after the local graph moves, so a
crash or a failed backup leaves the rewrite refused until you accept it
again.

**Before every push** the outgoing commits pass the destination checks
above, a 64 MiB cap (`graph.push_large`), and the secret scan, the same
`secret.scan` git's hooks run, over every commit the remote doesn't have,
so an overwritten or deleted secret in history is found too. When the
graph remote is the code's own (the personal setup), commits the
destination already holds (its branches, as `ls-remote` reports them at
push time, never a local tracking ref) don't count as outgoing:
they aren't sized or scanned again, so a first push doesn't walk all of
the code's history. A branch deleted from the destination between that
read and the push is a window this accepts: its commits were on that same
destination when it was read. A finding
refuses the push (`graph.secret`) and names the commit and path, never the
match; a scan that can't run, or a blob too large to read, refuses it too
(`graph.secret_scan_unavailable`). While `refs/replace/*` or a grafts file
exists, pushes are refused the same way, since either could make git show
the scan something other than what's sent. Remove the string from the graph's
history, and rotate the credential if it was ever real.

**Hooks.** Network commands run your `pre-push` hooks. A hook's refusal is
reported (`graph.push_refused`) and never retried around: background sync
stops until a person runs `mdkg graph sync`. Before each push, sync writes a
latch to `sync.json`; any push that fails, however git reports it, keeps
the latch (`graph.push_unrecorded` when the outcome is unknown), so a
refusal is never lost to a crash or a misread error.

**Conflicts.** When both sides changed a path, the remote's version wins,
deletions included (G12). The local version is kept in history as the
merge's first parent (for an adopted rewrite, in the backup ref instead) and named in an `Mdkg-Conflict: <path> local=<blob>
remote=<blob>` trailer. Each conflict is recorded in `<git common
dir>/mdkg/graph/conflicts.json`; `mdkg graph conflicts` lists them and
`status` counts them. `mdkg graph resolve <path>... --take local|remote`
publishes your choice for each path in one commit on the graph ref and
clears their entries in one write. A path with a hidden character (a
zero-width joiner, say) is listed with it escaped (`\u{200d}`), and
resolve takes it in that form too, as the sync's note suggests; a
recorded path that matches as typed always wins. A path with more than one open conflict
needs `--merge <commit>` (at least 7 hex digits of the merge `graph
conflicts` lists); without it, resolve lists them and settles nothing, so
repeating a resolution whose outcome was uncertain never settles an older
conflict in place of the one you chose. The result passes the same strict validation a
sync's merge does: one that would break the graph (two nodes with one
id, say) is refused (`graph.sync_invalid`), nothing is published and the
conflicts stay open; take the remote's version, or settle the paths that
belong together in one call. A conflict that differs only in a file's
mode is listed, but `--take local` restores its content, not its mode. A merged tip
that fails validation takes the remote's version of every path both sides
changed; if that fails too (a rename on one side and an edit on the other
can leave one id twice), it takes the remote's tree as it is, recording
each path the local side changed as a conflict; if even that fails,
nothing is taken (`graph.sync_invalid`).

**A rewritten remote** (its graph ref no longer descends from the tip this
clone last accepted) is refused, across restarts, with
`graph.remote_rewritten`. After checking it, a person runs `mdkg graph sync
--accept-rewrite`, which adopts it: the local graph is kept under
`refs/mdkg/backup/<remote>/<time>`, and only your changes since the last
accepted tip are re-applied on the rewritten one, in a commit whose only
parent is the remote's, so nothing the rewrite removed is pushed again.
It waits while sync conflicts are open (`graph.sync_conflict`): settle
them first, since adopting would leave the merges that record them.
An adoption is recognized from the history itself: once the local graph
holds the adopted tip, later syncs (a retry after another device pushed,
or one after a crash that lost `sync.json`'s update) go on from it and
never adopt again.
A deleted remote graph ref has no tip to adopt, and pushing would
recreate it with all the history the deletion removed, so it's refused
too, `--accept-rewrite` included. To publish the local graph there again,
a person runs `mdkg graph remote approve`; the next sync may then create
the ref. If the ref is back by then, at any tip, that permission ends and
the rewrite check applies to it as before.

**Offline.** An unreachable remote or a refused login is a warning
(`graph.remote_unreachable`, `graph.remote_auth`), recorded with the time
and the number of failures in a row in `sync.json`. `status` and `doctor`
show it; the next pull retries. A push that fails partway keeps the push
latch, though, so background sync stops until a person runs `mdkg graph
sync`. Local writes never wait for the network.

**Doctor.** With a graph remote, `doctor`'s `graph.remote` row runs the
destination checks and reports, most serious first: a code destination or
a changed one (error); never approved (a warning with the approve hint:
nothing is pulled or pushed); the push latch; a deleted remote graph ref
(a later failure of another kind first, naming the deletion; then
whether it waits for approval or for the push that recreates it); the
last failure; open
conflicts; approved but never synced; and how many graph commits are not
pushed or not pulled. `graph.secret_scan`
says whether the scanner can run.

## A graph ref is not a shared ledger yet

Leases, mail and the settlement log live in each clone's store; pushing the
graph ref would share committed content only. So two clones of one graph can
each hold a lease on the same node and each settle it, neither seeing the
other, and nothing reconciles them. The intended guard,
`graph.settled_elsewhere`, is defined but nothing raises it yet; 0.7.2's
sync shares the graph's commits, not the ledgers.

Until then, keep one clone writing a graph ref. `mdkg status` and the leases
it lists describe that clone, not the graph.

## Design on main is a copy

Main's `.mdkg/design/**` is never read as authority in ref mode. It may hold
a published copy so a reader without mdkg can read the design as context;
`git.graph_path` lets such a copy through when it equals the graph tip's
blob. Work nodes never go to main this way.

Whether a clone may publish the copy is `mdkg.graph.publishDesign`: unset
means off; explicit personal setup turns it on, and professional or local
setup leaves it off unless given `--publish-design`. Doctor reports drift
from the code base as information. `mdkg graph publish-design`, which prepares a
branch based on `mdkg.mainRef` and never writes `main`, arrives in 0.7.2 with
the sync commands.

## Limits at 0.7.1

- No sync, import, split, recover, `publish-design`, or remote approval.
  `mdkg graph` is the query group it was in 0.7.0 (`refs`, `backrefs`,
  `ready`, `critical-path`); `graph.remote` is configuration only. 0.7.2
  adds `graph remote approve`, `graph sync`, `graph conflicts` and `graph
  resolve`, and `agents up` uses `pushDelay` and `pullInterval`; import,
  split, recover and `publish-design` are still to come.
- A graph ref checked out on a branch cannot be used for ref-mode writes;
  authority reads still use the pinned ref.
- A `.worktrees/graph` view kept through the move isn't refreshed by graph
  writes, so after each one `doctor` warns `graph.view: the view is behind
  the tip` (and `doctor --strict` fails). Refresh it with `mdkg install
  --view`, which never overwrites an ignored draft in the view (a plain
  `git switch`, which doctor's hint suggests, can). If it refuses because
  an ignored draft sits where the tip now has a file, move that draft
  somewhere safe and run it again. To remove the view instead, preserve
  its ignored drafts first.

Install also has these residuals under the one-user, one-machine threat
model:

- A professional exclusion is clone-wide, so it can hide another worktree's
  untracked file at mdkg's own projection or hook path.
- Updating an existing untracked shared hook file has a crash window;
  its file and undo records are not one atomic transaction.
- APFS alias states remain an edge case for path checks.
- A workspace prefix differing only by letter case can evade the graph-path
  comparison during a move.
- Concurrent changes during the move are outside its snapshot checks.
- Detaching an old 0.7.0 `.worktrees/graph` does not migrate or consume its
  uncommitted edits; preserve and reconcile them before using the new mode.

The tests that exercise this layout are the
[graph ref fixtures](../crates/mdkg-graph/tests/graphref.rs), the
[write fixtures](../crates/mdkg-cli/tests/graph_ref_writes.rs) and the
[hook fixtures](../crates/mdkg-cli/tests/hook.rs). The
[setup fixtures](../crates/mdkg-cli/tests/graph_setups.rs) cover install,
adoption, projections, refusal ordering, local work, undo and doctor.
