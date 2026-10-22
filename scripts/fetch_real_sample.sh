#!/usr/bin/env bash
# MANUAL, NOT PART OF CI. Optional helper to close the one honesty gap in README
# section 8: every automated test uses SYNTHETIC fixtures whose ground truth is
# wgrib2 reading those same files. This script pulls a SMALL slice of a REAL NWS
# product from the public `aws-nws` S3 bucket so the same probes can be replayed
# against production bytes. Requires network access; nothing else in this repo
# touches the network (spec 13: ingest only reads local directories).
#
#   scripts/fetch_real_sample.sh [MODEL]     # MODEL = gfs (default) | hrrr
#
# Downloads a RANGE-limited prefix (a truncated GRIB2 is legal input for wgrib2:
# it stops at the truncation) into tests/realsample/ (gitignored), prints the
# record list, and shows how to point a wxpoint config at it.
set -u
cd "$(dirname "$0")/.."

MODEL=${1:-gfs}
MAX_BYTES=${MAX_BYTES:-8388608}          # 8 MiB prefix keeps this a few-MBs affair
OUT=tests/realsample
WG=${WG:-tools/wgrib2-full}
mkdir -p "$OUT"

case "$MODEL" in
  gfs)
    # 1.5-degree GFS product: much smaller than 0.25-degree, same template family
    # (regular lat/lon, template 3.0, JPEG2000 packing 5.40). Pick a recent cycle
    # by hand -- YYYY-MM-DD and HH below.
    YYYY_MM_DD=${YYYY_MM_DD:-2026-10-19}
    HH=${HH:-12}
    url="https://storage.googleapis.com/noaa-gfs-bdp-pds/gfs/${YYYY_MM_DD}/${HH}/gfs.t${HH}z.pgrb2.1p50.f000"
    fn="gfs.t${HH}z.pgrb2.1p50.f000.partial.grib2"
    ;;
  hrrr)
    YYYY_MM_DD=${YYYY_MM_DD:-2026-10-19}
    HH=${HH:-00}
    url="https://storage.googleapis.com/noaa-hrrr-pds/hrrr/${YYYY_MM_DD}/${HH}/hrrr.t${HH}z.wrmaf.f000.grib2"
    fn="hrrr.t${HH}z.wrmaf.f000.partial.grib2"
    ;;
  *) echo "usage: $0 [gfs|hrrr]"; exit 2;;
esac

echo "fetching first ${MAX_BYTES} bytes of"
echo "  $url"
echo "  -> $OUT/$fn"
curl -sfL --max-time 300 -r "0-$((MAX_BYTES - 1))" -o "$OUT/$fn" "$url" || {
  echo "FATAL: download failed (no network? path moved upstream?). This script is"
  echo "       best-effort and manual; nothing in CI depends on it."
  exit 1
}
head -c 4 "$OUT/$fn" | grep -q GRIB || { echo "FATAL: no GRIB magic bytes"; exit 1; }
ls -l "$OUT/$fn"

if [ -x "$WG" ]; then
  echo "--- wgrib2 record list (first 40 lines; truncated file is tolerated) ---"
  "$WG" "$OUT/$fn" 2>/dev/null | head -40
  echo "--- data representation templates present ---"
  "$WG" "$OUT/$fn" -G '^[0-9]+:[0-9]+:' -d ':{0}:code table 5.0=' 2>/dev/null |
    sed 's/.*code table 5.0=\([0-9]*\).*/DRT 5.\1/' | sort | uniq -c | sort -rn | head
else
  echo "(no executable $WG here: build wgrib2-full or set WG=... to inspect records)"
fi

cat <<EOF

Next steps (manual): make a config whose [[models]] ingest_dir points at
$OUT with a file_glob matching the fetched name, start the server, then compare
/v1/point answers against '$WG -lon <lon> <lat>' on the same file, exactly like
scripts/validate_realistic.py does for the synthetic realistic dataset. A
truncated prefix only exercises the messages that fit inside it.
EOF
