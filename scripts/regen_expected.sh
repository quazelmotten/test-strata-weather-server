#!/usr/bin/env bash
# Re-probe the reference wgrib2 binary against the fixtures and rewrite
# tests/fixtures/expected.json (the ground truth the Rust implementation is
# validated against). Idempotent. Uses ONLY the locally built wgrib2.
set -euo pipefail
cd "$(dirname "$0")/.."
WG=tools/wgrib2
"$WG" -version >/dev/null 2>&1 || true   # not all builds have -version; ignore

python3 - "$WG" <<'PYEOF'
import subprocess, json, random, sys

WG = sys.argv[1]

def probe(fname, *args):
    """Run wgrib2 FILE <args...>; returns list of non-empty stdout lines."""
    r = subprocess.run([WG, fname] + list(args), capture_output=True, text=True)
    return [ln.strip() for ln in (r.stdout or "").splitlines() if ln.strip()]

def parse_lon(line):
    tail = line.split(":", 2)[2]
    kv = dict(p.split("=", 1) for p in tail.split(","))
    return float(kv["lon"]), float(kv["lat"]), kv["val"]

out = {"note": "reference values probed from the locally built wgrib2",
       "fixtures": {}}

def add_inventory(name):
    """Record the raw -level 0 inventory so the Rust decoder can be checked
    message-by-message against wgrib2's own interpretation."""
    fx = out["fixtures"].setdefault(name, {})
    fx["inventory_raw"] = probe(f"tests/fixtures/{name}")

# ---------------- gfs_small & its complex-packing twin ----------------
GFS_PTS = [(350.0, 10.0), (10.0, 10.0), (350.0, 0.0), (10.0, 0.0),
           (0.0, 5.0), (359.9, 5.0), (-0.1, 5.0), (0.0, 0.0)]
gfs_records = [
    ("TMP_1000mb_f000", r":TMP:1000 mb:anl:"),
    ("TMP_1000mb_f006", r":TMP:1000 mb:6 hour fcst:"),
    ("TMP_850mb_f006_masked", r":TMP:850 mb:6 hour fcst:"),
    ("TMP_700mb_f012", r":TMP:700 mb:12 hour fcst:"),
    ("TMP_agl2m_f000", r":TMP:2 m above ground:anl:"),
]

def point_checks(name, records, pts):
    fname = f"tests/fixtures/{name}"
    inv = probe(fname)
    def inv_index(pat):
        import re
        for n, ln in enumerate(inv):
            if re.search(pat, ln):
                return n
        return None
    checks = []
    for lon, lat in pts:
        lines = probe(fname, "-lon", str(lon), str(lat))
        parsed = [parse_lon(ln) for ln in lines]
        for label, pat in records:
            idx = inv_index(pat)
            if idx is None or idx >= len(parsed):
                continue
            plon, plat, val = parsed[idx]
            checks.append({"record": label, "record_index": idx + 1,
                           "req_lon": lon, "req_lat": lat,
                           "snapped_lon": plon, "snapped_lat": plat,
                           "value": None if val.startswith("9.999e+20") else float(val),
                           "masked": val.startswith("9.999e+20"),
                           "raw": lines[idx]})
    return checks

for name in ("gfs_small.grib2", "gfs_small_c3.grib2"):
    fx = out["fixtures"].setdefault(name, {})
    fx["inventory_raw"] = probe(f"tests/fixtures/{name}")
    fx["point_checks"] = point_checks(name, gfs_records, GFS_PTS)

# ---------------- hrrr_small & twin (LCC projection + values) ----------
HRR = "tests/fixtures/hrrr_small.grib2"
NX, NY = 61, 41
LA1, LO1 = 21.11389, 233.44
rnd = random.Random(20261019)

def ij_checks(fname):
    hrr_inv = probe(fname)
    tmp_f000_idx = next(n for n, ln in enumerate(hrr_inv) if ":TMP:" in ln and ":anl:" in ln)
    def check_pt(lon, lat):
        lines = probe(fname, "-ll2ij", str(lon), str(lat))
        if not lines:
            return None
        tail = lines[0].split(":", 2)[2]
        ij = tail.split("->")[1].strip().lstrip("(").rstrip(")")
        i, j = (float(x) for x in ij.split(","))
        if not (1.0 <= i <= NX and 1.0 <= j <= NY):
            return None
        vlines = probe(fname, "-lon", str(lon), str(lat))
        val = raw = None
        if tmp_f000_idx < len(vlines):
            _, _, rawv = parse_lon(vlines[tmp_f000_idx])
            raw = vlines[tmp_f000_idx]
            val = None if rawv.startswith("9.999e+20") else float(rawv)
        return {"lon": lon, "lat": lat, "i": i, "j": j,
                "nearest_value_TMP_f000": val, "raw_ij": lines[0], "raw_val": raw}
    pts = []
    rec = check_pt(LO1, LA1)
    if rec:
        pts.append(rec)
    tries = 0
    while len(pts) < 25 and tries < 4000:
        tries += 1
        lo = round(LO1 - 6.0 + rnd.random() * 8.0, 5)
        la = round(LA1 - 1.2 + rnd.random() * 1.3, 5)
        rec = check_pt(lo, la)
        if rec:
            pts.append(rec)
    return pts

for name in ("hrrr_small.grib2", "hrrr_small_c3.grib2"):
    fx = out["fixtures"].setdefault(name, {})
    fx["inventory_raw"] = probe(f"tests/fixtures/{name}")
    fx["ij_checks"] = ij_checks(f"tests/fixtures/{name}")

# ---------------- stat_small (PDT 4.8) ---------------------------------
STAT = "tests/fixtures/stat_small.grib2"
out["fixtures"]["stat_small.grib2"] = {
    "inventory_raw": probe(STAT),
    "point_checks": [],
}
stat_pts = [(240.0, 20.0), (238.0, 18.0), (235.0, 17.0)]
lines_by_rec = {}
for lon, lat in stat_pts:
    lines = probe(STAT, "-lon", str(lon), str(lat))
    for idx, ln in enumerate(lines):
        plon, plat, val = parse_lon(ln)
        lines_by_rec.setdefault(idx, []).append((lon, lat, val))
for idx in sorted(lines_by_rec):
    inv_line = out["fixtures"]["stat_small.grib2"]["inventory_raw"][idx]
    for lon, lat, val in lines_by_rec[idx]:
        out["fixtures"]["stat_small.grib2"]["point_checks"].append({
            "record_index": idx + 1, "inv_line": inv_line,
            "req_lon": lon, "req_lat": lat,
            "value": None if val.startswith("9.999e+20") else float(val),
            "masked": val.startswith("9.999e+20")})

with open("tests/fixtures/expected.json", "w") as f:
    json.dump(out, f, indent=1)
print("expected.json rewritten")
PYEOF
