#!/usr/bin/env bash
# Check-only: the oracle submodule is initialized and its HEAD contains the
# oracle compare test. Never mutates the submodule.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$CRATE_ROOT/../.." && pwd)"
ORACLE_DIR="$WORKSPACE_ROOT/oracle/conflux-rust"
# "Add minimal MPT oracle comparison test" (2026-06-18): introduced
# minimal_mpt_oracle_compare.rs. Any HEAD descending from it will do.
ORACLE_TEST_COMMIT=817bed88afb11161824b7ec93876c76a38250db8

if [[ ! -e "$ORACLE_DIR/.git" ]]; then
  echo "oracle submodule not initialized: $ORACLE_DIR" >&2
  echo "fix: git submodule update --init oracle/conflux-rust" >&2
  exit 1
fi

if ! git -C "$ORACLE_DIR" merge-base --is-ancestor "$ORACLE_TEST_COMMIT" HEAD; then
  echo "oracle HEAD does not contain the compare test (commit $ORACLE_TEST_COMMIT, \"Add minimal MPT oracle comparison test\")" >&2
  echo "fix: check out any oracle rev descending from that commit" >&2
  exit 1
fi

echo "oracle ready: $ORACLE_DIR @ $(git -C "$ORACLE_DIR" rev-parse HEAD)"
