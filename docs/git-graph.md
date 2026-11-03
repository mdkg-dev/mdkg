# Git and the graph ref

mdkg reads its authority — the committed `.mdkg/config.json`, agent
definitions, designs and shared work — from git objects at one commit. In
0.7.0 that commit was the head of a checkout: the graph was committed on a
branch, usually `main`. Since 0.7.1 the graph can live on a **graph ref** of
its own, so a repository doesn't need a code branch, or `main`, to carry it.

This page describes what 0.7.1 does. Commands that arrive later are marked
where they'd change the picture; they aren't available yet.

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
It needs an existing graph ref. With no graph yet, run `mdkg install
--setup local`, then `mdkg init`, then `mdkg install --view` in that order
(choose personal or professional instead when appropriate). Asking for
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
again. A custom graph ref is refused up front (`graph.ref_invalid`),
still in ref mode with no mode or registration changed (binding the graph
may first complete an earlier interrupted graph write, as every graph
command does), since install's graph checkout is
made on `mdkg-graph` only: move the graph to `refs/heads/mdkg-graph`
(`git branch -m -- <name> mdkg-graph` for a branch, `git update-ref
refs/heads/mdkg-graph '<ref>^{commit}' ''` for a ref outside `refs/heads/`,
which peels an annotated tag and, with the empty old value, refuses if
`mdkg-graph` already exists: inspect that branch and keep or merge its
commits first) and set
`mdkg.graph.ref` and each `mdkg.graph.<id>.ref` record naming the old ref
to `refs/heads/mdkg-graph` first, or
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
anything is pushed. The approval command and graph sync arrive in 0.7.2;
there is no approval command to run in 0.7.1.

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

- **Checkout mode:** edit `.mdkg/config.json` and commit it. With a graph
  checkout registered, commit it there (`.worktrees/graph`), because a
  commit on another branch isn't read.
- **Ref mode:** no checkout has a copy, and `node edit` takes only nodes.
  A person changes the config through git. An agent can't: with the git
  hooks installed, its commit to a graph file is refused
  (`git.graph_path`).

  This recipe covers the graph at the repository root only; resolving a
  nested graph's target is planned for 0.7.2.

  1. Let mdkg name the target. In ref mode, `mdkg config show` reports
     the graph its settings came from, resolved exactly as every command
     resolves it (`graph:` in its text output, `data.graph` with
     `--json`): the repository's common git folder, the graph ref, the
     commit read, and the config's path in the ref's tree. Don't work
     these out with `git config` or `rev-parse`: git reads scopes and
     environment variables (such as `GIT_DIR` or `GIT_COMMON_DIR`) that
     mdkg ignores, so it can name another ref or repository. Clear those
     variables for the whole sequence, then set the target's values
     (the first four commands of the example below). If `config show`
     itself refuses because the committed config or an agent definition
     is broken, this recipe can't find its target; 0.7.2's `mdkg config
     set` covers that case.
  2. Check that no worktree has the graph branch checked out (`git
     --git-dir="$common" worktree list`). `update-ref` would move that
     worktree's branch without updating its files, and a later commit
     there could undo the change. Commit or save any edits there, then
     detach it (`git -C <worktree> switch --detach`).
  3. Make the change in a detached worktree of that commit, and move the
     ref with a compare-and-swap against it:

```console
$ unset GIT_DIR GIT_COMMON_DIR GIT_WORK_TREE GIT_INDEX_FILE \
    GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES
$ graph="$(mdkg config show --json | jq -c .data.graph)"
$ common="$(jq -r .common_dir <<<"$graph")"; ref="$(jq -r .ref <<<"$graph")"
$ old="$(jq -r .commit <<<"$graph")"; path="$(jq -r .config_path <<<"$graph")"
$ git --git-dir="$common" worktree add --detach /tmp/mdkg-config "$old"
$ $EDITOR "/tmp/mdkg-config/$path"
$ git -C /tmp/mdkg-config commit -am "config: purge acked mail at the ack"
$ git --git-dir="$common" update-ref "$ref" \
    "$(git -C /tmp/mdkg-config rev-parse HEAD)" "$old" \
  && git --git-dir="$common" worktree remove /tmp/mdkg-config && mdkg doctor
```

  The worktree is removed only when `update-ref` succeeds. If another write
  moved the ref since `config show` read it, `update-ref` refuses, the ref
  is unchanged, and the worktree still holds your commit. Retry with the
  block below, never by repeating the `update-ref` alone: it reads the new
  tip first, rebases onto that exact commit, and swaps only when the
  rebase succeeded and your commit sits on that tip, so the swap can't
  replace a newer graph with your older commit:

