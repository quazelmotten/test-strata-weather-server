#!/usr/bin/env bash
# wxpoint smoke test: boots the RELEASE binary against the GFS perf fixture,
# curls every endpoint family, checks fixture-truth values, and prints the
# release-build average point-query latency from /metrics (sum/count, us).
# Run from the repo root:  ./scripts/smoke.sh
set -u

ROOT=/tmp/opencode/wxpoint-smoke
BIN=target/release/wxpoint
BASE=http://127.0.0.1:8091
PASS=1

echo "== building release binary =="
cargo build --release -p wx-server || exit 1

rm -rf "$ROOT"
mkdir -p "$ROOT/ing"
cp tests/perfdata/gfs_perf_f000.grib2 "$ROOT/ing/"

cat > "$ROOT/config.toml" <<EOF
[server]
bind = "127.0.0.1:8091"
data_dir = "$ROOT/data"

[ingest]
scan_interval_secs = 2
retention_runs = 4
stable_window_secs = 0

[[models]]
name = "gfs"
ingest_dir = "$ROOT/ing"
file_glob = "gfs_perf_*.grib2"

  [[models.params]]
  name = "TMP"
  levels = ["isobaric:*"]
EOF

echo "== starting server =="
"$BIN" --config "$ROOT/config.toml" > "$ROOT/server.log" 2>&1 &
SRV=$!

ready=0
for i in $(seq 1 30); do
  sleep 1
  code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/ready" || echo 000)
  if [ "$code" = "200" ]; then ready=1; break; fi
done
if [ "$ready" != "1" ]; then
  echo "FAIL: never became ready"; cat "$ROOT/server.log"; kill $SRV; exit 1
fi
echo "ready after ${i}s (ingest visible forecast-hour-by-forecast-hour)"

check() { # check <label> <expected_http_code> <body_substring>
  local label="$1" want="$2" pat="$3" url="$4"
  local body code
  body=$(curl -s -w '\n%{http_code}' "$BASE$url")
  code=$(printf '%s' "$body" | tail -n1)
  body=$(printf '%s' "$body" | sed '$d')
  if [ "$code" = "$want" ] && printf '%s' "$body" | grep -q -- "$pat"; then
    echo "ok   [$label] http=$code"
  else
    echo "FAIL [$label] got http=$code body=$body"; PASS=0
  fi
}

# --- run time straight from the inventory endpoint (never from filenames) ---
RUN=$(curl -s "$BASE/v1/inventory?model=gfs" | grep -o '"run":"[^"]*"' | head -1 | cut -d'"' -f4)
echo "indexed run: $RUN"

# --- truth queries (fixture ground truth, §9.1/§9.5) -----------------------
# lat 0.125 sits exactly on a grid row -> exact-node value 609.0 at lon 359.9
check "point seam"     200 '"value":609' \
  "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=0.125&lon=359.9&valid=${RUN}"
check "point midcell"  200 '"value":609.5' \
  "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=0.0&lon=359.9&valid=${RUN}"
check "negative lon"   200 '"value":609' \
  "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=0.125&lon=-0.1&valid=${RUN}"

# --- error tokens (§7 status classes) --------------------------------------
check "unknown model"  404 'unknown_model' \
  "/v1/point?model=nope&param=TMP&level=isobaric:1000&lat=0&lon=0&valid=${RUN}"
check "bad level"      422 'level_out_of_range' \
  "/v1/point?model=gfs&param=TMP&level=isobaric:1&lat=0&lon=0&valid=${RUN}"

# --- meta endpoints ---------------------------------------------------------
check "models"    200 '"grid"'       "/v1/models"
check "inventory" 200 '"forecast_hours":\[' "/v1/inventory?model=gfs"
check "health"    200 '"ok":true'    "/health"
check "ready"     200 '"ready":true' "/ready"

# --- hammer queries, then read release latency off /metrics ----------------
N=400
for i in $(seq 1 $N); do
  curl -s -o /dev/null "$BASE/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=0.125&lon=359.9&valid=${RUN}"
done
M=$(curl -s "$BASE/metrics")
SUM=$(printf '%s' "$M" | grep '^wxpoint_query_latency_us_sum ' | awk '{print $2}')
CNT=$(printf '%s' "$M" | grep '^wxpoint_query_latency_us_count ' | awk '{print $2}')
echo "release avg point-query latency: $(awk -v s="$SUM" -v c="$CNT" 'BEGIN{printf "%.1f us (%d queries)", s/c, c}')"

kill $SRV 2>/dev/null
rm -rf "$ROOT"

if [ "$PASS" = "1" ]; then echo "SMOKE PASS"; else echo "SMOKE FAIL"; exit 1; fi
