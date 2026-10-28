# Format 0.7 conformance cases

Fixture graphs for `docs/format-0.7.md`. There is no 0.7 oracle: the
expected codes are the spec's, checked by
`crates/mdkg-graph/tests/format07.rs` (`conformance_v07_cases`).

## Layout

- `config.json`: the 0.7 config every case starts from (schema_version 3,
  format_version "0.7", a fixed graph_id).
- `cases/<name>/.mdkg/`: the case's node files, over `../base`'s
  templates.
- `cases/<name>/expected.json`: `{"errors": [...], "warnings": [...]}`, the
  diagnostic codes `validate` reports, sorted. Warnings list only the 0.7
  codes (`node.stale_type`, `node.date_only`, `node.legacy_extension_key`,
  `edge.blocks_ancestor`),
  so recommended-heading warnings don't pin the bodies.

## Cases

| Case | Covers |
|---|---|
| v01-valid | 0.7 ids, legacy_id, timestamps, graph refs to this graph, URIs, an extension key, a body link, a link inside a code span |
| v02-stale-type | A stale type in the node's own id and in a ref (section 4.3) |
| v03-legacy-id | A 0.6.0 id in an 0.7 graph |
| v04-missing-ref | `refs` naming a node the graph doesn't have |
| v05-duplicate-uuid | One UUID in two files with different types |
| v06-extension-conflict | `x-acme.billing` beside `x-acme.billing.plan` ([format-0.7.md §8](../../docs/format-0.7.md#8-extension-keys)) |
| v07-legacy-forms | Date-only timestamps and an `x_` key still read, with warnings |
| v08-bad-body-link | A body link to a missing node |
| v09-bad-timestamp | A timestamp naming a day that doesn't exist |
| v10-legacy-ref | A workspace-qualified 0.6.0 ref in an 0.7 graph |
| v11-body-link-grammar | Section 5.1's grammar: code spans, unfinished titles, a space before `)`, an unclosed link, fences, an unclosed fence, images and backslashes |
| v12-strict-whitespace | A byte-order mark before `---`: 0.7 files trim only space and tab |
| v13-strict-priority | `priority: 3.7`: a 0.7 priority is a plain integer |
| v14-v4-id | Version 4 UUIDs (migrated identity v2 nodes) are ids, refs and link targets |
| v15-v8-id | Version 8 UUIDs too: any RFC 9562 version |
| v16-bad-uuid-version | Version 0 isn't an RFC 9562 version |
| v17-opaque-refs | A loop's `scope_refs` and a proposal's `evidence_refs` are text the format doesn't interpret (the node schema's `text`) |
| v18-goal-scope-type | Section 5.2, rule 3: `scope_refs` naming a `dec` is the wrong type; naming an epic with a task under it is fine |
| v19-prev-next-asymmetric | Rule 2: a `next` with no matching `prev` back, and a `prev` with no matching `next` |
| v20-prev-next-cycle | Rule 2: a symmetric `prev`/`next` chain that loops |
| v21-goal-active-node | Rule 4: an `active_node` outside the goal's scope, and one of the wrong type, which is only that; with no `scope_refs`, any task is fine |
| v22-multiple-active-goals | Rule 5: two goals with `status: progress` and `goal_state: active` |
| v23-blocks-ancestor | Rule 6: a task blocked by its own epic, and an epic whose `blocks` names its child, are warnings |
| v24-goal-scope-skips | Rule 3: checkpoints and goals under a scoped epic, loop or feat are skipped, with what's below them, and are no error; an `active_node` below a skipped one is outside the scope |
| v25-updated-before-created | Section 3.4: `updated` earlier than `created` is an error, by a day or within one (`updated: 2026-10-24` after `created: 2026-10-24T10:00:00.000Z`); a date-only `created` reads as its first millisecond |
| v26-bad-date-only | Section 3.4: a date-only `created` must name a day that exists |
| v27-active-goal-not-in-progress | Rule 5: a goal with `goal_state: active` but `status: todo` doesn't count beside one in progress |
| v28-required-skills | Section 5.2, rule 9: a goal's `required_skills` names a skill that exists, in another case, and one that doesn't |
