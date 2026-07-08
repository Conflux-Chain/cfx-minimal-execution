#!/usr/bin/env bash
# Op-profile ONE 2000-epoch snapshot group of the CURRENT tree with the
# opprof.out methodology (MMPT_BULK_W=16000000, cumulative [profile] columns),
# so the output is directly comparable across commits. The instrumentation is
# in-tree (MMPT_PROFILE=1) — no source overlay. The executor runs from the given
# checkpoint to the next group boundary and exits there (--stop-after-checkpoint).
#
# Usage: scripts/run_op_profile.sh <ckpt.bin> <cfxpack-input> [label]
set -euo pipefail
cd "$(dirname "$0")/.."

CKPT="$1"; INPUT="$2"; LABEL=${3:-$(git rev-parse --short HEAD)}
OUT=data/replay-checkpoint/bench/results/opprof
LOG=$OUT/prof_$LABEL.log
CKC=$OUT/ck_$LABEL.bin
mkdir -p "$OUT"
trap 'rm -f "$CKC"' EXIT

scripts/build_replay.sh replay_executor --no-default-features --features backend-minimal-mpt

# Run on a scratch copy: the executor rewrites the checkpoint at the boundary.
cp -f "$CKPT" "$CKC"
env MMPT_PROFILE=1 MMPT_BULK_W=16000000 \
    ./replay_executor --input "$INPUT" --config data/conflux-rust-config/hydra-v303.toml \
    --checkpoint "$CKC" --checkpoint-every-groups 1 --stop-after-checkpoint \
    --max-mismatches 1000000000 --anomaly-streak 1000000000 > "$LOG" 2>&1

echo "--- $LABEL op profile ---"
awk '/\[profile\] cumulative/{blk=$0;c=1;next} c{blk=blk"\n"$0} /delta_root/{last=blk} END{print last}' "$LOG"
grep state_match "$LOG" | tail -1 || true  # absent when --stop-after-checkpoint exits before the final summary
