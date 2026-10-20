#!/usr/bin/env python3
"""
GRIB2 message writer, byte-exact against tools/wgrib2 v3.1.3.

All layouts were read off the wgrib2 source (authoritative for this project,
since fixtures are validated with the built binary):

  * Section framing:      LEN(uint4 BE, includes itself) | SECT#(1) | payload
                          END section: LEN=6, sect#=8, data byte 0, then the
                          four ASCII '7' terminator bytes counted INSIDE msglen.
  * Section 0 magic:      "GRIB" (NOT "GRID"!) + version(0) + unused(0)
                          + discipline + edition(2) + uint8 msglen.
                          Confirmed by rd_seq_grib.c (#define G/R/I/B 71/82/73/66,
                          c4 == 2) and the NCEP spec page for Section 0.
  * Section 1 (tmpl 1.0): offsets from CodeTable.c location functions.
  * Section 3 tmpl 3.0 / 3.30: sec3_grids.c sec3_lola()/sec3_lc() verbatim.
  * Section 4 PDT 4.0: pdt_len() case 0 => LEN = 34 + 4*n_after; levels are
                          CODE TABLE 4.5 FIXED SURFACES at octets 23-28
                          (not coordinate-list entries!).
  * Section 5 (tmpl 5.0): simple_pk.c mk_sec5and7(): LEN=21, uint4 defined-point
                          count at [5..8] (NCEP spec has NO dictionary pointer),
                          uint2 template at [9..10], IEEE ref at [11..14],
                          int2 binary scale E at [15..16], int2 S at [17..18]
                          (stored as -dec_scale), N bits at [19], reserved 0.
                          Decode (unpk_0.c): v = (R + n * 2^E) * 10^(-S).
  * Section 6: indicator byte at [5] (0 = bitmap follows), bitmap from [6],
                          bit=1 means point DEFINED (missing.c bitsum table),
                          MSB-first. LEN = 6 + ceil(npts/8).
  * Section 7: bitstream of packed ints for DEFINED points only, MSB-first.

Field values are generated analytically: every value is
    base + incr * 2^E
with integer incr, so f32 round-trip is exact and expected values can be
recomputed independently.

Usage: gen_grib2.py --out DIR [--perf-out DIR]
"""

import struct
import math
import argparse


def u4(v):
    return struct.pack(">I", v)


def i4(v):
    return struct.pack(">i", v)


def u2(v):
    return struct.pack(">H", v)


def i2(v):
    return struct.pack(">h", v)


def f4be(v):
    return struct.pack(">f", v)


def i4sm(v):
    """Signed int4 as read by wgrib2 int8.c int4(): SIGN-MAGNITUDE big-endian
    (bit 31 = sign, bits 0..30 = magnitude). NOT two's complement!"""
    sm = (abs(v) & 0x7FFFFFFF) | (0x80000000 if v < 0 else 0)
    return struct.pack(">I", sm)


def i2sm(v):
    """Same sign-magnitude convention for int2 fields (wgrib2 int2())."""
    sm = (abs(v) & 0x7FFF) | (0x8000 if v < 0 else 0)
    return struct.pack(">H", sm)


def section(num, payload):
    body = bytes([num]) + payload
    return u4(len(body) + 4) + body


def end_section():
    # LEN=6 covers [LEN|sect#|data]; the "7777" terminator is additional
    # but still counted inside the message length (rd_seq_grib.c checks
    # buffer+len-4 == '7777').
    return u4(6) + bytes([8, 0]) + b"7777"


def sec0(discipline, msglen):
    # octets 9-16: total message length as uint64 BE (spec Section 0)
    return b"GRIB" + bytes([0, 0, discipline, 2]) + struct.pack(">Q", msglen)


