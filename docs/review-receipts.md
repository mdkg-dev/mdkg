# Recording a reviewer's requirement verdict

Reviewer profiles stay read-only on Claude, Codex and OpenCode, with one
exception: the reviewer's own verdict on a requirement. The `req_verdict` MCP
tool is offered to reviewers only, and records a requirement `met` or `unmet`
in the reviewer's own name:

```json
{"node": "task-fix-login", "req": "R1", "state": "met", "at": "<reviewed-sha>", "text": "Login retries once."}
```

It sends one `review.verdict` op through `entry update`, and no other op can
pass through the tool. The ledger accepts the op only from an actor whose
committed definition has `role: "reviewer"`. A review round on the node must
also name that actor, be still open (not closed), and have reviewed exactly
`at`. A review request sent with `--node` and `--range` opens that round; a
request without them opens none, so `req_verdict` is refused for its
reviewer. A retry of the verdict already recorded is a no-op. `verified.by`
and the history event both name the reviewer. `entry_list` and `entry_show`
are read tools for every role. `entry_update` stays off for reviewers, and
`req_verdict` is off for workers.

When a reviewer couldn't record its verdict, an outside-turn coordinator can
relay it through the existing `entry update` operation. Send the review request with `--node` and `--range`
so it opens a review round. Name the reviewer directly. The launcher imports the
reviewer's final answer as a reply to that request and privately attests the
captured output's digest, actual turn id, direct reviewer and assigned round
head. The coordinator then reads
the verdict and supplies one typed op per requirement:

```json
{"op":"review.receipt","round":"V1","reviewer":"reviewer","message":"<imported-message-id>","at":"<reviewed-sha>","req":"R1","state":"met"}
```

Save the op as a line in a JSONL file, then run:

```sh
mdkg entry update .mdkg/work/fix-login.md --op-file /tmp/review-receipts.jsonl --json
```

Use `unmet` when the reviewer found the requirement unmet. `waived` is refused.
The op also accepts `text` for a short note. It is available through the same
entry-update CLI, MCP and HTTP surfaces as other ledger ops; it does not require
impersonating the reviewer with `--as`.

The ledger checks that the caller is the node's coordinator outside a launcher
turn, that the round belongs to the node and names the reviewer, and that the
reviewer has a committed reviewer definition. The full resolved SHA must equal
the round's reviewed head. The message must be an imported `findings`, `note` or
`notice` from that reviewer, replying to the round's node-bound request, with
the installed launcher's private turn attestation. Ordinary `msg import` calls
cannot create that provenance, even when they claim the reviewer's name. Old
plain imports are ineligible and cannot be backfilled by actor name. Missing
or mismatched bindings refuse the op without changing the requirement. Role
mailboxes are not historical reviewer identity: assign a direct reviewer name.

The coordinator interprets the verdict; message prose never grants permission.
The ledger does not parse arbitrary prose into requirement states. The receipt
retains the reviewer, recorder, review round, message id, SHA and state. Immutable
mail metadata and the round's reserved provenance remain sufficient after a body
expires or is acknowledged and purged. Ordinary and digest turns share this
attestation path; no public CLI flag, ledger op or MCP argument can invoke it.

`entry show` exposes `review_receipt` on relayed requirements. A met requirement's
`verified.by` is the reviewer; the history event's `by` remains the coordinator,
and its message is the cited imported verdict. The receipt also exposes the
attested `turn` and `digest`. Unmet receipts retain the same
provenance without a `verified` marker. Repeating an identical receipt is a no-op;
a later verdict can refresh the receipt at its reviewed head. An ordinary direct
requirement update replaces the relay provenance. Recording a requirement receipt
does not complete or otherwise change the review round itself.
