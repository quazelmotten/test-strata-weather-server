#!/usr/bin/env bash
# ============================================================================
# MANUAL, NETWORK — NOT PART OF CI.
#
# Fetches SMALL, targeted slices of REAL NWS model output so the point-query
# server can be validated against production bytes (spec §9 ground truth),
# not just the synthetic fixtures whose correctness bar is "wgrib2 agrees with
# us about our own generated files".
#
# Method: every NOMADS file has a sibling `.idx` giving the byte offset of
# each GRIB message. We download ONLY the .idx, pick the messages we care
# about by parameter/level, and HTTP-Range-extract exactly those byte spans
# (a few MB total instead of hundreds). Each extracted file is a legal GRIB2
# stream because messages are self-delimiting.
#
#   scripts/fetch_real_sample.sh            # fetch everything below
#   KEEP=1 scripts/fetch_real_sample.sh     # keep raw .idx files for debugging
#
# Environment overrides (defaults = the Oct 22 2026 live-fire configuration):
#   GFS_YMD=20261021 GFS_HH=12   HRRR_YMD=20261022 HRRR_HH=00
#   BASE=https://nomads.ncep.noaa.gov/pub/data/nccf/com
#
# Outputs land in tests/realdata/ (GITIGNORED — fetched data is NEVER
# committed). The script is IDEMPOTENT: an existing output that starts with
# GRIB, ends with 7777, and is non-empty is verified and left alone.
#
# crates/wx-ingest/tests/realdata_gate.rs consumes this directory and SKIPS
# cleanly (with a printed reason) whenever it is absent, so CI never touches
# the network while a developer box can pin live-byte behaviour on demand.
# Nothing else in this repo touches the network (spec §13).
# ============================================================================
set -u
cd "$(dirname "$0")/.."

