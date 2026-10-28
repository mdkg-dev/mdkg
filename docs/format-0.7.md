# mdkg format 0.7

**Format version:** `0.7`. It is frozen with mdkg 0.7.0: a change that an
0.7 reader can't read means a new format version.

The reference implementation is `crates/mdkg-format` (node files, ids,
refs, the node schema) with `crates/mdkg-graph` (graphs and their checks)
and `crates/mdkg-log` (log records and bundles).

The words MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119.

## 1. What the format covers

An mdkg graph has two planes:

- **Knowledge** lives in git, as markdown node files under `.mdkg/`: goals,
  decisions, designs, rules, skills, and task definitions with their final
  states.
- **Coordination** lives in a local store: claims, leases, temporary
  statuses, reviews and messages. Its durable record is a hash-linked log.
  Git keeps only anchors, small commitments to the log's head, and
  `export --work` writes verifiable bundles.

This document covers the files in both planes that leave the machine: node
files, `.mdkg/config.json`'s identity keys, ids and refs, log records,
anchors and bundles. The store's SQLite schema is not part of the format.

## 2. The graph and its config

A graph is a folder holding `.mdkg/config.json`. An 0.7 config carries:

| Key | Value |
|---|---|
| `schema_version` | `3`. 0.6.0 refuses anything above 2, so it never misreads an 0.7 graph |
| `format_version` | `"0.7"` |
| `graph_id` | A UUID (section 4.2), set once by `init` or `migrate` and never changed |

The combinations:

| `schema_version` | Means |
|---|---|
| absent, 0 or 1 | An unmigrated 0.6.0 graph, read in legacy mode (section 10). `graph_id` and `format_version` MUST be absent |
| 2 | 0.6.0's identity format v2, which keeps its graph identity in `.mdkg/graph.json`. Only `migrate` reads it (section 10.2); every other reader refuses it and says to migrate |
| 3 | An 0.7 graph. `format_version` MUST be `"0.7"`, and `graph_id` MUST be a UUID (section 4.2) |
| above 3 | Newer than this format: a reader MUST refuse it |

Any other combination is a config error.

**Where `graph_id` comes from.** A reader accepts any UUID (section 4.2).
`init` mints a fresh UUIDv7. `migrate` derives a UUIDv7 deterministically
(section 10.1), so every branch of a repository migrates to the same
`graph_id`, or keeps an identity v2 graph's own (section 10.2).

### 2.1 Other config keys

The rest of the config (workspaces, templates, statuses, priorities, index
and coordination settings) is as in 0.6.0, with these 0.7 rules:

- **`workspaces`:** with no `workspaces` key, the graph has one workspace,
  `root`, at `.`, with its folder at `.mdkg` (what 0.6.0 gives a config with
  no schema version).
- **`templates`:** `.mdkg/templates/` is optional. Templates seed `new`; the
  built-in ones apply where a graph has none. They never define what a node
  may hold (section 3.5). `validate` warns about falling back to the
  built-ins only in a legacy graph.
- **`work.location`:** `"local"` or `"git"`. Fresh `mdkg init` selects
  local work by default; `mdkg init --work shared` selects git storage.
  - `git`: every node file lives under the graph's folders and is committed
    (section 3.1).
  - `local`: work-type nodes (`task`, `bug`, `spike`, `test`, `checkpoint`)
    live beside the coordinator store on this machine, in the same file
    format. Every worktree shares them. Other types stay in git. Readers
    read both places as one graph, with unique ids across both (section 4.4).
  Local storage requires `schema_version: 3`. The old `work.committed`
  values `"none"` and `"definitions"` are read as aliases for `"local"` and
  `"git"`; conflicting settings or unknown values are config errors.
  Existing graphs with neither setting retain their implicit git storage.
