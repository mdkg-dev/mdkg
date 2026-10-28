#!/usr/bin/env python3
"""Claude Code 2.1.309 hooks/worker acceptance; Python 3.9+, macOS/Linux.

Offline (no Git, Claude, mdkg, network or model calls):
  python3 -I scripts/check-claude-hooks.py --self-test
Live, under the person's normal Claude login:
  python3 -I scripts/check-claude-hooks.py --live --mdkg /absolute/candidate/mdkg \
    --claude /absolute/path/to/claude --model claude-haiku-5-5 --person-reviewed

The required flag permits preparation for the person's pre-launch /hooks
review. Alternatively, --review-mode inventory --inventory-sha256 <report hash>
records a person-approved inventory instead. Both modes wait for a private,
one-use confirmation of that exact fixture before starting acceptance sessions.
Receipts retain scope "with unverified effects: managed/server policy";
--allow-builtin-plugins adds "and Claude built-in plugins" and records the
names reported by each acceptance session alongside the Claude binary hash.
A pass is observed behavior, never fixture-only execution or sandbox qualification.
No script reads credentials/auth files or deletes user Claude data. Git writes
and coordinator operations stay in a fresh temporary repository/worktrees.
The receipt lists session IDs and hook-reported transcript paths for manual
cleanup. Normal Claude startup can write project entries and other home state.

The candidate's install command supplies canonical hooks. This finite check
omits the async idle-wake entry (not tested), preserving synchronous start,
edit and stop commands. The observer delegates those to the actual candidate.
Stop mail and an unclaimed file change are inserted after UserPromptSubmit's
start read and before its output returns. A fixture guard restricts tools to
the exact test file's Read/Write or the worker inbox. The worker runs through real mdkg,
with its candidate MCP server behind a tools/list and tools/call filter.

CLI --help and installed settings schema verified for 2.1.309 (2026-10-27).
--settings syncClaudeAiPlugins/Skills:false disables sync for this invocation
without moving user files. Known external plugin/managed sources still refuse.
https://code.claude.com/docs/en/cli-reference
https://code.claude.com/docs/en/hooks
https://code.claude.com/docs/en/settings-reference
https://code.claude.com/docs/en/permissions
https://code.claude.com/docs/en/plugins/loading
https://code.claude.com/docs/en/plugins-reference
https://code.claude.com/docs/en/skills
https://code.claude.com/docs/en/claude-directory

Stdout: one JSON receipt. Stderr: progress; no raw authentication/provider
output. Exit 0 pass with unverified effects (or offline tests pass), 1 check
failure, 2 usage, 3 unverified/refused, 130 interrupted. Owned process groups
and fixture files are cleaned up on completion/error/interrupt; external
managed/server effects are outside that cleanup claim.
"""
import argparse
from contextlib import ExitStack
import hashlib
import io
import json
import os
from pathlib import Path
import pwd
import re
import runpy
import secrets
import select
import shlex
import signal
import stat
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from unittest.mock import patch
import uuid

_POPEN = subprocess.Popen  # Only the local Python process-cleanup regression uses this directly.

# Reuse the already-reviewed process-group cleanup and atomic event append.
# Loading this sibling module does not run its CLI or self-tests.
_SHARED = runpy.run_path(str(Path(__file__).with_name('check-opencode-hooks.py')))
stop_process = _SHARED['stop_process']
append_event = _SHARED['append_event']
SCHEMA = 'mdkg.claude-hooks-check.v2'
VERSION, MODEL = '2.1.309', 'claude-haiku-5-5'
SCOPE = 'with unverified effects: managed/server policy'
POLICY = {'syncClaudeAiPlugins': False, 'syncClaudeAiSkills': False, 'disableClaudeAiConnectors': True}
PHASES = ('start', 'edit-deny', 'edit-allow', 'stop')
LIMIT = 16 * 1024 * 1024
CHECKLIST = '''Pre-launch /hooks review under the normal Claude login:
- Use project/local setting sources and strict fixture MCP.
- Review mdkg start/edit/stop commands and the fixture guard; reject unexpected hooks/plugins.
- Accept unverified managed/server-policy effects for this check only.
- Plan to clean up this fixture's Claude transcripts/project entries yourself.
--person-reviewed permits fixture preparation. After inspecting the printed
fixture command with /hooks and exiting it, write the printed one-use marker.
The script records that attestation; it cannot verify what a person inspected.
'''
INVENTORY_CHECKLIST = '''Pre-launch inventory review under the normal Claude login:
- Have the person approve the inventory report identified by --inventory-sha256.
- Accept unverified managed/server-policy effects for this check only.
- Plan to clean up this fixture's Claude transcripts/project entries yourself.
--person-reviewed permits fixture preparation. After approval, write the printed
one-use marker. The script records the attestation, not independent proof of approval.
'''

class CheckFailed(Exception): pass
class Unverified(CheckFailed): pass
class InvalidInvocation(Unverified): pass
class Interrupted(BaseException): pass

def require(ok, message):
    if not ok: raise CheckFailed(message)

def read_json(path):
    raw = Path(path).read_bytes()
    require(len(raw) <= LIMIT, 'fixture data too large')
    return json.loads(raw)

def write_json(path, value):
    Path(path).write_text(json.dumps(value, sort_keys=True) + '\n')

def digest(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''): h.update(block)
    return h.hexdigest()

def rows(path):
    if not Path(path).exists(): return []
    raw = Path(path).read_bytes()
    require(len(raw) <= LIMIT and (not raw or raw.endswith(b'\n')), 'incomplete fixture events')
    return [json.loads(line) for line in raw.splitlines()]

def command(argv, cwd, env, timeout=30, data=None, own_group=True):
    p = subprocess.Popen([str(a) for a in argv], cwd=cwd, env=env, text=True,
                         stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=own_group)
    try:
        out, err = p.communicate(data, timeout=timeout)
        require(len(out) + len(err) <= LIMIT, 'command output too large')
        return p.returncode, out, err
    except subprocess.TimeoutExpired as exc:
        raise CheckFailed('fixture command timed out') from exc
    finally:
        try:
            if own_group: stop_process(p)
            elif p.poll() is None:
                p.terminate()
                try: p.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    p.kill(); p.wait(timeout=3)
        finally:
            for stream in (p.stdin, p.stdout, p.stderr):
                if stream: stream.close()

def environment(root, home, create=True):
    # No inherited provider keys, Git config, proxies, harness flags or mdkg pins.
    account_name = pwd.getpwuid(os.getuid()).pw_name
    env = {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': str(home), 'LANG': 'en_US.UTF-8',
           'USER': account_name, 'LOGNAME': account_name,
           'TERM': 'dumb', 'NO_COLOR': '1', 'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_NOSYSTEM': '1',
           'GIT_TERMINAL_PROMPT': '0', 'GIT_AUTHOR_NAME': 'mdkg hook check',
           'GIT_AUTHOR_EMAIL': 'test@example.invalid', 'GIT_COMMITTER_NAME': 'mdkg hook check',
           'GIT_COMMITTER_EMAIL': 'test@example.invalid', 'DISABLE_AUTOUPDATER': '1',
           'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1'}
    for name in ('CONFIG', 'CACHE', 'DATA', 'STATE', 'RUNTIME'):
        path = root / name.lower()
        if create: path.mkdir(mode=0o700)
        else: require(path.is_dir() and not path.is_symlink(), 'fixture environment directory changed')
        env['XDG_RUNTIME_DIR' if name == 'RUNTIME' else 'XDG_' + name + '_HOME'] = str(path)
    if create: (root / 'tmp').mkdir(mode=0o700)
    else: require((root / 'tmp').is_dir() and not (root / 'tmp').is_symlink(), 'fixture temporary directory changed')
    env.update({k: str(root / 'tmp') for k in ('TMPDIR', 'TMP', 'TEMP')})
    return env

def reject_present(paths):
    for path in paths:
        try: Path(path).lstat()
        except FileNotFoundError: continue
        except OSError as exc: raise Unverified('cannot inspect external configuration') from exc
        raise Unverified('detectable external configuration/plugin source exists')

def external_paths(parent, home, platform=sys.platform, review_mode='interactive'):
    if platform == 'darwin':
        base = Path('/Library/Managed Preferences')
        user = pwd.getpwuid(os.getuid()).pw_name
        system = Path('/Library/Application Support/ClaudeCode')
        paths = [system / name for name in ('managed-settings.json', 'managed-settings.d', 'managed-mcp.json')]
        paths += [base / 'com.anthropic.claudecode.plist',
                 base / user / 'com.anthropic.claudecode.plist']
    elif platform == 'linux':
        paths = [Path('/etc/claude-code') / name for name in ('managed-settings.json', 'managed-settings.d', 'managed-mcp.json')]
    else: raise Unverified('preflight supports macOS and Linux only')
    # Inventory mode inspects plugins/skills separately; every other refusal
    # stays in force. User settings are excluded by setting-sources.
    roots = ('hooks', 'mods', 'commands', 'agents')
    if review_mode != 'inventory': roots += ('plugins', 'skills')
    paths += [home / '.claude' / p for p in roots]
    for ancestor in (parent, *parent.parents):
        paths += [ancestor / p for p in ('.claude', '.mcp.json', '.claude-plugin', '.agents', '.git', '.mdkg', 'CLAUDE.md')]
    return paths

def inventory_sources(home):
    """Refuse plugin payloads; read only the registry and skill/command headers.

    Never follow links or open other files (in particular auth/config files).
    Unknown registry formats and ambiguous YAML fail closed. This is a local
    recheck, not verification of the operator-supplied report hash or approval.
    """
    result = {'plugins_directory': False, 'skills_directory': False, 'cache_directory': False,
              'synced_directory': False, 'installed_registry': False, 'installed_plugin_records': 0,
              'plugin_manifests': 0, 'skill_headers': 0, 'command_headers': 0, 'other_files_unread': 0}
    def refuse(message='cannot safely inspect plugin/skill inventory'):
        raise Unverified(message)
    def info(parent, name):
        value = os.stat(name, dir_fd=parent, follow_symlinks=False)
        if not (stat.S_ISDIR(value.st_mode) or stat.S_ISREG(value.st_mode)): refuse()
        if stat.S_ISREG(value.st_mode) and value.st_nlink != 1: refuse()
        return value
    def stamp(value):
        return (value.st_dev, value.st_ino, value.st_mode, value.st_nlink,
                value.st_size, value.st_mtime_ns, value.st_ctime_ns)
    def contents(parent, name, before):
        if not stat.S_ISREG(before.st_mode) or before.st_size > 1024 * 1024: refuse()
        fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
        with os.fdopen(fd, 'rb') as source:
            if stamp(os.fstat(source.fileno())) != stamp(before): refuse()
            raw = source.read(1024 * 1024 + 1)
            if stamp(os.fstat(source.fileno())) != stamp(before): refuse()
        if len(raw) > 1024 * 1024 or stamp(info(parent, name)) != stamp(before): refuse()
        return raw.decode('utf-8-sig')
    def json_source(parent, name, state):
        def pairs(items):
            obj = {}
            for key, value in items:
                if key in obj: refuse()
                obj[key] = value
            return obj
        value = json.loads(contents(parent, name, state), object_pairs_hook=pairs)
        if not isinstance(value, dict): refuse()
        return value
    def skill_header(raw):
        # Accept ordinary block-mapping headers only. Complex keys and merges
        # need a full YAML parser; refuse them rather than missing a hook key.
        lines = raw.splitlines()
        while lines and not lines[0].strip(): lines.pop(0)
        if not lines or lines[0].strip() != '---': return
        # Indented delimiters can be block-scalar content, not document ends.
        end = next((i for i in range(1, len(lines)) if re.fullmatch(r'(?:---|\.\.\.)[ \t]*', lines[i])), None)
        if end is None: refuse('unverified skill frontmatter')
        # Comment indentation cannot determine the mapping's root indentation.
        header = '\n'.join(line for line in lines[1:end] if not line.lstrip().startswith('#'))
        for line in textwrap.dedent(header).splitlines():
            if not line.strip() or line.startswith(' '): continue
            key = re.match(r'''^([A-Za-z_][\w-]*|'[^'\\]*'|"[^"\\]*")\s*:''', line)
            if not key: refuse('unverified skill frontmatter')
            if key[1].strip('\'"') == 'hooks': refuse('skill frontmatter declares hooks')
    base = home / '.claude'
    plugins = base / 'plugins'
    seen = 0
    dir_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    canonical = {name.casefold(): name for name in
                 ('.claude-plugin', 'plugin.json', 'installed_plugins.json', '.mcp.json', 'hooks.json', 'SKILL.md', 'cache', 'commands')}
    def walk(parent, name, folder, depth):
        nonlocal seen
        if depth > 20: refuse()
        before = info(parent, name)
        fd = os.open(name, dir_flags, dir_fd=parent)
        try:
            if stamp(os.fstat(fd)) != stamp(before): refuse()
            with os.scandir(fd) as children:
                for entry in children:
                    # This is Claude's payload cache, not catalog metadata.
                    # Require it empty, even for unregistered/manifestless or
                    # apparently harmless payloads: marketplace entries can
                    # supply executable hooks absent from plugin.json.
                    if folder == plugins / 'cache': refuse('plugin payload cache is not empty')
                    seen += 1
                    if seen > 20000: refuse()
                    name = entry.name; path = folder / name
                    state = info(fd, name)
                    # macOS may resolve these through canonical case aliases.
                    expected = canonical.get(name.casefold())
                    if expected and name != expected: refuse('noncanonical inventory control-file casing')
                    if stat.S_ISDIR(state.st_mode):
                        if path == plugins / 'cache': result['cache_directory'] = True
                        if path == plugins / 'synced': result['synced_directory'] = True
                        walk(fd, name, path, depth + 1)
                    elif name in ('.mcp.json', 'hooks.json'):
                        refuse('plugin/skill payload contains hooks or MCP definitions')
                    elif path == plugins / 'installed_plugins.json':
                        registry = json_source(fd, name, state)
                        if registry.get('version') not in (1, 2) or not isinstance(registry.get('plugins'), dict): refuse()
                        if registry['plugins']: refuse('installed plugin registry is not empty')
                        result['installed_registry'] = True
                    elif name == 'plugin.json' and folder.name == '.claude-plugin':
                        refuse('plugin payload manifest exists')
                    elif name == 'SKILL.md':
                        skill_header(contents(fd, name, state)); result['skill_headers'] += 1
                    elif path.suffix.casefold() == '.md' and 'commands' in folder.relative_to(base).parts:
                        skill_header(contents(fd, name, state)); result['command_headers'] += 1
                    else: result['other_files_unread'] += 1
            if stamp(info(parent, folder.name)) != stamp(before): refuse('inventory directory changed during inspection')
        finally: os.close(fd)
    try:
        # Hold descriptors from the home root down. Neither a queued directory
        # replacement nor a final-component link can redirect an inventory read.
        with ExitStack() as opened:
            home_fd = os.open(home, dir_flags); opened.callback(os.close, home_fd)
            try: base_state = info(home_fd, '.claude')
            except FileNotFoundError: return result
            base_fd = os.open('.claude', dir_flags, dir_fd=home_fd); opened.callback(os.close, base_fd)
            if stamp(os.fstat(base_fd)) != stamp(base_state): refuse()
            for kind in ('plugins', 'skills'):
                try: info(base_fd, kind)
                except FileNotFoundError: continue
                result[kind + '_directory'] = True
                walk(base_fd, kind, base / kind, 0)
            if stamp(info(home_fd, '.claude')) != stamp(base_state): refuse('inventory directory changed during inspection')
    except (OSError, ValueError, UnicodeError, RecursionError): refuse()
    return result

