#!/usr/bin/env python3
"""Regenerate tests/fixtures/writer_golden_v1.bin — the V1.2 phase A GOLDEN
for the storage writer (spec §4.5 format, written INDEPENDENTLY of Rust so the
writer equivalence test has an outside truth source).

Input (must match crates/wx-core/tests/writer_golden.rs exactly):
  nx=7 ny=5, levels [1000.0, 837.5, 500.0] hPa (canonical DESCENDING order),
  scan_ns=true (flags bit0), winds_grid=false, plane(li,j,i) = li*1000+j*10+i.

Layout: magic "WXPTBIN1" | u32 version | u32 nx | u32 ny | u16 nlevels |
u8 flags | u8 reserved | nlevels x f32 LE level coords | planes f32 LE
[level][j][i].

The bytes this produces are IDENTICAL to what the pre-change Rust writer emitted
(cross-checked when the fixture was first captured on 2026-10-21).
"""

import struct
import sys
import hashlib

MAGIC = b"WXPTBIN1"
VERSION = 1
NX, NY = 7, 5
LEVELS = [1000.0, 837.5, 500.0]
FLAGS = 0x01  # bit0 = scan_ns
OUT = "tests/fixtures/writer_golden_v1.bin"


def main():
    data = MAGIC + struct.pack("<IIIHBB", VERSION, NX, NY, len(LEVELS), FLAGS, 0)
    for lv in LEVELS:
        data += struct.pack("<f", lv)
    for li in range(len(LEVELS)):
        for j in range(NY):
            for i in range(NX):
                data += struct.pack("<f", float(li * 1000 + j * 10 + i))
    with open(OUT, "wb") as fh:
        fh.write(data)
    print("wrote {0} bytes to {1}".format(len(data), OUT))
    print("sha256 {0}".format(hashlib.sha256(data).hexdigest()))


if __name__ == "__main__":
    sys.exit(main())
