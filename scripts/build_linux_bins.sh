#!/usr/bin/env bash
# 必须在 Linux x86_64 上跑。一次编出完整重放路径需要的两个二进制：
#   <out>/cfx-replay-exec  — TAG=v3.0.3 ./scripts/build_replay.sh（禁止裸编 oracle HEAD）
#   <out>/label_blame      — 顶层 workspace，给 .cfxpack 打 FLAG_BLAMED → .cfxpackb
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$REPO_ROOT/bin}"
mkdir -p "$OUT"

TAG="${TAG:-v3.0.3}" "$REPO_ROOT/scripts/build_replay.sh" \
  "$OUT/cfx-replay-exec" --no-default-features --features backend-minimal-mpt

( cd "$REPO_ROOT" && cargo build --release -p cfxpack --bin label_blame --features label-blame )
cp -f "$REPO_ROOT/target/release/label_blame" "$OUT/label_blame"
echo "$OUT/cfx-replay-exec"
echo "$OUT/label_blame"