- **`orchestration`:** an object, or absent, or `null`. Its keys, each a
  string, `null` or absent:
  - `branch` (default `"agent/{agent}"`): the branch an agent works on.
    It holds `{agent}` exactly once, and is a name git takes: it doesn't
    start with `/`, `-` or `refs/`, doesn't end with `/`, holds no `//`,
    no `..`, no control character, and none of `` ~^:?*[\ `` or a space.
  - `worktree` (default `".worktrees/{agent}"`): the agent's worktree, as a
    relative path from the main checkout. Its last part holds `{agent}`
    exactly once; it has no `.` or empty part, no control character, and
    doesn't end with `/`.
  - `{agent}` is the agent's name, with `:` written `%3A`.
- **`projections`:** an optional list of enabled projections, using the
  names below. A list enables exactly those projections; `[]` enables
  none, which is what a new `init` writes. Without this key, the installer
  keeps the pre-projection set: `agents-md`, `mcp:claude`, `mcp:codex`,
  `mcp:opencode`, `skills:.agents/skills` and `skills:.claude/skills`.
  Hooks are not in that default set.
- **Unknown keys** are ignored by readers and kept by writers. A key that
  only adds behaviour, with today's behaviour when absent, may be added
  without a new format version. A key whose presence changes what an
  existing file means, or which files are valid, needs a new format
  version.

Projection paths are relative to the project's top:

| Projection | What it writes |
|---|---|
| `agents-md` | mdkg's managed block in `AGENTS.md` |
| `mcp:claude` | The `mcpServers.mdkg` entry in `.mcp.json` |
| `mcp:codex` | The `mcp_servers.mdkg` table in `.codex/config.toml` |
| `mcp:opencode` | The `mcp.mdkg` entry in `opencode.json` |
| `skills:.agents/skills` | Copies of `.mdkg/skills/` under `.agents/skills/`, with a `.mdkg-managed.json` inventory in that mirror |
| `skills:.claude/skills` | Copies of `.mdkg/skills/` under `.claude/skills/`, with a `.mdkg-managed.json` inventory in that mirror |
| `hooks:claude` | Hooks from `.mdkg/hooks/claude/settings.json`, merged into `.claude/settings.json` |
| `hooks:codex` | Hooks from `.mdkg/hooks/codex/hooks.json`, merged into `.codex/hooks.json` |
| `hooks:opencode` | `.mdkg/hooks/opencode/mdkg/server.ts`, copied to `.opencode/plugins/mdkg/server.ts` |

`install --enable <names>` and `install --disable <names>` take
comma-separated names, record the changed list and sync it. A plain
`install` syncs the enabled projections. Existing hook canonical files
take precedence; when one is missing, install writes the build's supplied
definition if available, or refuses with the missing canonical path.
The managed blocks, entries and mirrors preserve the user's other content.

The compatibility default and exact-list selection are covered by
`no_key_keeps_todays_projections` and `a_list_is_exactly_what_it_names` in
[the projection tests](../crates/mdkg-run/src/projection.rs).
`a_sync_writes_exactly_the_enabled_projections` and
`hooks_project_their_canonical_files_and_leave_the_users_own` in
[the installer tests](../crates/mdkg-run/src/install.rs) cover the output
selection and canonical hook handling. The
[setup tests](../crates/mdkg-cli/tests/setup.rs) cover fresh init,
enable/disable, hook installation and undo.

### 2.2 Configuration layer engine

The [settings engine](../crates/mdkg-format/src/settings/README.md), typed
graph adapter, scoped file editor and `config show` command are implemented.
The `config set` command and runtime/agent reader integration remain pending;
their intended interface is distinguished below from the available display.

#### 2.2.1 Sources and precedence

Sources apply in this order; a later source overrides an earlier value:

| Priority, lowest first | Source | Scope |
|---|---|---|
| 1 | Built-in defaults for the running mdkg version | Values absent from other sources |
| 2 | `$XDG_CONFIG_HOME/mdkg/config.json`, or `$HOME/.config/mdkg/config.json` | User preferences across projects |
| 3 | `.mdkg/config.json` | Project settings, normally committed |
| 4 | `<git common dir>/mdkg/config.local.json` | This repository on this machine, shared by linked checkouts |
| 5 | Explicit command flags | This invocation only |

The XDG path wins when `XDG_CONFIG_HOME` is nonempty and absolute. Otherwise
an absolute `HOME` supplies the fallback path; an unset `HOME` means no user
layer, while an empty or relative `HOME` is an error. A missing file at a
valid XDG path does not cause fallback to the HOME path. Missing optional
user/local files contribute nothing; the project config is required by the
graph adapter.

Only the physical project file may supply `schema_version`,
`format_version` and `graph_id`. Its identity is validated before optional
sources are loaded. Those keys are refused in user, machine-local and flag
inputs, even if their values match the project. The scoped editor cannot
change identity in any file; init and migration manage it.

Objects merge recursively. Arrays, scalars and `null` replace the lower
value; `null` remains a value for the setting's validator to interpret,
and an empty object does not clear inherited object keys. Each effective
leaf, including an array element or empty container, retains its source
layer and file path where applicable. Leaf keys use RFC 6901 JSON pointers,
such as `/work/priority_min`; built-in and flag origins have no file path.

The graph adapter normalizes `work.committed` within each source before
merging, so a higher layer's `work.location` can override a lower layer's
alias. Conflicting spellings within one source are refused. Per-file agent
definitions can contribute project inputs after `.mdkg/config.json` and
before the machine-local file, retaining each definition's path as its
origin. `config show` uses this ordering; connecting runtime agent loading
to it remains pending.

Every file must be a JSON object with unique keys. Unreadable files, final
symlinks, special files, inputs over 1 MiB and nesting beyond 64 levels are
refused rather than skipped. Unknown keys survive merging and edits;
typed readers still validate the settings they consume.

#### 2.2.2 Machine-local discovery

Common-directory discovery uses physical `.git` metadata, as the work store
does, independently of `GIT_DIR` and `GIT_COMMON_DIR`. Bare repositories and
`.git` entries without checkout metadata refuse with
`mdkg needs a checkout with .mdkg/`; nested metadata never selects an
enclosing checkout's store. Linked checkouts of bare repositories work.
Captured Git overrides that resolve elsewhere return warnings naming the
common directory mdkg uses. Unresolvable overrides also warn; neither
changes the selected local file. CLI and doctor consumers must display
these warnings. The engine takes captured environment inputs and reads no
ambient environment itself.

#### 2.2.3 Built-in defaults for 0.7.0

These are the context-free defaults in the
[versioned snapshot](../crates/mdkg-format/src/settings/defaults-0.7.0.json):

```json
{
  "tool": "mdkg",
  "work": {
    "status_enum": ["backlog", "blocked", "todo", "progress", "review", "done"],
    "priority_min": 0,
    "priority_max": 9,
    "next": {"status_preference": ["progress", "todo", "review", "blocked", "backlog"]}
  },
  "templates": {"root_path": ".mdkg/templates", "default_set": "default"},
  "index": {
    "tolerant": false,
    "limits": {"max_files": 100000, "max_file_bytes": 8388608, "max_total_bytes": 536870912, "max_depth": 64}
  },
  "customization": {"skill_mirrors": {"targets": [".agents/skills", ".claude/skills"]}},
  "coordination": {"leases": {"default": "exclusive", "paths": [], "unleased": "warn"}},
  "orchestration": {"branch": "agent/{agent}", "worktree": ".worktrees/{agent}"},
  "agents": {"defaults": {
    "trigger": "mail",
    "shape": {"kind": "batch", "max": 20},
    "session": "resume",
    "loop_guard": {"turns": 10, "window_secs": 3600},
    "concurrency": null,
    "timeout": null,
    "retry": "none",
    "budget": null,
    "auth": {"prefer": "subscription", "api_key_fallback": false}
  }}
}
```

Conditional compatibility defaults stay with their readers: the implicit
workspace layout, git work storage when neither storage key exists, and
the undeclared-projection set in section 2.1. Fresh init instead writes its
explicit choices, including `work.location: "local"` and `projections: []`.
The skill mirror target list above does not enable those projections.
An explicit project value overrides a user preference even when that
project value matches a built-in default.

#### 2.2.4 Inspecting and editing settings

These read-only commands show the same effective view:

```sh
mdkg config show
mdkg config show --effective
mdkg config show --effective --json
```

The view includes merged values, each leaf's origin, selected source paths,
the defaults version and discovery warnings. File-backed agent definitions
retain their own paths as origins. Source paths name the locations selected
for optional user/local files even when those files are absent. Without a
`.mdkg` directory at the selected root, the view reads only built-in and
user settings: it does not import parent projects or discover Git or a
machine-local layer. If an ancestor project exists, an existence-only check
adds a warning to select it with `--root`. A partial or linked `.mdkg` is refused. Inspection can
show wrong-typed settings for diagnosis; it does not certify that a runtime
consumer accepts them. The conditional compatibility defaults in section
2.2.3 still belong to those consumers.
Runtime-only controls such as `MDKG_LEASE_TTL_MS` are outside this
configuration view.

Settings files must not contain secrets. Both output formats replace leaf
values under credential-like names (including names containing `key`,
`token`, `secret`, `password`, `passwd`, `credential` or `authorization`)
with `[redacted]`; cookie names and exact `auth`, `pat` and `bearer` names
are also covered. Recognizable provider-token and JWT shapes, Bearer/Basic
authorization values, PEM/PGP private-key blocks and password-bearing URLs
are hidden under any name, including in arrays. URL query parameters and
command arguments with credential-like names are covered too, including
percent-encoded query names, `--token=value` and `--token value`. The whole
string leaf is hidden. JSON lists the affected pointers in `redacted`.
The boolean `api_key_fallback` flag (including its null reset), numeric
`session.context_tokens` policy setting, valid auth policies and their null
resets, and ordinary fields beneath object-valued
named agents/workspaces stay visible; malformed scalar or array entries
receive no name exemption. Ordinary prose such as `Basic reviewer for docs`
stays readable.
This is a display precaution, not comprehensive secret detection: field names
and unrecognized strings remain visible, and the files themselves are
unchanged. Do not paste settings output into mail or logs without checking it.

The setter command is not yet connected. Its intended commands require
exactly one writable scope, `--user` or `--local`:

```sh
mdkg config set --user index.tolerant true
mdkg config set --local work.priority_min 4
mdkg config show --effective
```

In this example, `/work/priority_min` resolves to
`4` from the machine-local file. The user preference `index.tolerant: true`
applies only if neither the project nor a higher source sets that field.

`--user` selects the discovered user file and `--local` selects the
common-directory file. Unscoped and project writes are not exposed by this
command. A set changes one source, so a higher-priority source may still
mask the change.
It preserves unrelated keys. Dotted object paths and JSON pointers are
equivalent for ordinary keys: `/work/priority_min` selects the same field
as `work.priority_min`. JSON pointers escape literal `/` as `~1` and `~` as
`~0`. Array elements cannot be edited individually; replace the array.

Values are parsed as JSON when valid, including booleans, numbers, arrays,
objects and `null`; other bare words become strings. Malformed JSON-looking
values are refused. Quote a numeric-looking string as JSON, then quote it
for the shell, for example `'"20"'`.

The implemented editor prepares the candidate without writes and requires
validation before publication. It checks for changed source bytes, inode
or permissions and uses directory-relative atomic replacement. No-op edits
keep the original bytes. Cooperating setters hold advisory locks through
replacement; editors ignoring locks are outside that boundary. If rename
succeeds but durability cannot be confirmed, the error explicitly reports
that configuration was published. Inspect the selected file before retrying.
The command layer must supply the project lease and graph-repair checks and
the relevant graph, launcher and agent validators when it is connected.

The [settings tests](../crates/mdkg-format/src/settings/tests.rs) cover
precedence, origins, input refusals and discovery. The
[graph adapter tests](../crates/mdkg-graph/src/config/layers/tests.rs) cover
identity ordering, alias normalization and typed previews. The
[editor tests](../crates/mdkg-run/src/config/tests.rs) and
[pipeline tests](../crates/mdkg-cli/tests/config_operations.rs) cover scoped
validation and publication using temporary homes and repository metadata;
they do not claim command-dispatch coverage.

## 3. Node files

### 3.1 Where they live

Node files are UTF-8 markdown files, one node per file, anywhere under a
workspace's `core/`, `design/` or `work/` folder (by default `.mdkg/core/`,
`.mdkg/design/` and `.mdkg/work/`), at any depth. A reader MUST find nodes by
walking those folders, and MUST NOT derive a node's id from its file name.
`core/core.md` is not a node.