def preflight(parent, home, inherited, review_mode='interactive'):
    redirects = ('CLAUDE_CONFIG_DIR', 'CLAUDE_CODE_PLUGIN_CACHE_DIR', 'CLAUDE_CODE_PLUGIN_DIRS',
                 'CLAUDE_CODE_PLUGIN_SEED_DIR', 'CLAUDE_CODE_SETTINGS_PATH')
    if any(inherited.get(k) for k in redirects): raise Unverified('Claude configuration redirect is set')
    reject_present(external_paths(parent, home, review_mode=review_mode))
    return inventory_sources(home) if review_mode == 'inventory' else None

def executable(value, label):
    if not value or not Path(value).is_absolute(): raise Unverified(label + ' needs an absolute executable path')
    path = Path(value).resolve(strict=True)
    if not path.is_file() or not os.access(path, os.X_OK): raise Unverified(label + ' is not executable')
    return path

def mdkg(cfg, actor, args, root=None, env=None, own_group=False):
    cwd = root or cfg['repo']
    code, out, _ = command([cfg['mdkg'], '--root', cwd, '--as', actor, *args, '--json'], cwd,
                           os.environ.copy() if env is None else env, cfg.get('timeout', 120) + 45, own_group=own_group)
    require(code == 0, 'candidate mdkg command failed')
    value = json.loads(out)
    require(value.get('ok') is True, 'candidate mdkg returned an unsuccessful envelope')
    data = value['data']
    if args[:2] == ['agents', 'up']:
        require(data.get('event') == 'turn' and isinstance(data.get('turn'), dict), 'manual agent pass did not return one turn')
        return data['turn']
    return data

def common_claude(cfg, review=False):
    return [cfg['claude'], '--setting-sources', 'project,local', '--strict-mcp-config',
            '--settings', json.dumps(POLICY), '--no-chrome'] + ([] if review else ['--disable-slash-commands'])

def subscription_login(cfg, cwd, env, own_group=True):
    # Ask Claude itself; never open its authentication files or forward its
    # account identifiers. Reject saved Console/API auth as well as env keys.
    code, out, _ = command(common_claude(cfg) + ['--mcp-config={"mcpServers":{}}', 'auth', 'status', '--json'],
                           cwd, env, own_group=own_group)
    try: value = json.loads(out)
    except ValueError: value = {}
    if not (code == 0 and value.get('loggedIn') is True and value.get('authMethod') == 'claude.ai'
            and value.get('apiProvider') == 'firstParty'):
        raise Unverified('normal Claude subscription login required; no model session started')
    return {'loggedIn': True, 'authMethod': 'claude.ai', 'apiProvider': 'firstParty'}

def turn_args(cfg, sid, phase, mcp):
    # Attach variadic values so later positionals cannot become config/tool
    # values. Claude accepts a comma-separated tool list in one argument.
    args = common_claude(cfg) + ['-p', '--session-id', sid, '--model', MODEL,
        '--output-format', 'stream-json', '--verbose', '--include-hook-events', '--permission-mode', 'dontAsk',
        '--permission-prompts', 'none', '--tools=' + ('Read,Write' if phase.startswith('edit') else ''),
        '--mcp-config=' + json.dumps(mcp)]
    if phase.startswith('edit'): args += ['--allowedTools=Read(/' + cfg['guard'] + '),Edit(/' + cfg['guard'] + ')']
    elif phase == 'worker': args += ['--allowedTools=mcp__mdkg__inbox']
    if phase != 'worker': args += ['--disallowedTools=mcp__*']
    return args

def default_inbox_arguments(args):
    return isinstance(args, dict) and (not args or (set(args) == {'all'} and args['all'] is False))

def safe_tool(value, cfg, phase):
    name, args = value.get('tool_name'), value.get('tool_input')
    if phase == 'worker': return name == 'mcp__mdkg__inbox' and default_inbox_arguments(args)
    if not phase.startswith('edit') or name not in ('Read', 'Write') or not isinstance(args, dict): return False
    expected_keys = {'file_path'} if name == 'Read' else {'file_path', 'content'}
    if set(args) != expected_keys or not isinstance(args['file_path'], str): return False
    target, given = Path(cfg['guard']), Path(args['file_path'])
    if not given.is_absolute() or given != target or target.is_symlink(): return False
    try: info = target.lstat()
    except OSError: return False
    text = 'DENIED_WRITE\n' if phase == 'edit-deny' else 'CLAIMED_WRITE\n'
    return (stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and target.resolve() == target
            and (name == 'Read' or args['content'] == text))

def phase_of(cfg, value):
    sid = value.get('session_id'); phase = cfg['sessions'].get(sid)
    if phase is None and Path(cfg['worker_session']).exists():
        if sid == read_json(cfg['worker_session'])['session']: phase = 'worker'
    require(phase is not None, 'unknown fixture session')
    require(value.get('cwd') == (cfg['worker'] if phase == 'worker' else cfg['repo']), 'unexpected hook worktree')
    return sid, phase

def hook_driver(config, mode, argv):
    cfg = None
    try:
        cfg = read_json(config)
        raw = sys.stdin.read(1024 * 1024 + 1)
        require(len(raw) <= 1024 * 1024, 'oversized hook input')
        value = json.loads(raw); sid, phase = phase_of(cfg, value)
        if mode == 'guard':
            allowed = safe_tool(value, cfg, phase)
            append_event(cfg['events'], {'kind': 'guard', 'session': sid, 'allowed': allowed,
                                        'tool': value.get('tool_name'), 'tool_use_id': value.get('tool_use_id')})
            if not allowed: print(json.dumps(denial('Outside this check\'s tool scope.')))
            return 0
        require(len(argv) == 4 and argv[0] == 'hook' and argv[1] in ('start', 'stop', 'edit') and
                argv[2:] == ['--harness', 'claude'], 'unexpected hook command')
        event, call = argv[1], secrets.token_hex(12)
        actor = 'probe-edit' if phase.startswith('edit') else 'probe-' + phase
        append_event(cfg['events'], {'kind': 'begin', 'call': call, 'session': sid, 'event': event,
                                    'harness_event': value.get('hook_event_name')})
        cwd = cfg['worker'] if phase == 'worker' else cfg['repo']
        code, out, err = command([cfg['mdkg'], '--root', cwd, '--as', actor, *argv], cwd,
                                  os.environ.copy(), data=raw, own_group=False)
        require(code == 0 and not err.strip(), 'candidate hook failed')
        require(cfg['sentinel'] not in out, 'mail body reached hook output')
        if phase == 'stop' and event == 'start' and value.get('hook_event_name') == 'UserPromptSubmit':
            fd = os.open(cfg['stop_once'], os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600); os.close(fd)
            append_event(cfg['events'], {'kind': 'start_read_complete', 'session': sid, 'call': call})
            Path(cfg['audit']).write_text('UNCLAIMED_AFTER_START\n')
            msg = mdkg(cfg, 'stop-sender', ['msg', 'send', '--to', 'probe-stop', '--kind', 'note',
                                          '--text', cfg['sentinel']])['message']
            append_event(cfg['events'], {'kind': 'stop_mail', 'session': sid, 'call': call, 'seq': msg['seq']})
        append_event(cfg['events'], {'kind': 'end', 'call': call, 'session': sid, 'event': event,
                     'harness_event': value.get('hook_event_name'), 'out': out,
                     'transcript': value.get('transcript_path'), 'stop_active': value.get('stop_hook_active')})
        sys.stdout.write(out); sys.stdout.flush()
        return 0
    except (Exception, Interrupted):
        if cfg is not None:
            try: append_event(cfg['events'], {'kind': 'error', 'mode': mode})
            except Exception: pass
        if mode == 'guard': print(json.dumps(denial('Fixture guard unavailable.'))); return 0
        print('Fixture hook observer failed.', file=sys.stderr)
        return 2

def denial(text):
    return {'hookSpecificOutput': {'hookEventName': 'PreToolUse', 'permissionDecision': 'deny', 'permissionDecisionReason': text}}

def parse_stream(raw, sid, allow_builtin_plugins=False):
    require(len(raw) <= LIMIT, 'Claude stream too large')
    values = [json.loads(line) for line in raw.splitlines() if line.strip()]
    require(values and all(isinstance(v, dict) for v in values), 'invalid Claude stream')
    final = [v for v in values if v.get('type') == 'result']
    require(len(final) == 1 and final[0].get('session_id') == sid, 'missing/mismatched Claude result')
    require(final[0].get('subtype') == 'success' and final[0].get('is_error') is False, 'Claude did not complete successfully')
    init = [v for v in values if v.get('type') == 'system' and v.get('subtype') == 'init']
    require(len(init) == 1 and init[0].get('session_id') == sid, 'missing Claude init evidence')
    plugins = init[0].get('plugins', [])
    require(isinstance(plugins, list), 'invalid Claude plugin evidence')
    for plugin in plugins:
        require(allow_builtin_plugins and isinstance(plugin, dict)
                and plugin.get('path') == 'builtin'
                and isinstance(plugin.get('source'), str) and plugin['source'].endswith('@builtin')
                and isinstance(plugin.get('name'), str) and bool(plugin['name']),
                'Claude reported an unexpected plugin')
    return values

def blocks(stream, typ):
    return [b for v in stream if v.get('type') in ('assistant', 'user') and not v.get('parent_tool_use_id')
            for b in v.get('message', {}).get('content', []) if isinstance(b, dict) and b.get('type') == typ]

def assistant_text(stream):
    return '\n'.join(b.get('text', '') for v in stream if v.get('type') == 'assistant' and not v.get('parent_tool_use_id')
                     for b in v.get('message', {}).get('content', []) if isinstance(b, dict) and b.get('type') == 'text')

def assistant_has_mail_header(stream, seq, kind, sender):
    # Model wording can vary; require each whole token in its final message.
    # Earlier turns and nested tool/subagent messages cannot supply evidence.
    messages = [v for v in stream if v.get('type') == 'assistant' and not v.get('parent_tool_use_id')]
    text = assistant_text(messages[-1:])
    return all(re.search(r'(?<![\w#-])' + re.escape(token) + r'(?![\w-])', text) is not None
               for token in ('#' + str(seq), kind, sender))

def result_for(stream, tool):
    matches = [b for b in blocks(stream, 'tool_result') if b.get('tool_use_id') == tool.get('id')]
    require(len(matches) == 1, 'tool attempt has no unique result')
    return matches[0]

def worker_inbox_calls(stream, events, sid):
    attempts = blocks(stream, 'tool_use')
    require(attempts and all(t.get('name') == 'mcp__mdkg__inbox' for t in attempts), 'worker used tools outside its inbox')
    calls, seen = [], set()
    for tool in attempts:
        ident = tool.get('id')
        require(isinstance(ident, str) and ident and ident not in seen, 'missing/duplicate worker tool identity')
        seen.add(ident)
        guards = [r for r in events if r.get('kind') == 'guard' and r.get('session') == sid and r.get('tool_use_id') == ident]
        require(len(guards) == 1 and guards[0].get('tool') == tool['name']
                and type(guards[0].get('allowed')) is bool, 'missing/ambiguous worker guard evidence')
        result = result_for(stream, tool)
        if not guards[0]['allowed']:
            require(result.get('is_error') is True, 'guard-denied worker call reported success')
            continue
        require(default_inbox_arguments(tool.get('input')), 'worker used tools outside its inbox')
        calls.append(tool)
    require(calls, 'worker made no inbox call past the guard')
    return calls

def read_then_write(stream, guard):
    calls = blocks(stream, 'tool_use')
    require(len(calls) == 2 and [t['name'] for t in calls] == ['Read', 'Write'], 'expected one Read followed by one Write')
    require(all(t.get('input', {}).get('file_path') == guard for t in calls), 'edit used another path')
    read_result = result_for(stream, calls[0])
    require(not read_result.get('is_error', False), 'fixture read failed')
    ordered = [b for v in stream for b in v.get('message', {}).get('content', []) if isinstance(b, dict)]
    require(ordered.index(read_result) < ordered.index(calls[1]), 'write preceded successful read result')
    return calls[1]

def check_sentinel(stream, events, sentinel):
    messages = [v.get('message') for v in stream if v.get('type') in ('assistant', 'user')]
    hooks = [v for v in stream if v.get('type') == 'system' and str(v.get('subtype', '')).startswith('hook_')]
    require(sentinel not in json.dumps([messages, hooks, events]), 'mail body reached model/hook evidence')

def cleanup_inventory(cfg, events):
    sessions = [{'phase': p, 'id': sid} for sid, p in cfg['sessions'].items()]
    sessions += [{'phase': 'worker', 'id': r['session']} for r in events
                 if r.get('kind') == 'worker_spawn' and isinstance(r.get('session'), str)]
    return {'fixture_paths': [cfg['repo'], cfg['worker']], 'sessions': sessions,
        'hook_reported_transcript_paths_unverified': sorted({r['transcript'] for r in events if isinstance(r.get('transcript'), str)}),
        'possible_claude_artifacts': ['~/.claude/projects/<encoded-fixture>/<session>.jsonl and session directories',
            '~/.claude/file-history/<session>/', '~/.claude/debug/', '~/.claude/tasks/', '~/.claude/session-env/',
            'matching entries in ~/.claude/history.jsonl', 'fixture entries in ~/.claude.json and its backups',
            'Claude temporary scratchpads/images'], 'automatic_user_home_cleanup': False,
        'instruction': 'Inspect and remove only this fixture\'s session/project data; preserve shared files and authentication.'}

