# Format conformance suite

The mdkg file format is the kernel's only compatibility promise (edd-1).
This suite pins it down with small fixture graphs and golden outputs from
the reference implementation, the TypeScript mdkg **0.6.0** CLI.

## Layout

- `base/`: the shared starting point (config and templates from
  `mdkg init --graph-only`).
- `cases/<name>/`: an overlay applied on top of `base/`, plus:
  - `commands`: one mdkg invocation per line.
  - `README.md`: what the case covers.
- `expected/<name>/<nn>.json`: one golden per command, recording
  `{oracle, args, exit_code, stdout, stderr}`. `stdout` is
  `{"json": …}` when the output parses as JSON, otherwise `{"text": …}`.
- `passing.txt`: the goldens mdkg must match today, one `case/nn` per
  line. Anything not listed is pending. `cargo test` fails if a listed golden
  diverges.

## How mdkg is compared

mdkg has its own output contract (edd-4), so it is held to the goldens
by facts, not bytes (`crates/mdkg-cli/tests/conformance.rs`):

- the exit code is identical;
- `list` and `show` return the same nodes with the same field values, and
  mdkg lists them in byte order of their qids;
- `search` finds every node 0.6.0 found, with the same field values. It
  may find more, since 0.7.0 searches bodies too, and it ranks them by
  relevance (edd-6, lane 4);
- errors and warnings are the same codes at the same paths, lines and nodes.
  A table maps each 0.6.0 message to its code; a message the table doesn't
  know fails the test.

Text-mode goldens are compared by exit code only, since text isn't part of
the contract.
- `make_cases.py`: regenerates the hand-written cases.
- `regen.py`: regenerates the goldens with the 0.6.0 oracle.

## Case families

| Prefix | Covers |
|---|---|
| `g` | Frontmatter grammar: delimiters, blank lines, keys, values, lists, CRLF, colons and quotes |
| `n` | Node rules: types, statuses, priorities, dates, ids, lowercase lists, skills, unknown keys, value kinds, decisions |
| `e` | Edges and references, missing targets, duplicate ids |
| `l` | Layout: nested folders, non-markdown files, core docs, files outside `.mdkg` |
| `s` | Sorting and workspaces (0.6.0 sorts with ICU collation; mdkg by bytes) |
| `q` | Search semantics and `show` errors |
| `r` | A small synthetic graph shaped like a real one (two workspaces, a rule, a decision, a design, a goal, tasks, a checkpoint, a skill) |

## Normalization

Goldens are recorded after exactly one transformation: the temporary fixture
root (in any of its spellings, including the macOS `/private` alias) becomes
`<ROOT>`.

Cache state is kept out of the goldens by fixture configuration, not by
editing output. `base/` sets `index.backend` to `json`, and the oracle runs
`mdkg index` before each command, so 0.6.0 never emits SQLite cache warnings.
(With the SQLite backend it warns whenever a graph with errors can't build its
cache.) mdkg has no committed cache and never emits those warnings.

## Regenerating

```bash
python3 -I conformance/make_cases.py .
python3 -I conformance/regen.py
```

Regeneration must be deterministic: running it twice yields identical files.
Tests never run the oracle.

## Out of scope for phase 1

Archive sidecars, bundles, subgraph snapshots, db and work contracts; see the
scope tiers in edd-3.
