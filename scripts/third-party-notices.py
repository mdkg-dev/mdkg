#!/usr/bin/env python3
"""Writes THIRD_PARTY_NOTICES.md: the license notices of every crate the
`mdkg` binary is built from, for every release target, as the MIT, BSD
and Apache licenses of those crates ask of anyone shipping them.

Usage: scripts/third-party-notices.py [--check | --check-wheel WHEEL... | --self-test]
  --check        exit 1 if THIRD_PARTY_NOTICES.md isn't what it would write.
  --check-wheel  exit 1 if a wheel carries a shared library (auditwheel's
                 `<package>.libs/`) that the wheel's own THIRD_PARTY_NOTICES.md
                 doesn't list. Needs no cargo.
  --self-test    test --check-wheel's reading of wheels, without cargo.

`cargo metadata` resolves the locked graph for each target, and each
crate's own license and notice files are read from Cargo's unpacked
sources. It runs `cargo fetch --locked` per target first, offline when the
cache has them, so a cache a native build filled is enough when nothing
is missing, and the registry fills the rest. Only normal dependencies count: what a
build or test needs but doesn't ship (build scripts' and dev dependencies)
is left out. The file names no local paths.

A wheel could also carry C libraries that auditwheel copies in, which
aren't crates; BUNDLED would list them, and it lists none. pypi.yml runs
--check-wheel on every wheel it builds, so a bundled library fails the
build unless it is listed.
"""
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import tempfile
import unittest
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "THIRD_PARTY_NOTICES.md"
BINARY = "mdkg"
# The release targets (dist-workspace.toml); Windows waits for 0.7.1.
TARGETS = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
]
# Shared libraries a wheel may carry beside the binary (auditwheel's
# `<package>.libs/<stem>-<hash>.so.<n>`), by stem, each with
# (stem, upstream project, build-image package, SPDX license, source).
# None: the Linux wheels build without FIDO2, whose hidapi backend would
# bring in libudev and the system libraries it links (decided 2026-11-03,
# #17661). A library added here is listed in THIRD_PARTY_NOTICES.md, and
# needs its license texts and corresponding source arranged first.
BUNDLED = []
BUNDLED_HEADING = "## Libraries bundled in the wheels"
BUNDLED_NAME = re.compile(r"(?:[^/]+\.libs/)(.+?)-[0-9a-f]{8}\.so(?:\.[0-9]+)*")
SHARED = re.compile(r"\.(?:so(?:\.[0-9]+)*|dylib)$")
NOTICE = re.compile(r"^(licen[cs]e|copying|notice|unlicense|copyright)([-._].*)?$", re.I)


def fetch(target):
    """The locked sources for `target`, which a native build doesn't
    unpack for another platform (linux-raw-sys on macOS): from the cache
    when they're there, else from the registry."""
    for offline in (["--offline"], []):
        done = subprocess.run(
            ["cargo", "fetch", "--locked", "--target", target, *offline],
            cwd=ROOT, capture_output=True, text=True,
        )
        if done.returncode == 0:
            return
    sys.exit(f"cargo fetch --locked --target {target} failed:\n{done.stderr}")


def metadata(target):
    fetch(target)
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--offline", "--locked",
         "--filter-platform", target],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def shipped(meta):
    """The packages the binary is built from, by id: a walk over normal
    dependency edges from the binary's package."""
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = next(p["id"] for p in meta["packages"] if p["name"] == BINARY
                and p["id"] in meta["workspace_members"])
    seen, todo = set(), [root]
    while todo:
        pid = todo.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                todo.append(dep["pkg"])
    members = set(meta["workspace_members"])
    return {pid: packages[pid] for pid in seen if pid not in members}


def notice_files(package):
    folder = pathlib.Path(package["manifest_path"]).parent
    files = {f for f in folder.iterdir() if f.is_file() and NOTICE.match(f.name)}
    if package.get("license_file"):
        named = folder / package["license_file"]
        if named.is_file():
            files.add(named)
    return sorted(files, key=lambda f: f.name.lower())


