# Scoping test runs with `mdkg affected`

`mdkg affected` reads the paths that differ between two commits and
says which tests they need. When it can't account for a change, it says to
run everything. Use it to keep test runs small while you iterate, then run
the full suite before you ask for a merge.

```sh
mdkg affected --base <rev> [--head HEAD] [--main <ref>] [--targets] [--json]
```

The selection is based on committed paths, explicit config rules, and Go/Rust
dependency graphs. Graph documents in format 0.7 are ordinary changed paths: a
rule may select their checks, and an unmapped document requests a full run.
The 0.7.0 fixture checks are in
[`crates/mdkg-cli/tests/affected.rs`](../crates/mdkg-cli/tests/affected.rs):
`v07_graph_document_rules_select_checks` and `unmapped_v07_graph_document_is_full`.

## Merges of main

A branch that merges main gets every file main changed since `--base` in
its diff. Main's own runs already tested those changes, so `affected`
skips a changed path whose content at the head is exactly what main had
where the two last met: its mode and blob at the head equal those at a
merge base of the head and main, or it is absent at both. A rename or copy
is skipped only when both its paths match. Skipped paths leave `paths` and
are listed in `data.from_main` (path, status, old path), and `data.main`
names the ref, its commit and the merge bases, so a reviewer can audit the
selection. A path the branch changed itself, including a conflict
resolution that differs from main's version or a mode change, stays in
`paths` and is judged as before; a global file the branch changed still
makes the run full.

Main is `--main <ref>` (any revision, such as `origin/main` when main
lands as a pull request on a host and the local branch hasn't been
updated), else the branch `git config mdkg.mainRef` names (read from the
repository's own config file only, never the environment or a worktree's
config), else the branch `main`. When none of them resolves, nothing is
skipped, and `data.main` is absent.

Under `--targets`, the verified mode `scripts/check.sh --affected` uses,
only an explicit `--main` naming a branch or remote-tracking ref
(`refs/heads/*`, `refs/remotes/*`) skips anything, so no ambient setting
can shrink a verified selection: without `--main` the answer is the one
without the rule, and a SHA or tag is refused in `data.main.detail`.
`scripts/check.sh --affected <base> --main main` passes it through and
prints `affected main:` with the ref, its commit and the `from_main`
count.

