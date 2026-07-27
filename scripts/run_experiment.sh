#!/usr/bin/env bash
# One-click experiment column: timing (结果一) + memory (结果三) for the
# minimal-mpt at a given commit or the current worktree.
#
#   run_experiment.sh worktree            [label] [reps]   # uncommitted state
#   run_experiment.sh <commit-ish>        [label] [reps]   # e.g. 570b037, HEAD~2
#   MEM_ONLY=1 run_experiment.sh ...                       # skip the executor
#       build and the timing replay; just build the mem loader and measure the
#       250M-entry load (peak/steady/load time), ~5 min end to end.
#
# Method (matches docs/opprof-full-sweep-2026-07-08.md, production width W=1M):
#   1. snapshot the source tree (git archive for commits; file copy for the
#      worktree) — the executor's three path deps (minimal-mpt /
#      replay-checkpoint / cfxpack) are ALL pointed at this one snapshot, so
#      the executor always builds against a mutually consistent crate set;
#   2. timing: replay ckpt_85520000 -> one 2000-epoch group, [profile] table +
#      timestamped log (exec/save/recovery derived);
#   3. memory: malloc_trim-instrumented ckpt-restore-rss loads ckpt_150052000
#      (250M entries), peak=VmHWM, steady=post-trim VmRSS, 110GiB watchdog.
# Pre-MMPT_PROFILE commits get the rebuilt state-layer overlay when their
# layout matches (pre-refactor WIPs); otherwise timing is wall-clock only.
# Builds retry 3x (this box's gcc throws transient ICEs under load).
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SWEEP="${SWEEP:-/conflux-new-storage-workspace/sweep-opprof}"
PROD=$REPO/oracle/merge-only-conflux-v3.0.3
TOML=$PROD/crates/replay-data-executor/Cargo.toml
CKPT_TIMING=$SWEEP/ckpt_85520000.bin
CKPT_MEM=$SWEEP/ckpt_150052000.bin
CAP_KB=$((110 * 1024 * 1024))

SRC="${1:?usage: run_experiment.sh <worktree|commit-ish> [label] [reps]}"
REPS="${3:-1}"

# --- 1. snapshot the source -------------------------------------------------
if [[ "$SRC" == worktree ]]; then
  LABEL="${2:-wt$(git -C "$REPO" rev-parse --short HEAD)}"
  SNAP=$SWEEP/snaps/exp_$LABEL
  rm -rf "$SNAP"; mkdir -p "$SNAP"
  cp "$REPO/Cargo.toml" "$REPO/Cargo.lock" "$SNAP/"
  cp -r "$REPO/crates" "$SNAP/"
else
  C=$(git -C "$REPO" rev-parse --short "$SRC")
  LABEL="${2:-$C}"
  SNAP=$SWEEP/snaps/exp_$LABEL
  rm -rf "$SNAP"; mkdir -p "$SNAP"
  git -C "$REPO" archive "$C" Cargo.toml Cargo.lock crates | tar -x -C "$SNAP"
fi
echo "== snapshot: $SNAP (label=$LABEL, reps=$REPS)"

# --- overlay for pre-instrumentation commits --------------------------------
if ! grep -rq MMPT_PROFILE "$SNAP/crates/minimal-mpt/src"; then
  if grep -q 'key_codec' "$SNAP/crates/minimal-mpt/src/state/mod.rs" 2>/dev/null; then
    cp "$SWEEP"/overlay/{mod,rotation,prefix}.rs "$SNAP/crates/minimal-mpt/src/state/"
    echo "== pre-refactor tree: profile overlay applied"
  else
    echo "!! no MMPT_PROFILE and layout does not match the overlay: timing will be wall-clock only"
  fi
fi

retry_build() { # dir, cargo args...
  local dir=$1; shift
  for attempt in 1 2 3; do
    if ( cd "$dir" && cargo build "$@" ); then return 0; fi
    echo "!! build attempt $attempt failed (transient ICE?), retrying"
  done
  echo "!! build failed 3x"; return 1
}

# --- 2. build the mem loader (malloc_trim variant of ckpt-restore-rss) ------
cp "$SWEEP/overlay/ckpt_restore_rss.rs" "$SNAP/crates/replay-checkpoint/src/bin/ckpt_restore_rss.rs"
retry_build "$SNAP" -q --release -p cfx-replay-checkpoint --bin ckpt-restore-rss
cp -f "$SNAP/target/release/ckpt-restore-rss" "$SWEEP/membin_$LABEL"