`new` puts each type in one folder:

| Folder | Types |
|---|---|
| `core/` | `rule` |
| `design/` | `edd`, `dec`, `prd`, `prop` |
| `work/` | `goal`, `loop`, `epic`, `feat`, `task`, `bug`, `spike`, `checkpoint`, `test` |

Agent workflow types (`manifest`, `spec`, `work`, `work_order`, `receipt`,
`feedback`, `dispute`, `proposal`) and `archive` may sit in any of the three
folders.

**Local work** (`work.location: "local"`): `new` puts `task`, `bug`,
`spike`, `test` and `checkpoint` nodes in a folder beside the coordinator
store instead of `work/`, so git never sees them:
`<git common folder>/mdkg/work/root/` for a graph at the top of its
worktree, or `…/mdkg/work/at-<path>/` for one in a subfolder `<path>`
(with `/` written `%2f` and `%` written `%25`). Every worktree of the
repository shares it. The files have the same format, and a reader of a
graph with local work MUST read that folder as part of the root workspace, with
the same rules, ids unique across both places included, except that a
target missing from a node there is a warning (section 5.2, rule 7). Work
refers to design: nodes there name committed nodes, and a committed node
never names one of them (section 5.2, rule 8). A node there is
reported by its path: relative to the graph root when the folder is inside
it (a main checkout, `.git/mdkg/work/root/…`), otherwise absolute. Such a
node is leased and its statuses recorded under `node:<uuid>`, its id's
UUID in lowercase canonical hex (section 4), the same from every worktree
and under any file name, so a rename or a `retype` keeps its lease and
statuses; a file there that names no id is no node and is never leased.
Edits to such a node are written in place, a
whole file at a time (a temporary file, synced, renamed over it, and the
folder synced): no work-type node is ever committed. `retype` never moves a node
between this folder and git.

`mdkg work localize --dry-run` plans conversion from shared work to local
work and prints a SHA-256 plan hash. `--apply <hash>` applies that exact
plan: node bytes and ids stay unchanged, `work.location` becomes `local`,
and one commit records only the config change and removal of those node
files. The five work types move to `<local work folder>/<id>.md`.
Committed references to these nodes refuse the conversion under the same
`edge.private_target` rule as validation. Point those references at a
committed container before planning again. Temporary `review` and `blocked`
values are copied to the local node scopes; the old records remain in the
coordinator history.

