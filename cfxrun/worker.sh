#!/usr/bin/env bash
# 在 worker 上运行：下载本段数据，按时间窗口重放到 END，结论写进 ./status。
# 由 cfxrun.py 上传并以 nohup 启动；重复启动是安全的，会从 ./height 记录的高度继续。
# 非段尾 checkpoint 会压缩并记入 manifest，供主控上传到持久化 S3。
set -uo pipefail
cd "$(dirname "$0")"
source seg.env # SEG START END CHUNK_GROUPS WINDOW_SECONDS
WINDOW_SECONDS=${WINDOW_SECONDS:-72000}
echo $$ > worker.pid
trap 'rm -f worker.pid' EXIT

log() { echo "[$(date '+%F %T')] $*"; }
st()  { echo "$*" > status; log "status=$*"; }

archive_checkpoint() {
  local h="$1"
  local file="ckpt_${h}.bin.zst"
  st "ARCHIVING $h"
  log "压缩一天边界 checkpoint → archives/$file"
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

cur=$(cat height 2>/dev/null || echo "$START")
# 若上次在压缩阶段中断，ckpt.bin 已经前进；必须先补齐归档再继续执行。
if [ -f pending_archive ]; then
  pending=$(cat pending_archive)
  archive_checkpoint "$pending" || exit 1
fi

crashes=0
while [ "$cur" -lt "$END" ]; do
  left=$(( (END - cur) / 2000 ))
  k=$(( left < CHUNK_GROUPS ? left : CHUNK_GROUPS ))
  pct=$(( (cur - START) * 100 / (END - START) ))
  st "RUNNING $cur ${pct}%"
  log "chunk 起点=$cur groups=$k 进度=${pct}%"
  echo "=== chunk from $cur, groups=$k, $(date -Is)" >> run.log
  ./cfx-replay-exec --input input --config hydra.toml --checkpoint ckpt.bin \
    --checkpoint-every-groups "$k" --checkpoint-every-seconds "$WINDOW_SECONDS" \
    --stop-after-checkpoint >> run.log 2>&1
  rc=$?
  h=$(grep -o 'stop-after-checkpoint: wrote checkpoint at height [0-9]*' run.log | tail -1 | grep -o '[0-9]*$')
  if [ "$rc" -eq 0 ] && [ -n "$h" ] && [ "$h" -gt "$cur" ]; then
    cur=$h
    echo "$cur" > height
    crashes=0
    if [ "$cur" -lt "$END" ]; then
      echo "$cur" > pending_archive
      archive_checkpoint "$cur" || exit 1
    fi
    log "chunk 完成 → 高度=$cur"
  elif [ "$rc" -eq 0 ]; then
    st "FAILED stopped_at=$cur rc=0"
    log "FAILED：正常退出但未写出更高的 checkpoint，见 run.log 末尾"
    tail -30 run.log >> worker.out
    exit 1
  else
    crashes=$((crashes + 1))
    log "进程异常退出 rc=$rc（第 $crashes 次）at=$cur"
    [ "$crashes" -ge 2 ] && { st "FAILED crash rc=$rc at=$cur"; exit 1; }
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
log "段完成；待上传 checkpoint: $archives 个"
