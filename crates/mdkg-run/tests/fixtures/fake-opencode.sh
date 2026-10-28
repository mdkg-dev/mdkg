#!/bin/sh
FAKE_DIR=$(pwd)
# A stand-in for `opencode run --format json`: JSON events with a session id
# and text parts. It records into its working folder.
#
# Before a worker turn, mdkg asks OpenCode for its version, starts a private
# `serve --stdio` and asks its API for the resolved agents. The fake answers
# 2.1.0 with build enabled, unless FAKE_OPENCODE_VERSION names another
# version, FAKE_OPENCODE_PREFLIGHT=fail makes the server exit, or the
# user's config under $XDG_CONFIG_HOME (or $HOME/.config) disables build.
# It stands in for OpenCode's own resolution; mdkg never reads that file.
case "$1" in
--version)
  echo "opencode v${FAKE_OPENCODE_VERSION:-2.1.0}" >> "$FAKE_DIR/fake-opencode-preflight"
  echo "opencode v${FAKE_OPENCODE_VERSION:-2.1.0}"
  exit 0
  ;;
serve)
  echo serve >> "$FAKE_DIR/fake-opencode-preflight"
  [ "${FAKE_OPENCODE_PREFLIGHT:-}" != fail ] || exit 1
  echo $$ > "$FAKE_DIR/fake-opencode-serve.pid"
  echo '{"url":"http://127.0.0.1:4096"}'
  while read -r line; do :; done
  exit 0
  ;;
api)
  echo "$4" >> "$FAKE_DIR/fake-opencode-preflight"
  case "$4" in
  server.info)
    printf '{"version":"%s","pid":%s}\n' "${FAKE_OPENCODE_VERSION:-2.1.0}" "$(cat "$FAKE_DIR/fake-opencode-serve.pid")"
    exit 0
    ;;
  model.list) data='[]';;
  plugin.list)
    data='[{"id":"opencode.agent","source":{"type":"builtin"},"state":{"status":"active"}},{"id":"opencode.config.agent","source":{"type":"builtin"},"state":{"status":"active"}}]'
    ;;
  agent.list)
    data='[{"id":"build","mode":"primary","permissions":[]}]'
    user="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/opencode.json"
    if tr -d ' \n' < "$user" 2>/dev/null | grep -q '"build":{"disable":true'; then
      data='[]'
    fi
    ;;
  *) exit 95;;
  esac
  printf '{"location":{"directory":"%s"},"data":%s}\n' "$(pwd -P)" "$data"
  exit 0
  ;;
esac
printf '%s\n' "$@" > "$FAKE_DIR/opencode.args"
printf '%s' "$OPENCODE_CONFIG_CONTENT" > "$FAKE_DIR/opencode.config"
# The environment it was started with, for the launcher's env tests.
env > "$FAKE_DIR/opencode.environ"
# Like OpenCode, resume only a session its data folder holds.
prev=""
for a in "$@"; do
  if [ "$prev" = "--session" ] && ! grep -qx -- "$a" "$FAKE_DIR/fake-opencode-sessions" 2>/dev/null; then
    echo "no session $a" >&2; exit 1
  fi
  prev="$a"
done
echo ses_fake >> "$FAKE_DIR/fake-opencode-sessions"
if [ "$(cat fake-mode 2>/dev/null)" = limit ]; then
  echo '{"type":"error","error":{"name":"APIError","data":{"message":"usage_limit_reached"}}}'
  exit 1
fi
if [ -f fake-commit ]; then
  echo "changed" > change.txt
  printf 'Change a file\n' > .mdkg-commit-msg
fi
echo '{"type":"step_start","sessionID":"ses_fake","part":{}}'
echo '{"type":"text","sessionID":"ses_fake","part":{"text":"opencode reply"}}'
