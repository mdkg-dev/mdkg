# Delegated decisions

mdkg lets a locally configured delegate relay a human's approval for one action
on an exact committed node version. It is an advisory boundary against
same-user processes, not authentication of the human or of the calling process.

Delegation belongs in the user settings file (`$XDG_CONFIG_HOME/mdkg/config.json`,
or `$HOME/.config/mdkg/config.json`) or the machine-local file
`<git-common-dir>/mdkg/config.local.json`:

```json
{
  "authority": {
    "delegates": {
      "orchestrator": {"for": "human:alice", "may": ["decision"]}
    }
  }
}
```

Project settings never grant delegation. A user settings file inside a Git
worktree or Git metadata is ignored. Machine-local grants replace user grants
for the same actor; an empty `may` or a null actor entry revokes that actor.

The configured delegate sends `msg send --kind decision --action <action>
--ref <node> --to <actor>` outside an agent turn. The actions are fixed:
`implement`, `test`, `commit`, `design`, `review`. The referenced node and graph
identity must be committed, and the current node bytes must match that version.
Optional message text stays untrusted. MCP posting and message import cannot
issue decisions.

No delegation authorizes push, publication, tags, credentials, data deletion,
security-setting changes or wider permissions. Configuration cannot add actions.

## Checking a decision

The send gate checks the turn environment marker, locked records in discovered
repositories, and launcher process ancestry. A positive turn signal refuses the
send. When a discovered live record cannot be checked because process metadata
is unavailable or incomplete, the decision is refused with a diagnostic;
ordinary launcher turns remain usable.

The prompt helper rechecks current delegation, graph identity, committed node
proof and message digest. It renders only the approved action, trusted delegate
and human names, parsed graph/node identity, content pin and commit. It does not
promote message text, node paths or arbitrary node-type prefixes. Revoked grants
stop promotion; reviewer turns can receive only `review` approvals.

`msg decisions` is a durable audit of send-time metadata. Its
`outsideAgentTurn` field means that the advisory CLI gate passed, not that the
caller was independently authenticated. An audit row does not prove that a
launcher promoted it. The launcher revalidates each new turn and records the IDs
it actually promoted in the turn's report (`approved_decisions` in `agents up`
output); retained interruption context does not carry old approval forward.
The trusted section goes in the launcher's own instructions, before every
message body. When an OpenCode prompt can't hold it whole, the launcher drops
the section and its IDs rather than truncating it, and so does the 16 KiB
section cap for a decision that won't fit. A dropped decision is still shown,
as an untrusted body, and acked; the report lists it in `dropped_decisions`
so the operator can have the delegate send it again.

## Known limits

A process running as the same OS user may deliberately change machine-local
delegation, turn records or the store, forge process observations, or use an
external tool service outside the launcher's ancestry. Harness permissions and
their approval review govern access to these files; delegation is not a replacement
for that isolation. A detached process that sends after its turn has ended has
no live turn record to identify it.

Repository discovery is also local to the target and current directory. Suppose
a worker's live turn is in repository A and `--root` names repository B. Keeping
the current directory in A exposes A's record and refuses the send. But a current
directory outside A, such as `/tmp`, or inside a nested Git repository or
submodule under A, can hide A's coordinator from discovery. With the turn
environment marker cleared and a valid grant in B, such a send can pass. Keeping
`--root` at A still exposes its record and refuses the send. These cross-repository
and delayed-detach cases are accepted limits of the advisory model; there is no
global per-user turn registry, TTY requirement or protected issuance service.

## Evidence

[CLI decision fixtures](../crates/mdkg-cli/tests/decisions.rs) cover trusted
layers, fixed actions, immutable references, revocation, graph binding, fixed
rendering, audit semantics, retries, and ordinary turn-marker/ancestry refusal.
`decision_target_cannot_hide_the_senders_live_repository` pins both the refused
source-visible cases and the allowed outside/nested-directory limits while the
source turn remains live. Submodule discovery has the same nearest-repository
boundary; that variant is not a separate end-to-end fixture.

[MCP fixtures](../crates/mdkg-cli/tests/mcp.rs) cover the specific decision-tool
refusal. [Store fixtures](../crates/mdkg-coord-sqlite/tests/decisions.rs) cover
atomic stamping, retry provenance and additive migration.
`agents_up_executes_a_turn_with_only_the_delegates_decision_trusted` runs
`agents up` with a fake harness. It shows the delegate's decision reaching
the turn's trusted section as the fixed sentence, the harness's own decision
send refused mid-turn, other mail staying untrusted, the refusals for the
other cases, and the audit row. The dry-run fixture
`next_launcher_turn_promotes_only_revalidated_decisions_and_records_ids`
covers Claude, Codex, OpenCode, reviewer restriction and revocation.
`opencode_keeps_the_trusted_section_whole_or_drops_it_and_says_so` pins the
OpenCode size boundary from both sides. agents up reports promoted and
dropped decision ids in its turn report (`approved_decisions`,
`dropped_decisions`); turn-end receipts are planned for a later release.
