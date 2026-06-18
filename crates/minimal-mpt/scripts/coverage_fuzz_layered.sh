#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo +nightly fuzz run layered_state_ops -- -runs="${FUZZ_RUNS:-50000}"
cargo +nightly fuzz coverage layered_state_ops

LLVM_COV="$(rustc +nightly --print sysroot)/lib/rustlib/$(rustc +nightly -vV | awk '/host:/ {print $2}')/bin/llvm-cov"
"$LLVM_COV" export \
  --format=lcov \
  --instr-profile=fuzz/coverage/layered_state_ops/coverage.profdata \
  target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/layered_state_ops \
  > fuzz/coverage/layered_state_ops/lcov.info

scripts/check_fuzz_coverage.py

echo "fuzz coverage: $ROOT/fuzz/coverage/layered_state_ops/lcov.info"
