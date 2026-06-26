#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$CRATE_ROOT/../.." && pwd)"
ORACLE_DIR="$WORKSPACE_ROOT/oracle/conflux-rust"
EXPECTED="$CRATE_ROOT/target/oracle_expected_trace.txt"
REPORT_DIR="$CRATE_ROOT/target/coverage/oracle-cfx-storage"

"$CRATE_ROOT/scripts/prepare_oracle.sh"

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "cargo-llvm-cov is not installed. Install with: cargo install cargo-llvm-cov" >&2
  exit 2
fi

mkdir -p "$(dirname "$EXPECTED")"
cd "$WORKSPACE_ROOT"
cargo run --release --quiet -p cfx-minimal-mpt --bin trace_standalone > "$EXPECTED"

cd "$ORACLE_DIR"
cargo llvm-cov clean --workspace
rm -rf "$REPORT_DIR"
mkdir -p "$REPORT_DIR"
CFX_MINIMAL_MPT_EXPECTED="$EXPECTED" \
  cargo llvm-cov test --release -p cfx-storage \
  minimal_mpt_oracle_compare_get_set_delete_commit \
  --features testonly_code --summary-only | tee "$REPORT_DIR/summary.txt"
CFX_MINIMAL_MPT_EXPECTED="$EXPECTED" \
  cargo llvm-cov test --release -p cfx-storage \
  minimal_mpt_oracle_compare_get_set_delete_commit \
  --features testonly_code --lcov --output-path "$REPORT_DIR/lcov.info" >/dev/null
cd "$CRATE_ROOT"
"$CRATE_ROOT/scripts/check_oracle_coverage.py"
cd "$ORACLE_DIR"
CFX_MINIMAL_MPT_EXPECTED="$EXPECTED" \
  cargo llvm-cov test --release -p cfx-storage \
  minimal_mpt_oracle_compare_get_set_delete_commit \
  --features testonly_code --html --output-dir "$REPORT_DIR" >/dev/null
echo "coverage summary: $REPORT_DIR/summary.txt"
echo "lcov report: $REPORT_DIR/lcov.info"
echo "html report: $REPORT_DIR/html/index.html"
