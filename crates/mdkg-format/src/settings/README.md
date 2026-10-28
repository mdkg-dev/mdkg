# Settings layers

Internal shared settings engine (`mdkg_format::settings`) for the graph
and launcher. It reads configuration and reports origins; it never writes a configuration,
opens a coordinator store, starts Git or loads credentials.

Precedence is built-in defaults, user `$XDG_CONFIG_HOME/mdkg/config.json`
(when XDG_CONFIG_HOME is nonempty and absolute) or `~/.config/mdkg/config.json`, project
`.mdkg/config.json`, machine-local `<git common dir>/mdkg/config.local.json`,
then explicit flags. Project agent definition inputs can be inserted after
the project config and before the local layer, each with its own source path.
The `settings-fs` feature enables bounded file reads and repository discovery.
Without it, parsing, merging and origins stay available without filesystem
operations. `Environment` takes captured home and XDG paths and optional Git
overrides with their actual cwd; the engine never reads process environment.
Unset, empty or relative XDG input falls back to an absolute home directory.
An absent file at a valid XDG path does not fall back to the home file. With
neither input there is no user layer. All worktrees of a repository find the same
local layer.

Discovery walks physical Git metadata, independently of GIT_DIR and
GIT_COMMON_DIR, matching the work store. Bare repositories, metadata directories
and `.git` entries that do not resolve to checkout metadata refuse with
`mdkg needs a checkout with .mdkg/`; a nested repository never falls through
to its enclosing checkout. Linked worktrees of bare repositories are supported.
The physical common-dir config's bare and worktree settings are inspected
without loading Git's user config or includes. The topology file is bounded
to 1 MiB and its final link is not followed. Captured Git overrides that resolve to a different
common directory return structured `Paths.warnings`, for example
`GIT_DIR points elsewhere; mdkg uses <path>`. An unresolvable override also
returns a warning. Callers must surface these in CLI and doctor output; they
never change which local configuration file is read. Relative Git overrides
use the supplied cwd, not the explicit graph root. The filesystem discovery
tests cover nested/bare boundaries, linked worktrees, overrides and XDG input
with temporary paths only.

Objects merge recursively. Arrays, scalars and null replace the lower
value. Null remains a value for a setting's validator to interpret; it
isn't a deletion. Origins use RFC 6901 JSON pointers for scalar values and
empty containers, including each array element. Replacing a container
removes its old origins. An empty object overlay preserves existing keys.

Only absent optional files are skipped. Other read errors, linked final
files, special files, duplicate keys, non-object documents and oversized
or excessively nested inputs are refused. Reads are bounded to 1 MiB and
64 levels of nesting. No input value is copied into an error message.

[The 0.7.0 defaults](defaults-0.7.0.json) record settings with a context-free
default. Graph identity is supplied by the physical project config and must be
validated by its reader before resolving other layers. The engine refuses
`schema_version`, `format_version` and `graph_id` in any other layer, naming
the source file without echoing its values. Conditional defaults
(workspace layout, legacy work storage and undeclared projections) remain
with their validators; this snapshot doesn't change their compatibility
rules. The CLI and runtime consumers integrate this engine separately.

The graph's `config::layers` adapter captures and validates physical project
identity before reading optional sources. Its `Project::resolve` returns
effective values, origins and Git warnings; `Resolved::config` performs typed
graph validation. Per-file definitions are project patches supplied by their
loader. Work-location aliases normalize within each source before merging, so
a higher layer can replace a lower source's choice. Paths retain the discovered
repository to keep typed work storage and local configuration on the same
common-directory snapshot. Each operation captures again. Existing physical-only
callers change over separately; the adapter never reads ambient environment.

`settings::edit` supplies pure single-key changes and strict JSON value parsing
for scoped setters. The file writer lives in `mdkg_run::config`; it requires a
validation callback and uses directory-relative atomic publication. The graph
adapter previews a replacement in its actual layer without writing that file.
