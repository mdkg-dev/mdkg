#!/usr/bin/env python3
"""Publishes the workspace's crates to crates.io one at a time, in
dependency order, waiting out crates.io's rate limit (a 429 names the time
to try again). A crate whose version is already there is skipped, so a
rerun after a partial publish picks up where it stopped.

.github/workflows/publish.yml runs it with a short-lived token from
crates.io's trusted publishing (CARGO_REGISTRY_TOKEN); it reads no other
credential.

Usage: scripts/publish-crates.py (--publish | --plan | --self-test)
                                 [--expect-version X.Y.Z] [--no-verify]
                                 [--deadline SECONDS]
  --publish     publish every crate not yet on crates.io
  --plan        print the order and exit; publishes nothing
  --self-test   check the ordering, the output parsing and the arguments
  --expect-version  refuse unless every crate has this version (the tag's)
  --no-verify   pass `--no-verify` to cargo (the workflow verified the
                workspace with `cargo publish --workspace --dry-run` first)
  --deadline    seconds from the start that a rate-limit wait may run to
                (default 1500); the trusted-publishing token lasts 30
                minutes, so a wait past it exits 3 naming the time to
                rerun after
Exactly one mode is required, and an unknown argument is refused, so
nothing publishes without `--publish`.
"""
import argparse
import contextlib
import datetime
import email.utils
import io
import json
import re
import subprocess
import sys
import time

ALREADY = re.compile(r"\bcrate\b[^\n]*\balready (?:exists|uploaded)")
RATE_LIMITED = re.compile(r"status 429|Too Many Requests")
# crates.io's 429 body: "... Please try again after <HTTP date> or email ..."
RETRY_AFTER = re.compile(r"try again after (.+? GMT)")
DEFAULT_WAIT = 60
MAX_ATTEMPTS = 10
DEADLINE = 1500


def publish_order(metadata):
    """The publishable workspace crates, each after its path dependencies
    (dev-dependencies excluded: cargo strips path-only ones); ties broken
    by name so the order is stable."""
    members = set(metadata["workspace_members"])
    crates = {
        p["name"]: p
        for p in metadata["packages"]
        if p["id"] in members and p.get("publish") != []
    }
    needs = {
        name: {
            d["name"]
            for d in p["dependencies"]
            if d.get("path") and d.get("kind") != "dev" and d["name"] in crates
        }
        for name, p in crates.items()
    }
    order = []
    while needs:
        ready = sorted(n for n, deps in needs.items() if not deps - set(order))
        if not ready:
            raise SystemExit(f"publish-crates: a dependency cycle among {sorted(needs)}")
        order.append(ready[0])
        del needs[ready[0]]
    return [(n, crates[n]["version"]) for n in order]


def classify(output):
    """`ok`, `exists`, (`wait`, seconds) or `failed`, from cargo's output
    when it exits non-zero."""
    if ALREADY.search(output):
        return "exists"
    if RATE_LIMITED.search(output):
        return ("wait", retry_seconds(output, now()))
    return "failed"


def retry_seconds(output, current):
    m = RETRY_AFTER.search(output)
    if not m:
        return DEFAULT_WAIT
    try:
        when = email.utils.parsedate_to_datetime(m.group(1))
    except (TypeError, ValueError):
        return DEFAULT_WAIT
    # A few seconds past the named time, so the limit has surely reset.
    return max(0, int((when - current).total_seconds())) + 5


def now():
    return datetime.datetime.now(datetime.timezone.utc)


def publish(name, no_verify):
    cmd = ["cargo", "publish", "-p", name, "--locked"]
    if no_verify:
        cmd.append("--no-verify")
    print(f"$ {' '.join(cmd)}", flush=True)
    done = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    print(done.stdout, end="", flush=True)
    return "ok" if done.returncode == 0 else classify(done.stdout)


