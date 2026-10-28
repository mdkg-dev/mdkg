#!/bin/sh
FAKE_DIR=$(pwd)
# `claude auth status --json`: logged in to claude.ai unless a
# fake-logged-out file says otherwise.
if [ "$1" = auth ]; then
  # The login check's environment, for the launcher's env tests.
  env > "$FAKE_DIR/claude-auth.environ"
  if [ -e fake-logged-out ]; then echo '{"loggedIn": false}'; else echo "{\"loggedIn\": true, \"authMethod\": \"$(cat fake-auth-method 2>/dev/null || echo claude.ai)\"}"; fi
  exit 0
fi
# A stand-in for `claude -p --output-format stream-json`: records its
# arguments and prompt, then prints an init event and a result.
# It records into its working folder, and reads fake-mode there:
# ok | error | crash.
printf '%s\n' "$@" > "$FAKE_DIR/claude.args"
# The environment it was started with, for the launcher's env tests.
env > "$FAKE_DIR/claude.environ"
# Whether the launcher recorded this turn's mark while it runs, for the
# hooks' launcher-turn proof.
common=$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null)
[ -n "$MDKG_RUN_TURN" ] && [ -f "$common/mdkg/hooks/turns/$MDKG_RUN_TURN.json" ] && touch "$FAKE_DIR/claude.mark.environ"
cat > "$FAKE_DIR/claude.prompt"
session=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--session-id" ] || [ "$prev" = "--resume" ]; then session="$a"; fi
  prev="$a"
done
# A fake-commit file makes it edit a file and leave a commit message.
if [ -f fake-commit ]; then
  echo "changed" > change.txt
  printf 'Change a file\n\nBy the fake worker.\n' > .mdkg-commit-msg
fi
# A fake-linger file makes it leave a process behind, as a `nohup … &`
# from the agent's shell would: in a session of its own, as Claude's and
# Codex's tool commands run. The file names the command (one that keeps
# writing the folder), which inherits the agent's environment.
if [ -s fake-linger ]; then
  # shellcheck disable=SC2046
  nohup perl -e 'use POSIX; POSIX::setsid(); exec @ARGV' $(cat fake-linger) >/dev/null 2>&1 &
  echo $! > fake-linger.pid
fi
echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$session\"}"
# fake-mode limit: the subscription's usage limit, unless the turn bills a
# key through apiKeyHelper (it runs the helper, keeping only the key's
# length, never the key).
if [ "$(cat fake-mode 2>/dev/null)" = limit ]; then
  helper=$(printf '%s\n' "$@" | sed -n 's/.*"apiKeyHelper":"\(.*\)".*/\1/p')
  if [ -z "$helper" ]; then
    echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"Claude AI usage limit reached|1792900000\",\"session_id\":\"$session\"}"
    exit 1
  fi
  key=$(sh -c "$helper")
  printf '%s' "$key" | wc -c | tr -d ' ' > "$FAKE_DIR/fake-key-length"
  # fake-leak: the agent prints the key, split across two writes, and puts
  # it in its reply.
  if [ -e fake-leak ]; then
    half=$(( ${#key} / 2 ))
    first=$(printf '%s' "$key" | cut -c1-$half)
    rest=$(printf '%s' "$key" | cut -c$((half + 1))-)
    printf 'leaked %s' "$first" >&2; sleep 0.3; printf '%s\n' "$rest" >&2
    echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"the key is $key\",\"session_id\":\"$session\"}"
    exit 0
  fi
fi
[ "$(cat fake-mode 2>/dev/null)" = slow ] && sleep 1
case "$(cat fake-mode 2>/dev/null || echo ok)" in
  error) echo "{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"session_id\":\"$session\"}" ;;
  crash) echo "boom" >&2; exit 3 ;;
  *) echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"claude reply\",\"session_id\":\"$session\"}" ;;
esac
