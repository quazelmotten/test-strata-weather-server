#!/usr/bin/env bash
# Milestone-5 load test (spec §8): sustained >=50k req/s on 8 cores against
# the RELEASE binary serving the full perf dataset, with SERVER-SIDE p50/p99
# read from the /metrics histogram (delta around the run) and client-side
# numbers from oha. Set NATIVE=1 to rebuild with -C target-cpu=native and
# re-measure (documented comparison; shipped build stays portable).
#
# Usage: ./scripts/loadtest.sh [seconds]     (default 20)
#        NATIVE=1 ./scripts/loadtest.sh 20
set -u
cd "$(dirname "$0")/.."
BIN=target/release/wxpoint
DUR=${1:-20}
BASE=http://127.0.0.1:8093
ROOT=/tmp/opencode/wxpoint-loadtest
OHA=${OHA:-tools/oha}
PASS=1

if [ "${NATIVE:-0}" = "1" ]; then
  echo "== rebuilding with target-cpu=native (portable build unaffected) =="
  RUSTFLAGS="-C target-cpu=native" CARGO_TARGET_DIR=target-native \
    cargo build --release -p wx-server || exit 1
  BIN=target-native/release/wxpoint
fi
[ -x "$BIN" ] || { echo "building release binary..."; cargo build --release -p wx-server; }

rm -rf "$ROOT"
mkdir -p "$ROOT/gfs" "$ROOT/hrrr"
cp tests/perfdata/gfs_perf_f00*.grib2 "$ROOT/gfs/"
cp tests/perfdata/hrrr_perf_f00*.grib2 "$ROOT/hrrr/"
touch "$ROOT"/gfs/*.grib2 "$ROOT"/hrrr/*.grib2

cat > "$ROOT/config.toml" <<EOF
[server]
bind = "127.0.0.1:8093"
data_dir = "$ROOT/data"

[ingest]
scan_interval_secs = 1
retention_runs = 4
stable_window_secs = 0
ingest_threads = 16

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

ready=0
for i in $(seq 1 300); do
  sleep 0.2
  code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/ready" || echo 000)
  if [ "$code" = "200" ]; then ready=1; break; fi
done
if [ "$ready" != "1" ]; then
  echo "FAIL: never became ready"; cat "$ROOT/server.log"; kill $SRV; exit 1
fi
echo "ready (perf dataset indexed)"

RUN="2026-10-19T12:00:00Z"   # from GRIB sec1, not filenames
# --- mixed query mix (spec §8 load-test mix) -------------------------------
U1="$BASE/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=39.875&lon=25.0&valid=$RUN"
U2="$BASE/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=0.125&lon=359.9&valid=$RUN"
U3="$BASE/v1/point?model=gfs&param=TMP&level=isobaric:775&lat=39.875&lon=25.0&valid=$RUN"
U4="$BASE/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=39.875&lon=25.0&valid=2026-10-19T15:00:00Z"
U5="$BASE/v1/point?model=gfs&param=TMP,HGT&level=isobaric:850&lat=39.875&lon=25.0&valid=$RUN"
U6="$BASE/v1/point?model=hrrr&param=TMP&level=isobaric:850&lat=35.009136&lon=-96.979227&valid=$RUN"
U7="$BASE/v1/point?model=nope&param=TMP&level=isobaric:1000&lat=0&lon=0&valid=$RUN"
U8="$BASE/v1/point?model=gfs&param=TMP&level=isobaric:1&lat=39.875&lon=25.0&valid=$RUN"

# sanity: every URL answers with its expected class before hammering
expect=(200 200 200 200 200 200 404 422)
idx=0
for u in "$U1" "$U2" "$U3" "$U4" "$U5" "$U6" "$U7" "$U8"; do
  code=$(curl -s -o /dev/null -w '%{http_code}' "$u")
  if [ "$code" != "${expect[idx]}" ]; then
    echo "FAIL: url idx $idx status $code != ${expect[idx]}"; PASS=0
  fi
  idx=$((idx+1))
done
[ "$PASS" = "1" ] || { kill $SRV; rm -rf "$ROOT"; exit 1; }

# --- metrics snapshot BEFORE ----------------------------------------------
curl -s "$BASE/metrics" > "$ROOT/metrics.before"

printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n' "$U1" "$U2" "$U3" "$U4" "$U5" "$U6" "$U7" "$U8" > "$ROOT/urls.txt"
echo "== oha: --worker-threads 8 (8 cores per spec §8) -c 64, keep-alive, ${DUR}s, 8-URL mixed mix =="
"$OHA" --worker-threads 8 -c 64 -z "${DUR}s" --no-tui --urls-from-file "$ROOT/urls.txt" | tee "$ROOT/oha.out"

# --- metrics snapshot AFTER + server-side percentiles ----------------------
curl -s "$BASE/metrics" > "$ROOT/metrics.after"
python3 - <<'EOF'
import re, sys
def buckets(fn):
    out = []
    cnt = ssum = 0
    for ln in open(fn):
        m = re.match(r'wxpoint_query_latency_us_bucket\{le="(\d+|\+inf)"\}\s*(\d+)', ln)
        if m:
            out.append((m.group(1), int(m.group(2))))
        m2 = re.match(r'wxpoint_query_latency_us_(count|sum) (\d+)', ln)
        if m2:
            if m2.group(1) == "count": cnt = int(m2.group(2))
            else: ssum = int(m2.group(2))
    return out, cnt, ssum
b0, c0, s0 = buckets("/tmp/opencode/wxpoint-loadtest/metrics.before")
b1, c1, s1 = buckets("/tmp/opencode/wxpoint-loadtest/metrics.after")
n = c1 - c0
if n == 0:
    print("no point queries recorded"); sys.exit(1)
deltas = [(le, b1[k][1] - b0[k][1]) for k, (le, _) in enumerate(b1)]
# emitted bucket lines are ALREADY cumulative -> delta of two snapshots is too
p50 = p99 = None
for le, d in deltas:
    lim = le if le != "+inf" else 10 ** 9
    if p50 is None and d >= n * 0.5: p50 = lim
    if p99 is None and d >= n * 0.99: p99 = lim
mean = (s1 - s0) / n
print(f"server-side (histogram delta over run): n={n}")
print(f"  mean={mean:.1f} us  p50<={p50} us  p99<={p99} us")
ok50 = p50 is not None and float(p50) <= 50.0
ok99 = p99 is not None and float(p99) <= 250.0
print(f"  spec targets: p50<50us {'MET' if ok50 else 'MISS'};"
      f" p99<250us {'MET' if ok99 else 'MISS'}")
EOF

kill $SRV 2>/dev/null
rm -rf "$ROOT"
