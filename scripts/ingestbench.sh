#!/usr/bin/env bash
# Ingest-to-queryable timing + peak RSS for the milestone-5 perf dataset
# (spec §8: "one forecast hour of GFS with ~10 configured params becomes
# queryable within a few seconds of the file being stable").
#
# Strategy: place the four perf GRIB files into fake ingest dirs, start the
# RELEASE binary (stable_window_secs=0 => a file is eligible once re-observed
# unchanged on the NEXT scan round, so earliest ingest ~= t_place + 1 scan
# interval), then poll /ready and /v1/inventory until each (model, run, fh)
# becomes visible. Peak RSS comes from /usr/bin/time -v on the server child.
#
# Usage: ./scripts/ingestbench.sh [scan_interval_secs]   (default 1)
set -u
cd "$(dirname "$0")/.."
BIN=target/release/wxpoint
[ -x "$BIN" ] || { echo "building release binary..."; cargo build --release -p wx-server; }
SCAN=${1:-1}
ROOT=/tmp/opencode/wxpoint-ingestbench
BASE=http://127.0.0.1:8092
PASS=1

rm -rf "$ROOT"
mkdir -p "$ROOT/gfs" "$ROOT/hrrr"
T0=$(date +%s%N)
cp tests/perfdata/gfs_perf_f00*.grib2 "$ROOT/gfs/"
cp tests/perfdata/hrrr_perf_f00*.grib2 "$ROOT/hrrr/"
touch "$ROOT"/gfs/*.grib2 "$ROOT"/hrrr/*.grib2   # mtime = now (fresh delivery)
echo "files placed at t0"

cat > "$ROOT/config.toml" <<EOF
[server]
bind = "127.0.0.1:8092"
data_dir = "$ROOT/data"

[ingest]
scan_interval_secs = $SCAN
retention_runs = 4
stable_window_secs = 0
ingest_threads = 16
decode_batch_files = ${BATCH:-4}

[[models]]
name = "gfs"
ingest_dir = "$ROOT/gfs"
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
ingest_dir = "$ROOT/hrrr"
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
  levels = ["height_above_ground:2"]
EOF

"$BIN" --config "$ROOT/config.toml" > "$ROOT/server.log" 2>&1 &
SRV=$!

ms_since() { echo $(( ($(date +%s%N) - T0) / 1000000)); }

# --- first queryable moment overall (/ready: >=1 run indexed per model) ----
READY=0
for i in $(seq 1 600); do
  sleep 0.1
  code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/ready" || echo 000)
  if [ "$code" = "200" ]; then READY=$(ms_since); break; fi
done
if [ "$READY" = "0" ]; then
  echo "FAIL: never became ready"; cat "$ROOT/server.log"; kill $SRV; exit 1
fi
echo "first-queryable (/ready): ${READY} ms after placement"

# --- per-(model,fh) visibility via inventory polling ------------------------
vis() { # vis <model> <fh-number> -> prints ms when forecast_hours list contains fh
  local mdl="$1" fh="$2" t=""
  for i in $(seq 1 600); do
    sleep 0.1
    if curl -s "$BASE/v1/inventory?model=$mdl" | grep -q "\"forecast_hours\":\[[^]]*\b${fh}\b"; then
      echo $(ms_since); return
    fi
  done
  echo FAIL
}
GFS0=$(vis gfs 0); GFS6=$(vis gfs 6)
H0=$(vis hrrr 0); H3=$(vis hrrr 3)
echo "gfs f000 visible: ${GFS0} ms | gfs f006: ${GFS6} ms"
echo "hrrr f000 visible: ${H0} ms | hrrr f003: ${H3} ms"

# peak RSS (VmHWM = lifetime peak RSS) + CPU time straight from /proc
PEAK_KB=$(awk '/VmHWM/{print $2}' /proc/$SRV/status 2>/dev/null)
CPU=$(awk '{print ($14+$15)/100}' /proc/$SRV/stat 2>/dev/null)
echo "peak RSS (VmHWM): ${PEAK_KB} kB (~$((PEAK_KB/1024)) MB) | server CPU so far: ${CPU}s"
grep -E "scan_round ingested" "$ROOT/server.log" | head -4 || true

kill $SRV 2>/dev/null
rm -rf "$ROOT"
