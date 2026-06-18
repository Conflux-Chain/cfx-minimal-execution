#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="${1:-state_ops}"

cargo +nightly fuzz coverage "$TARGET"

if [[ -z "${LLVM_COV:-}" ]]; then
  LLVM_COV="$(rustup which --toolchain nightly llvm-cov 2>/dev/null || true)"
fi
if [[ -z "$LLVM_COV" ]]; then
  HOST="$(rustc -vV | sed -n 's/^host: //p')"
  LLVM_COV="$HOME/.rustup/toolchains/nightly-$HOST/lib/rustlib/$HOST/bin/llvm-cov"
fi
BINARY="$ROOT/target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/$TARGET"
PROFILE="$ROOT/fuzz/coverage/$TARGET/coverage.profdata"
REPORT_DIR="$ROOT/target/coverage/fuzz-$TARGET"
IGNORE='(^|/)(rustc|\.cargo|fuzz/fuzz_targets|target)/'

mkdir -p "$REPORT_DIR/html"

"$LLVM_COV" report "$BINARY" \
  -instr-profile="$PROFILE" \
  -ignore-filename-regex="$IGNORE" \
  > "$REPORT_DIR/summary.txt"

"$LLVM_COV" export "$BINARY" \
  -instr-profile="$PROFILE" \
  -format=lcov \
  -ignore-filename-regex="$IGNORE" \
  > "$REPORT_DIR/lcov.info"

"$LLVM_COV" show "$BINARY" \
  -instr-profile="$PROFILE" \
  -format=html \
  -output-dir="$REPORT_DIR/html" \
  -ignore-filename-regex="$IGNORE" \
  >/dev/null

cat "$REPORT_DIR/summary.txt"
echo
echo "fuzz-only coverage reports:"
echo "  summary: $REPORT_DIR/summary.txt"
echo "  lcov:    $REPORT_DIR/lcov.info"
echo "  html:    $REPORT_DIR/html/index.html"
