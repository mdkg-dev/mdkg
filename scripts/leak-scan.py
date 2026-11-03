#!/usr/bin/env python3
"""Scan source files for credential shapes without printing their values.

Use --root for an exported tree, or --files0-from - for a NUL-delimited
list relative to --root. Without either, scan Git's working files using
the literal KEEP/DROP/DROP_CASES data in export-public.policy when present,
or all tracked and non-ignored files in a public source tree. No checkout
selection code is executed.
No network requests or provider validation are performed.
"""
import sys
if not sys.flags.isolated:
    quoted = "'" + sys.argv[0].replace("'", "'\"'\"'") + "'"
    print("Run the scanner with isolated imports: python3 -I " + quoted, file=sys.stderr)
    raise SystemExit(2)

import argparse
from contextlib import ExitStack, redirect_stderr, redirect_stdout
import io
import json
import os
from pathlib import Path
import re
import subprocess
import stat
from tempfile import TemporaryDirectory

DISTRIBUTION_ROOT = Path(__file__).resolve().parent.parent
PINNED_GIT = None

# No left boundary: escaped newlines or arbitrary text cannot hide a key.
CREDENTIALS = [
    ("API key", re.compile(r"sk-(?:(?:proj|svcacct|admin|or-v1)-)?[A-Za-z0-9_]{20,}")),
    ("typed API key", re.compile(r"sk-(?:proj|svcacct|admin)-(?=[A-Za-z0-9_-]*[A-Z])[A-Za-z0-9_-]{20,}")),
    ("Anthropic key", re.compile(r"sk-ant-[A-Za-z0-9_-]{20,}")),
    ("GitHub token", re.compile(r"gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}")),
    ("GitLab token", re.compile(r"glpat-[A-Za-z0-9_-]{20,}")),
    ("npm token", re.compile(r"npm_[A-Za-z0-9]{30,}")),
    ("PyPI token", re.compile(r"pypi-[A-Za-z0-9_-]{30,}")),
    ("AWS key id", re.compile(r"(?:AKIA|ASIA)[0-9A-Z]{16}")),
    ("Stripe key", re.compile(r"(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{24,}|sk_org_[A-Za-z0-9_]{24,}|whsec_[A-Za-z0-9]{24,}")),
    ("Slack token", re.compile(r"xox[abpr]-[A-Za-z0-9-]{20,}")),
    ("Google API key", re.compile(r"AIza[A-Za-z0-9_-]{30,}")),
]
# Existing exact test exceptions, assembled so this scanner is safe to ship.
# New fixtures should use fragments instead of adding exceptions.
ALLOWED = {
    ("crates/mdkg-cli/tests/msg.rs", "AKIA" + "IOSFODNN7EXAMPLE"),
    ("crates/mdkg-cli/tests/msg.rs", "ghp_" + "abcdefghijklmnopqrstuvwxyz0123456789"),
    ("crates/mdkg-cli/src/migrate/mint.rs", "sk-" + "9" * 20),
}
PEM = re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----")
B64 = set("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=")
META = re.compile(r"[A-Za-z][A-Za-z-]*: [^\n\\]*")


def body_after(text, pos):
    """Count key body bytes across real/escaped newlines and PEM metadata."""
    count = 0
    while pos < len(text):
        if text[pos] in " \t\r\n":
            pos += 1
        elif text.startswith(("\\n", "\\r"), pos):
            pos += 2
        elif text[pos] in B64:
            match = META.match(text, pos)
            if match:
                pos = match.end()
                continue
            while pos < len(text) and text[pos] in B64:
                count += 1
                pos += 1
        else:
            break
    return count


def matches(text):
    for label, pattern in CREDENTIALS:
        for match in pattern.finditer(text):
            yield label, match
    for match in PEM.finditer(text):
        if body_after(text, match.end()) >= 16:
            yield "private key", match


def checkout_root(root=None):
    current = Path(root or Path.cwd()).resolve()
    if root is None:
        while not os.path.lexists(current / ".git") and current.parent != current:
            current = current.parent
    return current


def within_checkout(root, path):
    """Containment by filesystem identity, including case aliases."""
    identity = os.stat(root)
    candidate = Path(path).resolve()
    for parent in (candidate, *candidate.parents):
        try:
            info = os.stat(parent)
        except FileNotFoundError:
            continue
        if (info.st_dev, info.st_ino) == (identity.st_dev, identity.st_ino):
            return True
    return False