Conversion requires a committed, clean `.mdkg`, a single root workspace,
no conflicting leases or other running agent turns, and `.mdkg` and the git
common directory on the same filesystem. Stop writers and use a feature
branch. The prepared graph is swapped in atomically after durable local
copies and a backup exist in Git metadata. Repeating the same `--apply`
finishes an interrupted conversion without replacing changed local files.
An existing local work set from another checkout requires merging that
checkout's conversion, rather than combining divergent copies.
See the [migration guide](migrating-from-0.6.md#making-shared-work-local)
for recovery and rollback. Filesystem interruption tests in
[the implementation](../crates/mdkg-cli/src/work/apply.rs) and
[CLI integration tests](../crates/mdkg-cli/tests/localize.rs) cover this contract.

Scratch files are separate from node files and the coordinator store.
`mdkg scratch path` returns `<git common dir>/mdkg/scratch/<actor>`;
`mdkg scratch path --shared` returns `<git common dir>/mdkg/scratch/shared`.
Every worktree shares these locations, independent of `work.location`.
The queries create no files or folders. Scratch is never part of the graph,
committed definitions or work-log bundles. The `scratch_path` MCP tool
has the same output contract (`mdkg.scratch.path.v1`), with an optional
boolean `shared` argument. [Scratch tests](../crates/mdkg-cli/tests/scratch.rs)
cover the paths, actor precedence, read-only behavior and CLI/MCP parity.

### 3.2 File names

`new` names a node file `<type>-<slug>.md`:

- `<type>` is the node's type, such as `task` or `checkpoint`.
- `<slug>` comes from the title: lowercase it; keep ASCII letters and
  digits; replace each run of anything else with one `-`; trim `-` from both
  ends; cut to 80 bytes, then trim a trailing `-`. An empty slug is `node`.
- When the name is taken in that folder, `new` appends `-2`, then `-3`, and
  so on, before `.md`. `new` never replaces a file: it writes the whole node
  under a temporary name and links it into place.
- `new` mints the id itself (section 4.2). Node refs it was given as
  handles (`--parent goal-4`, `--scope task-19`, `--refs task-19`) are
  written as full ids.

The id lives only in frontmatter. A file MAY be renamed at any time, since no
ref uses a file name. If two machines create the same name, git shows an
add/add conflict. Renaming one of the files resolves it, and no ref changes.

### 3.3 Frontmatter grammar

A node file starts with a line `---`, then frontmatter lines, then a line
`---`, then the markdown body. This is 0.6.0's grammar, unchanged except for
extension keys and the lexical rules. Each rule below is an error when
broken:

- The first line, trimmed, MUST be `---` (`frontmatter.missing_open`), and a
  later line, trimmed, MUST be `---` (`frontmatter.unclosed`).
- No frontmatter line may be blank (`frontmatter.blank_line`). Blank lines
  are fine in the body.
- Each frontmatter line is `key: value`, split at the first `:`, both sides
  trimmed (`frontmatter.not_key_value`).
- A key is a **core key**, `[a-z][a-z0-9_]*`, or an **extension key**
  (section 8) (`frontmatter.invalid_key`). A key appears once
  (`frontmatter.duplicate_key`).
- A value is not empty (`frontmatter.empty_value`). It is one of:
  - `true` or `false`, exactly (lowercase);
  - a list, when it starts with `[`: it MUST end with `]`
    (`frontmatter.bad_list`). The items between are split on commas and
    trimmed. `[]` is empty, and an empty item, as in `[a, , b]`, is an error
    (`frontmatter.empty_list_item`);
  - otherwise a string. There is no quoting or escaping.
- CRLF reads as LF.
- **Trimming** in a format 0.7 file removes ASCII space (U+0020) and tab
  (U+0009) only, from both ends. Anything else, such as a byte-order mark
  (U+FEFF) or U+00A0, is part of the text: a file starting with a BOM has
  no opening `---`. A CR in a frontmatter line other than before its LF
  is an error (`frontmatter.stray_cr`), so every value writes back as it
  reads. 0.6.0 files (section 10) are trimmed as 0.6.0
  does, by ECMAScript `String.prototype.trim`, for reading and migrating
  only. A writer MUST NOT write that form into an 0.7 file.
- **Limits:** the closing `---` is at most line 1,001 (so at most 999 lines
  between the delimiters), 64 KiB per line, 10,000 items per list
  (`frontmatter.too_large`).

Writing:
- mdkg writes new files with LF line endings and keys in its canonical
  order: `DEFAULT_KEY_ORDER` in `crates/mdkg-format/src/frontmatter.rs`
  (`id`, then `legacy_id`, `type`, `title`, and so on), then any other keys
  in byte order.
- An edit to one field (`update`, `done`) rewrites that line in place and
  keeps the file's line endings and key order.
- A reader MUST accept any key order. The canonical order is not required of
  other writers.

### 3.4 Keys

Every node has these:

| Key | Kind | Rule |
|---|---|---|
| `id` | string | `<type>-<uuid>` (section 4) |
| `type` | string | One of the graph's types |
| `title` | string | Not empty |
| `created` | string | RFC 3339 UTC to the millisecond: `YYYY-MM-DDTHH:MM:SS.mmmZ`. Set once, when the node is created |
| `updated` | string | Same form. mdkg rewrites it on every knowledge edit. Never earlier than `created` (`node.bad_date`) |

A timestamp is exactly 24 characters, each field in range (month 01-12, a
day that exists in that month, hour below 24, minute and second below 60;
no leap second).

These are optional:

| Key | Kind | Rule |
|---|---|---|
| `legacy_id` | string | The node's 0.6.0 id, set by `migrate` only (section 4.5) |
| `status` | string | Required on work types (the config's statuses) and `dec` (`proposed`, `accepted`, `rejected`, `superseded`). Files hold only lasting statuses |
| `priority` | string | Work types only. A plain integer, `-?[0-9]+` (`3.7`, `+3` and `2x` are errors), in the config's range. 0.6.0 files read it as `parseInt` does |
| `tags`, `owners` | list | Lowercase |
| `links`, `artifacts` | list | Free-form strings |
| `parent`, `epic`, `prev`, `next` | string | Work types. A node id (section 5) |
| `relates`, `blocked_by`, `blocks` | list | Node ids |
| `refs`, `context_refs`, `evidence_refs` | list | Refs or URIs (section 5). `context_refs` and `evidence_refs` are work-type keys; a proposal's `evidence_refs` is text |
| `scope` | list | Node ids |
| `skills` | list | Kebab-case skill slugs. Work types only |
| `aliases` | list | Search synonyms. Not ids, never resolved as handles |
| `supersedes` | string | `dec` only. The id of the `dec` it replaces |

Goals also take `scope_refs` (a list of refs), `active_node` and
`last_active_node` (refs). Other type-specific keys (goals, loops, agent
workflow types, archives) are listed in the node schema (section 3.5). A key
that is neither in the type's schema nor an extension key is an error
(`node.unknown_key`).

**Timestamps.** mdkg writes `created` and `updated` itself, in UTC. `new`
writes `created` from the time inside the UUID it minted (section 4.2), so a
new node's `created` names the same millisecond as its id. An 0.7 reader
MUST also accept the date-only form `YYYY-MM-DD`, naming a day that exists,
and `validate` warns about it. To compare `updated` with `created`, a date
reads as its first millisecond, UTC: `created: 2026-10-24T10:00:00.000Z`
with `updated: 2026-10-24` is an error, and `created: 2026-10-24` with any
`updated` that day is fine.

### 3.5 The node schema

`crates/mdkg-format/schema/node-0.7.json` is the normative definition of
which keys each type may carry in a format 0.7 file. It is part of this
specification and ships with it. For each type it gives:
- every key the type accepts, with its kind (`scalar`, `list`, `boolean`);
- which keys are required;
- each key's constraint, by name, from the schema's `constraints` table
  (`node-id`, `ref-or-uri`, `timestamp`, and so on), as sections 3.4, 4 and
  5 state them;
- the keys some agent workflow types accept for compatibility with 0.6.0
  (`profile`, `contract_profile` and others), marked `compat`. They are
  ordinary keys of those types.

`legacy_id` is a key of every type (`common_keys`), and extension keys
(section 8) are accepted on every type without appearing in the schema.

- **Validity comes from the schema alone.** A key a type's schema doesn't
  list, and that isn't an extension key, is an error (`node.unknown_key`). A
  value of the wrong kind is `node.wrong_kind`.
- **Templates don't change validity.** A graph's `.mdkg/templates/` only
  seeds the files `new` writes. A key a local template adds is still
  refused. The built-in templates use only keys the schema allows.
- **Changes to the schema are format changes.** Adding a key, a type or a
  kind, or loosening a constraint, makes files that readers of this version
  refuse, so it needs a new `format_version`. To try out a key first, use an
  extension key; `x-mdkg.*` is reserved for that.
- 0.6.0 files (section 10) are checked against their graph's templates, as
  0.6.0 checks them.

## 4. Ids

### 4.1 Form

An id is `<type>-<uuid>`:

```
goal-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b
```

- `<type>` matches `[a-z][a-z0-9_]*`, is at most 32 characters, and is not
  exactly 8 hex digits (which would read as a UUID's first group; section
  6).
- `<uuid>` is an RFC 9562 UUID in lowercase hex, in 8-4-4-4-12 groups, 36
  characters with the hyphens: its version (the 13th hex digit) is 1 to 8,
  and its variant bits are `10` (the 17th hex digit is 8, 9, a or b).
- Ids are written and compared in exactly this form. Uppercase hex, missing
  hyphens and surrounding spaces are not ids, though a handle MAY be typed
  that way (section 6).

### 4.2 The UUID

The UUID is the node's identity. Any RFC 9562 version is an identity, but
mdkg mints only version 7: every new node, and every `init`, gets a UUIDv7.
Other versions exist only on nodes migrated from 0.6.0's identity format v2
(section 10.2), whose UUIDs are version 4. A UUIDv7's 128 bits, most
significant first:

| Bits | Field |
|---|---|
| 48 | `unix_ts_ms`: Unix time in milliseconds |
| 4 | version, `0111` (7) |
| 12 | `rand_a` |
| 2 | variant, `10` |
| 62 | `rand_b` |

- Any machine or agent mints a UUID locally, with no counter, lease or
  coordinator. Ids are unique across graphs and machines, so an id never
  needs qualifying.
- A UUIDv7's bytes sort in time order, and ids sort by UUID (by bytes, for
  any version).
- One process MUST mint strictly increasing UUIDs, even when the clock steps
  back. How it does so (RFC 9562's methods 1 to 3) is up to the minter, and
  `unix_ts_ms` MAY then run slightly ahead of the clock. The bits that are
  not time or counter SHOULD come from a cryptographically secure random
  source.
- The reference mints with the `uuid` crate's `now_v7`, which keeps a
  42-bit counter across `rand_a` and the top of `rand_b`. Its deterministic
  `Minter` (for tests and `migrate`) uses method 1: `rand_a` alone is the
  counter.
- A graph's `graph_id` is a UUID of the same form, with no type: a
  UUIDv7, or a migrated v2 graph's own UUID.
- A UUID's time, where `new` and section 3.4 use it, is a UUIDv7's
  `unix_ts_ms`. Other versions have none.

### 4.3 The type in the id

The type is a hint: the UUID alone identifies the node.

- Two ids with the same UUID and different types name the same node.
- When a node's `type` changes, the type in its id goes stale. `validate`
  warns about a stale type (`node.stale_type`): in the node's own `id`, or
  in a ref or body link to it. A type is stale when it differs from the
  node's `type` field.
- `mdkg retype <node> <type>` sets the node's `type`, and rewrites the type
  in every `<type>-<uuid>` naming its UUID, in every node file of the graph
  (keys and bodies alike, code included). A match is a whole token: the
  type is `[a-z][a-z0-9_]*`, preceded by no letter, digit, `_` or `-`, and
  the UUID is followed by no hex digit or `-`. Each rewritten file gets a
  new `updated`.
- `retype` keeps file names. It checks the whole graph as it would be
  before writing anything, takes a lease on every file it rewrites (another
  actor's lease on one stops it), and commits them once.
- Each file is replaced whole (written beside it, then renamed over it),
  so a crash never leaves a partial file. Under its leases, `retype`
  refuses if a file changed since it read it, or has an unmerged version
  from another worktree (`coord.sync_needed`).
- Before writing, it records its plan (each path and the sha256 of what it
  will hold) in the repository's git folder, synced, and keeps it after the
  change is committed and recorded, marked recorded, so a rerun can record
  it again if a crash lost the coordinator's records. Plans are never
  removed: there is one small file per node and worktree, replaced by that
  node's next retype. If it stops
  part way, the files
  it wrote are left as they are, and their refs still name the node by
  UUID. Running it again rewrites what is still stale, and commits and
  records it together with every file that holds exactly what the plan
  said; other edits stay uncommitted.
- Indexes and maps SHOULD key nodes by UUID.

### 4.4 Uniqueness

Two files with the same UUID in one graph are an error
(`node.duplicate_id`), whatever their types.

### 4.5 `legacy_id`

`migrate` sets `legacy_id` to the node's 0.6.0 id, such as `task-19`,
`rule-guide` or `agent.repo-worker`: a lowercase 0.6.0 canonical or portable
id, never workspace-qualified.

- It is read-only. mdkg never issues one, never changes one, and never writes
  one into a ref.
- Handles resolve it (section 6), so `mdkg show task-19` keeps working after
  migration.
- Two nodes MUST NOT share a `legacy_id` in one workspace.

## 5. Refs

| Form | Example | Means |
|---|---|---|
| Node id | `goal-0192f3a4-…` | A node in this graph |
| Graph ref | `mdkg://0192f3a0-…/goal-0192f3a4-…` | A node in the graph with that `graph_id` |
| Pinned | a node id or graph ref, then `@sha256:<64 lowercase hex>` | That node at one revision |

The `mdkg:` scheme is reserved: a value starting with `mdkg:` (any case) is a
graph ref or an error, never a plain URI.

Which keys take which:

| Keys | Take |
|---|---|
| `parent`, `epic`, `prev`, `next`, `relates`, `blocked_by`, `blocks`, `scope`, `supersedes` | Node ids only: no graph refs, no pins |
| `refs`, and a work type's `context_refs` and `evidence_refs` | Any ref, or a URI with another scheme (`https://…`, `archive://<portable id>`) |
| `scope_refs`, `active_node`, `last_active_node` (goals) | A node id or graph ref, unpinned |

Other `*_refs` keys of loops and agent workflow types are strings the format
does not interpret. `migrate` rewrites any value in them that is exactly a
0.6.0 id of a node in the graph.

- **Graph refs** to this graph's own `graph_id` mean the same as node ids.
  mdkg writes node ids for nodes in this graph.
- **Pins** are reserved syntax in format 0.7: a reader parses
  `@sha256:<64 lowercase hex>` where section 5 allows it and preserves it
  exactly, and a writer never adds or changes one. Format 0.7 defines no
  hash for a pin and never verifies one; a later format version defines
  what it hashes. A pinned ref resolves as its unpinned ref does.
- **Validation.** A node id in any key above MUST name a node in the graph.
  One whose type is stale is a warning, and `retype` fixes it. A graph ref
  to another graph is not checked. Some keys also limit the types they
  name, and `prev` and `next` must agree: section 5.2 gives every check on
  the graph as a whole.
- **0.6.0 refs are gone:** workspace-qualified ids (`root:task-19`) and
  `mdkg://<uuid>/<uuid>` identity refs are legacy input only (section 10).

### 5.1 Links in bodies

A node's body names other nodes with markdown links whose target is a ref:
`[task-19](task-0192f3a4-…)`, or `[the plan](mdkg://<graph_id>/edd-…)`.

- The link text is free; the target is a node id or graph ref, optionally
  pinned.
- Links are found by mdkg's own grammar, a small subset of CommonMark, so
  every implementation finds the same ones:
  - the body is split into lines at LF (a CR before it is dropped);
  - a **fence** opens with a line that, after any leading whitespace,
    starts with 3 or more backticks or tildes (whatever follows), and
    closes on a line that, after any leading whitespace, holds only a run
    of the same character at least as long, then nothing but spaces or
    tabs. Nothing inside is a link, and a fence that never closes runs to
    the end of the body;
  - on any other line, a **code span** opens with a run of backticks and
    closes with the next run of exactly as many on the same line. Nothing
    inside is a link. Code spans never cross lines;
  - a **link** is `](`, then a target of one or more characters with no
    whitespace and no `)`, then either `)` at once, or spaces or tabs, a
    title, optional spaces or tabs, and `)`, all on the same line. A title
    is `"…"`, `'…'` or `(…)`, and ends at the first closing delimiter: no
    escapes, no nesting;
  - there are no backslash escapes, and an image (`![…](…)`) counts as a
    link.

  A link whose target is a node id or a graph ref is checked. Other
  targets (URLs, `#anchors`, legacy ids, anything that doesn't parse as a
  ref, malformed `mdkg:` values included) are skipped. Pins aren't
  verified, and a legacy graph's bodies aren't checked. To show an example
  link without it being checked, put it in a fence or a one-line code span.
  `conformance/v07` case v11 pins these rules.
- `validate` reports a link to a node this graph doesn't have
  (`node.bad_body_link`), and warns about one with a stale type
  (`node.stale_type`). Links into other graphs are not checked.
- `migrate` rewrites a 0.6.0 id mentioned in a body's prose as a link whose
  text is the mention as written, `[task-19](<new id>)`; inside a link's
  target, and inside code spans and fences, as the new id alone. A mention
  inside a word, a path or a URL (`task-12-foo.md`) is left alone.

### 5.2 Graph checks

Once every file reads (sections 3 to 5), a reader checks the graph as a
whole. These rules are normative: a reader built from this section reports
the same errors and warnings as mdkg. `validate` reports them, and mdkg's
writing commands check the graph they would write against them.

1. **Targets.** In `parent`, `epic`, `prev`, `next`, `relates`,
   `blocked_by`, `blocks`, `refs`, `scope`, `supersedes`, a work type's
   `context_refs` and `evidence_refs`, and a goal's `last_active_node`
   (the keys section 5 says take refs), every node id,
   and every graph ref to this graph, MUST name a node in the graph:
   otherwise `edge.missing_target`. URIs and refs into other graphs aren't
   checked. A goal's `scope_refs` and `active_node` follow rules 3 and 4.
2. **`prev` and `next`.** When node A's `next` names node B, B's `prev`
   MUST name A, and when A's `prev` names B, B's `next` MUST name A:
   otherwise `edge.prev_next_asymmetric`, on A. A `prev` or `next` that
   names no node is only `edge.missing_target`. Following `next` from any
   node MUST NOT reach a node already passed: a chain that loops is
   `edge.cycle`, once per loop.
3. **A goal's scope.** Every `scope_refs` entry MUST name a node in the
   graph (`goal.scope_ref_missing`); a ref into another graph isn't
   checked. Each entry MUST name a `loop`, `epic`, `feat`, `task`, `bug`,
   `test` or `spike`: otherwise `goal.scope_ref_wrong_type`, on the goal,
   and that node isn't in the scope. The goal's scope is the nodes its
   entries name, then, below each `loop`, `epic` or `feat` in it, every
   node of those seven types whose `epic` or `parent` names that node, and
   so on down. A node of another type below one (a `checkpoint` whose
   `epic` is a scoped epic, a `goal` whose `parent` is one) is no error:
   it isn't in the scope, and neither is anything below it. A 0.6.0
   graph (section 10) keeps 0.6.0's rule, so the 0.6.0 CLI still reads
   it: there, every node of another type the walk reaches is
   `goal.scope_ref_wrong_type`.
4. **A goal's active node.** An `active_node` that isn't a ref into
   another graph MUST name a node (`goal.active_node_missing`) that is a
   `feat`, `task`, `bug`, `test` or `spike`
   (`goal.active_node_wrong_type`). When the goal's scope holds any node,
   the active node MUST be one of them (`goal.active_node_outside_scope`);
   a goal with no scope may name any such node. Each of these checks
   applies only when the one before it passes, so an active node gets at
   most one of the three.
5. **One active goal.** A workspace MUST NOT hold more than one goal with
   both `status: progress` and `goal_state: active`: otherwise one
   `goal.multiple_active` for the workspace, naming them all. A goal whose
   `goal_state` is `active` with another status doesn't count.
6. **Blocking an ancestor.** A node's ancestors are its `parent` and
   `epic`, theirs, and so on. A `blocked_by` naming one of the node's
   ancestors, or an ancestor's `blocks` naming the node, is a warning,
   `edge.blocks_ancestor`, on the node that holds the key: the two could
   only wait on each other, so the ready set ignores that edge.
7. **Local work.** A node in the private folder beside the store (section
   3.1) is shared by every worktree, while the committed nodes it names
   are per branch, so one it names may be on another branch. Where rule 1
   or section 5.1 finds a missing target from such a node, it is a
   warning, `edge.target_not_on_branch`, not an error. The same target
   missing from a committed node is still an error, except a goal's
   `active_node` or `last_active_node`: these record where work is now,
   so one whose id's type is `task`, `bug`, `spike`, `test` or
   `checkpoint` and that names no node here (a fresh clone holds no
   private nodes) is the same warning.
8. **Work refers to design.** With local work, a committed node MUST NOT name
   a node in the private folder in `parent`, `epic`, `prev`, `next`,
   `relates`, `blocked_by`, `blocks`, `refs`, `scope`, `supersedes`, a
   work type's `context_refs` or `evidence_refs`, or a goal's
   `scope_refs`: otherwise
   `edge.private_target`, on the committed node. git would hold a pointer
   that only one machine's store can follow, so the private node names
   the committed one instead (its `epic`, `parent` or `refs`), and a goal
   scopes the committed epic or feat that holds the work. A goal's
   `active_node` and `last_active_node` (rule 7) and body links are
   exempt. mdkg's writing commands refuse to write such a ref.

9. **Skills.** Every slug in a work type's `skills`, and in a goal's
   `required_skills`, MUST name a skill: a folder `.mdkg/skills/<slug>/`
   holding a `SKILL.md` (or 0.6.0's `SKILLS.md`, with a warning), compared
   lowercased. A folder holding both is no skill (`skill.both_files`).
   Otherwise
   `skill.missing_slug`. A 0.6.0 graph checks `skills` only, as 0.6.0
   does.

`conformance/v07` cases v18 to v24, v27 and v28 pin rules 2 to 6 and 9. Rules 7 and 8 need a
private folder, which the suite's fixtures don't have.

## 6. Handles

A handle is what a person or agent types to name a node. Handles are not part
of the files, but every implementation that takes a node name SHOULD accept
the same handles in the same order, and stop at the first kind that matches
anything (`crates/mdkg-format/src/handle.rs`):

1. **A full id or ref** (section 5), found by UUID. A stale type still
   resolves, and the result says so. A graph ref to another graph resolves
   to that ref, pin included. If no node has the UUID, the handle names
   nothing: resolution does not go on to the later kinds.
2. **A 0.6.0 id**, lowercased, matched exactly against `legacy_id` (or the
   id itself on a legacy graph). On a legacy graph a workspace-qualified
   handle (`root:task-19`) names that workspace's node. On an 0.7 graph it
   matches nothing, since `legacy_id` is never qualified.
3. **A unique UUID prefix**, as a short SHA names a commit: at least 4 hex
   digits, with or without `<type>-` in front, case-insensitive, with
   hyphens either left out or exactly where the full form has them
   (`0192f3a4`, `0192f3a47b2c`, `task-0192f3a4-7b`). A first segment of
   exactly 8 hex digits is the UUID's first group. A typed prefix matches the
   type written in the id, so after a retype only the new type matches.
   UUIDv7's leading digits are time, so ids minted in one burst need about
   12 digits to tell apart.
4. **A title fragment:** each whitespace-separated word of it appears in the
   title as a substring, ignoring case (so `par` matches "parser"). If
   several titles match and exactly one equals the whole fragment, ignoring
   case, that one wins.

An ambiguous handle lists every match. Output meant for machines (`--json`,
MCP) always carries full ids.

## 7. Not in 0.7

- **The alias counter and id allocation:** ids are never allocated, so there
  is nothing to collide on across branches.
- **Workspace qualifiers in refs.**
- **0.6.0's identity v2** (`graph_id` and `node_id` keys in node files, and
  `.mdkg/graph.json`): `migrate` replaces it, keeping its UUIDs (section
  10.2).

## 8. Extension keys

Tools and users MAY add their own keys:
`x-<namespace>.<field>[.<field>…]`.

- `<namespace>` matches `[a-z][a-z0-9-]*`: a tool or company name.
- Each `<field>` matches `[a-z][a-z0-9_]*`.
- A key can't be both a value and the prefix of another key: `x-acme.billing`
  with `x-acme.billing.plan` is an error (`node.extension_key_conflict`).
- `x-mdkg.*` is reserved for core fields being tried out before promotion.
- Examples: `x-acme.review_owner: team-a`, `x-my-co.billing.plan: pro`.

Extension keys are accepted on every type without a schema entry, kept on
every rewrite, and never interpreted by mdkg. Values follow the normal value
grammar. 0.6.0 can't read a file that has one, so mdkg writes none into a
legacy graph.

**Legacy `x_` keys.** 0.6.0 rejects every unknown key, `x_` ones included,
but 0.7's pre-release builds accepted `x_<anything>`. 0.7 readers still read
`x_` keys, and `validate` warns about them. `migrate` rewrites `x_<rest>`:

- Split `<rest>` at its first `_`: `x_acme_review_owner` becomes
  `x-acme.review_owner`.
- With no `_` in `<rest>` (`x_owner`), or when the split doesn't give a valid
  namespace and field (`x__owner`, `x_1co_owner`), `migrate` leaves the key
  as it is and reports it, for a person to rename.

## 9. Log records, anchors and bundles

The reference implementation is `crates/mdkg-log` (`record.rs`,
`bundle.rs`) and `crates/mdkg-coord-sqlite` (`log.rs`).

### 9.1 Records

Each coordinator store keeps one chain of records. Every write that changes
coordination state appends one record, in the same transaction as the
change, so a crash or a failed write leaves both or neither. That covers
claims, renewals, releases, expiries, status marks, `done`, `sync`,
`abandon`, settlement, and message and room events, purges included. Two
writes add records that aren't one change each: an import (a marker and
the imported records) and a checkpoint (section 9.5).

An example, with the body abbreviated:

```json
{"actor":"worker","body":{"command":{…},"journal":41,"outcome":{…}},
 "kind":"lease.claim","prev":"sha256:…","seq":42,
 "subject":["task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b","path:.mdkg/work/task-x.md","lease:lease-7"],
 "ts":"2026-10-24T18:03:11.482Z","v":1}
```

A record is a JSON object with exactly these eight keys:

| Key | Type | Meaning |
|---|---|---|
| `v` | integer | The record format version, `1` |
| `seq` | integer | 1 for the chain's first record, then +1 per record, with no gaps |
| `prev` | string or `null` | The previous record's hash; `null` exactly when `seq` is 1 |
| `ts` | string | When the event happened, as its writer says: the writer's clock for a live record, the original time for an imported one. RFC 3339 UTC to the millisecond, exactly `YYYY-MM-DDTHH:MM:SS.sssZ`. Records are ordered by `seq`, never by `ts` |
| `actor` | string, not empty | Who acted: a coordination actor name, or `mdkg` for the system. A `human:<name>` appears only on the mail commands a human runs (acks, leaving a room) |
| `kind` | string | `<domain>.<verb>`: two or more `.`-separated segments of `[a-z0-9_]` (section 9.2) |
| `subject` | array of strings | What the record is about, in the writer's order (below) |
| `body` | object | Kind-specific (section 9.2) |

- **Subjects.** A node is named by its id (section 4), bare, as in-graph
  refs write it. Anything else is a typed ref, `<type>:<value>`:
  `legacy:<id>` for a node known only by a 0.6.0 id (a store used before
  `migrate`, or imported history); `path:<path from the repository root>`
  for a lease on files; `role:<name>` and `run:<name>` for a role's or an
  agent turn's lease; `node:<uuid>` for a local work node's lease and
  statuses (section 3.1), `<uuid>` being its id's UUID in lowercase
  canonical hex (`node:0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b`), the same from
  every worktree and through a rename or a `retype`;
  `lease:<lease id>`; `changeset:<changeset id>`; `msg:<message id>`;
  `room:<name>`; `store:<store id>`. A writer never puts a legacy id bare.
  Records are never rewritten, so one written before a `retype` keeps the
  old type in its ids: consumers MUST match node subjects by UUID.
- **Hashing.** A record's hash is `sha256:` and the lowercase hex sha256 of
  its RFC 8785 canonical JSON in UTF-8 (`crates/mdkg-format/src/canonical.rs`:
  no floats, no integers beyond 2^53 - 1).
- **Storage.** A record is stored, and exported, as exactly its canonical
  text. A reader MUST reject any other spelling (whitespace, key order, an
  unknown top-level key), so the hash covers exactly what is kept. A record
  doesn't carry its own hash: its successor's `prev` does, and a store may
  index it beside the record.
- **The chain.** `prev` links each record to the one before it, so changing,
  dropping or reordering any record breaks every later link. Its first
  record is a `log.genesis` naming the store, so one store's chain can't
  pass for another's. A store id is 32 lowercase hex digits. A store also
  notes its chain's last seq and hash beside the chain, and checking a store
  compares the two, so a record dropped from its end shows even before
  the next anchor.
- **Versions.** A reader that meets a `v` above the ones it knows MUST stop:
  it can't verify what it can't read. Kinds, and keys inside a `body`, are
  added without a new version; readers keep and hash unknown ones.
- **Not recorded:** reads, inbox cursors, and the store's state snapshot,
  which is a cache of the journal.

### 9.2 Kinds (v1)

| Kind | Written when | Subject | Body |
|---|---|---|---|
| `log.genesis` | The store is created, or upgraded to keep a chain | `store:<id>` | `{store, journal_seq}`: the journal entries before the chain number up to `journal_seq` |
| `log.checkpoint` | A compaction (section 9.5) | none | `{from, to, first, last, count, summary}` |
| `log.import` | Before imported history (`migrate`) | none | `{import_id, count, skipped}`; the next `count` records are the import, each with its own `ts` and `actor`. An imported record whose body's `migrate_digest` the chain already holds isn't written again; `skipped` counts those |
| `lease.claim`, `lease.renew`, `lease.release`, `lease.expire`, `lease.reconcile`, `leaf.register`, `changeset.commit`, `changeset.abandon`, `main.observe`, `request.prune`, `alias.issue`, `work.status` | One per coordinator journal entry: claims and refused claims, renewals, releases, expiries, offline leases reconciled, a node file first seen (`leaf.register`), `done` and `sync` commits, abandons, observations of main, expired claim keys dropped, a 0.6.0-compatible alias issued, status marks | The lease's node, its scope (`path:`, `role:`, `run:` or `node:`) and `lease:`; the changesets | `{journal, command, outcome}`. Only `journal`, the journal entry's number, is part of the format. `command` and `outcome` are that entry's command and outcome as the reference implementation writes them: opaque JSON a consumer MUST NOT rely on, which may change without a new record version |
| `msg.send` | A message is stored (not a retry of the same send) | `msg:<id>`, and `room:<name>` for a room post | `{message_id, context_id, kind, sender, via, room, to, references, digest, size, retention}`. `digest` is the body's; the body never enters the log |
| `msg.ack` | Deliveries change to acked, or acking purges a body | `msg:<id>` for each id the ack named, once each | `{acked: [{message_id, mailbox}], by, via, outcome_digest, purged}`: the deliveries this ack changed and the bodies it purged. An ack's outcome text enters only as a digest |
| `msg.purge` | Bodies are purged under their retention (by whichever command finds them due) | `msg:<id>` for each | `{purged: [message_id], reason: "retention"}` |
| `room.create`, `room.join`, `room.leave` | Room membership changes | `room:<name>` | `{room, by, members}`, `{room, by, added}`, `{room, by, removed}` |
| `legacy.*` | Written by `migrate` from 0.6.0 history | as imported | as imported |

The actor is who ran the command (`--as`, `$MDKG_ACTOR`, or the worktree's
name); failing that, the actor the command names; failing that, `mdkg`.

### 9.3 Anchors

An anchor commits a chain to git: `.mdkg/anchors/<store-id>.json` under the
graph root, one line of canonical JSON and a newline:

```json
{"format":"mdkg-anchor","head":"sha256:…","seq":57,"store":"0a1b…","ts":"2026-10-24T18:03:11.482Z","v":1}
```

- `store` is the store's id (32 lowercase hex digits), `head` the hash of
  the record at `seq` in its chain, and `ts` when the anchor was written.
- A reader MUST reject an anchor that isn't canonical, has a key besides
  these six, a `format` other than `mdkg-anchor`, or a `v` other than 1.
  It accepts the file with or without its final newline.
- A chain **holds** an anchor when it has a record at `seq` whose hash is
  `head`. Every later state of an honest chain holds every earlier anchor.
  A chain that doesn't hold its anchor was rewritten, forked or restored
  from an older copy after it.
- Anchors are written by `mdkg anchor`, by `goal done` itself, and before
  a release tag, not on every commit. The user commits them.
  Each first compacts the chain (section 9.5), so the anchored record is
  the new `log.checkpoint`, or the head when nothing came since the last
  one. `goal done` reports an anchor it couldn't write as a warning: the
  goal is done either way.
- A new anchor replaces the file only when the chain holds the old one,
  unless forced after a deliberate restore.

### 9.4 Bundles

`mdkg export --work` writes a bundle: UTF-8 JSON lines, each ending with
`\n`.

1. The header, in canonical JSON:
   `{"count":N,"format":"mdkg-work-bundle","head":"sha256:…","store":"<id>","v":1}`.
2. Then `N` records, seq 1 to N (N is at least 1: the genesis), each as
   its canonical text.

A bundle **verifies** when the header is canonical with `v` 1 and a store
id; every
record line parses strictly (section 9.1); the first is a `log.genesis`
naming the header's `store`; the records link with no gap; and `count` and
`head` are the chain's. It verifies against an anchor when it also holds
that anchor (section 9.3). `mdkg verify <bundle> [--anchor <file>]` checks
this with no store or repository. A bundle carries no message bodies, only
their digests.

### 9.5 Compaction

At a milestone (an anchor, so also a goal done and a release), the store
appends a `log.checkpoint` summarizing every record since the last one:
`from` and `to` are their seqs, `first` and `last` their hashes, `count`
how many, and `summary` at most 150 words citing both hashes and naming
the kinds and actors it has room for, the most frequent first. With
nothing since the last checkpoint, none is written. 0.7.0 prunes no records. Pruning the raw records a checkpoint covers would need a new
bundle version, since a v1 bundle starts at the genesis.

## 10. Reading 0.6.0 graphs

Until a graph is migrated, 0.7 readers read it as 0.6.0 reads it:

- ids `<prefix>-<number>`, the reserved `rule-guide`, `rule-soul` and
  `rule-human`, and portable ids on agent workflow types and archives;
- refs `<id>`, `<workspace>:<id>`, and identity refs
  `mdkg://<uuid>/<uuid>`;
- `created` and `updated` as `YYYY-MM-DD` only;
- 0.6.0's file names, `<prefix>-<number>-<slug>.md`, with `chk` as the
  checkpoint prefix.

mdkg also reads extension keys there, which 0.6.0 does not. A legacy graph
has no `graph_id`, so no `mdkg://` ref is local to it. mdkg writes only
0.6.0 forms into a legacy graph, so the 0.6.0 CLI keeps reading it.

`migrate` turns a legacy graph into an 0.7 graph in one commit:
- it derives the `graph_id` and each node's UUID (section 10.1), or keeps
  an identity v2 graph's (section 10.2);
- it rewrites `created` to its UUID's time as a timestamp when the UUID is
  a v7 it derived, and otherwise (a kept v2 UUID) to the old `created`
  date at `00:00:00.000Z`; `updated` becomes the later of the new
  `created` and the old `updated` date at `00:00:00.000Z`;
- it sets `legacy_id` and rewrites every ref (section 5) and body mention
  (section 5.1);
- it rewrites `x_` keys (section 8);
- it writes every value in its 0.7 form: a `priority` as the plain integer
  0.6.0 read (`3.7` becomes `3`), and every line trimmed as 0.6.0 trimmed
  it, so no character section 3.3 doesn't trim (a BOM, U+00A0 around a
  delimiter, key or value) is left where 0.6.0 ignored it;
- it checks the whole planned graph under the format 0.7 rules before
  committing, and stops if any file would not read.

### 10.1 Migration identity

Ids from `migrate` are derived, not random, so a dry run and the apply that
follows agree, and every branch of a repository migrates the nodes it shares
with the others to the same ids. Both are RFC 9562 UUIDv7s built the same
way from a time and 10 derived bytes:

- **Derived bytes.** `bits(domain, parts)` is the first 10 bytes of the
  sha256 of the concatenation, for the domain and then each part in order,
  of the string's UTF-8 byte length as an unsigned 64-bit big-endian
  integer followed by its UTF-8 bytes.
- **Layout.** The UUID's bytes are the time's low 48 bits, big-endian
  (bytes 0-5), then the 10 derived bytes (bytes 6-15), with byte 6's high
  nibble set to `0111` (the version) and byte 8's two high bits set to `10`
  (the variant). 74 derived bits survive.

**The graph id.** `graph_id = uuid(root_ms, bits("mdkg-graph/v1", [root, path]))`:
- `root` is the repository's root commit reachable from `HEAD`, as 40
  lowercase hex digits. With several roots, the one with the earliest
  committer time wins, then the lowest SHA in byte order.
- `root_ms` is that commit's committer time, in whole seconds, times 1000.
- `path` is the graph's folder relative to the repository's top, with `/`
  separators and no trailing `/`; empty for a graph at the top.

**A node's UUID.** `uuid(day_ms + n, bits("mdkg-migrate/v1", [graph_id, legacy_id, file]))`:
- `day_ms` is the start, UTC, of the date in the first 10 characters of the
  node's `created`, which MUST be a valid `YYYY-MM-DD` date in or after
  1970;
- `n` is the number after the last `-` of `legacy_id` (`task-19` is 19),
  or 0 when that part isn't all decimal digits (`rule-guide`,
  `agent.repo-worker`). It MUST be below 86,400,000, so the UUID stays
  inside its creation day; a larger number, however many digits, is
  refused, never read as 0;
