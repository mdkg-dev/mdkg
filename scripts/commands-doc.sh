#!/usr/bin/env bash
# Regenerates docs/commands.md from the CLI's own --help. The test
# crates/mdkg-cli/tests/commands_doc.rs fails when the file is stale.
set -euo pipefail
cd "$(dirname "$0")/.."
MDKG_WRITE_COMMANDS_DOC=1 cargo test --quiet -p mdkg --test commands_doc
echo "wrote docs/commands.md"
