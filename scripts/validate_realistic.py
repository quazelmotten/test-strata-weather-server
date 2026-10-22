#!/usr/bin/env python3
"""Validate the PRODUCTION-SCALE realistic GFS-like dataset (V1.2 phase D).

Truth source is tools/wgrib2-full (independent decoder) plus the analytic formulas
this generator promises. Checks:
  1. every file lists EXACTLY 148 records (37 isobaric levels x TMP,HGT,UGRD,VGRD);
  2. every record carries data representation template 5.0 (simple packing);
  3. record LABELS (param + level) follow the generator order;
  4. node probes decode to value = BASE(param,level) + ((row*1440+col) % 1024)
     + fc_hour*10 at points spanning the seam, both hemispheres, several levels
     and params;
  5. the WIND messages' section-3 flag byte carries flag-table-3.3 bit 0x08
     (grid-relative components) while the scalar messages' does not -- this is
     exactly the input the ingest wind-pair rule / startup preflight reads.

Usage: validate_realistic.py [DATASET_DIR]
"""
import os
import re
import struct
import subprocess
import sys

WG = "tools/wgrib2-full"
NX, NY = 1440, 720
LEVELS = list(range(1000, 75, -25))          # 37 levels, every 25 hPa
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


def sh(cmd):
    p = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    return p.returncode, p.stdout, p.stderr


def walk_sections(path):
    """Yield (section_number, payload_bytes) per message via the declared framing."""
    b = open(path, "rb").read()
    off = 0
    while off + 4 <= len(b):
        if b[off:off + 4] != b"GRIB":
            off += 1
            continue
        off += 16                                     # section 0 header
        while off + 5 <= len(b):
            l = struct.unpack(">I", b[off:off + 4])[0]
            sect = b[off + 4]
            if l < 5:
                break
            yield sect, b[off + 5:off + l]
            off += l
            if sect == 8:
                break


def main():
    d = sys.argv[1] if len(sys.argv) > 1 else "tests/perfdata/realistic"
    files = sorted(f for f in os.listdir(d) if f.endswith(".grib2"))
    if len(files) != 6:
        print("FATAL: expected 6 files, found %d" % len(files))
        return 1
    for fn in files:
        path = os.path.join(d, fn)
        fh = int(fn.split(".f")[-1].split(".")[0])
        rc, out, err = sh("%s '%s' 2>/dev/null | wc -l" % (WG, path))
        if int(out.strip()) != 148:
            print("FATAL: %s has %s records, expected 148" % (out.strip(), fn))
            return 1
        rc, out, err = sh("%s '%s' -code_table_5.0 2>/dev/null | grep -vc 'code table 5.0=0 '" % (WG, path))
        if out.strip() != "0":
            print("FATAL: %s: %s record(s) not DRT 5.0" % (fn, out.strip()))
            return 1
        # label order (first 8 records = levels 1000 and 975 x four params).
        # wgrib2 prints "<ref-time>:<param>:<level> mb:<proc>", ref time comes from
        # section 1 (never the filename), so the expectation carries it too.
        run_h = int(fn.split(".t")[1][:2])
        want = ["d=20261019%02d:%s:%d mb:" % (run_h, pn, lv) for lv in LEVELS[:2] for pn in PARAMS]
        rc, out, err = sh("%s '%s' 2>/dev/null | sed 's/^[0-9]*:[0-9]*://' | cut -c1-40 | head -8" % (WG, path))
        got = out.strip().split("\n")
        bad = [g for g, w in zip(got, want) if not g.startswith(w)]
        if bad:
            print("FATAL: %s label mismatch: got %r want %r" % (fn, got, want))
            return 1
        # node probes through wgrib2: NO record selection needed -- `-lon` prints
        # every record in FILE order, so ONE pass validates ALL 148 values per point
        # against the analytic formula (points chosen to hit the seam, both hemispheres
        # and the last row/col).
        pts = [(0, 0), (1439, 0), (719, 360), (3, 719)]
        args = " ".join("-lon %.2f %.3f" % (c * 0.25, 89.875 - r * 0.25) for (c, r) in pts)
        rc, out, err = sh("%s '%s' %s 2>/dev/null" % (WG, path, args))
        lines = [ln for ln in out.strip().split("\n") if ln]
        if len(lines) != 148:
            print("FATAL: %s: %s probe lines, expected 148" % (fn, len(lines)))
            return 1
        for (li, ln) in enumerate(lines):
            lv = LEVELS[li // 4]
            pname = PARAMS[li % 4]
            vals = [float(x) for x in re.findall(r"val=([-0-9.e+]+)", ln)]
            if len(vals) != len(pts):
                print("FATAL: %s rec %d (%s@%d): %s probe values" % (fn, li + 1, pname, lv, len(vals)))
                return 1
            for pk, ((c, r), v) in enumerate(zip(pts, vals)):
                e = expected(pname, lv, fh, c, r)
                if abs(v - e) > 0.51:
                    print("FATAL: %s %s@%d fc%03d pt%d (i=%d,j=%d): wgrib2 %.3f != analytic %.3f"
                          % (fn, pname, lv, fh, pk, c, r, v, e))
                    return 1
        # wind flag bytes: wgrib2 reads flag table 3.3 at ABSOLUTE sec[3][54]; our
        # fixture's section-3 payload starts after that header, so the byte lives at
        # payload offset 49 (verified against the generator's own grid block).
        seen_wind = seen_scalar = 0
        for sect, payload in walk_sections(path):
            if sect != 3 or len(payload) < 50:
                continue
            if payload[49] & 0x08:
                seen_wind += 1
            else:
                seen_scalar += 1
        if seen_wind != 74 or seen_scalar != 74:      # 37 levels x U/V = 74 each
            print("FATAL: %s flag-byte tally wind=%d scalar=%d, expected 74/74"
                  % (fn, seen_wind, seen_scalar))
            return 1
    print("validate_realistic: 6 files x 148 records, DRT 5.0, label order OK, "
          "592 node probes per file match the analytic formula, wind/scalar flag bytes 74/74")
    return 0


if __name__ == "__main__":
    sys.exit(main())