def trusted_path(root):
    root = checkout_root(root)
    paths = []
    for raw in os.environ.get("PATH", os.defpath).split(os.pathsep):
        directory = Path(raw)
        try:
            if (not directory.is_absolute() or not directory.is_dir()
                    or within_checkout(root, directory)
                    or within_checkout(DISTRIBUTION_ROOT, directory)):
                continue
        except OSError:
            continue
        paths.append(str(directory))
    return os.pathsep.join(paths)


def metadata_directory(path, opened):
    """Resolve links before '..', then validate the resolved directory identity."""
    path = Path(os.path.realpath(path))
    expected = os.stat(path, follow_symlinks=False)
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    fd = os.open(path.anchor, flags)
    opened.callback(os.close, fd)
    for part in path.parts[1:]:
        fd = os.open(part, flags, dir_fd=fd)
        opened.callback(os.close, fd)
    info = os.fstat(fd)
    if (info.st_dev, info.st_ino) != (expected.st_dev, expected.st_ino):
        raise ValueError("Git metadata directory changed during validation")
    if info.st_uid != os.getuid():
        raise ValueError("Git metadata directory is not owned by the current user")
    return fd


def metadata_file(directory, name, limit, optional=False):
    """One bounded, no-follow, owned regular-file read; no path in errors."""
    try:
        fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
    except FileNotFoundError:
        if optional:
            return None
        raise
    try:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_nlink > 1):
            raise ValueError("unexpected Git metadata file")
        data = bytearray()
        while chunk := os.read(fd, min(4096, limit + 1 - len(data))):
            data.extend(chunk)
            if len(data) > limit:
                raise ValueError("Git metadata exceeds its size bound")
        return bytes(data)
    finally:
        os.close(fd)


def metadata_path(data):
    value = data.rstrip(b"\r\n")
    if not value or any(byte in value for byte in (b"\0", b"\n", b"\r")):
        raise ValueError("unexpected Git metadata path")
    return Path(os.fsdecode(value))


def owned_checkout(root, allow_init=False):
    """Check open metadata descriptors; concurrent admin replacement is outside this gate."""
    try:
        with ExitStack() as opened:
            root_fd = metadata_directory(root, opened)
            try:
                info = os.stat(".git", dir_fd=root_fd, follow_symlinks=False)
            except FileNotFoundError:
                if allow_init:
                    return
                raise ValueError("no .git entry in the requested checkout") from None
            if stat.S_ISDIR(info.st_mode):
                git_fd = os.open(".git", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=root_fd)
                opened.callback(os.close, git_fd)
                if os.fstat(git_fd).st_uid != os.getuid():
                    raise ValueError("Git directory is not owned by the current user")
                gitdir = root / ".git"
            else:
                if not stat.S_ISREG(info.st_mode):
                    raise ValueError("unexpected .git entry")
                data = metadata_file(root_fd, ".git", 8192)
                if not data.startswith(b"gitdir: "):
                    raise ValueError("unexpected worktree .git entry")
                target = metadata_path(data[8:])
                gitdir = root / target
                git_fd = metadata_directory(gitdir, opened)
            common = metadata_file(git_fd, "commondir", 8192, optional=True)
            if common is not None:
                metadata_directory(gitdir / metadata_path(common), opened)
    except (OSError, ValueError):
        raise ValueError("cannot validate owned, bounded, no-follow Git metadata") from None


def pin_git(root, executable=None):
    """Pin one absolute executable outside the distribution and fixture roots."""
    import shutil
    global PINNED_GIT
    candidate = executable or PINNED_GIT or shutil.which("git", path=trusted_path(root))
    if (candidate is None or not Path(candidate).is_absolute()
            or within_checkout(root, candidate) or within_checkout(DISTRIBUTION_ROOT, candidate)
            or not Path(candidate).is_file() or not os.access(candidate, os.X_OK)):
        raise ValueError("no trusted Git executable outside checkout and distribution")
    PINNED_GIT = str(Path(candidate).resolve())
    return PINNED_GIT


