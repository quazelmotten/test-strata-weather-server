#!/usr/bin/env python3
# Spec §9.3 wind-rotation validation, machine-independent of IPOLATES.
#
# WHY not `wgrib2 -new_grid_winds earth`: that invoker needs the IPOLATES
# Fortran library (33 .f90 modules); this machine has no Fortran compiler at
# all (see docs/DECISIONS.md, phase-1 evidence), and the built binaries refuse
# with "IPOLATES package is not installed". The golden check is stubbed in
# scripts/validate_rotation_wgrib2.sh for machines that do have it.
#
# WHAT replaces it: an INDEPENDENT measurement of the same physical quantity
# -- the CONFORMAL angle, i.e. the bearing of the grid +i axis from TRUE north
# -- taken from pyproj's own Lambert Conformal Conic forward transform with the
# earth shape EXACTLY matching the fixture's Code Table 3.2 byte (including the
# INLINE user-defined axes of code 3). In a conformal conic map meridians are
# exactly straight radial lines, so the chord between two points on the meridian
# through P is exactly the meridian tangent: the planar angle between that chord
# and the +x axis equals the earth-surface angle with NO curvature error and
# without any geodesy library (pyproj's Geod cannot even build custom ellipsoids
# here -- it returns NaN -- which is another reason to avoid it).
#
# Output: tests/fixtures/rotation_expected.csv, consumed by
# crates/wx-ingest/tests/rotation_vs_pyproj.rs (tolerance 1e-6 degrees).
#
# Usage: python3 scripts/validate_rotation.py [--out PATH]

import csv
import math
import os
import random
import struct
import sys

from pyproj import Proj

D2R = math.pi / 180.0


def be_u32(b, o):
    return struct.unpack_from(">I", b, o)[0]


def sm_i32(b, o):
    v = struct.unpack_from(">I", b, o)[0]
    return -(v & 0x7FFFFFFF) if v & 0x80000000 else (v & 0x7FFFFFFF)


def scaled_val(b, o):
    """GRIB scaled value: INT1 factor + SIGN-MAGNITUDE INT4, value*10^-factor."""
    fac = b[o]
    if fac == 255:
        return None
    return sm_i32(b, o + 1) / float(10 ** fac)


def parse_sec3_lambert(raw):
    """Find the first section-3 payload in a GRIB2 file and decode template
    3.30 (offsets relative to the payload start, as written/read by wgrib2:
    the on-disk section is [LEN u4][sect# u8][payload], so payload starts at
    the LEN prefix + 5 bytes)."""
    n = len(raw)
    pos = 0
    while pos < n - 16:
        if raw[pos:pos + 4] != b"GRIB":
            break
        msglen = struct.unpack_from(">Q", raw, pos + 8)[0]
        body_end = pos + msglen
        q = pos + 16
        while q < body_end:
            if raw[q:q + 4] == b"7777":
                break
            ln = be_u32(raw, q)
            num = raw[q + 4]
            if num == 3:
                p = raw[q + 5:q + ln]
                tmpl = (p[7] << 8) | p[8]
                if tmpl == 30:
                    return _lambert_fields(p)
            q += ln
        pos = body_end
    raise SystemExit("no template 3.30 grid found")


def _lambert_fields(p):
    """Decode template 3.30 fields. The on-disk section is [LEN u4][sect#][...];
    wgrib2/gen_grib2 refer to those bytes as gds[k], while `p` starts after LEN+
    sect#, so gds[k] == p[k-5] (verified against the Rust reader: nx at gds[30]
    is read at p[25])."""
    def gi(k):
        return p[k - 5]
    def gu4(k):
        return be_u32(p, k - 5)
    def gs4(k):
        return sm_i32(p, k - 5)
    out = {
        "nx": gu4(30), "ny": gu4(34),
        "la1": gs4(38) / 1e6, "lo1": gu4(42) / 1e6,
        "flag33": gi(46),
        "lad": gs4(47) / 1e6, "lov": gu4(51) / 1e6,
        "dx_m": gs4(55) / 1000.0, "dy_m": gs4(59) / 1000.0,
        "proj_flag": gi(63), "scan": gi(64),
        "latin1": gs4(65) / 1e6, "latin2": gs4(69) / 1e6,
        "shape": gi(14),
    }
    shape = out["shape"]
    if shape == 1:
        r = scaled_val(p, 10)          # gds[15] factor, gds[16..20] value
        out["major_m"], out["minor_m"] = r, r
    elif shape in (3, 7):
        maj = scaled_val(p, 15)       # gds[20] factor, gds[21..25] value
        minr = scaled_val(p, 20)      # gds[25] factor, gds[26..30] value
        unit = 1000.0 if shape == 3 else 1.0   # code 3 declares km
        out["major_m"], out["minor_m"] = maj * unit, minr * unit
    else:
        tbl = {6: (6371229.0, 6371229.0), 2: (6378160.0, 6356775.0),
               0: (6367470.0, 6367470.0)}
        out["major_m"], out["minor_m"] = tbl.get(shape, (6378137.0, 6356752.3))
    out["winds_grid"] = (out["flag33"] & 8) != 0
    return out


