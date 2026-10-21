#!/usr/bin/env bash
# Regenerate GRIB2 fixtures idempotently. Requires python3 and tools/wgrib2-full
# (the full-codec build: jasper/openjpeg JPEG2000 + libpng decode + libaec encode,
# see docs/DECISIONS.md). Every packing twin is validated two ways:
#   1. wgrib2-full reports the expected DRT code for every record, and
#   2. `-checksum data` (POSIX cksum over the decoded float array) is IDENTICAL
#      to the simple-packed original's, proving the repack changed nothing but
#      the encoding.
set -euo pipefail
cd "$(dirname "$0")/.."
WG=tools/wgrib2-full
"$WG" -version >/dev/null 2>&1 || true   # not all builds have -version; ignore

python3 scripts/gen_grib2.py --out tests/fixtures --perf-out tests/perfdata

# ---------------------------------------------------------------------------
# packing twins: same values, different section-7 representation
# ---------------------------------------------------------------------------
cksums() { "$WG" "$1" -checksum data 2>/dev/null | awk -F'data_cksum=' '{print $2}' | tr '\n' ','; }

check_twin() {
  local base="$1" drt="$2" tag="$3"
  local f="tests/fixtures/${base}_${tag}.grib2"
  # every record must carry the requested data representation template
  bad=$("$WG" "$f" -code_table_5.0 | { grep -vc "code table 5.0=${drt} " || true; })
  if [ "${bad}" != "0" ]; then
    echo "FATAL: ${f}: ${bad} record(s) not DRT 5.${drt}"; exit 1
  fi
  local a b
  a="$(cksums "tests/fixtures/${base}.grib2")"
  b="$(cksums "$f")"
  if [ "$a" != "$b" ]; then
    echo "FATAL: ${f} decoded arrays differ from tests/fixtures/${base}.grib2"; exit 1
  fi
  echo "ok ${base}_${tag} (DRT 5.${drt}, checksum-equal)"
}

for base in gfs_small hrrr_small; do
  # complex packing + spatial differencing (DRT 5.3), written by wgrib2 itself
  "$WG" "tests/fixtures/${base}.grib2" -set_grib_type c3 \
      -grib_out "tests/fixtures/${base}_c3.grib2" >/dev/null
  check_twin "$base" 3 c3
  # JPEG2000 (DRT 5.40)
  "$WG" "tests/fixtures/${base}.grib2" -set_grib_type jpeg \
      -grib_out "tests/fixtures/${base}_jpeg.grib2" >/dev/null
  check_twin "$base" 40 jpeg
  # CCSDS AEC (DRT 5.42)
  "$WG" "tests/fixtures/${base}.grib2" -set_grib_type aec \
      -grib_out "tests/fixtures/${base}_aec.grib2" >/dev/null
  check_twin "$base" 42 aec
  # IEEE raw float32 (DRT 5.4)
  "$WG" "tests/fixtures/${base}.grib2" -set_grib_type ieee \
      -grib_out "tests/fixtures/${base}_ieee.grib2" >/dev/null
  check_twin "$base" 4 ieee
done

# PNG (DRT 5.41): wgrib2 has no PNG encoder (enc_png lives in g2clib), so the
# twins come from scripts/make_png_twin.py; wgrib2-full reads them back with
# identical values. `cycle` rotates the five PNG filter types across messages.
for base in gfs_small hrrr_small; do
  python3 scripts/make_png_twin.py "tests/fixtures/${base}.grib2" \
      "tests/fixtures/${base}_png.grib2" none >/dev/null
  check_twin "$base" 41 png
  python3 scripts/make_png_twin.py "tests/fixtures/${base}.grib2" \
      "tests/fixtures/${base}_png_f.grib2" cycle >/dev/null
  check_twin "$base" 41 png_f
done

# PDT 4.8 statistically-processed fixture x JPEG2000 packing (spec \u00a74.2:
# template 4.8 must survive alongside compressed representations).
"$WG" tests/fixtures/stat_small.grib2 -set_grib_type jpeg \
    -grib_out tests/fixtures/stat_small_jpeg.grib2 >/dev/null
check_twin stat_small 40 jpeg

# PDT 4.8 readback sanity: wgrib2 must recognize accumulation/average + range
"$WG" tests/fixtures/stat_small.grib2 | grep -q 'acc' || {
  echo "FATAL: wgrib2 did not recognize the accumulation message"; exit 1; }
"$WG" tests/fixtures/stat_small.grib2 | grep -q 'ave' || {
  echo "FATAL: wgrib2 did not recognize the average message"; exit 1; }

# The unsupported-template guard fixture declares DRT 5.1 (dictionary packing),
# which nothing in the wgrib2 ecosystem can decode. A bare listing never touches
# section 7, so the guard has to force a decode (`-checksum data`).
if "$WG" tests/fixtures/unsupported_drt.grib2 -checksum data >/dev/null 2>&1; then
  echo "FATAL: wgrib2 unexpectedly read the dictionary-packed guard fixture"; exit 1
fi
echo "ok unsupported_drt (DRT 5.1 rejected by wgrib2-full)"

# ---------------------------------------------------------------------------
# perf-scale JPEG twin (gitignored dir): full GFS geometry, one forecast hour
# ---------------------------------------------------------------------------
if [ -f tests/perfdata/gfs_perf_f000.grib2 ]; then
  "$WG" tests/perfdata/gfs_perf_f000.grib2 -set_grib_type jpeg \
      -grib_out tests/perfdata/gfs_perf_f000_jpeg.grib2 >/dev/null
  a="$(cksums tests/perfdata/gfs_perf_f000.grib2)"
  b="$(cksums tests/perfdata/gfs_perf_f000_jpeg.grib2)"
  if [ "$a" != "$b" ]; then
    echo "FATAL: perf jpeg twin differs from gfs_perf_f000.grib2"; exit 1
  fi
  echo "ok gfs_perf_f000_jpeg (DRT 5.40, checksum-equal)"
fi

echo "gen_fixtures.sh: fixtures regenerated under tests/fixtures (+ tests/perfdata)"
