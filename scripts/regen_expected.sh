#!/usr/bin/env bash
# Re-probe the reference wgrib2 binary against the fixtures and rewrite
# tests/fixtures/expected.json (the ground truth the Rust implementation is
# validated against). Idempotent. Uses ONLY the locally built wgrib2 --
# tools/wgrib2-full, the full-codec build, so every packing twin (complex
# packing, JPEG2000, PNG, CCSDS AEC, IEEE) can be probed too: the twins must
# produce byte-identical inventories and point values next to the simple-packed
# original.
set -euo pipefail
cd "$(dirname "$0")/.."
WG=tools/wgrib2-full
"$WG" -version >/dev/null 2>&1 || true   # not all builds have -version; ignore

python3 - "$WG" <<'PYEOF'
import subprocess, json, random, re, sys

WG = sys.argv[1]

def norm(obj):
    """Byte offsets inside recorded wgrib2 output lines legitimately change with
    the packing; normalize them away so twins can be compared exactly."""
    if isinstance(obj, str):
        return re.sub(r"^(\d+):(\d+):", r"\1:", obj)
    if isinstance(obj, list):
        return [norm(x) for x in obj]
    if isinstance(obj, dict):
        return {k: norm(v) for k, v in obj.items()}
    return obj


def strip_offsets(lines):
    """Inventory lines carry per-record byte offsets, which of course change
    when the packing changes; compare the semantic part only."""
    return [re.sub(r"^(\d+):(\d+):", r"\1:", ln) for ln in lines]


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

GFS_TWINS = ("gfs_small.grib2", "gfs_small_c3.grib2", "gfs_small_jpeg.grib2",
             "gfs_small_png.grib2", "gfs_small_png_f.grib2", "gfs_small_aec.grib2",
             "gfs_small_ieee.grib2")
for name in GFS_TWINS:
    fx = out["fixtures"].setdefault(name, {})
    fx["inventory_raw"] = probe(f"tests/fixtures/{name}")
    fx["point_checks"] = point_checks(name, gfs_records, GFS_PTS)

# spec \u00a79.1 at the reference-tool level: every packing twin is
# indistinguishable from the simple-packed original (masked nodes come back as
# UNDEFINED, which point_checks records as value=None/masked=true).
gb = out["fixtures"]["gfs_small.grib2"]
for name in GFS_TWINS[1:]:
    fx = out["fixtures"][name]
    assert norm(fx["inventory_raw"]) == norm(gb["inventory_raw"]), \
        f"{name}: inventory differs"
    assert norm(fx["point_checks"]) == norm(gb["point_checks"]), \
        f"{name}: point checks differ"

# ---------------- hrrr_small & twin (LCC projection + values) ----------
HRR = "tests/fixtures/hrrr_small.grib2"
NX, NY = 61, 41
LA1, LO1 = 21.11389, 233.44

def ij_checks(fname):
    # fresh RNG per file: every HRRR twin gets the SAME probe points, so their
    # recorded values must be identical across packings.
    rnd = random.Random(20261019)
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

HRRR_TWINS = ("hrrr_small.grib2", "hrrr_small_c3.grib2", "hrrr_small_jpeg.grib2",
              "hrrr_small_png.grib2", "hrrr_small_png_f.grib2", "hrrr_small_aec.grib2",
              "hrrr_small_ieee.grib2")
for name in HRRR_TWINS:
    fx = out["fixtures"].setdefault(name, {})
    fx["inventory_raw"] = probe(f"tests/fixtures/{name}")
    fx["ij_checks"] = ij_checks(f"tests/fixtures/{name}")

hb = out["fixtures"]["hrrr_small.grib2"]
for name in HRRR_TWINS[1:]:
    fx = out["fixtures"][name]
    assert strip_offsets(fx["inventory_raw"]) == strip_offsets(hb["inventory_raw"]), \
        f"{name}: inventory differs"
    assert fx["ij_checks"] == hb["ij_checks"], f"{name}: ij checks differ"

# ---------------- stat_small (PDT 4.8) + its JPEG2000 twin ---------------
def stat_truth(name):
    fname = f"tests/fixtures/{name}"
    entry = {
        "inventory_raw": probe(fname),
        "point_checks": [],
    }
    stat_pts = [(240.0, 20.0), (238.0, 18.0), (235.0, 17.0)]
    lines_by_rec = {}
    for lon, lat in stat_pts:
        lines = probe(fname, "-lon", str(lon), str(lat))
        for idx, ln in enumerate(lines):
            plon, plat, val = parse_lon(ln)
            lines_by_rec.setdefault(idx, []).append((lon, lat, val))
    for idx in sorted(lines_by_rec):
        inv_line = entry["inventory_raw"][idx]
        for lon, lat, val in lines_by_rec[idx]:
            entry["point_checks"].append({
                "record_index": idx + 1, "inv_line": inv_line,
                "req_lon": lon, "req_lat": lat,
                "value": None if val.startswith("9.999e+20") else float(val),
                "masked": val.startswith("9.999e+20")})
    return entry

out["fixtures"]["stat_small.grib2"] = stat_truth("stat_small.grib2")
out["fixtures"]["stat_small_jpeg.grib2"] = stat_truth("stat_small_jpeg.grib2")
assert (norm(out["fixtures"]["stat_small.grib2"]) ==
        norm(out["fixtures"]["stat_small_jpeg.grib2"])), "PDT 4.8 twin differs"

with open("tests/fixtures/expected.json", "w") as f:
    json.dump(out, f, indent=1)
print("expected.json rewritten")
PYEOF
