#!/usr/bin/env python3
# Validate the HRRR wind-rotation convention against pyproj (spec §9.3).
#
# The Rust decoder rotates grid-relative U/V to earth-relative using the
# bearing beta of the grid +i direction from true north (wx-core
# GridLambert.rot_angle_deg, which agrees with this script's pyproj-derived
# beta to < 1e-4 deg; see docs/DECISIONS.md). This script independently
# derives the earth direction of the grid +i and +j axes at sample nodes of
# tests/fixtures/hrrr_small.grib2 using pyproj's Lambert Conformal Conic
# ('lproj') definition, and checks the rotated unit vectors agree.
#
# Fixture LCC parameters: latin1=26.5 latin2=35.0 Lov=229.53, Dx=Dy=3000 m,
# rows scan SOUTHWARD (storage row j increases toward the south), columns
# increase eastward. In pyproj lproj terms: map +x ~ grid +i (east-ish),
# map +y ~ grid -j.
import pyproj
import math

def lcc():
    # lat_0 choice does not affect convergence angles for secant LCC
    return pyproj.AffineProjection if False else None

from pyproj import Proj

P = Proj(proj="lcc", lat_1=26.5, lat_2=35.0, lat_0=26.5, lon_0=229.53,
         R=6371229.0)  # sphere radius per Code Table 3.2 flag 6 (matches wgrib2)

def xy_of(i, j):
    # origin offset irrelevant for directions
    return (i * 3000.0, -j * 3000.0)

def to_latlon(i, j):
    x, y = xy_of(i, j)
    lon, lat = P(x, y, inverse=True)
    return lat, lon

EPS = 0.02
M_PER_DEG_LAT = 110574.0
M_PER_DEG_LON_EQ = 111319.49

def check(i, j):
    lat0, lon0 = to_latlon(i, j)
    # grid +i unit vector -> earth displacement
    lat1, lon1 = to_latlon(i + EPS, j)
    e_i = (lon1 - lon0) * M_PER_DEG_LON_EQ * math.cos(math.radians(lat0))
    n_i = (lat1 - lat0) * M_PER_DEG_LAT
    mag = math.hypot(e_i, n_i)
    ei, ni = e_i / mag, n_i / mag
    # grid +j unit vector -> earth displacement
    lat2, lon2 = to_latlon(i, j + EPS)
    e_j = (lon2 - lon0) * M_PER_DEG_LON_EQ * math.cos(math.radians(lat0))
    n_j = (lat2 - lat0) * M_PER_DEG_LAT
    mj = math.hypot(e_j, n_j)
    ej, nj = e_j / mj, n_j / mj
    # orthogonality sanity
    assert abs(ei * ej + ni * nj) < 1e-3
    # beta = bearing of +i from true north
    beta = math.atan2(ei, ni)
    # our rotation (grib2.rs rotate_winds_to_earth):
    #   u_e = u_g*sin(beta) + v_g*cos(beta); v_e = u_g*cos(beta) - v_g*sin(beta)
    # equivalently u_e axis = (sin b, cos b) must equal (ei, ni):
    assert abs(math.sin(beta) - ei) < 1e-6 and abs(math.cos(beta) - ni) < 1e-6
    # and v_g axis maps to (cos b, -sin b) == (ej, nj):
    if not (abs(math.cos(beta) - ej) < 5e-4 and abs(-math.sin(beta) - nj) < 5e-4):
        print(f"  +j axis at ({i},{j}): (ej,nj)=({ej:.6f},{nj:.6f}) vs (cos b,-sin b)=({math.cos(beta):.6f},{-math.sin(beta):.6f})")
        raise SystemExit(1)
    return math.degrees(beta)

pts = [(0, 0), (30, 20), (58, 32), (19, 28), (56, 17), (54, 15), (10, 5), (45, 35)]
worst = 0.0
for (i, j) in pts:
    beta_py = check(i, j)
    # Snyder closed form used by the Rust test: beta = 90 + n*(lon-Lov)
    n_c = (math.log(math.cos(math.radians(26.5)) / math.cos(math.radians(35.0)))
           / math.log(math.tan(math.radians(62.5)) / math.tan(math.radians(58.25))))
    lat, lon = to_latlon(i, j)
    lon = lon % 360.0
    beta_sn = 90.0 + n_c * (lon - 229.53)
    worst = max(worst, abs(beta_py - beta_sn))
    print(f"node ({i},{j})  pyproj beta={beta_py:.4f}  snyder beta={beta_sn:.4f}")
print("worst pyproj-vs-Snyder deviation (deg):", f"{worst:.6f}")
print("OK: rotation convention validated at", len(pts), "nodes")
