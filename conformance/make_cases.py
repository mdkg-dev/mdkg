"""Generate the hand-written conformance cases into conformance/cases.

Run with: python3 -I conformance/make_cases.py . (from the repo root), then
regenerate goldens with conformance/regen.py. Every case, r01-synthetic
included, is generated here from invented content.
"""
import json
import shutil
import sys
from pathlib import Path

ROOT = Path(sys.argv[1])  # mdkg repo
CASES = ROOT / "conformance" / "cases"
D = "2026-10-18"


def node(nid, ntype, title, extra=(), body="# Overview\n\nBody text.\n", status=None, crlf=False, raw=None):
    if raw is not None:
        text = raw
    else:
        lines = ["---", f"id: {nid}", f"type: {ntype}", f"title: {title}"]
        if status:
            lines.append(f"status: {status}")
        lines += list(extra)
        lines += [f"created: {D}", f"updated: {D}", "---"]
        text = "\n".join(lines) + "\n" + body
    if crlf:
        text = text.replace("\n", "\r\n")
    return text


def work(nid, title="Task", **kw):
    return node(nid, kw.pop("ntype", "task"), title, status=kw.pop("status", "todo"), **kw)


def case(name, files, commands, readme):
    d = CASES / name
    if d.exists():
        shutil.rmtree(d)
    for rel, content in files.items():
        p = d / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(content.encode())
    (d / "commands").write_text("\n".join(commands) + "\n")
    (d / "README.md").write_text(readme.strip() + "\n")


W = ".mdkg/work/"
DS = ".mdkg/design/"
STD = ["list --json", "validate --json"]

# ---------------------------------------------------------------- grammar
case("g01-valid-basic", {
    W + "task-1-basic.md": work("task-1", "Basic task", extra=["priority: 3", "tags: [alpha, beta-two]", "owners: [nick]", "links: [https://example.com/a]", "artifacts: []", "refs: [task-2]", "aliases: [basic]"]),
    W + "task-2-other.md": work("task-2", "Other task", status="backlog"),
}, STD + ["show task-1 --json", "show root:task-2 --json"], "A valid task using every scalar and list kind.")

bad = {
    "g02-blank-line": "---\nid: task-1\ntype: task\ntitle: Blank\n\nstatus: todo\ncreated: 2026-10-18\nupdated: 2026-10-18\n---\n",
    "g03-missing-open": "id: task-1\ntype: task\n---\n",
    "g04-unclosed": "---\nid: task-1\ntype: task\ntitle: Unclosed\nstatus: todo\n",
    "g05-no-colon": "---\nid: task-1\ntype task\ntitle: No colon\nstatus: todo\ncreated: 2026-10-18\nupdated: 2026-10-18\n---\n",
    "g06-invalid-key": "---\nid: task-1\nType: task\ntitle: Upper key\nstatus: todo\ncreated: 2026-10-18\nupdated: 2026-10-18\n---\n",
    "g07-duplicate-key": "---\nid: task-1\ntype: task\ntitle: Dup\ntitle: Dup again\nstatus: todo\ncreated: 2026-10-18\nupdated: 2026-10-18\n---\n",
    "g08-empty-value": "---\nid: task-1\ntype: task\ntitle:\nstatus: todo\ncreated: 2026-10-18\nupdated: 2026-10-18\n---\n",
    "g09-list-unclosed": "---\nid: task-1\ntype: task\ntitle: List\nstatus: todo\ntags: [a, b\ncreated: 2026-10-18\nupdated: 2026-10-18\n---\n",
    "g10-list-empty-item": "---\nid: task-1\ntype: task\ntitle: List\nstatus: todo\ntags: [a, , b]\ncreated: 2026-10-18\nupdated: 2026-10-18\n---\n",
}
for name, raw in bad.items():
    case(name, {W + "task-1-x.md": raw}, STD, f"Frontmatter grammar error case {name}.")

case("g11-crlf", {W + "task-1-crlf.md": work("task-1", "CRLF task", crlf=True, body="# Overview\n\nLine one.\nLine two.\n")},
     STD + ["show task-1 --json"], "CRLF line endings parse; the body is normalized to LF.")
case("g12-colon-and-quotes", {
    W + "task-1-colon.md": work("task-1", "Ratio: 3:1 wins"),
    W + "task-2-quotes.md": work("task-2", "\"Quoted\" title, with comma"),
    W + "task-3-spaces.md": node("task-3", "task", "  Spaced title  ", status="todo", extra=["tags: [ a ,b ]"]),
}, STD + ["show task-1 --json", "show task-2 --json", "show task-3 --json"], "Values keep later colons, quotes and commas; lists trim items.")