def beta_pyproj(P, x0, y0, dx, dy, j_south, i, j, dlat_deg=0.25):
    """Conformal angle (deg) of the grid +i axis from true north at node (i,j),
    measured purely in pyproj's projected plane."""
    dj_north = -j if j_south else j
    x = x0 + i * dx
    y = y0 + dj_north * dy
    lon, lat = P(x, y, inverse=True)          # pyproj's OWN inverse
    ax, ay = P(lon, lat - dlat_deg)           # meridian through that point
    bx, by = P(lon, lat + dlat_deg)
    mx, my = bx - ax, by - ay                 # chord toward increasing latitude
    m = math.hypot(mx, my)
    mx, my = mx / m, my / m
    b = 90.0 - math.degrees(math.atan2(mx, my))
    while b > 180.0:
        b -= 360.0
    while b < -180.0:
        b += 360.0
    return b, lat, lon


def nodes(nx, ny, n_random=14):
    rnd = random.Random(20261019)
    pts = [(0, 0), (nx - 1, 0), (0, ny - 1), (nx - 1, ny - 1),
           (nx // 2, ny // 2), (nx // 4, ny // 4), (3 * nx // 4, 3 * ny // 4)]
    for _ in range(n_random):
        pts.append((rnd.randrange(0, nx), rnd.randrange(0, ny)))
    return pts


def run_fixture(path, out_rows):
    raw = open(path, "rb").read()
    gf = parse_sec3_lambert(raw)
    P = Proj(proj="lcc", lat_1=gf["latin1"], lat_2=gf["latin2"],
             lat_0=gf["lad"], lon_0=gf["lov"], no_defs=True, errcheck=False,
             R=gf["major_m"]) if abs(gf["major_m"] - gf["minor_m"]) < 1e-6 \
        else Proj(proj="lcc", lat_1=gf["latin1"], lat_2=gf["latin2"],
                  lat_0=gf["lad"], lon_0=gf["lov"], no_defs=True,
                  errcheck=False, a=gf["major_m"], b=gf["minor_m"])
    # origin of stored node (0,0) == (lo1, la1): any fixed offset is irrelevant
    # for ANGLES, but keeping it identical to the Rust grid makes the CSV rows
    # directly comparable with rot_angle_deg(i, j).
    x0, y0 = P(gf["lo1"], gf["la1"])
    j_south = (gf["scan"] & 0x40) == 0
    print("== {0}: shape CT3.2={1} axes=({2:.3f},{3:.3f}) m  winds_grid={4}"
          " latin=({5},{6}) LoV={7} scan={8}".format(
              os.path.basename(path), gf["shape"], gf["major_m"], gf["minor_m"],
              gf["winds_grid"], gf["latin1"], gf["latin2"], gf["lov"], gf["scan"]))
    worst_fwd = 0.0
    for (i, j) in nodes(gf["nx"], gf["ny"]):
        b, lat, lon = beta_pyproj(P, x0, y0, gf["dx_m"], gf["dy_m"], j_south,
                                  i, j)
        out_rows.append([os.path.basename(path), i, j,
                         "%.9f" % lat, "%.9f" % lon, "%.10f" % b])
    print("   wrote {0} node rows".format(len(out_rows)))


def main():
    out_path = "tests/fixtures/rotation_expected.csv"
    if "--out" in sys.argv:
        out_path = sys.argv[sys.argv.index("--out") + 1]
    rows = []
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    for name in ("hrrr_small.grib2", "hrrr_ellip_small.grib2"):
        run_fixture(os.path.join(root, "tests", "fixtures", name), rows)
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    with open(out_path, "w", newline="") as fh:
        w = csv.writer(fh, lineterminator="\n")
        w.writerow(["fixture", "i", "j", "lat", "lon", "beta_deg"])
        w.writerows(rows)
    print("OK: {0} rows -> {1}".format(len(rows), out_path))


if __name__ == "__main__":
    main()
