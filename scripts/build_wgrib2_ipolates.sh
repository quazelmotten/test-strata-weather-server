#!/usr/bin/env bash
# Build wgrib2 linked against IPOLATES (USE_IPOLATES=3) WITHOUT root, and use it
# to (re)generate the spec §9.3 earth-relative wind golden.
#
# WHAT THIS IS FOR. The committed golden tests/fixtures/hrrr_ipolates_earth.grib2
# (+ .PROVENANCE.txt sidecar) was produced by exactly such a binary during V1.4
# phase X (docs/DECISIONS.md §21): `wgrib2 -new_grid_winds earth` only exists
# when wgrib2 links the Fortran IPOLATES package, and the stock `tools/wgrib2*`
# builds this repo uses elsewhere do NOT. THE TESTS NEED NO REBUILD — the golden
# is committed and runs offline. This script is OPTIONAL, only needed to
# REGENERATE the golden (or reproduce the rotation proof on your own bytes).
#
# REQUIREMENTS: network (apt-get DOWNLOADS packages, installs nothing), gcc-13
# already present system-wide, and the repo's local wgrib2 source tree at
# tools/wgrib2-full-src (gitignored; see scripts/gen_fixtures.sh header for the
# sibling codec builds). No sudo is ever invoked — debs are EXTRACTED with
# dpkg-deb into a merged fakeroot, the recipe proven on Ubuntu 24.04 without
# root (Agent G, 2026-10-24; adapted from /tmp/opencode/ipolates/repro.sh and
# the "build gotchas" section of /tmp/opencode/ipolates/RESULT.md §1).
#
# OUTPUT PATH CONVENTION (matches RESULT.md): everything under
# ${IPOLATES_WORK:-/tmp/opencode/ipolates}: fakeroot/ (toolchain), src/ (source
# copy), wgrib2-ipolates (the binary). Repo paths stay relative to this script.
set -euo pipefail
cd "$(dirname "$0")/.."                       # repo root

WORK="${IPOLATES_WORK:-/tmp/opencode/ipolates}"
FR="$WORK/fakeroot"                           # merged gcc+gfortran tree
SRC="$WORK/src"                               # working copy of the source tree
ORIG="$WORK/fnlist-orig"                      # pristine fnlist backup (gotcha B)
WG="$WORK/wgrib2-ipolates"                    # the output binary
export PATH="$FR/usr/bin:$PATH"
# GOTCHA C: link-time -lgfortran/-lquadmath resolve from libgfortran-13-dev
# INSIDE the fakeroot via LIBRARY_PATH (the .so.5 runtime libs themselves are
# already installed system-wide on any box with gcc-13).
export LIBRARY_PATH="$FR/usr/lib/gcc/x86_64-linux-gnu/13"
mkdir -p "$WORK"

