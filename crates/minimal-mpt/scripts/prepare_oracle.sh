#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$CRATE_ROOT/../.." && pwd)"
ORACLE_DIR="$WORKSPACE_ROOT/oracle/conflux-rust"
ORACLE_BRANCH="minimal-execution-oracle"
ORACLE_TEST="$ORACLE_DIR/crates/dbs/storage/src/tests/minimal_mpt_oracle_compare.rs"
MOD_FILE="$ORACLE_DIR/crates/dbs/storage/src/tests/mod.rs"

if [[ ! -e "$ORACLE_DIR/.git" ]]; then
  cd "$WORKSPACE_ROOT"
  git submodule update --init oracle/conflux-rust
fi

if ! git -C "$ORACLE_DIR" rev-parse --verify "$ORACLE_BRANCH" >/dev/null 2>&1; then
  echo "missing oracle branch: $ORACLE_BRANCH" >&2
  exit 1
fi

git -C "$ORACLE_DIR" checkout -q "$ORACLE_BRANCH"

if [[ ! -f "$ORACLE_TEST" ]]; then
  echo "missing oracle test: $ORACLE_TEST" >&2
  exit 1
fi
if ! grep -q "mod minimal_mpt_oracle_compare;" "$MOD_FILE"; then
  echo "oracle test is not registered in $MOD_FILE" >&2
  exit 1
fi

actual="$(git -C "$ORACLE_DIR" rev-parse HEAD)"
echo "oracle ready: $ORACLE_DIR @ $actual"