def worker_options(argv, cfg):
    flags = {'-p', '--verbose'}
    scalar = {'--session-id', '--output-format', '--tools', '--permission-mode', '--permission-prompts', '--mcp-config', '--model'}
    lists = {'--allowedTools', '--disallowedTools'}
    got = {}; i = 0
    while i < len(argv):
        key = argv[i]; i += 1
        require(key not in got and key in flags | scalar | lists, 'unexpected worker argument')
        if key in flags: got[key] = True
        elif key in scalar:
            require(i < len(argv), 'missing worker option')
            got[key] = argv[i]; i += 1
        else:
            start = i
            while i < len(argv) and not argv[i].startswith('-'): i += 1
            require(i > start, 'empty launcher tool rules')
            got[key] = argv[start:i]
    require(set(got) == flags | scalar | lists, 'incomplete worker command')
    expected = {'--output-format': 'stream-json', '--tools': 'Read,Edit,Write,Glob,Grep,Bash',
                '--permission-mode': 'acceptEdits', '--permission-prompts': 'none', '--model': MODEL}
    require(all(got[k] == v for k, v in expected.items()), 'unexpected worker mode')
    sid = str(uuid.UUID(got['--session-id']))
    require(sid == got['--session-id'], 'invalid worker session ID')
    config = json.loads(got['--mcp-config'])
    require(set(config) == {'mcpServers'} and set(config['mcpServers']) == {'mdkg'}, 'unexpected worker MCP servers')
    server = config['mcpServers']['mdkg']
    require(set(server) == {'command', 'args', 'env'} and server['command'] == cfg['mdkg'] and
            server['args'] == ['--root', cfg['graph'], 'mcp'], 'unexpected candidate MCP command')
    env = {'MDKG_ACTOR': 'probe-worker', 'MDKG_RUN_TURN': sid, 'HOME': cfg['home'],
           'XDG_CONFIG_HOME': cfg['xdg'], 'MDKG_MAIN_REF': 'main',
           'MDKG_TRUSTED_COMMON_DIR': str(Path(cfg['repo']) / '.git'), 'MDKG_TRUSTED_WORKTREE': cfg['graph']}
    require(server['env'] == env, 'unexpected MCP environment bindings')
    return sid, server

def mcp_allowed(request):
    method = request.get('method')
    if method in ('initialize', 'notifications/initialized', 'ping', 'tools/list'): return True
    params = request.get('params')
    return (method == 'tools/call' and isinstance(params, dict)
            and set(params) <= {'name', 'arguments', '_meta'} and params.get('name') == 'inbox'
            and default_inbox_arguments(params.get('arguments')))

def mcp_filtered(method, response):
    if 'error' in response: return response
    result = response.get('result', {})
    if method == 'initialize': result['capabilities'] = {'tools': {}}
    if method == 'tools/list':
        tools = [t for t in result.get('tools', []) if t.get('name') == 'inbox']
        require(len(tools) == 1, 'candidate MCP lacks a unique inbox tool')
        result['tools'] = tools
        result.pop('nextCursor', None)
    return response

def mcp_proxy(config):
    cfg = read_json(config); spec = read_json(cfg['worker_server'])
    env = os.environ.copy(); env.update(spec['env'])
    # This supervisor owns server shutdown. Keep the server outside both the
    # worker group and the launcher's turn-mark sweep, which otherwise kill it
    # before stdin EOF can cancel/join its detached inbox children. Pins and
    # actor stay unchanged; only the filtered inbox tool is exposed.
    env.pop('MDKG_RUN_TURN', None)
    env['MDKG_MCP_CALL_TIMEOUT_MS'] = '5000'
    p = subprocess.Popen([spec['command'], *spec['args']], cwd=cfg['worker'], env=env,
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, start_new_session=True)
    try:
        append_event(cfg['events'], {'kind': 'mcp_spawn', 'pid': p.pid, 'pgid': p.pid})
        for line in sys.stdin:
            require(len(line) <= 1024 * 1024, 'oversized MCP request')
            request = json.loads(line)
            if not mcp_allowed(request):
                if 'id' in request:
                    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'],
                          'error': {'code': -32601, 'message': 'Only the fixture inbox tool is available.'}}), flush=True)
                continue
            # Progress notifications are unnecessary for the bounded inbox
            # call and would complicate request/response correlation.
            if request.get('method') == 'tools/call': request['params'].pop('_meta', None)
            p.stdin.write(json.dumps(request) + '\n'); p.stdin.flush()
            if 'id' not in request: continue
            require(select.select([p.stdout], [], [], 30)[0], 'MCP response timeout')
            response = json.loads(p.stdout.readline(1024 * 1024))
            require(response.get('id') == request['id'], 'unexpected MCP response identity')
            response = mcp_filtered(request['method'], response)
            if request['method'] == 'tools/call':
                append_event(cfg['events'], {'kind': 'mcp_inbox', 'success': not response.get('error') and
                                            not response.get('result', {}).get('isError', False)})
            print(json.dumps(response), flush=True)
        return 0
    finally:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        drain_mcp(p)

def drain_mcp(p):
    # The candidate's EOF path cancels and joins call threads (including
    # children in separate groups). Terminating the server first is unsafe.
    try:
        p.stdin.close()
        p.wait(timeout=12)
    except subprocess.TimeoutExpired as exc:
        stop_mcp_family(p.pid)
        p.wait(timeout=3)
        raise CheckFailed('candidate MCP did not confirm graceful shutdown') from exc
    finally:
        p.stdout.close()

def descendant_groups(table, parent):
    """Only numeric process metadata, never another process's args or env."""
    entries = [tuple(map(int, line.split())) for line in table.splitlines() if line.strip()]
    require(all(len(row) == 3 for row in entries), 'invalid process metadata')
    seen = {parent}
    while True:
        more = {pid for pid, ppid, _ in entries if ppid in seen}
        if more <= seen: break
        seen |= more
    return {pgid for pid, _, pgid in entries if pid in seen and pgid > 1 and pgid != os.getpgrp()}

def process_groups(parent):
    ps = '/bin/ps' if Path('/bin/ps').exists() else '/usr/bin/ps'
    code, out, _ = command([ps, '-axo', 'pid=,ppid=,pgid='], '/',
                           {'PATH': '/usr/bin:/bin', 'LANG': 'C'}, timeout=5)
    require(code == 0, 'cannot inspect owned process groups for cleanup')
    return descendant_groups(out, parent)

def group_alive(pgid):
    try: os.killpg(pgid, 0); return True
    except ProcessLookupError: return False

def stop_mcp_family(pid):
    # Snapshot detached CLI groups while their server is still alive. Graceful
    # EOF normally avoids this fallback; a stuck server still must not orphan
    # those children when it is killed. Re-check before each signal.
    groups = process_groups(pid) | {pid}
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for pgid in groups:
            if group_alive(pgid):
                try: os.killpg(pgid, sig)
                except ProcessLookupError: pass
        if sig == signal.SIGTERM: time.sleep(0.2)
    deadline = time.monotonic() + 3
    while any(group_alive(g) for g in groups) and time.monotonic() < deadline: time.sleep(0.05)
    require(not any(group_alive(g) for g in groups), 'owned MCP groups did not terminate')

def worker_driver(config, argv):
    cfg = read_json(config)
    require(str(Path.cwd().resolve()) == cfg['worker'], 'unexpected worker cwd')
    preflight(Path(cfg['root']).parent, Path(cfg['home']), {}, cfg.get('review_mode', 'interactive'))
    if argv == ['auth', 'status', '--json']:
        # Let Claude access its own login. Forward only the required status
        # fields, never account identifiers or arbitrary auth command output.
        print(json.dumps(subscription_login(cfg, cfg['worker'], os.environ.copy(), own_group=False)))
        return 0
    sid, server = worker_options(argv, cfg)
    subscription_login(cfg, cfg['worker'], os.environ.copy(), own_group=False)
    write_json(cfg['worker_session'], {'session': sid})
    write_json(cfg['worker_server'], server)
    append_event(cfg['events'], {'kind': 'worker_spawn', 'session': sid, 'pid': os.getpid(), 'pgid': os.getpgrp()})
    mcp = {'mcpServers': {'mdkg': {'command': sys.executable,
            'args': ['-I', cfg['script'], '--mcp-proxy', config]}}}
    args = turn_args(cfg, sid, 'worker', mcp)
    # Exec preserves the launcher's process group and stdin prompt/stream.
    os.execve(cfg['claude'], args, os.environ.copy())

def review_driver(config):
    cfg = read_json(config)
    require(cfg.get('review_mode', 'interactive') == 'interactive', 'inventory mode has no interactive helper')
    root, home = Path(cfg['root']), Path(cfg['home'])
    require(root.stat().st_uid == os.getuid(), 'review must run as the fixture owner')
    env = environment(root, home, create=False)
    env['TERM'] = 'xterm-256color'
    preflight(root.parent, home, {})  # Parent-terminal redirects are discarded.
    subscription_login(cfg, cfg['repo'], env)
    sessions = [sid for sid, phase in cfg['sessions'].items() if phase == 'review']
    require(len(sessions) == 1, 'missing unique review session')
    args = common_claude(cfg, review=True) + ['--session-id', sessions[0], '--model', MODEL,
            '--mcp-config={"mcpServers":{}}', '--tools=', '--disallowedTools=mcp__*',
            '--permission-mode', 'dontAsk', '--permission-prompts', 'none']
    os.chdir(cfg['repo'])
    os.execve(cfg['claude'], args, env)

