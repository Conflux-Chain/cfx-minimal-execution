#!/bin/bash
# Unified verification entry for cfx-minimal-mpt.
# Usage: crates/minimal-mpt/scripts/verify.sh [fast|full|coverage]
#
#   fast     — anti-regression gate, target <60s: crate fmt check, -Dwarnings
#              cargo check (all targets), all-target tests with
#              verify-incremental (root cross-check vs stateless oracle),
#              corpus replay of 3 fuzz targets (-runs=0, no mutation; the
#              large_trie_ops corpus is 63 heavyweight inputs ≈ 140s, so
#              only full replays it) + 10s mutation fuzz of layered_state_ops
#   full     — fast + executor compile check (oracle/conflux-rust consumes
#              cfx-minimal-mpt by path; a top-level build won't catch breaking
#              its API) + 30s fuzz smoke on 3 targets + 50k-run layered fuzz
#              + oracle differential compare (plain cargo test)
#   coverage — exactly full, plus the two coverage gates at the end: layered
#              corpus coverage replay + line gate, oracle compare under
#              llvm-cov + line gate. The gates are line-number-pinned and
#              brittle — that is why fast/full skip them.
#
# Default tier: full.
set -euo pipefail
cd "$(dirname "$0")/../../.."   # workspace root

TIER="${1:-full}"
case "$TIER" in
    fast|full|coverage) ;;
    *) echo "usage: $0 [fast|full|coverage]" >&2; exit 2 ;;
esac
mkdir -p target/verify-logs

echo "==== fmt check (crate only) ===="
cargo fmt -p cfx-minimal-mpt -- --check

echo "==== check (release, -Dwarnings, all targets) ===="
RUSTFLAGS="-Dwarnings" cargo check --release -p cfx-minimal-mpt --all-targets

echo "==== tests (all targets, verify-incremental) ===="
cargo test --release -p cfx-minimal-mpt --all-targets --features verify-incremental

for t in state_ops incremental_trie layered_state_ops; do
    echo "==== fuzz $t (corpus replay) ===="
    ( cd crates/minimal-mpt &&
      cargo +nightly fuzz run "$t" -- -runs=0 ) \
        > "target/verify-logs/fuzz_replay_$t.log" 2>&1 \
        || { tail -40 "target/verify-logs/fuzz_replay_$t.log"; exit 1; }
done

echo "==== fuzz layered_state_ops (10s smoke) ===="
( cd crates/minimal-mpt &&
  cargo +nightly fuzz run layered_state_ops -- -max_total_time=10 ) \
    > target/verify-logs/fuzz_fast_layered.log 2>&1 \
    || { tail -40 target/verify-logs/fuzz_fast_layered.log; exit 1; }

if [[ "$TIER" == fast ]]; then
    echo "VERIFY(fast) PASSED commit=$(git rev-parse --short HEAD)"
    exit 0
fi

echo "==== executor compile check (conflux-rust, backend-minimal-mpt) ===="
crates/minimal-mpt/scripts/prepare_oracle.sh
( cd oracle/conflux-rust/crates/replay-data-executor &&
  cargo build --release --no-default-features --features backend-minimal-mpt --bin cfx-replay-exec )

for t in state_ops large_trie_ops incremental_trie; do
    echo "==== fuzz $t (30s smoke) ===="
    ( cd crates/minimal-mpt &&
      cargo +nightly fuzz run "$t" -- -max_total_time=30 ) \
        > "target/verify-logs/fuzz_$t.log" 2>&1 \
        || { tail -40 "target/verify-logs/fuzz_$t.log"; exit 1; }
done

echo "==== fuzz layered_state_ops (${FUZZ_RUNS:-50000} runs) ===="
( cd crates/minimal-mpt &&
  cargo +nightly fuzz run layered_state_ops -- -runs="${FUZZ_RUNS:-50000}" ) \
    > target/verify-logs/fuzz_layered_state_ops.log 2>&1 \
    || { tail -40 target/verify-logs/fuzz_layered_state_ops.log; exit 1; }

echo "==== oracle differential compare (plain) ===="
EXPECTED="$PWD/crates/minimal-mpt/target/oracle_expected_trace.txt"
mkdir -p "$(dirname "$EXPECTED")"
cargo run --release --quiet -p cfx-minimal-mpt --bin trace_standalone > "$EXPECTED"
( cd oracle/conflux-rust &&
  CFX_MINIMAL_MPT_EXPECTED="$EXPECTED" \
  cargo test --release -p cfx-storage \
    minimal_mpt_oracle_compare_get_set_delete_commit \
    --features testonly_code )

if [[ "$TIER" == full ]]; then
    echo "VERIFY(full) PASSED commit=$(git rev-parse --short HEAD)"
    exit 0
fi

echo "==== layered fuzz coverage + gate ===="
crates/minimal-mpt/scripts/coverage_fuzz_state_ops.sh layered_state_ops
crates/minimal-mpt/scripts/check_coverage.py fuzz

echo "==== oracle compare under llvm-cov + coverage gate ===="
crates/minimal-mpt/scripts/coverage_oracle_cfx_storage.sh

echo "VERIFY(coverage) PASSED commit=$(git rev-parse --short HEAD)"