def git_read(root, *args):
    # Only index/ref reads are used: no filters, diff or textconv.
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    safe_root = checkout_root(root)
    owned_checkout(safe_root, allow_init="init" in args)
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_SYSTEM=os.devnull,
               GIT_CONFIG_GLOBAL=os.devnull, GIT_OPTIONAL_LOCKS="0",
               GIT_EXTERNAL_DIFF="", GIT_PAGER="", GIT_ALLOW_PROTOCOL="",
               GIT_NO_LAZY_FETCH="1", GIT_NO_REPLACE_OBJECTS="1", GIT_TERMINAL_PROMPT="0", PATH=trusted_path(root))
    git = pin_git(safe_root)
    return subprocess.run(
        [str(Path(git).resolve()), "--no-pager", "-c", "core.fsmonitor=false",
         "-c", "core.hooksPath=" + os.devnull, "-c", "safe.directory=" + str(safe_root),
         "-c", "diff.external=", "-c", "core.excludesFile=" + os.devnull,
         "-c", "core.attributesFile=" + os.devnull, "-c", "protocol.allow=never", *args],
        cwd=root, env=env, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def working_paths(root):
    listed = git_read(root, "ls-files", "--cached", "--others", "--exclude-standard", "-z")
    paths = []
    for raw in listed.stdout.split(b"\0"):
        if not raw:
            continue
        rel = os.fsdecode(raw)
        try:
            os.lstat(root / rel)
        except FileNotFoundError:
            continue  # An unstaged deletion is not a working-file candidate.
        except OSError:
            pass  # Keep unreadable entries so scan reports the failure.
        paths.append(rel)
    return paths


def export_policy(root, revision=None):
    """Read a confined working policy or an immutable committed policy."""
    if revision is not None:
        if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", revision):
            raise ValueError("policy revision must be a resolved commit")
        row = git_read(root, "ls-tree", "-z", revision, "--", "scripts/export-public.policy").stdout
        if not row:
            raise ValueError("revision has no policy data; use its matching exporter")
        fields = row.split(b"\t", 1)[0].split()
        if len(fields) != 3 or fields[0] not in (b"100644", b"100755") or fields[1] != b"blob":
            raise ValueError("committed policy must be a regular file")
        oid = fields[2].decode("ascii")
        if int(git_read(root, "cat-file", "-s", oid).stdout) > 65536:
            raise ValueError("export policy exceeds 64 KiB")
        return parse_policy(git_read(root, "cat-file", "blob", oid).stdout)
    with ExitStack() as opened:
        flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
        root_fd = os.open(root, flags)
        opened.callback(os.close, root_fd)
        try:
            scripts_fd = os.open("scripts", flags, dir_fd=root_fd)
        except FileNotFoundError:
            return None
        opened.callback(os.close, scripts_fd)
        try:
            policy_fd = os.open("export-public.policy", os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW,
                                dir_fd=scripts_fd)
        except FileNotFoundError:
            return None
        opened.callback(os.close, policy_fd)
        if not stat.S_ISREG(os.fstat(policy_fd).st_mode):
            raise ValueError("export policy must be a regular file")
        chunks = bytearray()
        while data := os.read(policy_fd, 4096):
            chunks.extend(data)
            if len(chunks) > 65536:
                raise ValueError("export policy exceeds 64 KiB")
    return parse_policy(chunks)


def parse_policy(chunks):
    arrays = {}
    current = None
    headers = {"[keep]": "KEEP", "[drop]": "DROP", "[drop_cases]": "DROP_CASES"}
    for line in chunks.decode("utf-8").split("\n"):
        if not line or line.startswith("#"):
            continue
        if line in headers:
            current = headers[line]
            if current in arrays:
                raise ValueError("duplicate export policy section")
            arrays[current] = []
        elif (current is None
              or not re.fullmatch(r"[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*", line)
              or any(p in (".", "..") for p in line.split("/")) or line.startswith("-")):
            raise ValueError("export policy needs literal relative paths")
        else:
            arrays[current].append(line)
    if set(arrays) != {"KEEP", "DROP", "DROP_CASES"} or not arrays["KEEP"]:
        raise ValueError("export policy needs keep, drop and drop_cases sections")
    return arrays


def default_paths(root):
    """Only this scanner distribution can authorize policy narrowing."""
    if not os.path.samefile(root, DISTRIBUTION_ROOT):
        print("external checkout: policy narrowing disabled; scanning all tracked and non-ignored files", file=sys.stderr)
        return working_paths(root)
    policy = export_policy(DISTRIBUTION_ROOT)
    return policy_paths(root, policy) if policy is not None else working_paths(root)


def policy_paths(root, policy):
    """Candidate algorithm shared with the explicitly invoked exporter."""
    # Git's pathspecs preserve the exporter's directory/prefix semantics.
    listed = git_read(root, "ls-files", "--cached", "--others", "--exclude-standard", "-z",
                      "--", *policy["KEEP"])
    paths = []
    for raw in listed.stdout.split(b"\0"):
        if not raw:
            continue
        rel = os.fsdecode(raw)
        if rel in policy["DROP"] or any(
                rel.startswith(f"conformance/{kind}/{case}/")
                for case in policy["DROP_CASES"] for kind in ("cases", "expected")):
            continue
        try:
            os.lstat(root / rel)
        except FileNotFoundError:
            continue
        except OSError:
            pass  # Keep unreadable candidates so scan fails them.
        paths.append(rel)
    return paths


def git_root():
    top = git_read(None, "rev-parse", "--show-toplevel")
    return Path(os.fsdecode(top.stdout.removesuffix(b"\n")))


def candidate_bytes(root, rel):
    """Read a regular candidate or a link's text without following parents."""
    parts = Path(rel).parts
    if not parts:
        raise OSError("empty candidate path")
    with ExitStack() as opened:
        flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
        directory = os.open(root, flags)
        opened.callback(os.close, directory)
        for part in parts[:-1]:
            directory = os.open(part, flags, dir_fd=directory)
            opened.callback(os.close, directory)
        info = os.stat(parts[-1], dir_fd=directory, follow_symlinks=False)
        if stat.S_ISLNK(info.st_mode):
            return os.fsencode(os.readlink(parts[-1], dir_fd=directory))
        fd = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
        opened.callback(os.close, fd)
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise OSError("candidate must be a regular file or link")
        data = bytearray()
        while chunk := os.read(fd, 65536):
            data.extend(chunk)
            if b"\0" in data[:8192]:
                break
        return bytes(data)


def scan(root, paths=None, full=False):
    root = root.resolve()
    used = set()
    errors = []
    if paths is None:
        paths = []
        def unread(error):
            errors.append(f"{error.filename}: unreadable directory")
        for folder, dirs, files in os.walk(root, onerror=unread):
            dirs[:] = [d for d in dirs if d != ".git"]
            paths.extend(Path(folder, f).relative_to(root).as_posix() for f in files)
            paths.extend(Path(folder, d).relative_to(root).as_posix()
                         for d in dirs if Path(folder, d).is_symlink())
    for rel in paths:
        path = root / rel
        # Never traverse a symlink to read outside the selected tree.
        if Path(rel).is_absolute() or ".." in Path(rel).parts:
            errors.append(f"{rel!r}: path leaves scan root")
            continue
        try:
            data = candidate_bytes(root, rel)
        except OSError:
            errors.append(f"{rel!r}: unreadable file")
            continue
        if b"\0" in data[:8192]:
            continue
        text = data.decode("utf-8", "replace")
        for label, match in matches(text):
            pair = (rel, match.group(0))
            if pair in ALLOWED:
                used.add(pair)
            else:
                line = text.count("\n", 0, match.start()) + 1
                errors.append(f"{rel!r}:{line}: {label}")
    if full:
        errors.extend(f"{rel!r}: stale credential exception" for rel, _ in sorted(ALLOWED - used))
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 2
    print("credential scan passed")
    return 0


def self_test():
    cases = json.loads(Path(__file__).with_name("credential-cases.json").read_text())
    assert cases["version"] == 1
    for parts in cases["credentials"]:
        text = "".join(parts)
        assert list(matches(text)), f"missed shared credential case {parts[0]!r}"
        assert list(matches("literal\\n" + text)), "escaped prefix hid credential"
        for sep in cases["separators"]:
            assert list(matches("literal" + sep + text)), f"separator {sep!r} hid credential"
    for parts in cases["ordinary"]:
        text = "".join(parts)
        assert not list(matches(text)), f"ordinary text flagged: {text!r}"
    assert list(matches("-----BEGIN PRIVATE KEY-----\\n" + "A" * 16))
    assert not list(matches("-----BEGIN PRIVATE KEY-----\\nAAAA\\n"))
    secret = "".join(cases["credentials"][0])
    def captured(root, paths=None, full=False):
        output = io.StringIO()
        with redirect_stderr(output), redirect_stdout(output):
            code = scan(root, paths, full)
        assert secret not in output.getvalue(), "diagnostic disclosed a credential"
        return code
    with TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "bad file.txt").write_text(secret)
        assert captured(root) == 2
        (root / "bad file.txt").unlink()
        assert captured(root, ["missing.txt"]) == 2
        assert captured(root, ["../outside"]) == 2
        assert captured(root) == 0
        assert captured(root, full=True) == 2, "stale exceptions passed"
        (root / "link").symlink_to(secret)
        assert captured(root) == 2, "credential in link target passed"
        (root / "link").unlink()
        allowed_path, fake = sorted(ALLOWED)[0]
        target = root / allowed_path
        target.parent.mkdir(parents=True)
        target.write_text(fake)
        assert captured(root) == 0
        target.rename(target.with_name("wrong.rs"))
        assert captured(root) == 2, "exception passed at another path"
        # A selected path through a directory link must never be opened.
        (root / "alias").symlink_to(target.parent, target_is_directory=True)
        assert captured(root, ["alias/wrong.rs"]) == 2
        os.mkfifo(root / "fifo")
        assert captured(root, ["fifo"]) == 2, "candidate FIFO was not refused"
    with TemporaryDirectory() as tmp:
        root = Path(tmp)
        git_read(root, "-c", "init.templateDir=", "init", "--quiet")
        (root / ".gitignore").write_text("ignored.txt\n")
        (root / "tracked.txt").write_text("ordinary text")
        git_read(root, "add", ".gitignore", "tracked.txt")
        new = root / "new file\n.txt"
        new.write_text(secret)
        (root / "ignored.txt").write_text(secret)
        assert set(working_paths(root)) == {".gitignore", "tracked.txt", new.name}
        assert captured(root, working_paths(root)) == 2, "untracked credential passed"
        git_read(root, "add", new.name)
        assert captured(root, working_paths(root)) == 2, "staged credential passed"
        new.write_text("ordinary text")
        assert captured(root, working_paths(root)) == 0, "ignored file was selected"
        (root / "tracked.txt").rename(root / "moved.txt")
        (root / "moved.txt").write_text(secret)
        assert "tracked.txt" not in working_paths(root), "unstaged deletion was selected"
        assert "moved.txt" in working_paths(root), "move destination was omitted"
        assert captured(root, working_paths(root)) == 2, "credential in moved file passed"
        (root / "moved.txt").write_text("ordinary text")
        (root / "dangling").symlink_to("missing-target")
        assert "dangling" in working_paths(root), "dangling link was mistaken for a deletion"
        assert captured(root, working_paths(root)) == 0
        (root / "tracked.txt").write_text("ordinary text")
        scripts = root / "scripts"
        scripts.mkdir()
        exporter = scripts / "export-public.policy"
        policy = "[keep]\ntracked.txt\n[drop]\n[drop_cases]\n"
        exporter.write_text(policy)
        (scripts / "export-public.sh").write_text("touch should-not-run\n")
        assert policy_paths(root, export_policy(root)) == ["tracked.txt"], "private export selection was ignored"
        original_command = [sys.executable, "-I", str(Path(__file__).resolve())]
        fixture_scanner = scripts / "leak-scan.py"
        fixture_scanner.write_bytes(Path(__file__).read_bytes())
        command = [sys.executable, "-I", str(fixture_scanner)]
        new.write_text(secret)
        selected = subprocess.run(command, cwd=scripts, capture_output=True)
        assert selected.returncode == 0, "private scan included an excluded credential"
        assert not (scripts / "should-not-run").exists() and not (root / "should-not-run").exists(), "scanner executed checkout code"
        external = subprocess.run(original_command, cwd=root, capture_output=True)
        assert external.returncode == 2, "external checkout policy narrowed a trusted scanner"
        assert b"policy narrowing disabled" in external.stderr
        (root / "tracked.txt").write_text(secret)
        selected = subprocess.run(command, cwd=scripts, capture_output=True)
        assert selected.returncode == 2, "private scan missed a selected credential"
        (root / "tracked.txt").write_text("ordinary text")
        for invalid in (policy + "[keep]\n", policy.replace("[drop]\n", ""),
                        policy.replace("tracked.txt", "$(touch should-not-run)"),
                        policy + "KEEP+=(new-dir)\n",
                        policy + "declare -a KEEP=(new-dir)\n",
                        policy + "readonly KEEP=(new-dir)\n",
                        policy + "KEEP[0]=new-dir\n",
                        policy.replace("tracked.txt", "docs/c#notes")):
            exporter.write_text(invalid)
            selected = subprocess.run(command, cwd=scripts, capture_output=True)
            assert selected.returncode == 2, "invalid literal policy passed"
            assert not (scripts / "should-not-run").exists() and not (root / "should-not-run").exists(), "policy expansion executed"
        exporter.write_text(policy)
        exporter.unlink()
        os.mkfifo(exporter)
        selected = subprocess.run(command, cwd=root, capture_output=True, timeout=3)
        assert selected.returncode == 2, "special policy file passed"
        exporter.unlink()
        exporter.write_text(policy)
        from unittest.mock import patch
        real_open = os.open
        outside = root / "external-policy"
        outside.mkdir()
        (outside / "export-public.policy").write_text(policy.replace("tracked.txt", "other.txt"))
        retired = root / "retired-scripts"
        def swapped_parent(path, flags, *args, **kwargs):
            if path == "export-public.policy":
                scripts.rename(retired)
                scripts.symlink_to(outside, target_is_directory=True)
            return real_open(path, flags, *args, **kwargs)
        with patch("os.open", side_effect=swapped_parent):
            assert export_policy(root)["KEEP"] == ["tracked.txt"], "parent swap redirected policy read"
        scripts.unlink()
        retired.rename(scripts)
        other = root / "other-policy"
        exporter.rename(other)
        exporter.symlink_to(other)
        selected = subprocess.run(command, cwd=root, capture_output=True)
        assert selected.returncode == 2, "symlinked policy passed"
        exporter.unlink()
        other.unlink()
        exporter.write_text(policy)
        scripts.rename(root / "actual-scripts")
        scripts.symlink_to(root / "actual-scripts", target_is_directory=True)
        try:
            export_policy(root)
        except OSError:
            pass
        else:
            raise AssertionError("symlinked policy parent passed")
        scripts.unlink()
        (root / "actual-scripts").rename(scripts)
        exporter.unlink()
        new.write_text("ordinary text")
        assert set(default_paths(root)) == set(working_paths(root)), "public source fallback changed"
        selected = subprocess.run(command, cwd=scripts, capture_output=True)
        assert selected.returncode == 0, "bare public scan did not use Git working files"
        new.write_text(secret)
        selected = subprocess.run(command, cwd=scripts, capture_output=True)
        assert selected.returncode == 2, "bare public scan missed a selected credential"
    command = original_command
    with TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "selected.txt").write_text("ordinary text")
        (root / "unselected.txt").write_text(secret)
        selected = subprocess.run(command + ["--files0-from", "-"], cwd=root,
                                  input=b"selected.txt\0", capture_output=True)
        assert selected.returncode == 0, "explicit standalone list required a Git repository"
        (root / "selected.txt").write_text(secret)
        selected = subprocess.run(command + ["--files0-from", "-"], cwd=root,
                                  input=b"selected.txt\0", capture_output=True)
        assert selected.returncode == 2, "explicit standalone list missed a selected credential"
    with TemporaryDirectory() as tmp:
        root = Path(tmp)
        git_read(root, "-c", "init.templateDir=", "init", "--quiet")
        sub = root / "subfolder"
        sub.mkdir()
        # Explicit lists are relative to cwd, even below a Git top-level.
        # Opposite contents at the top-level detect accidental root discovery.
        for local, parent, expected in ((secret, "ordinary text", 2),
                                        ("ordinary text", secret, 0)):
            (sub / "selected.txt").write_text(local)
            (root / "selected.txt").write_text(parent)
            selected = subprocess.run(command + ["--files0-from", "-"], cwd=sub,
                                      input=b"selected.txt\0", capture_output=True)
            assert selected.returncode == expected, "explicit list changed its subfolder root"
    from types import SimpleNamespace
    from unittest.mock import patch
    raw = b"/root-\r-\xff\n"
    with patch("subprocess.run", return_value=SimpleNamespace(stdout=raw)):
        assert os.fsencode(git_root()) == raw[:-1], "Git root decoding changed POSIX path bytes"
    with TemporaryDirectory() as tmp:
        import shlex
        root = Path(tmp)
        git_read(root, "-c", "init.templateDir=", "init", "--quiet")
        (root / "README.md").write_text(secret)
        git_read(root, "add", "README.md")
        marker = root / "configured-code-ran"
        helper = root / "hostile-helper"
        helper.write_text("#!/bin/sh\n: > " + shlex.quote(str(marker)) + "\n")
        helper.chmod(0o700)
        git_read(root, "config", "core.fsmonitor", str(helper))
        git_read(root, "config", "diff.external", str(helper))
        git_read(root, "config", "diff.hostile.textconv", str(helper))
        hostile = {**os.environ, "GIT_EXTERNAL_DIFF": str(helper),
                   "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "core.fsmonitor",
                   "GIT_CONFIG_VALUE_0": str(helper),
                   "PATH": str(root) + os.pathsep + "." + os.pathsep + os.environ.get("PATH", os.defpath)}
        (root / "git").write_text(helper.read_text())
        (root / "git").chmod(0o700)
        for private in (False, True):
            if private:
                (root / "scripts").mkdir()
                (root / "scripts/export-public.policy").write_text(
                    "[keep]\nREADME.md\n[drop]\n[drop_cases]\n")
                copied = root / "scripts/leak-scan.py"
                copied.write_bytes(Path(__file__).read_bytes())
                selected_command = [sys.executable, "-I", str(copied)]
            else:
                selected_command = command
            selected = subprocess.run(selected_command, cwd=root, env=hostile, capture_output=True)
            assert selected.returncode == 2, "hostile Git settings skipped a selected credential"
            assert not marker.exists(), "scanner ran configured Git code"
    with TemporaryDirectory() as tmp:
        root = Path(tmp).resolve()
        git_read(root, "-c", "init.templateDir=", "init", "--quiet")
        real_fstat = os.fstat
        for target in (root, root / ".git"):
            identity = os.stat(target)
            observed = []
            def wrong_owner(fd):
                info = real_fstat(fd)
                if (info.st_dev, info.st_ino) == (identity.st_dev, identity.st_ino):
                    observed.append(True)
                    return SimpleNamespace(st_mode=info.st_mode, st_uid=os.getuid() + 1,
                                           st_dev=info.st_dev, st_ino=info.st_ino)
                return info
            with patch("os.fstat", side_effect=wrong_owner):
                try:
                    owned_checkout(root)
                except ValueError:
                    pass
                else:
                    raise AssertionError("foreign-owned metadata descriptor was trusted")
            assert observed, "ownership test did not observe its descriptor"
    with TemporaryDirectory() as tmp:
        base = Path(tmp).resolve()
        root, admin, common = (base / name for name in ("checkout", "admin", "common"))
        for directory in (root, admin, common):
            directory.mkdir()
        (root / ".git").write_bytes(b"gitdir: ../admin\n")
        metadata = admin / "commondir"
        metadata.write_bytes(b"../common\n")
        owned_checkout(root)
        external = base / "external"
        external.write_text("SYNTHETIC_PRIVATE_METADATA")
        metadata.unlink()
        metadata.symlink_to(external)
        for condition in ("link", "oversize", "nul", "fifo", "hardlink"):
            if condition != "link":
                metadata.unlink()
                if condition == "fifo":
                    os.mkfifo(metadata)
                elif condition == "hardlink":
                    os.link(external, metadata)
                else:
                    metadata.write_bytes(b"x" * 8193 if condition == "oversize" else b"../common\0\n")
            if condition == "hardlink":
                with ExitStack() as opened, patch("os.read", side_effect=AssertionError("hard-linked metadata was read")):
                    directory = metadata_directory(admin, opened)
                    try:
                        metadata_file(directory, "commondir", 8192)
                    except ValueError:
                        pass
                    else:
                        raise AssertionError("hard-linked metadata passed")
            try:
                owned_checkout(root)
            except ValueError as error:
                assert str(error) == "cannot validate owned, bounded, no-follow Git metadata"
            else:
                raise AssertionError("unsafe linked-worktree metadata accepted: " + condition)
    with TemporaryDirectory() as tmp:
        base = Path(tmp).resolve()
        root, owned, physical = (base / name for name in ("checkout", "owned", "physical"))
        for directory in (root, owned, physical):
            directory.mkdir()
        for directory, marker in ((owned, "lexical"), (physical, "physical")):
            git_read(directory, "-c", "init.templateDir=", "init", "--quiet")
            git_read(directory, "config", "fixture.destination", marker)
        (physical / "subdir").mkdir()
        (owned / "link").symlink_to(physical / "subdir", target_is_directory=True)
        target = owned / "link" / ".." / ".git"
        (root / ".git").write_text("gitdir: " + str(target) + "\n")
        with ExitStack() as opened:
            checked = os.fstat(metadata_directory(target, opened))
            actual = os.stat(target)
            assert (checked.st_dev, checked.st_ino) == (actual.st_dev, actual.st_ino), "symlink/.. selected lexical metadata"
        assert git_read(root, "config", "--local", "fixture.destination").stdout == b"physical\n"
        real_fstat = os.fstat
        def swapped_identity(fd):
            info = real_fstat(fd)
            return SimpleNamespace(st_dev=info.st_dev, st_ino=info.st_ino + 1)
        with ExitStack() as opened, patch("os.fstat", side_effect=swapped_identity):
            try:
                metadata_directory(target, opened)
            except ValueError:
                pass
            else:
                raise AssertionError("changed directory identity passed")
        broken = base / "not-a-directory"
        broken.write_text("ordinary text")
        real_stat = os.stat
        for failure in (PermissionError, NotADirectoryError):
            def inaccessible(path, *args, **kwargs):
                if Path(path) == broken:
                    raise failure("unusable PATH entry")
                return real_stat(path, *args, **kwargs)
            with patch.dict(os.environ, {"PATH": str(broken) + os.pathsep + "/usr/bin"}), patch("os.stat", side_effect=inaccessible):
                assert trusted_path(root) == "/usr/bin", "unusable PATH entry hid later Git"
    with TemporaryDirectory() as tmp:
        base = Path(tmp).resolve()
        distribution, fixture = base / "distribution", base / "fixture"
        distribution.mkdir()
        fixture.mkdir()
        (fixture / ".git").mkdir()
        fake = distribution / "git"
        fake.write_text("#!/bin/sh\nexit 99\n")
        fake.chmod(0o700)
        real_git = pin_git(DISTRIBUTION_ROOT)
        with patch.dict(globals(), {"DISTRIBUTION_ROOT": distribution, "PINNED_GIT": None}), patch.dict(os.environ, {"PATH": str(distribution)}):
            try:
                pin_git(fixture)
            except ValueError:
                pass
            else:
                raise AssertionError("fixture pinned Git from the source distribution")
        with patch.dict(globals(), {"DISTRIBUTION_ROOT": distribution, "PINNED_GIT": None}), patch.dict(os.environ, {"PATH": str(distribution) + os.pathsep + str(Path(real_git).parent)}):
            assert pin_git(fixture) == real_git
            with patch.dict(os.environ, {"PATH": str(distribution)}):
                assert pin_git(fixture) == real_git, "fixture re-resolved Git after pinning"
                assert git_read(fixture, "--version").stdout.startswith(b"git version ")
    with TemporaryDirectory() as tmp:
        base = Path(tmp).resolve()
        root = base / "case-checkout"
        root.mkdir()
        (root / ".git").mkdir()
        alias = base / "CASE-CHECKOUT"
        if alias.exists() and os.path.samefile(root, alias):
            marker = base / "case-git-ran"
            fake = root / "git"
            fake.write_text("#!/bin/sh\n: > " + shlex.quote(str(marker)) + "\n")
            fake.chmod(0o700)
            with patch.dict(os.environ, {"PATH": str(alias)}), patch.dict(globals(), {"PINNED_GIT": None}):
                assert trusted_path(root) == ""
                try:
                    git_read(root, "--version")
                except ValueError:
                    pass
                else:
                    raise AssertionError("case-aliased checkout git accepted")
            assert not marker.exists()
            with patch.dict(globals(), {"DISTRIBUTION_ROOT": root,
                    "export_policy": lambda path: {},
                    "policy_paths": lambda path, policy: ["narrowed"]}):
                assert default_paths(alias) == ["narrowed"], "case alias widened the distribution scan"
        else:
            print("skipped: checkout case-alias regression: case-sensitive filesystem", file=sys.stderr)
    print(f"credential self-test passed ({len(cases['credentials'])} shared shapes, filesystem checks)")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path)
    parser.add_argument("--files0-from", choices=["-"])
    parser.add_argument("--full", action="store_true", help="also reject unused exact exceptions")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    root = args.root
    if root is None and args.files0_from:
        root = Path.cwd()
    if root is None:
        try:
            root = git_root()
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            parser.error(str(error))
    paths = None
    if args.files0_from:
        paths = [os.fsdecode(p) for p in sys.stdin.buffer.read().split(b"\0") if p]
    elif args.root is None:
        try:
            paths = default_paths(root)
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            parser.error(str(error))
    return scan(root, paths, args.full)


if __name__ == "__main__":
    raise SystemExit(main())
