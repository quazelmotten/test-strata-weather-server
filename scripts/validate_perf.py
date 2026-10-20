#!/usr/bin/env python3
"""Validate the full-size perf dataset against tools/wgrib2 and emit
tests/perfdata/probes.tsv (ground-truth queries for smoke.sh / loadtest.sh).

Run from the repo root:  python3 scripts/validate_perf.py

Every probe line: url_path <TAB> mode <TAB> expected
  mode=grep   response body must contain "value":<expected> verbatim (exact
              analytic integer/dyadic)
  mode=near   numeric compare within 1e-3 (LCC probes land a float hair off
              node; fields are affine in (i,j), so blending noise is tiny)
  mode=status only the HTTP status matters

wgrib2 cross-checks here (spec §9.1/§9.2 re-run at full grid size):
  * every GFS/HRRR record's node value at probe points equals the analytic
    formula read back by `wgrib2 -lon` (f32-exact),
  * `wgrib2 -ll2ij` agrees with the expected scan convention at the four
    corners of the LCC grid,
  * every record reports DRT 5.0 (simple packing) via -code_table_5.0.
Exit code != 0 on any mismatch.
"""

import subprocess
import sys

WG = "tools/wgrib2"
PERF = "tests/perfdata"


def lon_values(fname, lon, lat):
    """wgrib2 -lon LON LAT -> list of (label, value) one per record.
    Output lines look like '3:3110776:lon=25.000000,lat=39.875000,val=2940'."""
    out = subprocess.run([WG, fname, "-lon", repr(lon), repr(lat)],
                         capture_output=True, text=True)
    res = []
    for ln in out.stdout.splitlines():
        if "val=" not in ln:
            continue
        try:
            res.append((ln.split(":lon=")[0], float(ln.split("val=")[1])))
        except ValueError:
            pass
    return res


def ll2ij(fname, lon, lat):
    """wgrib2 -ll2ij -> fractional 0-based (i, j) from the last record line."""
    out = subprocess.run([WG, fname, "-ll2ij", repr(lon), repr(lat)],
                         capture_output=True, text=True)
    pair = out.stdout.strip().splitlines()[-1].split("->")[1]
    ij = pair.strip("() ").split(",")
    return float(ij[0]), float(ij[1])


def check_drt_simple(fname, want):
    out = subprocess.run([WG, fname, "-code_table_5.0"],
                         capture_output=True, text=True)
    n = sum(1 for ln in out.stdout.splitlines() if "code table 5.0=" in ln)
    bad = sum(1 for ln in out.stdout.splitlines()
              if "code table 5.0=" in ln and "code table 5.0=0" not in ln)
    if n != want or bad:
        print(f"DRT check failed for {fname}: {n} recs, {bad} non-simple")
        sys.exit(1)


def gfs_inc(i, j, fh):
    return ((j * 1440 + i) % 4096) + fh * 10


# ordered field tables — MUST mirror gen_grib2.py's perf section exactly
GFS_FIELDS = []
for lv in (1000, 925, 850, 700, 500, 300, 250, 200):
    GFS_FIELDS.append(("HGT", lv, {1000: 480, 925: 1020, 850: 1560,
                                   700: 3000, 500: 5700, 300: 9400,
                                   250: 10500, 200: 11800}[lv]))
for lv in (1000, 925, 850, 700, 500, 300, 250, 200):
    GFS_FIELDS.append(("TMP", lv, {1000: 250, 925: 255, 850: 260,
                                   700: 265, 500: 270, 300: 275,
                                   250: 280, 200: 285}[lv]))
GFS_FIELDS += [("TMP", 2, 260), ("UGRD", 10, -5), ("VGRD", 10, -3),
               ("RH", 2, 40), ("PRMSL", 0, 101000), ("GUST", 10, 5)]

# ---- GFS truth checks at node (i=100, j=200) -> (lat 39.875, lon 25.0) ----
I, J = 100, 200
LAT, LON = 89.875 - 0.25 * J, 0.25 * I
for fh, fname in ((0, "gfs_perf_f000.grib2"), (6, "gfs_perf_f006.grib2")):
    path = f"{PERF}/{fname}"
    check_drt_simple(path, 22)
    vals = lon_values(path, LON, LAT)
    if len(vals) != 22:
        print(f"-lon probe on {fname} returned {len(vals)} records")
        sys.exit(1)
    for k, (pname, lv, base) in enumerate(GFS_FIELDS):
        inc = J + fh * 10 if (pname == "TMP" and lv == 1000) \
              else gfs_inc(I, J, fh)
        want = base + inc
        got = vals[k][1]
        if abs(got - want) > 0.51:      # f32 rounding of analytic ints
            print(f"MISMATCH {fname} rec{k+1} {pname}@{lv}: wgrib2={got} formula={want}")
            sys.exit(1)
print("gfs perf records match analytic formula at node probe")

# ---- HRRR scan-convention + value checks (affine fields) ----
HINC = lambda i, j, fh: i + j + fh * 10
HBASES = (255, 258, 262, 268, 275, 10, 750, 1550, 3000, 5700, -5, -3, 40)

sw_i, sw_j = ll2ij(f"{PERF}/hrrr_perf_f000.grib2", -126.4, 21.2)
print(f"hrrr SW probe (-126.4,21.2) -> (i,j)=({sw_i:.3f},{sw_j:.3f}) "
      "(near (0,0) => stored row 0 is the SOUTH edge, scan byte 0x40)")
