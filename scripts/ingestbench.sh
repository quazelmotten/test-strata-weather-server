#!/usr/bin/env bash
# Ingest-to-queryable benchmark (spec §8: "one forecast hour of GFS with ~10
# configured params becomes queryable within a few seconds of the file being
# stable") PLUS the V1.2 phase-D production-scale measurement: wall-to-first-
# queryable, wall-to-all-visible, PEAK RSS, storage-counter deltas (renames /
# bytes-written vs final store size / fsyncs) and per-file ingest times.
#
#   DATASET=realistic (default) : tests/perfdata/realistic/ -- 6 files (3 runs x
#       f000+f006), full GFS geometry 1440x720, 37 isobaric levels x TMP,HGT,UGRD,
#       VGRD = 148 messages/file (~190 MB each), wind messages carry the grid-
#       relative flag on a lat/lon grid. THIS is the merge-path stress: L=37 planes
#       per GROUP, so the pre-V1.2 per-message writer shows its O(L^2) amplification.
#   DATASET=perf : the milestone-5 dataset (gfs 22 msgs + hrrr LCC 13 msgs).
#
#   VARIANT=after (default) : current tree; counters come from /metrics
#       (wxpoint_storage_* gauges, added V1.2 phase D).
#   VARIANT=before         : detached worktree of tag v1.1 (commit 82f4998, i.e.
#       phases A/B0/B/C ABSENT), built there; that build has NO storage counters,
#       so accounting comes from strace LAUNCH mode (rename/fsync/write syscalls +
#       byte sums). Wall/RSS numbers come from a SECOND, untraced run.
#
# Usage:
#   scripts/ingestbench.sh                       # AFTER, realistic dataset
#   VARIANT=before scripts/ingestbench.sh        # BEFORE (v1.1 worktree)
#   BATCH=1 ... / RETENTION=2 ... to sweep the knobs
set -u
cd "$(dirname "$0")/.."

DATASET=${DATASET:-realistic}
VARIANT=${VARIANT:-after}
BATCH=${BATCH:-4}
RETENTION=${RETENTION:-4}
SCAN=${SCAN:-1}
THREADS=${THREADS:-16}
PORT=${PORT:-8092}
BASE="http://127.0.0.1:${PORT}"
ROOT=/tmp/opencode/wxpoint-ingestbench-${VARIANT}-${DATASET}
BIN=target/release/wxpoint

if [ "$VARIANT" = "before" ]; then
  WT=/tmp/opencode/wxpoint-v11-worktree
  if [ ! -d "$WT" ]; then
    echo "creating detached v1.1 worktree at $WT ..."
    git worktree add --detach "$WT" v1.1 >/dev/null
  fi
  BIN=$WT/target/release/wxpoint
  if [ ! -x "$BIN" ]; then
    echo "building release binary in the v1.1 worktree (first time, ~2 min)..."
    (cd "$WT" && cargo build --release -p wx-server) >/tmp/opencode/v11-build.log 2>&1 || {
      echo "FATAL: v1.1 worktree build failed"; tail -20 /tmp/opencode/v11-build.log; exit 1; }
  fi
fi
[ -x "$BIN" ] || { echo "building release binary..."; cargo build --release -p wx-server; }