def sec1(year, month, day, hour, minute, second):
    p = bytearray()
    p += u2(7)          # center: NCEP
    p += u2(0)          # subcenter
    p += bytes([125])   # master tables version (ct 1.0)
    p += bytes([0])     # local tables version (ct 1.1)
    p += bytes([1])     # ct 1.2 significance of ref time: forecast
    p += u2(year) + bytes([month, day, hour, minute, second])
    p += bytes([128])   # ct 1.3 production status: operational
    p += bytes([1])     # ct 1.4 type of processed data: forecast
    p += u2(0)          # identification template number: 1.0
    p += bytes([1])     # ct 1.6 calendar definition: Gregorian
    return section(1, bytes(p))


def sec3_latlon(nx, ny, lon1_deg, lat1_deg, dlon_deg, dlat_deg, wind_grid=False):
    g = bytearray(72)
    g[0:4] = u4(72)
    g[4] = 3
    g[5] = 0                                   # source of grid def template
    g[6:10] = u4(nx * ny)                      # number of data points
    g[10] = 0                                  # octets of optional number list
    g[11] = 0                                  # (list, always one byte present)
    g[12] = 0
    g[13] = 0                                  # template 3.0
    # shape of earth (NCEP default, sec3_grids.c)
    g[14] = 6
    g[15] = 0
    g[16:20] = u4(0)
    g[20] = 0
    g[21:25] = u4(0)
    g[25] = 0
    g[26:30] = u4(0)
    E6 = 1000000
    g[30:34] = u4(nx)
    g[34:38] = u4(ny)
    g[38:42] = i4sm(0)                        # basic angle increment
    g[42:46] = u4(0xFFFFFFFF)                  # sub-angle: undefined
    g[46:50] = i4sm(int(math.floor(lat1_deg * E6 + 0.5)))
    g[50:54] = u4(int(math.floor(lon1_deg * E6 + 0.5)))
    flag33 = 32 + 16                           # scalar; no rotation needed
    if wind_grid:
        flag33 |= 8                            # winds relative to earth tangent? -> grid
    g[54] = flag33
    lat2 = lat1_deg + (ny - 1) * dlat_deg
    lon2 = lon1_deg + (nx - 1) * dlon_deg
    while lon2 >= 360.0:
        lon2 -= 360.0
    while lon2 < 0.0:
        lon2 += 360.0
    g[55:59] = i4sm(int(math.floor(lat2 * E6 + 0.5)))
    g[59:63] = u4(int(math.floor(lon2 * E6 + 0.5)))
    g[63:67] = i4sm(int(math.floor(abs(dlon_deg) * E6 + 0.5)))
    g[67:71] = i4sm(int(math.floor(abs(dlat_deg) * E6 + 0.5)))
    scan = 0
    if dlon_deg < 0.0:
        scan |= 128
    if dlat_deg > 0.0:
        scan |= 64
    g[71] = scan
    return bytes(g)


def sec3_lambert(nx, ny, la1, lo1, lov, lad, latin1, latin2, dx_m, dy_m,
                 wind_grid=False):
    g = bytearray(81)
    g[0:4] = u4(81)
    g[4] = 3
    g[5] = 0
    g[6:10] = u4(nx * ny)
    g[10] = 0
    g[11] = 0
    g[12] = 0
    g[13] = 30                                 # template 3.30 Lambert conformal
    g[14] = 6                                  # earth shape (NCEP default)
    g[15] = 0
    g[16:20] = u4(0)
    g[20] = 0
    g[21:25] = u4(0)
    g[25] = 0
    g[26:30] = u4(0)
    E6 = 1000000
    g[30:34] = u4(nx)
    g[34:38] = u4(ny)
    g[38:42] = i4sm(int(math.floor(la1 * E6 + 0.5)))
    g[42:46] = u4(int(math.floor(lo1 * E6 + 0.5)))
    g[46] = 32 + 16                            # flag table 3.3
    if wind_grid:
        g[46] |= 8                             # grid-relative winds (FlagTable.c: res&8)
    g[47:51] = i4sm(int(math.floor(lad * E6 + 0.5)))
    g[51:55] = u4(int(math.floor(lov * E6 + 0.5)))
    g[55:59] = i4sm(int(round(dx_m * 1000.0)))   # Dx in millimeters
    g[59:63] = i4sm(int(round(dy_m * 1000.0)))
    g[63] = 1                                  # projection flag: normal
    g[64] = 0                                  # scanning mode (+i +j)
    g[65:69] = i4sm(int(math.floor(latin1 * E6 + 0.5)))
    g[69:73] = i4sm(int(math.floor(latin2 * E6 + 0.5)))
    g[73:77] = i4sm(-90 * E6)                  # "south pole" lat (negative!)
    g[77:81] = i4sm(0)                         # south pole lon
    return bytes(g)