class LiveCheck:
    def __init__(self, root, args, report, home):
        self.root, self.args, self.report, self.home = root, args, report, home
        self.repo = root / 'project'; self.repo.mkdir()
        self.env = environment(root, home)
        self.cfg = {k: str(v) for k, v in {
            'root': root, 'repo': self.repo, 'graph': root / 'graph', 'worker': root / 'worker',
            'mdkg': args.mdkg, 'claude': args.claude, 'home': home, 'xdg': self.env['XDG_CONFIG_HOME'],
            'script': Path(__file__).resolve(), 'guard': self.repo / 'guard.txt', 'audit': self.repo / 'audit.txt',
            'events': root / 'events.jsonl', 'stop_once': root / 'stop-once',
            'worker_session': root / 'worker-session.json', 'worker_server': root / 'worker-mcp.json'}.items()}
        self.cfg.update(timeout=args.timeout, review_mode=args.review_mode,
                        sentinel='BODY_MUST_STAY_OUT_' + secrets.token_hex(16),
                        sessions={str(uuid.uuid4()): p for p in PHASES})
        if args.review_mode == 'interactive': self.cfg['sessions'][str(uuid.uuid4())] = 'review'
        self.sids = {p: sid for sid, p in self.cfg['sessions'].items()}
        self.config = root / 'observer.json'
        self.worker_nonce = 'WORKER_READ_' + secrets.token_hex(16)
        write_json(self.config, self.cfg)
        self.receipts = {}
    def run(self, argv, cwd=None, timeout=30, data=None):
        code, out, _ = command(argv, cwd or self.repo, self.env, timeout, data)
        require(code == 0, 'fixture command returned a nonzero exit')
        return out
    def mdkg(self, actor, *args, root=None):
        return mdkg(self.cfg, actor, list(args), root=root, env=self.env, own_group=True)
    def wrapper(self, name, mode):
        path = self.root / name
        path.write_text('#!/bin/sh\nexec ' + ' '.join(shlex.quote(str(p)) for p in
                        (sys.executable, '-I', self.cfg['script'], mode, self.config)) + ' "$@"\n')
        path.chmod(0o700)
        return path
    def prepare(self):
        self.report['local_inventory'] = preflight(self.root.parent, self.home, os.environ, self.args.review_mode)
        process_groups(os.getpid())  # Refuse before Claude if cleanup metadata is unavailable.
        version = self.run([self.cfg['claude'], '--version']).strip()
        require(version == VERSION + ' (Claude Code)', 'requires reviewed Claude Code 2.1.309')
        help_text = self.run([self.cfg['claude'], '--help'])
        for flag in ('--setting-sources', '--strict-mcp-config', '--settings', '--disable-slash-commands', '--no-chrome',
                     '--session-id', '--model', '--output-format', '--verbose', '--include-hook-events',
                     '--permission-mode', '--permission-prompts', '--tools', '--mcp-config', '--allowedTools', '--disallowedTools'):
            require(flag in help_text, 'installed Claude lacks a required flag')
        self.report.update(claude=version, mdkg_version=self.run([self.cfg['mdkg'], '--version']).strip(),
            artifacts={'mdkg_sha256': digest(self.cfg['mdkg']), 'claude_sha256': digest(self.cfg['claude']),
                       'script_sha256': digest(__file__), 'shared_helpers_sha256': digest(Path(__file__).with_name('check-opencode-hooks.py'))})
        self.run(['git', 'init', '-q', '--template=', '-b', 'main'])
        self.mdkg('probe-setup', 'init', '--graph-only', '--work', 'shared')
        observer = self.wrapper('hook-observer', '--observe')
        guard = self.wrapper('fixture-guard', '--guard')
        worker = self.wrapper('claude-worker', '--worker-driver')
        self.env['MDKG_RUN_CLAUDE'] = str(worker)
        self.mdkg('probe-setup', 'install', '--enable', 'hooks:claude', '--binary', str(observer))
        canonical = read_json(self.repo / '.mdkg/hooks/claude/settings.json')
        settings = read_json(self.repo / '.claude/settings.json')
        require(settings.get('hooks') == canonical.get('hooks'), 'projection differs from candidate canonical hooks')
        require(set(settings['hooks']) == {'SessionStart', 'UserPromptSubmit', 'PreToolUse', 'Stop'}, 'unexpected canonical events')
        removed = 0
        for event, entries in settings['hooks'].items():
            for entry in entries:
                kept = []
                for hook in entry['hooks']:
                    words = shlex.split(hook['command'])
                    expected = 'start' if event in ('SessionStart', 'UserPromptSubmit') else 'edit' if event == 'PreToolUse' else 'stop'
                    if hook.get('async'):
                        require(event == 'Stop' and words[:5] == [str(observer), 'hook', 'wait', '--harness', 'claude'], 'unexpected async hook')
                        removed += 1; continue
                    require(words == [str(observer), 'hook', expected, '--harness', 'claude'] and hook.get('type') == 'command', 'unexpected canonical command')
                    kept.append(hook)
                entry['hooks'] = kept
        require(removed == 1, 'expected one omitted async idle-wake hook')
        settings['hooks']['PreToolUse'].append({'matcher': '.*', 'hooks': [{'type': 'command', 'command': shlex.quote(str(guard)),
                                                                         'onFailure': 'block'}]})
        settings['permissions'] = {'defaultMode': 'dontAsk', 'allow': [], 'deny': ['Bash', 'PowerShell', 'Agent', 'WebFetch', 'WebSearch']}
        write_json(self.repo / '.claude/settings.json', settings)
        (self.repo / 'guard.txt').write_text('ORIGINAL\n'); (self.repo / 'audit.txt').write_text('ORIGINAL\n')
        config = read_json(self.repo / '.mdkg/config.json')
        auth = {'prefer': 'subscription', 'api_key_fallback': False}
        config['agents'] = {'defaults': {'auth': auth}, 'probe-worker': {'policy': {'auth': auth},
            'instructions': 'Call mcp__mdkg__inbox with {}. Read the fixture mail in that successful tool result. '
            'Make no edits, claims, acknowledgments or outgoing tool messages. Return WORKER_REPLY followed by the '
            'WORKER_READ_ value from that mail. The launcher sends your final response by mail.'}}
        write_json(self.repo / '.mdkg/config.json', config)
        self.run(['git', 'add', '--all'])
        self.run(['git', '-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgsign=false', 'commit', '-q', '-m', 'isolated Claude hook fixture'])
        self.run(['git', 'worktree', 'add', '-q', '-b', 'mdkg-graph', self.cfg['graph'], 'main'])
        self.run(['git', 'worktree', 'add', '-q', '-b', 'agent/probe-worker', self.cfg['worker'], 'main'])
        self.report['not_tested'] = ['async idle wake', 'managed/server-policy effects', 'general filesystem isolation']
        self.report['fixture'] = {'repository': self.cfg['repo'], 'worker': self.cfg['worker'],
            'canonical_hooks_sha256': digest(self.repo / '.mdkg/hooks/claude/settings.json'),
            'effective_settings_sha256': digest(self.repo / '.claude/settings.json')}
    def review(self):
        self.report['phase'] = 'person_review'
        subscription_login(self.cfg, self.repo, self.env)
        token = secrets.token_hex(16)
        marker = self.root / 'person-reviewed'
        if self.args.review_mode == 'interactive':
            helper = self.wrapper('review-claude', '--review-driver')
            print('In another terminal, inspect this exact fixture with /hooks, then exit Claude:', file=sys.stderr)
            print(shlex.quote(str(helper)), file=sys.stderr)
            print('The interactive review session is operator-owned; exit it before confirming.', file=sys.stderr)
            print('After review and exit, confirm with:', file=sys.stderr)
        else:
            print('Inventory report SHA-256: ' + self.args.inventory_sha256, file=sys.stderr)
            print('After the person approves that report for this fixture, confirm with:', file=sys.stderr)
        print("(umask 077; printf '%s\\n' " + shlex.quote(token) + ' > ' + shlex.quote(str(marker)) + ')', file=sys.stderr)
        wait_review(marker, token, self.args.review_timeout)
        expected = self.report['fixture']['effective_settings_sha256']
        require(all(digest(Path(cwd) / '.claude/settings.json') == expected for cwd in (self.cfg['repo'], self.cfg['worker'])),
                'settings changed during review')
        self.report['local_inventory'] = preflight(self.root.parent, self.home, os.environ, self.args.review_mode)
        self.report['person_reviewed'] = True
        self.report['approval'] = 'approved by the person'
        self.report['reviewed_settings_sha256'] = expected
    def events(self, phase):
        all_rows = rows(self.cfg['events'])
        require(not any(r.get('kind') == 'error' for r in all_rows), 'fixture hook observer failed')
        sid = self.sids[phase]
        result = [r for r in all_rows if r.get('session') == sid]
        begins = {r['call'] for r in result if r['kind'] == 'begin'}
        ends = {r['call'] for r in result if r['kind'] == 'end'}
        require(begins == ends, 'incomplete synchronous hook evidence')
        return result
    def parse_session(self, raw, sid, phase):
        stream = parse_stream(raw, sid, allow_builtin_plugins=self.args.allow_builtin_plugins)
        init = next(v for v in stream if v.get('type') == 'system' and v.get('subtype') == 'init')
        self.report['sessions'][phase] = {'id': sid, 'builtin_plugins': [p['name'] for p in init.get('plugins', [])]}
        return stream
    def turn(self, phase, prompt):
        self.report['local_inventory'] = preflight(self.root.parent, self.home, os.environ, self.cfg.get('review_mode', 'interactive'))
        subscription_login(self.cfg, self.repo, self.env)
        raw = self.run(turn_args(self.cfg, self.sids[phase], phase, {'mcpServers': {}}), timeout=self.args.timeout, data=prompt)
        stream = self.parse_session(raw, self.sids[phase], phase); events = self.events(phase)
        check_sentinel(stream, events, self.cfg['sentinel'])
        self.receipts[phase] = stream
        self.report['sessions'][phase].update(completed=True,
            tool_calls=[t['name'] for t in blocks(stream, 'tool_use')],
            hook_events=[r['harness_event'] for r in events if r['kind'] == 'end'])
        return stream, events
    def scenarios(self):
        self.report['phase'] = 'start'
        msg = self.mdkg('start-sender', 'msg', 'send', '--to', 'probe-start', '--kind', 'note', '--text', self.cfg['sentinel'])['message']
        header = '#' + str(msg['seq']) + ' note from start-sender'
        stream, events = self.turn('start', 'Quote the mdkg mail header supplied by the start hook (number, kind, sender). Do not use tools or read a body. Say the mail can wait, then stop.')
        require(assistant_has_mail_header(stream, msg['seq'], 'note', 'start-sender'), 'assistant did not receive the start header')
        for event in ('SessionStart', 'UserPromptSubmit'):
            require(any(r.get('harness_event') == event and header in r.get('out', '') for r in events), 'missing start hook header evidence')
        require(not blocks(stream, 'tool_use'), 'start scenario attempted tools')
        self.report['checks']['start'] = {'header_received': True, 'body_absent': True, 'mail_seq': msg['seq']}
        self.report['phase'] = 'edit'
        prompt = 'Use Read exactly once on ' + self.cfg['guard'] + ' (file_path only). After the read succeeds, use Write exactly once on that same path with content DENIED_WRITE followed by a newline. If denied, do not retry or use another tool. Then stop.'
        stream, events = self.turn('edit-deny', prompt)
        write = read_then_write(stream, self.cfg['guard'])
        require(result_for(stream, write).get('is_error') is True, 'unclaimed write was not rejected')
        require(Path(self.cfg['guard']).read_text() == 'ORIGINAL\n', 'unclaimed write changed the file')
        require(any('"permissionDecision":"deny"' in r.get('out', '').replace(' ', '') for r in events if r.get('event') == 'edit'), 'no candidate guard denial')
        self.mdkg('probe-edit', 'claim', self.cfg['guard'])
        stream, events = self.turn('edit-allow', prompt.replace('DENIED_WRITE', 'CLAIMED_WRITE'))
        write = read_then_write(stream, self.cfg['guard'])
        require(not result_for(stream, write).get('is_error', False), 'claimed write failed')
        require(Path(self.cfg['guard']).read_text() == 'CLAIMED_WRITE\n', 'claimed write did not change the file')
        require(any(r.get('event') == 'edit' and r.get('kind') == 'end' and not r.get('out', '').strip() for r in events), 'candidate did not allow the claimed path')
        Path(self.cfg['guard']).write_text('ORIGINAL\n')
        self.report['checks']['edit'] = {'unclaimed_denied': True, 'claimed_written': True}
        self.report['phase'] = 'stop'
        stream, events = self.turn('stop', 'Say FIRST_STOP, then stop without tools. If a stop hook asks you to continue, quote its mdkg mail header and changed-file audit and say they can wait for the fixture owner. Do not use tools.')
        marker = [r for r in events if r['kind'] == 'stop_mail']; require(len(marker) == 1, 'missing definite stop mail insertion')
        marker = marker[0]; seq = marker['seq']
        ordered = [r['kind'] for r in events if r.get('call') == marker['call']]
        require(ordered == ['begin', 'start_read_complete', 'stop_mail', 'end'], 'start/mail barrier order was not proved')
        notice = '#' + str(seq) + ' note from stop-sender'
        stops = [r for r in events if r.get('event') == 'stop' and r['kind'] == 'end']
        blocked = [r for r in stops if r['out'].strip()]
        require(len(blocked) == 1 and notice in blocked[0]['out'] and '1 changed file' in blocked[0]['out'], 'stop did not block once for mail and audit')
        require(any(r.get('stop_active') is True and not r['out'].strip() for r in stops), 'stop loop guard was not exercised')
        require(assistant_has_mail_header(stream, seq, 'note', 'stop-sender') and not blocks(stream, 'tool_use'),
                'assistant did not handle the stop notice without tools')
        require(all(notice not in r.get('out', '') for r in events if r.get('event') == 'start'), 'stop mail appeared in start output')
        self.report['checks']['stop'] = {'definite_interleaving': True, 'mail_seq': seq, 'audit_block_once': True, 'body_absent': True}
        self.worker_scenario()
    def worker_scenario(self):
        self.report['phase'] = 'worker'
        root = self.cfg['graph']
        self.mdkg('probe-launcher', 'agents', 'edit', 'probe-worker', '--harness', 'claude', '--role', 'worker',
                  '--cwd', self.cfg['worker'], '--model', MODEL, root=root)
        idle = self.mdkg('probe-launcher', 'agents', 'up', 'probe-worker', '--once', '--binary', self.cfg['mdkg'], root=root)
        require(idle['outcome'] == 'idle', 'worker registration started an unexpected turn')
        msg = self.mdkg('worker-sender', 'msg', 'send', '--to', 'probe-worker', '--kind', 'note', '--text', self.worker_nonce, root=root)['message']
        data = self.mdkg('probe-launcher', 'agents', 'up', 'probe-worker', '--once', '--fresh', '--binary', self.cfg['mdkg'],
                         '--timeout', str(self.args.timeout), '--keep-logs', root=root)
        require(data.get('outcome') == 'replied' and data.get('turn') == 1 and data.get('shown') == [msg['messageId']], 'worker did not deliver one expected turn')
        sid = read_json(self.cfg['worker_session'])['session']
        log = (Path(data['logs']) / 'turn-1.stdout').resolve(strict=True)
        require(self.root in log.parents, 'worker log escaped the fixture')
        stream = self.parse_session(log.read_text(), sid, 'worker')
        events = rows(self.cfg['events'])
        calls = worker_inbox_calls(stream, events, sid)
        require(any(not result_for(stream, t).get('is_error', False) and msg['messageId'] in json.dumps(result_for(stream, t))
                    and self.worker_nonce in json.dumps(result_for(stream, t)) for t in calls), 'successful actual inbox read not proved')
        require(any(r.get('kind') == 'mcp_inbox' and r.get('success') for r in events), 'candidate MCP proxy saw no successful inbox read')
        inbox = self.mdkg('worker-sender', 'msg', 'inbox', root=root)
        replies = [m for m in inbox['messages'] if m['messageId'] == data.get('reply_id')]
        require(len(replies) == 1, 'worker reply absent from sender mailbox')
        reply = replies[0]
        require(reply['sender'] == 'probe-worker' and reply['via'] == 'probe-launcher' and reply['contextId'] == msg['messageId']
                and msg['messageId'] in reply['referenceIds'], 'worker reply provenance mismatch')
        text = '\n'.join(p.get('text', '') for p in reply['parts'])
        require('WORKER_REPLY' in text and self.worker_nonce in text, 'worker reply lacks the inbox value')
        require(not self.mdkg('probe-worker', 'msg', 'inbox', root=root)['messages'], 'worker mail was not acknowledged by the launcher')
        require(not self.run(['git', 'status', '--porcelain'], cwd=self.cfg['worker']).strip(), 'worker changed its checkout')
        self.report['sessions']['worker'].update(completed=True, tool_calls=[t['name'] for t in calls])
        self.report['checks']['worker'] = {'inbox_read': True, 'reply_delivered': True, 'reply_id': reply['messageId'], 'turn': 1}
    def cleanup(self):
        # A partial final append must not skip termination of a worker whose
        # earlier spawn record was fully persisted before exec.
        observed = []
        if Path(self.cfg['events']).exists():
            for line in Path(self.cfg['events']).read_bytes().splitlines():
                try:
                    value = json.loads(line)
                    if isinstance(value, dict): observed.append(value)
                except (ValueError, UnicodeError): pass
        # mdkg owns its worker process group; capture its wrapper PID before exec
        # so an interrupted launcher cannot leave that owned group running.
        groups = []
        for r in observed:
            if (r.get('kind') == 'worker_spawn' and type(r.get('pid')) is int and r['pid'] > 1
                    and r.get('pid') == r.get('pgid') and r['pgid'] != os.getpgrp()):
                groups.append(r['pgid'])
                try: os.killpg(r['pgid'], signal.SIGTERM)
                except ProcessLookupError: pass
        # Killing the worker closes proxy/server pipes. Detached servers must
        # finish their EOF cancellation path before the fixture is removed.
        pending = {r['pid'] for r in observed if r.get('kind') == 'mcp_spawn' and type(r.get('pid')) is int and r['pid'] > 1}
        deadline = time.monotonic() + 15
        while pending and time.monotonic() < deadline:
            for pid in list(pending):
                try: os.killpg(pid, 0)
                except ProcessLookupError: pending.remove(pid)
            if pending: time.sleep(0.1)
        for pid in pending: stop_mcp_family(pid)
        for pgid in groups:
            try: os.killpg(pgid, signal.SIGKILL)
            except ProcessLookupError: pass
        self.report['manual_cleanup'] = cleanup_inventory(self.cfg, observed)
        require(not any(group_alive(pid) for pid in pending), 'detached MCP shutdown remains unverified')

