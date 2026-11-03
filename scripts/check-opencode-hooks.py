#!/usr/bin/env python3
r"""Check the canonical mdkg plugin on OpenCode 2.1.0 or 2.2.5, outside a sandbox.

Build the candidate and its exporters first:
  cargo build --locked -p mdkg-run --example opencode-hook-plugin
  cargo build --locked -p mdkg-run --example opencode-worker-config
  cargo build --locked -p mdkg --bin mdkg
Then, from this checkout:
  python3 -I scripts/check-opencode-hooks.py --self-test
  node scripts/check-opencode-safety.mjs --self-test
  node scripts/check-opencode-plugin.mjs target/debug/examples/opencode-hook-plugin
  python3 -I scripts/check-opencode-hooks.py --live \
    --model opencode/deepseek-v4.1-flash-free > /tmp/opencode-hooks-result.json
  python3 -I scripts/check-opencode-hooks.py --live-permissions \
    > /tmp/opencode-permissions-result.json

--live-permissions starts no session and calls no model: it starts a loopback
server with this build's worker inline config (OPENCODE_CONFIG_CONTENT, from
the opencode-worker-config exporter) and checks that OpenCode's resolved
build agent denies mdkg's refusals and the credential-printing commands,
taking the last matching rule as OpenCode does. A release that ignored the
inline config would fail it.

This is a resolved-rule check, not a shell execution test: it does not prove
the native shell parser produces those resources, that plugins cannot alter
permissions, or that wrappers and direct credential-store reads are blocked.
It covers the inline config and last-matching-rule behavior for the listed
literal spellings only; real worker shell behavior needs separate live checks.

--live starts a loopback server, runs four scenarios with two queued replies,
and writes Git only in its freshly created temporary repository. It never uses the user's
OpenCode home, logins, service, plugins or MCP configuration. HOME, all XDG
roots and the server password are temporary; provider credentials are not
inherited. A free Zen model must be explicitly named. Managed process groups
and temporary files are cleaned up on exit, error or interruption. Stdout is
a JSON receipt; progress goes to stderr. Do not run --live under a prohibition
on Git writes. Requires Python 3.9+ on POSIX; the plugin mock check uses Node 24+.

The plugin comes from mdkg-run's generator, not a copied implementation. A
wrapper observes real hook input/output and assigns each test session its
own actor. For the stop case it inserts one mail AFTER start has read the
inbox and BEFORE returning context, making the stop interleaving definite.
Mail bodies are inert sentinels and must never reach hook output or model
messages. A separate fixture plugin removes every tool except write and
permits only the edit session's guard.txt target. Deny-by-default permissions
and policies also block reads, shell execution and external access. Before
starting OpenCode, the driver rejects any system, managed or ancestor
configuration source outside the fixture, including an inherited TMPDIR
under a project. It does not remove or alter those external sources.

API references (v2, checked 2026-10-26):
https://dev.opencode.ai/v2/docs/cli/commands/
https://dev.opencode.ai/v2/docs/api#session.message.list
https://dev.opencode.ai/v2/docs/build/plugins#tools
https://github.com/anomalyco/opencode/blob/v2.1.0/packages/core/src/config/discovery.ts
Session messages, including replies after queued prompts, are the evidence;
run --server's stdout is not used to count later turns. --self-test is offline
and invokes no Git commands, OpenCode process, network or model.
"""

import argparse
from contextlib import contextmanager
import errno
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import secrets
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch


class CheckFailed(RuntimeError):
    pass


class ObserverStopped(BaseException):
    pass


@contextmanager
def defer_interrupts():
    """Register a spawned process before a single INT/TERM can unwind it.

    Use handlers, not a blocked signal mask, so the child does not inherit
    blocked shutdown signals. The caller's cleanup must enclose this scope.
    """
    pending = []
    handlers = {}

    def remember(signum, frame):
        if not pending:
            pending.append((signum, frame))

    try:
        for signum in (signal.SIGINT, signal.SIGTERM):
            handler = signal.getsignal(signum)
            if handler != signal.SIG_IGN:
                handlers[signum] = handler
                signal.signal(signum, remember)
        yield
    finally:
        for signum, handler in handlers.items():
            signal.signal(signum, handler)
        if pending:
            signum, frame = pending[0]
            handler = handlers[signum]
            if callable(handler):
                handler(signum, frame)
            else:
                raise KeyboardInterrupt


# The releases mdkg's preflight accepts (OPENCODE_QUALIFIED in
# crates/mdkg-run/src/profile.rs).
QUALIFIED = ("2.1.0", "2.2.5")


def qualified_version(text):
    text = text.strip()
    version = text[len("opencode v"):] if text.startswith("opencode v") else text
    return version if version in QUALIFIED else None


def wildcard(pattern, value):
    """OpenCode's permission pattern: `*` any text, `?` one character."""
    regex = "".join(".*" if c == "*" else "." if c == "?" else re.escape(c) for c in pattern)
    return re.fullmatch(regex, value, re.S) is not None


def last_effect(rules, action, resource):
    for rule in reversed(rules):
        if wildcard(rule["action"], action) and wildcard(rule["resource"], resource):
            return rule["effect"]
    return "ask"


def permission_probes(mdkg):
    """Shell commands a worker's inline config must deny, and ones it must not."""
    deny = ["mdkg auth status", "mdkg abandon fixture-task", f"{mdkg} auth key anthropic",
            f"{mdkg.name} agents use-key lane", "opencode auth export", "opencode auth list",
            "/opt/homebrew/bin/opencode auth export", "opencode service get password",
            "opencode api credential.list", "opencode debug config", "opencode pair",
            "opencode serve --port 4097", "claude setup-token",
            # The desktop app's CLI, spacing, quoting and global options.
            "/Applications/OpenCode.app/Contents/Resources/opencode-cli auth export",
            "opencode  auth\texport", 'opencode "auth" export',
            "opencode --log-level debug auth export", "./opencode auth export",
            "../bin/opencode-cli auth export", "'./opencode' auth export",
            '"/Applications/OpenCode.app/Contents/Resources/opencode-cli" auth export',
            '"opencode" auth export', "~/bin/opencode auth export", "$HOME/bin/opencode auth export",
            '"$HOME/bin/opencode" auth export', "env NO_COLOR=1 /opt/bin/opencode auth export",
            "command opencode auth export", "exec opencode auth export", "nohup opencode auth export",
            "timeout 10 opencode auth export", "NO_COLOR=1 /opt/bin/opencode auth export"]
    other = ["git status", "cargo test", "opencode --version", "opencode service status",
             "claude auth status", "cat scripts/opencode-catalog-env.py crates/mdkg-run/src/auth.rs",
             "rg auth scripts/opencode-catalog-env.py", "cat /opt/bin/opencode docs/auth.md",
             "grep auth ./bin/opencode",
             "cat scripts/claude-helper.py crates/mdkg-run/src/setup-token.rs"]
    return deny, other


def require(condition, message):
    if not condition:
        raise CheckFailed(message)


