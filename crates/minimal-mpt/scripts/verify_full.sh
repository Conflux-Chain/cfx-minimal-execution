#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$CRATE_ROOT/../.." && pwd)"

cd "$WORKSPACE_ROOT"
cargo fmt --all -- --check
cargo test -p cfx-minimal-mpt --all-targets
"$CRATE_ROOT/scripts/coverage_fuzz_layered.sh"
"$CRATE_ROOT/scripts/run_oracle_compare.sh"

echo "full verification passed"
echo "fuzz coverage: $CRATE_ROOT/fuzz/coverage/layered_state_ops/lcov.info"
echo "oracle coverage: $CRATE_ROOT/target/coverage/oracle-cfx-storage/summary.txt"