def wait_review(path, token, timeout):
    deadline = time.monotonic() + timeout
    while True:
        try: info = path.lstat()
        except FileNotFoundError: info = None
        if info is not None:
            require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_uid == os.getuid()
                    and stat.S_IMODE(info.st_mode) == 0o600, 'review marker must be a private regular file')
            require(path.read_bytes() == (token + '\n').encode(), 'review marker does not match this fixture')
            return
        if time.monotonic() >= deadline: raise Unverified('person review timed out; no acceptance session started')
        time.sleep(0.25)

class Parser(argparse.ArgumentParser):
    def error(self, message): raise InvalidInvocation('invalid arguments; use --help')

def parse_args(argv):
    p = Parser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = p.add_mutually_exclusive_group(required=True)
    mode.add_argument('--self-test', action='store_true')
    mode.add_argument('--live', action='store_true')
    p.add_argument('--mdkg'); p.add_argument('--claude'); p.add_argument('--model')
    p.add_argument('--person-reviewed', action='store_true', help='permit preparation for the chosen review and private confirmation')
    p.add_argument('--review-mode', choices=('interactive', 'inventory'), default='interactive')
    p.add_argument('--inventory-sha256', help='SHA-256 of the inventory report the person approves')
    p.add_argument('--allow-builtin-plugins', action='store_true',
                   help='accept only plugins with path builtin and source ending @builtin; expand unverified-effects scope')
    p.add_argument('--timeout', type=int, default=180, help='per-scenario timeout, 45..600 seconds')
    p.add_argument('--review-timeout', type=int, default=900, help='seconds to inspect the generated fixture, 30..1800')
    args = p.parse_args(argv)
    if not 45 <= args.timeout <= 600: raise InvalidInvocation('timeout must be 45..600 seconds')
    if not 30 <= args.review_timeout <= 1800: raise InvalidInvocation('review timeout must be 30..1800 seconds')
    if args.review_mode == 'inventory':
        if not args.inventory_sha256 or not re.fullmatch('[0-9a-fA-F]{64}', args.inventory_sha256):
            raise InvalidInvocation('inventory mode requires a SHA-256 report hash')
        args.inventory_sha256 = args.inventory_sha256.lower()
    elif args.inventory_sha256 is not None:
        raise InvalidInvocation('inventory hash requires inventory review mode')
    if args.self_test and (args.mdkg or args.claude or args.model or args.person_reviewed or args.review_mode != 'interactive'
                          or args.inventory_sha256 or args.allow_builtin_plugins):
        raise InvalidInvocation('self-test takes no candidate, model, review attestation or plugin opt-in')
    return args

def base_receipt():
    return {'schema': SCHEMA, 'ok': False, 'outcome': 'unverified', 'scope': SCOPE,
            'phase': 'preflight', 'checks': {}, 'sessions': {}, 'person_reviewed': False, 'allow_builtin_plugins': False}

def live(args, report):
    report.update(review_mode=args.review_mode, inventory_sha256=args.inventory_sha256,
                  allow_builtin_plugins=args.allow_builtin_plugins,
                  scope=SCOPE + (' and Claude built-in plugins' if args.allow_builtin_plugins else ''))
    if not args.person_reviewed: raise Unverified('pre-launch review required: pass --person-reviewed to prepare for confirmation')
    if args.model != MODEL: raise Unverified('this check requires explicit --model claude-haiku-5-5')
    report['review_requested'] = True
    home = Path.home().resolve(strict=True)
    parent = Path(tempfile.gettempdir()).resolve(strict=True)
    report['local_inventory'] = preflight(parent, home, os.environ, args.review_mode)
    args.mdkg = executable(args.mdkg, 'mdkg'); args.claude = executable(args.claude, 'claude')
    if (home / '.cargo/bin') in args.mdkg.parents: raise Unverified('use a candidate build, not ~/.cargo/bin')
    with tempfile.TemporaryDirectory(prefix='mdkg-claude-hooks-') as folder:
        root = Path(folder).resolve(strict=True)
        check = LiveCheck(root, args, report, home)
        try:
            print('Preparing disposable Claude hook fixture: ' + str(root), file=sys.stderr)
            check.prepare()
            check.review()
            print('Starting four acceptance scenarios; ' + report['scope'], file=sys.stderr)
            check.scenarios()
        finally:
            check.cleanup()
    report.update(ok=True, outcome='pass with unverified effects', phase='complete', fixture_removed=True)

def interrupted(_sig, _frame): raise Interrupted()

def main(argv=None, out=None, err=None):
    out = sys.stdout if out is None else out; err = sys.stderr if err is None else err
    report = base_receipt(); code = 3
    try:
        args = parse_args(argv)
        if args.self_test:
            # Every child-process entry point trips during the offline suite;
            # tests needing fake commands replace the narrower command helper.
            with patch('subprocess.Popen', side_effect=AssertionError('offline test attempted an unapproved process')), \
                 patch('socket.socket', side_effect=AssertionError('offline test attempted network access')):
                result = unittest.TextTestRunner(stream=err, verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(OfflineTests))
            report.update(mode='self-test', ok=result.wasSuccessful(), outcome='offline tests passed' if result.wasSuccessful() else 'offline tests failed',
                          tests_run=result.testsRun, failures=len(result.failures), errors=len(result.errors), live_qualification='unverified')
            code = 0 if result.wasSuccessful() else 1
        else:
            err.write(INVENTORY_CHECKLIST if args.review_mode == 'inventory' else CHECKLIST)
            if args.allow_builtin_plugins:
                err.write('Built-in plugin opt-in: accept unverified effects from Claude built-in plugins as well.\n')
            live(args, report); code = 0
    except InvalidInvocation as exc:
        report.update(outcome='invalid_invocation', diagnostic=str(exc)); code = 2
    except Unverified as exc:
        report['diagnostic'] = str(exc); code = 3
    except CheckFailed as exc:
        report.update(outcome='failed', diagnostic=str(exc)); code = 1
    except (KeyboardInterrupt, Interrupted):
        report.update(outcome='interrupted', diagnostic='check interrupted'); code = 130
    except Exception:
        report.update(outcome='failed', diagnostic='unexpected check error; no raw command output included'); code = 1
    out.write(json.dumps(report, sort_keys=True) + '\n')
    return code

