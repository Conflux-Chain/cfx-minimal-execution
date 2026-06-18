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
  git config submodule.oracle/conflux-rust.url https://github.com/Conflux-Chain/conflux-rust.git
  git submodule sync oracle/conflux-rust
  git submodule update --init oracle/conflux-rust
fi

if ! git -C "$ORACLE_DIR" rev-parse --verify "$ORACLE_BRANCH" >/dev/null 2>&1; then
  if git -C "$ORACLE_DIR" rev-parse --verify "origin/$ORACLE_BRANCH" >/dev/null 2>&1; then
    git -C "$ORACLE_DIR" branch "$ORACLE_BRANCH" "origin/$ORACLE_BRANCH" >/dev/null
  else
    git -C "$ORACLE_DIR" fetch origin "$ORACLE_BRANCH:$ORACLE_BRANCH"
  fi
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