def isolated_env(root, password):
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "en_US.UTF-8",
           "TERM": "dumb", "NO_COLOR": "1", "HOME": str(root / "home"),
           "OPENCODE_PASSWORD": password, "GIT_CONFIG_GLOBAL": "/dev/null",
           "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0",
           "GIT_AUTHOR_NAME": "mdkg hook check", "GIT_AUTHOR_EMAIL": "test@example.invalid",
           "GIT_COMMITTER_NAME": "mdkg hook check", "GIT_COMMITTER_EMAIL": "test@example.invalid"}
    for name in ("CONFIG", "DATA", "STATE", "CACHE", "RUNTIME"):
        env[f"XDG_{name}_HOME" if name != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(root / name.lower())
    for name in ("TMPDIR", "TMP", "TEMP"):
        env[name] = str(root / "tmp")
    for key, value in env.items():
        if key == "HOME" or key.startswith("XDG_") or key in ("TMPDIR", "TMP", "TEMP"):
            Path(value).mkdir(mode=0o700, parents=True, exist_ok=True)
    return env


def system_config_paths():
    """Pinned 2.1.0 config/discovery.ts; no CLI control isolates these."""
    if sys.platform == "linux":
        return [Path("/etc/opencode")]
    require(sys.platform == "darwin", "configuration preflight supports macOS and Linux only")
    try:
        users = {pwd.getpwuid(uid).pw_name for uid in (os.getuid(), os.geteuid())}
    except (KeyError, OSError) as exc:
        raise CheckFailed("cannot establish the OS account for managed configuration checks") from exc
    require(all(user and "/" not in user and user not in (".", "..") for user in users),
            "invalid OS account for managed configuration checks")
    managed = Path("/Library/Managed Preferences")
    return [Path("/Library/Application Support/opencode"), managed / "ai.opencode.managed.plist",
            *(managed / user / "ai.opencode.managed.plist" for user in sorted(users))]


def external_config_paths(start):
    paths = system_config_paths()
    for parent in (start, *start.parents):
        # Whole directories can load code/skills even without a JSON file.
        # Reject ancestor repositories too, before mdkg/Git fixture setup.
        paths.extend(parent / name for name in
                     ("opencode.json", "opencode.jsonc", ".opencode", ".claude", ".agents",
                      "AGENTS.md", ".git", ".mdkg"))
    return paths


def reject_present_config(paths):
    for path in paths:
        try:
            path.lstat()
        except FileNotFoundError:
            continue
        except OSError as exc:
            raise CheckFailed(f"cannot inspect external configuration: {path}") from exc
        raise CheckFailed(f"external configuration source exists: {path}")


def preflight_external_config(start):
    reject_present_config(external_config_paths(Path(start).resolve(strict=True)))


def fixture_config():
    # Policies cannot be relaxed by an agent rule or --auto. The fixture
    # plugin further narrows edit to the write tool and checks its target.
    return {
        "update": "disable",
        "formatter": False,
        "permissions": [{"action": "*", "resource": "*", "effect": "deny"},
                        {"action": "edit", "resource": "guard.txt", "effect": "allow"}],
        "policies": [{"action": "tool.use", "resource": "*", "effect": "deny"},
                     {"action": "tool.use", "resource": "edit:guard.txt", "effect": "allow"},
                     {"action": "provider.use", "resource": "*", "effect": "deny"},
                     {"action": "provider.use", "resource": "opencode", "effect": "allow"}],
    }


def redact(value, password):
    if isinstance(value, str):
        return value.replace(password, "[redacted]") if password else value
    if isinstance(value, dict):
        return {k: redact(v, password) for k, v in value.items()}
    if isinstance(value, list):
        return [redact(v, password) for v in value]
    return value


def file_sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def stop_process(process, grace=2, wait_for_group=False):
    """Every managed command owns its process group; never use pkill/pgrep."""
    # A finished group leader can leave children holding pipes or a waiter.
    # Signal the owned group even when the leader already exited.
    def signal_group(sig):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            return False
        return True

    deadline = time.monotonic() + grace
    signal_group(signal.SIGTERM)
    try:
        process.wait(timeout=grace)
    except subprocess.TimeoutExpired:
        pass
    if wait_for_group:
        # Server children need time to record shutdown even if the server
        # itself exits before them.
        time.sleep(max(0, deadline - time.monotonic()))
    # Force-stop any remainder in this command's own group, including a
    # leader that ignored TERM. No process-inspection probe is needed.
    signal_group(signal.SIGKILL)
    process.wait(timeout=2)


def command(argv, cwd, env, timeout=20, own_group=True):
    # A CLI can exit before its large pipe-backed stdout has drained.
    # A private regular file also lets us read the entire result to EOF.
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8", newline="") as output:
        process = subprocess.Popen([str(a) for a in argv], cwd=cwd, env=env,
                                   stdin=subprocess.DEVNULL, stdout=output,
                                   stderr=subprocess.PIPE, text=True, start_new_session=own_group)
        try:
            _, err = process.communicate(timeout=timeout)
            output.seek(0)
            out = output.read()
            require(process.returncode == 0,
                    f"{Path(argv[0]).name} {argv[1]} exited {process.returncode}: {(err or out)[-2000:]}")
            return out
        except subprocess.TimeoutExpired as exc:
            raise CheckFailed(f"{Path(argv[0]).name} {argv[1]} timed out") from exc
        finally:
            try:
                if own_group:
                    stop_process(process)
                elif process.poll() is None:
                    # Observer children share the server's group.
                    process.terminate()
                    try:
                        process.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=2)
            finally:
                process.stderr.close()


def command_json(raw, operation):
    try:
        return json.loads(raw)
    except json.JSONDecodeError as exc:
        raise CheckFailed(f"{operation} returned invalid JSON ({len(raw.encode('utf-8'))} bytes; "
                          f"character {exc.pos})") from None


def mdkg_json(cfg, actor, args):
    raw = command([cfg["mdkg"], "--root", cfg["repo"], "--as", actor, *args, "--json"],
                  cfg["repo"], os.environ.copy(), own_group=False)
    value = json.loads(raw)
    require(isinstance(value, dict), "mdkg envelope must be an object")
    require(value.get("ok") is True, "mdkg command returned an unsuccessful envelope")
    return value["data"]


def append_event(path, value):
    raw = (json.dumps(value, separators=(",", ":")) + "\n").encode()
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    try:
        require(os.write(fd, raw) == len(raw), "short observer log write")
    finally:
        os.close(fd)


def guard_target(value, repo):
    args = value.get("args")
    if value.get("tool") != "write" or not isinstance(args, dict):
        return False
    path = args.get("path", args.get("filePath", args.get("file_path")))
    if not isinstance(path, str) or not path:
        return False
    return (Path(repo) / path).resolve() == (Path(repo) / "guard.txt").resolve()


