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
      u32 header_crc | nlevels x f32 level coords | planes f32 [level][j][i]

The final u32 of the fixed header was "reserved, must be zero" up to V1.3 E0.
Since V1.4 phase C it is the header integrity word (`crc_slot` above); the byte
COUNT did not change, so the fixture stays 488 bytes and the only difference
versus the pre-phase-C artifact is those four bytes. BIN_VERSION deliberately
stays 2: the reader accepts both shapes (zero slot = legacy, unsealed), while
writers always seal, so an existing store keeps answering across the upgrade
instead of being deleted and re-decoded by the pipeline's version trap.
"""

import struct
import sys
import hashlib
import zlib

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


def crc_slot(region):
    """V1.4 phase C integrity word for the field header (V1.2-era behaviour
    unchanged for v1, which has no slot). CRC-32 (reflected IEEE / zlib
    polynomial — the same digest GRIB2 section 7 and `zlib.crc32` produce) over
    the fixed header EXCLUDING its own four bytes at offset 52, concatenated
    with the level coordinates. A raw digest of exactly 0 is stored as 1,
    because zero means 'no CRC present' (a pre-phase-C file); mirrors
    `wx_core::storage::crc_slot_value`."""
    c = zlib.crc32(region) & 0xFFFFFFFF
    return 1 if c == 0 else c


def main():
    head_v1 = MAGIC + struct.pack("<IIIHBB", 1, NX, NY, len(LEVELS), FLAGS, 0)
    pre_v2 = (MAGIC + struct.pack("<IIIHBBBB", 2, NX, NY, len(LEVELS), FLAGS,
                                  STAT_KIND, STAT_PROCESSED, 0)
              + b"\x00\x00"                     # remaining 2 reserved bytes
              + struct.pack("<qqq", STAT_BEGIN, STAT_END, STAT_STEP))
    lv = b"".join(struct.pack("<f", x) for x in LEVELS)
    body = lv + planes()
    assert len(pre_v2) == 52, "integrity slot must start at byte 52"
    head_v2 = pre_v2 + struct.pack("<I", crc_slot(pre_v2 + lv))
    assert len(head_v2) == 56, "v2 fixed header stays 56 bytes"
    emit(OUT_V1, head_v1 + body)
    blob_v2 = head_v2 + body
    emit(OUT_V2, blob_v2)
    # The Rust gate (crates/wx-core/tests/writer_golden.rs) embeds the expected
    # bytes as hex so a mutated fixture cannot pass silently; print them here in
    # exactly the shape the test needs (160 hex chars = 80 bytes per line).
    print("v2 bytes as Rust string-literal lines:")
    hx = blob_v2.hex()
    for k in range(0, len(hx), 160):
        print('    hx += "{0}";'.format(hx[k:k + 160]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