- `graph_id` is in its hyphenated lowercase form, `legacy_id` as the node
  file writes it;
- `file` is the node file's path inside the graph's `.mdkg/` folder:
  `work/task-19-x.md` for `.mdkg/work/task-19-x.md`, and for
  `projects/x/.mdkg/work/task-19-x.md` in a graph at `projects/x`.
  `migrate` converts only a graph of one enabled workspace, `.mdkg` at
  the graph's root, and refuses any other (`migrate.unsupported`).
- A node whose `created` or number breaks these rules isn't migrated: the
  plan reports it, and the graph is not migrated until it is fixed.

Nodes keep their creation order: by day, then within a day by their old
number. Two nodes with one legacy id in two branches (0.6.0's cross-branch
collisions) get two ids, since their files differ.

**Test vectors.** For `root = 0123456789abcdef0123456789abcdef01234567` and
`root_ms = 1760745600000`:

| Input | Id |
|---|---|
| graph, `path = ""` | `0199f49d-b400-7f84-b960-fb50082705ab` |
| graph, `path = "projects/x"` | `0199f49d-b400-7c37-a863-be5af3275130` |
| node `task-19`, created `2026-10-24`, file `work/task-19-x.md` (graph above) | `01a16b35-0813-739b-92f7-2747dfa64e10` |
| the same, created `2026-10-24T10:00:00.000Z` | `01a16b35-0813-739b-92f7-2747dfa64e10` |
| node `rule-guide`, created `2026-10-24`, file `core/guide.md` | `01a16b35-0800-7c25-b839-45a8d069d1ba` |
| node `agent.repo-worker`, created `2026-01-05`, file `work/sub/agent.md` | `019b8b74-1800-7fe6-a2ad-34345fb113a5` |

