#!/usr/bin/env python3
"""Pins the GitHub workflows: every action by commit SHA, and cargo-dist
installed at a fixed version by `cargo install --locked` (crates.io's
checksums) instead of a downloaded installer piped to a shell.

`dist generate` rewrites .github/workflows/release.yml with tag-pinned
actions and a `curl | sh` install, so run this after it.

Usage: scripts/pin-workflows.py [--check | --self-test | --resolve]
  (none)     rewrite the workflows to the pins below
  --check    exit 1 if any workflow uses an unpinned action or installs
             dist any other way (offline; scripts/check.sh runs it)
  --self-test  check the checker against workflows it must pass and fail
  --resolve  look each pin's tag up with `git ls-remote` (network, read
             only) and print the table, so a maintainer can review a
             new tag's commit before pinning it
"""
import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
# Release workflows hold publishing tokens; CI also runs repository code.
WORKFLOWS = [ROOT / ".github" / "workflows" / n for n in ("release.yml", "pypi.yml", "ci.yml")]
DIST_VERSION = "0.32.0"
DIST_INSTALL = f"cargo install cargo-dist --version {DIST_VERSION} --locked"
# action@tag -> the commit that tag named when it was pinned, looked up
# 2026-10-25 with `--resolve` (the peeled commit for an annotated tag).
PINS = {
    "actions/attest@v4": "1e69f48acb82d1966a394da916b4c1698aa569d6",
    "actions/checkout@v4": "11d5960a326750d5838078e36cf38b85af677262",
    "actions/checkout@v6": "d23441a48e516b6c34aea4fa41551a30e30af803",
    "actions/download-artifact@v4": "d3f86a106a0bac45b974a628896c90dbdf5c8093",
    "actions/download-artifact@v8": "9000827ccba6bdab643e8b6fd33ac0654aef8333",
    # v4 and v2 tag commits verified 2026-10-27 on upstream release pages,
    # with a matching read-only --resolve receipt from the orchestrator:
    # https://github.com/actions/setup-node/releases/tag/v4
    # https://github.com/Swatinem/rust-cache/releases/tag/v2
    "actions/setup-node@v4": "49933ea5288caeca8642d1e84afbd3f7d6820020",
    "actions/setup-node@v6": "249970729cb0ef3589644e2896645e5dc5ba9c38",
    "Swatinem/rust-cache@v2": "d1527c8f67ef9af357290475d4af4b0fab8f12e0",
    "actions/upload-artifact@v4": "ea165f8d65b6e75b540449e92b4886f43607fa02",
    "actions/upload-artifact@v7": "cf430e030ddbb5b0abf93d22962f4752f3646cd9",
    "PyO3/maturin-action@v1": "e83996d129638aa358a18fbd1dfb82f0b0fb5d3b",
    # pypa's own advice is a full version tag pinned by commit, not its
    # moving `release/v1` branch.
    "pypa/gh-action-pypi-publish@v1.13.0": "ed0c53931b1dc9bd32cbe73a98c7f6766f8a527e",
}
# Moving refs, replaced by a pinned tag of the same line.
RETAG = {"pypa/gh-action-pypi-publish@release/v1": "pypa/gh-action-pypi-publish@v1.13.0"}

# Any `uses:` line, its value read without quotes or a trailing comment.
USES_ANY = re.compile(r"^(\s*(?:-\s+)?uses:\s*)(.*?)\s*$")
REF = re.compile(r"^([\w.-]+/[\w./-]+)@([^\s@]+)$")
SHA = re.compile(r"^[0-9a-f]{40}$")
DIGEST = re.compile(r"^docker://[^@\s]+@sha256:[0-9a-f]{64}$")
STEP = re.compile(r"^(\s*-?\s*)name:\s*Install dist\s*$")
# A `uses` key in any form; one USES_ANY doesn't read (a flow mapping, a
# quoted key, a space before the colon) fails the check.
ANY_USES_KEY = re.compile(r"""(?:^|[\s{,\[])['"]?uses['"]?\s*:""")
CARGO_DIST = re.compile(r"cargo\s+install\b.*\bcargo-dist\b|\bcargo-dist\b.*\bcargo\s+install\b")
# cargo-dist's generated installs, replaced by DIST_INSTALL.
CURL_DIST = re.compile(r'^(\s*)run:\s+"curl [^"]*cargo-dist-installer\.sh \| sh"\s*$')
MATRIX_DIST = re.compile(r"^(\s*)run:\s+\$\{\{\s*matrix\.install_dist\.run\s*\}\}\s*$")
# The generated install step's comment about `curl`, gone with the curl.
CURL_COMMENT = re.compile(
    r"^\s*# (we specify bash to get pipefail; it guards against the `curl` command"
    r"|failing\. otherwise `sh` won't catch that `curl` returned non-0)\s*$"
)


