#!/bin/bash
# Run every gate an agent must pass before asking for a merge (AGENTS.md).
set -euo pipefail

cd "$(/usr/bin/dirname "$0")/.."

NODE24="${MDKG_NODE24:-/opt/homebrew/opt/node@24/bin/node}"
MDKG_TS_CLI="${MDKG_TS_CLI:-/opt/homebrew/lib/node_modules/mdkg/dist/cli.js}"

expected_checkout=$(pwd -P)
CHECK_PYTHON=
for check_python_candidate in /opt/homebrew/bin/python3 /usr/local/bin/python3 /usr/bin/python3; do
  if [[ -x "$check_python_candidate" ]]; then CHECK_PYTHON=$check_python_candidate; break; fi
done
[[ -n "$CHECK_PYTHON" ]] || { echo "no Python at a trusted system location" >&2; exit 1; }
CHECK_GIT=$("$CHECK_PYTHON" -I - "$expected_checkout" <<'PY_CHECK_GIT'
from pathlib import Path
import runpy, sys
root = Path(sys.argv[1])
print(runpy.run_path(str(root / "scripts/leak-scan.py"))["pin_git"](root))
PY_CHECK_GIT
)
PATH=$("$CHECK_PYTHON" -I - "$expected_checkout" <<'PY_CHECK_PATH'
from pathlib import Path
import runpy, sys
root = Path(sys.argv[1])
print(runpy.run_path(str(root / "scripts/leak-scan.py"))["trusted_path"](root))
PY_CHECK_PATH
)
export PATH
git() { "$CHECK_GIT" "$@"; }


step() { printf '\n== %s\n' "$1"; }

affected_base=
affected_main=
ci_env=0
ci_mdkg=
check_usage="Usage: scripts/check.sh [--affected <base-ref> [--main <ref>]]
       scripts/check.sh --ci-env [--mdkg <absolute path to an mdkg binary>]"
case $# in
  0) ;;
  1)
    if [[ "$1" == --help ]]; then
      echo "$check_usage"
      echo "Affected mode checks the committed head before/after local tests; restored mid-run edits are not detected."
      echo "Ignored files, including ignored build configuration, are not verified."
      echo "Exit 3 means local checks passed, but selected external/script checks are incomplete."
      echo "--ci-env runs every check the way public CI does: on a fresh public export of HEAD"
      echo "(export-public.sh --init, with mdkg-next or mdkg from PATH unless --mdkg names one), with an"
      echo "empty HOME, no system git config, CI's git identity only in HOME, a fresh target"
      echo "folder and no other inherited environment. It needs the 0.6.0 oracle and runs only in the"
      echo "development repository (the public tree has no exporter). docs/releasing.md has the details."
      exit 0
    elif [[ "$1" == --ci-env ]]; then
      ci_env=1
    else
      echo "$check_usage" >&2; exit 2
    fi ;;
  3)
    [[ "$1" == --ci-env && "$2" == --mdkg && -n "$3" ]] \
      || { echo "$check_usage" >&2; exit 2; }
    ci_env=1 ci_mdkg=$3 ;;
  2)
    [[ "$1" == --affected && -n "$2" ]] || { echo "expected --affected <base-ref>" >&2; exit 2; }
    affected_base=$2 ;;
  4)
    # Paths main's runs tested are skipped only for an explicit branch
    # or remote-tracking ref (orchestrator, #12089).
    [[ "$1" == --affected && -n "$2" && "$3" == --main && -n "$4" ]] \
      || { echo "expected --affected <base-ref> --main <ref>" >&2; exit 2; }
    affected_base=$2 affected_main=$4 ;;
  *) echo "$check_usage" >&2; exit 2 ;;
esac

affected_plan=
check_tmp=
ci_tmp=
cleanup_check() {
  local check_status=$?
  if [[ -n "$affected_plan" ]]; then
    rm -f "$affected_plan" || echo "warning: couldn't remove affected plan: $affected_plan" >&2
  fi
  if [[ -n "$check_tmp" ]]; then
    rm -rf "$check_tmp" || echo "warning: couldn't remove check temp folder: $check_tmp" >&2
  fi
  if [[ -n "$ci_tmp" ]]; then
    rm -rf "$ci_tmp" || echo "warning: couldn't remove the CI-like export: $ci_tmp" >&2
  fi
  exit "$check_status"
}
trap cleanup_check EXIT

