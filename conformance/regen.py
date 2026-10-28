#!/usr/bin/env python3
"""Regenerate conformance goldens with the TypeScript mdkg 0.6.0 CLI (the oracle).

Each case in cases/<name>/ is an overlay applied on top of base/. Its
`commands` file lists one mdkg invocation per line (shell-style quoting).
For every command this script assembles a fresh temporary repo root, runs
`mdkg index` first so cache warnings never appear, then records the exit
code, stdout and stderr in expected/<name>/<nn>.json after normalization.

Run with: python3 -I conformance/regen.py [case ...]
Never run from tests: goldens change only when a human regenerates them.
"""

import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
NODE24 = os.environ.get("MDKG_NODE24", "/opt/homebrew/opt/node@24/bin/node")
TS_CLI = os.environ.get("MDKG_TS_CLI", "/opt/homebrew/lib/node_modules/mdkg/dist/cli.js")
ORACLE = "mdkg 0.6.0 (TypeScript)"


def assemble(case_dir: Path, dest: Path) -> None:
    shutil.copytree(HERE / "base", dest, dirs_exist_ok=True)
    for src in case_dir.rglob("*"):
        rel = src.relative_to(case_dir)
        if rel.parts[0] in ("commands", "README.md"):
            continue
        target = dest / rel
        if src.is_dir():
            target.mkdir(parents=True, exist_ok=True)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(src, target)


def normalize(text: str, root: Path) -> str:
    # The real path and the macOS /private alias both map to <ROOT>.
    forms = {str(root), str(root.resolve()), str(root).replace("/private/", "/", 1)}
    # Longest first, so "/private/var/..." is replaced before "/var/...".
    for form in sorted(forms, key=len, reverse=True):
        text = text.replace(form, "<ROOT>")
    return text


def run(root: Path, args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(
        [NODE24, TS_CLI, *args],
        cwd=root,
        capture_output=True,
        text=True,
        env={**os.environ, "NO_COLOR": "1"},
        timeout=120,
    )


def regen_case(case_dir: Path) -> int:
    commands_file = case_dir / "commands"
    commands = [
        shlex.split(line)
        for line in commands_file.read_text().splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    out_dir = HERE / "expected" / case_dir.name
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    for number, args in enumerate(commands, start=1):
        with tempfile.TemporaryDirectory(prefix="mdkg-conf-") as tmp:
            root = Path(tmp)
            assemble(case_dir, root)
            run(root, ["index"])
            result = run(root, args)
            stdout = normalize(result.stdout, root)
            try:
                parsed = json.loads(stdout) if stdout.strip() else None
                stdout_value = {"json": parsed}
            except json.JSONDecodeError:
                stdout_value = {"text": stdout}
            record = {
                "oracle": ORACLE,
                "args": args,
                "exit_code": result.returncode,
                "stdout": stdout_value,
                "stderr": normalize(result.stderr, root),
            }
            path = out_dir / f"{number:02d}.json"
            path.write_text(json.dumps(record, indent=2, ensure_ascii=False) + "\n")
    return len(commands)


def main() -> None:
    cases_root = HERE / "cases"
    names = sys.argv[1:] or sorted(p.name for p in cases_root.iterdir() if p.is_dir())
    total = 0
    for name in names:
        count = regen_case(cases_root / name)
        total += count
        print(f"{name}: {count} command(s)")
    print(f"regenerated {total} golden(s) with {ORACLE}")


if __name__ == "__main__":
    main()