def uses_value(raw):
    """The value of a `uses:` line, and its trailing comment (a pin's tag)."""
    value, _, comment = raw.partition(" #")
    value = value.strip().strip("'\"")
    return value, comment.strip() or None


def pin_line(line):
    m = USES_ANY.match(line)
    if not m:
        return line, None
    head, raw = m.groups()
    value, comment = uses_value(raw)
    if value.startswith("./") or DIGEST.match(value):
        return line, None
    r = REF.match(value)
    if not r:
        return line, f"{value}: a `uses:` value this script doesn't read"
    action, ref = r.groups()
    key = RETAG.get(f"{action}@{ref}", f"{action}@{ref}")
    action, ref = key.split("@", 1)
    if SHA.match(ref):
        if comment and PINS.get(f"{action}@{comment}") == ref:
            return f"{head}{action}@{ref} # {comment}\n", None
        return line, f"{action}@{ref}: a commit not in the pin table"
    sha = PINS.get(key)
    if not sha:
        return line, f"{key}: no pin (add it to PINS; `--resolve` looks it up)"
    return f"{head}{action}@{sha} # {ref}\n", None


def rewrite(text):
    out, problems = [], []
    for line in text.splitlines(keepends=True):
        if CURL_COMMENT.match(line):
            continue
        if CURL_DIST.match(line) or MATRIX_DIST.match(line):
            indent = re.match(r"^(\s*)", line).group(1)
            line = f"{indent}run: {DIST_INSTALL}\n"
        line, problem = pin_line(line)
        if problem:
            problems.append(problem)
        out.append(line)
    return "".join(out), problems


def check(path):
    return check_text(str(path.relative_to(ROOT)), path.read_text())


def check_text(name, text):
    """Fails closed: every `uses:` must be a local action, a Docker image by
    digest, or a pinned commit with its tag; every dist install the pinned
    command."""
    problems = []
    lines = text.splitlines()
    for n, line in enumerate(lines, 1):
        where = f"{name}:{n}"
        m = USES_ANY.match(line)
        if not m and ANY_USES_KEY.search(line) and not line.lstrip().startswith("#"):
            problems.append(f"{where}: a `uses` this script can't read; write it as `- uses: <action>`")
        if m:
            value, tag = uses_value(m.group(2))
            r = REF.match(value)
            if value.startswith("./") or DIGEST.match(value):
                pass
            elif not r:
                problems.append(f"{where}: {value!r} isn't a pinned action")
            elif not SHA.match(r.group(2)):
                problems.append(f"{where}: {value} isn't pinned by commit")
            elif not tag or PINS.get(f"{r.group(1)}@{tag}") != r.group(2):
                problems.append(f"{where}: {value} isn't in the pin table")
        if "cargo-dist-installer" in line or "install_dist" in line:
            problems.append(f"{where}: installs dist another way than `{DIST_INSTALL}`")
        if CARGO_DIST.search(line) and line.strip() != f"run: {DIST_INSTALL}":
            problems.append(f"{where}: installs dist without exactly `{DIST_INSTALL}`")
        step_head = STEP.match(line)
        if step_head:
            # The step's own `run` key, at the column of its `name`, not a
            # nested one (`env: {run: …}`).
            column = len(step_head.group(1))
            runs = []
            for later in lines[n:]:
                indent = len(later) - len(later.lstrip())
                if later.strip() and indent < column:
                    break
                if re.match(r"^\s*-\s", later) and indent < column + 1:
                    break
                if indent == column and later.lstrip().startswith("run:"):
                    runs.append(later.strip())
            if runs != [f"run: {DIST_INSTALL}"]:
                problems.append(f"{where}: this step must run exactly `{DIST_INSTALL}`")
    return problems


