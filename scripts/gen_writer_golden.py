#!/usr/bin/env python3
"""Regenerate the storage-writer GOLDEN fixtures (spec §4.5 format, written
INDEPENDENTLY of Rust so the writer equivalence test has an outside truth source):

  tests/fixtures/writer_golden_v1.bin  BIN_VERSION 1 layout (24-byte fixed header)
      The historical artifact captured from the V1.1 writer on 2026-02-21 and
      re-confirmed by phases A/D. KEPT as the OLD-vs-NEW reference: the Rust
      golden test asserts this file is REJECTED by the current reader
      (`HeaderError::BadVersion`) instead of being mis-parsed.

  tests/fixtures/writer_golden_v2.bin  BIN_VERSION 2 layout (56-byte fixed header)
      Current since V1.3 phase E0, when the header gained the PDT 4.8
      statistical-process metadata (stat_kind, stat_processed and the
      begin/end/step window in seconds relative to the run time). This is what
      `write_bin_atomic*` must emit BYTE FOR BYTE.

Input (must match crates/wx-core/tests/writer_golden.rs exactly):
  nx=7 ny=5, levels [1000.0, 837.5, 500.0] hPa (canonical DESCENDING order),
  scan_ns=true (flags bit0), winds_grid=false, plane(li,j,i) = li*1000+j*10+i.
  v2 additionally carries an ACCUMULATION window: stat_processed=1,
  stat_kind=1 (accumulation), begin_secs=0, end_secs=21600, step_secs=0 — i.e.
  the 0-6 h APCP shape the guard exists for, so the golden pins the i64 fields
  with non-zero values rather than a row of zeros.

Layouts (all little-endian):
  v1: magic "WXPTBIN1" | u32 version=1 | u32 nx | u32 ny | u16 nlevels |
      u8 flags | u8 reserved | nlevels x f32 level coords | planes f32 [level][j][i]
  v2: magic "WXPTBIN1" | u32 version=2 | u32 nx | u32 ny | u16 nlevels |
      u8 flags | u8 stat_kind | u8 stat_processed | u8[3] reserved |
      i64 stat_begin_secs | i64 stat_end_secs | i64 stat_step_secs |
      u32 reserved | nlevels x f32 level coords | planes f32 [level][j][i]
"""

import struct
import sys
import hashlib

MAGIC = b"WXPTBIN1"
NX, NY = 7, 5
LEVELS = [1000.0, 837.5, 500.0]
FLAGS = 0x01  # bit0 = scan_ns
# v2 statistical metadata: 0-6 h accumulation, offsets relative to the run time
STAT_KIND = 1        # STAT_ACCUMULATION
STAT_PROCESSED = 1   # true
STAT_BEGIN = 0
STAT_END = 21600
STAT_STEP = 0

OUT_V1 = "tests/fixtures/writer_golden_v1.bin"
OUT_V2 = "tests/fixtures/writer_golden_v2.bin"


def planes():
    data = b""
    for li in range(len(LEVELS)):
        for j in range(NY):
            for i in range(NX):
                data += struct.pack("<f", float(li * 1000 + j * 10 + i))
    return data


def emit(path, blob):
    with open(path, "wb") as fh:
        fh.write(blob)
    print("wrote {0} bytes to {1}".format(len(blob), path))
    print("sha256 {0}".format(hashlib.sha256(blob).hexdigest()))


def main():
    head_v1 = MAGIC + struct.pack("<IIIHBB", 1, NX, NY, len(LEVELS), FLAGS, 0)
    head_v2 = (MAGIC + struct.pack("<IIIHBBBB", 2, NX, NY, len(LEVELS), FLAGS,
                                   STAT_KIND, STAT_PROCESSED, 0)
               + b"\x00\x00"                      # remaining 2 reserved bytes
               + struct.pack("<qqqI", STAT_BEGIN, STAT_END, STAT_STEP, 0))
    lv = b"".join(struct.pack("<f", x) for x in LEVELS)
    body = lv + planes()
    emit(OUT_V1, head_v1 + body)
    emit(OUT_V2, head_v2 + body)
    return 0


if __name__ == "__main__":
    sys.exit(main())
