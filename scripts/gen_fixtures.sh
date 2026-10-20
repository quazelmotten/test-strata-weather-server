#!/usr/bin/env bash
# Regenerate GRIB2 fixtures idempotently. Requires only python3.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 scripts/gen_grib2.py --out tests/fixtures --perf-out tests/perfdata
echo "gen_fixtures.sh: fixtures regenerated under tests/fixtures (+ tests/perfdata)"
