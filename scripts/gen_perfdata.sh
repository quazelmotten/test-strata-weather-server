#!/usr/bin/env bash
# Regenerate the milestone-5 perf dataset (gitignored tests/perfdata/) and
# validate it against tools/wgrib2 (spec §9.1/§9.2 re-run at full grid size),
# then emit tests/perfdata/probes.tsv consumed by smoke.sh + loadtest.sh.
# Requires python3(+numpy) and tools/wgrib2. Idempotent.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 scripts/gen_grib2.py --out tests/fixtures --perf-out tests/perfdata
python3 scripts/validate_perf.py

echo "== perf dataset =="
ls -la tests/perfdata/