# ---------------------------------------------------------------------------
# STEP 0 — toolchain without root (idempotent: skipped once gfortran works).
# GOTCHA A (Ubuntu noble): plain `gfortran-13` is a 14 kB DRIVER STUB; the real
# compiler lives in the -x86-64-linux-gnu subpackages, so download ALL of them.
# The extracted driver resolves cc1/cc1plus/f951 and crt*.o RELATIVE TO ITS OWN
# LOCATION, which is why the fakeroot must FIRST be seeded with COPIES OF THE
# SYSTEM gcc-13 trees (identical version 13.3.0-6ubuntu2~24.04.1); f951 alone
# gave "-fuse-linker-plugin, but liblto_plugin.so not found", and crt*.o live
# only in the system gcc dir. Merging both trees is mandatory, not optional.
# ---------------------------------------------------------------------------
if ! "$FR"/usr/bin/gfortran --version >/dev/null 2>&1; then
  mkdir -p "$WORK/debs" "$FR"
  ( cd "$WORK/debs"
    apt-get download gfortran-13 gfortran-13-x86-64-linux-gnu \
        libgfortran-13-dev libgfortran5 libquadmath0 cpp-13 )   # NETWORK
  cp -a /usr/libexec/gcc/x86_64-linux-gnu/13 "$FR/usr/libexec/gcc/x86_64-linux-gnu/" 2>/dev/null \
      || { mkdir -p "$FR/usr/libexec/gcc/x86_64-linux-gnu"; \
           cp -a /usr/libexec/gcc/x86_64-linux-gnu/13 "$FR/usr/libexec/gcc/x86_64-linux-gnu/"; }
  mkdir -p "$FR/usr/lib/gcc/x86_64-linux-gnu" "$FR/usr/bin"
  cp -a /usr/lib/gcc/x86_64-linux-gnu/13 "$FR/usr/lib/gcc/x86_64-linux-gnu/"
  cp -a /usr/bin/gcc-13 "$FR/usr/bin/"
  for d in "$WORK"/debs/*.deb; do dpkg-deb -x "$d" "$FR"; done
fi
"$FR"/usr/bin/gfortran --version | head -1     # loud abort if the merge failed

# ---------------------------------------------------------------------------
# STEP 1 — source tree + config flags. The shipped tools/wgrib2-full-src tree
# carries USE_IPOLATES=3 by stock default already; we set the companion flags
# explicitly anyway (only jasper/png/zlib/aec/gctpc are prebuilt here):
#   USE_IPOLATES=3 USE_SPECTRAL=0 MAKE_FTN_API=0 USE_NETCDF3=0 USE_PROJ4=0
#   MAKE_SHARED_LIB=0 USE_JASPER=1 USE_OPENJPEG=0
# ---------------------------------------------------------------------------
[ -d "$SRC" ] || { [ -d tools/wgrib2-full-src ] || {
    echo "FATAL: no source tree at \$SRC ($SRC) or tools/wgrib2-full-src"; exit 1; }
  cp -a tools/wgrib2-full-src "$SRC"; }
MF="$SRC/makefile"
sed -ri 's/^USE_IPOLATES = .*/USE_IPOLATES = 3/; s/^USE_SPECTRAL = .*/USE_SPECTRAL = 0/;
         s/^MAKE_FTN_API = .*/MAKE_FTN_API = 0/; s/^USE_NETCDF3 = .*/USE_NETCDF3 = 0/;
         s/^USE_PROJ4 = .*/USE_PROJ4 = 0/; s/^MAKE_SHARED_LIB = .*/MAKE_SHARED_LIB = 0/;
         s/^USE_JASPER = .*/USE_JASPER = 1/; s/^USE_OPENJPEG = .*/USE_OPENJPEG = 0/' "$MF"

# ---------------------------------------------------------------------------
# STEP 2 — ip2lib_d MUST be built SERIALLY FIRST. GOTCHA D: it has NO Fortran
# module dependency ordering, so `make -j48` dies on "Cannot open module file
# 'gdswzd_mod.mod'". Use the exact exported flags the top makefile uses for the
# double-precision path, then hand the archive to the main build.
# ---------------------------------------------------------------------------
( cd "$SRC/grib2/ip2lib_d"
  FFLAGS="-c -O3 -fopenmp" FTN_REAL8="-fdefault-real-8 -fdefault-double-8" \
      make FC=gfortran AR=ar ARFLAGS=crvs )
cp "$SRC/grib2/ip2lib_d/libip2_d.a" "$SRC/grib2/lib/"