mdkg never requires a main branch. Any doubt skips nothing and `data.main.detail` says why: an
explicit ref that names no commit, a shallow clone, no shared history, a
git error, or a head that is on main itself (main's own range is never
skipped, since main's runs are what the rule relies on). With a stale
local main, main's newer changes don't match, so they are judged as the
branch's own and the run is less selective.

What a selective run then misses is the combination: main's change next
to the branch's own, or a branch that sets a path back to main's version
(it counts as main's). A full run of the merged result, at integration,
covers it. `a_lane_merging_main_stays_selective_for_its_own_change` and
the other `*main*` cases in `crates/mdkg-cli/tests/affected.rs` cover
the rule, its conflict and mode cases, the no-main fallback, the
`--targets` rule and the config-only default.

## On a long-lived branch

A branch that hands off its commits one at a time (a lane) runs
`affected --base <the head its last handoff named>`, not the merge base
with main: that merge base can be far back and selects the whole
workspace. Then:

- Run the Rust plan, `data.selected.rust_targets[].cargo_args`
  (deduplicated), not `cargo test -p` on each of `data.selected.rust`:
  that list names the packages for older consumers, and a target rule can
  select one test of a package. A prose edit under `.mdkg/` selects only
  `--test commands_doc --test schemas` of `mdkg` here, although `rust`
  says `mdkg`. Text output prints the plan as `cargo:` lines.
- Run tests with `TMPDIR` reached through a symlink, as
  `scripts/check.sh` does, so a path bug only a symlinked folder shows
  fails on the branch, not first at integration. Keep the base short:
  a Unix socket path must fit in 104 bytes on macOS, and macOS's default
  `TMPDIR` under `/var/folders` leaves too little room.

  ```sh
  t=$(mktemp -d /tmp/mck.XXXXXX) && mkdir "$t/real" && ln -s real "$t/linked"
  TMPDIR="$t/linked" cargo test <the plan's arguments>
  ```

## In a review chain

After you commit a fix and before the next review round:

1. Pick the chain's base: the parent of the chain's first commit.
2. Run:

   ```sh
   mdkg affected --targets --base <chain-first-commit>^ --json > /tmp/affected.json
   ```

3. Check `.ok` first. An error has `ok: false`, no `data`, and a nonzero
   exit: it provides no verified target selection. Read the diagnostic.
   - For `config.invalid` naming an opt-in or `required-features` target,
     correct the rule or manifest and retry. A default full-workspace run
     skips those targets too, so it cannot substitute for correcting that
     unsupported selection.
   - For an unverifiable revision or missing metadata, fetch or repair the
     required local inputs and retry, or perform the repository's complete
     full-suite verification separately.

   On success, read
   `data.mode`:
   - `full`: run the whole suite. `data.reasons` says why (a global file, a
     path nothing maps, ...).
   - `selective`: run only what `data.selected` lists. Run each command
     only when its list is non-empty: `go test` with no packages tests the
     current folder, and `cargo test` with no `-p` tests the whole
     workspace.
     - Go: `data.selected.go` has one entry per module. For each, from its
       `dir` (relative to the repository's top), run `go test -count=1`
       on its `packages`:

       ```sh
       jq -c '.data.selected.go[]' /tmp/affected.json | while read -r m; do
         (cd "$(jq -r .dir <<<"$m")" && go test -count=1 $(jq -r '.packages[]' <<<"$m"))
       done
       ```

     - Rust (older builds): when `jq -e '.data.selected.rust | length > 0'` holds,
       `cargo test $(jq -r '.data.selected.rust[] | "-p " + .' /tmp/affected.json)`.
     - Rust target plans: request `--targets` before using
       `data.selected.rust_targets`, then run each target with nextest from
       the repository root: `cargo nextest run -p <package> --test <name>` for
       a `test` target, `cargo nextest run -p <package> --bin <name>` for a
       `bin` target, and `cargo nextest run -p <package> --lib` for a `lib`
       target. An entry with `kind: "package"` runs every target of its
       package: `cargo nextest run -p <package>`. Several scopes also run in
       one invocation: `cargo nextest run -p <package> -E 'kind(lib) or
       binary(<name>)'`, which is what `scripts/check.sh --affected` does for
       the whole plan. nextest runs no doctests, so add `cargo test --doc -p
       <package>` only when the change touches doc comments. A doc example
       can also break when the code it calls changes; the wave check, which
       runs every doctest, catches that. Do not also run
       `selected.rust`: it preserves the package list for legacy consumers,
       which covers the supported default test scopes.
       Empty target lists mean no Cargo invocation. `rust_target_rules_*`
       in the affected fixture tests cover the plan and legacy projection.
       On successful output this build always emits `rust_targets` and
       `warnings`, which may be empty. Their schema fields remain optional
       for compatibility with older output.
     - Suites in `data.selected.suites` are names from the config's rules;
       run whatever your repository means by them.
     - An empty selection means only paths that need no tests changed.
4. Give the reviewer the same selection, so the review runs the tests
   that matter.
5. Before asking for a merge, run the full suite anyway
   (`scripts/check.sh` here).

`--targets` is the explicit strict mode for consumers of the target plan.
Unresolvable revisions, incomplete ancestry, partial clones, unreadable
configuration or snapshots, and unavailable required metadata/adapters fail
with `ok: false`, no `data`, a named diagnostic and a corrective hint. A
verified global or unmapped change can still request a full run. Without
`--targets`, the shipped v1 full-run fallbacks remain unchanged. The
`rust_target_mode_requires_local_revisions_even_without_rules` fixture and the
partial-clone, snapshot and metadata fixtures check both modes.

It reads only the two commits, never your checkout: uncommitted, staged
or untracked files don't count, so commit first to include them. The Go
and Rust graphs come from the head's tree, written from git's objects into
a private temporary folder that's removed afterwards. A head with a
symlink, or with paths a disk could merge (differing only in case, say),
isn't written, and the answer is `full`. Nothing is downloaded: `go list`
runs with no go env file (`GOENV=off`), modules on, `GOPROXY=off`, no
private-module exceptions, `GOTOOLCHAIN=local` and no VCS stamping, with
a private `HOME` with go's telemetry off and your module cache passed on
(a Go installed by a version manager that looks under `HOME` won't run
there, so the answer is `full`), and lists packages as linux/amd64 (v1) with cgo on, on every
machine, and `cargo metadata` with `--offline`
and no rustup auto-install, so a module cache or toolchain you don't have
makes the answer `full` (`adapter_failed`).

The snapshot goes in the first of these that takes a new folder, so a
read-only sandbox (a Codex reviewer's, say) still gets a selection:

1. the temporary folder (`$TMPDIR`);
2. the per-user cache: `$XDG_CACHE_HOME/mdkg/affected`, else
   `~/Library/Caches/mdkg/affected` on macOS and `~/.cache/mdkg/affected`
   elsewhere;
3. `$CARGO_TARGET_DIR/mdkg-affected`: a sandbox that lets cargo build
   grants its target folder, often as the only writable one.

`data.snapshot.place` names the one used (`temp`, `user_cache` or
`cargo_target_dir`) and `data.snapshot.skipped` lists each place passed
over with why; the text output prints `snapshot:` lines only when a place
was skipped. When the snapshot isn't written the answer is `full`, with an
`adapter_failed` reason, `place` is `null`, and `data.snapshot.error` says
why: every place refused a folder (each is in `skipped`), the head's
tree was refused before any place was tried (a symlink in it, say;
`skipped` is empty), or writing it failed in the place that took the
folder (a full disk, say; that place isn't in `skipped`, and the folder
is removed). Under `--targets` the command fails with that same
error. A head
with no `Cargo.toml` or `go.mod` needs no snapshot, and `data.snapshot` is
left out.

The committed head's config is checked with mdkg's format/version guard;
an unsupported version or malformed 0.7 graph identity requests a full run
(`config_invalid`). Missing or invalid uncommitted checkout config cannot
override the committed decision. These cases are covered by
`invalid_committed_graph_configs_fail_closed` and
`checkout_config_never_changes_the_committed_decision` in the same test file.

## What it maps

- **Go:** every `go.mod` module (outside `testdata`, `vendor`, and `.` or
  `_` folders). A case variant such as `GO.MOD` is discovered but refused
  before any Go command runs, on every host. A `.go` file maps to its folder's
  package and any packages embedding that file. A package's
  dependents are its importers, transitively, plus every package whose
  tests import one of those. Files build constraints exclude (another
  OS, a tag) count too: their imports are read from their source. A
  folder whose every Go file is excluded (an `integration`-tagged test
  package, the module's own top folder included) makes the answer `full`
  when the change reaches anything it imports. Files marked `//go:build ignore` never build and don't count. Other files map to a package when they sit directly in
  its folder (except a module's top folder), under its `testdata/`, or
  among its embedded files. Removing a path beneath a package with embeds
  requests a full run because the head cannot prove its former owners.
  A build-excluded file importing `embed`, a duplicate package import path
  across modules, or an incomplete package is `adapter_failed`.
- **Rust:** the workspace members from `cargo metadata --no-deps
  --offline`. A file maps to a member when it lies in the folder holding
  one of the member's targets (`src/`, `tests/`, `src/bin/`), is a `.rs`
  file beside a target at the member's top (`build.rs`, a top-level
  `lib.rs`), or is a `.rs` file under the folder of a target outside the
  member (only beside it, when that's the repository's top); a path
  several members claim selects them all. A target whose file is missing
  at the head makes the answer `full`, and so does a non-`.rs` file under
  an outside target's folder (the target can `include_str!` it). A
  `.rs` file another crate's target reaches through a literal `#[path]`
  or `include!` selects that crate too (below); any other way of reading
  another crate's folder needs a rule naming the including crate.
  `cargo metadata` runs with a private `CARGO_HOME`.
- **Rust test files:** a changed `.rs` file that only integration tests
  reach selects just those test targets, with no rule. `affected` reads
  every workspace target's module tree from the head: its `mod`
  declarations (with `#[path]`, and inline modules), and `include!`,
  `include_str!` and `include_bytes!` with literal paths. A test's own
  root selects that test; a shared helper such as `tests/common/temp.rs`
  selects every test whose tree reaches it, in any crate. Dependent
  crates aren't selected, because the file is in no library or binary.
  A `mod` under `#[cfg]` counts, so the reading errs towards more tests.
  A file counts only under its exact tracked spelling, never by asking
  the filesystem: a path a tree names that isn't exactly a tracked path
  (another case or Unicode form, which some volumes open, or a file this
  host can't open) makes the tree uncertain, so the plan is the same on
  every host.
  A tree it can't read soundly (a module a macro names, a `cfg_attr`
  path, an include with a computed path, a `mod` whose file is missing,
  or a `mod` inside a function or macro body) counts as reaching every
  file in the repository, since a computed path can climb out of its
  root's folder; for a file that isn't `.rs`, its crate then runs whole
  beside the file's own. This widening applies only to files the Rust
  adapter maps to a member, by the **Rust** ownership rules above, and
  matching Rust target rules can still narrow a mapped crate's fallback
  ([Rules](#rules)). A file the adapter doesn't map (a crate's
  `README.md`, say, which lies outside its target folders) is left to
  the rules wherever it sits, so a file a test reads needs a rule that
  selects that test; the exception is a non-`.rs` file under the folder of
  a target outside its member, which is `full` (see **Rust** above). The whole-crate fallback stays,
  with the reason in its warning, when a library, binary, example or
  build script reaches the file, when it isn't a `.rs` file (a test may
  read it at run time), when it was deleted, or when no tree reaches it.
  A test that reads a `.rs` file at run time, rather than through `mod`
  or `include!`, isn't seen by this reading: give that file a rule naming
  the test.
  `rust_test_files_select_the_tests_whose_module_trees_reach_them` covers
  these cases.
- **Rust manifests:** before Cargo runs, every committed `Cargo.toml` is parsed
  with decoded TOML keys, including Cargo's legacy `project`,
  `dev_dependencies` and `build_dependencies` aliases. Case variants of
  `Cargo.toml` are refused before Cargo runs, even on a case-sensitive host.
  Any file named `Cargo.toml` in any ASCII casing, at any depth, is global,
  including fixtures under `testdata/` or examples under `docs/`. This also
  covers deletion and rename, even when the head has no Rust adapter.
  Patch/replace tables, paths leaving the snapshot,
  malformed manifests, a top-level `.cargo/config`, and toolchain files
  naming a `path` request a full run. A workspace root other than the top
  or a path dependency on any nonmember also requests a full run because
  `--no-deps` cannot show the missing edges.
- **Submodules:** a changed gitlink at either revision requests a full run,
  including deletion, rename and type changes. Rules cannot override this:
  the submodule's own code and tests are absent from the snapshot.

The `review_*` and `round3_*` cases in `crates/mdkg-cli/tests/affected.rs` exercise these
adapter boundaries. The Rust preflight cases use a Cargo sentinel to check
that rejected manifests and toolchain files never invoke Cargo.
`root_manifest_case_variants_override_none_rules_before_cargo` and
`removed_case_variant_manifests_override_none_rules_without_an_adapter` cover
root discovery and removal under a `select: none` rule.
The `round5_*` tests cover Go manifest preflight, built-in global case variants,
and preserved case-sensitive matching for user-defined global globs.

Text output escapes control and bidirectional characters in paths, reasons
and selections. JSON retains the original strings. The
`round3_text_output_escapes_*` cases check both views using committed paths
and manifest values containing terminal controls.

### Known limits

- A cargo config outside the repository that patches dependencies into
  it (yours in `~/.cargo`, or one in a folder above the checkout) changes
  the real build but not the answer; so can a `rust-toolchain.toml` in a
  folder above the snapshot's place (your temporary folder, user cache or
  `$CARGO_TARGET_DIR`).
- A cgo `#include` reaching into another package's folder, or a test
  reading another package's files at run time, needs a rule. A Rust
  `#[path]` or `include!` with a literal path is followed.
- A dependency on a named registry, or a Go module or toolchain you don't
  have cached, makes the answer `full`. Anything else in a crate's folder (a README, a `fixtures/` folder)
  needs a rule. Dependents follow `path` dependencies; dev-dependencies
  end the walk.
- **Global files** run everything: any `go.mod`, `go.sum`, `Cargo.toml`
  (features unify across the workspace) or `Cargo.lock`; at the top level
  only, `go.work`, `go.work.sum`, `.go-version`, `rust-toolchain`, `rust-toolchain.toml`,
  `.tool-versions`, `.cargo/config`, `.cargo/config.toml` and
  `.mdkg/config.json`; anything under `.github/workflows/`; and whatever
  the config adds. Built-in global paths match ignoring ASCII case, including
  directory components, regardless of the host filesystem. User-defined
  `global` globs and rule `paths` still match case-sensitively.
- **Everything else** needs a rule, or the answer is `full`.

## Rules

In `.mdkg/config.json`:

```json
"affected": {
  "version": 1,
  "global": ["scripts/ci/**"],
  "rules": [
    {"paths": ["**/*.md", "docs/**"], "select": "none"},
    {"paths": ["migrations/**"], "select": {"go": ["./internal/repositories"], "suites": ["migrations"]}},
    {"paths": ["api/**/*.proto"], "select": "all"}
  ]
}
```

- `paths` are globs relative to the repository's top, matched against the
  whole path and case-sensitively: `**` as a whole segment matches any
  number of folders (zero included); `*` and `?` stay within one folder
  (so `a**b` is `a*b`); everything else matches itself. There are no
  escapes, classes or braces. A leading `/` or `\`, a trailing `/`, or
  an empty, `.` or `..` segment is invalid.
- `select` is `all`, `none` (no tests needed), or named targets: Go
  package patterns (`.`, `./...`, `./dir`, `./dir/...`) that must match a
  package folder `go list` found, workspace crate names, and suite names
  (letters, digits, `.`, `_` and `-`). Targets are taken as named, with
  no dependents added.
- Rust target rules add `rust_targets` beside `rust`, without changing
  `affected.version` or the `mdkg.affected.v1` envelope. For example:

  ```json
  {"paths": ["crates/cli/src/query/**"], "select": {"rust_targets": [
    {"package": "cli", "test": "query"},
    {"package": "cli", "lib": true}
  ]}}
  ```

  A target rule asserts that only the named targets exercise those paths:
  list every integration, `lib` and `bin` target that covers them, or use
  `rust` for the whole package. `lib: true` excludes documentation tests;
  use `"rust": ["cli"]` when those are required for the example above.

  A target needs a workspace package and exactly one of `test` (an integration
  target name from Cargo metadata), `bin` (binary unit tests), or `lib: true`.
  Unknown packages (in any target of the rule) and malformed entries fail closed. Cargo metadata is still offline.
  A target the head doesn't have (an older head than the shared config, say)
  is skipped with a `data.warnings` entry, and the rule's other targets still
  run; when none of a rule's targets exist at the head, its packages' whole
  tests run instead.
  A matching target rule narrows that package's tests; every dependent package
  remains selected. A dependent without an explicit target in the matching
  rules runs whole-crate tests. A matching `rust` whole-package rule, or any
  unmapped changed source path in that package, wins over finer targets when
  selections combine. Cargo receives every selected package together to
  preserve feature unification. Filters apply across that package set, so
  the union may also run another selected package's same-named target, even
  when that extra target is unconfigured or marked `test = false`. This is
  known over-selection; configured scopes still receive the checks below. A
  whole-package fallback runs the combined package set without target filters,
  replacing the fine plan conservatively. In 0.7.1, rules cannot select opt-in
  targets marked `test = false` or guarded by nonempty `required-features`.
  Such a rule returns `ok: false`, no `data`,
  a nonzero exit and `config.invalid` with a hint to remove the target entry
  or enable default testing without a feature gate. This validation precedes path selection, so
  neither an unrelated path nor a full-run global file hides the refusal.
  `rust_target_opt_in_rules_are_refused_before_selective_full_and_no_change_decisions`
  covers those cases; `rust_target_opt_in_library_and_binary_rules_are_refused`
  covers library and binary targets.
  `rust_target_feature_gated_rules_are_refused_even_with_default_features` covers
  feature gates, including ones enabled by default. Opt-in support is deferred.
  `rust_target_grouped_invocation_preserves_feature_unified_tests`
  executes the plan and checks feature-gated unit tests still run.
  `rust_target_rules_do_not_drop_uncovered_dependents` and
  `rust_target_whole_crate_fallback_dominates_default_scopes_across_paths` cover
  these safeguards.
- Paths mapped by the Rust adapter but lacking a test-target rule keep the
  whole-crate fallback, unless their test targets can be derived (Rust
  test files, above). `data.warnings` names the path and fallback packages,
  and says why no finer targets were derived; text output prints the
  warning too. This is selective, not a full-run
  reason. Paths no adapter or rule accounts for still request a full run.
- Every matching rule applies, and `all` wins. `none` only says a path
  needs no tests: it never drops an adapter's selection or any reason
  for a full run.
- A target that names nothing (a package that doesn't exist) makes the
  answer `full` when its rule matches a changed path, so a stale rule
  fails closed.
- The section is strict: unknown keys, an empty list, an empty `select`
  object, or a `version` other than 1 make the answer `full`
  (`config_invalid`).
- The config is a global file, so a change to these rules always runs
  everything.

## When it says `full`

| Reason | Meaning |
|---|---|
| `base_missing`, `head_missing` | A revision names no commit (in a shallow clone, fetch it). |
| `base_not_ancestor` | The base isn't an ancestor of the head, or a shallow clone lacks the history. |
| `partial_clone` | Objects may be missing and would be fetched; use a full clone. |
| `global_file` | A file every test depends on changed. |
| `unmatched_path` | Nothing maps the path (add a rule if it needs no tests or names a suite), or it's a `.go` file no package builds here (build constraints, an ignored folder); no rule fixes the latter. |
| `rule_selects_all` | A rule says so. |
| `adapter_failed` | `go list` or `cargo metadata` failed or loaded a package incompletely, or the head's tree couldn't be written faithfully (a symlink, paths a disk would merge, a path git won't check out, over the size caps); the reason has the error. |
| `package_removed` | A deleted package's former importers need a full run. |
| `config_invalid` | The head has no `.mdkg/config.json`, it isn't a supported config object with valid format identity, the `affected` section is malformed, or a matching rule's target names nothing. |
| `unreadable_path` | A path isn't UTF-8, or git reported an unknown status. |

## Repository rules

This repository maps every source file and individual integration test
files in `.mdkg/config.json`. Crate module roots keep whole-package
coverage, and so do mdkg's shared test helpers
(`crates/mdkg-cli/tests/common/`). A new integration test file needs no
rule, since its targets are derived. The binary's credential check
(`no_tracked_line_looks_like_a_credential_on_its_own`) reads every tracked
file at run time, so no selection promises to run it for a given change:
it's a gate of the full check (`scripts/check.sh` and each wave), as the
leak scan is. A newly added module without a rule warns and falls
back; add its integration coverage to the rules as the module grows. Docs and
node changes select `commands_doc` and `schemas`; `docs/concepts.md` and
local templates also select the graph `format07` target, which checks the
generated headings table and template behavior. Local templates additionally
select `affected`, whose private fixture uses the configured active set's
14 supported node-type templates, checks dependency preservation and rejects
malformed task data. Partial overrides use bundled defaults for absent kinds.
Other spec, loop and skill template files are not exercised by this fixture.
Shared schema/default inputs
keep whole-package coverage. The configuration itself remains a global file.
`.config/nextest.toml` selects everything, like `clippy.toml`: it changes how
every test runs.

## Running the gates

To run the local gates with the committed target plan:

```sh
MDKG_BIN=/path/to/pinned/mdkg scripts/check.sh --affected <base-ref>
```

Both modes run the Cargo tests with `TMPDIR` set to a link the script
makes to a folder under the run's temp area, on macOS and Linux alike,
and remove both at exit.

Omit `--affected` for the full gate run. Selective mode keeps format and
source/script checks, lints the selected packages, and runs one grouped
`cargo nextest` invocation with four build jobs and four test threads, plus
`cargo test --doc` for a whole-package scope. An empty Rust selection launches
no Cargo test or clippy process; formatting still runs. The full run uses
cargo-nextest for the tests and `cargo test --workspace --doc` for the
doctests; a missing cargo-nextest stops the gate with an install hint.
Affected mode pins the checkout, ignores inherited Git routing, refuses
hidden index flags and compares tracked working bytes with HEAD. It repeats
the check after execution and prints fallback warnings. These are pre/post
observations: a write restored during the run is not detected. Ignored files,
including ignored build configuration, are not verified. The raw comparison
assumes working bytes match committed blobs; clean/smudge filters or line-ending
conversion that change those bytes cause a refusal. No filters are executed.
The gate and its working helpers are trusted programs for one user and one
writer. Owned Git administrative configuration is trusted; includes are not
isolated, while external excludes and attributes files are disabled. Concurrent
same-user replacement of the plan or Git administration is outside this model.
Tracked inputs are opened through no-follow handles for every parent component.
Git is pinned once outside both the source checkout and fixture directories;
Python starts from a trusted system location before PATH is filtered.
Metadata directory paths resolve symlinks before parent components such as `..`.
The guard walks that real path through no-follow handles and checks the opened
directory's device/inode identity and ownership. Metadata files must be owned,
bounded regular files with one link. A same-user swap between validation and
Git's own open remains outside this pre/post check. Directory ancestors must be
readable: a searchable but unreadable parent, such as mode 0711, can be refused
even when Git can traverse it.
A `full` decision runs every local gate and reports platform, quint, release
and scripts checks as outstanding with exit 3, even without explicit suite labels. An
invalid or failed affected report stops
the script. The pinned executable must support `--targets`; package-only v1
plans run their selected packages together. The lint suite runs Clippy for
its selected packages; a clippy.toml change requests full gates so new crates
are covered automatically. Workflows use the existing static gates.
Selected scripts require their own syntax/self-tests, so scripts, platform
(Ubuntu CI), quint, release and other unhandled suites return an explicit
incomplete result with exit 3 after local checks. This mode is for
iteration; full validation before integration remains required.

The runner checks its argument routing and verification with 40 self-test cases
on every gate run, covering nextest filter scopes, whole-package fallback,
doctests after a failing test run, empty
selections, legacy plans, rejected invalid plans and raw-byte refusal of a CRLF
working copy under `eol=crlf`, symlinked-parent refusal and template-root policy.
A configured template-root change must also update its explicit affected-test
or full-coverage rule; the self-test checks this invariant, so the global
configuration gate catches the mismatch before integration.

`python3 -I scripts/leak-scan.py` finds the Git root and uses
the literal keep/drop/drop_cases paths in `scripts/export-public.policy` when
that private data is present. The exporter uses the same confined policy
reader; revision exports read the policy from that commit, while working
checks use the working policy. Git-backed scans refuse foreign-owned roots
or `.git` entries. The scanner confines candidate reads and rejects special files,
and executes no checkout selection code. Git reads disable configured helpers
and lazy network access. Selection also works before the first commit.
Narrowing applies only to the
scanner's own distribution checkout; external trees are scanned without
applying their policy files, and the output says so. Git lookup ignores
relative and checkout-contained PATH entries. A non-isolated bare invocation
prints the required `python3 -I` command before importing other modules.
In public source trees it scans tracked and non-ignored new working files.
`check.sh` calls the same default. `--root` and `--files0-from -` retain
explicit tree/list selection. Explicit-list paths are relative to `--root`, or
the current directory when it is omitted, and require no Git repository.
The scanner's `--self-test` checks both default
selection paths without reading harness configuration or contacting providers.
