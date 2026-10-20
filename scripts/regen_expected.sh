#!/usr/bin/env bash
# Re-probe the reference wgrib2 binary against the fixtures and rewrite
# tests/fixtures/expected.json (the ground truth the Rust implementation is
# validated against). Idempotent. Uses ONLY the locally built wgrib2.
set -euo pipefail
WG=/home/frankzappa/dylantest2/tools/wgrib2
cd "$(dirname "$0")/.."
"$WG" -version >/dev/null 2>&1 || true   # not all builds have -version; ignore

python3 - "$WG" <<'PYEOF'
import subprocess, json, random, sys

WG = sys.argv[1]

def probe(fname, *args):
    """Run wgrib2 FILE <args...>; returns list of non-empty stdout lines."""
    r = subprocess.run([WG, fname] + list(args), capture_output=True, text=True)
    return [ln.strip() for ln in (r.stdout or "").splitlines() if ln.strip()]

def parse_lon(line):
    # "N:off:lon=...,lat=...,val=..."
    tail = line.split(":", 2)[2]
    kv = dict(p.split("=", 1) for p in tail.split(","))
    return float(kv["lon"]), float(kv["lat"]), kv["val"]

out = {"note": "reference values probed from the locally built wgrib2",
       "fixtures": {}}

# ---------------- gfs_small ----------------
GFS = "tests/fixtures/gfs_small.grib2"
inv = probe(GFS)   # full inventory, one line per record, in order
def inv_index(label_pat):
    import re
    for n, ln in enumerate(inv):
        if re.search(label_pat, ln):
            return n
    return None
gfs_records = [
    ("TMP_1000mb_f000", r":TMP:1000 mb:anl:"),
    ("TMP_1000mb_f006", r":TMP:1000 mb:6 hour fcst:"),
    ("TMP_850mb_f006_masked", r":TMP:850 mb:6 hour fcst:"),
    ("TMP_700mb_f012", r":TMP:700 mb:12 hour fcst:"),
    ("TMP_agl2m_f000", r":TMP:2 m above ground:anl:"),
]
gfs_points = [(350.0, 10.0), (10.0, 10.0), (350.0, 0.0), (10.0, 0.0),
              (0.0, 5.0), (359.9, 5.0), (-0.1, 5.0), (0.0, 0.0)]
checks = []
for lon, lat in gfs_points:
    lines = probe(GFS, "-lon", str(lon), str(lat))
    parsed = [parse_lon(ln) for ln in lines]
    for label, pat in gfs_records:
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
out["fixtures"]["gfs_small.grib2"] = {"point_checks": checks}

# ---------------- hrrr_small (LCC projection + values) ----------------
HRR = "tests/fixtures/hrrr_small.grib2"
NX, NY = 61, 41
LA1, LO1 = 21.11389, 233.44
ij_checks = []
rnd = random.Random(20261019)
hrr_inv = probe(HRR)
tmp_f000_idx = next(n for n, ln in enumerate(hrr_inv) if ":TMP:" in ln and ":anl:" in ln)
def check_pt(lon, lat):
    lines = probe(HRR, "-ll2ij", str(lon), str(lat))
    if not lines:
        return None
    tail = lines[0].split(":", 2)[2]
    ij = tail.split("->")[1].strip().lstrip("(").rstrip(")")
    i, j = (float(x) for x in ij.split(","))
    if not (1.0 <= i <= NX and 1.0 <= j <= NY):
        return None
    vlines = probe(HRR, "-lon", str(lon), str(lat))
    val = raw = None
    if tmp_f000_idx < len(vlines):
        _, _, rawv = parse_lon(vlines[tmp_f000_idx])
        raw = vlines[tmp_f000_idx]
        val = None if rawv.startswith("9.999e+20") else float(rawv)
    return {"lon": lon, "lat": lat, "i": i, "j": j,
            "nearest_value_TMP_f000": val, "raw_ij": lines[0], "raw_val": raw}

rec = check_pt(LO1, LA1)
if rec:
    ij_checks.append(rec)
tries = 0
while len(ij_checks) < 25 and tries < 4000:
    tries += 1
    lo = round(LO1 - 6.0 + rnd.random() * 8.0, 5)
    la = round(LA1 - 1.2 + rnd.random() * 1.3, 5)
    rec = check_pt(lo, la)
    if rec:
        ij_checks.append(rec)
out["fixtures"]["hrrr_small.grib2"] = {"ij_checks": ij_checks}

with open("tests/fixtures/expected.json", "w") as f:
    json.dump(out, f, indent=1)
print(f"expected.json: {len(checks)} gfs point checks, {len(ij_checks)} hrrr ij/value checks")
PYEOF
