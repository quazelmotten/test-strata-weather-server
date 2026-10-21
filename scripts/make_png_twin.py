#!/usr/bin/env python3
"""Repack a simple-packed GRIB2 fixture as GRIB2 template 5.41 (PNG packing).

wgrib2 v3.1.3 DECODES 5.41 but cannot ENCODE it (no PNG writer; enc_png lives in
g2clib). Layout facts read off the wgrib2 source (authoritative here):

  * the data representation template number lives in SECTION 5, octets 10-11
    (code_table_5_0() == uint2(sec[5]+9)); section 7 has NO template byte.
  * section layout is [u32 length-including-itself][section number][payload...]
    (rd_seq_grib.c walks p += uint4(p) and tests p[4]).
  * png path: nbits = sec[5][19] (Sec5 bits-per-value) and the PNG stream starts
    at sec[7]+5, i.e. immediately after the section number -- no PSV field.
    dec_png_clone cross-checks the PNG IHDR bit depth against Sec5 octet 20 and
    raises a DELAYED ERROR on mismatch, so both must agree.

So: Sec5 gets DRS=41 and a widened bits-per-value (8 or 16); Sec7 becomes
[u32 len][7][PNG stream]: grayscale, depth = bits-per-value, width = nvals,
height = 1, filter type 0 per row, big-endian samples. Reference value and
scales are untouched, so decoded values are IDENTICAL. Section 6 is copied
untouched (masked messages keep their bitmap; width = defined-value count).
"""
import struct, sys, zlib

def chunk(typ, data):
    return struct.pack(">I", len(data)) + typ + data + struct.pack(
        ">I", zlib.crc32(typ + data) & 0xFFFFFFFF)

def png_gray(raw, depth, width):
    rowlen = width * depth // 8
    rows = b"".join(b"\x00" + raw[i:i+rowlen] for i in range(0, len(raw), rowlen))
    ihdr = struct.pack(">IIbbbbb", width, 1, depth, 0, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\x0a" + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(rows, 6)) + chunk(b"IEND", b""))

def u4(b, o):  return int.from_bytes(b[o:o+4], "big")

def unpack_bits(buf, n, nbits):
    vals, pos = [], 0
    for _ in range(n):
        v = 0
        for _ in range(nbits):
            v = (v << 1) | ((buf[pos >> 3] >> (7 - (pos & 7))) & 1)
            pos += 1
        vals.append(v)
    return vals

def pack_bits(vals, nbits):
    out = bytearray((len(vals) * nbits + 7) // 8)
    pos = 0
    for v in vals:
        for b in range(nbits - 1, -1, -1):
            if (v >> b) & 1:
                out[pos >> 3] |= 0x80 >> (pos & 7)
            pos += 1
    return bytes(out)

src = open(sys.argv[1], "rb").read()
out, p, conv, skip = bytearray(), 0, 0, 0
while p < len(src):
    assert src[p:p+4] == b"GRIB", f"bad magic at {p}"
    msglen = (u4(src, p + 8) << 32) | u4(src, p + 12)
    secs, q = [], p + 16
    while q < p + msglen:
        if src[q:q+4] == b"7777":
            secs.append(b"7777"); q += 4; break
        ln = u4(src, q)
        secs.append(src[q:q+ln]); q += ln

    newsecs, sec5i = [], None
    for sec in secs:
        if sec[:4] == b"7777":
            newsecs.append(sec); continue
        num = sec[4]
        if num == 5:
            newsecs.append(bytearray(sec)); sec5i = len(newsecs) - 1
            continue
        if num == 7 and sec5i is not None and int.from_bytes(newsecs[sec5i][9:11], "big") == 0:
            sec5 = newsecs[sec5i]
            nvals, nbits = u4(sec5, 5), sec5[19]
            vals = unpack_bits(sec[5:] + b"\0\0", nvals, nbits)
            nb = next(w for w in (8, 16, 24, 32) if w >= nbits)
            assert max(vals) < (1 << nb), "value span does not fit"
            sec5[9:11] = b"\x00\x29"          # DRS template 5.41
            sec5[19] = nb                     # bits per value == PNG bit depth
            packed = pack_bits(vals, nb).rjust(nvals * nb // 8, b"\0")
            png = png_gray(packed, nb, nvals)
            newsecs.append(struct.pack(">I", 5 + len(png)) + b"\x07" + png)
            conv += 1
            continue
        if num == 7:
            skip += 1
        newsecs.append(sec)
    body = b"".join(bytes(s) for s in newsecs)
    out += src[p:p+8] + struct.pack(">Q", 16 + len(body)) + body
    p += msglen
open(sys.argv[2], "wb").write(out)
print(f"converted {conv} messages, left {skip} section-7s unchanged")
