#!/usr/bin/env python3
# ============================================================================
# V1.3 phase E5 REAL-DATA PROBE SUITE — MANUAL / NETWORK, NEVER CI.
#
# Compares the LIVE wxpoint server's answers against expectations derived
# INDEPENDENTLY of our Rust code path:
#   * decoded node values straight from tools/wgrib2-full `-lon` (production
#     bytes through a mature reference decoder),
#   * LCC projection + conformal-angle geometry from pyproj via
#     scripts/validate_rotation.py (the E-era IPOLATES-free §9.3 chain),
#   * plain arithmetic for blends.
#
# Covers spec §9 items the synthetic fixtures cannot:
#   (A) WIND ROTATION on REAL HRRR bytes: ground-truth component convention
#       read from the section-3 flag byte (bit 0x08), raw grid-relative U/V
#       printed by wgrib2 at exact nodes, rotated to earth-relative here with
#       gamma measured by pyproj, compared against the server's stored
#       earth-relative answer. Tolerance derives from wgrib2's SIX-SIGNIFICANT
#       -lon print (half-ulp <= ~3e-6 relative) plus f32 storage rounding;
#       any wrong convention/sign would miss by O(1..30) m/s, so 1e-4 m/s is
#       decisive. Recorded max deltas are printed honestly.
#   (B) TIME INTERPOLATION on REAL GFS bytes: 2 m TMP at fh {0,6,12} of one
#       live run; midpoint/third blends computed as f64 linear interpolations
#       of the printed endpoints (tolerance likewise print-derived, 1e-3 K);
#       exact-fh stays node-exact; time_interp=none off-slot MUST error.
#   (C) PDT 4.8 METADATA FROM PRODUCTION BYTES: /v1/inventory stat flags for
#       APCP (0-0 day accumulation) and WIND (5-6 hour max, raw triple
#       d0c2n1), and the interpolation guards answering 422 stat_time_guard.
#   (D) E4c live bonus: hybrid-level query on the release server.
#
# Run via scripts/realdata_acceptance.sh (starts the server); standalone:
#   python3 scripts/realdata_probes.py [--base URL] [--realdata DIR] [--wg PATH]
# Exit 0 iff every pin passes.
# ============================================================================

import json
import math
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from validate_rotation import parse_sec3_lambert, beta_pyproj  # noqa: E402
from pyproj import Proj  # noqa: E402

D2R = math.pi / 180.0

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASE = "http://127.0.0.1:18099"
REALDATA = os.path.join(ROOT, "tests", "realdata")
WG = os.path.join(ROOT, "tools", "wgrib2-full")

argv = sys.argv[1:]
if "--base" in argv:
    BASE = argv[argv.index("--base") + 1]
if "--realdata" in argv:
    REALDATA = argv[argv.index("--realdata") + 1]
if "--wg" in argv:
    WG = argv[argv.index("--wg") + 1]

ROT_TOL = 1e-4   # m/s; derived from 6-sig-digit wgrib2 print + f32 storage
TIME_TOL = 1e-3  # K; same print derivation on endpoint values

results = []