def parse_args(argv):
    parser = argparse.ArgumentParser(
        prog="publish-crates.py",
        allow_abbrev=False,
        description="Publish the workspace's crates to crates.io in dependency order.",
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--publish", action="store_true", help="publish every crate not yet on crates.io")
    mode.add_argument("--plan", action="store_true", help="print the order; publishes nothing")
    mode.add_argument("--self-test", action="store_true", help="check this script; publishes nothing")
    parser.add_argument("--expect-version", help="refuse unless every crate has this version")
    parser.add_argument("--no-verify", action="store_true", help="pass --no-verify to cargo publish")
    parser.add_argument("--deadline", type=int, default=DEADLINE,
                        help=f"seconds from the start a rate-limit wait may run to (default {DEADLINE})")
    return parser.parse_args(argv)


def wait_fits(seconds, elapsed, deadline):
    """Whether a rate-limit wait of `seconds` ends by the deadline."""
    return elapsed + seconds <= deadline


def main():
    args = parse_args(sys.argv[1:])
    if args.self_test:
        return self_test()
    start = time.monotonic()
    expect = args.expect_version
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"],
        stdout=subprocess.PIPE, check=True, text=True,
    ).stdout)
    order = publish_order(metadata)
    wrong = [f"{n} {v}" for n, v in order if expect is not None and v != expect]
    if wrong:
        print(f"publish-crates: expected version {expect}, found {', '.join(wrong)}", file=sys.stderr)
        return 1
    if args.plan:
        for n, v in order:
            print(f"{n} {v}")
        return 0
    published, skipped = [], []
    for name, version in order:
        for _ in range(MAX_ATTEMPTS):
            result = publish(name, args.no_verify)
            if result == "ok":
                published.append(name)
                break
            if result == "exists":
                print(f"publish-crates: {name} {version} is already on crates.io; skipped", flush=True)
                skipped.append(name)
                break
            if result == "failed":
                print(f"publish-crates: {name} {version} failed; published: {published or 'none'}", file=sys.stderr)
                return 1
            seconds = result[1]
            if not wait_fits(seconds, time.monotonic() - start, args.deadline):
                until = (now() + datetime.timedelta(seconds=seconds)).strftime("%Y-%m-%d %H:%M:%S UTC")
                print(f"publish-crates: crates.io rate-limits {name} for {seconds}s; rerun after {until}."
                      f" Published: {published or 'none'}", file=sys.stderr)
                return 3
            print(f"publish-crates: rate-limited; waiting {seconds}s", flush=True)
            time.sleep(seconds)
        else:
            print(f"publish-crates: {name} still rate-limited after {MAX_ATTEMPTS} tries", file=sys.stderr)
            return 3
    print(f"publish-crates: published {published or 'none'}; already there {skipped or 'none'}")
    return 0


def self_test():
    def pkg(name, *deps, publish=None, dev=()):
        return {
            "id": f"{name}-id", "name": name, "version": "1.2.3", "publish": publish,
            "dependencies": [{"name": d, "path": f"../{d}", "kind": None} for d in deps]
            + [{"name": d, "path": f"../{d}", "kind": "dev"} for d in dev]
            + [{"name": "serde", "kind": None}],
        }

    packages = [
        pkg("mdkg", "mdkg-run", "mdkg-coord", "mdkg-format"),
        pkg("mdkg-run", "mdkg-coord-sqlite", "mdkg-graph"),
        pkg("mdkg-coord-sqlite", "mdkg-coord", "mdkg-log"),
        pkg("mdkg-coord", "mdkg-format", "mdkg-log"),
        pkg("mdkg-log", "mdkg-format", dev=("mdkg-run",)),
        pkg("mdkg-graph", "mdkg-format"),
        pkg("mdkg-format"),
        pkg("fixture", "mdkg", publish=[]),
    ]
    meta = {"packages": packages, "workspace_members": [p["id"] for p in packages]}
    at = datetime.datetime(2026, 10, 28, 12, 0, 0, tzinfo=datetime.timezone.utc)
    limited = ("error: failed to publish to registry at https://crates.io\n\nCaused by:\n"
               "  the remote server responded with an error (status 429 Too Many Requests): "
               "You have published too many new crates in a short period of time. Please try "
               "again after Wed, 28 Oct 2026 12:10:00 GMT or email help@crates.io to have your "
               "limit increased.")
    cases = [
        ([n for n, _ in publish_order(meta)],
         ["mdkg-format", "mdkg-graph", "mdkg-log", "mdkg-coord", "mdkg-coord-sqlite", "mdkg-run", "mdkg"]),
        (retry_seconds(limited, at), 605),
        (retry_seconds(limited, at + datetime.timedelta(hours=1)), 5),
        (retry_seconds("status 429 Too Many Requests", at), DEFAULT_WAIT),
        (retry_seconds("Please try again after not a date GMT or", at), DEFAULT_WAIT),
        (classify("error: crate mdkg-format@1.2.3 already exists on crates.io index"), "exists"),
        (classify("crate version `1.2.3` is already uploaded"), "exists"),
        (classify(limited)[0], "wait"),
        (classify("error: failed to verify package tarball"), "failed"),
        (classify("error: file already exists: target/package/x"), "failed"),
        (wait_fits(900, 500, 1500), True),
        (wait_fits(900, 700, 1500), False),
    ]
    bad = [(got, want) for got, want in cases if got != want]
    cyclic = {"packages": [pkg("a", "b"), pkg("b", "a")], "workspace_members": ["a-id", "b-id"]}
    try:
        publish_order(cyclic)
        bad.append(("no error", "a dependency cycle"))
    except SystemExit:
        pass
    # Nothing but `--publish` may publish: no mode, an unknown argument
    # (`--dry-run`) or `--help` stops before any cargo command.
    refused = ([], ["--dry-run"], ["--help"], ["--pub"], ["--publish", "--dead", "60"],
               ["--plan", "--publish"], ["--publish", "--deadline"])
    for argv in refused:
        try:
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                parsed = parse_args(argv)
            bad.append((vars(parsed), f"{argv} refused"))
        except SystemExit:
            pass
    if parse_args(["--publish", "--no-verify"]).deadline != DEADLINE:
        bad.append(("another deadline", DEADLINE))
    for got, want in bad:
        print(f"self-test: got {got!r}, want {want!r}", file=sys.stderr)
    if not bad:
        print(f"publish-crates self-test passed ({len(cases) + len(refused) + 2} cases)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