# --ci-env: the gate before a release candidate (docs/releasing.md). Public
# CI found failures that no local run had (0.7.1): tests that needed a git
# identity, read this repository's own .mdkg, or used a shell tool's GNU-
# or BSD-only form. Each came from the environment, not the code, so this
# mode rebuilds CI's environment around a fresh public export of HEAD and
# runs that export's own scripts/check.sh in it:
# - the tree is `export-public.sh --init` of HEAD, committed once as a new
#   repository, so its .mdkg is a fresh graph, as the public one is;
# - HOME is a new, empty folder holding only the .gitconfig CI's
#   "Configure git" step writes (an identity and init.defaultBranch).
#   GIT_CONFIG_GLOBAL stays unset, as on CI: a test that moves HOME loses
#   the identity there too;
# - GIT_CONFIG_NOSYSTEM=1, and user.useConfigOnly=true through
#   GIT_CONFIG_COUNT: macOS's git otherwise makes up an identity from the
#   account and host name where Linux's refuses to commit;
# - a fresh CARGO_TARGET_DIR inside the export, so nothing compiled
#   earlier is reused (cargo never rebuilds a registry crate of the same
#   version);
# - CARGO_HOME a new folder too, linking only the real one's download
#   caches and their locks (registry, git): no config.toml (a rustc
#   wrapper, build flags or [env] CI doesn't have), no credentials, no
#   installed binaries;
# - no other inherited variable but RUSTUP_HOME, PATH, the locale and
#   terminal, and the build and test job counts. `env -i` drops the
#   rest, credentials included;
# - the 0.6.0 oracle's two paths, always: CI installs it, so a run
#   without it would skip checks CI makes.
# It isolates the environment, not the disk: the tests still run as you,
# so a test naming an absolute path still reads it.
# It can't change the operating system: Linux-only failures (clippy's
# cfg(target_os) code, Linux socket errors) still need ci-remote.sh's
# Ubuntu leg or public CI.
if ((ci_env)); then
  # The public tree ships check.sh but not the exporter: say so before
  # anything else, since no option makes the mode work there.
  [[ -f scripts/export-public.sh ]] \
    || { echo "--ci-env runs in the development repository: this checkout has no scripts/export-public.sh" >&2; exit 2; }
  if [[ -z "$ci_mdkg" ]]; then
    # A development checkout's own pinned build comes first; plain `mdkg`
    # may be an older release whose init writes an older format.
    ci_mdkg=$(command -v mdkg-next || command -v mdkg) \
      || { echo "--ci-env needs an mdkg binary for the export's init: pass --mdkg <path>" >&2; exit 2; }
  fi
  [[ "$ci_mdkg" == /* ]] || { echo "--mdkg needs an absolute path" >&2; exit 2; }
  [[ -f "$ci_mdkg" && -x "$ci_mdkg" ]] || { echo "--mdkg isn't an executable file" >&2; exit 2; }
  [[ -n "${HOME:-}" ]] || { echo "--ci-env needs HOME to find the toolchain" >&2; exit 2; }
  # CI installs the 0.6.0 oracle; without it the write-command tests skip
  # their 0.6 compatibility checks, so this mode requires it. Absolute
  # paths only: the inner run starts in the export, where a relative one
  # would name nothing and the tests would skip the oracle silently.
  [[ "$NODE24" == /* && "$MDKG_TS_CLI" == /* ]] \
    || { echo "--ci-env needs absolute paths in MDKG_NODE24 and MDKG_TS_CLI" >&2; exit 2; }
  [[ -f "$NODE24" && -x "$NODE24" && -f "$MDKG_TS_CLI" ]] \
    || { echo "--ci-env needs the 0.6.0 oracle, as CI has: set MDKG_NODE24 to Node 24 and MDKG_TS_CLI to mdkg@0.6.0's dist/cli.js" >&2; exit 2; }
  ci_head=$(git rev-parse --verify HEAD^{commit})
  # The rest of this mode's paths come from mktemp: short, under /tmp, so
  # the tests' Unix socket paths fit (see the tests step below).
  ci_tmp=$(mktemp -d /tmp/mci.XXXXXX)
  ci_home="$ci_tmp/home"
  ci_src="$ci_tmp/src"
  ci_cargo_real=${CARGO_HOME:-$HOME/.cargo}
  ci_rustup=${RUSTUP_HOME:-$HOME/.rustup}
  # The links and the inner run (which starts in the export) need both
  # homes absolute; a relative one would name a folder that isn't there.
  [[ "$ci_cargo_real" == /* && "$ci_rustup" == /* ]] \
    || { echo "--ci-env needs absolute paths in CARGO_HOME, RUSTUP_HOME and HOME" >&2; exit 2; }
  ci_cargo="$ci_tmp/cargo"
  mkdir "$ci_home" "$ci_cargo"
  # Cargo's downloads, shared so this run doesn't fetch every crate again,
  # with the lock files that guard them, so a build elsewhere and this one
  # take the same locks. Nothing else of the real Cargo home is linked.
  for ci_name in registry git .package-cache .package-cache-mutate .global-cache; do
    if [[ -e "$ci_cargo_real/$ci_name" ]]; then
      ln -s "$ci_cargo_real/$ci_name" "$ci_cargo/$ci_name"
    fi
  done
  ci_vars=(
    HOME="$ci_home"
    PATH="$PATH"
    CARGO_HOME="$ci_cargo"
    RUSTUP_HOME="$ci_rustup"
    CARGO_TARGET_DIR="$ci_src/target"
    CI=true
    MDKG_NODE24="$NODE24"
    MDKG_TS_CLI="$MDKG_TS_CLI"
    GIT_CONFIG_NOSYSTEM=1
    GIT_CONFIG_COUNT=1
    GIT_CONFIG_KEY_0=user.useConfigOnly
    GIT_CONFIG_VALUE_0=true
  )
  for ci_name in USER LOGNAME LANG LC_ALL TERM CARGO_BUILD_JOBS NEXTEST_TEST_THREADS \
      RUST_TEST_THREADS; do
    if [[ -n "${!ci_name+x}" ]]; then ci_vars+=("$ci_name=${!ci_name}"); fi
  done
  ci_run() { /usr/bin/env -i "${ci_vars[@]}" "$@"; }

  step "CI-like run: export $ci_head"
  scripts/export-public.sh --rev "$ci_head" --init "$ci_mdkg" "$ci_src"
  # What CI's "Configure git for the tests' commits" step writes.
  ci_run "$CHECK_GIT" config --global user.name "mdkg CI"
  ci_run "$CHECK_GIT" config --global user.email "ci@mdkg.invalid"
  ci_run "$CHECK_GIT" config --global init.defaultBranch main
  # build.rs reads the commit for `--version`, as CI's checkout gives it.
  ci_run "$CHECK_GIT" -C "$ci_src" init -q
  ci_run "$CHECK_GIT" -C "$ci_src" add -A
  ci_run "$CHECK_GIT" -C "$ci_src" commit -q -m "CI-like export of $ci_head"

  step "CI-like run: scripts/check.sh in $ci_src"
  echo "(a fresh build: the first run compiles everything)"
  ci_status=0
  (cd "$ci_src" && ci_run /bin/bash scripts/check.sh) || ci_status=$?
  if ((ci_status)); then
    step "CI-like run failed (exit $ci_status) on the export of $ci_head"
  else
    step "CI-like run passed on the export of $ci_head"
  fi
  exit "$ci_status"
fi
mode=full
# This reader never interpolates a target or package into shell source. Cargo
# receives the exact argv array, and empty selections launch no Cargo process.
affected_cargo() {
  "$CHECK_PYTHON" -I - "$affected_plan" "$1" "$expected_checkout" "$CHECK_GIT" <<'PY_AFFECTED'
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import stat
import subprocess
import sys

CHECKOUT = Path(sys.argv[3]).resolve() if len(sys.argv) > 3 else None
SCANNER = runpy.run_path(str((CHECKOUT or Path.cwd()) / "scripts/leak-scan.py"))
SCANNER["pin_git"](CHECKOUT or Path.cwd(), sys.argv[4] if len(sys.argv) > 4 else None)

def index_flags(listing):
    for row in listing.split(b"\0"):
        if row and (row[:1].islower() or row[:1].upper() == b"S"):
            raise SystemExit("affected verification refuses assume-unchanged/skip-worktree index flags")


def check_head(actual, expected):
    if expected is not None and actual != expected:
        raise SystemExit("incomplete affected verification: HEAD changed during the run")


def require_clean(expected_head=None):
    root = CHECKOUT or Path.cwd().resolve()
    def git(*args):
        return SCANNER["git_read"](root, *args).stdout
    actual_root = Path(os.fsdecode(git("rev-parse", "--show-toplevel").removesuffix(b"\n"))).resolve()
    if actual_root != root:
        raise SystemExit("affected checkout identity differs from the verification root")
    check_head(git("rev-parse", "--verify", "HEAD").decode("ascii").strip(), expected_head)
    index_flags(git("ls-files", "-v", "-z"))
    if git("ls-files", "--others", "--exclude-standard", "-z"):
        raise SystemExit("affected verification requires a clean checkout (untracked files)")
    indexed = {}
    for row in git("ls-files", "--stage", "-z").split(b"\0"):
        if row:
            fields, name = row.split(b"\t", 1)
            mode, oid, stage = fields.split()
            if stage != b"0" or name in indexed:
                raise SystemExit("affected verification refuses an unmerged index")
            indexed[name] = (mode, oid)
    tree = {}
    for row in git("ls-tree", "-r", "-z", "HEAD").split(b"\0"):
        if row:
            fields, name = row.split(b"\t", 1)
            mode, kind, oid = fields.split()
            if kind != b"blob":
                raise SystemExit("incomplete affected verification: submodules need separate checks")
            tree[name] = (mode, oid)
    if tree != indexed:
        raise SystemExit("affected verification requires a clean committed index")
    algorithm = git("rev-parse", "--show-object-format").decode("ascii").strip()
    for name, (mode, oid) in tree.items():
        rel = Path(os.fsdecode(name))
        if rel.is_absolute() or not rel.parts or ".." in rel.parts:
            raise SystemExit("invalid tracked input path")
        digest = hashlib.new(algorithm)
        try:
            with ExitStack() as opened:
                flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
                directory = os.open(root, flags)
                opened.callback(os.close, directory)
                for part in rel.parts[:-1]:
                    directory = os.open(part, flags, dir_fd=directory)
                    opened.callback(os.close, directory)
                if mode == b"120000":
                    body = os.fsencode(os.readlink(rel.name, dir_fd=directory))
                    digest.update(b"blob " + str(len(body)).encode() + b"\0" + body)
                elif mode in (b"100644", b"100755"):
                    fd = os.open(rel.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
                    opened.callback(os.close, fd)
                    info = os.fstat(fd)
                    if not stat.S_ISREG(info.st_mode) or bool(info.st_mode & 0o111) != (mode == b"100755"):
                        raise SystemExit("affected verification found a changed tracked file type/mode")
                    digest.update(b"blob " + str(info.st_size).encode() + b"\0")
                    while chunk := os.read(fd, 65536):
                        digest.update(chunk)
                else:
                    raise SystemExit("unsupported tracked file mode")
        except OSError:
            raise SystemExit("affected verification cannot read a tracked input") from None
        if digest.hexdigest().encode() != oid:
            raise SystemExit("affected verification requires tracked bytes to match HEAD")


def template_coverage(config):
    if "affected" not in config:
        return  # Public trees without this repository's rule policy.
    templates = config.get("templates")
    root = templates.get("root_path") if isinstance(templates, dict) else None
    if not isinstance(root, str):
        root = ".mdkg/templates"
    root = str(Path(root))
    pattern = "**" if root == "." else root.rstrip("/") + "/**"
    for rule in config["affected"].get("rules", []):
        if not {pattern, "**"}.intersection(rule.get("paths", [])):
            continue
        select = rule.get("select")
        if select == "all":
            return
        if isinstance(select, dict) and ("mdkg" in select.get("rust", []) or any(
                target.get("package") == "mdkg" and target.get("test") == "affected"
                for target in select.get("rust_targets", []))):
            return
    raise SystemExit("configured template root needs an explicit affected-test or full-coverage rule")


def required_suites(data):
    suites = set(data.get("selected", {}).get("suites", []))
    if data.get("mode") == "full":
        suites.update(("platform", "quint", "release", "scripts"))
    for path in data.get("paths", []):
        suites.update(value[6:] for value in path.get("selects", []) if value.startswith("suite:"))
    return suites


def finish(data):
    incomplete = sorted(required_suites(data) - {"lint", "workflows"})
    if incomplete:
        print("local checks passed; incomplete selected suites: " + ", ".join(incomplete), file=sys.stderr)
        raise SystemExit(3)


def execute(report, action):
    if report.get("ok") is not True:
        raise SystemExit("affected failed; fix its diagnostic and retry before treating the selection as verified")
    data = report["data"]
    mode = data["mode"]
    if action == "verify":
        require_clean(data["head"]["sha"])
        return
    if action == "mode":
        for warning in data.get("warnings", []):
            print("affected fallback: " + json.dumps(warning, ensure_ascii=True), file=sys.stderr)
        main = data.get("main")
        if isinstance(main, dict):
            print("affected main: " + json.dumps({
                "rev": main.get("rev"), "sha": main.get("sha"),
                "from_main": len(data.get("from_main", [])), "detail": main.get("detail"),
            }, ensure_ascii=True), file=sys.stderr)
    if mode not in ("full", "selective"):
        raise SystemExit("affected returned an unknown mode")
    if mode == "full":
        if action == "finish":
            finish(data)
            return
        if action != "mode":
            raise SystemExit("a full decision cannot use the selective runner")
        print("affected: full " + json.dumps(data["reasons"]), file=sys.stderr)
        print(mode)
        raise SystemExit(0)
    selected = data["selected"]
    packages = selected["rust"]
    def valid_name(value):
        return isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}", value)
    if not isinstance(packages, list) or any(not valid_name(p) for p in packages):
        raise SystemExit("invalid affected Rust packages")
    suites = selected["suites"]
    if not isinstance(suites, list) or any(not isinstance(s, str) or not s for s in suites):
        raise SystemExit("invalid affected suites")
    if "lint" in suites and not packages:
        raise SystemExit("incomplete affected verification: lint suite has no selected packages")
    if selected["go"]:
        raise SystemExit("this repository's selective runner handles Rust; run scripts/check.sh in full mode for Go")
    base_args = ["test"]
    for package in packages:
        base_args += ["-p", package]
    # A v1 plan that omits targets selects whole packages, as Cargo's default
    # test run does.
    whole, filters = True, set()
    if "rust_targets" in selected:
        targets = selected["rust_targets"]
        covered, filters = set(), set()
        for target in targets:
            package, kind = target["package"], target["kind"]
            if package not in packages:
                raise SystemExit("affected target is outside its package selection")
            covered.add(package)
            if kind in ("test", "bin"):
                name = target.get("name")
                if not valid_name(name):
                    raise SystemExit("affected target has no name")
                filters.add(("--" + kind, name))
            elif kind == "lib":
                filters.add(("--lib",))
            elif kind == "package":
                pass
            else:
                raise SystemExit("affected returned an unknown target kind")
        if covered != set(packages):
            raise SystemExit("affected target plan does not cover its packages")
        whole = any(target["kind"] == "package" for target in targets)
        grouped_args = base_args + ([] if whole else [arg for scope in sorted(filters) for arg in scope])
        for target in targets:
            if target["cargo_args"] != grouped_args:
                raise SystemExit("invalid affected Cargo argument set")
    if action == "mode":
        print("affected suites: " + ", ".join(selected["suites"]), file=sys.stderr)
        if "platform" in selected["suites"]:
            print("platform suite also requires Ubuntu CI", file=sys.stderr)
        print(mode)
    elif action == "clippy":
        if packages:
            args = ["cargo", "clippy"]
            for package in packages:
                args += ["-p", package]
            subprocess.run(args + ["--all-targets", "--quiet", "--", "-D", "warnings"], check=True,
                           env={**os.environ, "CARGO_BUILD_JOBS": "4"})
    elif action == "test":
        # nextest runs the tests: each target's tests go in one pool, so a slow
        # binary no longer holds up the ones after it (decided 2026-10-30). The
        # affected plan's target scopes become nextest filters: `--test` and
        # `--bin` name a binary, `--lib` is the lib kind, and a whole package
        # runs every target. It runs no doctests, so a whole-package scope also
        # runs `cargo test --doc`, which is what a plain `cargo test` included.
        commands = []
        if packages:
            nextest = ["cargo", "nextest", "run"]
            for package in packages:
                nextest += ["-p", package]
            if not whole:
                expression = " or ".join(
                    "kind(lib)" if scope == ("--lib",) else "binary(%s)" % scope[1]
                    for scope in sorted(filters))
                nextest += ["-E", expression]
            # Lanes test four at a time; the machine-wide build slots cap the
            # processes across lanes. A selected target with no tests passes,
            # as `cargo test` did: the scopes come from Cargo's metadata, not
            # from a hand-written filter.
            commands.append(nextest + ["--test-threads=4", "--no-tests=pass"])
            if whole:
                doctest = ["cargo", "test", "--doc"]
                for package in packages:
                    doctest += ["-p", package]
                commands.append(doctest)
        failed = False
        for command in commands:
            print("cargo " + " ".join(command), flush=True)
            # Run every command so one failure doesn't hide the rest.
            status = subprocess.run(command, check=False,
                                    env={**os.environ, "CARGO_BUILD_JOBS": "4"}).returncode
            failed = failed or status != 0
        if failed:
            raise SystemExit(1)
    elif action == "finish":
        finish(data)
    else:
        raise SystemExit("unknown affected runner action")

def self_test():
    import contextlib
    import io
    from unittest.mock import patch

    def run(report, action, status=0):
        def finished(*args, **kwargs):
            return subprocess.CompletedProcess(args[0] if args else [], status)
        with patch("subprocess.run", side_effect=finished) as calls, contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            try:
                execute(report, action)
            except SystemExit as error:
                return error.code, [call.args[0] for call in calls.call_args_list]
            return 0, [call.args[0] for call in calls.call_args_list]

    def report(selection, mode="selective"):
        return {"ok": True, "data": {"mode": mode, "reasons": [], "selected": selection}}

    empty = {"rust": [], "rust_targets": [], "go": [], "suites": []}
    packages = ["core", "cli"]
    grouped = ["test", "-p", "core", "-p", "cli", "--lib", "--test", "smoke"]
    base = ["test", "-p", "core", "-p", "cli"]
    plan = {"rust": packages, "go": [], "suites": ["platform"], "rust_targets": [
        {"package": "core", "kind": "lib", "cargo_args": grouped},
        {"package": "cli", "kind": "test", "name": "smoke", "cargo_args": grouped}]}
    whole = {**plan, "rust_targets": [
        {"package": "core", "kind": "package", "cargo_args": base},
        {"package": "cli", "kind": "test", "name": "smoke", "cargo_args": base}]}
    legacy = {"rust": packages, "go": [], "suites": []}
    tampered = {**plan, "rust_targets": [
        plan["rust_targets"][0], {**plan["rust_targets"][1], "cargo_args": ["test"]}]}
    uncovered = {**plan, "rust_targets": plan["rust_targets"][:1]}
    option_target = {**plan, "rust_targets": [
        {"package": "core", "kind": "test", "name": "--help",
         "cargo_args": base + ["--test", "--help"]},
        {"package": "cli", "kind": "test", "name": "--help",
         "cargo_args": base + ["--test", "--help"]}]}
    bin_plan = {"rust": ["cli"], "go": [], "suites": [], "rust_targets": [
        {"package": "cli", "kind": "bin", "name": "mdkg",
         "cargo_args": ["test", "-p", "cli", "--bin", "mdkg"]}]}
    lib_plan = {"rust": ["core"], "go": [], "suites": [], "rust_targets": [
        {"package": "core", "kind": "lib", "cargo_args": ["test", "-p", "core", "--lib"]}]}
    filtered = ["cargo", "nextest", "run", "-p", "core", "-p", "cli",
                "-E", "kind(lib) or binary(smoke)", "--test-threads=4", "--no-tests=pass"]
    everything = ["cargo", "nextest", "run", "-p", "core", "-p", "cli",
                  "--test-threads=4", "--no-tests=pass"]
    doctests = ["cargo", "test", "--doc", "-p", "core", "-p", "cli"]
    cases = [
        (report(empty), "test", (0, [])),
        (report(empty), "clippy", (0, [])),
        (report(plan), "test", (0, [filtered])),
        (report(whole), "test", (0, [everything, doctests])),
        (report(legacy), "test", (0, [everything, doctests])),
        (report(bin_plan), "test", (0, [["cargo", "nextest", "run", "-p", "cli",
                                         "-E", "binary(mdkg)", "--test-threads=4", "--no-tests=pass"]])),
        (report(lib_plan), "test", (0, [["cargo", "nextest", "run", "-p", "core",
                                         "-E", "kind(lib)", "--test-threads=4", "--no-tests=pass"]])),
        (report(empty, "full"), "mode", (0, [])),
        (report(empty, "full"), "finish", (3, [])),
        ({"ok": False}, "test", None),
        (report(tampered), "test", None),
        (report(uncovered), "test", None),
        (report({**empty, "go": ["module"]}), "test", None),
        (report(plan), "clippy", (0, [["cargo", "clippy", "-p", "core", "-p", "cli", "--all-targets", "--quiet", "--", "-D", "warnings"]])),
        (report(option_target), "test", None),
        (report({**legacy, "rust": ["--help"]}), "test", None),
        (report({**empty, "suites": ["lint"]}), "mode", None),
        (report({**legacy, "suites": ["lint"]}), "clippy", (0, [["cargo", "clippy", "-p", "core", "-p", "cli", "--all-targets", "--quiet", "--", "-D", "warnings"]])),
        (report({**empty, "suites": ["quint"]}), "finish", (3, [])),
        (report({**empty, "suites": ["platform"]}), "finish", (3, [])),
        (report({**empty, "suites": ["release"]}), "finish", (3, [])),
        (report({**empty, "suites": ["workflows"]}), "finish", (0, [])),
        (report({**empty, "suites": ["scripts"]}), "finish", (3, [])),
        ({"ok": True, "data": {"mode": "full", "selected": empty, "paths": [{"selects": ["suite:quint", "suite:platform"]}]}}, "finish", (3, [])),
    ]
    for candidate, action, expected in cases:
        result = run(candidate, action)
        if expected is None:
            assert result[0] != 0 and result[1] == [], result
        else:
            assert result == expected, (result, expected)
    # A failing nextest run still runs the doctests, then fails the step.
    assert run(report(whole), "test", status=1) == (1, [everything, doctests])
    output = io.StringIO()
    with contextlib.redirect_stderr(output):
        try:
            finish(report(empty, "full")["data"])
        except SystemExit as error:
            assert error.code == 3
        else:
            raise AssertionError("full receipt passed without external checks")
    assert "scripts" in output.getvalue(), "full receipt omitted script checks"
    for listing, refused in [(b"h source.rs\0", True), (b"S source.rs\0", True), (b"H source.rs\0", False)]:
        try:
            index_flags(listing)
        except SystemExit:
            assert refused
        else:
            assert not refused
    try:
        check_head("changed", "head")
    except SystemExit:
        pass
    else:
        raise AssertionError("moved head was accepted")
    warning_report = report(empty)
    warning_report["data"]["warnings"] = [{"path": "src/new.rs", "packages": ["core"]}]
    output = io.StringIO()
    with contextlib.redirect_stderr(output), contextlib.redirect_stdout(io.StringIO()):
        execute(warning_report, "mode")
    assert "src/new.rs" in output.getvalue() and "core" in output.getvalue()
    from tempfile import TemporaryDirectory
    with TemporaryDirectory(prefix="mdkg-affected-eol-") as tmp:
        root = Path(tmp).resolve()
        def git(*args):
            return SCANNER["git_read"](root, *args).stdout
        git("-c", "init.templateDir=", "init", "--quiet")
        (root / ".gitattributes").write_bytes(b"* text eol=crlf\n")
        sample = root / "sample.txt"
        sample.write_bytes(b"one\ntwo\n")
        (root / ".gitignore").write_bytes(b"nested\n")
        nested = root / "nested"
        nested.mkdir()
        (nested / "sample.txt").write_bytes(b"nested\n")
        git("add", ".gitattributes", ".gitignore", "sample.txt")
        git("add", "-f", "nested/sample.txt")
        git("-c", "commit.gpgSign=false", "-c", "user.name=Fixture",
            "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "-m", "LF fixture")
        assert git("cat-file", "blob", "HEAD:sample.txt") == b"one\ntwo\n"
        with patch.dict(globals(), {"CHECKOUT": root}):
            require_clean()  # Unchanged LF bytes are accepted.
            sample.write_bytes(b"one\r\ntwo\r\n")
            try:
                require_clean()
            except SystemExit as error:
                assert "tracked bytes to match HEAD" in str(error), error
            else:
                raise AssertionError("eol=crlf normalized away changed working bytes")
            sample.write_bytes(b"one\ntwo\n")
            with TemporaryDirectory(prefix="mdkg-affected-parent-") as outside:
                target = Path(outside).resolve()
                (target / "sample.txt").write_bytes(b"nested\n")
                (nested / "sample.txt").unlink()
                nested.rmdir()
                nested.symlink_to(target, target_is_directory=True)
                try:
                    require_clean()
                except SystemExit as error:
                    assert "cannot read a tracked input" in str(error), error
                else:
                    raise AssertionError("tracked-byte guard followed a symlinked parent")
    coverage = {"templates": {"root_path": "docs/templates"}, "affected": {"rules": [
        {"paths": ["docs/templates/**"], "select": {"rust_targets": [{"package": "mdkg", "test": "affected"}]}}]}}
    template_coverage(coverage)
    coverage["templates"]["root_path"] = "other/templates"
    try:
        template_coverage(coverage)
    except SystemExit:
        pass
    else:
        raise AssertionError("configured template root lost its coverage")
    coverage_cases = [
        ({"root_path": "."}, "**"),
        ({"root_path": "./"}, "**"),
        ({"root_path": None}, ".mdkg/templates/**"),
        (None, ".mdkg/templates/**"),
        ({"root_path": "docs/templates"}, "**"),
    ]
    for templates, pattern in coverage_cases:
        template_coverage({"templates": templates, "affected": {"rules": [
            {"paths": [pattern], "select": "all"}]}})
    config_path = (CHECKOUT or Path.cwd()) / ".mdkg/config.json"
    if config_path.is_file():
        template_coverage(json.loads(config_path.read_text()))
    print(f"affected runner self-test passed ({len(cases) + 11 + len(coverage_cases)} cases)")


if sys.argv[2] == "self-test":
    self_test()
elif sys.argv[2] == "clean":
    require_clean()
elif sys.argv[2] == "path":
    print(SCANNER["trusted_path"](CHECKOUT or Path.cwd()))
else:
    with open(sys.argv[1]) as source:
        execute(json.load(source), sys.argv[2])

PY_AFFECTED
}

if [[ -n "$affected_base" ]]; then
  for check_git_name in ${!GIT_@}; do unset "$check_git_name"; done
  PATH=$(affected_cargo path)
  export PATH GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_OPTIONAL_LOCKS=0
  export GIT_NO_LAZY_FETCH=1 GIT_NO_REPLACE_OBJECTS=1 GIT_ALLOW_PROTOCOL= GIT_TERMINAL_PROMPT=0
  affected_cargo clean
  : "${MDKG_BIN:?set MDKG_BIN to the pinned mdkg executable for --affected}"
  affected_plan=$(mktemp "/tmp/mdkg-check-affected.XXXXXX")
  affected_args=(affected --targets --base "$affected_base" --json)
  [[ -z "$affected_main" ]] || affected_args+=(--main "$affected_main")
  if ! "$MDKG_BIN" "${affected_args[@]}" > "$affected_plan"; then
    cat "$affected_plan" >&2
    echo "affected verification failed; fix its diagnostic before treating the change as verified" >&2
    exit 1
  fi
  mode=$(affected_cargo mode)
  affected_cargo verify
fi

step "affected runner self-test"
affected_cargo self-test

# The tests run on cargo-nextest (decided 2026-10-30); doctests stay on
# `cargo test --doc`. Fail here, before the long gates, rather than fall back
# to cargo's one-binary-at-a-time test runs. PATH is final by now: pinned in
# affected mode above, inherited otherwise.
if ! command -v cargo-nextest >/dev/null 2>&1; then
  echo "cargo-nextest is required to run the tests: brew install cargo-nextest" >&2
  exit 1
fi

step "shipped credential scan"
"$CHECK_PYTHON" -I scripts/leak-scan.py --self-test
"$CHECK_PYTHON" -I scripts/leak-scan.py

step "negative process-group kill check"
"$CHECK_PYTHON" -I - <<'PY_KILL'
"""Reject direct negative process-group operands without an option separator."""
import pathlib
import re
import shlex
import subprocess
from tempfile import TemporaryDirectory

NEGATIVE = re.compile(r"^-(?:[0-9${%])")
SIGNAL = re.compile(r"^-(?:[A-Z][A-Z0-9]*|[0-9]{1,2})$")
NATIVE = re.compile(r'''(?P<rust>\bCommand::new)\s*\(\s*(?:system_tool\s*\(\s*)?"(?:/(?:usr/)?bin/)?kill"|["'](?:/(?:usr/)?bin/)?kill["']\s*,''')
SHELL = re.compile(r"(?<![\w.-])(?:/(?:usr/)?bin/)?kill[ \t]+([^\n;|&]*)")


def raw_literal(text, pos):
    """Return a hashed Rust raw string's value and end without reading comments."""
    if pos and (text[pos - 1].isalnum() or text[pos - 1] == "_"):
        return None
    match = re.match(r'(?:br|cr|r)(#+)"', text[pos:])
    if not match:
        return None
    start = pos + match.end()
    delimiter = '"' + match[1]
    end = text.find(delimiter, start)
    if end < 0:
        return text[start:], len(text)
    return text[start:end], end + len(delimiter)


def skip_trivia(text, pos):
    """Skip whitespace and host-language comments outside string literals."""
    while pos < len(text):
        if text[pos].isspace():
            pos += 1
        elif text.startswith("//", pos) or text[pos] == "#":
            newline = text.find("\n", pos)
            pos = newline if newline >= 0 else len(text)
        elif text.startswith("/*", pos):
            depth, pos = 1, pos + 2
            while pos < len(text) and depth:
                if text.startswith("/*", pos):
                    depth, pos = depth + 1, pos + 2
                elif text.startswith("*/", pos):
                    depth, pos = depth - 1, pos + 2
                else:
                    pos += 1
        else:
            break
    return pos


def expressions(text, start, closing):
    """Split arguments without losing unknown expressions or nested commas."""
    parts, stack, quote, escaped = [], [], None, False
    begin = pos = start
    pairs = {"(": ")", "[": "]", "{": "}"}
    while pos < len(text):
        if not quote:
            pos = skip_trivia(text, pos)
            if pos == len(text):
                break
            raw = raw_literal(text, pos)
            if raw is not None:
                pos = raw[1]
                continue
        char = text[pos]
        if quote:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
        elif char in ('"', "'", "`"):
            quote = char
        elif not stack and (char == "," or char == closing):
            part = text[begin:pos].strip()
            if part:
                parts.append(part)
            if char == closing:
                return parts
            begin = pos + 1
        elif char in pairs:
            stack.append(pairs[char])
        elif stack and char == stack[-1]:
            stack.pop()
        pos += 1
    if text[begin:].strip():
        parts.append(text[begin:].strip())
    return parts


def argument_value(expression):
    # Unknown values keep their position, notably the signal after -s/-n.
    pos = skip_trivia(expression, 0)
    if expression[pos:pos + 1] == "&":
        return argument_value(expression[pos + 1:])
    if expression[pos:pos + 1] == "(":
        parts = expressions(expression, pos + 1, ")")
        return argument_value(parts[0]) if len(parts) == 1 else ""
    expression = expression[pos:]
    raw = raw_literal(expression, 0)
    if raw is not None:
        return raw[0]
    formatted = re.match(r"format!\s*\(", expression)
    if formatted:
        parts = expressions(expression, formatted.end(), ")")
        return argument_value(parts[0]) if parts else ""
    literal = re.match(r'''^(?:[frb]+)?(["'`])''', expression)
    if not literal:
        return ""
    return expression[literal.end():].split(literal[1], 1)[0]


def native_args(chunk, rust):
    def array_start(text):
        pos = skip_trivia(text, 0)
        while text[pos:pos + 1] in ("&", "("):
            pos = skip_trivia(text, pos + 1)
        vector = re.match(r"vec!\s*", text[pos:])
        if vector:
            pos = skip_trivia(text, pos + vector.end())
        return pos + 1 if text[pos:pos + 1] == "[" else None

    if rust:
        args = []
        for call in re.finditer(r"\.arg(s?)\s*\(\s*", chunk):
            rest = chunk[call.end():]
            array = array_start(rest) if call[1] else None
            if array is not None:
                args.extend(argument_value(part) for part in expressions(rest, array, "]"))
            else:
                parts = expressions(rest, 0, ")")
                args.append(argument_value(parts[0]) if parts else "")
        return args
    # Python places the executable in the array; JS spawn passes it separately.
    rest = chunk[chunk.index(",") + 1:]
    array = array_start(rest)
    return [argument_value(part) for part in expressions(rest, array if array is not None else 0, "]")]


def unsafe_args(args):
    args = list(args)
    if args and args[0] in ("kill", "/bin/kill", "/usr/bin/kill"):
        args.pop(0)
    if args and args[0] in ("-s", "-n", "--signal"):
        args = args[2:]
    elif args and SIGNAL.fullmatch(args[0]):
        args = args[1:]
    for arg in args:
        if arg == "--":
            return False
        if NEGATIVE.match(arg):
            return True
    return False


def violations(text):
    found = set()
    # Shell files and shell fragments in Rust/Python/JS string literals.
    chars, offsets, pos = [], [], 0
    while pos < len(text):
        if text.startswith("\\\n", pos):
            pos += 2
            continue
        if text.startswith(('\\"', "\\n"), pos):
            chars.append('"' if text[pos + 1] == '"' else "\n")
            offsets.append(pos)
            pos += 2
        else:
            chars.append(text[pos])
            offsets.append(pos)
            pos += 1
    shell_text = "".join(chars)
    for match in SHELL.finditer(shell_text):
        try:
            args = shlex.split(match[1], comments=True)
        except ValueError:
            # Quotes in a surrounding host-language string can be incomplete.
            # Retain the operand starts rather than dropping the check.
            args = re.findall(r'''[^\s"']+''', match[1])
        if unsafe_args(args):
            found.add(text.count("\n", 0, offsets[match.start()]) + 1)
    # Native argument arrays/chains, including multiline Rust format! operands
    # and Python/JS formatted strings. Stop at the statement's semicolon.
    for match in NATIVE.finditer(text):
        chunk = ",".join(expressions(text, match.start(), ";"))
        # A call's .status/.spawn/.output ends its argument construction.
        chunk = re.split(r"\.(?:status|spawn|output)\s*\(", chunk, maxsplit=1)[0]
        args = native_args(chunk, match["rust"])
        if unsafe_args(args):
            found.add(text.count("\n", 0, match.start()) + 1)
    return sorted(found)


def source_text(path):
    try:
        return path.read_text()
    except FileNotFoundError:
        return None


def self_test():
    command = 'Command::' + 'new("kill")'
    for signal in ("-TERM", "-9", "-s TERM"):
        bad = "kill" + " " + signal + ' "-' + '$group"'
        good = "kill" + " " + signal + ' -- "-' + '$group"'
        assert violations(bad), bad
        assert not violations(good), good
    for operand in ('-1234', '-{pgid}', '-{}'):
        bad = command + '\n.args(["-KILL", &format!("' + operand + '", id)]).status();'
        good = command + '\n.args(["-KILL", "--", &format!("' + operand + '", id)]).status();'
        assert violations(bad), bad
        assert not violations(good), good
    bad = 'subprocess.run([' + '"ki' + 'll", "-9", f"-' + '{group}"])'
    assert violations(bad)
    assert not violations(bad.replace('"-9",', '"-9", "--",'))
    assert not violations(command + '.args(["-0", pid]).status();')
    assert not violations("kill" + ' -0 "$pid"')
    assert violations("kill" + " -TERM -1234 --"), "late separator must not waive a negative operand"
    executable = '"ki' + 'll"'
    forms = (
        ('subprocess.run([' + executable + ', {args}])', 'f"-{group}"'),
        ('spawn(' + executable + ', [{args}]);', '`-${group}`'),
        (command + '.args([{args}]).status();', '&format!("-{}", group)'),
    )
    for form, operand in forms:
        for signal in ('signal', 'choose_signal(a, b)'):
            for option in ('-s', '-n', '--signal'):
                args = '"' + option + '", ' + signal + ', ' + operand
                bad = form.replace('{args}', args)
                good = form.replace('{args}', args.replace(', ' + operand, ', "--", ' + operand))
                assert violations(bad), bad
                assert not violations(good), good
    bad = command + '.arg("-s").arg(signal).arg(format!("-{}", group)).status();'
    assert violations(bad), bad
    assert not violations(bad.replace('.arg(format!', '.arg("--").arg(format!'))
    for comment in ('/* process group */', '/* nested /* ,; */ comment */',
                    '// process group,;\n'):
        bad = command + '.args(["-s", signal, ' + comment + ' &format!("-{}", group)]).status();'
        good = bad.replace('signal,', 'signal, "--",')
        assert violations(bad), bad
        assert not violations(good), good
        bad = command + '.arg("-s").arg(signal).arg(format!(' + comment + ' "-{}", group)).status();'
        assert violations(bad), bad
        assert not violations(bad.replace('.arg(format!', '.arg("--").arg(format!'))
    for wrapper in ('({operand})', '(({operand}))', '(# group,;\n {operand})'):
        operand = wrapper.replace('{operand}', 'f"-{group}"')
        bad = 'subprocess.run([' + executable + ', "-s", signal, ' + operand + '])'
        assert violations(bad), bad
        assert not violations(bad.replace('signal,', 'signal, "--",'))
    for header in (' /* group,; */ [', '(& [', 'vec!['):
        trailer = '))' if header.startswith('(') else ')'
        bad = command + '.args(' + header + '"-s", signal, format!("-{}", group)]' + trailer + '.status();'
        assert violations(bad), bad
        assert not violations(bad.replace('signal,', 'signal, "--",'))
    for hashes in ('', '#', '##', '###'):
        def raw(value):
            return 'r' + hashes + '"' + value + '"' + hashes
        args = raw('-s') + ', signal, ' + raw('-1234')
        bad = command + '.args([' + args + ']).status();'
        good = bad.replace('signal,', 'signal, ' + raw('--') + ',')
        assert violations(bad), bad
        assert not violations(good), good
    args = 'r##"-s"##, r##"TERM \"# // /* ,;"##, r##"-1234"##'
    bad = command + '.args([' + args + ']).status();'
    assert violations(bad), bad
    assert not violations(bad.replace(', r##"-1234"##', ', r#"--"#, r##"-1234"##'))
    with TemporaryDirectory() as tmp:
        path = pathlib.Path(tmp) / 'deleted.rs'
        path.write_text('ordinary source')
        assert source_text(path) == 'ordinary source'
        path.unlink()
        assert source_text(path) is None
        try:
            source_text(path.parent)
        except OSError:
            pass
        else:
            raise AssertionError('unreadable source must not be skipped')
    print("negative kill self-test passed")


self_test()
listed = subprocess.run(["git", "ls-files", "-z", "--", "*.rs", "scripts"],
                        check=True, stdout=subprocess.PIPE).stdout
failed = False
for raw in listed.split(b"\0"):
    if not raw:
        continue
    path = pathlib.Path(raw.decode("utf-8", "surrogateescape"))
    if path.suffix not in (".rs", ".sh", ".py", ".mjs", ".js", ".rb"):
        continue
    if path.is_symlink():
        raise SystemExit(f"{str(path)!r}: source symlink needs review")
    text = source_text(path)
    if text is None:
        continue
    for line in violations(text):
        print(f"{str(path)!r}:{line}: negative process id requires -- before the operand")
        failed = True
if failed:
    raise SystemExit(1)
print("negative kill source check passed")
PY_KILL

step "test hygiene"
"$CHECK_PYTHON" -I - <<'PY_HYGIENE'
"""Catch, by grep, three test habits that passed here and failed on public CI.

Cheap and incomplete by design: `--ci-env` is the real check of each.
- A git commit with no identity in reach. macOS's git makes one up from
  the account; Linux's refuses. A Rust file that runs `git commit` must
  name an identity itself (user.name/user.email, GIT_AUTHOR_*/
  GIT_COMMITTER_*), unless the baseline below says where it gets one.
- A path climbing to this repository's own .mdkg. The public export's
  graph is a fresh `mdkg init`, so a test reading it needs a guard within
  WINDOW lines: one that prints "skipped", or handles NotFound.
- `env` with a working-directory option (GNU only; BSD's has none) or
  running the `command` builtin (BSD's env finds it as /usr/bin/command,
  GNU's can't).
"""
import pathlib
import re
import subprocess

WINDOW = 15
COMMIT = re.compile(r'"commit",\s*"-[a-z-]|\bgit(?:\s+-[cC]\s+\S+)*\s+commit\s+-')
IDENTITY = re.compile(r"user\.(?:name|email)|GIT_(?:AUTHOR|COMMITTER)_(?:NAME|EMAIL)")
# Files whose commits get an identity from a helper in another file, or
# that only print a command for a person to run. New entries need a reason.
IDENTITY_BASELINE = {
    "crates/mdkg-cli/src/doctor.rs": "a hint naming the command for the user",
    "crates/mdkg-cli/src/serve_cli/git_hook.rs": "git_serve::fakes sets GIT_AUTHOR_*",
    "crates/mdkg-run/src/turn/receipt/tasks/tests.rs": "receipt::tests::Repo sets user.*",
}
OWN_GRAPH = re.compile(
    r'(?:\bjoin|include_str!|include_bytes!)\(\s*"(?:\.\./)+\.mdkg(?:/|")'
    r'|\bjoin\(\s*"(?:\.\./)*\.\."\s*\)\s*\.join\(\s*"\.mdkg')
# A variable holding the repository's top (CARGO_MANIFEST_DIR climbed with
# "..") that a later line joins ".mdkg" onto.
TOP = re.compile(r'\blet\s+(?:mut\s+)?(\w+)\s*=[^;]*?CARGO_MANIFEST_DIR[^;]*?'
                 r'\.join\(\s*"\.\.(?:/\.\.)*/?"\s*\)')
GUARD = re.compile(r"\bskipped\b|\bNotFound\b")
ENV = re.compile(
    r"(?<![\w./-])(?:/usr/bin/)?env"
    r"(?:\s+(?:-[A-Za-z0-9]+|-u\s+\S+|[A-Za-z_][A-Za-z0-9_]*=\S*))*?"
    r"\s+(?:-[A-Za-z0-9]*C|--chdir\b|command\b)")


def problems(path, text):
    lines = text.splitlines()
    found = []
    if path.endswith(".rs"):
        if path not in IDENTITY_BASELINE and not IDENTITY.search(text):
            for number, line in enumerate(lines, 1):
                if COMMIT.search(line):
                    found.append((number, "commits with no git identity in this file "
                                  "(CI's Linux git refuses; name one, as -c user.name=…)"))
                    break
        tops = sorted({m.group(1) for m in TOP.finditer(text)})
        joined = re.compile(r"\b(?:%s)\s*\.join\(\s*(?:&?format!\(\s*)?\"\.mdkg"
                            % "|".join(map(re.escape, tops))) if tops else None
        for number, line in enumerate(lines, 1):
            if OWN_GRAPH.search(line) or (joined and joined.search(line)):
                near = lines[max(0, number - 1 - WINDOW):number + WINDOW]
                if not any(GUARD.search(other) for other in near):
                    found.append((number, "reads this repository's own .mdkg with no guard "
                                  "(print \"skipped\" or handle NotFound: the public "
                                  "export's graph is fresh)"))
    for number, line in enumerate(lines, 1):
        if ENV.search(line):
            found.append((number, "env's -C/--chdir is GNU-only, and env can't run "
                          "the `command` builtin on Linux; use git -C or cd"))
    return found


def self_test():
    commit = 'run(&["commit", "-q", "-m", "x"]);\n'
    assert problems("a.rs", commit)
    assert not problems("a.rs", commit + 'run(&["-c", "user.name=T"]);\n')
    assert not problems("a.rs", '.env("GIT_AUTHOR_NAME", "t");\n' + commit)
    assert not problems("a.rs", 'format!("git -c user.email=e commit -qm x")\n')
    assert problems("a.rs", 'format!("cd {d} && git commit -qm x")\n')
    assert not problems("a.rs", 'assert_eq!(data["commit"], sha);\n("commit", "-") => None,\n')
    assert not problems("crates/mdkg-cli/src/doctor.rs", commit)
    climb = 'let s = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.mdkg/templates");\n'
    assert problems("a.rs", climb)
    assert not problems("a.rs", climb + 'if !s.is_dir() { eprintln!("skipped: none"); }\n')
    assert not problems("a.rs", climb + 'Err(e) if e.kind() == ErrorKind::NotFound => {}\n')
    assert problems("a.rs", 'root.join("../..").join(".mdkg/config.json")\n')
    top = 'let repo = Path::new(env!("CARGO_MANIFEST_DIR"))\n    .join("../..");\n'
    assert problems("a.rs", top + 'read(repo.join(format!(".mdkg/t/{k}.md")))\n')
    assert problems("a.rs", top + 'read(repo.join(".mdkg/config.json"))\n')
    assert not problems("a.rs", top + 'read(repo.join("Cargo.toml"))\n')
    assert not problems("a.rs", top + 'if !repo.join(".mdkg").is_dir() {\n'
                        '    eprintln!("skipped: no graph");\n}\n')
    assert not problems("a.rs", 'dir.join(".mdkg/config.json")\n"lnk/../.mdkg",\n')
    assert not problems("a.rs", 'include_str!("../../../conformance/base/.mdkg/config.json")\n')
    # Spelled ENV here, so this file doesn't match itself.
    for bad in ['/usr/bin/ENV -C dir git', 'ENV -i PATH="$PATH" -C d x',
                'git() { ENV -i PATH="$PATH" command git "$@"; }', "ENV command -v x",
                "ENV --chdir=d x", "ENV -iC d x"]:
        bad = bad.replace("ENV", "env")
        assert problems("a.sh", bad + "\n"), bad
    for good in ['env -i PATH="$PATH" git -C d push', '.env("A", "b")', "printenv -C",
                 'env!("X")', "BSD env has no `-C`", "my_env command", "cd d && command -v x"]:
        assert not problems("a.sh", good + "\n"), good


self_test()
listed = subprocess.run(["git", "ls-files", "-z", "--", "crates", "scripts", "conformance"],
                        check=True, stdout=subprocess.PIPE).stdout
failed = False
for raw in listed.split(b"\0"):
    name = raw.decode("utf-8", "surrogateescape")
    if pathlib.PurePath(name).suffix not in (".rs", ".sh", ".py", ".mjs", ".js", ".rb"):
        continue
    path = pathlib.Path(name)
    if path.is_symlink() or not path.is_file():
        continue
    text = path.read_bytes().decode("utf-8", "replace")
    for number, message in problems(name, text):
        print(f"{name!r}:{number}: {message}")
        failed = True
if failed:
    raise SystemExit(1)
print("test hygiene check passed")
PY_HYGIENE

step "cargo fmt --check"
cargo fmt --all --check

step "cargo clippy"
if [[ "$mode" == selective ]]; then
  affected_cargo clippy
else
  cargo clippy --workspace --all-targets --quiet -- -D warnings
  # The Linux pip wheels build without FIDO2: lint the stub backend and
  # the no-FIDO2 hint that only that build compiles.
  cargo clippy -p mdkg-presence --no-default-features --all-targets --quiet -- -D warnings
fi

step "tests: nextest, then doctests"
# Tests see their temp folder through a symlink this script makes, on every
# platform, so a path bug that only a symlinked folder shows (macOS's /var
# is one; a no-follow open, say) fails here, not first at integration.
# The base is short (under /tmp, itself a symlink on macOS): a Unix socket
# path must fit SUN_LEN (104 bytes on macOS), and /var/folders/... plus a
# test's own names don't.
check_tmp=$(mktemp -d /tmp/mck.XXXXXX)
mkdir "$check_tmp/real"
ln -s real "$check_tmp/linked"
export TMPDIR="$check_tmp/linked"
[[ -L "$TMPDIR" && -d "$TMPDIR/" ]] || { echo "can't make the symlinked TMPDIR" >&2; exit 1; }
echo "TMPDIR=$TMPDIR (a symlink)"
# With the 0.6.0 CLI available, the write-command tests also check that it
# still reads every file mdkg writes to a 0.6 graph.
if [[ -x "$NODE24" && -f "$MDKG_TS_CLI" ]]; then
  export MDKG_ORACLE_NODE="$NODE24" MDKG_ORACLE_CLI="$MDKG_TS_CLI"
fi
if [[ "$mode" == selective ]]; then
  affected_cargo test
else
  # nextest runs every binary's tests in one pool, each test in its own
  # process (decided 2026-10-30). `--no-fail-fast` runs every test even after one
  # fails, so one failure doesn't hide the rest or cost a second full run
  # (decided 2026-10-30).
  # nextest runs no doctests, so they stay on cargo test. Both always run, and
  # the step fails afterwards, so a test failure doesn't hide a doctest one.
  tests_failed=0
  cargo nextest run --workspace --no-fail-fast || tests_failed=1
  cargo test --workspace --doc --no-fail-fast || tests_failed=1
  # The version line's "no FIDO2" follows mdkg-presence's resolved feature,
  # not the CLI's: feature unification can turn the backend on with the
  # CLI's `fido2` off. Rebuilds check that build, one with no FIDO2 at all
  # (the Linux pip wheels' build), and mdkg-presence's backend test there.
  cargo test -p mdkg -p mdkg-serve --no-default-features --features mdkg-serve/fido2 \
    --test version --no-fail-fast || tests_failed=1
  cargo test -p mdkg --no-default-features --test version --no-fail-fast || tests_failed=1
  cargo test -p mdkg-presence --no-default-features --lib --no-fail-fast fido2:: || tests_failed=1
  if ((tests_failed)); then
    exit 1
  fi
fi

step "pinned workflows"
# The release workflows publish: every action is pinned by commit, and dist
# is installed at a fixed version. publish.yml's crate publisher parses
# cargo's and crates.io's output, checked here rather than at release.
"$CHECK_PYTHON" -I scripts/pin-workflows.py --self-test
"$CHECK_PYTHON" -I scripts/pin-workflows.py --check
"$CHECK_PYTHON" -I scripts/publish-crates.py --self-test

step "third-party notices"
# Every release archive, the npm package and the wheels carry
# THIRD_PARTY_NOTICES.md; it must match the locked dependencies.
"$CHECK_PYTHON" -I scripts/third-party-notices.py --check
"$CHECK_PYTHON" -I scripts/third-party-notices.py --self-test

step "mdkg validate (TypeScript 0.6.0)"
# A graph `migrate` moved to 0.7.0 names itself in its config; 0.6.0 can't
# read it, so this step ends with the migration (orchestrator, 2026-10-24).
if grep -q '"graph_id"' .mdkg/config.json; then
  echo "skipped: this graph is on format 0.7 (migrated); 0.6.0 can't read it"
elif [[ -x "$NODE24" && -f "$MDKG_TS_CLI" ]]; then
  "$NODE24" "$MDKG_TS_CLI" validate 2>&1 \
    | grep -v -E 'mirror root missing|missing recommended heading|SQLite cache' \
    | tail -n 5
  "$NODE24" "$MDKG_TS_CLI" validate >/dev/null 2>&1
else
  echo "skipped: set MDKG_NODE24 and MDKG_TS_CLI to run the TypeScript validator"
fi

if [[ -n "$affected_base" ]]; then
  affected_cargo verify
  echo "Committed head observed before and after; writes restored during the run are not detected."
  affected_cargo finish
  step "local checks passed"
else
  step "all checks passed"
fi