def render():
    packages = {}
    for target in TARGETS:
        packages.update(shipped(metadata(target)))
    crates = sorted(packages.values(), key=lambda p: (p["name"], p["version"]))
    # Each distinct text once, with the crates that carry it.
    texts, order, missing = {}, [], []
    entries = []
    for p in crates:
        refs = []
        for f in notice_files(p):
            text = f.read_text(encoding="utf-8", errors="replace").replace("\r\n", "\n").strip()
            key = hashlib.sha256(text.encode()).hexdigest()[:12]
            if key not in texts:
                texts[key] = (f.name, text, [])
                order.append(key)
            texts[key][2].append(f"{p['name']} {p['version']}")
            refs.append(key)
        if not refs:
            missing.append(p)
        entries.append((p, refs))
    lines = [
        "# Third-party notices",
        "",
        "mdkg is built from the open-source crates below. Their licenses ask",
        "that their notices travel with the program; they do, here. mdkg's own",
        "license is in LICENSE. Generated by `scripts/third-party-notices.py`;",
        "don't edit it by hand.",
        "",
        "## Crates",
        "",
        "| Crate | Version | License | Notices |",
        "|---|---|---|---|",
    ]
    for p, refs in entries:
        notes = ", ".join(f"[{k}](#notice-{k})" for k in refs) or "none in the crate; see its license"
        lines.append(f"| {p['name']} | {p['version']} | {p.get('license') or 'see notices'} | {notes} |")
    if BUNDLED:
        lines += ["", BUNDLED_HEADING, "",
                  "| Library | Project | Build-image package | License | Source |",
                  "|---|---|---|---|---|"]
        lines += [f"| {stem} | {project} | {package} | {spdx} | {source} |"
                  for stem, project, package, spdx, source in BUNDLED]
    lines += ["", "## Notices", ""]
    for key in order:
        name, text, users = texts[key]
        lines += [
            f'<a id="notice-{key}"></a>',
            f"### {key}: {name}",
            "",
            "Carried by: " + ", ".join(users) + ".",
            "",
            "```text",
            text.replace("```", "'''"),
            "```",
            "",
        ]
    return "\n".join(lines).rstrip() + "\n", crates, missing


def unlisted(wheel):
    """The shared libraries in `wheel` that its own THIRD_PARTY_NOTICES.md
    doesn't list: anything shaped like a shared library outside the
    wheel's scripts whose stem has no row in the notices' bundled-library
    table. The notices read are the wheel's copy (its dist-info), so
    adding a name to BUNDLED without regenerating the file still fails."""
    bad = []
    with zipfile.ZipFile(wheel) as z:
        names = z.namelist()
        notices = [n for n in names if re.fullmatch(r"[^/]+\.dist-info/licenses/THIRD_PARTY_NOTICES\.md", n)]
        listed = set()
        if len(notices) == 1:
            text = z.read(notices[0]).decode("utf-8", "replace")
            section = text.partition(BUNDLED_HEADING)[2].partition("\n## ")[0]
            listed = set(re.findall(r"^\| (lib[^ |]+) \|", section, re.M))
        for name in names:
            if name.endswith("/") or ".data/scripts/" in name or not SHARED.search(name):
                continue
            m = BUNDLED_NAME.fullmatch(name)
            if not m or m[1] not in listed:
                bad.append(name)
    return bad


def check_wheels(wheels):
    failed = False
    for wheel in wheels:
        bad = unlisted(wheel)
        for name in bad:
            print(f"{wheel}: bundles {name}, which the wheel's THIRD_PARTY_NOTICES.md doesn't list "
                  "(add it to BUNDLED in scripts/third-party-notices.py and regenerate)", file=sys.stderr)
        failed |= bool(bad)
        if not bad:
            print(f"{wheel}: every bundled library is listed")
    return 1 if failed or not wheels else 0