BASE=${BASE:-https://nomads.ncep.noaa.gov/pub/data/nccf/com}
GFS_YMD=${GFS_YMD:-$(date -u -d 'yesterday' +%Y%m%d 2>/dev/null || echo 20261021)}
GFS_HH=${GFS_HH:-12}
HRRR_YMD=${HRRR_YMD:-$(date -u +%Y%m%d 2>/dev/null || echo 20261022)}
HRRR_HH=${HRRR_HH:-00}
OUT=tests/realdata
WG=${WG:-tools/wgrib2-full}
CURL_TIMEOUT=${CURL_TIMEOUT:-120}
mkdir -p "$OUT"

GFS_DIR="$BASE/gfs/prod/gfs.$GFS_YMD/$GFS_HH/atmos"
HRRR_DIR="$BASE/hrrr/prod/hrrr.$HRRR_YMD/conus"
# Product stem on the nccf/com mirror as of Oct 2026 is wrfnatf (the live-fire
# preserved files were named "wrfnaf" by the orchestrator's own renaming).
HRSTEM=${HRSTEM:-wrfnatf}

fail=0

# ---------------------------------------------------------------------------
# verify_file NAME: magic + trailing 7777 + non-empty, else fail marker
# ---------------------------------------------------------------------------
verify_file() {
    local f="$1"
    [ -s "$f" ] || return 1
    [ "$(head -c 4 "$f")" = "GRIB" ] || return 1
    [ "$(tail -c 4 "$f")" = "7777" ] || return 1
    return 0
}

# ---------------------------------------------------------------------------
# fetch_slice OUTNAME URL PATTERN [PATTERN...]
#   Range-extracts every message whose .idx line matches ANY PATTERN (POSIX
#   regex matched against the whole idx line) and concatenates them.
# ---------------------------------------------------------------------------
fetch_slice() {
    local name="$1"; shift
    local url="$1"; shift
    local out="$OUT/$name"
    # Join the remaining PATTERN args with the @@ separator awk will split on
    # ($* would insert spaces and corrupt the first/last patterns).
    local pats=""
    while [ $# -gt 0 ]; do
        if [ -z "$pats" ]; then pats="$1"; else pats="$pats@@$1"; fi
        shift
    done

    if verify_file "$out"; then
        echo "== $name: present and structurally complete, keeping (idempotent)"
        sha256sum "$out"
        return 0
    fi

    echo "== $name: fetching .idx from"
    echo "     $url.idx"
    local idx="$OUT/.$name.idx.tmp"
    if ! curl -sfL --max-time "$CURL_TIMEOUT" -o "$idx" "$url.idx"; then
        echo "   WARN: .idx fetch failed (rotated off NOMADS? network?) — skipping $name"
        rm -f "$idx"
        return 0
    fi

    # Emit "start end" byte ranges; end == -1 means "to EOF". NOMADS .idx
    # lines are RECORD:OFFSET:d=YYYYMMDDHH:PARAM:LEVEL:... — the OFFSET is
    # the SECOND colon-separated field (reading the first yields record
    # numbers, which silently fetches garbage: pinned by this comment after
    # the E4a live-fire caught exactly that).
    local ranges
    ranges=$(awk -v pats="$pats" '
        BEGIN { n = 0 }
        {
            m = index($0, ":")
            if (m == 0) next
            rest = substr($0, m + 1)
            m2 = index(rest, ":")
            if (m2 == 0) next
            off[n] = substr(rest, 1, m2 - 1) + 0
            line[n] = $0
            n++
        }
        END {
            split(pats, pa, "@@")
            for (i = 0; i < n; i++) {
                hit = 0
                for (j in pa) if (line[i] ~ pa[j]) { hit = 1; break }
                if (hit) {
                    e = (i + 1 < n) ? off[i + 1] - 1 : -1
                    printf "%d %d\n", off[i], e
                }
            }
        }' "$idx")

    if [ -z "$ranges" ]; then
        echo "   WARN: no .idx line matched — skipping $name (product drift?)"
        [ "${KEEP:-0}" = "1" ] && mv "$idx" "$OUT/$name.idx" || rm -f "$idx"
        return 0
    fi

    local tmp="$OUT/.$name.part.tmp"
    : > "$tmp"
    local total=0
    while read -r start end; do
        [ -n "$start" ] || continue
        local spec
        if [ "$end" = "-1" ]; then spec="${start}-"; else spec="${start}-${end}"; fi
        if ! curl -sfL --max-time "$CURL_TIMEOUT" -r "$spec" -o "$tmp.chunk" "$url"; then
            echo "   WARN: range fetch failed for $name ($spec) — abandoning slice"
            rm -f "$tmp" "$tmp.chunk"
            fail=1
            return 0
        fi
        cat "$tmp.chunk" >> "$tmp"
        total=$((total + 1))
    done <<< "$ranges"
    rm -f "$tmp.chunk"

    if [ "$total" = "0" ] || ! verify_file "$tmp"; then
        echo "   WARN: assembled slice failed magic/7777 verification — dropping $name"
        rm -f "$tmp"
        fail=1
        return 0
    fi
    mv "$tmp" "$out"
    [ "${KEEP:-0}" = "1" ] && mv "$idx" "$OUT/$name.idx" || rm -f "$idx"
    echo "   OK: $total message(s), $(stat -c %s "$out") bytes"
    sha256sum "$out"
}

echo "wxpoint real-sample fetch (MANUAL / NETWORK)"
echo "  GFS : $GFS_DIR  (run ${GFS_YMD}T${GFS_HH}Z)"
echo "  HRRR: $HRRR_DIR (run ${HRRR_YMD}T${HRRR_HH}Z)"
echo

# --- GFS sflux family -------------------------------------------------------
# NOTE (E4a diagnosis): the live GFS sflux messages ride grid definition
# TEMPLATE 3.40 (3072 x 1536), NOT regular lat/lon 3.0 — our decoder skips
# them by design (README limitations). They are fetched anyway so the skip
# behaviour itself is pinned against production bytes.
fetch_slice gfs_sflux_f000.grib2 \
    "$GFS_DIR/gfs.t${GFS_HH}z.sfluxgrbf000.grib2" \
    ':ACOND:' ':TMP:2 m above ground'
fetch_slice gfs_sflux_f006.grib2 \
    "$GFS_DIR/gfs.t${GFS_HH}z.sfluxgrbf006.grib2" \
    ':UGRD:10 m above ground' ':VGRD:10 m above ground'
fetch_slice gfs_sflux_f012.grib2 \
    "$GFS_DIR/gfs.t${GFS_HH}z.sfluxgrbf012.grib2" \
    ':HGT:' ':TMP:.*hybrid'

# --- HRRR CONUS family ------------------------------------------------------
fetch_slice hrrr_tmp2m_f00.grib2 \
    "$HRRR_DIR/hrrr.t${HRRR_HH}z.${HRSTEM}00.grib2" \
    ':TMP:2 m above ground'
fetch_slice hrrr_uv10m_f06.grib2 \
    "$HRRR_DIR/hrrr.t${HRRR_HH}z.${HRSTEM}06.grib2" \
    ':UGRD:10 m above ground' ':VGRD:10 m above ground'
# The zero-window accumulation: APCP with begin==end==run time, encoded with
# an EMPTY section 7 (212 bytes total on Oct 22 2026). We take the FIRST
# APCP:surface message of f000; the gate tolerates any size and pins the
# all-zero degenerate semantics only when the slice really is that shape.
fetch_slice hrrr_apcp_tight.grib2 \
    "$HRRR_DIR/hrrr.t${HRRR_HH}z.${HRSTEM}00.grib2" \
    ':APCP:surface'
# Optional: PDT 4.8 instantaneous-max wind product (time-interpolation guard
# regression material). Skipped with a warning when the product line lacks it.
fetch_slice hrrr_windmax_f06.grib2 \
    "$HRRR_DIR/hrrr.t${HRRR_HH}z.${HRSTEM}06.grib2" \
    ':WIND:'

echo
echo "== checksums of everything in $OUT =="
if ls "$OUT"/*.grib2 >/dev/null 2>&1; then
    sha256sum "$OUT"/*.grib2
else
    echo "(nothing fetched — check dates/network; NOMADS keeps only recent cycles)"
fi

if [ -x "$WG" ] && ls "$OUT"/*.grib2 >/dev/null 2>&1; then
    echo
    echo "== wgrib2-full inventory (reference view of the same bytes) =="
    for f in "$OUT"/*.grib2; do
        echo "-- $f"
        "$WG" "$f" 2>/dev/null | head -8
    done
fi

echo
echo "Done. With data present, 'cargo test -p wx-ingest --test realdata_gate'"
echo "activates the live-byte gates; without it they skip cleanly."
[ "$fail" = "0" ] || echo "NOTE: at least one slice failed to assemble (see WARNs)."
exit 0
