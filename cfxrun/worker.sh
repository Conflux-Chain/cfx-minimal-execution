#!/usr/bin/env bash
# 在 worker 上运行：下载 →（必要时）label_blame → 按 24h 窗口重放到 END。
# 由 cfxrun.py 上传并以 nohup 启动；重复启动安全，已有 height/ckpt.bin 则从中恢复。
#
# 完整路径：
#   1) 执行器必须是 build_replay.sh（v3.0.3 merge）产物，禁止 oracle HEAD 裸编
#   2) 未打标 .cfxpack 先 label_blame 成 .cfxpackb，再 --anomaly-streak 0
#   3) 墙钟每 WINDOW_SECONDS（默认 24h）在 2000-epoch 边界写 ckpt；
#      非段尾的中间 ckpt 压缩进 archives/，由主控 multipart 上传到
#      s3://conflux-replay-txs/checkpoints/<run>/<seg>/
#   段尾 ckpt 只留本地，不上传（输入侧已有下一段起点）。
set -uo pipefail
cd "$(dirname "$0")"
source seg.env # SEG START END WINDOW_SECONDS ANOMALY_STREAK
WINDOW_SECONDS=${WINDOW_SECONDS:-86400}
ANOMALY_STREAK=${ANOMALY_STREAK:-0}
LABEL_WINDOW=${LABEL_WINDOW:-10000}
echo $$ > worker.pid
trap 'rm -f worker.pid' EXIT

log() { echo "[$(date '+%F %T')] $*"; }
st()  { echo "$*" > status; log "status=$*"; }

last_height() {
  local h
  h=$(grep -o 'stop-after-checkpoint: wrote checkpoint at height [0-9]*' run.log 2>/dev/null | tail -1 | grep -o '[0-9]*$')
  if [ -z "$h" ]; then
    h=$(grep -o 'wrote checkpoint [^ ]* at height [0-9]*' run.log 2>/dev/null | tail -1 | grep -o '[0-9]*$')
  fi
  if [ -z "$h" ]; then
    h=$(grep 'replay progress ' run.log 2>/dev/null | tail -1 | grep -o 'height=[0-9]*' | grep -o '[0-9]*$')
  fi
  echo "${h:-$START}"
}

archive_checkpoint() {
  local h="$1"
  local file="ckpt_${h}.bin.zst"
  st "ARCHIVING $h"
  log "压缩 24h 边界 checkpoint → archives/$file"
  zstd -T0 -3 -q -f ckpt.bin -o "archives/$file" || {
    st "FAILED archive height=$h"
    return 1
  }
  local size sha tmp
  size=$(stat -c %s "archives/$file")
  sha=$(sha256sum "archives/$file" | awk '{print $1}')
  tmp="archives/manifest.tsv.tmp"
  awk -F '\t' -v h="$h" '$1 != h' archives/manifest.tsv 2>/dev/null > "$tmp" || true
  printf '%s\t%s\t%s\t%s\n' "$h" "$size" "$sha" "$file" >> "$tmp"
  mv "$tmp" archives/manifest.tsv
  rm -f pending_archive
  log "已归档 archives/$file size=$size sha256=$sha"
}

if [ ! -f .downloaded ]; then
  st DOWNLOADING
  command -v zstd >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq zstd; }
  mkdir -p input archives
  n=$(wc -l < urls.txt)
  log "开始下载 $n 个文件"
  i=0
  while read -r path url; do
    i=$((i + 1))
    st "DOWNLOADING $i/$n"
    curl -sfS --retry 5 --retry-delay 2 -o "$path" "$url" || { st FAILED "download $path"; exit 1; }
  done < urls.txt
  if [ -f ckpt.bin.zst ]; then
    st DECOMPRESSING
    log "解压 checkpoint $(du -h ckpt.bin.zst | awk '{print $1}')"
    zstd -dq --rm -f ckpt.bin.zst -o ckpt.bin || { st FAILED decompress; exit 1; }
  fi
  touch .downloaded
  log "下载完成"
fi
mkdir -p archives