def self_test():
    class Tests(unittest.TestCase):
        NOTICES = "mdkg-0.7.2.dist-info/licenses/THIRD_PARTY_NOTICES.md"

        def wheel(self, d, names, stems=None):
            # A notices file as render() would write one if BUNDLED listed
            # these, with a decoy row past its section.
            stems = ["libudev", "libpcre2-8", "libuuid"] if stems is None else stems
            notices = "\n".join(["# x", "", BUNDLED_HEADING, "", "| Library | x |", "|---|---|",
                                  *(f"| {s} | x |" for s in stems), "", "## Notices", "| libcap | x |"])
            path = pathlib.Path(d) / "mdkg-0.7.2-py3-none-manylinux_2_28_x86_64.whl"
            with zipfile.ZipFile(path, "w") as z:
                for n in names:
                    z.writestr(n, notices if n == self.NOTICES else b"x")
            return path

        def test_notices_in_the_wheel_decide(self):
            libs = ["mdkg-0.7.2.data/scripts/mdkg", "mdkg.libs/libudev-1a2b3c4d.so.1.6.11"]
            with tempfile.TemporaryDirectory() as d:
                # Not in the wheel's notices: fails.
                self.assertEqual(unlisted(self.wheel(d, libs + [self.NOTICES], stems=["libuuid"])), [libs[1]])
                # No notices in the wheel: every library fails.
                self.assertEqual(unlisted(self.wheel(d, libs)), [libs[1]])
                # Its own notices list it: passes.
                self.assertEqual(unlisted(self.wheel(d, libs + [self.NOTICES])), [])
                # A wheel with no libraries needs no notices row.
                self.assertEqual(unlisted(self.wheel(d, libs[:1])), [])

        def test_real_notices_list_nothing(self):
            # BUNDLED is empty, so the notices have no bundled section, and a
            # wheel carrying any library with them fails.
            text = OUT.read_text()
            self.assertNotIn(BUNDLED_HEADING, text)
            with tempfile.TemporaryDirectory() as d:
                path = pathlib.Path(d) / "mdkg-0.7.2-py3-none-manylinux_2_28_x86_64.whl"
                lib = "mdkg.libs/libudev-1a2b3c4d.so.1.6.11"
                with zipfile.ZipFile(path, "w") as z:
                    z.writestr("mdkg-0.7.2.data/scripts/mdkg", b"x")
                    z.writestr(lib, b"x")
                    z.writestr(self.NOTICES, text)
                self.assertEqual(unlisted(path), [lib])
                self.assertEqual(check_wheels([path]), 1)

        def test_listed_and_unlisted(self):
            with tempfile.TemporaryDirectory() as d:
                ok = ["mdkg-0.7.2.data/scripts/mdkg", "mdkg.libs/libudev-1a2b3c4d.so.1.6.11",
                      "mdkg.libs/libpcre2-8-0f0f0f0f.so.0.7.1", "mdkg.libs/libuuid-deadbeef.so.1",
                      self.NOTICES]
                self.assertEqual(unlisted(self.wheel(d, ok)), [])
                self.assertEqual(check_wheels([self.wheel(d, ok)]), 0)
            for extra in ["mdkg.libs/libcap-12345678.so.2", "mdkg.libs/libudev.so.1",
                          "mdkg/libfoo.so", "lib/libz.dylib", "mdkg.libs/libudev-1a2b3c4d.so.1.x"]:
                with self.subTest(extra=extra), tempfile.TemporaryDirectory() as d:
                    w = self.wheel(d, ok + [extra])
                    if extra.endswith(".x"):
                        self.assertEqual(unlisted(w), [])  # not shaped like a library
                        continue
                    self.assertEqual(unlisted(w), [extra])
                    self.assertEqual(check_wheels([w]), 1)
            self.assertEqual(check_wheels([]), 1, "no wheel checks nothing")

        def test_bundled_well_formed(self):
            stems = [b[0] for b in BUNDLED]
            self.assertEqual(len(stems), len(set(stems)))
            for b in BUNDLED:
                self.assertEqual(len(b), 5)
                self.assertTrue(b[4].startswith("https://"))

    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests))
    return 0 if result.wasSuccessful() else 1


def main():
    args = sys.argv[1:]
    if args[:1] == ["--check-wheel"]:
        return check_wheels(args[1:])
    if args == ["--self-test"]:
        return self_test()
    text, crates, missing = render()
    if "--check" in sys.argv[1:]:
        current = OUT.read_text() if OUT.is_file() else ""
        if current != text:
            print("THIRD_PARTY_NOTICES.md is stale: run scripts/third-party-notices.py", file=sys.stderr)
            return 1
        print(f"THIRD_PARTY_NOTICES.md is current ({len(crates)} crates)")
        return 0
    OUT.write_text(text)
    print(f"wrote THIRD_PARTY_NOTICES.md: {len(crates)} crates, {len(missing)} without a notice file")
    for p in missing:
        print(f"  no notice file: {p['name']} {p['version']} ({p.get('license')})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