### 10.2 Identity format v2

A graph in 0.6.0's identity format v2 has config `schema_version` 2, a
manifest `.mdkg/graph.json` (`format` `"mdkg-graph"`, `format_version` 2,
and the graph's UUID as `graph_id`), and in every node a `graph_id` (the
manifest's) and a `node_id` (the node's UUID, usually version 4). Refs to
its nodes are written `mdkg://<graph_id>/<node_id>`. Only `migrate` reads
it, and it keeps every UUID:

- `id: task-4` with `node_id: <uuid>` becomes `id: task-<uuid>`, with
  `legacy_id: task-4`;
- the manifest's `graph_id` becomes the config's `graph_id`, and the
  manifest is removed;
- the per-node `graph_id` and `node_id` keys are dropped;
- in ref fields (section 5) and bodies, each `mdkg://<graph>/<uuid>`
  becomes `<type>-<uuid>` when it names a node of this graph, and
  `mdkg://<graph>/<type>-<uuid>` when it names a node of another graph that
  the config declares as a subgraph, with the type that graph's own
  migration gives the node. Other keys' values, extension keys included,
  stay as written;
- a ref to a node that only another branch of the repository holds is typed
  from that branch, but this branch has no such node: in a ref field it
  leaves the key and is listed under `# Migrated records` (below) as
  `<key>: <type>-<uuid> (<ref>, on another branch)`; in code it becomes the
  id, and in prose it stays as written. It isn't warned about;