# 未打标的 .cfxpack 在本机标成 .cfxpackb（FLAG_BLAMED），然后 streak=0。
# S3 上已是 cfxpackb 则跳过。缺 label_blame 二进制视为路径不完整，直接失败。
unlabelled=$(find input -maxdepth 1 \( -name '*.cfxpack' -o -name '*.cfxpack.zst' \) ! -name '*.cfxpackb*' 2>/dev/null | wc -l)
if [ "$unlabelled" -gt 0 ]; then
  if [ ! -x ./label_blame ]; then
    st "FAILED no label_blame binary (unlabelled cfxpack on disk)"
    exit 1
  fi
  st LABELLING
  log "label_blame --input-dir input --window $LABEL_WINDOW"
  ./label_blame --input-dir input --window "$LABEL_WINDOW" >> worker.out 2>&1 \
    || { st FAILED label_blame; tail -30 worker.out; exit 1; }
fi
n_b=$(find input -maxdepth 1 -name '*.cfxpackb' 2>/dev/null | wc -l)
n_raw=$(find input -maxdepth 1 \( -name '*.cfxpack' -o -name '*.cfxpack.zst' \) 2>/dev/null | wc -l)
if [ "$n_b" -gt 0 ] && [ "$n_raw" -eq 0 ]; then
  ANOMALY_STREAK=0
  log "packs labelled → anomaly_streak=0"
fi

cur=$(cat height 2>/dev/null || echo "$START")
# 若上次在压缩阶段中断，ckpt.bin 已经前进；必须先补齐归档再继续执行。
if [ -f pending_archive ]; then
  pending=$(cat pending_archive)
  archive_checkpoint "$pending" || exit 1
fi

log "重放 $cur → $END，每 ${WINDOW_SECONDS}s（24h 边界）写中间 ckpt 并归档待上传"
crashes=0
while [ "$cur" -lt "$END" ]; do
  left=$(( (END - cur) / 2000 ))
  [ "$left" -lt 1 ] && left=1
  pct=$(( (cur - START) * 100 / (END - START) ))
  st "RUNNING $cur ${pct}%"
  log "窗口起点=$cur remaining_groups=$left 进度=${pct}%"
  echo "=== window from $cur, groups=$left, window_seconds=$WINDOW_SECONDS, $(date -Is)" >> run.log
  ./cfx-replay-exec --input input --config hydra.toml --checkpoint ckpt.bin \
    --checkpoint-every-groups "$left" --checkpoint-every-seconds "$WINDOW_SECONDS" \
    --stop-after-checkpoint --anomaly-streak "$ANOMALY_STREAK" \
    >> run.log 2>&1
  rc=$?
  h=$(grep -o 'stop-after-checkpoint: wrote checkpoint at height [0-9]*' run.log | tail -1 | grep -o '[0-9]*$')
  if [ -z "$h" ]; then
    h=$(last_height)
  fi
  if [ "$rc" -eq 0 ] && [ -n "$h" ] && [ "$h" -gt "$cur" ]; then
    cur=$h
    echo "$cur" > height
    crashes=0
    if [ "$cur" -lt "$END" ]; then
      echo "$cur" > pending_archive
      archive_checkpoint "$cur" || exit 1
    fi
    log "窗口完成 → 高度=$cur"
  elif [ "$rc" -eq 0 ]; then
    st "FAILED stopped_at=$cur rc=0"
    log "FAILED：正常退出但未写出更高的 checkpoint，见 run.log 末尾"
    tail -30 run.log >> worker.out
    exit 1
  else
    if grep -q 'replay anomaly:' run.log; then
      st "FAILED anomaly at=$cur rc=$rc"
      log "FAILED：连续 mismatch 超阈值，见 run.log 末尾"
      tail -30 run.log >> worker.out
      exit 1
    fi
    crashes=$((crashes + 1))
    log "进程异常退出 rc=$rc（第 $crashes 次）at=$cur"
    [ "$crashes" -ge 2 ] && { st "FAILED crash rc=$rc at=$cur"; tail -30 run.log >> worker.out; exit 1; }
    log "将从高度 $cur 重试一次"
  fi
done

mismatches=$(grep -c '^epoch height=' run.log || true)
if [ -f archives/manifest.tsv ]; then
  archives=$(wc -l < archives/manifest.tsv)
else
  archives=0
fi
if [ "$mismatches" -eq 0 ]; then
  if [ "$archives" -gt 0 ]; then
    st "DONE_PENDING_UPLOAD $cur $archives"
  else
    st "DONE $cur"
  fi
else
  st "DONE_WITH_MISMATCH $cur $mismatches PENDING_UPLOAD $archives"
  log "完成但有 $mismatches 条孤立不匹配记录（未超 anomaly_streak）"
fi
log "段完成；待上传中间 checkpoint: $archives 个"
