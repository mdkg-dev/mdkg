#!/usr/bin/env bash
# Run every gate an agent must pass before asking for a merge (AGENTS.md).
set -euo pipefail

cd "$(dirname "$0")/.."

NODE24="${MDKG_NODE24:-/opt/homebrew/opt/node@24/bin/node}"
MDKG_TS_CLI="${MDKG_TS_CLI:-/opt/homebrew/lib/node_modules/mdkg/dist/cli.js}"

step() { printf '\n== %s\n' "$1"; }

step "cargo fmt --check"
cargo fmt --all --check

step "cargo clippy"
cargo clippy --workspace --all-targets --quiet -- -D warnings

step "cargo test"
# With the 0.6.0 CLI available, the write-command tests also check that it
# still reads every file mdkg writes to a 0.6 graph.
if [[ -x "$NODE24" && -f "$MDKG_TS_CLI" ]]; then
  export MDKG_ORACLE_NODE="$NODE24" MDKG_ORACLE_CLI="$MDKG_TS_CLI"
fi
cargo test --workspace --quiet

step "pinned workflows"
# The release workflows hold the publishing tokens: every action is pinned
# by commit, and dist is installed at a fixed version.
/usr/bin/env python3 -I scripts/pin-workflows.py --self-test
/usr/bin/env python3 -I scripts/pin-workflows.py --check

step "third-party notices"
# Every release archive, the npm package and the wheels carry
# THIRD_PARTY_NOTICES.md; it must match the locked dependencies.
/usr/bin/env python3 -I scripts/third-party-notices.py --check

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

step "all checks passed"
