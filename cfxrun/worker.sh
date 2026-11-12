#!/usr/bin/env bash
# 在 worker 上运行：下载 →（必要时）label_blame → 重放到 END。
# 由 cfxrun.py 上传并以 nohup 启动；重复启动安全，已有 ckpt.bin 则从中恢复。
#
# 完整路径：
#   1) 执行器必须是 build_replay.sh（v3.0.3 merge）产物，禁止 oracle HEAD 裸编
#   2) 未打标 .cfxpack 先 label_blame 成 .cfxpackb，再 --anomaly-streak 0
#   3) 执行器一次跑到段尾，--checkpoint-every-seconds LOCAL（默认 1h）原地覆盖 ckpt.bin
#   4) 独立计时器：墙钟累计 WINDOW（默认 24h）就把**现成的** ckpt.bin 硬链快照后
#      压缩进 archives/，由主控 multipart 上传到
#      s3://conflux-replay-txs/checkpoints/<run>/<seg>/ckpt_<高度>.bin.zst
#      不回内存重 dump；归档高度取 ckpt 自身高度，与当前重放高度可差 ≤1h。
#   段尾 ckpt 只留本地，不上传（输入侧已有下一段起点）。
set -uo pipefail
cd "$(dirname "$0")"
source seg.env # SEG START END WINDOW_SECONDS ANOMALY_STREAK
WINDOW_SECONDS=${WINDOW_SECONDS:-86400}
LOCAL_CKPT_SECONDS=${LOCAL_CKPT_SECONDS:-3600}
ANOMALY_STREAK=${ANOMALY_STREAK:-0}
LABEL_WINDOW=${LABEL_WINDOW:-10000}

log() { echo "[$(date '+%F %T')] $*"; }
st()  { echo "$*" > status; log "status=$*"; }

# 重复部署安全：别让两个 worker / 两个执行器同时写 ckpt.bin。
if [ -s worker.pid ]; then
  old=$(cat worker.pid)
  if [ "$old" != "$$" ] && kill -0 "$old" 2>/dev/null \
     && tr '\0' ' ' < "/proc/$old/cmdline" 2>/dev/null | grep -q 'worker\.sh'; then
    log "已有 worker 在跑（pid $old），本实例退出"
    exit 0
  fi
fi
if [ -s exec.pid ]; then
  old=$(cat exec.pid)
  if kill -0 "$old" 2>/dev/null \
     && tr '\0' ' ' < "/proc/$old/cmdline" 2>/dev/null | grep -q 'cfx-replay-exec'; then
    log "发现上轮遗留的执行器（pid $old），先终止再续跑"
    kill -9 "$old" 2>/dev/null
    for _ in $(seq 1 30); do kill -0 "$old" 2>/dev/null || break; sleep 1; done
  fi
  rm -f exec.pid
fi

echo $$ > worker.pid
trap 'rm -f worker.pid' EXIT

# run.log 末行进度里的当前高度（主要用于展示/续跑记录）
current_height() {
  grep 'replay progress ' run.log 2>/dev/null | tail -1 \
    | grep -o 'height=[0-9]*' | grep -o '[0-9]*$'
}

# ckpt.bin 实际对应的高度：执行器每次写完会打印 [ckpt] height=<h>
ckpt_height() {
  grep -o '\[ckpt\] height=[0-9]*' run.log 2>/dev/null | tail -1 | grep -o '[0-9]*$'
}

last_height() {
  local h
  h=$(ckpt_height)
  if [ -z "$h" ]; then
    h=$(grep -o 'wrote checkpoint [^ ]* at height [0-9]*' run.log 2>/dev/null | tail -1 | grep -o '[0-9]*$')
  fi
  if [ -z "$h" ]; then
    h=$(current_height)
  fi
  echo "${h:-$START}"
}

# 把当前 ckpt.bin 硬链成 archives/ckpt_<h>.bin（不动 ckpt.bin，也不占额外空间）。
# 执行器用 tmp+rename 覆盖 ckpt.bin，硬链锁住旧 inode；若期间高度已变则重来。
SNAP_H=
snapshot_ckpt() {
  local tries=0 h h2
  while [ "$tries" -lt 3 ]; do
    h=$(ckpt_height)
    [ -z "$h" ] && return 1
    # 清掉除本次高度以外的残留快照（失败/换高度时不漏盘）
    find archives -maxdepth 1 -name 'ckpt_*.bin' ! -name "ckpt_${h}.bin" -delete 2>/dev/null
    rm -f "archives/ckpt_${h}.bin"
    ln -f ckpt.bin "archives/ckpt_${h}.bin" 2>/dev/null \
      || cp -f ckpt.bin "archives/ckpt_${h}.bin" || return 1
    h2=$(ckpt_height)
    if [ "$h" = "$h2" ]; then
      SNAP_H=$h
      log "已快照 ckpt.bin → archives/ckpt_${h}.bin（高度 $h）"
      return 0
    fi
    rm -f "archives/ckpt_${h}.bin"
    tries=$((tries + 1))
  done
  return 1
}