# ---------------------------------------------------------------- node rules
rules = {
    "n01-unknown-type": work("task-1", ntype="widget"),
    "n02-missing-status": node("task-1", "task", "No status"),
    "n03-bad-status": work("task-1", status="doing"),
    "n04-uppercase-status": work("task-1", status="TODO"),
    "n05-status-on-prd": node("prd-1", "prd", "PRD with status", status="todo"),
    "n06-priority-range": work("task-1", extra=["priority: 12"]),
    "n07-priority-nonint": work("task-1", extra=["priority: high"]),
    "n08-bad-date": node("task-1", "task", "Bad date", status="todo", raw="---\nid: task-1\ntype: task\ntitle: Bad date\nstatus: todo\ncreated: 18-10-2026\nupdated: 2026-10-18\n---\n"),
    "n09-uppercase-tag": work("task-1", extra=["tags: [Alpha]"]),
    "n10-unknown-key": work("task-1", extra=["colour: blue"]),
    "n11-bad-id": work("Task_1"),
    "n12-skills-not-kebab": work("task-1", extra=["skills: [Bad_Skill]"]),
    "n13-priority-on-dec": node("dec-1", "dec", "Dec with priority", status="accepted", extra=["priority: 1"]),
    "n14-list-as-scalar": work("task-1", extra=["tags: alpha"]),
    "n15-scalar-as-list": work("task-1", extra=["parent: [goal-1]"]),
}
for name, content in rules.items():
    folder = DS if any(t in name for t in ("prd", "dec")) else W
    case(name, {folder + "x-1.md": content}, STD, f"Node rule case {name}.")

case("n16-decisions", {
    DS + "dec-1-a.md": node("dec-1", "dec", "Accepted", status="accepted"),
    DS + "dec-2-b.md": node("dec-2", "dec", "Supersedes one", status="proposed", extra=["supersedes: dec-1"]),
    DS + "dec-3-c.md": node("dec-3", "dec", "Rejected", status="rejected"),
}, STD + ["show dec-2 --json"], "Valid decision records and supersedes.")
case("n17-dec-bad-status", {DS + "dec-1-a.md": node("dec-1", "dec", "Bad", status="todo")}, STD, "Decision status outside the decision set.")

# ---------------------------------------------------------------- edges
case("e01-edges", {
    W + "goal-1-g.md": node("goal-1", "goal", "Goal", status="todo", extra=["goal_state: active", "goal_condition: Ship it", "scope_refs: []", "required_skills: []", "required_checks: []", "max_iterations: 25", "blocked_after_attempts: 3"]),
    W + "epic-1-e.md": node("epic-1", "epic", "Epic", status="todo"),
    W + "task-1-a.md": work("task-1", "A", extra=["epic: epic-1", "parent: goal-1", "next: task-2", "relates: [task-3]", "blocks: [task-2]"]),
    W + "task-2-b.md": work("task-2", "B", extra=["prev: task-1", "blocked_by: [task-1]", "context_refs: [epic-1]", "evidence_refs: [task-3]"]),
    W + "task-3-c.md": work("task-3", "C", extra=["relates: [root:task-1]"]),
}, STD + ["show task-1 --json", "show task-2 --json", "show task-3 --json"], "Every edge kind, local and qualified refs.")
case("e02-missing-target", {W + "task-1-a.md": work("task-1", extra=["parent: goal-9", "blocked_by: [task-7]"])}, STD, "Edges to nodes that do not exist.")
case("e03-duplicate-id", {W + "task-1-a.md": work("task-1", "First"), W + "task-1-b.md": work("task-1", "Second")}, STD, "Two files with the same id.")

# ---------------------------------------------------------------- layout and sorting
case("l01-nested-and-ignored", {
    W + "sub/deeper/task-1-a.md": work("task-1", "Nested"),
    W + "notes.txt": "not markdown\n",
    DS + "prd-1-p.md": node("prd-1", "prd", "Product"),
    ".mdkg/core/rule-1-r.md": node("rule-1", "rule", "Rule one"),
    "outside/task-9.md": work("task-9", "Outside the graph"),
}, STD + ["list --type rule --json", "list --type prd --json"], "Nested folders, non-markdown files, core docs, files outside .mdkg.")
case("s01-sorting", {W + f"{i}.md": work(i, f"Sort {i}") for i in ["task-1", "task-2", "task-10", "task-11", "task-100", "bug-3", "spike-1", "feat-20"]} | {DS + "dec-5.md": node("dec-5", "dec", "Five", status="accepted"), DS + "edd-1.md": node("edd-1", "edd", "Design")},
     STD + ["list --type task --json", "list --status todo --json"], "qid ordering for numeric ids and mixed prefixes.")