- a ref nobody can type (a UUID no node of this graph, of another branch or
  of a declared subgraph has) is unresolved, and `migrate` warns
  `migrate.unresolved_ref`. In a ref field, the ref leaves the key, and a
  line at the end of the body, under the heading `# Migrated records`,
  lists it: `- Refs to nodes this graph doesn't have, moved here from the
  frontmatter: <key>: <ref>, …` (several keys share the line, joined by
  `; `). In a body's text, it stays as written.

A v2 node without both keys, or whose `graph_id` isn't the manifest's, or
two nodes with one `node_id`, stop the migration. `conformance/identity-v2`
is a fixture of this format.

## 11. Conformance

`conformance/` holds fixture graphs and the outputs they must produce. The
0.6.0 cases stay as the legacy-mode suite (section 10). Format 0.7's cases
are in `conformance/v07/`, with the spec's expected codes, and
`crates/mdkg-graph/tests/format07.rs` (`conformance_v07_cases`) runs them.

## 12. Diagnostic codes added in 0.7

| Code | Severity | When |
|---|---|---|
| `node.bad_legacy_id` | error | `legacy_id` isn't a lowercase 0.6.0 id |
| `node.duplicate_legacy_id` | error | Two nodes in a workspace share a `legacy_id` |
| `node.extension_key_conflict` | error | An extension key is both a value and another key's prefix |
| `node.bad_body_link` | error | A body link names a node the graph doesn't have |
| `node.stale_type` | warning | An id, ref or body link names a type the node no longer has |
| `node.date_only` | warning | `created` or `updated` is a date in an 0.7 graph |
| `node.legacy_extension_key` | warning | An `x_` key |
| `edge.blocks_ancestor` | warning | A node is blocked by one of its own ancestors (section 5.2, rule 6) |
| `edge.private_target` | error | With local work, a committed node names a private work node (section 5.2, rule 8) |
| `edge.target_not_on_branch` | warning | With local work, a private work node names a node this branch doesn't have, or a goal's active node isn't in this checkout (section 5.2, rule 7) |

