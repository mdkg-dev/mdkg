#!/bin/sh
# A stand-in for the mdkg CLI's mail commands (the M1 contract): logs
# its arguments to fake.log in the --root folder and prints canned JSON
# envelopes. A fake-wait file there makes `msg wait` time out (exit 6); a
# fake-claim file makes `claim` find the lease held (exit 5).
root="$2"
printf '%s\n' "$*" >> "$root/fake.log"
printf '%s\n' "${MDKG_TRUSTED_COMMON_DIR:-unset}" >> "$root/trusted.log"
printf '%s\n' "${MDKG_TRUSTED_WORKTREE:-unset}" >> "$root/trusted-worktree.log"
cmd=""
for a in "$@"; do
  case "$a" in msg|claim|release) [ -z "$cmd" ] && cmd="$a" ;; inbox|wait|import|ack) [ "$cmd" = msg ] && cmd="msg $a" ;; esac
done
case "$cmd" in
  "msg inbox")
    echo '{"schema":"mdkg.msg.inbox.v1","ok":true,"data":{"mailboxes":["a"],"messages":[{"messageId":"m1","seq":5,"kind":"note","sender":"b","parts":[{"kind":"text","text":"hi"}]}],"cursor":5,"more":false},"diagnostics":[]}' ;;
  "msg wait")
    if [ -f "$root/fake-wait" ]; then
      echo '{"schema":"mdkg.msg.wait.v1","ok":false,"diagnostics":[{"code":"msg.wait_timeout","severity":"error","message":"no mail"}]}'
      exit 6
    fi
    echo '{"schema":"mdkg.msg.wait.v1","ok":true,"data":{"messages":[],"cursor":0,"more":false},"diagnostics":[]}' ;;
  "msg import")
    echo '{"schema":"mdkg.msg.import.v1","ok":true,"data":{"message":{"messageId":"import:abc","seq":6,"kind":"note","sender":"a"},"duplicate":false},"diagnostics":[]}' ;;
  "msg ack")
    echo '{"schema":"mdkg.msg.ack.v1","ok":true,"data":{"acked":1,"purged":1},"diagnostics":[]}' ;;
  claim)
    if [ -f "$root/fake-claim" ]; then
      echo '{"schema":"mdkg.claim.v1","ok":false,"diagnostics":[{"code":"coord.lease_held","severity":"error","message":"held","hint":"pick other work"}]}'
      exit 5
    fi
    echo '{"schema":"mdkg.claim.v1","ok":true,"data":{},"diagnostics":[]}' ;;
  release)
    echo '{"schema":"mdkg.release.v1","ok":true,"data":{},"diagnostics":[]}' ;;
  *) echo "not json"; exit 1 ;;
esac
