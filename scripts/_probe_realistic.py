#!/usr/bin/env python3
"""End-to-end VALUE probes against a LIVE server over the realistic dataset.

Complements scripts/validate_realistic.py (which validates the FILES against
wgrib2): this checks that what the SERVER ANSWERS equals the same analytic
formula wgrib2 confirmed, i.e. decode -> store -> index -> interpolate -> JSON
with no drift. Queries are pinned with `run=` so a specific source file is
meant, and include three interpolation cases:

  * exact grid NODES (lat/lon chosen on the 0.25 deg lattice) -> exact value;
  * vertical: level 837.5 mb between stored 850 and 825 -> linear blend;
  * time: valid = run + 3 h between stored f000 and f006 -> linear blend.

Usage: _probe_realistic.py BASE_URL [DATASET_DIR]
Exits non-zero on the first mismatch and prints the offending query.
"""
import json
import sys
import urllib.request

NX, NY = 1440, 720
LEVELS = list(range(1000, 75, -25))
PARAMS = ("TMP", "HGT", "UGRD", "VGRD")


def base_of(pname, lv):
    if pname == "TMP":
        return 290 - (1000 - lv) // 4
    if pname == "HGT":
        return (1000 - lv) * 45 // 100
    if pname == "UGRD":
        return 5 + (1000 - lv) // 40
    return -3 - (1000 - lv) // 60


def expected(pname, lv, fh, i, j):
    return base_of(pname, lv) + ((j * NX + i) % 1024) + fh * 10


def get(base, qs):
    req = urllib.request.Request(base + "/v1/point?" + qs)
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.loads(r.read().decode())


def main():
    base = sys.argv[1].rstrip("/")
    pts = [(0, 0), (1439, 0), (719, 360), (3, 719)]
    runs = [("2026-10-19T00:00:00Z", 0), ("2026-10-19T06:00:00Z", 6),
            ("2026-10-19T12:00:00Z", 12)]
    n_ok = 0
    for (run_iso, run_h) in runs:
        for fh in (0, 6):
            # valid time = run + fh hours (hour arithmetic is trivial here: all
            # runs are 00/06/12Z on 2026-10-19, fh 0/6 stay inside the same day)
            hh = run_h + fh
            valid = "2026-10-19T%02d:00:00Z" % hh
            for (i, j) in pts:
                lat = 89.875 - j * 0.25
                lon = i * 0.25
                for pname in PARAMS:
                    for lv in (1000, 850, 500, 100):
                        qs = ("model=gfs&param={0}&level=isobaric:{1}"
                              "&lat={2:.3f}&lon={3:.3f}&valid={4}&run={5}"
                              ).format(pname, lv, lat, lon, valid, run_iso)
                        body = get(base, qs)
                        v = body["results"][0]["value"]
                        e = expected(pname, lv, fh, i, j)
                        if v is None or abs(v - e) > 1e-3:
                            print("MISMATCH {0}: got {1} want {2}".format(qs, v, e))
                            return 1
                        n_ok += 1
                    # vertical interpolation probe (one per param/fh/point set,
                    # at the midpoint of the 850..825 gap)
                    p_mid = 837.5
                    t = (850 - p_mid) / (850 - 825)
                    e_v = (1 - t) * expected(pname, 850, fh, pts[0][0], pts[0][1]) \
                        + t * expected(pname, 825, fh, pts[0][0], pts[0][1])
                    qs = ("model=gfs&param={0}&level=isobaric:837.5"
                          "&lat=89.875&lon=0.0&valid={1}&run={2}").format(
                              pname, valid, run_iso)
                    v = get(base, qs)["results"][0]["value"]
                    if v is None or abs(v - e_v) > 1e-3:
                        print("MISMATCH(vertical) {0}: got {1} want {2}".format(qs, v, e_v))
                        return 1
                    n_ok += 1
    # time interpolation probe: valid exactly between f000 and f006 of run 12Z
    # (fh 3 -> value offset 30, exact because the field is affine in fc_hour)
    qs = ("model=gfs&param=TMP&level=isobaric:850&lat=89.875&lon=0.0"
          "&valid=2026-10-19T15:00:00Z&run=2026-10-19T12:00:00Z")
    v = get(base, qs)["results"][0]["value"]
    e = expected("TMP", 850, 3, 0, 0)
    if v is None or abs(v - e) > 1e-3:
        print("MISMATCH(time) {0}: got {1} want {2}".format(qs, v, e))
        return 1
    n_ok += 1
    print("_probe_realistic: {0} probes OK (nodes + vertical 837.5 + time fh3)".format(n_ok))
    return 0


if __name__ == "__main__":
    sys.exit(main())