multi_cfg = json.loads((ROOT / "conformance" / "base" / ".mdkg" / "config.json").read_text())
multi_cfg["workspaces"].update({
    "a_b": {"path": "ws1", "enabled": True, "mdkg_dir": ".mdkg", "visibility": "private"},
    "ab": {"path": "ws2", "enabled": True, "mdkg_dir": ".mdkg", "visibility": "private"},
    "ab2": {"path": "ws3", "enabled": True, "mdkg_dir": ".mdkg", "visibility": "private"},
    "a2": {"path": "ws5", "enabled": True, "mdkg_dir": ".mdkg", "visibility": "private"},
    "off": {"path": "ws4", "enabled": False, "mdkg_dir": ".mdkg", "visibility": "private"},
})
case("s02-workspaces", {
    ".mdkg/config.json": json.dumps(multi_cfg, indent=2) + "\n",
    W + "task-1-r.md": work("task-1", "Root task"),
    "ws1/.mdkg/work/task-1.md": work("task-1", "Underscore workspace"),
    "ws2/.mdkg/work/task-1.md": work("task-1", "Plain workspace"),
    "ws5/.mdkg/work/task-1.md": work("task-1", "Letter-digit workspace"),
    "ws3/.mdkg/work/task-1.md": work("task-1", "Digit workspace"),
    "ws4/.mdkg/work/task-1.md": work("task-1", "Disabled workspace"),
}, STD + ["list --ws a_b --json", "show a_b:task-1 --json"], "Several workspaces; aliases with _ and digits exercise collation.")

# ---------------------------------------------------------------- search and show
case("q01-search", {
    W + "task-1-a.md": work("task-1", "Parser for Frontmatter", extra=["tags: [format, parser]"]),
    W + "task-2-b.md": work("task-2", "Coordinator leases", extra=["tags: [coord]"], body="# Overview\n\nThe word frontmatter appears only in this body.\n"),
    W + "task-3-c.md": work("task-3", "Merkle roots", extra=["aliases: [frontmatter-roots]"]),
}, ["search frontmatter --json", "search \"parser format\" --json", "search FRONTMATTER --json", "search nothing-matches --json", "search frontmatter --type task --limit 1 --json"],
     "Search is an AND of case-insensitive terms over metadata, not bodies.")
case("q02-show-errors", {W + "task-1-a.md": work("task-1")}, ["show task-9 --json", "show other:task-1 --json"], "Showing missing ids and workspaces.")

# ---------------------------------------------------------------- CLI behaviour (review findings 5, 6, 12)
case("c01-cli-flags", {
    W + "task-1-basic.md": work("task-1", "Basic task", extra=["priority: 3", "tags: [alpha, beta-two]", "owners: [nick]", "links: [https://example.com/a]", "refs: [task-2]", "aliases: [basic]", "relates: [task-2]"]),
    W + "task-2-other.md": work("task-2", "Other task", status="backlog", extra=["tags: [gamma]"]),
}, ["list --status TODO --json", "list --ws ROOT --json", "list --tags \"ALPHA, gamma\" --json", "search basic task --json", "show task-1", "show task-1 --meta"],
     "Flag values are case-insensitive, unquoted search words are joined, and show text lists metadata and edges.")
case("c02-non-ascii", {
    W + "task-1-a.md": work("task-1", "Café task", extra=["refs: [abcdé]"]),
}, ["list --json", "validate --json"], "Non-ASCII values are reported, never crash.")
case("c03-show-missing-non-ascii", {W + "task-1-a.md": work("task-1", "Plain")}, ["show ééé --json"], "Showing a non-ASCII id that doesn't exist.")

# ---------------------------------------------------------------- a small real-shaped graph
# r01-synthetic: a real-shaped graph from invented content.
# Invented names, links and bodies, fixed dates; the same four commands.
R1 = "2026-01-15"


def r1node(lines, body):
    return "---\n" + "\n".join(lines + [f"created: {R1}", f"updated: {R1}"]) + "\n---\n" + body


def sections(*names, text="Example text."):
    return "".join(f"# {n}\n\n{text}\n\n" for n in names)


