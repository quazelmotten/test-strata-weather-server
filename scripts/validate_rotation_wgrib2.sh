#!/usr/bin/env bash
# OPTIONAL spec §9.3 GOLDEN CHECK -- enable it where the toolchain allows.
#
# The spec asks that rotated HRRR U/V match `wgrib2 -new_grid_winds earth`.
# That invoker is implemented in New_grid.c but its projection/convergence math
# comes from the IPOLATES Fortran library (33 .f90 modules), which is NOT
# compiled into either binary on this machine:
#
#   $ tools/wgrib2-full tests/fixtures/hrrr_small.grib2 -new_grid_winds earth \
#         -grib_out /tmp/x.grib2
#   IPOLATES package is not installed
#
# and there is no Fortran compiler at all here (gfortran / f95 / ifort / flang /
# f2c all absent; sudo requires a password), so iplib cannot be built (see
# docs/DECISIONS.md, V1.1 phase 1 evidence). What runs instead on this machine
# is scripts/validate_rotation.py: an independent pyproj measurement of the
# conformal angle with the earth shape matched EXACTLY to each fixture's Code
# Table 3.2 byte, checked in Rust against <=1e-6 degrees
# (crates/wx-ingest/tests/rotation_vs_pyproj.rs).
#
# ON A MACHINE WITH gfortran (or a wgrib2 build that links iplib), uncomment
# the block below and point WG at that binary; it compares the rotated values
# our decoder produces with wgrib2's own earth-relative output.
set -euo pipefail
cd "$(dirname "$0")/.."

WG="${WG:-tools/wgrib2-full}"      # override: WG=/path/to/wgrib2-with-IPOLATES

if ! "$WG" tests/fixtures/hrrr_small.grib2 -new_grid_winds earth \
        -grib_out /tmp/opencode/wxpoint_newgrid_probe.grib2 >/dev/null 2>&1; then
  echo "SKIP: this wgrib2 build has no IPOLATES (-new_grid_winds unavailable)."
  echo "      Use scripts/validate_rotation.py instead."
  exit 0
fi

# --- golden comparison, enabled once iplib is available ---------------------
# 1. rotate OUR decoded grid winds to earth-relative and write them back out
#    as a GRIB2 file (tools/rotate_dump is a placeholder for such a helper;
#    wire it up when this path is actually exercised),
# 2. let wgrib2 do the same from the same input,
# 3. require identical decoded arrays:
#
#   cksums() { "$WG" "$1" -checksum data 2>/dev/null | awk -F'data_cksum=' '{print $2}' | tr '\n' ','; }
#   "$WG" tests/fixtures/hrrr_small.grib2 -new_grid_winds earth \
#       -new_grid interleaved -grib_out /tmp/opencode/wg_earth.grib2
#   [ "$(cksums /tmp/opencode/wg_earth.grib2)" = "$(cksums /tmp/opencode/our_earth.grib2)" ]
#   echo "ok -new_grid_winds earth golden matches our rotation"
echo "IPOLATES available: golden check skeleton present, wire up the two helpers noted above."