# level types (code table 4.5)
LEVEL_ISOBARIC = 100
LEVEL_AGL = 103
LEVEL_SURFACE = 1

# parameter codes (wgrib2 NCEP gribtable.dat: pcat, pnum)
PARAMS = {
    "TMP": (0, 0),
    "HGT": (3, 5),
    "UGRD": (2, 2),
    "VGRD": (2, 3),
    "RH": (1, 1),
    "SPFH": (1, 0),
    "DPT": (0, 6),
    "PRMSL": (3, 1),
    "PRES": (3, 0),
    "VVEL": (2, 8),
    "APCP": (1, 8),
    "GUST": (2, 22),
    "CAPE": (7, 6),
    "CIN": (7, 7),
    "MSLMA": (3, 198),
}


def sec4(cat, num, level_type, level_value, fcst_hour):
    p = bytearray()
    p += u2(0)            # number of coordinate values after template
    p += u2(0)            # PDT template 4.0
    p += bytes([cat])     # code table 4.1
    p += bytes([num])     # code table 4.2
    p += bytes([2])       # ct 4.3 generating process type: forecast
    p += bytes([0])       # background generating process identifier
    p += bytes([0])       # analysis/forecast generating process identifier
    p += u2(0)            # hours of observational data cutoff
    p += bytes([0])       # minutes of observational data cutoff
    p += bytes([1])       # ct 4.4 unit of time range: hour
    p += u4(fcst_hour)    # forecast time in those units
    p += bytes([level_type])   # first fixed surface type (ct 4.5)
    p += bytes([0])            # scale factor
    p += i4(level_value)       # scaled value (isobaric in Pa!)
    p += bytes([255])          # second fixed surface type: missing
    p += bytes([255])          # scale factor
    p += bytes([255, 255, 255, 255])
    assert len(p) == 29
    return section(4, bytes(p))


def sec4_pdt48(cat, num, period_flag, stat_proc, range_unit, range_len,
               step_unit, step_len, fcst_hour, level_type, scale, level_value,
               genproc=2, stat_year=2026, stat_mon=10, stat_day=19,
               stat_hr=12, stat_min=0, stat_sec=0):
    """PDT template 4.8 (statistically processed), byte layout validated
    EMPIRICALLY against tools/wgrib2 v3.1.3 readback (see docs/DECISIONS.md):
    prefix identical to PDT 4.0 through the fixed surfaces, then
      [29..35] valid-fmtd-time for stat processing (7 bytes)
      [36]     number of time-range specifications (we emit 1)
      [37..40] four opaque bytes (wgrib2 requires the space; writes zeros)
      block(12): CT4.6 period flag, CT4.11 interval code, range unit,
                 u4 range length, step unit, u4 step length
    wgrib2 displays the accumulation/average label from the CT 4.6 period
    flag (0=ave, 1=acc, 2=max, 3=min) and the range as
    "<fcst>-<fcst+len> hour ..." so step_len=0 keeps the classic display."""
    p = bytearray()
    p += u2(0)            # coordinate values after template
    p += u2(8)            # PDT template 4.8
    p += bytes([cat])
    p += bytes([num])
    p += bytes([genproc]) # ct 4.3
    p += bytes([0])       # background proc id
    p += bytes([0])       # an/fcst proc id
    p += u2(0)            # obs cutoff hours
    p += bytes([0])       # obs cutoff minutes
    p += bytes([1])       # ct 4.4 unit of time period: hour
    p += u4(fcst_hour)
    p += bytes([level_type])
    p += bytes([scale])
    p += i4(level_value)
    p += bytes([255, 255, 255, 255, 255, 255])   # second surface: missing
    assert len(p) == 29
    p += u2(stat_year) + bytes([stat_mon, stat_day, stat_hr, stat_min, stat_sec])
    p += bytes([1])                              # n time ranges
    p += bytes([0, 0, 0, 0])                     # opaque 4 bytes
    p += bytes([period_flag, stat_proc, range_unit]) + u4(range_len)
    p += bytes([step_unit]) + u4(step_len)
    return section(4, bytes(p))


