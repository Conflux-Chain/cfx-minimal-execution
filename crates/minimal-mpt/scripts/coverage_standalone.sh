#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$CRATE_ROOT/../.." && pwd)"
REPORT_DIR="$CRATE_ROOT/target/coverage/standalone"
cd "$WORKSPACE_ROOT"

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "cargo-llvm-cov is not installed. Install with: cargo install cargo-llvm-cov" >&2
  exit 2
fi

cargo llvm-cov clean --workspace
rm -rf "$REPORT_DIR"
mkdir -p "$REPORT_DIR"
cargo llvm-cov -p cfx-minimal-mpt --all-targets --ignore-filename-regex 'src/bin/trace_standalone.rs' --summary-only | tee "$REPORT_DIR/summary.txt"
cargo llvm-cov -p cfx-minimal-mpt --all-targets --ignore-filename-regex 'src/bin/trace_standalone.rs' --lcov --output-path "$REPORT_DIR/lcov.info" >/dev/null
cargo llvm-cov -p cfx-minimal-mpt --all-targets --ignore-filename-regex 'src/bin/trace_standalone.rs' --html --output-dir "$REPORT_DIR" >/dev/null
echo "coverage summary: $REPORT_DIR/summary.txt"
echo "lcov report: $REPORT_DIR/lcov.info"
echo "html report: $REPORT_DIR/html/index.html"
