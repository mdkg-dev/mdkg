# Scoping test runs with `mdkg affected`

`mdkg affected` reads the paths that differ between two commits and
says which tests they need. When it can't account for a change, it says to
run everything. Use it to keep test runs small while you iterate, then run
the full suite before you ask for a merge.

```sh
mdkg affected --base <rev> [--head HEAD] [--json]
```

The selection is based on committed paths, explicit config rules, and Go/Rust
dependency graphs. Graph documents in format 0.7 are ordinary changed paths: a
rule may select their checks, and an unmapped document requests a full run.
The 0.7.0 fixture checks are in
[`crates/mdkg-cli/tests/affected.rs`](../crates/mdkg-cli/tests/affected.rs):
`v07_graph_document_rules_select_checks` and `unmapped_v07_graph_document_is_full`.

## In a review chain

After you commit a fix and before the next review round:

1. Pick the chain's base: the parent of the chain's first commit.
2. Run:

   ```sh
   mdkg affected --base <chain-first-commit>^ --json > /tmp/affected.json
   ```

3. Check `.ok` first: a failure (outside a repository, say) has `ok:
   false`, no `data`, and a non-zero exit; run the whole suite. Then read
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

     - Rust: when `jq -e '.data.selected.rust | length > 0'` holds,
       `cargo test $(jq -r '.data.selected.rust[] | "-p " + .' /tmp/affected.json)`.
     - Suites in `data.selected.suites` are names from the config's rules;
       run whatever your repository means by them.
     - An empty selection means only paths that need no tests changed.
4. Give the reviewer the same selection, so the review runs the tests
   that matter.
5. Before asking for a merge, run the full suite anyway
   (`scripts/check.sh` here).

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
  `#[path]` or `include!` reaching into another crate's folder needs a
  rule naming the including crate. `cargo metadata` runs with a private
  `CARGO_HOME`. Before Cargo runs, every committed `Cargo.toml` is parsed
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
  folder above your temporary folder.
- A `#[path]`, `include!` or cgo `#include` reaching into another
  package's folder, or a test reading another package's files, needs a
  rule.
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