def pack_bits(vals, nbits):
    out = bytearray((len(vals) * nbits + 7) // 8)
    pos = 0
    for v in vals:
        for b in range(nbits - 1, -1, -1):
            if (v >> b) & 1:
                out[pos >> 3] |= 0x80 >> (pos & 7)
            pos += 1
    return bytes(out)


def sec5_simple(nvals, ref, e_exp, s_stored, nbits):
    p = bytearray()
    p += u4(nvals)
    p += u2(0)            # template 5.0
    p += f4be(ref)        # IEEE reference value
    p += i2sm(e_exp)        # binary scale exponent E
    p += i2sm(s_stored)     # stored decimal scale S; decode multiplies by 10^-S
    p += bytes([nbits])
    p += bytes([0])
    assert len(p) == 16
    return section(5, bytes(p))


def sec6_bitmap(bitmap_bytes):
    # indicator 0 = bitmap follows
    return section(6, bytes([0]) + bitmap_bytes)


def sec6_nobitmap():
    # indicator 255 = no bitmap (NCEP encoders always emit this section)
    return section(6, bytes([255]))


def sec7(bitstream):
    return section(7, bitstream)


def build_record(sec1_bytes, grid_bytes, sec4_bytes, sec5_bytes, sec7_bytes,
                 sec6_bytes=None):
    parts = [sec0(0, 0)]
    parts.append(sec1_bytes)
    parts.append(grid_bytes)
    parts.append(sec4_bytes)
    parts.append(sec5_bytes)
    parts.append(sec6_bytes if sec6_bytes is not None else sec6_nobitmap())
    parts.append(sec7_bytes)
    parts.append(end_section())
    total = sum(len(x) for x in parts)
    parts[0] = sec0(0, total)
    return b"".join(parts)


def encode_field(increments, base, e_exp, mask=None):
    """
    increments: list of int (one per grid point, scan order); value = base + incr*2^E.
    mask: None or list of bool (True = defined). Returns (sec5, sec7, sec6|None, nvals).
    """
    n = len(increments)
    if mask is None:
        incs = list(increments)
        bm = None
    else:
        incs = [inc for inc, m in zip(increments, mask) if m]
        bm = mask
    maxinc = max(incs) if incs else 0
    mininc = min(incs) if incs else 0
    span = maxinc - mininc
    nbits = max(1, span.bit_length())
    shifted = [i - mininc for i in incs]
    ref = base + mininc * (2.0 ** e_exp)
    sec5b = sec5_simple(len(incs), ref, e_exp, 0, nbits)
    sec7b = sec7(pack_bits(shifted, nbits))
    sec6b = None
    if bm is not None:
        nbytes = (n + 7) // 8
        bb = bytearray(nbytes)
        for k, m in enumerate(bm):
            if m:
                bb[k >> 3] |= 0x80 >> (k & 7)
        sec6b = sec6_bitmap(bytes(bb))
    return sec5b, sec7b, sec6b, len(incs)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True, help="output dir for small fixtures")
    ap.add_argument("--perf-out", default=None, help="dir for full-size perf fixture")
    a = ap.parse_args()

    import os
    os.makedirs(a.out, exist_ok=True)

    # ---------------- GFS-like: regular lat/lon, N->S scan ----------------
    nx, ny = 41, 21
    lon1, lat1 = 350.0, 10.0
    dlon, dlat = 0.5, -0.5   # N->S like GFS
    grid = sec3_latlon(nx, ny, lon1, lat1, dlon, dlat, wind_grid=False)
    t1 = sec1(2026, 10, 19, 12, 0, 0)

    levels = [("isobaric", LEVEL_ISOBARIC, v) for v in (1000, 850, 700)]
    levels += [("agl2", LEVEL_AGL, 2)]
    levels += [("agl10", LEVEL_AGL, 10)]
    fhs = (0, 6, 12)

    def field_incr(param, level_value, fh):
        # analytic: exact dyadic values, distinct per param/level/time
        base_i = {"TMP": 0, "UGRD": 1000, "VGRD": 2000}[param]
        lv_i = {1000: 0, 850: 100, 700: 200, 2: 300, 10: 400}[level_value]
        out = []
        for j in range(ny):
            for i in range(nx):
                out.append(base_i + lv_i + fh * 10 + j * 4 + i)
        return out

    recs = []
    for param in ("TMP", "UGRD", "VGRD"):
        pcat, pnum = PARAMS[param]
        for (ltype_name, ltype, lval) in levels:
            if param in ("UGRD", "VGRD") and lval == 2:
                continue  # winds only at 10 m AGL
            for fh in fhs:
                incs = field_incr(param, lval, fh)
                mask = None
                # exercise bitmap/masked data on one message
                if param == "TMP" and lval == 850 and fh == 6:
                    mask = [not ((k % 97) == 0) for k in range(nx * ny)]
                s5b, s7b, s6b, _ = encode_field(incs, 250.0, 0, mask)
                s4 = sec4(pcat, pnum, ltype, lval * 100 if ltype == LEVEL_ISOBARIC else lval, fh)
                recs.append(build_record(t1, grid, s4, s5b, s7b, s6b))

    with open(os.path.join(a.out, "gfs_small.grib2"), "wb") as f:
        for r in recs:
            f.write(r)

    # ---------------- HRRR-like: Lambert conformal, grid winds ----------------
    hnx, hny = 61, 41
    la1, lo1 = 21.11389, 233.44
    lov, lad = 229.53, 26.5
    latin1, latin2 = 26.5, 35.0
    dxm, dym = 3000.0, 3000.0
    hgrid_w = sec3_lambert(hnx, hny, la1, lo1, lov, lad, latin1, latin2,
                           dxm, dym, wind_grid=True)
    hgrid_s = sec3_lambert(hnx, hny, la1, lo1, lov, lad, latin1, latin2,
                           dxm, dym, wind_grid=False)
    th = sec1(2026, 10, 19, 12, 0, 0)
    hrecs = []
    for param, (pcat, pnum), wgrid in (("TMP", PARAMS["TMP"], False),
                                       ("UGRD", PARAMS["UGRD"], True),
                                       ("VGRD", PARAMS["VGRD"], True)):
        for fh in (0, 3):
            incs = []
            for j in range(hny):
                for i in range(hnx):
                    off = {"TMP": 0, "UGRD": 5000, "VGRD": 9000}[param]
                    incs.append(off + fh * 100 + j * 10 + i)
            s5b, s7b, s6b, _ = encode_field(incs, 260.0, 0, None)
            s4 = sec4(pcat, pnum, LEVEL_AGL, 2 if param == "TMP" else 10, fh)
            grid = hgrid_w if wgrid else hgrid_s
            hrecs.append(build_record(th, grid, s4, s5b, s7b, None))

    with open(os.path.join(a.out, "hrrr_small.grib2"), "wb") as f:
        for r in hrecs:
            f.write(r)

    # ------------- PDT 4.8 statistically-processed fixture -------------------
    # APCP accumulations + TMP average on a tiny regular grid. Values analytic
    # like everywhere else. wgrib2 must print "accumulation"/"average" + the
    # time range when it reads this file back (checked by gen_fixtures.sh).
    snx, sny = 11, 7
    sgrid = sec3_latlon(snx, sny, 240.0, 20.0, -0.5, -0.5)
    sth = sec1(2026, 10, 19, 12, 0, 0)
    srecs = []
    def sincr(k):
        return k % 60
    for (pname, ltype, lval, fh, plen, pflag, sp) in (
            ("APCP", LEVEL_SURFACE, 0, 0, 6, 1, 1),     # 0-6 h accumulation
            ("APCP", LEVEL_SURFACE, 0, 6, 6, 1, 1),     # 6-12 h accumulation
            ("TMP", LEVEL_AGL, 2, 3, 3, 0, 2)):         # 3-6 h average
        pcat, pnum = PARAMS[pname]
        base = 250.0 if pname == "TMP" else 10.0
        incs = [j * snx + i for j in range(sny) for i in range(snx)]
        s5b, s7b, s6b, _ = encode_field(incs, base, 0, None)
        s4 = sec4_pdt48(pcat, pnum, pflag, sp, 1, plen, 1, 0, fh,
                        ltype, 0, lval, genproc=2)
        srecs.append(build_record(sth, sgrid, s4, s5b, s7b, None))
    with open(os.path.join(a.out, "stat_small.grib2"), "wb") as f:
        for r in srecs:
            f.write(r)

    # ------- unsupported representation guard (JPEG2000 5.40) --------------
    # Structurally valid message whose section 5 declares DRT 5.40; our
    # decoder must fail with the typed UnsupportedData error, never garbage.
    u5 = bytearray()
    u5 += u4(snx * sny)
    u5 += u2(40)          # template 5.40 JPEG2000
    u5 += f4be(0.0) + i2sm(0) + i2sm(0) + bytes([8, 0])
    u5 += u4(16) + bytes([0, 0, 0, 0, 0, 0, 0])
    urec = build_record(sth, sgrid, sec4(PARAMS["APCP"][0], PARAMS["APCP"][1],
                                         LEVEL_SURFACE, 0, 6), section(5, bytes(u5)),
                        sec7(bytes(range(16))), None)
    with open(os.path.join(a.out, "unsupported_j2k.grib2"), "wb") as f:
        f.write(urec)

    # ---------------- full-size perf fixture (gitignored dir) ----------------
    if a.perf_out:
        os.makedirs(a.perf_out, exist_ok=True)
        pnx, pny = 1440, 720
        # NOTE: lat1 = +-90 exactly trips wgrib2's float check (90000000 * 1e-6 > 90),
        # so the perf grid uses 89.875 .. -89.875 (720 rows at 0.25 deg).
        pgrid = sec3_latlon(pnx, pny, 0.0, 89.875, 0.25, -0.25)
        pt1 = sec1(2026, 10, 19, 12, 0, 0)
        pcat, pnum = PARAMS["TMP"]
        # value = 250 + j  (j = row index N->S); step 1 -> E=0, 10 bits
        incs = []
        for j in range(pny):
            for i in range(pnx):
                incs.append(j)
        s5b, s7b, s6b, _ = encode_field(incs, 250.0, 0, None)
        s4 = sec4(pcat, pnum, LEVEL_ISOBARIC, 1000 * 100, 0)
        with open(os.path.join(a.perf_out, "gfs_perf_f000.grib2"), "wb") as f:
            f.write(build_record(pt1, pgrid, s4, s5b, s7b, None))

    print("fixtures written")


if __name__ == "__main__":
    main()