if [[ "${MEM_ONLY:-0}" != 1 ]]; then
  # --- 3. build the executor against the snapshot ---------------------------
  git -C "$PROD" checkout -- crates/replay-data-executor/Cargo.toml Cargo.lock
  sed -i \
    -e "s#\.\./\.\./\.\./\.\./crates/minimal-mpt#$SNAP/crates/minimal-mpt#" \
    -e "s#\.\./\.\./\.\./\.\./crates/replay-checkpoint#$SNAP/crates/replay-checkpoint#" \
    -e "s#\.\./\.\./\.\./\.\./crates/cfxpack#$SNAP/crates/cfxpack#" \
    "$TOML"
  retry_build "$PROD/crates/replay-data-executor" --release --no-default-features \
    --features backend-minimal-mpt --bin cfx-replay-exec
  cp -f "$PROD/target/release/cfx-replay-exec" "$SWEEP/bin_$LABEL"
  git -C "$PROD" checkout -- crates/replay-data-executor/Cargo.toml Cargo.lock

  # --- 4. timing runs (结果一), production width ----------------------------
  stamp() { python3 -u -c 'import sys,time
for line in sys.stdin:
    sys.stdout.write(f"{time.time():.3f} {line}")'; }
  CKC=$SWEEP/ck_scratch.bin
  trap 'rm -f "$CKC"' EXIT
  for rep in $(seq 1 "$REPS"); do
    echo "== timing run $LABEL rep$rep"
    cp -f "$CKPT_TIMING" "$CKC"
    ( cd "$REPO" && env MMPT_PROFILE=1 MMPT_TIMING=1 MMPT_MERGE_TIMING=1 MMPT_RECOVER_TIMING=1 \
        "$SWEEP/bin_$LABEL" --input data/packed-full --config data/conflux-rust-config/hydra-v303.toml \
        --checkpoint "$CKC" --checkpoint-every-groups 1 --stop-after-checkpoint \
        --max-mismatches 1000000000 --anomaly-streak 1000000000 2>&1 ) \
      | stamp > "$SWEEP/results/prof_${LABEL}_$rep.log"
  done
fi

# --- 5. memory run (结果三) --------------------------------------------------
echo "== memory run $LABEL"
LOG=$SWEEP/results/mem_$LABEL.log
env MMPT_RECOVER_TIMING=1 MMPT_TIMING=1 "$SWEEP/membin_$LABEL" "$CKPT_MEM" 5 >"$LOG" 2>&1 &
pid=$!
while kill -0 "$pid" 2>/dev/null; do
  rss=$(awk '/^VmRSS/{print $2}' /proc/$pid/status 2>/dev/null || echo 0)
  if [[ ${rss:-0} -gt $CAP_KB ]]; then
    echo "!! $LABEL exceeded RSS cap, killing" | tee -a "$LOG"; kill -9 "$pid" || true; break
  fi
  sleep 1
done
wait "$pid" || echo "!! mem loader exited nonzero (see $LOG)"

# --- 6. summary --------------------------------------------------------------
echo "==== $LABEL results ===="
python3 "$SWEEP/summarize.py" 2>/dev/null | grep -E "col|^${LABEL}_" || true
SWEEP="$SWEEP" python3 - "$LABEL" <<'EOF'
import os, re, sys
from pathlib import Path
label = sys.argv[1]
R = Path(os.environ['SWEEP']) / 'results' 
for p in sorted(R.glob(f'prof_{label}_*.log')):
    td = tw = sr = None
    for line in p.read_text().splitlines():
        m = re.match(r'^(\d+\.\d+) (.*)$', line)
        if not m: continue
        ts, rest = float(m.group(1)), m.group(2)
        if 'delta_root calls' in rest: td = ts
        if 'wrote checkpoint' in rest: tw = ts
        mm = re.search(r'stream_read_snapshot entries=\d+ elapsed=(\d+)ms', rest)
        if mm: sr = int(mm.group(1)) / 1000
    save = f'{tw-td:.1f}s' if td and tw else '?'
    print(f'{p.stem}: save={save} recovery_stream_read={sr}s')
mp = R / f'mem_{label}.log'
if mp.exists():
    txt = mp.read_text()
    m = re.search(r'after_trim rss_kb=(\d+) hwm_kb=(\d+)', txt)
    t = re.search(r'load done height=\d+ elapsed=([\d.]+)s', txt)
    if m:
        print(f'mem_{label}: peak={int(m.group(2))/1048576:.1f}GiB steady={int(m.group(1))/1048576:.1f}GiB load={t.group(1) if t else "?"}s')
EOF
echo "==== $LABEL done ===="