The graph checks of section 5.2 keep 0.6.0's codes, all errors:
`edge.missing_target`, `edge.prev_next_asymmetric`, `edge.cycle`,
`goal.scope_ref_missing`, `goal.scope_ref_wrong_type`,
`goal.active_node_missing`, `goal.active_node_wrong_type`,
`goal.active_node_outside_scope`, `goal.multiple_active` and
`skill.missing_slug`.

Other 0.7 failures reuse 0.6.0's codes too: a missing target in any ref key is
`edge.missing_target`; a 0.6.0 id is `node.bad_id`; a 0.6.0 ref is
`node.bad_ref`; a bad timestamp is `node.bad_date`; one UUID in two files is
`node.duplicate_id`.

## 13. Changes from 0.6.0

| 0.6.0 | 0.7 |
|---|---|
| `task-19`, allocated by a counter | `task-<uuidv7>`, minted locally |
| Identity v2: `task-4` with `node_id: <uuid>` | `task-<uuid>`, the same UUID |
| File `task-19-<slug>.md` | File `task-<slug>.md`, renamable |
| `chk-7` | `checkpoint-<uuidv7>`, in `checkpoint-<slug>.md` |
| `root:task-19` | Not needed: ids are unique everywhere |
| `mdkg://<uuid>/<uuid>` | `mdkg://<graph_id>/<id>`, optionally `@sha256:` pinned |
| `created: 2026-10-24` | `created: 2026-10-24T18:03:11.482Z` |
| No record of the old id | `legacy_id`, read-only |
| Unknown keys rejected | `x-<namespace>.<field>` extension keys |
| `schema_version: 1` | `schema_version: 3`, `format_version: "0.7"`, `graph_id` |