# ---------------------------------------------------------------------------
# STEP 3 — stale-object wipe + pristine fnlist restore. GOTCHA E (both hit
# live): the shipped tree contains .o files from its original USE_IPOLATES=0
# build, and wgrib2/makefile SUFFIX RULES DO NOT LIST config.h as an object
# prerequisite — merely editing the makefile relinks STALE objects and the
# binary still prints "IPOLATES package is not installed". Separately,
# ./function.sh regenerates fnlist.c/h once per build, and a DOUBLE RUN
# duplicates nfunctions ("redefinition of 'nfunctions'" compile error), so
# restore the pristine pair before EVERY rebuild (backed up on first run).
# ---------------------------------------------------------------------------
rm -f "$SRC"/grib2/wgrib2/*.o
if [ ! -d "$ORIG" ]; then
  mkdir -p "$ORIG"
  cp "$SRC/grib2/wgrib2/fnlist.c" "$SRC/grib2/wgrib2/fnlist.h" "$ORIG/"
fi
cp "$ORIG/fnlist.c" "$ORIG/fnlist.h" "$SRC/grib2/wgrib2/"

# ---------------------------------------------------------------------------
# STEP 4 — build + install to the RESULT.md output-path convention.
# ---------------------------------------------------------------------------
( cd "$SRC/grib2" && make -j"$(nproc)" CC=gcc FC=gfortran )
cp "$SRC/grib2/wgrib2/wgrib2" "$WG"
chmod +x "$WG"

# Verification pins (from RESULT.md §"VERDICT"): no "not installed" string,
# gdswzd symbols present, all five -new_grid* options listed (fork quirk:
# `-help` REQUIRES an argument; use `-help all`).
if strings "$WG" | grep -q "IPOLATES package is not installed"; then
  echo "FATAL: binary still says IPOLATES is not installed — gotcha E bit you"; exit 1; fi
nm "$WG" | grep -q gdswzd
"$WG" -help all 2>&1 | grep -q new_grid
"$WG" tests/fixtures/hrrr_small.grib2 >/dev/null          # decode smoke

# ---------------------------------------------------------------------------
# STEP 5 (optional, REGEN=1) — regenerate the golden. Method (identical to the
# PROVENANCE sidecar): U,V messages must be ADJACENT for New_grid.c to pair
# them as one vector, then an IDENTITY-resolution Lambert -> identical-Lambert
# neighbor regrid rotates every node in place with no interpolation smearing.
# NOTE (fork quirks that cost someone an afternoon): `-new_grid`'s FOURTH
# argument IS the output file; display invokers on trimmed fixtures print RAW
# packed values, so compare the FILE (decoded), never a text dump; and
# `-lon` reports "outside of domain" for these lambert fixtures in BOTH builds
# (pre-existing fork behavior, not an IPOLATES regression).
# The REAL-bytes goldens (tests/realdata/hrrr_real_uv.grib2 ->
# hrrr_real_earth.grib2) additionally need the fetched real input, which
# scripts/fetch_real_sample.sh does NOT produce — see .PROVENANCE.txt for the
# .idx+HTTP-range extraction that did.
# ---------------------------------------------------------------------------
if [ "${REGEN:-0}" = "1" ]; then
  "$WG" tests/fixtures/hrrr_small.grib2 -d 3 -grib_out "$WORK/m3.grib2"   # UGRD fh0
  "$WG" tests/fixtures/hrrr_small.grib2 -d 5 -grib_out "$WORK/m5.grib2"   # VGRD fh0
  "$WG" tests/fixtures/hrrr_small.grib2 -d 4 -grib_out "$WORK/m4.grib2"   # UGRD fh3
  "$WG" tests/fixtures/hrrr_small.grib2 -d 6 -grib_out "$WORK/m6.grib2"   # VGRD fh3
  cat "$WORK/m3.grib2" "$WORK/m5.grib2" "$WORK/m4.grib2" "$WORK/m6.grib2" > "$WORK/uv_in.grib2"
  "$WG" "$WORK/uv_in.grib2" -new_grid_winds earth -new_grid_interpolation neighbor \
      -new_grid lambert:229.53:26.5:35.0:26.5 233.44:61:3000. \
                21.11389:41:-3000. "$WORK/hrrr_earth.grib2"
  echo "golden candidate: $WORK/hrrr_earth.grib2"
  echo "install as tests/fixtures/hrrr_ipolates_earth.grib2 ONLY after" \
       "cargo test -p wx-ingest --test rotation_vs_ipolates stays green and the" \
       ".PROVENANCE.txt sidecar is updated (date, wgrib2 -version line)."
  if [ -f "$WORK/hrrr_real_uv.grib2" ]; then
    "$WG" "$WORK/hrrr_real_uv.grib2" -new_grid_winds earth \
        -new_grid_interpolation neighbor \
        -new_grid lambert:262.5:38.5:38.5:38.5 237.280472:1799:3000. \
                  21.138123:1059:3000. "$WORK/hrrr_real_earth.grib2"
    echo "real-bytes golden: $WORK/hrrr_real_earth.grib2 — install together with" \
         "hrrr_real_uv.grib2 under gitignored tests/realdata/ to activate the" \
         "hrrr_rotation_matches_ipolates_golden_real_bytes gate."
  else
    echo "note: $WORK/hrrr_real_uv.grib2 absent — real-bytes gate will skip cleanly" \
         "(fetch_real_sample.sh does NOT create this input; see .PROVENANCE.txt)"
  fi
fi

echo "ok: $WG"