compress_snapshot() {
  local h="$1"
  local snap="archives/ckpt_${h}.bin"
  local file="ckpt_${h}.bin.zst"
  [ -f "$snap" ] || return 0
  st "ARCHIVING $h"
  log "压缩快照 → archives/$file"
  zstd -T0 -3 -q -f "$snap" -o "archives/$file" || {
    st "FAILED archive height=$h"
    return 1
  }
  rm -f "$snap"
  local size sha tmp
  size=$(stat -c %s "archives/$file")
  sha=$(sha256sum "archives/$file" | awk '{print $1}')
  tmp="archives/manifest.tsv.tmp"
  awk -F '\t' -v h="$h" '$1 != h' archives/manifest.tsv 2>/dev/null > "$tmp" || true
  printf '%s\t%s\t%s\t%s\n' "$h" "$size" "$sha" "$file" >> "$tmp"
  mv "$tmp" archives/manifest.tsv
  rm -f pending_archive
  date +%s > window_origin
  echo "$h" > archive_high
  log "已归档 archives/$file size=$size sha256=$sha"
}

# 到点才归档：直接拿现成 ckpt.bin，不回内存重写。
archive_if_due() {
  local h origin now elapsed last
  h=$(ckpt_height)
  [ -z "$h" ] && return 0
  last=$(cat archive_high 2>/dev/null || echo 0)
  [ "$h" -le "$last" ] && return 0          # 这个高度已归档
  [ "$h" -ge "$END" ] && return 0           # 段尾不传
  now=$(date +%s)
  origin=$(cat window_origin 2>/dev/null || echo "$now")
  elapsed=$((now - origin))
  [ "$elapsed" -lt "$WINDOW_SECONDS" ] && return 0
  log "距上次归档 ${elapsed}s ≥ ${WINDOW_SECONDS}s：归档本地 ckpt（高度 $h）"
  snapshot_ckpt || { log "快照失败，下轮重试"; return 0; }
  echo "$SNAP_H" > pending_archive
  compress_snapshot "$SNAP_H"
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

cur=$(last_height)
echo "$cur" > height
# 上次归档被中断：优先用已落盘的硬链快照续压（高度与内容仍然对应）。
if [ -f pending_archive ]; then
  pending=$(cat pending_archive)
  if [ -f "archives/ckpt_${pending}.bin" ]; then
    compress_snapshot "$pending" || exit 1
  else
    rm -f pending_archive   # 快照没落成，等下一个本地 ckpt 再归档
  fi
fi
# 归档计时跨重启保留；archive_high 记录已归档到的 ckpt 高度。
[ -f window_origin ] || date +%s > window_origin
[ -f archive_high ] || echo 0 > archive_high
TICK=${TICK:-30}
window_origin=$(cat window_origin)

log "重放 $cur → $END；执行器单进程跑到底，每 ${LOCAL_CKPT_SECONDS}s 覆盖 ckpt.bin，墙钟每 ${WINDOW_SECONDS}s 归档上传"
crashes=0
while :; do
  left=$(( (END - cur) / 2000 ))
  [ "$left" -lt 1 ] && left=1
  st "RUNNING $cur $(( (cur - START) * 100 / (END - START) ))%"
  log "续跑起点=$cur remaining_groups=$left local_ckpt=${LOCAL_CKPT_SECONDS}s archive_window=${WINDOW_SECONDS}s"
  echo "=== run from $cur, groups=$left, local_ckpt_seconds=$LOCAL_CKPT_SECONDS, window_seconds=$WINDOW_SECONDS, $(date -Is)" >> run.log
  ./cfx-replay-exec --input input --config hydra.toml --checkpoint ckpt.bin \
    --checkpoint-every-groups "$left" --checkpoint-every-seconds "$LOCAL_CKPT_SECONDS" \
    --anomaly-streak "$ANOMALY_STREAK" >> run.log 2>&1 &
  ep=$!
  echo "$ep" > exec.pid
  while kill -0 "$ep" 2>/dev/null; do
    sleep "$TICK"
    n=$(current_height)
    if [ -n "$n" ]; then
      # 只用于展示；height 文件必须跟随 ckpt 高度，否则重启后 left 会算少。
      echo "RUNNING $n $(( (n - START) * 100 / (END - START) ))%" > status
    fi
    archive_if_due || true
  done
  wait "$ep"; rc=$?
  rm -f exec.pid
  if [ "$rc" -eq 0 ]; then
    cur=$(last_height)
    echo "$cur" > height
    if [ "$cur" -lt "$END" ]; then
      st "FAILED stopped_at=$cur rc=0"
      log "FAILED：正常退出但未到达 END=$END（ckpt 高度 $cur）"
      tail -30 run.log >> worker.out
      exit 1
    fi
    log "重放结束于高度 $cur"
    break
  fi
  if grep -q 'replay anomaly:' run.log; then
    st "FAILED anomaly at=$cur rc=$rc"
    log "FAILED：连续 mismatch 超阈值，见 run.log 末尾"
    tail -30 run.log >> worker.out
    exit 1
  fi
  crashes=$((crashes + 1))
  cur=$(last_height)
  echo "$cur" > height
  log "执行器异常退出 rc=$rc（第 $crashes 次），ckpt.bin 高度=$cur"
  [ "$crashes" -ge 2 ] && { st "FAILED crash rc=$rc at=$cur"; tail -30 run.log >> worker.out; exit 1; }
  log "将从 ckpt.bin 高度 $cur 重试一次"
done

# 段尾若还留着未压完的快照，先补完再报状态。
if [ -f pending_archive ]; then
  pending=$(cat pending_archive)
  [ -f "archives/ckpt_${pending}.bin" ] && { compress_snapshot "$pending" || exit 1; } || rm -f pending_archive
fi

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