# ---------------------------------------------------------------------------
# dataset -> files, config, expected visibility triples
# ---------------------------------------------------------------------------
rm -rf "$ROOT"
mkdir -p "$ROOT/gfs" "$ROOT/data"
case "$DATASET" in
  realistic|reduced)
    if [ "$DATASET" = "realistic" ]; then
      SRC=tests/perfdata/realistic
      MINLEV=37
    else
      SRC=tests/perfdata/realistic_reduced
      MINLEV=16
    fi
    ls "$SRC"/gfs.t*.grib2 >/dev/null 2>&1 || {
      echo "FATAL: no $DATASET dataset (run scripts/gen_realistic.sh first)"; exit 1; }
    # hard links where possible (190 MB x 6 copies otherwise); mtime reset so the
    # delivery looks fresh. NOTE: a hard link shares the INODE, so touching the
    # copy also bumps the source file's mtime (harmless here, documented).
    for f in "$SRC"/gfs.t*.grib2; do
      ln "$f" "$ROOT/gfs/$(basename "$f")" 2>/dev/null || cp "$f" "$ROOT/gfs/$(basename "$f")"
    done
    touch "$ROOT"/gfs/*.grib2
    MODELS='[[models]]
name = "gfs"
ingest_dir = "'"$ROOT"'/gfs"
file_glob = "gfs.*.grib2"

  [[models.params]]
  name = "TMP"
  levels = ["isobaric:*"]

  [[models.params]]
  name = "HGT"
  levels = ["isobaric:*"]

  [[models.params]]
  name = "UGRD"
  levels = ["isobaric:*"]

  [[models.params]]
  name = "VGRD"
  levels = ["isobaric:*"]'
    EXPECT_DIRS="2026101900 2026101906 2026101912"
    EXPECT_FH="0 6"
    ;;
  perf)
    SRC=tests/perfdata
    mkdir -p "$ROOT/hrrr"
    cp "$SRC"/gfs_perf_f00*.grib2 "$ROOT/gfs/" 2>/dev/null
    cp "$SRC"/hrrr_perf_f00*.grib2 "$ROOT/hrrr/" 2>/dev/null
    touch "$ROOT"/gfs/*.grib2 "$ROOT"/hrrr/*.grib2
    MODELS='[[models]]
name = "gfs"
ingest_dir = "'"$ROOT"'/gfs"
file_glob = "gfs_perf_*.grib2"

  [[models.params]]
  name = "HGT"
  levels = ["isobaric:*"]

  [[models.params]]
  name = "TMP"
  levels = ["height_above_ground:2", "isobaric:*"]

  [[models.params]]
  name = "UGRD"
  levels = ["height_above_ground:10"]

  [[models.params]]
  name = "VGRD"
  levels = ["height_above_ground:10"]

  [[models.params]]
  name = "RH"
  levels = ["height_above_ground:2"]

  [[models.params]]
  name = "PRMSL"
  levels = ["surface"]

  [[models.params]]
  name = "GUST"
  levels = ["height_above_ground:10"]

[[models]]
name = "hrrr"
ingest_dir = "'"$ROOT"'/hrrr"
file_glob = "hrrr_perf_*.grib2"

  [[models.params]]
  name = "TMP"
  levels = ["height_above_ground:2", "isobaric:*"]

  [[models.params]]
  name = "HGT"
  levels = ["isobaric:*"]

  [[models.params]]
  name = "UGRD"
  levels = ["height_above_ground:10"]

  [[models.params]]
  name = "VGRD"
  levels = ["height_above_ground:10"]

  [[models.params]]
  name = "RH"
  levels = ["height_above_ground:2"]'
    EXPECT_DIRS=""
    EXPECT_FH="0 6"
    ;;
esac

BATCHLINE=""
if [ "$VARIANT" = "after" ]; then
  BATCHLINE="decode_batch_files = $BATCH"
fi

cat > "$ROOT/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
data_dir = "$ROOT/data"

[ingest]
scan_interval_secs = $SCAN
retention_runs = $RETENTION
stable_window_secs = 0
ingest_threads = $THREADS
$BATCHLINE

$MODELS
EOF

# ---------------------------------------------------------------------------
# run one server instance, polling RSS while ingest proceeds
# ---------------------------------------------------------------------------
run_one() { # $1: label, $2: "strace"|"plain" -> sets globals T_* and LOG
  local label="$1" mode="$2"
  local log="$ROOT/server-${label}.log"
  rm -rf "$ROOT/data"
  if [ "$mode" = "strace" ]; then
    strace -f -y -e trace=rename,renameat,renameat2,fsync,write,sendfile \
      -o "$ROOT/strace-${label}.log" -s 8 "$BIN" --config "$ROOT/config.toml" \
      > "$log" 2>&1 &
  else
    "$BIN" --config "$ROOT/config.toml" > "$log" 2>&1 &
  fi
  local srv=$!
  local t0=$(date +%s%N)
  READY=0; ALLV=0; PEAK_KB=0; CPU_S=0; EXIT_WHY="ok"; PROBE_OUT="(skipped)"
  local polls=${MAXPOLLS:-1200}
  local i=0 pending=""
  while [ "$i" -lt "$polls" ]; do
    i=$((i+1))
    sleep 0.1
    local kb=$(awk '/VmHWM/{print $2}' /proc/$srv/status 2>/dev/null); kb=${kb:-0}
    [ "$kb" -gt "$PEAK_KB" ] && PEAK_KB=$kb
    if [ "$READY" = "0" ]; then
      local code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/ready" 2>/dev/null || echo 000)
      [ "$code" = "200" ] && READY=$(( ($(date +%s%N) - t0) / 1000000 ))
    fi
    if [ "$ALLV" = "0" ]; then
      curl -s "$BASE/v1/inventory" > "$ROOT/inv.json" 2>/dev/null
      if [ -s "$ROOT/inv.json" ]; then
        if pending=$(python3 scripts/_visible.py "$ROOT/inv.json" $EXPECT_SPEC 2>/dev/null); then
          ALLV=$(( ($(date +%s%N) - t0) / 1000000 ))
        fi
      fi
    fi
    # ROBUST EXIT: both milestones reached -> stop immediately (this is what used
    # to hang: the loop had NO success condition at all, and the visibility parser
    # did not know /v1/inventory wraps its blocks under an "inventory" key, so the
    # condition could never fire and the run polled its whole budget.)
    if [ "$READY" != "0" ] && [ "$ALLV" != "0" ]; then break; fi
    if ! kill -0 $srv 2>/dev/null; then EXIT_WHY="server-died"; break; fi
  done
  if [ "$EXIT_WHY" = "ok" ] && { [ "$READY" = "0" ] || [ "$ALLV" = "0" ]; }; then
    EXIT_WHY="deadline(${polls}x0.1s)"
  fi
  CPU_S=$(awk '{printf "%.2f", ($14+$15)/100}' /proc/$srv/stat 2>/dev/null)
  # metrics BEFORE the process dies (only the AFTER build exposes them)
  if [ "$VARIANT" = "after" ]; then
    curl -s "$BASE/metrics" > "$ROOT/metrics-${label}.txt"
  fi
  # correctness rides along: probe the LIVE server over the realistic dataset
  if [ "$DATASET" = "realistic" ] && [ "$ALLV" != "0" ]; then
    PROBE_OUT=$(python3 scripts/_probe_realistic.py "$BASE" 2>&1 | tail -1)
  fi
  kill $srv 2>/dev/null; wait $srv 2>/dev/null
  T_LOG="$log"; T_MODE="$mode"
  echo "[$label/$mode] first-queryable=${READY}ms all-visible=${ALLV}ms peakRSS=$((PEAK_KB/1024))MB cpu=${CPU_S}s exit=${EXIT_WHY}"
  echo "[$label/$mode] value-probes: ${PROBE_OUT}"
  if [ "$EXIT_WHY" != "ok" ] && [ -n "$pending" ]; then echo "[$label/$mode] pending: $pending"; fi
}

# expected tuples for the visibility parser: "model:dir:fh[:min_levels]".
# The realistic dataset demands the WHOLE 37-level isobaric stack per field, so a
# half-published parameter never counts as visible.
EXPECT_SPEC=""
if [ "$DATASET" = "realistic" ] || [ "$DATASET" = "reduced" ]; then
  if [ "$DATASET" = "realistic" ]; then
    ALLDIRS="2026101900 2026101906 2026101912"; MINLEV=37
  else
    ALLDIRS="2026101900"; MINLEV=16          # single-run reduced variant
  fi
  nruns=$RETENTION
  KEEPDIRS=$(echo $ALLDIRS | tr ' ' '\n' | tail -n $nruns | tr '\n' ' ')
  for d in $KEEPDIRS; do for h in $EXPECT_FH; do EXPECT_SPEC="$EXPECT_SPEC gfs:$d:$h:$MINLEV"; done; done
else
  for h in $EXPECT_FH; do EXPECT_SPEC="$EXPECT_SPEC gfs:2026101912:$h"; done
  for h in 0 3; do EXPECT_SPEC="$EXPECT_SPEC hrrr:2026101912:$h"; done
fi

echo "== ingestbench: variant=$VARIANT dataset=$DATASET batch=${BATCH} retention=$RETENTION threads=$THREADS scan=$SCAN"
T0_PLACE=$(date +%s%N)
if [ "$VARIANT" = "before" ]; then
  # V1.2 phase D: the full-scale BEFORE-under-strace measurement was PATHOLOGICAL
  # (the O(L^2) writer traced ~54 GB of writes and the tracer stalled), so syscall
  # accounting for the pre-V1.2 build is OPT-IN via STRACE_COUNTERS=1 on a SMALL
  # dataset. The default BEFORE pass is UNTRACED wall/RSS only, with write volume
  # reported ANALYTICALLY (see the accounting section below).
  COUNTER_RUN=0
  if [ "${STRACE_COUNTERS:-0}" = "1" ]; then
    run_one counters strace
    COUNTER_RUN=1
  fi
  run_one timing plain
else
  run_one main plain
  COUNTER_RUN=0
fi

# ---------------------------------------------------------------------------
# per-file ingest lines + counter accounting
# ---------------------------------------------------------------------------
echo "--- per-file ingest (server log) ---"
grep -E "ingested .*groups" "$T_LOG" | head -12 || true

final_store_bytes() {
  find "$ROOT/data" -name 'f*.bin' -printf '%s\n' 2>/dev/null | awk '{t+=$1} END{print t+0}'
}
STORE_BYTES=$(final_store_bytes)
echo "--- storage accounting ---"
if [ "$VARIANT" = "after" ]; then
  M="$ROOT/metrics-main.txt"
  ren=$(grep -m1 '^wxpoint_storage_renames_total ' "$M" | awk '{print $2}')
  bwr=$(grep -m1 '^wxpoint_storage_bytes_written_total ' "$M" | awk '{print $2}')
  fsy=$(grep -m1 '^wxpoint_storage_fsyncs_total ' "$M" | awk '{print $2}')
  grp=$(grep -m1 '^wxpoint_ingest_groups_written_total ' "$M" | awk '{print $2}')
  ign=$(grep -m1 '^wxpoint_ignored_files_total ' "$M" | awk '{print $2}')
  quar=$(grep -m1 '^wxpoint_quarantined_files_total ' "$M" | awk '{print $2}')
  rskip=$(grep -m1 '^wxpoint_retention_skipped_files_total ' "$M" | awk '{print $2}')
  stw=$(grep -m1 '^wxpoint_state_writes_total ' "$M" | awk '{print $2}')
  stb=$(grep -m1 '^wxpoint_state_bytes_written ' "$M" | awk '{print $2}')
  nf=$(find "$ROOT/data" -name 'f*.bin' | wc -l)
  python3 - "$ren" "$bwr" "$fsy" "$grp" "$nf" "$STORE_BYTES" "${stw:-0}" "${stb:-0}" <<'PY'
import sys
ren,bwr,fsy,grp,nf,store,stw,stb=[float(x) for x in sys.argv[1:9]]
ratio=(bwr/store) if store else 0.0
print(f"renames={int(ren)} (field files={int(nf)} + grid.bin/manifest writes)")
print(f"bytes_written={bwr/1e6:.1f} MB vs final store {store/1e6:.1f} MB -> amplification {ratio:.4f}x (FIELD-PATH counters only; state saves are reported separately below)")
print(f"fsyncs={int(fsy)} groups_written={int(grp)}")
# V1.3 phase E3a: the state writer is deliberately OUT of the storage counters
# above; batched persistence (per decode chunk + dirty-gated round end) makes
# these numbers small. Report them so the amplification headline stays honest.
print(f"state_saves={int(stw)} state_bytes={int(stb)} (files.json rewrites, batched per chunk+round)")
PY
else
  if [ "$COUNTER_RUN" = "1" ] && [ -f "$ROOT/strace-counters.log" ]; then
  SLOG="$ROOT/strace-counters.log"
  python3 - "$SLOG" "$STORE_BYTES" <<'PY'
import re,sys
path,store=sys.argv[1],float(sys.argv[2])
rn=fs=0; wb=0
pat_w=re.compile(r'\b(write|sendfile)\((\d+), .*?, (\d+)\)\ = (\d+)')
for ln in open(path, errors='replace'):
    if ' rename' in ln and ') =' in ln: rn+=1
    elif re.search(r'\bfsync\(', ln): fs+=1
    else:
        m=pat_w.search(ln)
        if m and int(m.group(2))>2: wb+=int(m.group(4))
ratio=(wb/store) if store else 0.0
print(f"renames={rn} bytes_written={wb/1e6:.1f} MB vs final store {store/1e6:.1f} MB -> amplification {ratio:.4f}x fsyncs={fs}")
PY
  else
    python3 - "${MINLEV:-16}" "$STORE_BYTES" <<'PY'
import sys
L=int(sys.argv[1]); store=float(sys.argv[2])
mult=(L+1)/2.0
print("(pre-V1.2 build has NO storage counters and this run was UNTRACED on purpose:")
print(f" write volume is therefore ANALYTIC: per GROUP the old writer serialized 1+2+...+{L} planes where {L} suffice -> multiplier (L+1)/2 = {mult:.2f}x")
print(f" i.e. ~{store*mult/1e6:.0f} MB written for a {store/1e6:.0f} MB final store on THIS dataset")
print(" measured cross-reference (phase B, standard fixture, L=8): 3.55x = 78 plane serializations where 8 suffice, 45 renames/GFS-hour)")
PY
  fi
fi
echo "--- peak RSS: see the timing line(s) above (VmHWM poller every 100 ms) ---"
# KEEP the small artifacts (server logs, metrics snapshot, inventory, strace trace)
# under /tmp/opencode for the record; drop only the multi-gigabyte store so a repeat
# run starts COLD (that is what the measurement is about).
mv "$ROOT"/server-*.log /tmp/opencode/ 2>/dev/null || true
mv "$ROOT"/metrics-*.txt /tmp/opencode/ 2>/dev/null || true
mv "$ROOT"/inv.json /tmp/opencode/ 2>/dev/null || true
[ -f "$ROOT/strace-counters.log" ] && mv "$ROOT/strace-counters.log" /tmp/opencode/wxpoint-before-strace.log
rm -rf "$ROOT"
if [ "${EXIT_WHY:-ok}" != "ok" ]; then
  echo "FATAL: ingest visibility never completed (${EXIT_WHY})"
  exit 3
fi
exit 0