def resolve():
    wanted = set(PINS)
    for path in WORKFLOWS:
        for line in path.read_text().splitlines():
            m = USES_ANY.match(line)
            if m:
                value, tag = uses_value(m.group(2))
                r = REF.match(value)
                if r:
                    wanted.add(f"{r.group(1)}@{tag if SHA.match(r.group(2)) else r.group(2)}")
    for key in sorted(wanted):
        action, tag = key.split("@", 1)
        repo = "/".join(action.split("/")[:2])
        # Read only, with no stored credentials and no prompt.
        out = subprocess.run(
            ["git", "-c", "credential.helper=", "ls-remote", f"https://github.com/{repo}",
             f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}"],
            capture_output=True, text=True, check=True, env={**os.environ, "GIT_TERMINAL_PROMPT": "0"},
        ).stdout.split("\n")
        refs = dict(reversed(l.split("\t")) for l in out if "\t" in l)
        # An annotated tag's peeled line is the commit.
        sha = refs.get(f"refs/tags/{tag}^{{}}") or refs.get(f"refs/tags/{tag}")
        print(f'    "{key}": "{sha}",' if sha else f"    # {key}: no tag of that name")


def self_test():
    """The checker against workflows it must pass and fail."""
    sha = PINS["actions/checkout@v6"]
    good = f"      - uses: actions/checkout@{sha} # v6\n"
    step = "      - name: Install dist\n        run: {}\n"
    cases = [
        (True, good),
        (True, "      - uses: ./.github/actions/local\n"),
        (True, "      - uses: docker://alpine@sha256:" + "a" * 64 + "\n"),
        (True, step.format(DIST_INSTALL)),
        (False, "      - uses: actions/checkout@v6\n"),
        (False, '      - uses: "actions/checkout@v6"\n'),
        (False, "      - uses: actions/checkout@v6 # checkout the repository\n"),
        (False, f"      - uses: actions/checkout@{sha}\n"),
        (False, f"      - uses: actions/checkout@{'b' * 40} # v6\n"),
        (False, "      - uses: docker://alpine:3\n"),
        (False, "      - uses: pypa/gh-action-pypi-publish@release/v1\n"),
        (False, step.format("cargo install cargo-dist --locked")),
        (False, step.format("cargo install cargo-dist --version 0.31.0 --locked")),
        (False, step.format("cargo install cargo-dist --version 0.32.0")),
        (False, "      - name: Install dist\n        run: |\n          curl -LsSf https://x/cargo-dist-installer.sh | sh\n"),
        (False, "      - name: Install dist\n        run: ${{ matrix.install_dist.run }}\n"),
        (False, "      - {uses: pypa/gh-action-pypi-publish@release/v1}\n"),
        (False, "      - 'uses': actions/checkout@v6\n"),
        (False, "      - uses : actions/checkout@v6\n"),
        (False, "      - name: Install dist\n        env:\n          run: " + DIST_INSTALL
         + "\n        run: echo skipped\n"),
        (False, "    steps: [uses: pypa/gh-action-pypi-publish@release/v1]\n"),
        (False, step.format("cargo install --locked --version 0.32.0 cargo-dist")),
    ]
    bad = [text for ok, text in cases if bool(check_text("case", text)) == ok]
    for text in bad:
        print(f"self-test: the checker got this wrong:\n{text}", file=sys.stderr)
    if not bad:
        print(f"pin self-test passed ({len(cases)} cases)")
    return 1 if bad else 0


def main():
    args = sys.argv[1:]
    if "--self-test" in args:
        return self_test()
    if "--resolve" in args:
        resolve()
        return 0
    if "--check" in args:
        problems = [p for path in WORKFLOWS for p in check(path)]
        for p in problems:
            print(p, file=sys.stderr)
        if problems:
            print("run scripts/pin-workflows.py (after `dist generate`)", file=sys.stderr)
            return 1
        print(f"workflows pinned ({len(WORKFLOWS)} files)")
        return 0
    problems = []
    for path in WORKFLOWS:
        text, found = rewrite(path.read_text())
        problems += [f"{path.relative_to(ROOT)}: {p}" for p in found]
        path.write_text(text)
    for p in problems:
        print(p, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
