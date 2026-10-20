#!/usr/bin/env bash
# Regenerate GRIB2 fixtures idempotently. Requires python3 and tools/wgrib2.
set -euo pipefail
cd "$(dirname "$0")/.."
WG=tools/wgrib2

python3 scripts/gen_grib2.py --out tests/fixtures --perf-out tests/perfdata

# Complex-packing (DRT 5.3, spatial differencing) variants produced BY wgrib2
# itself, so the decoder is exercised against wgrib2's own encoder output.
for base in gfs_small hrrr_small; do
  "$WG" "tests/fixtures/${base}.grib2" -set_grib_type c3 \
      -grib_out "tests/fixtures/${base}_c3.grib2" >/dev/null
  # every record must now report DRT 5.3 (complex + spatial differencing)
  if "$WG" "tests/fixtures/${base}_c3.grib2" -code_table_5.0 | grep -qv 'code table 5.0=3'; then
    echo "FATAL: c3 conversion left a non-complex record"; exit 1
  fi
done

# PDT 4.8 readback sanity: wgrib2 must recognize accumulation/average + range
"$WG" tests/fixtures/stat_small.grib2 | grep -q 'acc' || {
  echo "FATAL: wgrib2 did not recognize the accumulation message"; exit 1; }
"$WG" tests/fixtures/stat_small.grib2 | grep -q 'ave' || {
  echo "FATAL: wgrib2 did not recognize the average message"; exit 1; }

echo "gen_fixtures.sh: fixtures regenerated under tests/fixtures (+ tests/perfdata)"