def pin(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    tag = "PASS" if ok else "FAIL"
    print("%-6s %s%s" % (tag, name, (" | " + detail) if detail else ""))


def rd(path):
    return open(path, "rb").read()


def fpath(name):
    return os.path.join(REALDATA, name)


def wg_lines(f, *args):
    r = subprocess.run([WG, fpath(f)] + list(args), capture_output=True,
                       text=True)
    return [ln.strip() for ln in (r.stdout or "").splitlines() if ln.strip()]


RUN_RE = re.compile(r"d=(\d{10})")


def run_iso_of(f):
    """Reference time of a file's first message, straight from wgrib2's own
    inventory line (d=YYYYMMDDHH) -> ISO + epoch-ish datetime tuple."""
    for ln in wg_lines(f):
        m = RUN_RE.search(ln)
        if m:
            s = m.group(1)
            iso = "%s-%s-%sT%s:00:00Z" % (s[0:4], s[4:6], s[6:8], s[8:10])
            return iso, (int(s[0:4]), int(s[4:6]), int(s[6:8]), 0, 0, 0)
    raise SystemExit("no d= token in %s inventory" % f)


def add_hours(iso, hours):
    import datetime
    dt = datetime.datetime.strptime(iso, "%Y-%m-%dT%H:%M:%SZ")
    dt += datetime.timedelta(hours=hours)
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


LONLAT_RE = re.compile(r"lon=([\d.]+),lat=([\d.]+),val=([-\d.eE+]+)")


def wg_lon(f, lon, lat):
    """[(snapped_lon, snapped_lat, value)] for every message in f."""
    out = []
    for ln in wg_lines(f, "-lon", "%.6f" % lon, "%.6f" % lat):
        m = LONLAT_RE.search(ln)
        if m:
            out.append((float(m.group(1)), float(m.group(2)),
                        float(m.group(3))))
    return out


def wg_val_at(f, lon, lat, want_pat):
    """Value printed by -lon for the FIRST message whose inventory line
    matches want_pat (matched against the plain inventory listing)."""
    inv = wg_lines(f)
    pts = wg_lon(f, lon, lat)
    idx = None
    for k, ln in enumerate(inv):
        if re.search(want_pat, ln):
            idx = k
            break
    if idx is None or idx >= len(pts):
        raise SystemExit("probe pattern '%s' not found in %s" % (want_pat, f))
    return pts[idx]


def http_get(path):
    url = BASE + path
    try:
        with urllib.request.urlopen(url, timeout=60) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")


def point(model, param, level, lat, lon, valid, run=None, tinterp=None):
    q = {"model": model, "param": param, "level": level,
         "lat": "%.9f" % lat, "lon": "%.9f" % lon, "valid": valid}
    if run:
        q["run"] = run
    if tinterp:
        q["time_interp"] = tinterp
    # NOTE: wxpoint's query parser reads the RAW query (the spec §7 wire form
    # uses bare colons, which RFC3986 permits in query values; curl sends
    # them verbatim). Standard percent-encoding here would hand the parser a
    # literal "%3A" inside level/valid — so keep : , - . / _ unescaped.
    st, body = http_get("/v1/point?" + urllib.parse.urlencode(
        q, safe=":,-._/"))
    return st, body


def point_values(st, body):
    """param -> value map from a 200 response."""
    out = {}
    if st != 200:
        return out
    j = json.loads(body)
    for r in j.get("results", []):
        out[r["param"]] = r.get("value")
    return out


# ===========================================================================
# (A) REAL WIND ROTATION
# ===========================================================================
print("== (A) real HRRR wind rotation ==")
uvf = "hrrr_uv10m_f06.grib2"
gf = parse_sec3_lambert(rd(fpath(uvf)))
pin("A0 flag33=0x%02X grid-relative component bit SET (live bytes)" % gf["flag33"],
    (gf["flag33"] & 8) != 0,
    "nx=%d ny=%d shape=%d LaV=%.4f LoV=%.4f Latin=(%.4f,%.4f)" %
    (gf["nx"], gf["ny"], gf["shape"], gf["lad"], gf["lov"],
     gf["latin1"], gf["latin2"]))

maj, minr = gf["major_m"], gf["minor_m"]
if abs(maj - minr) < 1e-6:
    P = Proj(proj="lcc", lat_1=gf["latin1"], lat_2=gf["latin2"],
             lat_0=gf["lad"], lon_0=gf["lov"], no_defs=True, errcheck=False,
             R=maj)
else:
    P = Proj(proj="lcc", lat_1=gf["latin1"], lat_2=gf["latin2"],
             lat_0=gf["lad"], lon_0=gf["lov"], no_defs=True, errcheck=False,
             a=maj, b=minr)
x0, y0 = P(gf["lo1"], gf["la1"])
j_south = (gf["scan"] & 0x40) == 0

# well-spread CONUS nodes + one NEAR-EDGE node (two cells inside the eastern
# column so bilinear stays strictly inside the domain, spec 6.1 no-extrapolate)
nodes = [(200, 400), (899, 529), (1300, 650), (1600, 850), (600, 150),
         (gf["nx"] - 3, 529)]
uv_run, _ = run_iso_of(uvf)
uv_valid = add_hours(uv_run, 6)

rot_worst = 0.0
for n, (i, j) in enumerate(nodes):
    dj = -j if j_south else j
    x = x0 + i * gf["dx_m"]
    y = y0 + dj * gf["dy_m"]
    lon, lat = P(x, y, inverse=True)
    if lon > 180.0:
        lon -= 360.0
    # raw GRID-relative components at this exact node from the reference decoder
    slon, slat, u_g = wg_val_at(uvf, lon, lat, r":UGRD:10 m above ground")
    _, _, v_g = wg_val_at(uvf, lon, lat, r":VGRD:10 m above ground")
    if abs(slat - lat) > 2e-5 or abs(((slon - lon + 180.0) % 360.0) - 180.0) > 2e-5:
        pin("A%d snap check (%d,%d)" % (n + 1, i, j), False,
            "wgrib2 snapped to (%.6f,%.6f) not our node (%.6f,%.6f)" %
            (slon, slat, lon, lat))
        continue
    b_deg, _, _ = beta_pyproj(P, x0, y0, gf["dx_m"], gf["dy_m"], j_south, i, j)
    s = math.sin(b_deg * D2R)
    c = math.cos(b_deg * D2R)
    # WMO convention as documented in grib2.rs rotate_winds_to_earth and the
    # DECISIONS angle contract: g_i = sin(b) e_E + cos(b) e_N,
    # g_j = cos(b) e_E - sin(b) e_N  =>
    u_e = u_g * s + v_g * c
    v_n = u_g * c - v_g * s
    st, body = point("hrrr", "UGRD,VGRD", "height_above_ground:10",
                     lat, lon, uv_valid)
    vals = point_values(st, body)
    if "UGRD" not in vals or "VGRD" not in vals:
        pin("A%d server answer (%d,%d)" % (n + 1, i, j), False,
            "http %d: %s" % (st, body[:120]))
        continue
    du = abs(vals["UGRD"] - u_e)
    dv = abs(vals["VGRD"] - v_n)
    worst = max(du, dv)
    rot_worst = max(rot_worst, worst)
    edge = " NEAR-EDGE" if i >= gf["nx"] - 4 else ""
    pin("A%d rotation u/v @(%d,%d)%s" % (n + 1, i, j, edge),
        worst <= ROT_TOL,
        "node(%.5f,%.5f) gamma=%.4f deg raw=(%.5f,%.5f) earth=(%.5f,%.5f) "
        "server=(%.5f,%.5f) |delta|=%.2e" %
        (lat, lon, b_deg, u_g, v_g, u_e, v_n, vals["UGRD"], vals["VGRD"], worst))

st, body = http_get("/v1/models")
mblk = body.split('"name":"hrrr"', 1)
ok = st == 200 and len(mblk) == 2 and \
    re.search(r'"source_winds_grid"\s*:\s*(true|1)', mblk[1]) is not None
pin("A7 /v1/models hrrr source_winds_grid=true (decoder agrees with bytes)",
    ok, "http %d" % st)
print("   rotation max |delta| over all probed nodes: %.3e m/s" % rot_worst)

# ===========================================================================
# (B) REAL TIME INTERPOLATION (same run, multiple forecast hours)
# ===========================================================================
print("== (B) real GFS time interpolation (TMP 2 m, run-consistent) ==")
g0, g6, g12 = "gfs_sflux_f000.grib2", "gfs_tmp_f006.grib2", "gfs_tmp_f012.grib2"
pat = r":TMP:2 m above ground"
# the E4b/E4c node: gaussian row 40.006506 N, column lon -95.0390625 (exact)
BLAT, BLON = 40.006506, -95.0390625
_, _, e0 = wg_val_at(g0, BLON, BLAT, pat)
_, _, e6 = wg_val_at(g6, BLON, BLAT, pat)
_, _, e12 = wg_val_at(g12, BLON, BLAT, pat)
gfs_run, _ = run_iso_of(g0)
print("   endpoints wgrib2: fh0=%.4f fh6=%.4f fh12=%.4f K (run %s)" %
      (e0, e6, e12, gfs_run))


def lerp(a, b, t):
    return a + (b - a) * t


time_worst = 0.0
for name, fh, want, ti in [
        ("B1 exact fh6 stays node-exact", 6, e6, "none"),
        ("B2 midpoint fh3 linear blend (fh0,fh6)", 3, lerp(e0, e6, 0.5), "linear"),
        ("B3 third-point fh2 linear blend (fh0,fh6)", 2, lerp(e0, e6, 1.0 / 3.0), "linear"),
        ("B4 midpoint fh9 linear blend (fh6,fh12)", 9, lerp(e6, e12, 0.5), "linear")]:
    st, body = point("gfs", "TMP", "height_above_ground:2", BLAT, BLON,
                     add_hours(gfs_run, fh), tinterp=ti)
    vals = point_values(st, body)
    if "TMP" not in vals:
        pin(name, False, "http %d: %s" % (st, body[:140]))
        continue
    d = abs(vals["TMP"] - want)
    time_worst = max(time_worst, d)
    pin(name, d <= TIME_TOL,
        "got %.6f want %.6f |delta|=%.2e" % (vals["TMP"], want, d))
# time_interp=none OFF a stored slot must ERROR (no silent blend)
st, body = point("gfs", "TMP", "height_above_ground:2", BLAT, BLON,
                 add_hours(gfs_run, 3), tinterp="none")
pin("B5 time_interp=none at fh3 errors (no silent blend)",
    st >= 400 and '"value"' not in body,
    "http %d: %s" % (st, body[:140]))
print("   time-interp max |delta| over blends: %.3e K" % time_worst)

# ===========================================================================
# (C) PDT 4.8 METADATA FROM PRODUCTION BYTES
# ===========================================================================
print("== (C) production PDT 4.8 metadata + guards ==")
st, body = http_get("/v1/inventory?model=hrrr")
inv = json.loads(body) if st == 200 else {}
hrrr_run, _ = run_iso_of("hrrr_apcp_tight.grib2")


def inv_find(param):
    # shape: {"inventory":[{"model":...,"runs":[{"run":...,"fields":[...]}]}]}
    for mdl in inv.get("inventory", []):
        for run in mdl.get("runs", []):
            for fe in run.get("fields", []):
                if fe.get("param") == param:
                    return fe
    return None


apcp = inv_find("APCP")
pin("C1 inventory APCP stat_processed=true stat_kind=accumulation",
    apcp is not None and apcp.get("stat_processed") is True and
    apcp.get("stat_kind") == "accumulation",
    "entry=%s" % (json.dumps(apcp)[:150] if apcp else "MISSING"))
wind = inv_find("d0c2n1")
pin("C2 inventory WIND raw triple d0c2n1 stat_processed=true stat_kind=maximum",
    wind is not None and wind.get("stat_processed") is True and
    wind.get("stat_kind") == "maximum",
    "entry=%s" % (json.dumps(wind)[:150] if wind else "MISSING"))

# exact-window answers stay available...
st, body = point("hrrr", "APCP", "surface", 35.0, -97.0, hrrr_run,
                 tinterp="none")
vals = point_values(st, body)
ok = st == 200 and vals.get("APCP") is not None and math.isfinite(vals["APCP"])
pin("C3 APCP exact fh0 (zero-window) answers via API", ok,
    "http %d value=%s (tight all-zero degenerate s7)" %
    (st, vals.get("APCP")))
# Honest pin: production WIND (5-6 h max) is indexed at its BEGIN offset
# (inventory forecast_hours [5]) — spec §7 defines valid=run+fh slots for
# INSTANTANEOUS fields; stat products ride their own begin/end window, which
# the guard enforces. Query the stored slot (5h) and expect a real answer.
st, body = point("hrrr", "d0c2n1", "height_above_ground:10", 35.0, -97.0,
                 add_hours(hrrr_run, 5), tinterp="none")
vals = point_values(st, body)
w = vals.get("d0c2n1")
ok = st == 200 and w is not None and math.isfinite(w) and 0.0 <= w <= 60.0
pin("C4 WIND (raw key) exact stored slot (begin-offset 5h) answers, plausible 0..60",
    ok, "http %d value=%s" % (st, w))

# ...and mid-window queries MUST be refused by the guard, never blended
st, body = point("hrrr", "APCP", "surface", 35.0, -97.0,
                 add_hours(hrrr_run, 3), tinterp="linear")
pin("C5 APCP fh3 linear -> 422 stat_time_guard naming accumulation",
    st == 422 and "stat_time_guard" in body and "accumulation" in body,
    "http %d: %s" % (st, body[:160]))
st, body = point("hrrr", "d0c2n1", "height_above_ground:10", 35.0, -97.0,
                 add_hours(hrrr_run, 5.5), tinterp="linear")
pin("C6 WIND fh5.5 linear -> 422 stat_time_guard", st == 422 and
    "stat_time_guard" in body, "http %d: %s" % (st, body[:160]))

# ===========================================================================
# (D) E4c live bonus: hybrid level on the release server
# ===========================================================================
print("== (D) hybrid-level live query (release binary) ==")
_, _, hyb_truth = wg_val_at("gfs_sflux_f012.grib2", BLON, BLAT,
                            r":HGT:1 hybrid level")
st, body = point("gfs", "HGT", "hybrid:1", BLAT, BLON,
                 add_hours(gfs_run, 12), tinterp="none")
vals = point_values(st, body)
h = vals.get("HGT")
pin("D1 HGT hybrid:1 fh12 matches wgrib2 (%.3f gpm)" % hyb_truth,
    st == 200 and h is not None and abs(h - hyb_truth) <= 0.01,
    "http %d got %s" % (st, h))

# ===========================================================================
nfail = sum(1 for _, ok, _ in results if not ok)
print("== SUMMARY: %d/%d pins passed ==" % (len(results) - nfail, len(results)))
print("REALDATA ACCEPTANCE %s" % ("PASS" if nfail == 0 else "FAIL"))
sys.exit(0 if nfail == 0 else 1)
