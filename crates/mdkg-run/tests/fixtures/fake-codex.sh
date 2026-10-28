#!/bin/sh
FAKE_DIR=$(pwd)
# `codex login status`: ChatGPT, unless a fake-logged-out file says not.
if [ "$1" = login ]; then
  if [ -e fake-logged-out ]; then echo "Not logged in"; exit 1; fi
  echo "Logged in using $(cat fake-codex-login 2>/dev/null || echo ChatGPT)"; exit 0
fi
# Every run's environment, for the launcher's env tests.
env >> "$FAKE_DIR/codex.environ"
# `codex --version`: a fake-version file overrides it.
if [ "$1" = --version ]; then
  echo "codex-cli $(cat fake-version 2>/dev/null || echo 0.163.0-alpha.1)"
  exit 0
fi
# `codex debug prompt-input`: a plain prompt, or one with a skill listing
# when a fake-prompt-leak file says so; records its environment.
if [ "$1" = debug ]; then
  env > "$FAKE_DIR/codex.debugenv"
  if [ -e fake-prompt-leak ]; then
    echo '[{"role":"developer","content":[{"type":"input_text","text":"<skills_instructions> x"}]}]'
  else
    echo '[{"role":"user","content":[{"type":"input_text","text":"x"}]}]'
  fi
  exit 0
fi
# `codex sandbox … -- cmd`: denies every file (prints the marker only)
# unless a fake-sandbox-leaks file says to run the command as it is.
if [ "$1" = sandbox ]; then
  printf '%s\n' "$@" > "$FAKE_DIR/codex.sandbox.args"
  while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done
  shift
  if [ -e fake-sandbox-leaks ]; then exec "$@"; fi
  echo MDKG-PREFLIGHT
  [ -e fake-sandbox-writes ] && echo "MDKG-WROTE /tmp/.mdkg-wprobe-x"
  exit 1
fi
# `codex features list`: each turned-off feature as the overrides leave it;
# a fake-feature-on file names one that stays on.
if [ "$1" = features ]; then
  printf '%s\n' "$@" > "$FAKE_DIR/codex.features.args"
  for a in "$@"; do
    case "$a" in features.*=false)
      f=${a#features.}; f=${f%=false}
      if [ "$(cat fake-feature-on 2>/dev/null)" = "$f" ]; then v=true; else v=false; fi
      [ "$(cat fake-feature-missing 2>/dev/null)" = "$f" ] && continue
      echo "$f    stable    $v" ;;
    esac
  done
  exit 0
fi
# A stand-in for `codex exec` without --json: the header on stderr, the
# final message in the -o file. It records into its working folder; a
# fake-sandbox file there overrides the sandbox it reports.
printf '%s\n' "$@" > "$FAKE_DIR/codex.args"
cat > "$FAKE_DIR/codex.prompt"
out=""
sandbox=""
approval="never"
session="0199b000-0000-7000-8000-00000000c0de"
prev=""
for a in "$@"; do
  [ "$prev" = "-o" ] && out="$a"
  [ "$prev" = "resume" ] && session="$a"
  # The header Codex prints for the sandbox mode it was given.
  [ "$prev" = "-s" ] && [ "$a" = workspace-write ] && sandbox='workspace-write [workdir, /tmp, $TMPDIR]'
  [ "$prev" = "-s" ] && [ "$a" = read-only ] && sandbox=read-only
  case "$a" in
    --approve-for-me) sandbox='workspace-write [workdir, /tmp, $TMPDIR]'; approval='on-request' ;;
    approval_policy='"on-request"') approval='on-request' ;;
    approval_policy='"never"') approval='never' ;;
    approval_policy='"untrusted"') approval='untrusted' ;;
    sandbox_mode='"workspace-write"') sandbox='workspace-write [workdir, /tmp, $TMPDIR]' ;;
    sandbox_mode='"read-only"') sandbox=read-only ;;
    sandbox_mode='"danger-full-access"') sandbox=danger-full-access ;;
  esac
  prev="$a"
done
pwd > "$FAKE_DIR/codex.cwd"
# Like Codex, keep the session's file in its home: a stand-in in its
# folder, never the user's CODEX_HOME.
mkdir -p "$FAKE_DIR/fake-codex-home/sessions/2026/10/21"
: > "$FAKE_DIR/fake-codex-home/sessions/2026/10/21/rollout-x-$session.jsonl"
{
  echo "OpenAI Codex v0.163.0 (fake)"
  echo "--------"
  echo "workdir: $(pwd)"
  echo "approval: $(cat fake-approval 2>/dev/null || echo "$approval")"
  echo "sandbox: $(cat fake-sandbox 2>/dev/null || echo "$sandbox")"
  echo "session id: $session"
  echo "--------"
  echo "user"
  echo "sandbox: danger-full-access"
} >&2
# A fake-pre file is a shell script it runs first, in its folder.
[ -f fake-pre ] && sh fake-pre
# A fake-commit file makes it edit a file and leave a commit message.
if [ -f fake-commit ]; then
  echo "changed" > change.txt
  [ -f fake-commit-env ] && { printf 'SECRET=1\n-----BEGIN PRIVATE KEY-----\n' > "$(cat fake-commit-env)"; }
  [ -f fake-commit-types ] && { mkdir -p src; echo "declare const x: 1;" > src/process.env.d.ts; }
  if [ -f fake-commit-big ]; then
    head -c 70000 /dev/zero | tr '\0' 'x' > .mdkg-commit-msg
  elif [ -f fake-commit-hardlink ]; then
    printf 'Linked\n' > linked-msg; ln -f linked-msg .mdkg-commit-msg
  elif [ -f fake-commit-link ]; then
    ln -sf "$(cat fake-commit-link)" .mdkg-commit-msg
  else
    printf 'Change a file\n\nBy the fake worker.\n' > .mdkg-commit-msg
  fi
fi
# A fake-hang file keeps it running after the header, like a long turn.
# Its own child stands in for something the agent started (a build).
if [ -f fake-hang ]; then
  sleep 30 &
  echo $! > fake-sleep.pid
  wait
fi
# The final message: in the -o file, or with no -o on stdout, as Codex
# prints it. fake-leak: it carries the key the turn bills.
reply='{"summary": "codex reply", "findings": []}'
[ -f fake-leak ] && reply="{\"summary\": \"key $CODEX_API_KEY\", \"findings\": []}"
if [ -n "$out" ]; then echo "$reply" > "$out"; else echo "$reply"; fi
