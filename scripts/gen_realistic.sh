#!/usr/bin/env bash
# Generate + validate the PRODUCTION-SCALE realistic GFS-like dataset used by
# scripts/ingestbench.sh (V1.2 phase D). ~30 s, ~1.1 GB under tests/perfdata/
# (gitignored). See the header comment of the `--real-out` block in
# scripts/gen_grib2.py for the exact geometry and why it deviates from "~40
# levels" (37 = every 25 hPa 1000..100, a superset of the real NWS 25-level stack).
set -u
cd "$(dirname "$0")/.."
python3 scripts/gen_grib2.py --out tests/fixtures --real-out tests/perfdata/realistic
python3 scripts/validate_realistic.py tests/perfdata/realistic
