#!/usr/bin/env bash
# ============================================================================
# V1.3 phase E5 CONSOLIDATED REAL-DATA ACCEPTANCE — MANUAL / NETWORK ONLY.
# NEVER wired into CI (workspace tests skip cleanly without tests/realdata/,
# and nothing automated invokes this script; spec §13 keeps downloads out).
#
# What it does end to end against PRODUCTION BYTES:
#   1. scripts/fetch_real_sample.sh       (idempotent NOMADS .idx + Range fetch)
#   2. cargo build --release --bin wxpoint (incremental when fresh)
#   3. boots target/release/wxpoint on a spare port with a purpose-built
#      current-operational config (gfs sflux incl hybrid:* + hrrr incl the
#      raw d0c2n1 WIND triple), waits for /ready
#   4. runs scripts/realdata_probes.py: real wind rotation vs pyproj+wgrib2,
#      real time interpolation, production PDT 4.8 metadata + guards, live
#      hybrid query — every expectation derived INDEPENDENTLY of our Rust code
#   5. prints the sha256 of the fetched files (provenance)
# Exits non-zero on ANY failed pin. Server shutdown is trapped either way.
#
#   PORT=18099 scripts/realdata_acceptance.sh
#   GFS_YMD=... GFS_HH=... HRRR_YMD=... HRRR_HH=... pass through to the fetch.
# ============================================================================
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-18099}
ROOT=/tmp/opencode/wxpoint-e5-accept
OUT=tests/realdata

echo "== E5 real-data acceptance (MANUAL/NETWORK): port $PORT, workdir $ROOT =="

# --- 1. fetch (idempotent; keeps existing complete slices byte-stable) -----
bash scripts/fetch_real_sample.sh || { echo "FAIL: fetch"; exit 1; }
for req in gfs_sflux_f000.grib2 gfs_sflux_f006.grib2 gfs_sflux_f012.grib2 \
           gfs_tmp_f006.grib2 gfs_tmp_f012.grib2 hrrr_uv10m_f06.grib2 \
           hrrr_apcp_tight.grib2 hrrr_windmax_f06.grib2; do
    [ -s "$OUT/$req" ] || { echo "FAIL: required slice $OUT/$req missing"; exit 1; }
done

# --- 2. release binary (bounded by the caller; incremental if fresh) -------
echo "== building release wxpoint =="
cargo build --release --bin wxpoint || { echo "FAIL: release build"; exit 1; }

# --- 3. purpose-built operational config ------------------------------------
rm -rf "$ROOT"
mkdir -p "$ROOT/data"
CFG="$ROOT/config.toml"
cat > "$CFG" << EOF
[server]
bind = "127.0.0.1:$PORT"
data_dir = "$ROOT/data"

[ingest]
scan_interval_secs = 5
retention_runs = 4
stable_window_secs = 0

[[models]]
name = "gfs"
ingest_dir = "$OUT"
file_glob = "gfs_*.grib2"

  [[models.params]]
  name = "TMP"
  levels = ["height_above_ground:2", "hybrid:*"]

  [[models.params]]
  name = "UGRD"
  levels = ["height_above_ground:10"]

  [[models.params]]
  name = "VGRD"
  levels = ["height_above_ground:10"]

  [[models.params]]
  name = "HGT"
  levels = ["surface", "hybrid:*"]

  [[models.params]]
  discipline = 2
  category = 0
  number = 228
  units = "m/s"
  levels = ["surface"]

[[models]]
name = "hrrr"
ingest_dir = "$OUT"
file_glob = "hrrr_*.grib2"

  [[models.params]]
  name = "TMP"
  levels = ["height_above_ground:2"]

  [[models.params]]
  name = "UGRD"
  levels = ["height_above_ground:10"]

  [[models.params]]
  name = "VGRD"
  levels = ["height_above_ground:10"]

  [[models.params]]
  name = "APCP"
  levels = ["surface"]

  # WIND (Wind Speed) is OUTSIDE the built-in table: raw triple pinned from
  # the live bytes (discipline 0, category 2, number 1) with the gribtable
  # units m/s. PDT 4.8 max-over-window product -> also production stat proof.
  [[models.params]]
  discipline = 0
  category = 2
  number = 1
  units = "m/s"
  levels = ["height_above_ground:10"]
EOF

# --- 4. boot + wait /ready ---------------------------------------------------
SRV=""
cleanup() {
    if [ -n "$SRV" ]; then
        kill "$SRV" 2>/dev/null
        wait "$SRV" 2>/dev/null
    fi
}
trap cleanup EXIT
./target/release/wxpoint --config "$CFG" > "$ROOT/server.log" 2>&1 &
SRV=$!
echo "== server pid $SRV; waiting for /ready =="
READY=0
for k in $(seq 1 240); do
    if curl -sf "http://127.0.0.1:$PORT/ready" > /dev/null 2>&1; then
        READY=1
        break
    fi
    kill -0 "$SRV" 2>/dev/null || break
    sleep 2
done
if [ "$READY" != "1" ]; then
    echo "FAIL: /ready never came up (server log tail:)"
    tail -12 "$ROOT/server.log"
    exit 1
fi
echo "== ready after ${k} polls (~$((k * 2))s of ingest) =="

# --- 5. probes ----------------------------------------------------------------
python3 scripts/realdata_probes.py --base "http://127.0.0.1:$PORT"
RC=$?

# --- 6. provenance -------------------------------------------------------------
echo
echo "== sha256 of fetched real-data files =="
sha256sum "$OUT"/*.grib2

exit $RC
