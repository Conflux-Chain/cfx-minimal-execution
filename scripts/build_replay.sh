#!/usr/bin/env bash
# Linux x86_64 执行器：把 oracle HEAD merge 进 TAG（默认 v3.0.3）再编。
# 不要在 oracle/conflux-rust 工作区里直接 cargo build —— 那是裸 HEAD，
# PoS 段会从第一个新执行的 epoch 起 state 全错。
#
# 完整重放路径见 cfxrun/README.md：
#   1) 本脚本出 cfx-replay-exec
#   2) 顶层 workspace 编 label_blame（scripts/build_linux_bins.sh 一次做完）
#   3) cfxrun up 把两个二进制拷到 worker；未打标的 .cfxpack 先 label_blame 成 .cfxpackb
#   4) 打标成功则 --anomaly-streak 0
#   5) 墙钟 ≥24h 的段归档中间 ckpt → s3://conflux-replay-txs/checkpoints/<run>/
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ORACLE="$REPO_ROOT/oracle/conflux-rust"
TAG="${TAG:-v3.0.3}"
PROD="$REPO_ROOT/oracle/merge-only-conflux-$TAG"

if [[ $# -lt 1 ]]; then
  echo "usage: TAG=v3.0.3 $0 <output-path> [cargo flags...]  # e.g. --no-default-features --features backend-minimal-mpt" >&2
  exit 2
fi

OUTPUT="$1"; shift

if ! git -C "$ORACLE" diff --quiet || ! git -C "$ORACLE" diff --cached --quiet; then
  echo "oracle/conflux-rust has uncommitted changes, aborting" >&2
  exit 1
fi
ORACLE_HEAD="$(git -C "$ORACLE" rev-parse HEAD)"

mkdir -p $PROD

if [[ ! -e "$PROD/.git" ]]; then
  git -C "$ORACLE" worktree prune
  git -C "$ORACLE" worktree add --detach "$PROD" "$ORACLE_HEAD"
fi

# Merge oracle HEAD with TAG; auto-resolve conflicts that are outside the
# replay execution path (checkout ours). Cargo.lock gets ours + cargo check
# to incrementally add missing entries without re-resolving existing versions.
git -C "$PROD" merge --abort 2>/dev/null || true
git -C "$PROD" checkout --detach --force "$ORACLE_HEAD"
if ! git -C "$PROD" merge --no-edit "$TAG"; then
  git -C "$PROD" diff --name-only --diff-filter=U | while read -r f; do
    git -C "$PROD" checkout --ours "$f"
    git -C "$PROD" add "$f"
  done
  ( cd "$PROD" && cargo check --release -p cfx-replay-data-executor )
  git -C "$PROD" add Cargo.lock
  git -C "$PROD" -c core.editor=true merge --continue
fi

( cd "$PROD/crates/replay-data-executor" &&
  cargo build --release "$@" --bin cfx-replay-exec )

mkdir -p "$(dirname "$OUTPUT")"
cp -f "$PROD/target/release/cfx-replay-exec" "$OUTPUT"
echo "$OUTPUT"