class OfflineTests(unittest.TestCase):
    def test_manual_agent_pass_decodes_the_turn_event(self):
        cfg = {'mdkg': '/candidate/mdkg', 'repo': '/fixture', 'timeout': 60}
        report = {'outcome': 'idle'}
        envelope = {'ok': True, 'data': {'event': 'turn', 'agent': 'worker', 'turn': report}}
        with patch(__name__ + '.command', return_value=(0, json.dumps(envelope), '')) as called:
            self.assertEqual(mdkg(cfg, 'launcher', ['agents', 'up', 'worker', '--once']), report)
            self.assertIn('agents', called.call_args.args[0])
            self.assertNotIn('run', called.call_args.args[0])
        envelope['data'] = {'event': 'wake'}
        with patch(__name__ + '.command', return_value=(0, json.dumps(envelope), '')):
            with self.assertRaises(CheckFailed):
                mdkg(cfg, 'launcher', ['agents', 'up', 'worker', '--once'])

    def fixture(self, root):
        target = root / 'guard.txt'; target.write_text('ORIGINAL\n')
        return {'guard': str(target), 'repo': str(root), 'worker': str(root / 'worker'), 'graph': str(root / 'graph'),
                'home': '/normal-home', 'xdg': str(root / 'config'), 'mdkg': '/candidate/mdkg', 'claude': '/candidate/claude',
                'worker_session': str(root / 'worker-session.json'), 'events': str(root / 'events.jsonl'),
                'audit': str(root / 'audit.txt'), 'stop_once': str(root / 'stop-once'), 'sentinel': 'PRIVATE_BODY',
                'sessions': {'s': 'stop'}, 'timeout': 60}
    def worker_argv(self, cfg):
        sid = '00000000-0000-4000-8000-000000000001'
        server = {'command': cfg['mdkg'], 'args': ['--root', cfg['graph'], 'mcp'], 'env': {
            'MDKG_ACTOR': 'probe-worker', 'MDKG_RUN_TURN': sid, 'HOME': cfg['home'], 'XDG_CONFIG_HOME': cfg['xdg'],
            'MDKG_MAIN_REF': 'main', 'MDKG_TRUSTED_COMMON_DIR': cfg['repo'] + '/.git', 'MDKG_TRUSTED_WORKTREE': cfg['graph']}}
        return ['-p', '--session-id', sid, '--output-format', 'stream-json', '--verbose', '--tools', 'Read,Edit,Write,Glob,Grep,Bash',
                '--permission-mode', 'acceptEdits', '--permission-prompts', 'none', '--mcp-config', json.dumps({'mcpServers': {'mdkg': server}}),
                '--allowedTools', 'Read', '--disallowedTools', 'Bash(rm *)', '--model', MODEL]
    def stream(self, messages=()):
        return [{'type': 'system', 'subtype': 'init', 'session_id': 's', 'plugins': []}, *messages,
                {'type': 'result', 'subtype': 'success', 'session_id': 's', 'is_error': False}]
    def test_no_review_never_touches_candidate_or_creates_fixture(self):
        out, err = io.StringIO(), io.StringIO()
        with patch('tempfile.TemporaryDirectory', side_effect=AssertionError('created fixture')), patch('pathlib.Path.resolve', side_effect=AssertionError('accessed path')):
            code = main(['--live', '--mdkg', '/candidate', '--claude', '/claude', '--model', MODEL], out, err)
        self.assertEqual(code, 3); self.assertEqual(json.loads(out.getvalue())['outcome'], 'unverified')
        self.assertIn('/hooks', err.getvalue())
    def test_unknown_arguments_are_not_echoed(self):
        out = io.StringIO(); main(['--live', '--PRIVATE_VALUE'], out, io.StringIO())
        self.assertNotIn('PRIVATE_VALUE', out.getvalue())
    def test_only_named_model_and_explicit_candidate(self):
        args = parse_args(['--live', '--person-reviewed', '--model', 'wrong'])
        with self.assertRaises(Unverified): live(args, base_receipt())
        with self.assertRaises(Unverified): executable('mdkg', 'mdkg')
    def test_external_dangling_link_refuses_without_reading(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'settings'; path.symlink_to(Path(folder) / 'absent')
            with patch('builtins.open', side_effect=AssertionError('opened user data')):
                with self.assertRaises(Unverified): reject_present([path])
    def test_ancestor_and_managed_sources_are_covered(self):
        paths = external_paths(Path('/outside/project/tmp'), Path('/person'), 'linux')
        for p in ('/etc/claude-code/managed-settings.json', '/outside/project/.claude', '/.mcp.json', '/person/.claude/plugins', '/person/.claude/skills'):
            self.assertIn(Path(p), paths)
        self.assertNotIn(Path('/person/.claude.json'), paths)
        self.assertNotIn(Path('/person/.claude/.credentials.json'), paths)
    def test_redirect_refused_before_any_source_read(self):
        with patch('pathlib.Path.lstat', side_effect=AssertionError('unexpected stat')):
            with self.assertRaises(Unverified): preflight(Path('/tmp'), Path('/person'), {'CLAUDE_CONFIG_DIR': '/elsewhere'})
    def test_inventory_arguments_require_hash_and_keep_interactive_default(self):
        self.assertEqual(parse_args(['--live']).review_mode, 'interactive')
        args = parse_args(['--live', '--review-mode', 'inventory', '--inventory-sha256', 'A' * 64])
        self.assertEqual(args.inventory_sha256, 'a' * 64)
        for argv in (['--live', '--inventory-sha256', 'a' * 64],
                     ['--live', '--review-mode', 'inventory'],
                     ['--self-test', '--review-mode', 'inventory', '--inventory-sha256', 'a' * 64]):
            with self.assertRaises(InvalidInvocation): parse_args(argv)
        for value in ('a' * 63, 'g' * 64, 'a' * 64 + '\n', '/report.json'):
            with self.assertRaises(InvalidInvocation):
                parse_args(['--live', '--review-mode', 'inventory', '--inventory-sha256', value])
    def test_inventory_allows_catalog_and_hook_free_skills_without_reading_other_files(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder); plugins = home / '.claude/plugins'
            plugins.mkdir(parents=True); (plugins / 'cache').mkdir(); (plugins / 'synced').mkdir()
            (plugins / 'catalog-cache.json').write_text('PRIVATE_METADATA')
            for source in ('marketplaces/catalog', 'synced/catalog'):
                market = plugins / source / '.claude-plugin/marketplace.json'; market.parent.mkdir(parents=True)
                write_json(market, {'name': 'catalog', 'plugins': [{'name': 'not-installed', 'source': './payload',
                    'hooks': {'SessionStart': []}}]})
            write_json(plugins / 'installed_plugins.json', {'version': 2, 'plugins': {}})
            skill = home / '.claude/skills/synced/fixture/SKILL.md'; skill.parent.mkdir(parents=True)
            skill.write_text('---\nname: fixture\ndescription: Example hooks & patterns!\nallowed-tools:\n  - Read\n---\nPRIVATE_BODY\n')
            command = skill.parent / 'commands/example.md'; command.parent.mkdir()
            command.write_text('---\ndescription: Hook-free legacy command\n---\nPRIVATE_COMMAND\n')
            (home / '.claude/.credentials.json').write_text('PRIVATE_AUTH')
            original = os.open; opened = []
            def tracked(path, flags, *args, **kwargs):
                if not flags & os.O_DIRECTORY: opened.append(str(path))
                return original(path, flags, *args, **kwargs)
            with patch('os.open', side_effect=tracked): result = inventory_sources(home)
            self.assertEqual(set(opened), {'installed_plugins.json', 'SKILL.md', 'example.md'})
            self.assertEqual(result['skill_headers'], 1); self.assertEqual(result['installed_plugin_records'], 0)
            self.assertEqual(result['command_headers'], 1)
            self.assertTrue(result['cache_directory'] and result['synced_directory'])
            self.assertNotIn('PRIVATE', json.dumps(result))
            # The same directory presence still refuses in default mode.
            with self.assertRaises(Unverified): reject_present(external_paths(Path('/outside'), home, 'linux'))
    def test_inventory_plugin_hook_and_mcp_definitions_refuse(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder); root = home / '.claude/plugins/cache/catalog/plugin/version'
            manifest = root / '.claude-plugin/plugin.json'; manifest.parent.mkdir(parents=True)
            registry = home / '.claude/plugins/installed_plugins.json'
            write_json(registry, {'version': 2, 'plugins': {'fixture@catalog': [{'installPath': str(root)}]}})
            write_json(manifest, {'name': 'fixture'})
            with self.assertRaises(Unverified): inventory_sources(home)
            for key in ('hooks', 'mcpServers'):
                write_json(manifest, {'name': 'fixture', key: './custom.json'})
                with self.subTest(key=key), self.assertRaises(Unverified): inventory_sources(home)
            manifest.unlink()
            for name in ('.mcp.json', 'hooks/hooks.json'):
                path = root / name; path.parent.mkdir(exist_ok=True); path.write_text('{}')
                with self.subTest(name=name), self.assertRaises(Unverified): inventory_sources(home)
                path.unlink()
            # A registry pointing to an uninspected external tree never opens it.
            write_json(registry, {'version': 2, 'plugins': {'fixture@catalog': [{'installPath': '/elsewhere/payload'}]}})
            with self.assertRaises(Unverified): inventory_sources(home)
    def test_inventory_registry_and_manifest_alone_refuse_without_reading_payload(self):
        for version in (1, 2):
            with self.subTest(version=version), tempfile.TemporaryDirectory() as folder:
                home = Path(folder); plugins = home / '.claude/plugins'; plugins.mkdir(parents=True)
                entry = {'installPath': '/uninspected/payload'}
                write_json(plugins / 'installed_plugins.json', {'version': version,
                    'plugins': {'fixture@catalog': entry if version == 1 else [entry]}})
                with self.assertRaisesRegex(Unverified, 'registry is not empty'): inventory_sources(home)
        for source in ('plugins/synced/fixture', 'skills/fixture'):
            with self.subTest(source=source), tempfile.TemporaryDirectory() as folder:
                home = Path(folder); manifest = home / '.claude' / source / '.claude-plugin/plugin.json'
                manifest.parent.mkdir(parents=True); manifest.write_text('UNREAD_PAYLOAD')
                with self.assertRaisesRegex(Unverified, 'payload manifest exists'): inventory_sources(home)
    def test_inventory_marketplace_hook_with_harmless_payload_refuses(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder); plugins = home / '.claude/plugins'
            root = plugins / 'cache/catalog/fixture/1'; manifest = root / '.claude-plugin/plugin.json'
            manifest.parent.mkdir(parents=True); write_json(manifest, {'name': 'fixture'})
            write_json(plugins / 'installed_plugins.json', {'version': 2, 'plugins': {'fixture@catalog': [{'installPath': str(root)}]}})
            market = plugins / 'marketplaces/catalog/.claude-plugin/marketplace.json'; market.parent.mkdir(parents=True)
            write_json(market, {'name': 'catalog', 'plugins': [{'name': 'fixture', 'source': './fixture',
                'hooks': {'SessionStart': [{'hooks': [{'type': 'command', 'command': 'fixture-placeholder'}]}]}}]})
            with self.assertRaises(Unverified): inventory_sources(home)
    def test_inventory_cached_payload_without_registry_or_manifest_refuses(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder); root = home / '.claude/plugins/cache/catalog/fixture/1'
            root.mkdir(parents=True); (root / 'README.md').write_text('No manifest or hook declarations.\n')
            with self.assertRaises(Unverified): inventory_sources(home)
    def test_inventory_legacy_command_hook_headers_refuse(self):
        for name in ('commands/review.md', 'commands/nested/review.md', 'commands/REVIEW.MD'):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as folder:
                home = Path(folder); command = home / '.claude/skills/fixture' / name
                command.parent.mkdir(parents=True)
                for header in ('hooks: {}', '"hooks": {}', 'description: |\n  ---\nhooks: {}', '# comment\n  hooks: {}'):
                    command.write_text('---\n' + header + '\n---\nFixture command.\n')
                    with self.subTest(header=header), self.assertRaises(Unverified): inventory_sources(home)
    def test_inventory_legacy_commands_use_bounded_no_follow_reads(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder); command = home / '.claude/skills/fixture/commands/review.md'
            command.parent.mkdir(parents=True); outside = home / 'private'; outside.write_text('UNREAD_PRIVATE')
            command.symlink_to(outside)
            with self.assertRaises(Unverified): inventory_sources(home)
            command.unlink(); os.link(outside, command)
            with self.assertRaises(Unverified): inventory_sources(home)
            command.unlink(); command.write_text('x' * (1024 * 1024 + 1))
            with self.assertRaises(Unverified): inventory_sources(home)
    def test_inventory_skill_hooks_and_ambiguous_headers_refuse(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder); skill = home / '.claude/skills/fixture/SKILL.md'; skill.parent.mkdir(parents=True)
            for header in ('hooks: {}', '"hooks": {}', "'hooks': {}", '  name: indented\n  hooks: {}',
                           '# comment\n  hooks: {}', 'description: |\n  ---\nhooks: {}',
                           'description: |\n  ...\nhooks: {}', '{hooks: {}}', '"h\\u006foks": {}', '<<: *alias'):
                skill.write_text('---\n' + header + '\n---\nbody\n')
                with self.subTest(header=header), self.assertRaises(Unverified): inventory_sources(home)
            skill.write_text('---\nname: unfinished\n')
            with self.assertRaises(Unverified): inventory_sources(home)
    def test_inventory_links_unreadable_and_malformed_registry_refuse(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder); plugins = home / '.claude/plugins'; plugins.mkdir(parents=True)
            link = plugins / 'cache'; link.symlink_to(home / 'absent')
            with self.assertRaises(Unverified): inventory_sources(home)
            link.unlink()
            with patch('os.scandir', side_effect=PermissionError()):
                with self.assertRaises(Unverified): inventory_sources(home)
            registry = plugins / 'installed_plugins.json'
            for value in ('{', '{"version":2,"version":2,"plugins":{}}', '{"version":3,"plugins":{}}'):
                registry.write_text(value)
                with self.subTest(value=value), self.assertRaises(Unverified): inventory_sources(home)
    def test_inventory_control_file_case_aliases_refuse(self):
        for name in ('hooks/HOOKS.JSON', '.MCP.JSON', '.CLAUDE-PLUGIN/plugin.json',
                     '.claude-plugin/PLUGIN.JSON', 'skill.md', 'INSTALLED_PLUGINS.JSON'):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as folder:
                home = Path(folder); path = home / '.claude/plugins' / name
                path.parent.mkdir(parents=True); path.write_text('{}')
                with self.assertRaises(Unverified): inventory_sources(home)
    def test_inventory_directory_replacement_cannot_read_external_payload(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder); cache = home / '.claude/plugins/cache'; cache.mkdir(parents=True)
            outside = home / 'outside'; outside.mkdir(); (outside / 'SKILL.md').write_text('PRIVATE_EXTERNAL')
            original = os.open; reads = []
            def swapped(name, flags, *args, **kwargs):
                if name == 'cache':
                    cache.rmdir(); cache.symlink_to(outside, target_is_directory=True)
                if not flags & os.O_DIRECTORY: reads.append(name)
                return original(name, flags, *args, **kwargs)
            with patch('os.open', side_effect=swapped):
                with self.assertRaises(Unverified): inventory_sources(home)
            self.assertEqual(reads, [])
    def test_inventory_open_directory_replacement_is_detected(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder); cache = home / '.claude/plugins/cache'; cache.mkdir(parents=True)
            outside = home / 'outside'; outside.mkdir(); (outside / 'SKILL.md').write_text('PRIVATE_EXTERNAL')
            original = os.open; reads = []
            def swapped(name, flags, *args, **kwargs):
                fd = original(name, flags, *args, **kwargs)
                if name == 'cache':
                    cache.rename(cache.with_name('previous')); cache.symlink_to(outside, target_is_directory=True)
                if not flags & os.O_DIRECTORY: reads.append(name)
                return fd
            with patch('os.open', side_effect=swapped):
                with self.assertRaises(Unverified): inventory_sources(home)
            self.assertEqual(reads, [])
    def test_inventory_preserves_other_refusals(self):
        standard = set(external_paths(Path('/outside/tmp'), Path('/person'), 'linux'))
        inventory = set(external_paths(Path('/outside/tmp'), Path('/person'), 'linux', 'inventory'))
        self.assertEqual(standard - inventory, {Path('/person/.claude/plugins'), Path('/person/.claude/skills')})
        self.assertFalse(inventory - standard)
        with patch(__name__ + '.inventory_sources', side_effect=AssertionError('inventory read before redirect check')):
            with self.assertRaises(Unverified): preflight(Path('/tmp'), Path('/person'), {'CLAUDE_CONFIG_DIR': '/elsewhere'}, 'inventory')
    def test_review_modes_wait_for_confirmation_and_record_approval_only_after_it(self):
        for mode in ('interactive', 'inventory'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as folder:
                args = parse_args(['--live', '--review-mode', mode] + (['--inventory-sha256', 'a' * 64] if mode == 'inventory' else []))
                obj = object.__new__(LiveCheck); obj.root = Path(folder); obj.repo = obj.root / 'project'; obj.home = Path('/person')
                obj.args = args; obj.env = {}; obj.cfg = {'repo': str(obj.repo), 'worker': str(obj.root / 'worker')}
                for cwd in obj.cfg.values():
                    path = Path(cwd) / '.claude/settings.json'; path.parent.mkdir(parents=True); write_json(path, {})
                expected = digest(obj.repo / '.claude/settings.json')
                obj.report = {'person_reviewed': False, 'fixture': {'effective_settings_sha256': expected}}
                validator = wait_review
                def confirmed(marker, token, timeout):
                    self.assertFalse(obj.report['person_reviewed']); self.assertNotIn('approval', obj.report)
                    marker.write_text(token + '\n'); marker.chmod(0o600); validator(marker, token, 0)
                with patch(__name__ + '.subscription_login'), patch(__name__ + '.preflight') as check, \
                     patch(__name__ + '.wait_review', side_effect=confirmed), \
                     patch.object(obj, 'wrapper', return_value=obj.root / 'helper') as helper, patch('sys.stderr', new_callable=io.StringIO) as err:
                    obj.review()
                self.assertEqual(helper.call_count, 1 if mode == 'interactive' else 0)
                self.assertEqual('/hooks' in err.getvalue(), mode == 'interactive')
                self.assertEqual(check.call_args.args[-1], mode)
                self.assertEqual(obj.report['approval'], 'approved by the person')
                self.assertTrue(obj.report['person_reviewed'])
    def test_environment_drops_parent_keys_and_pins(self):
        with tempfile.TemporaryDirectory() as folder:
            env = environment(Path(folder), Path('/normal-home'))
            self.assertEqual(env['HOME'], '/normal-home')
            self.assertNotIn('ANTHROPIC_API_KEY', env); self.assertNotIn('MDKG_RUN_TURN', env)
            self.assertTrue(env['TMPDIR'].startswith(folder)); self.assertEqual(env['GIT_CONFIG_NOSYSTEM'], '1')
    def test_environment_uses_os_account_with_no_additional_inherited_variables(self):
        expected = {'PATH', 'HOME', 'LANG', 'TERM', 'NO_COLOR', 'USER', 'LOGNAME',
                    'GIT_CONFIG_GLOBAL', 'GIT_CONFIG_NOSYSTEM', 'GIT_TERMINAL_PROMPT',
                    'GIT_AUTHOR_NAME', 'GIT_AUTHOR_EMAIL', 'GIT_COMMITTER_NAME', 'GIT_COMMITTER_EMAIL',
                    'DISABLE_AUTOUPDATER', 'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC',
                    'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_RUNTIME_DIR',
                    'TMPDIR', 'TMP', 'TEMP'}
        poison = {'USER': 'inherited-user', 'LOGNAME': 'inherited-login', 'ANTHROPIC_API_KEY': 'fixture-key',
                  'MDKG_RUN_TURN': 'inherited-turn', 'EXTRA_PARENT_VARIABLE': 'fixture-value'}
        with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, poison, clear=True), \
             patch('os.getuid', return_value=12345), \
             patch('pwd.getpwuid', return_value=unittest.mock.Mock(pw_name='fixture-account')) as lookup:
            env = environment(Path(folder), Path('/normal-home'))
            repeated = environment(Path(folder), Path('/normal-home'), create=False)
        self.assertEqual(env['USER'], 'fixture-account'); self.assertEqual(env['LOGNAME'], 'fixture-account')
        self.assertEqual(set(env), expected); self.assertEqual(repeated, env)
        self.assertEqual(lookup.call_args_list, [unittest.mock.call(12345), unittest.mock.call(12345)])
    def test_exact_write_only_no_shell_or_external_read(self):
        with tempfile.TemporaryDirectory() as folder:
            cfg = self.fixture(Path(folder).resolve())
            valid = {'tool_name': 'Write', 'tool_input': {'file_path': cfg['guard'], 'content': 'DENIED_WRITE\n'}}
            self.assertTrue(safe_tool(valid, cfg, 'edit-deny'))
            for name in ('Bash', 'Read', 'Edit', 'Agent', 'mcp__mdkg__room_post'):
                self.assertFalse(safe_tool({**valid, 'tool_name': name}, cfg, 'edit-deny'))
            self.assertFalse(safe_tool({'tool_name': 'Write', 'tool_input': {'file_path': '/outside', 'content': 'DENIED_WRITE\n'}}, cfg, 'edit-deny'))
            self.assertFalse(safe_tool(valid, cfg, 'start'))
            self.assertFalse(safe_tool(valid, cfg, 'edit-allow'))
            self.assertTrue(safe_tool({'tool_name': 'Read', 'tool_input': {'file_path': cfg['guard']}}, cfg, 'edit-allow'))
            self.assertFalse(safe_tool({'tool_name': 'Read', 'tool_input': {'file_path': '/outside'}}, cfg, 'edit-allow'))
    def test_hardlinks_and_symlinks_are_refused(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve(); cfg = self.fixture(root)
            value = {'tool_name': 'Write', 'tool_input': {'file_path': cfg['guard'], 'content': 'CLAIMED_WRITE\n'}}
            os.link(cfg['guard'], root / 'alias'); self.assertFalse(safe_tool(value, cfg, 'edit-allow'))
            Path(cfg['guard']).unlink(); Path(cfg['guard']).symlink_to(root / 'alias')
            self.assertFalse(safe_tool(value, cfg, 'edit-allow'))
    def test_tool_args_keep_policy_and_narrow_permissions(self):
        cfg = {'claude': '/claude', 'guard': '/fixture/guard.txt'}
        args = turn_args(cfg, 's', 'edit-allow', {'mcpServers': {}})
        self.assertEqual(args[args.index('--setting-sources') + 1], 'project,local')
        self.assertIn('--strict-mcp-config', args)
        self.assertIn('--allowedTools=Read(//fixture/guard.txt),Edit(//fixture/guard.txt)', args)
        self.assertEqual(json.loads(args[args.index('--settings') + 1]), POLICY)
        self.assertNotIn('--dangerously-skip-permissions', args)
    def assert_variadic_values_are_attached(self, argv):
        # A following positional/subcommand must never become another value
        # of these variadic options. Require --option=value, including empty.
        self.assertTrue({'--mcp-config', '--allowedTools', '--disallowedTools', '--tools'}.isdisjoint(argv))
    def test_claude_variadic_options_cannot_consume_auth_or_prompt_tokens(self):
        cfg = {'claude': '/claude', 'guard': '/fixture path/guard.txt'}
        good = {'loggedIn': True, 'authMethod': 'claude.ai', 'apiProvider': 'firstParty'}
        with patch(__name__ + '.command', return_value=(0, json.dumps(good), '')) as run:
            subscription_login(cfg, '/fixture', {})
        argv = run.call_args.args[0]
        self.assert_variadic_values_are_attached(argv)
        self.assertEqual(argv[-3:], ['auth', 'status', '--json'])
        self.assertIn('--mcp-config={"mcpServers":{}}', argv)
        for phase in (*PHASES, 'worker'):
            argv = turn_args(cfg, 's', phase, {'mcpServers': {}})
            self.assert_variadic_values_are_attached(argv + ['fixture prompt'])
            tools = next(arg for arg in argv if arg.startswith('--tools='))
            self.assertEqual(tools, '--tools=Read,Write' if phase.startswith('edit') else '--tools=')
    def test_review_has_commands_but_automatic_turns_do_not(self):
        cfg = {'claude': '/claude'}
        self.assertNotIn('--disable-slash-commands', common_claude(cfg, review=True))
        self.assertIn('--disable-slash-commands', common_claude(cfg))
        sources = external_paths(Path('/tmp'), Path('/person'), 'linux')
        self.assertIn(Path('/person/.claude/commands'), sources)
        self.assertIn(Path('/person/.claude/agents'), sources)
    def test_review_helper_drops_terminal_credentials_redirects_and_pins(self):
        class Executed(Exception): pass
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve(); cfg = self.fixture(root)
            cfg['root'] = str(root); cfg['sessions'] = {'review-id': 'review'}
            environment(root, Path(cfg['home']))
            config = root / 'review.json'; write_json(config, cfg)
            poison = {'ANTHROPIC_API_KEY': 'PRIVATE_KEY', 'ANTHROPIC_AUTH_TOKEN': 'PRIVATE_TOKEN',
                      'CLAUDE_CONFIG_DIR': '/outside', 'CLAUDE_CODE_PLUGIN_DIRS': '/outside',
                      'MDKG_TRUSTED_WORKTREE': '/outside', 'MDKG_RUN_TURN': 'parent-turn'}
            with patch.dict(os.environ, poison, clear=True), patch(__name__ + '.preflight') as check, \
                 patch(__name__ + '.subscription_login') as auth, patch('os.chdir'), \
                 patch('os.execve', side_effect=Executed()) as launch:
                with self.assertRaises(Executed): review_driver(str(config))
            env = launch.call_args.args[2]
            self.assertTrue(set(poison).isdisjoint(env))
            self.assertEqual(env['HOME'], cfg['home'])
            self.assertEqual(auth.call_args.args[2], env)
            self.assertEqual(check.call_args.args[2], {})
            self.assertNotIn('--disable-slash-commands', launch.call_args.args[1])
            self.assert_variadic_values_are_attached(launch.call_args.args[1])
            self.assertIn('--tools=', launch.call_args.args[1])
    def test_subscription_status_is_filtered_and_api_auth_refused(self):
        cfg = {'claude': '/claude'}
        good = {'loggedIn': True, 'authMethod': 'claude.ai', 'apiProvider': 'firstParty', 'email': 'PRIVATE_VALUE'}
        with patch(__name__ + '.command', return_value=(0, json.dumps(good), '')):
            result = subscription_login(cfg, '/fixture', {})
        self.assertNotIn('PRIVATE_VALUE', json.dumps(result))
        for method in ('api_key', 'api_key_helper', 'oauth_token', 'third_party'):
            with patch(__name__ + '.command', return_value=(0, json.dumps({**good, 'authMethod': method}), '')):
                with self.assertRaises(Unverified): subscription_login(cfg, '/fixture', {})
    def test_direct_turn_refuses_api_auth_before_model_process(self):
        obj = object.__new__(LiveCheck)
        obj.root = Path('/fixture'); obj.home = Path('/person'); obj.repo = Path('/fixture/project')
        obj.cfg = {'claude': '/claude'}; obj.env = {}; obj.report = {}
        with patch(__name__ + '.preflight'), patch(__name__ + '.subscription_login', side_effect=Unverified('API auth refused')), \
             patch.object(obj, 'run', side_effect=AssertionError('model launched')):
            with self.assertRaises(Unverified): obj.turn('start', 'prompt')
    def test_worker_rejects_extra_settings_and_changed_bindings(self):
        with tempfile.TemporaryDirectory() as folder:
            cfg = self.fixture(Path(folder).resolve()); argv = self.worker_argv(cfg)
            self.assertEqual(worker_options(argv, cfg)[0], argv[2])
            for addition in (['--settings', '{}'], ['--resume', 's'], ['--model', MODEL]):
                with self.assertRaises(CheckFailed): worker_options(argv + addition, cfg)
            broken = argv.copy(); at = broken.index('--mcp-config') + 1
            obj = json.loads(broken[at]); obj['mcpServers']['mdkg']['env']['MDKG_TRUSTED_WORKTREE'] = '/outside'
            broken[at] = json.dumps(obj)
            with self.assertRaises(CheckFailed): worker_options(broken, cfg)
    def test_mcp_filter_exposes_only_inbox_and_rejects_writes(self):
        response = {'result': {'tools': [{'name': 'room_post'}, {'name': 'inbox'}], 'nextCursor': 'more'}}
        self.assertEqual(mcp_filtered('tools/list', response)['result'], {'tools': [{'name': 'inbox'}]})
        self.assertTrue(mcp_allowed({'method': 'tools/call', 'params': {'name': 'inbox', 'arguments': {}}}))
        self.assertTrue(mcp_allowed({'method': 'tools/call', 'params': {'name': 'inbox', 'arguments': {}, '_meta': {'progressToken': 1}}}))
        for name, args in [('room_post', {}), ('inbox', {'as': 'someone-else'})]:
            self.assertFalse(mcp_allowed({'method': 'tools/call', 'params': {'name': name, 'arguments': args}}))
    def test_worker_guard_and_mcp_accept_only_default_inbox_arguments(self):
        for args, allowed in (({}, True), ({'all': False}, True), ({'all': True}, False),
                              ({'all': 0}, False), ({'all': None}, False), ({'all': 'false'}, False),
                              ({'all': False, 'after': 0}, False), ({'limit': 1}, False), ([], False), (None, False)):
            with self.subTest(args=args):
                self.assertIs(safe_tool({'tool_name': 'mcp__mdkg__inbox', 'tool_input': args}, {}, 'worker'), allowed)
                self.assertIs(mcp_allowed({'method': 'tools/call', 'params': {'name': 'inbox', 'arguments': args}}), allowed)
        self.assertFalse(safe_tool({'tool_name': 'mcp__mdkg__room_post', 'tool_input': {}}, {}, 'worker'))
    def test_worker_inbox_evidence_excludes_only_correlated_guard_denials(self):
        def evidence(first_args, allowed, result_error=True, name='mcp__mdkg__inbox'):
            calls = [{'type': 'tool_use', 'id': 'attempt', 'name': name, 'input': first_args},
                     {'type': 'tool_use', 'id': 'read', 'name': 'mcp__mdkg__inbox', 'input': {}}]
            results = [{'type': 'tool_result', 'tool_use_id': c['id'], 'is_error': result_error if i == 0 else False,
                        'content': 'denied' if i == 0 else 'fixture mail'} for i, c in enumerate(calls)]
            stream = self.stream([{'type': 'assistant', 'message': {'content': calls}},
                                  {'type': 'user', 'message': {'content': results}}])
            events = [{'kind': 'guard', 'session': 's', 'tool_use_id': c['id'], 'tool': c['name'],
                       'allowed': allowed if i == 0 else True} for i, c in enumerate(calls)]
            return stream, events, calls
        for args in ({'all': False}, {'all': True}):
            stream, events, calls = evidence(args, False)
            self.assertEqual(worker_inbox_calls(stream, events, 's'), [calls[1]])
        stream, events, calls = evidence({'all': False}, True, False)
        self.assertEqual(worker_inbox_calls(stream, events, 's'), calls)
        for args, allowed, error, name in (({'all': True}, True, False, 'mcp__mdkg__inbox'),
                                         ({'all': True}, True, True, 'mcp__mdkg__inbox'),
                                         ({}, False, False, 'mcp__mdkg__inbox'),
                                         ({}, False, True, 'Bash'), ({}, False, True, 'mcp__mdkg__room_post')):
            stream, events, _ = evidence(args, allowed, error, name)
            with self.assertRaises(CheckFailed): worker_inbox_calls(stream, events, 's')
        stream, events, _ = evidence({'all': True}, False)
        for changed in (events[1:], [{**events[0], 'session': 'other'}, events[1]],
                        [{**events[0], 'tool_use_id': 'other'}, events[1]], events + [events[0]]):
            with self.assertRaises(CheckFailed): worker_inbox_calls(stream, changed, 's')
        events[1]['allowed'] = False
        stream[2]['message']['content'][1]['is_error'] = True
        with self.assertRaises(CheckFailed): worker_inbox_calls(stream, events, 's')
    def test_worker_guard_records_call_identity_for_denial_evidence(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve(); cfg = self.fixture(root); cfg['sessions'] = {'s': 'worker'}
            path = root / 'config.json'; write_json(path, cfg)
            value = {'session_id': 's', 'cwd': cfg['worker'], 'hook_event_name': 'PreToolUse',
                     'tool_use_id': 'attempt', 'tool_name': 'mcp__mdkg__inbox', 'tool_input': {'all': True}}
            out = io.StringIO()
            with patch('sys.stdin', io.StringIO(json.dumps(value))), patch('sys.stdout', out):
                self.assertEqual(hook_driver(str(path), 'guard', []), 0)
            self.assertEqual(rows(cfg['events']), [{'kind': 'guard', 'session': 's', 'allowed': False,
                              'tool': 'mcp__mdkg__inbox', 'tool_use_id': 'attempt'}])
            self.assertEqual(json.loads(out.getvalue())['hookSpecificOutput']['permissionDecision'], 'deny')
    def test_success_requires_terminal_result_and_no_plugin(self):
        stream = self.stream(); self.assertEqual(parse_stream('\n'.join(map(json.dumps, stream)), 's'), stream)
        for changed in (stream[:-1], [{**stream[0], 'plugins': [{'name': 'foreign'}]}, stream[-1]]):
            with self.assertRaises(CheckFailed): parse_stream('\n'.join(map(json.dumps, changed)), 's')
    def test_assistant_mail_header_requires_separate_tokens_in_final_text(self):
        def message(text, **extra):
            return {'type': 'assistant', 'message': {'content': [{'type': 'text', 'text': text}]}, **extra}
        for sender in ('start-sender', 'stop-sender'):
            for text in (f'mdkg mail: #1, note, from {sender}. The mail can wait.',
                         f'A note arrived from {sender}; its mail number is **#1**.',
                         f'#1 note from {sender}'):
                self.assertTrue(assistant_has_mail_header(self.stream([message(text)]), 1, 'note', sender))
            for text in (f'note from {sender}', f'#1 from {sender}', '#1 note',
                         f'#10 note from {sender}', f'#11 note from {sender}',
                         f'#1 noteworthy from {sender}', f'#1 note from not-{sender}',
                         f'#1 note from {sender}-extra'):
                self.assertFalse(assistant_has_mail_header(self.stream([message(text)]), 1, 'note', sender))
            valid = message(f'#1 note from {sender}')
            self.assertFalse(assistant_has_mail_header(self.stream([valid, message('The mail can wait.')]), 1, 'note', sender))
            self.assertFalse(assistant_has_mail_header(self.stream([message('The mail can wait.'),
                             message(f'#1 note from {sender}', parent_tool_use_id='nested')]), 1, 'note', sender))
        self.assertFalse(assistant_has_mail_header(self.stream(), 1, 'note', 'start-sender'))
    def test_start_scenario_accepts_paraphrase_but_keeps_hook_and_body_checks(self):
        exact = '#1 note from start-sender'
        for text, hook_text, diagnostic in (
                ('mdkg mail: #1, note, from start-sender.', exact, 'end of start fixture'),
                ('A note from start-sender has number #1.', exact, 'end of start fixture'),
                ('note from start-sender', exact, 'assistant did not receive the start header'),
                ('#1 from start-sender', exact, 'assistant did not receive the start header'),
                ('#1 note', exact, 'assistant did not receive the start header'),
                (exact + ' PRIVATE_BODY', exact, 'mail body reached model/hook evidence'),
                (exact, '#1, note, from start-sender', 'missing start hook header evidence')):
            with self.subTest(text=text, hook_text=hook_text):
                obj = object.__new__(LiveCheck); obj.args = parse_args(['--live'])
                obj.root = Path('/fixture'); obj.repo = obj.root / 'project'; obj.home = Path('/person')
                obj.cfg = {'claude': '/claude', 'guard': '/fixture/guard.txt', 'sentinel': 'PRIVATE_BODY'}
                obj.sids = {p: 's' for p in PHASES}; obj.env = {}; obj.report = base_receipt(); obj.receipts = {}
                stream = self.stream([{'type': 'assistant', 'message': {'content': [{'type': 'text', 'text': text}]}}])
                events = [{'kind': 'end', 'harness_event': e, 'out': hook_text} for e in ('SessionStart', 'UserPromptSubmit')]
                with patch(__name__ + '.preflight'), patch(__name__ + '.subscription_login'), \
                     patch.object(obj, 'mdkg', return_value={'message': {'seq': 1}}), \
                     patch.object(obj, 'events', return_value=events), \
                     patch.object(obj, 'run', side_effect=['\n'.join(map(json.dumps, stream)), CheckFailed('end of start fixture')]):
                    with self.assertRaisesRegex(CheckFailed, diagnostic): obj.scenarios()
                self.assertEqual('start' in obj.report['checks'], diagnostic == 'end of start fixture')
    def test_builtin_plugins_require_opt_in_and_matching_path_and_source(self):
        plugin = {'name': 'cc-plugin-agents-md', 'path': 'builtin', 'source': 'cc-plugin-agents-md@builtin'}
        stream = self.stream(); stream[0]['plugins'] = [plugin]
        raw = '\n'.join(map(json.dumps, stream))
        with self.assertRaises(CheckFailed): parse_stream(raw, 's')
        self.assertEqual(parse_stream(raw, 's', allow_builtin_plugins=True), stream)
        for other in ({**plugin, 'path': '/external/plugin'}, {**plugin, 'source': 'plugin@marketplace'},
                      {**plugin, 'path': 'Builtin'}, {**plugin, 'source': 'plugin@builtin-extra'},
                      {**plugin, 'source': None}, {**plugin, 'path': None},
                      {**plugin, 'name': ''}, {**plugin, 'name': None}, {}, 'builtin'):
            with self.subTest(plugin=other):
                stream[0]['plugins'] = [plugin, other]
                with self.assertRaises(CheckFailed):
                    parse_stream('\n'.join(map(json.dumps, stream)), 's', allow_builtin_plugins=True)
        for malformed in (None, {}, '', 'builtin'):
            stream[0]['plugins'] = malformed
            with self.assertRaises(CheckFailed):
                parse_stream('\n'.join(map(json.dumps, stream)), 's', allow_builtin_plugins=True)
    def test_session_receipts_keep_exact_builtin_names_for_each_phase(self):
        names = ['cc-plugin-agents-md', 'cc-plugin-telemetry', 'cc-plugin-plugin-authoring']
        stream = self.stream()
        stream[0]['plugins'] = [{'name': n, 'source': n + '@builtin', 'path': 'builtin'} for n in names]
        obj = object.__new__(LiveCheck)
        obj.args = parse_args(['--live', '--allow-builtin-plugins'])
        obj.report = base_receipt()
        for phase in (*PHASES, 'worker'):
            self.assertEqual(obj.parse_session('\n'.join(map(json.dumps, stream)), 's', phase), stream)
            self.assertEqual(obj.report['sessions'][phase], {'id': 's', 'builtin_plugins': names})
        obj.parse_session('\n'.join(map(json.dumps, self.stream())), 's', 'empty')
        self.assertEqual(obj.report['sessions']['empty']['builtin_plugins'], [])
        obj.args = parse_args(['--live'])
        with self.assertRaises(CheckFailed): obj.parse_session('\n'.join(map(json.dumps, stream)), 's', 'refused')
        self.assertNotIn('refused', obj.report['sessions'])
    def test_completed_turn_preserves_builtin_plugin_evidence(self):
        stream = self.stream()
        stream[0]['plugins'] = [{'name': 'cc-plugin-telemetry', 'source': 'cc-plugin-telemetry@builtin', 'path': 'builtin'}]
        obj = object.__new__(LiveCheck)
        obj.args = parse_args(['--live', '--allow-builtin-plugins'])
        obj.root = Path('/fixture'); obj.repo = obj.root / 'project'; obj.home = Path('/person')
        obj.cfg = {'claude': '/claude', 'guard': '/fixture/project/guard.txt', 'sentinel': 'PRIVATE_BODY'}
        obj.sids = {p: 's' for p in PHASES}; obj.env = {}; obj.report = base_receipt(); obj.receipts = {}
        with patch(__name__ + '.preflight'), patch(__name__ + '.subscription_login'), \
             patch.object(obj, 'run', return_value='\n'.join(map(json.dumps, stream))), \
             patch.object(obj, 'events', return_value=[]):
            for phase in PHASES:
                obj.turn(phase, 'fixture prompt')
                self.assertTrue(obj.report['sessions'][phase]['completed'])
                self.assertEqual(obj.report['sessions'][phase]['builtin_plugins'], ['cc-plugin-telemetry'])
    def test_builtin_flag_expands_scope_even_when_review_is_missing(self):
        for allowed in (False, True):
            with self.subTest(allowed=allowed):
                out, err = io.StringIO(), io.StringIO()
                argv = ['--live'] + (['--allow-builtin-plugins'] if allowed else [])
                with patch('pathlib.Path.resolve', side_effect=AssertionError('accessed path')):
                    self.assertEqual(main(argv, out, err), 3)
                receipt = json.loads(out.getvalue())
                self.assertIs(receipt['allow_builtin_plugins'], allowed)
                self.assertEqual(receipt['scope'], SCOPE + (' and Claude built-in plugins' if allowed else ''))
                self.assertEqual(receipt['outcome'], 'unverified')
                if allowed: self.assertIn('Claude built-in plugins', err.getvalue())
        with self.assertRaises(InvalidInvocation): parse_args(['--self-test', '--allow-builtin-plugins'])
    def test_success_receipt_retains_builtin_effects_scope(self):
        for allowed in (False, True):
            args = parse_args(['--live', '--person-reviewed', '--model', MODEL]
                              + (['--allow-builtin-plugins'] if allowed else []))
            report = base_receipt()
            with patch(__name__ + '.preflight'), patch(__name__ + '.executable', return_value=Path('/candidate')), \
                 patch(__name__ + '.LiveCheck'), patch('sys.stderr', new_callable=io.StringIO):
                live(args, report)
            self.assertEqual(report['outcome'], 'pass with unverified effects')
            self.assertIs(report['allow_builtin_plugins'], allowed)
            self.assertEqual(report['scope'], SCOPE + (' and Claude built-in plugins' if allowed else ''))
    def test_body_scan_covers_nested_messages_and_hook_output(self):
        check_sentinel(self.stream(), [], 'PRIVATE_BODY')
        with self.assertRaises(CheckFailed): check_sentinel(self.stream([{'type': 'user', 'message': {'content': [{'type': 'tool_result', 'content': 'PRIVATE_BODY'}]}}]), [], 'PRIVATE_BODY')
        with self.assertRaises(CheckFailed): check_sentinel(self.stream(), [{'out': 'PRIVATE_BODY'}], 'PRIVATE_BODY')
    def test_reply_text_is_not_inbox_proof(self):
        stream = self.stream([{'type': 'assistant', 'message': {'content': [{'type': 'text', 'text': 'WORKER_REPLY'}]}}])
        self.assertEqual(assistant_text(stream), 'WORKER_REPLY'); self.assertFalse(blocks(stream, 'tool_use'))
        with self.assertRaises(CheckFailed): result_for(stream, {'id': 'inbox-call'})
    def test_stop_mail_is_after_candidate_read_before_output(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve(); cfg = self.fixture(root); path = root / 'config.json'; write_json(path, cfg)
            value = {'session_id': 's', 'cwd': str(root), 'hook_event_name': 'UserPromptSubmit'}
            out = io.StringIO(); order = []
            def candidate(*args, **kwargs): order.append('read'); self.assertFalse(out.getvalue()); return 0, '', ''
            def send(*args, **kwargs):
                order.append('send'); self.assertEqual(order, ['read', 'send']); self.assertFalse(out.getvalue())
                return {'message': {'seq': 6}}
            with patch('sys.stdin', io.StringIO(json.dumps(value))), patch('sys.stdout', out), patch(__name__ + '.command', candidate), patch(__name__ + '.mdkg', send):
                self.assertEqual(hook_driver(str(path), 'observe', ['hook', 'start', '--harness', 'claude']), 0)
            self.assertEqual([r['kind'] for r in rows(cfg['events'])], ['begin', 'start_read_complete', 'stop_mail', 'end'])
            self.assertEqual(Path(cfg['audit']).read_text(), 'UNCLAIMED_AFTER_START\n')
    def test_cleanup_inventory_never_opens_transcripts(self):
        with tempfile.TemporaryDirectory() as folder:
            cfg = self.fixture(Path(folder)); item = {'transcript': '/person/.claude/projects/untrusted.jsonl'}
            with patch('builtins.open', side_effect=AssertionError('read transcript')):
                result = cleanup_inventory(cfg, [item])
            self.assertIn(item['transcript'], result['hook_reported_transcript_paths_unverified'])
            self.assertFalse(result['automatic_user_home_cleanup'])
    def test_guard_config_failure_still_denies(self):
        out = io.StringIO()
        with patch(__name__ + '.read_json', side_effect=OSError('unreadable')), patch('sys.stdout', out):
            self.assertEqual(hook_driver('/fixture/config', 'guard', []), 0)
        self.assertEqual(json.loads(out.getvalue())['hookSpecificOutput']['permissionDecision'], 'deny')
    def test_partial_event_log_does_not_skip_worker_cleanup(self):
        with tempfile.TemporaryDirectory() as folder:
            cfg = self.fixture(Path(folder))
            Path(cfg['events']).write_text(json.dumps({'kind': 'worker_spawn', 'pid': 43210, 'pgid': 43210, 'session': 's'}) + '\n{"partial":')
            obj = object.__new__(LiveCheck); obj.cfg = cfg; obj.report = {}
            with patch('os.killpg') as kill:
                obj.cleanup()
            self.assertEqual(kill.call_args_list, [unittest.mock.call(43210, signal.SIGTERM), unittest.mock.call(43210, signal.SIGKILL)])
            self.assertIn({'phase': 'worker', 'id': 's'}, obj.report['manual_cleanup']['sessions'])
    def test_review_requires_private_matching_marker(self):
        with tempfile.TemporaryDirectory() as folder:
            marker = Path(folder) / 'review'
            with self.assertRaises(Unverified): wait_review(marker, 'token', 0)
            marker.write_text('wrong\n'); marker.chmod(0o600)
            with self.assertRaises(CheckFailed): wait_review(marker, 'token', 0)
            marker.write_text('token\n'); wait_review(marker, 'token', 0)
            marker.chmod(0o644)
            with self.assertRaises(CheckFailed): wait_review(marker, 'token', 0)
    def test_write_requires_correlated_successful_read_first(self):
        read = {'type': 'tool_use', 'name': 'Read', 'id': 'r', 'input': {'file_path': '/guard'}}
        result = {'type': 'tool_result', 'tool_use_id': 'r', 'content': 'ORIGINAL', 'is_error': False}
        write = {'type': 'tool_use', 'name': 'Write', 'id': 'w', 'input': {'file_path': '/guard'}}
        stream = self.stream([{'type': 'assistant', 'message': {'content': [read]}},
                              {'type': 'user', 'message': {'content': [result]}},
                              {'type': 'assistant', 'message': {'content': [write]}}])
        self.assertEqual(read_then_write(stream, '/guard'), write)
        stream[2], stream[3] = stream[3], stream[2]
        with self.assertRaises(CheckFailed): read_then_write(stream, '/guard')
    def test_numeric_process_tree_includes_detached_child_group(self):
        table = '100 1 100\n101 100 101\n102 101 101\n200 1 200\n'
        with patch('os.getpgrp', return_value=999):
            self.assertEqual(descendant_groups(table, 100), {100, 101})
    def test_interruption_drains_child_in_separate_process_group(self):
        # The only real child processes in --self-test are these fixed local
        # Python fixtures. No Git, Claude, candidate, auth or network operation.
        code = '''import os, signal, subprocess, sys
from pathlib import Path
child = subprocess.Popen([sys.executable, '-I', '-S', '-c', 'import time; time.sleep(60)'], start_new_session=True)
print(child.pid, flush=True)
sys.stdin.read()
os.killpg(child.pid, signal.SIGTERM)
child.wait(timeout=3)
Path(sys.argv[1]).write_text('drained')
'''
        with tempfile.TemporaryDirectory() as folder:
            marker = Path(folder) / 'drained'
            p = _POPEN([sys.executable, '-I', '-S', '-c', code, str(marker)], cwd=folder,
                       env={'PATH': '/usr/bin:/bin', 'HOME': folder}, stdin=subprocess.PIPE,
                       stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, start_new_session=True)
            child = None
            try:
                self.assertTrue(select.select([p.stdout], [], [], 5)[0])
                child = int(p.stdout.readline())
                self.assertNotEqual(child, p.pid)
                with self.assertRaises(Interrupted):
                    try: raise Interrupted()
                    finally: drain_mcp(p)
                self.assertEqual(marker.read_text(), 'drained')
                with self.assertRaises(ProcessLookupError): os.kill(child, 0)
            finally:
                if child is not None:
                    try: os.killpg(child, signal.SIGKILL)
                    except ProcessLookupError: pass
                stop_process(p)
                for stream in (p.stdin, p.stdout): stream.close()
    def test_all_receipts_retain_effects_scope(self):
        self.assertEqual(base_receipt()['scope'], SCOPE)
        out = io.StringIO(); main(['--live'], out, io.StringIO())
        self.assertEqual(json.loads(out.getvalue())['scope'], SCOPE)

if __name__ == '__main__':
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    internal = {'--observe', '--guard', '--worker-driver', '--mcp-proxy', '--review-driver'}
    if len(sys.argv) >= 3 and sys.argv[1] in internal:
        try:
            mode, config = sys.argv[1:3]
            if mode == '--mcp-proxy': status = mcp_proxy(config)
            elif mode == '--worker-driver': status = worker_driver(config, sys.argv[3:])
            elif mode == '--review-driver': status = review_driver(config)
            else: status = hook_driver(config, 'guard' if mode == '--guard' else 'observe', sys.argv[3:])
        except BaseException:
            print('Fixture helper failed.', file=sys.stderr); status = 2
        sys.exit(status)
    sys.exit(main())