TASK_HEADINGS = ("Overview", "Acceptance Criteria", "Files Affected", "Implementation Notes", "Test Plan", "Links / Artifacts")
r1_cfg = json.loads((ROOT / "conformance" / "base" / ".mdkg" / "config.json").read_text())
r1_cfg["workspaces"]["aux"] = {"path": "aux", "enabled": True, "mdkg_dir": ".mdkg", "visibility": "private"}
case("r01-synthetic", {
    ".mdkg/config.json": json.dumps(r1_cfg, indent=2) + "\n",
    ".mdkg/core/rule-1-example.md": r1node(
        ["id: rule-1", "type: rule", "title: Example writing rule", "tags: [style]", "owners: [example-team]",
         "links: [https://example.com/style]", "artifacts: []", "relates: []", "refs: [edd-1]", "aliases: [writing-rule]"],
        "# Rule\n\nWrite short sentences. Keep the kernel of each idea in its first line.\n"),
    DS + "dec-1-example.md": r1node(
        ["id: dec-1", "type: dec", "title: Example decision", "status: accepted", "tags: [example]", "owners: []",
         "links: []", "artifacts: []", "relates: [edd-1, goal-1]", "refs: [edd-1]", "aliases: []"],
        sections("Context", "Decision", "Alternatives considered", "Consequences", "Links / references")),
    DS + "edd-1-example.md": r1node(
        ["id: edd-1", "type: edd", "title: Example kernel design", "tags: [kernel, example]", "owners: [example-team]",
         "links: [https://example.com/design]", "artifacts: []", "relates: [dec-1]", "refs: []", "aliases: [kernel-design]"],
        # Rollout plan is left out on purpose: one missing-heading warning.
        sections("Overview", "Architecture", "Data model", "APIs / interfaces", "Failure modes", "Observability",
                 "Security / privacy", "Testing strategy", text="An invented design for a small kernel.")),
    W + "goal-1-example.md": r1node(
        ["id: goal-1", "type: goal", "title: Example goal", "status: todo", "priority: 1", "goal_state: active",
         "goal_condition: Both example tasks are done", "scope_refs: [task-1]", "active_node: task-1",
         "required_skills: [example-check]", "required_checks: [the example check passes]", "max_iterations: 5",
         "blocked_after_attempts: 2", "tags: [example]", "owners: []", "links: []", "artifacts: []", "relates: [dec-1]",
         "blocked_by: []", "blocks: []", "refs: []", "context_refs: []", "evidence_refs: []", "aliases: []"],
        "# Objective\n\nFinish both example tasks.\n"),
    W + "task-1-start.md": r1node(
        ["id: task-1", "type: task", "title: Start the example", "status: todo", "priority: 2", "parent: goal-1",
         "next: aux:task-1", "tags: [example, start]", "owners: [example-team]", "links: []", "artifacts: []",
         "relates: [dec-1]", "blocked_by: []", "blocks: [aux:task-1]", "refs: [rule-1]", "context_refs: [edd-1]",
         "evidence_refs: [https://example.com/evidence]", "aliases: []", "skills: [example-check]"],
        sections(*TASK_HEADINGS)),
    "aux/.mdkg/work/task-1-finish.md": r1node(
        ["id: task-1", "type: task", "title: Finish the example", "status: done", "priority: 3", "parent: root:goal-1",
         "prev: root:task-1", "tags: [example, finish]", "owners: []", "links: []", "artifacts: []",
         "relates: [root:dec-1]", "blocked_by: [root:task-1]", "blocks: []", "refs: []", "context_refs: []",
         "evidence_refs: []", "aliases: []", "skills: []"],
        sections(*TASK_HEADINGS)),
    W + "chk-1-summary.md": r1node(
        ["id: chk-1", "type: checkpoint", "title: Example summary", "checkpoint_kind: implementation", "status: done",
         "tags: [example]", "owners: []", "links: []", "artifacts: []", "relates: []", "blocked_by: []", "blocks: []",
         "refs: []", "context_refs: []", "evidence_refs: []", "aliases: []", "skills: []", "scope: [task-1]"],
        sections("Summary", "Scope Covered", "Decisions Captured", "Implementation Summary", "Verification / Testing",
                 "Known Issues / Follow-ups", "Links / Artifacts", text="Invented evidence: the example check passed.")),
    # An agent folder without its skills mirror: 0.6.0 then warns that
    # both configured mirror roots are missing.
    ".agents/README.md": "Agent files for the example graph. The skills mirror is left out on purpose.\n",
    ".mdkg/skills/example-check/SKILL.md": "---\nname: example-check\ndescription: \"Checks that the example tasks (task-1) meet goal-1's condition.\"\n---\n\n# Purpose\n\nAn invented skill for the conformance suite.\n",
}, ["list --json", "validate --json", "show edd-1 --json", "search kernel --json"],
     """A small synthetic graph shaped like a real one: two workspaces (root and
aux), a rule, a decision, a design (the `kernel` search hit), a goal, two
tasks with the same local id in different workspaces, a checkpoint and a
skill whose mirrors are missing. All content is invented, with fixed dates.""")