def hook_driver(config, events, argv):
    """Transparent real-mdkg invocation, except the documented stop barrier."""
    event = argv[1] if len(argv) > 1 and argv[1] in ("start", "edit", "stop", "wait") else "observer"
    record = {"call": secrets.token_hex(16), "session": None, "event": event}
    cfg, began = None, False

    def stopped(_signum, _frame):
        raise ObserverStopped()

    signal.signal(signal.SIGTERM, stopped)
    try:
        cfg = json.loads(Path(config).read_text())
        require(cfg["events"] == str(events), "observer log does not match configuration")
        raw = sys.stdin.buffer.read(1_048_577)
        require(len(raw) <= 1_048_576, "oversized hook input")
        value = json.loads(raw)
        require(isinstance(value, dict), "hook input must be an object")
        require(len(argv) == 4 and argv[0] == "hook" and argv[2:] == ["--harness", "opencode"]
                and event != "observer", "unexpected plugin command")
        session = value.get("sessionID")
        require(isinstance(session, str) and session in cfg["sessions"], "plugin supplied no recognized sessionID")
        record["session"] = session
        phase = cfg["sessions"][session]
        actor = "probe-" + phase
        append_event(events, {**record, "stage": "begin"})
        began = True
        args = [cfg["mdkg"], "--root", cfg["repo"], "--as", actor, *argv]
        if event == "wait":
            args += ["--timeout", str(cfg["timeout"])]
        result = subprocess.run(args, input=raw, cwd=cfg["repo"], capture_output=True,
                                timeout=cfg["timeout"] + 15)
        output = json.loads(result.stdout)
        require(isinstance(output, dict) and all(k in ("context", "deny", "block", "wake")
                and isinstance(v, str) for k, v in output.items()), "invalid mdkg hook output")
        if phase == "stop" and event == "start" and result.returncode == 0:
            try:
                fd = os.open(cfg["stop_once"], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            except FileExistsError:
                pass
            else:
                os.close(fd)
                mail = mdkg_json(cfg, cfg["stop_sender"], ["msg", "send", "--to", actor,
                                 "--kind", "note", "--text", cfg["sentinel"]])["message"]
                append_event(events, {"session": session, "event": "stop_mail",
                                      "stage": "end", "seq": mail["seq"]})
        sys.stdout.buffer.write(result.stdout)
        sys.stdout.buffer.flush()
        sys.stderr.buffer.write(result.stderr)
        sys.stderr.buffer.flush()
        append_event(events, {**record, "stage": "end", "code": result.returncode, "out": output,
                              "guard_target": event == "edit" and guard_target(value, cfg["repo"]),
                              "stderr": result.stderr.decode(errors="replace")})
        return result.returncode
    except ObserverStopped:
        if began and event == "wait" and cfg and Path(cfg["shutdown"]).exists():
            append_event(events, {**record, "stage": "cancelled", "code": 0})
            return 0
        append_event(events, {**record, "stage": "end", "code": 1,
                              "stderr": "observer interrupted before completing the hook"})
        return 1
    except Exception as exc:
        append_event(events, {**record, "stage": "end", "code": 1,
                              "stderr": f"hook observer failed: {exc}"})
        print(f"hook observer failed: {exc}", file=sys.stderr)
        return 1


def message_text(message):
    if message.get("type") == "assistant":
        return "\n".join(p.get("text", "") for p in message.get("content", []) if p.get("type") == "text")
    return message.get("text", "")


def tool_parts(messages):
    return [part for m in messages if m.get("type") == "assistant"
            for part in m.get("content", []) if part.get("type") == "tool"]


def assistant_said(messages, text):
    return any(m.get("type") == "assistant" and m.get("finish") == "stop"
               and text in message_text(m) for m in messages)


def queued_notices(messages, seq):
    return [i for i, message in enumerate(messages) if message["type"] == "user"
            and message_text(message).startswith("You have mdkg mail:")
            and f"#{seq} " in message_text(message)]


def answered_notice(messages, seq, marker):
    return any(assistant_said(messages[i + 1:], marker) for i in queued_notices(messages, seq))


def message_pages(fetch):
    """v2 cursors are opaque; do not combine order with a cursor."""
    messages, seen = [], set()
    params = {"limit": "100", "order": "asc"}
    for _ in range(20):
        page = fetch(params)
        require(isinstance(page, dict) and isinstance(page.get("data"), list), "unexpected v2 message envelope")
        require(all(isinstance(m, dict) and "type" in m for m in page["data"]), "unexpected v2 message shape")
        messages.extend(page["data"])
        cursors = page.get("cursor")
        require(cursors is None or isinstance(cursors, dict), "invalid message cursor envelope")
        cursor = (cursors or {}).get("next")
        if cursor is None:
            return messages
        require(isinstance(cursor, str) and cursor not in seen, "invalid/repeated message cursor")
        seen.add(cursor)
        params = {"limit": "100", "cursor": cursor}
    raise CheckFailed("session exceeded the message-page limit")


def evidence(messages):
    """Keep only test evidence, never provider-native payloads or credentials."""
    return [{"id": m.get("id"), "type": m["type"], "text": message_text(m),
             "finish": m.get("finish"),
             "tools": [{"name": p.get("name"), "status": p.get("state", {}).get("status"),
                        "error": p.get("state", {}).get("error")} for p in tool_parts([m])]}
            for m in messages]


class LiveCheck:
    def __init__(self, root, args, report, password):
        self.root, self.args, self.report = root, args, report
        self.repo = root / "project"
        self.repo.mkdir()
        self.env = isolated_env(root, password)
        self.sessions = {phase: "ses_mdkg" + secrets.token_hex(12) for phase in ("context", "edit", "stop", "wake")}
        self.events = root / "events.jsonl"
        self.shutdown = root / "shutdown"
        self.sentinel = "BODY_MUST_STAY_OUT_" + secrets.token_hex(16)
        self.server = None
        self.url = ""
        self.safety_ready = root / "safety-ready.json"
        self.safety_token = secrets.token_hex(32)
        self.env["MDKG_CHECK_SAFETY"] = json.dumps({
            "directory": str(self.repo), "session": self.sessions["edit"],
            "ready": str(self.safety_ready), "token": self.safety_token})
        self.cfg = {"mdkg": str(args.mdkg), "repo": str(self.repo), "events": str(self.events),
                    "sessions": {sid: p for p, sid in self.sessions.items()}, "timeout": args.timeout,
                    "stop_once": str(root / "stop-once"), "stop_sender": "stop-" + secrets.token_hex(6),
                    "sentinel": self.sentinel, "shutdown": str(self.shutdown)}

    def run(self, argv, timeout=20):
        if str(argv[0]) == str(self.args.opencode):
            preflight_external_config(self.root.parent)
        return command(argv, self.repo, self.env, timeout)

    def mdkg(self, actor, *args):
        value = json.loads(self.run([self.args.mdkg, "--as", actor, *args, "--json"]))
        require(value.get("ok") is True, "mdkg returned an unsuccessful envelope")
        return value["data"]

    def api(self, operation, **params):
        argv = [self.args.opencode, "api", "--server", self.url, operation]
        for key, value in params.items():
            argv += ["--param", f"{key}={value}"]
        return command_json(self.run(argv), operation)

    def messages(self, phase):
        messages = message_pages(lambda params: self.api("session.message.list", sessionID=self.sessions[phase], **params))
        require(self.sentinel not in json.dumps(messages), "mail body sentinel reached session messages")
        self.report["sessions"][phase] = evidence(messages)
        return messages

    def observed(self, phase, event=None, stage="end"):
        rows = self.event_rows()
        self.verify_events(rows)
        return [r for r in rows if r["session"] == self.sessions[phase] and r["stage"] == stage
                and (event is None or r["event"] == event)]

    def verify_events(self, rows, final=False):
        require(self.sentinel not in json.dumps(rows), "mail body sentinel reached hook output")
        pending, finished = {}, set()
        for row in rows:
            if row["stage"] == "end":
                require(row.get("code", 0) == 0 and not row.get("stderr", "").strip(),
                        f"hook {row['event']} reported a failure or diagnostic")
            if row["event"] == "stop_mail":
                continue
            call = row.get("call")
            require(isinstance(call, str), "observer invocation has no id")
            if row["stage"] == "begin":
                require(call not in pending and call not in finished, "duplicate hook begin")
                pending[call] = row
            else:
                require(call in pending and row["stage"] in ("end", "cancelled"), "unmatched hook completion")
                require(row["event"] == pending[call]["event"] and row["session"] == pending[call]["session"],
                        "hook completion changed identity")
                if row["stage"] == "cancelled":
                    require(row["event"] == "wait" and self.shutdown.exists(), "unexpected hook cancellation")
                del pending[call]
                finished.add(call)
        if final:
            require(not pending, "hook invocations did not record completion or shutdown cancellation")

    def event_rows(self):
        if not self.events.exists():
            return []
        # A final partial append is not complete evidence yet.
        require(self.events.stat().st_size <= 8 * 1024 * 1024, "observer log exceeded 8 MiB")
        lines = self.events.read_bytes().split(b"\n")[:-1]
        return [json.loads(line) for line in lines]

    def until(self, label, predicate):
        deadline = time.monotonic() + self.args.timeout
        while time.monotonic() < deadline:
            require(self.server is None or self.server.poll() is None, "OpenCode server exited")
            if predicate():
                return
            time.sleep(0.25)
        raise CheckFailed(f"timed out waiting for {label}")

    def turn(self, phase, prompt):
        require(self.confined(), "fixture tool confinement is not ready")
        self.run([self.args.opencode, "run", "--server", self.url, "--session", self.sessions[phase],
                  "--model", self.args.model, "--format", "json", prompt], self.args.timeout)

    def send(self, phase, sender):
        return self.mdkg(sender, "msg", "send", "--to", "probe-" + phase,
                         "--kind", "note", "--text", self.sentinel)["message"]["seq"]

    def ack(self, phase):
        inbox = self.mdkg("probe-" + phase, "msg", "inbox")
        ids = [m["messageId"] for m in inbox["messages"]]
        if ids:
            self.mdkg("probe-" + phase, "msg", "ack", *ids)

    def prepare(self):
        preflight_external_config(self.root.parent)
        config = Path(self.env["XDG_CONFIG_HOME"]) / "opencode" / "opencode.json"
        config.parent.mkdir()
        config.write_text(json.dumps(fixture_config()))
        safety = Path(__file__).with_name("check-opencode-safety.mjs")
        self.report["artifacts"] = {
            "mdkg_sha256": file_sha256(self.args.mdkg),
            "generator_sha256": file_sha256(self.args.generator),
            "script_sha256": file_sha256(__file__),
            "safety_plugin_sha256": file_sha256(safety),
        }
        version = self.run([self.args.opencode, "--version"]).strip()
        require(qualified_version(version), f"requires a qualified OpenCode ({', '.join(QUALIFIED)}); found {version}")
        self.report["opencode"] = version
        self.report["mdkg_version"] = self.run([self.args.mdkg, "--version"]).strip()
        cfg = self.root / "observer.json"
        cfg.write_text(json.dumps(self.cfg))
        wrapper = self.root / "mdkg-observer"
        wrapper.write_text("#!/bin/sh\nexec " + " ".join(shlex.quote(str(p)) for p in
                           (sys.executable, "-I", Path(__file__).resolve(), "--hook-driver", cfg, self.events)) + ' "$@"\n')
        wrapper.chmod(0o700)
        plugin = self.run([self.args.generator, wrapper])
        require('id: "mdkg"' in plugin and 'async setup(ctx:' in plugin, "unexpected canonical plugin output")
        self.report["plugin_sha256"] = hashlib.sha256(plugin.encode()).hexdigest()
        self.run(["git", "init", "-q", "--template=", "-b", "main"])
        self.mdkg("probe-setup", "init", "--graph-only", "--work", "shared")
        canonical = self.repo / ".mdkg/hooks/opencode/mdkg/server.ts"
        canonical.parent.mkdir(parents=True)
        canonical.write_text(plugin)
        self.mdkg("probe-setup", "install", "--enable", "hooks:opencode", "--binary", str(wrapper))
        require((self.repo / ".opencode/plugins/mdkg/server.ts").read_text() == plugin, "plugin projection differs")
        safety_plugin = self.repo / ".opencode/plugins/mdkg-check-safety/server.ts"
        safety_plugin.parent.mkdir()
        safety_plugin.write_text(safety.read_text())
        (self.repo / "guard.txt").write_text("ORIGINAL\n")
        self.run(["git", "add", "--all"])
        self.run(["git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false",
                  "commit", "-q", "-m", "isolated hook fixture"])
        self.start_server()

    def start_server(self):
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.url = f"http://127.0.0.1:{port}"
        preflight_external_config(self.root.parent)
        with defer_interrupts():
            self.server = subprocess.Popen([self.args.opencode, "serve", "--hostname", "127.0.0.1", "--port", str(port)],
                                           cwd=self.repo, env=self.env, stdin=subprocess.DEVNULL,
                                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        def ready():
            try:
                return isinstance(self.api("server.info"), dict)
            except (CheckFailed, json.JSONDecodeError):
                return False
        self.until("server readiness", ready)
        self.until("fixture tool confinement", self.fixture_plugins_ready)
        self.report["checks"]["fixture_confinement"] = "loaded before sessions"

    def fixture_plugins_ready(self):
        # server.info is global: it does not build the lazy Location services
        # or load project plugins. Materialize the fixture explicitly before
        # waiting for the safety plugin's receipt. Activation is asynchronous;
        # an empty or partial inventory stays pending. LocationQuery uses
        # OpenAPI deepObject encoding, hence the bracketed query parameter.
        plugins = self.api("plugin.list", **{"location[directory]": str(self.repo)})
        require(isinstance(plugins, dict) and isinstance(plugins.get("location"), dict)
                and plugins["location"].get("directory") == str(self.repo)
                and isinstance(plugins.get("data"), list), "unexpected fixture plugin inventory")
        require(all(isinstance(p, dict) and p.get("state") == {"status": "active"}
                    for p in plugins["data"]), "fixture plugin failed to activate")
        complete = True
        for plugin_id in ("mdkg", "mdkg-check-safety"):
            matches = [p for p in plugins["data"] if p.get("id") == plugin_id]
            require(len(matches) <= 1, f"duplicate fixture plugin: {plugin_id}")
            complete = complete and len(matches) == 1
        return complete and self.confined()

    def confined(self):
        if not self.safety_ready.exists():
            return False
        return json.loads(self.safety_ready.read_text()) == {"token": self.safety_token, "tools": ["write"]}

    def context_check(self):
        sender = "context-" + secrets.token_hex(6)
        self.send("context", sender)
        self.turn("context", "Without using any tools, reply CONTEXT_SEEN followed by the sender in the mdkg mail context you received. Do not read mail or files.")
        self.until("mail context in the assistant reply", lambda: assistant_said(self.messages("context"), sender))
        messages = self.messages("context")
        require(not tool_parts(messages), "context check used tools; reply is not proof of injected context")
        require(any(sender in r.get("out", {}).get("context", "") for r in self.observed("context", "start")), "no real start hook supplied the header")
        self.ack("context")

    def edit_check(self):
        self.turn("edit", "Use the write tool exactly once to replace guard.txt with CHANGED. If denied, stop and report the denial. Do not claim a lease, retry, or use shell/execute/patch or any other fallback.")
        self.until("edit deny", lambda: any("claim <path>" in r.get("out", {}).get("deny", "") for r in self.observed("edit", "edit")))
        edits = self.observed("edit", "edit")
        require(len(edits) == 1 and edits[0].get("guard_target") is True,
                "edit check did not make exactly one write to guard.txt")
        parts = tool_parts(self.messages("edit"))
        require(len(parts) == 1 and parts[0].get("name") == "write"
                and parts[0].get("state", {}).get("status") == "error"
                and "claim <path>" in json.dumps(parts[0].get("state", {}).get("error")),
                "session must contain exactly one rejected write and no fallback tools")
        require((self.repo / "guard.txt").read_text() == "ORIGINAL\n", "model used a fallback write; edit guard is not isolation")

    def reminder_check(self, phase):
        marker = "STOP_REMINDER_SEEN" if phase == "stop" else "IDLE_WAKE_SEEN"
        self.turn(phase, f"Reply READY once, without tools. If a later message starts with You have mdkg mail, reply {marker} without tools. This is a hook test: do not read or ack mail.")
        if phase == "wake":
            self.until("idle waiter", lambda: bool(self.observed(phase, "wait", "begin")))
            sender = "wake-" + secrets.token_hex(6)
            seq = self.send(phase, sender)
            field, event = "wake", "wait"
        else:
            injected = self.observed(phase, "stop_mail")
            require(len(injected) == 1, "stop mail was not inserted exactly once after start")
            seq = injected[0]["seq"]
            field, event = "block", "stop"
        self.until(marker, lambda: answered_notice(self.messages(phase), seq, marker))
        self.until("second stop", lambda: len(self.observed(phase, "stop")) >= 2)
        # More than one notice/queued user message is a loop, even if the
        # first reply was correct. Give another idle cycle time to expose it.
        time.sleep(2)
        messages = self.messages(phase)
        notices = [r for r in self.observed(phase) if f"#{seq} " in r.get("out", {}).get(field, "")]
        require(len(notices) == 1 and notices[0]["event"] == event, f"{phase} did not deliver exactly one {field}")
        queued = queued_notices(messages, seq)
        require(len(queued) == 1, f"{phase} queued a repeated or missing reminder")
        require(not tool_parts(messages), f"{phase} model used tools instead of the bounded reply")
        self.ack(phase)

    def execute(self):
        try:
            self.report["phase"] = "setup"
            self.prepare()
            for phase, check in (("context", self.context_check), ("edit", self.edit_check),
                                 ("stop", lambda: self.reminder_check("stop")),
                                 ("wake", lambda: self.reminder_check("wake"))):
                print(f"checking {phase}", file=sys.stderr, flush=True)
                self.report["phase"] = phase
                check()
                self.observed(phase)  # A successful model reply must not hide hook write failures.
                self.report["checks"][phase] = "passed"
        finally:
            try:
                if self.server is not None:
                    try:
                        self.shutdown.touch()
                    finally:
                        stop_process(self.server, wait_for_group=True)
            finally:
                # Include evidence on a failed phase too, after producers stop.
                rows = self.event_rows()
                self.report["hook_events"] = rows[-200:]
                self.verify_events(rows, final=True)
        self.report["phase"] = "complete"


def permission_check(root, args, report, password):
    repo = root / "project"
    repo.mkdir()
    env = isolated_env(root, password)

    def run(argv, timeout=20):
        preflight_external_config(root.parent)
        return command(argv, repo, env, timeout)

    cli = qualified_version(run([args.opencode, "--version"]))
    require(cli, f"requires a qualified OpenCode ({', '.join(QUALIFIED)})")
    report["opencode"] = cli
    report["artifacts"] = {"mdkg_sha256": file_sha256(args.mdkg),
                           "worker_config_sha256": file_sha256(args.worker_config),
                           "script_sha256": file_sha256(__file__)}
    inline = run([args.worker_config, args.mdkg, repo])
    require(command_json(inline, "worker config").get("permission"), "the exporter printed no permissions")
    env["OPENCODE_CONFIG_CONTENT"] = inline
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    url = f"http://127.0.0.1:{port}"

    def api(operation, **params):
        argv = [args.opencode, "api", "--server", url, operation]
        for key, value in params.items():
            argv += ["--param", f"{key}={value}"]
        return command_json(run(argv), operation)

    preflight_external_config(root.parent)
    server = None
    try:
        with defer_interrupts():
            server = subprocess.Popen([args.opencode, "serve", "--hostname", "127.0.0.1", "--port", str(port)],
                                      cwd=repo, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL, start_new_session=True)
        deadline = time.monotonic() + args.timeout
        while True:
            try:
                info = api("server.info")
                break
            except (CheckFailed, json.JSONDecodeError):
                require(time.monotonic() < deadline, "server readiness timed out")
                time.sleep(0.5)
        require(info.get("version") == cli, "the server is another release than the CLI")
        where = {"location[directory]": str(repo)}
        # model.list awaits config activation; agent.list alone can answer
        # with defaults first (as mdkg's preflight does).
        require(isinstance(api("model.list", **where).get("data"), list), "activation failed")
        builds = [a for a in api("agent.list", **where).get("data", []) if a.get("id") == "build"]
        require(len(builds) == 1 and isinstance(builds[0].get("permissions"), list),
                "no single resolved build agent")
        rules = builds[0]["permissions"]
        deny, other = permission_probes(args.mdkg)
        effects = {c: last_effect(rules, "shell", c) for c in deny + other}
        report["checks"]["permissions"] = effects
        missed = [c for c in deny if effects[c] != "deny"]
        require(not missed, f"the inline config's refusals did not apply: {missed}")
        blocked = [c for c in other if effects[c] == "deny"]
        require(not blocked, f"ordinary commands were denied: {blocked}")
    finally:
        if server is not None:
            stop_process(server, wait_for_group=True)


class OfflineTests(unittest.TestCase):
    def test_interrupt_during_server_spawn_registers_it_before_cleanup(self):
        spawn = subprocess.Popen
        children = []

        def interrupted(_signum, _frame):
            raise KeyboardInterrupt

        def start(*_args, **_kwargs):
            child = spawn([sys.executable, "-I", "-c", "import time; time.sleep(30)"],
                          stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL, start_new_session=True)
            children.append(child)
            # Faithfully inject the vulnerable window: the OS has spawned
            # the server but Popen has not returned it to its caller yet.
            os.kill(os.getpid(), signal.SIGTERM)
            return child

        old_handler = signal.signal(signal.SIGTERM, interrupted)
        try:
            for mode in ("permissions", "hooks"):
                with tempfile.TemporaryDirectory() as tmp:
                    args = argparse.Namespace(mdkg=Path("mdkg"), opencode=Path("opencode"),
                                              worker_config=Path("worker-config"), timeout=10)
                    with patch.object(socket, "socket"), patch.object(subprocess, "Popen", side_effect=start), \
                         patch(__name__ + ".preflight_external_config"), \
                         patch(__name__ + ".file_sha256", return_value="fixture"), \
                         patch(__name__ + ".command", side_effect=["2.2.5", '{"permission":{"bash":"deny"}}']):
                        with self.assertRaises(KeyboardInterrupt):
                            if mode == "permissions":
                                permission_check(Path(tmp), args, {"checks": {}}, "test")
                            else:
                                check = LiveCheck(Path(tmp), args, {"checks": {}}, "test")
                                with patch.object(check, "prepare", side_effect=check.start_server):
                                    check.execute()
                    self.assertIsNotNone(children[-1].poll(), f"{mode} server survived startup interruption")
                    self.assertIs(signal.getsignal(signal.SIGTERM), interrupted)
        finally:
            signal.signal(signal.SIGTERM, old_handler)
            for child in children:
                stop_process(child, grace=.1)

    def test_only_the_qualified_releases_pass(self):
        for text in ("opencode v2.1.0", "2.1.0", "opencode v2.2.5\n", "2.2.5"):
            self.assertIn(qualified_version(text), QUALIFIED, text)
        for text in ("opencode v2.2.0", "opencode v2.2.6", "opencode v2.2.5-beta.1",
                     "opencode v12.2.5", "v2.2.5", "", "not-a-version"):
            self.assertIsNone(qualified_version(text), text)

    def test_the_last_matching_rule_decides_a_shell_command(self):
        rules = [{"action": "*", "resource": "*", "effect": "allow"},
                 {"action": "shell", "resource": "opencode auth", "effect": "deny"},
                 {"action": "shell", "resource": "opencode auth *", "effect": "deny"},
                 {"action": "shell", "resource": "*/opencode auth *", "effect": "deny"},
                 {"action": "edit", "resource": "*", "effect": "ask"}]
        self.assertEqual(last_effect(rules, "shell", "opencode auth export"), "deny")
        self.assertEqual(last_effect(rules, "shell", "opencode auth"), "deny")
        self.assertEqual(last_effect(rules, "shell", "/usr/bin/opencode auth list"), "deny")
        self.assertEqual(last_effect(rules, "shell", "opencode authors"), "allow")
        self.assertEqual(last_effect(rules, "shell", "git status"), "allow")
        self.assertEqual(last_effect([], "shell", "git status"), "ask")
        self.assertTrue(wildcard("a?c", "abc") and not wildcard("a.c", "abc"))

    def test_probes_cover_every_refusal_and_leave_ordinary_commands(self):
        deny, other = permission_probes(Path("/opt/bin/mdkg-next"))
        self.assertIn("/opt/bin/mdkg-next auth key anthropic", deny)
        self.assertIn("mdkg-next agents use-key lane", deny)
        self.assertTrue(set(deny).isdisjoint(other))


    def test_environment_does_not_inherit_user_config_or_credentials(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {
            "OPENCODE_CONFIG": "/external", "OPENCODE_CONFIG_DIR": "/external",
            "OPENCODE_CONFIG_CONTENT": "external", "OPENCODE_CLI_CONFIG": "/external",
            "TMPDIR": "/external", "TMP": "/external", "TEMP": "/external",
            "OPENAI_API_KEY": "fake-test-value"}):
            env = isolated_env(Path(tmp), "throwaway-test")
            self.assertEqual(env["OPENCODE_PASSWORD"], "throwaway-test")
            self.assertNotIn("OPENAI_API_KEY", env)
            self.assertNotIn("MDKG_TRUSTED_COMMON_DIR", env)
            self.assertNotIn("OPENCODE_CONFIG", env)
            self.assertEqual({k for k in env if k.startswith("OPENCODE_")}, {"OPENCODE_PASSWORD"})
            self.assertEqual(set(env) & {"HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"}, {"HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"})
            for key, value in env.items():
                if key == "HOME" or key.startswith("XDG_") or key in ("TMPDIR", "TMP", "TEMP"):
                    self.assertTrue(Path(value).is_relative_to(tmp))
                    self.assertTrue(Path(value).is_dir())

    def test_discovery_checks_system_managed_and_every_ancestor(self):
        with patch.object(sys, "platform", "linux"):
            self.assertEqual(system_config_paths(), [Path("/etc/opencode")])
        with patch.object(sys, "platform", "darwin"), patch.object(pwd, "getpwuid") as user:
            user.return_value.pw_name = "os-account"
            paths = system_config_paths()
            self.assertIn(Path("/Library/Application Support/opencode"), paths)
            self.assertIn(Path("/Library/Managed Preferences/ai.opencode.managed.plist"), paths)
            self.assertIn(Path("/Library/Managed Preferences/os-account/ai.opencode.managed.plist"), paths)
        with patch.object(sys, "platform", "darwin"), patch.object(pwd, "getpwuid", side_effect=KeyError):
            with self.assertRaises(CheckFailed):
                system_config_paths()
        with patch(__name__ + ".system_config_paths", return_value=[Path("/system/opencode")]):
            paths = external_config_paths(Path("/outer/project/temp"))
        self.assertIn(Path("/system/opencode"), paths)
        for ancestor in ("/", "/outer", "/outer/project", "/outer/project/temp"):
            for name in ("opencode.json", "opencode.jsonc", ".opencode", ".claude", ".agents", "AGENTS.md"):
                self.assertIn(Path(ancestor) / name, paths)

    def test_external_configuration_refuses_files_empty_directories_and_links(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "config"
            reject_present_config([source])
            source.write_text("{}")
            with self.assertRaises(CheckFailed):
                reject_present_config([source])
            source.unlink()
            source.mkdir()
            with self.assertRaises(CheckFailed):
                reject_present_config([source])
            source.rmdir()
            source.symlink_to(root / "absent")
            with self.assertRaises(CheckFailed):
                reject_present_config([source])
            with patch.object(Path, "lstat", side_effect=PermissionError):
                with self.assertRaisesRegex(CheckFailed, "cannot inspect"):
                    reject_present_config([source])

    def test_permissions_and_policies_deny_all_but_the_guarded_edit(self):
        config = fixture_config()
        def decision(rules, action, resource):
            return next((r["effect"] for r in reversed(rules)
                         if r["action"] in ("*", action) and r["resource"] in ("*", resource)), None)
        for action, resource in [("read", "/external/secret"), ("shell", "cat /external/secret"),
                                 ("external_directory", "/external"), ("execute", "*"),
                                 ("edit", "../guard.txt"), ("subagent", "general")]:
            self.assertEqual(decision(config["permissions"], action, resource), "deny")
            self.assertEqual(decision(config["policies"], "tool.use", action + ":" + resource), "deny")
        self.assertEqual(decision(config["permissions"], "edit", "guard.txt"), "allow")
        self.assertEqual(decision(config["policies"], "tool.use", "edit:guard.txt"), "allow")
        self.assertFalse(config["formatter"])

    def test_no_session_starts_without_exact_tool_confinement_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            args = argparse.Namespace(mdkg=Path("mdkg"), opencode=Path("opencode"), model="opencode/test-free", timeout=10)
            check = LiveCheck(Path(tmp), args, {}, "test")
            with patch.object(check, "run") as run:
                for receipt in (None, {"token": "wrong", "tools": ["write"]},
                                {"token": check.safety_token, "tools": ["write", "shell"]}):
                    if receipt is not None:
                        check.safety_ready.write_text(json.dumps(receipt))
                    with self.assertRaises(CheckFailed):
                        check.turn("edit", "fixture")
                run.assert_not_called()
                check.safety_ready.write_text(json.dumps({"token": check.safety_token, "tools": ["write"]}))
                check.turn("edit", "fixture")
                self.assertNotIn("--auto", run.call_args.args[0])

    def test_server_bootstraps_fixture_plugins_before_starting_a_session(self):
        with tempfile.TemporaryDirectory() as tmp:
            args = argparse.Namespace(mdkg=Path("mdkg"), opencode=Path("opencode"), model="opencode/test-free", timeout=10)
            check = LiveCheck(Path(tmp), args, {"checks": {}}, "test")
            order = []
            def api(operation, **params):
                order.append(operation)
                if operation == "server.info":
                    self.assertFalse(check.safety_ready.exists())
                    return {}
                self.assertEqual(operation, "plugin.list")
                self.assertEqual(params, {"location[directory]": str(check.repo)})
                if order.count("plugin.list") == 1:
                    return {"location": {"directory": str(check.repo)}, "data": []}
                check.safety_ready.write_text(json.dumps({"token": check.safety_token, "tools": ["write"]}))
                return {"location": {"directory": str(check.repo)}, "data": [
                    {"id": name, "state": {"status": "active"}} for name in ("mdkg", "mdkg-check-safety")]}
            def until(label, predicate):
                for _ in range(2):
                    if predicate():
                        break
                else:
                    self.fail(label)
                order.append(label)
            with patch.object(socket, "socket") as sock, patch.object(subprocess, "Popen") as spawn, \
                 patch(__name__ + ".preflight_external_config"), \
                 patch.object(check, "api", side_effect=api), patch.object(check, "until", side_effect=until), \
                 patch.object(check, "run", side_effect=lambda *args: order.append("session")):
                sock.return_value.__enter__.return_value.getsockname.return_value = ("127.0.0.1", 1234)
                check.start_server()
                check.turn("edit", "fixture")
                spawn.assert_called_once()
                self.assertEqual(order, ["server.info", "server readiness", "plugin.list", "plugin.list",
                                         "fixture tool confinement", "session"])

    def test_inactive_duplicate_or_wrong_location_plugins_are_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            args = argparse.Namespace(mdkg=Path("mdkg"), opencode=Path("opencode"), model="opencode/test-free", timeout=10)
            check = LiveCheck(Path(tmp), args, {"checks": {}}, "test")
            active = [{"id": name, "state": {"status": "active"}} for name in ("mdkg", "mdkg-check-safety")]
            bad = [
                {"location": {"directory": "/elsewhere"}, "data": active},
                {"location": {"directory": str(check.repo)}, "data": active + active[:1]},
                {"location": {"directory": str(check.repo)}, "data": [active[0],
                    {"id": "mdkg-check-safety", "state": {"status": "failed", "error": "test"}}]},
                {"location": None, "data": active},
            ]
            for inventory in bad:
                with patch.object(check, "api", return_value=inventory):
                    with self.assertRaises(CheckFailed):
                        check.fixture_plugins_ready()
            for partial in ([], active[:1], active):
                with patch.object(check, "api", return_value={"location": {"directory": str(check.repo)}, "data": partial}):
                    self.assertFalse(check.fixture_plugins_ready(), "admitted missing plugins or missing receipt")

    def test_pagination_preserves_order_and_drops_order_after_cursor(self):
        calls = []
        def fetch(params):
            calls.append(params)
            n = len(calls)
            return {"data": [{"type": "user", "id": str(n)}], "cursor": {"next": "next" if n == 1 else None}}
        self.assertEqual([m["id"] for m in message_pages(fetch)], ["1", "2"])
        self.assertEqual(calls[1], {"limit": "100", "cursor": "next"})
        with self.assertRaises(CheckFailed):
            message_pages(lambda _: {"data": [], "cursor": {"next": "repeated"}})
        with self.assertRaises(CheckFailed):
            message_pages(lambda _: [{"type": "assistant"}])

    def test_only_finished_assistant_text_proves_a_reply(self):
        fake = [{"type": "user", "text": "MATCH"}, {"type": "assistant", "finish": "stop", "content": [{"type": "reasoning", "text": "MATCH"}]}]
        self.assertFalse(assistant_said(fake, "MATCH"))
        fake.append({"type": "assistant", "finish": "stop", "content": [{"type": "text", "text": "MATCH"}], "native": {"private": "not evidence"}})
        self.assertTrue(assistant_said(fake, "MATCH"))
        self.assertNotIn("native", evidence(fake)[-1])

    def test_reminder_reply_must_follow_the_queued_notice(self):
        reply = {"type": "assistant", "finish": "stop", "content": [{"type": "text", "text": "MARKER"}]}
        messages = [reply, {"type": "user", "text": "You have mdkg mail: #6 note from a"}]
        self.assertFalse(answered_notice(messages, 6, "MARKER"))
        messages.append(reply)
        self.assertTrue(answered_notice(messages, 6, "MARKER"))
        self.assertFalse(answered_notice(messages, 7, "MARKER"))

    def test_redaction_and_partial_observer_lines(self):
        self.assertEqual(redact({"a": ["secret-value"]}, "secret"), {"a": ["[redacted]-value"]})
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "events"
            append_event(path, {"event": "start"})
            with path.open("ab") as file:
                file.write(b'{"unfinished":')
            self.assertEqual([json.loads(line) for line in path.read_bytes().split(b"\n")[:-1]], [{"event": "start"}])

    def test_large_worker_config_is_read_to_eof_without_a_stdout_pipe(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = {"permission": {"bash": {f"echo fixture-{n}": "allow" for n in range(12000)}},
                      "tail": "CONFIG_END"}
            payload = json.dumps(config)
            self.assertGreater(len(payload.encode()), 256 * 1024)
            path = root / "worker.json"
            path.write_text(payload)
            # Model a CLI whose exit cuts buffered pipe output at 256KiB.
            program = ("import os,stat,sys; from pathlib import Path; "
                       "data=Path(sys.argv[1]).read_text(); "
                       "sys.stdout.write(data[:262144] if stat.S_ISFIFO(os.fstat(1).st_mode) else data)")
            raw = command([sys.executable, "-I", "-c", program, path], root, isolated_env(root, "test"))
            self.assertEqual(command_json(raw, "worker config"), config)
            self.assertEqual(command_json(raw, "agent.list")["tail"], "CONFIG_END")
            with self.assertRaisesRegex(CheckFailed, r"agent.list.*262144 bytes"):
                command_json(payload[:262144], "agent.list")

    def test_json_diagnostic_omits_body_characters_with_the_fallback_scanner(self):
        decoder = json.JSONDecoder()
        decoder.parse_string = json.decoder.py_scanstring
        decoder.scan_once = json.scanner.py_make_scanner(decoder)
        raw = '"\\☃"'
        with self.assertRaises(json.JSONDecodeError) as original:
            decoder.decode(raw)
        self.assertIn("☃", original.exception.msg)
        with patch.object(json, "loads", side_effect=decoder.decode):
            with self.assertRaises(CheckFailed) as caught:
                command_json(raw, "agent.list")
        self.assertEqual(str(caught.exception),
                         f"agent.list returned invalid JSON ({len(raw.encode('utf-8'))} bytes; "
                         f"character {original.exception.pos})")
        self.assertNotIn("☃", str(caught.exception))

    def test_command_timeout_reaps_its_process(self):
        with tempfile.TemporaryDirectory() as tmp:
            pidfile = Path(tmp) / "pid"
            with self.assertRaises(CheckFailed):
                command([sys.executable, "-I", "-c",
                         "import os,sys,time; from pathlib import Path; "
                         "Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(30)", str(pidfile)],
                        tmp, isolated_env(Path(tmp), "test"), timeout=0.5)
            with self.assertRaises(ProcessLookupError):
                os.kill(int(pidfile.read_text()), 0)

    def test_shutdown_marker_failure_still_terminates_the_server(self):
        with tempfile.TemporaryDirectory() as tmp:
            check = object.__new__(LiveCheck)
            check.report = {}
            check.events, check.shutdown = Path(tmp) / "events", Path(tmp) / "shutdown"
            check.sentinel = "BODY_SENTINEL"
            check.server = subprocess.Popen([sys.executable, "-I", "-c", "import time; time.sleep(30)"],
                                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                            stderr=subprocess.DEVNULL, start_new_session=True)
            try:
                with patch.object(check, "prepare", side_effect=CheckFailed("setup failed")), \
                     patch.object(Path, "touch", side_effect=OSError(errno.ENOSPC, "disk full")):
                    with self.assertRaises(OSError) as caught:
                        check.execute()
                self.assertEqual(caught.exception.errno, errno.ENOSPC)
                self.assertIsNotNone(check.server.poll(), "server survived failed marker write")
            finally:
                stop_process(check.server, grace=.1)

    @unittest.skipUnless(hasattr(os, "fork"), "requires POSIX fork")
    def test_cleanup_stops_a_child_after_its_group_leader_exits(self):
        with tempfile.TemporaryDirectory() as tmp:
            heartbeat = Path(tmp) / "heartbeat"
            code = ("import os,signal,sys,time\nfrom pathlib import Path\n"
                    "if os.fork(): os._exit(0)\n"
                    "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                    "while True:\n Path(sys.argv[1]).write_text(str(time.monotonic_ns()))\n time.sleep(.02)\n")
            process = subprocess.Popen([sys.executable, "-I", "-c", code, str(heartbeat)],
                                       stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                       stderr=subprocess.DEVNULL, start_new_session=True)
            try:
                process.wait(timeout=2)
                deadline = time.monotonic() + 2
                while not heartbeat.exists() and time.monotonic() < deadline:
                    time.sleep(.02)
                self.assertTrue(heartbeat.exists())
                stop_process(process, grace=.1)
                time.sleep(.1)
                before = heartbeat.read_text()
                time.sleep(.1)
                self.assertEqual(heartbeat.read_text(), before)
            finally:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=2)

    def test_observer_forwards_real_output_and_inserts_stop_mail_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake = root / "fake-mdkg"
            cfg = {"mdkg": str(fake), "repo": str(root), "events": str(root / "events"),
                   "sessions": {"ses_stop": "stop"}, "timeout": 1,
                   "stop_once": str(root / "once"), "stop_sender": "sender", "sentinel": "BODY_SENTINEL",
                   "shutdown": str(root / "shutdown")}
            # The fake observes the exact raw input and order; it cannot
            # invoke Git, OpenCode, a coordinator or a model.
            fake.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + " -I "
                            + shlex.quote(str(root / "fake.py")) + ' "$@"\n')
            fake.chmod(0o700)
            (root / "fake.py").write_text(
                "import json,sys\nfrom pathlib import Path\n"
                "args=sys.argv[1:]\n"
                "if 'hook' in args:\n"
                " raw=sys.stdin.read()\n"
                " with Path('calls').open('a') as f: f.write(json.dumps({'args':args,'raw':raw})+'\\n')\n"
                " print(json.dumps({'context':'KNOWN_HEADER'}))\n"
                "else:\n"
                " with Path('calls').open('a') as f: f.write(json.dumps({'args':args})+'\\n')\n"
                " print(json.dumps({'ok':True,'data':{'message':{'seq':6}}}))\n")
            config = root / "cfg"
            config.write_text(json.dumps(cfg))
            argv = [sys.executable, "-I", str(Path(__file__).resolve()), "--hook-driver", str(config),
                    cfg["events"], "hook", "start", "--harness", "opencode"]
            raw = '{"sessionID":"ses_stop", "cwd":"fixture"}'
            for _ in range(2):
                out = subprocess.run(argv, input=raw, text=True, capture_output=True,
                                     env=isolated_env(root / "home-env", "test"), timeout=5)
                self.assertEqual(out.returncode, 0, out.stderr)
                self.assertEqual(json.loads(out.stdout), {"context": "KNOWN_HEADER"})
                self.assertEqual(out.stderr, "")
            calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
            self.assertEqual(len(calls), 3)
            self.assertEqual(calls[0]["raw"], raw)
            self.assertIn("probe-stop", calls[0]["args"])
            self.assertIn("msg", calls[1]["args"])
            self.assertEqual(calls[2]["raw"], raw)
            rows = [json.loads(line) for line in (root / "events").read_text().splitlines()]
            self.assertEqual([r["event"] for r in rows], ["start", "stop_mail", "start", "start", "start"])
            self.assertNotIn("BODY_SENTINEL", json.dumps(rows))
            for bad in ('[]', '{"sessionID":"unknown"}', '{"sessionID":{}}'):
                out = subprocess.run(argv, input=bad, text=True, capture_output=True, timeout=5)
                self.assertNotEqual(out.returncode, 0)
            self.assertEqual(len((root / "calls").read_text().splitlines()), 3)

    def observer_fixture(self, root, body, event="start", phase="wake"):
        fake = root / "fake-mdkg"
        fake.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + " -I "
                        + shlex.quote(str(root / "fake.py")) + ' "$@"\n')
        fake.chmod(0o700)
        (root / "fake.py").write_text(body)
        cfg = {"mdkg": str(fake), "repo": str(root), "events": str(root / "events"),
               "sessions": {"s": phase}, "timeout": 30, "shutdown": str(root / "shutdown"),
               "stop_once": str(root / "once"), "stop_sender": "sender", "sentinel": "BODY_SENTINEL"}
        config = root / "cfg"
        config.write_text(json.dumps(cfg))
        argv = [sys.executable, "-I", str(Path(__file__).resolve()), "--hook-driver", str(config),
                cfg["events"], "hook", event, "--harness", "opencode"]
        check = object.__new__(LiveCheck)
        check.events, check.shutdown, check.sentinel = Path(cfg["events"]), Path(cfg["shutdown"]), cfg["sentinel"]
        return cfg, argv, check

    def wait_for_file(self, path):
        deadline = time.monotonic() + 2
        while not path.exists() and time.monotonic() < deadline:
            time.sleep(.02)
        self.assertTrue(path.exists(), str(path))

    def test_observer_errors_are_terminal_failures(self):
        for body in ["print('[]')", "print('not JSON')", "raise RuntimeError('failed command')"]:
            with tempfile.TemporaryDirectory() as tmp:
                _, argv, check = self.observer_fixture(Path(tmp), body)
                out = subprocess.run(argv, input='{"sessionID":"s"}', text=True,
                                     capture_output=True, timeout=5)
                self.assertEqual(out.returncode, 1)
                rows = check.event_rows()
                self.assertEqual([r["stage"] for r in rows], ["begin", "end"])
                self.assertEqual(rows[-1]["code"], 1)
                self.assertEqual(rows[0]["call"], rows[1]["call"])
                with self.assertRaises(CheckFailed):
                    check.verify_events(rows, final=True)

    def test_shutdown_interrupts_stop_mail_without_detaching_its_child(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            body = ("import os,sys,time\nfrom pathlib import Path\n"
                    "if 'hook' in sys.argv:\n print('{}')\n"
                    "else:\n"
                    " Path('mail-pid').write_text(str(os.getpid()))\n"
                    " while True:\n  Path('heartbeat').write_text(str(time.monotonic_ns()))\n  time.sleep(.02)\n")
            _, argv, check = self.observer_fixture(root, body, phase="stop")
            process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                       stderr=subprocess.DEVNULL, start_new_session=True)
            try:
                process.stdin.write(b'{"sessionID":"s"}')
                process.stdin.close()
                self.wait_for_file(root / "heartbeat")
                check.shutdown.touch()
                stop_process(process, grace=.2, wait_for_group=True)
                before = (root / "heartbeat").read_text()
                time.sleep(.1)
                self.assertEqual((root / "heartbeat").read_text(), before)
                with self.assertRaises(CheckFailed):
                    check.verify_events(check.event_rows(), final=True)
            finally:
                stop_process(process, grace=.1)
                # Also clean up if this regression finds a detached child.
                if (root / "mail-pid").exists():
                    try:
                        os.kill(int((root / "mail-pid").read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_only_recorded_waiter_shutdown_is_a_valid_cancellation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, argv, check = self.observer_fixture(root,
                "import time\nfrom pathlib import Path\nPath('ready').touch()\ntime.sleep(30)\n", event="wait")
            process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                       stderr=subprocess.DEVNULL, start_new_session=True)
            try:
                process.stdin.write(b'{"sessionID":"s"}')
                process.stdin.close()
                self.wait_for_file(root / "ready")
                with self.assertRaises(CheckFailed):
                    check.verify_events(check.event_rows(), final=True)
                check.shutdown.touch()
                stop_process(process, grace=.2, wait_for_group=True)
                self.assertEqual(check.event_rows()[-1]["stage"], "cancelled")
                check.verify_events(check.event_rows(), final=True)
            finally:
                stop_process(process, grace=.1)

    def test_hook_errors_and_body_leaks_cannot_qualify_a_check(self):
        check = object.__new__(LiveCheck)
        check.sentinel = "PRIVATE_BODY"
        for row in [
            {"event": "start", "stage": "end", "code": 1},
            {"event": "stop", "stage": "end", "stderr": "recording failed"},
            {"event": "start", "stage": "end", "out": {"context": "PRIVATE_BODY"}},
        ]:
            with self.assertRaises(CheckFailed):
                check.verify_events([row])

    def test_guard_evidence_identifies_the_attempted_tool_and_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            for path in ["guard.txt", str(Path(tmp) / "guard.txt")]:
                self.assertTrue(guard_target({"tool": "write", "args": {"path": path}}, tmp))
            for value in [
                {"tool": "read", "args": {"path": "guard.txt"}},
                {"tool": "write", "args": {"path": "elsewhere"}},
                {"tool": "write", "args": {"path": "../guard.txt"}},
                {"tool": "write", "args": {"path": None}},
            ]:
                self.assertFalse(guard_target(value, tmp))

    def test_live_rejects_paid_or_missing_models_before_starting_any_program(self):
        for model in [None, "openai/gpt-6", "opencode/paid", "opencode/free;anything"]:
            argv = [sys.executable, "-I", str(Path(__file__).resolve()), "--live",
                    "--mdkg", "/nonexistent-mdkg", "--opencode", "/nonexistent-opencode"]
            if model is not None:
                argv += ["--model", model]
            out = subprocess.run(argv, text=True, capture_output=True, timeout=5)
            self.assertEqual(out.returncode, 1)
            receipt = json.loads(out.stdout)
            self.assertFalse(receipt["ok"])
            self.assertIn("explicit opencode/*-free", receipt["error"])

    def test_inherited_temp_directory_under_project_refuses_before_binary_lookup(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            nested = root / "project" / "tmp"
            nested.mkdir(parents=True)
            (root / "project" / ".opencode").mkdir()
            out = subprocess.run([sys.executable, "-I", str(Path(__file__).resolve()), "--live",
                                  "--model", "opencode/test-free", "--mdkg", "/nonexistent-mdkg",
                                  "--opencode", "/nonexistent-opencode"],
                                 env={**os.environ, "TMPDIR": str(nested)}, text=True, capture_output=True, timeout=5)
            self.assertEqual(out.returncode, 1)
            receipt = json.loads(out.stdout)
            self.assertFalse(receipt["ok"])
            self.assertIn("external configuration source exists", receipt["error"])
            self.assertEqual(list(nested.iterdir()), [])


def binary(value):
    found = shutil.which(str(value))
    require(found is not None, f"binary missing: {value}; build the candidate and plugin exporter first")
    return Path(found).resolve()


def main():
    if sys.argv[1:2] == ["--hook-driver"]:
        try:
            return hook_driver(sys.argv[2], sys.argv[3], sys.argv[4:])
        except Exception as exc:
            print(f"hook observer failed: {exc}", file=sys.stderr)
            return 1
    repo = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--live", action="store_true")
    mode.add_argument("--live-permissions", action="store_true")
    parser.add_argument("--mdkg", default=repo / "target/debug/mdkg", type=Path)
    parser.add_argument("--generator", default=repo / "target/debug/examples/opencode-hook-plugin", type=Path)
    parser.add_argument("--worker-config", default=repo / "target/debug/examples/opencode-worker-config", type=Path)
    parser.add_argument("--opencode", default="opencode")
    parser.add_argument("--model", help="Explicit Zen free model, opencode/<name>-free")
    parser.add_argument("--timeout", default=120, type=int, help="Per-phase limit in seconds (10..600)")
    args = parser.parse_args()
    if args.self_test:
        return 0 if unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(OfflineTests)).wasSuccessful() else 1
    password = secrets.token_urlsafe(32)
    report = {"ok": False, "model": args.model, "checks": {}, "sessions": {}}
    try:
        require(os.name == "posix", "this process-group cleanup check requires a POSIX host")
        require(10 <= args.timeout <= 600, "timeout must be between 10 and 600 seconds")
        def interrupted(_signum, _frame):
            raise KeyboardInterrupt
        # Before either mode starts a server: SIGTERM unwinds its cleanup.
        signal.signal(signal.SIGTERM, interrupted)
        if args.live_permissions:
            temp_base = Path(tempfile.gettempdir()).resolve(strict=True)
            preflight_external_config(temp_base)
            args.mdkg, args.worker_config, args.opencode = map(binary, (args.mdkg, args.worker_config, args.opencode))
            with tempfile.TemporaryDirectory(prefix="mdkg-opencode-permissions-", dir=temp_base) as tmp:
                permission_check(Path(tmp).resolve(strict=True), args, report, password)
            report["ok"] = True
            print(json.dumps(redact(report, password), indent=2))
            return 0
        require(args.model and re.fullmatch(r"opencode/[a-z0-9][a-z0-9.-]*-free", args.model), "name an explicit opencode/*-free model; no paid/provider fallback")
        temp_base = Path(tempfile.gettempdir()).resolve(strict=True)
        preflight_external_config(temp_base)
        args.mdkg, args.generator, args.opencode = map(binary, (args.mdkg, args.generator, args.opencode))
        with tempfile.TemporaryDirectory(prefix="mdkg-opencode-hooks-", dir=temp_base) as tmp:
            LiveCheck(Path(tmp).resolve(strict=True), args, report, password).execute()
        report["ok"] = True
    except (CheckFailed, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as exc:
        report["error"] = str(exc) or "interrupted"
    print(json.dumps(redact(report, password), indent=2))
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