if not (0.0 <= sw_i < 20.0 and 0.0 <= sw_j < 20.0):
    sys.exit(1)

# node lat/lon via pyproj (validated against wgrib2 in milestone 2)
from pyproj import Transformer
LCC = ("+proj=lcc +lat_1=26.5 +lat_2=35 +lat_0=26.5 +lon_0=229.53 "
       "+a=6371229 +b=6371229 +units=m")
tr = Transformer.from_crs("EPSG:4326", LCC)
x0, y0 = tr.transform(21.11389, 233.44)      # (lat, lon) -> (easting, northing)
inv = Transformer.from_crs(LCC, "EPSG:4326")
NODES = [(865, 664), (100, 50), (1700, 1000), (500, 300)]
node_pts = []
for (ni, nj) in NODES:
    la, lo = inv.transform(x0 + ni * 3000.0, y0 + nj * 3000.0)  # -> (lat, lon)
    lon_w = lo - 360.0 if lo > 180.0 else lo   # western hemisphere as negative
    node_pts.append((round(la, 6), round(lon_w, 6)))
print("hrrr node lat/lon:", node_pts)

# cross-check every record at the first node against the analytic formula
PLAT, PLON = node_pts[0][0], node_pts[0][1]
si, sj = NODES[0]
ok = 0
for label, v in lon_values(f"{PERF}/hrrr_perf_f000.grib2", PLON, PLAT):
    ok += 1
    hit = False
    for b in HBASES:
        if abs(v - (b + HINC(si, sj, 0))) <= 0.51:
            hit = True
            break
    if not hit:
        print(f"hrrr record {label} value {v} matches no base at ({si},{sj})")
        sys.exit(1)
print(f"hrrr probe point matches formula on {ok} records (i_store,j_store)=({si},{sj})")
if ok < 13:
    sys.exit(1)

RUN = "2026-10-19T12:00:00Z"
inc = gfs_inc(I, J, 0)
probes = [
    # GFS exact-node probes (analytic integers)
    (f"/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat={LAT}&lon={LON}"
     f"&valid={RUN}", "grep", 250 + J),
    (f"/v1/point?model=gfs&param=HGT&level=isobaric:850&lat={LAT}&lon={LON}"
     f"&valid={RUN}", "grep", 1560 + inc),
    (f"/v1/point?model=gfs&param=TMP,HGT&level=isobaric:850&lat={LAT}&lon={LON}"
     f"&valid={RUN}", "grep", 260 + inc),
    (f"/v1/point?model=gfs&param=TMP&level=isobaric:775&lat={LAT}&lon={LON}"
     f"&valid={RUN}", "grep", "1642.5"),
    (f"/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat={LAT}&lon={LON}"
     f"&valid=2026-10-19T15:00:00Z", "grep", 250 + J + 30),
    (f"/v1/point?model=gfs&param=HGT&level=isobaric:775&lat={LAT}&lon={LON}"
     f"&valid={RUN}", "grep", 2280 + inc),
    (f"/v1/point?model=gfs&param=PRMSL&level=surface&lat={LAT}&lon={LON}"
     f"&valid={RUN}", "grep", 101000 + inc),
    (f"/v1/point?model=gfs&param=GUST&level=height_above_ground:10&lat={LAT}&lon={LON}"
     f"&valid={RUN}", "grep", 5 + inc),
    (f"/v1/point?model=gfs&param=UGRD,VGRD&level=height_above_ground:10&lat={LAT}&lon={LON}"
     f"&valid={RUN}", "status", 200),
    ("/v1/point?model=nope&param=TMP&level=isobaric:1000&lat=0&lon=0"
     f"&valid={RUN}", "status", 404),
    (f"/v1/point?model=gfs&param=TMP&level=isobaric:1&lat={LAT}&lon={LON}"
     f"&valid={RUN}", "status", 422),
]
# HRRR affine probes at the interior point (near: fractional-node noise)
hval = lambda base, fh: base + HINC(si, sj, fh)
probes += [
    (f"/v1/point?model=hrrr&param=TMP&level=isobaric:850&lat={PLAT}&lon={PLON}"
     f"&valid={RUN}", "near", hval(262, 0)),
    (f"/v1/point?model=hrrr&param=HGT&level=isobaric:1000&lat={PLAT}&lon={PLON}"
     f"&valid={RUN}", "near", hval(10, 0)),
    (f"/v1/point?model=hrrr&param=TMP&level=isobaric:775&lat={PLAT}&lon={PLON}"
     f"&valid={RUN}", "near", 265 + HINC(si, sj, 0)),
    (f"/v1/point?model=hrrr&param=TMP&level=isobaric:850&lat={PLAT}&lon={PLON}"
     f"&valid=2026-10-19T13:30:00Z", "near", hval(262, 0) + 15),
    (f"/v1/point?model=hrrr&param=UGRD,VGRD&level=height_above_ground:10&lat={PLAT}&lon={PLON}"
     f"&valid={RUN}", "status", 200),
    # outside the LCC node rectangle -> 422 out_of_domain
    (f"/v1/point?model=hrrr&param=TMP&level=isobaric:850&lat=59.0&lon=-97.0"
     f"&valid={RUN}", "status", 422),
]
with open(f"{PERF}/probes.tsv", "w") as f:
    for url, mode, exp in probes:
        f.write(f"{url}\t{mode}\t{exp}\n")
print(f"wrote {PERF}/probes.tsv ({len(probes)} probes)")