```console
$ old="$(git --git-dir="$common" rev-parse "$ref")"
$ git -C /tmp/mdkg-config rebase "$old" \
  && git -C /tmp/mdkg-config merge-base --is-ancestor "$old" HEAD \
  && new="$(git -C /tmp/mdkg-config rev-parse HEAD)" \
  && [ "$new" != "$old" ] \
  && git --git-dir="$common" update-ref "$ref" "$new" "$old" \
  && git --git-dir="$common" worktree remove /tmp/mdkg-config && mdkg doctor
```

  If any step fails (a refused or conflicted rebase, or the ref moved
  again), the ref is unchanged. Finish or abort the rebase (`git -C
  /tmp/mdkg-config rebase --continue` or `--abort`), and run the block
  again.
  Afterwards, `mdkg doctor` checks the committed config of the graph at
  the repository root, in `.mdkg` (`dir` above; in ref mode every command
  reads the root graph there until 0.7.2, so doctor, `config show`, every
  command and `install --setup` refuse an `mdkg.graph.<id>.dir` naming
  another folder for it), including mail
  settings that would refuse every send and ack. Run `mdkg config
  show` after it too: check the project layer's value and source (the
  commit you made), since its effective view can show a user or
  machine-local `coordination` setting that it warns about and that
  changes nothing at run time.

  With mdkg's git hooks installed, the commit is checked against the
  graph's current config. If that config is itself what's broken (an
  invalid lease policy, say), the hook refuses the correcting commit and
  names `mdkg doctor`. A person can then commit with `git -C
  /tmp/mdkg-config commit --no-verify -am "..."`, which skips git's hooks
  for that one commit; agents can't.

  This changes only the local ref. Publishing it is the same push as any
  graph change, and it needs the same approval for the destination.

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
checks the configured remote, protected code refs and approval state. No
approval is reported as `ok`, saying nothing is pushed: no 0.7.1 command
approves a graph remote, so it isn't a warning and `doctor --strict` passes. The remaining checks are
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
one. At 0.7.1 the key is read and shown by `status`, and `graph.remoteRef`
(the remote's ref, default the graph ref's own name) must be a branch, but
nothing contacts the remote. Sync is 0.7.2.

The graph ref holds the graph: config, definitions, design and shared work. A
graph-only ref carries no code history; a ref in 0.7.0's full shape (a
`mdkg-graph` branch that carried the repository) holds the code too. The
clone's identity records live in its common git config, never in a commit.
Secrets are part of what the graph holds: with `hooks:git` the scanner
refuses a commit or push that carries one before it leaves.

<!-- L2 (0.7.2): graph.remote is pushed and pulled on a cadence (pushDelay 120 s, pullInterval 900 s); background pushes go only to a destination a person pinned with `mdkg graph remote approve`, and run the clone's own pre-push hooks. -->

## A graph ref is not a shared ledger yet

Leases, mail and the settlement log live in each clone's store; pushing the
graph ref would share committed content only. So two clones of one graph can
each hold a lease on the same node and each settle it, neither seeing the
other, and nothing at 0.7.1 reconciles them. The intended guard,
`graph.settled_elsewhere`, is defined but nothing raises it; it lands with
sync, which is what lets a clone notice another's write.

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

- No sync, import, split, recover, `publish-design`, or remote approval. `mdkg
  graph` is the query group it was in 0.7.0 (`refs`, `backrefs`, `ready`,
  `critical-path`); `graph.remote` is configuration only.
- `mdkg.graph.sync.pushDelay` and `pullInterval` are read (defaults 120 and
  900 seconds) but nothing uses them yet.
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
