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


def scaled_pair(v, max_factor=9):
    """GRIB "scaled value": INT1 scale factor + SIGN-MAGNITUDE INT4 value,
    meaning v = value * 10^-factor (wgrib2 int8.c::scaled2dbl). Picks the
    smallest factor that represents v exactly inside the int4 range."""
    for f in range(0, max_factor + 1):
        iv = int(round(v * (10 ** f)))
        if abs(iv / float(10 ** f) - v) <= 1e-9 * max(1.0, abs(v)):
            if abs(iv) < 2 ** 31:
                return bytes([f]) + i4sm(iv)
    raise ValueError("no exact scaled representation for %r" % (v,))


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
                 wind_grid=False, scan_mode=0, earth_shape=6,
                 axes_km=None):
    """earth_shape: Code Table 3.2 byte. axes_km: (major, minor) in KILOMETRES
    written into the INLINE slots that codes 3 (km) / 7 (m) declare; wgrib2
    reads them from exactly these offsets (Earth.c::axes_earth: shape byte
    +6/+7 major, +11/+12 minor)."""
    g = bytearray(81)
    g[0:4] = u4(81)
    g[4] = 3
    g[5] = 0
    g[6:10] = u4(nx * ny)
    g[10] = 0
    g[11] = 0
    g[12] = 0
    g[13] = 30                                 # template 3.30 Lambert conformal
    g[14] = earth_shape                        # shape of earth (CT 3.2)
    g[15] = 0                                  # scale factor of spherical radius
    g[16:20] = u4(0)                           # scaled value of same
    g[20] = 0                                  # scale factor of major axis
    g[21:25] = u4(0)                           # scaled value of major axis
    g[25] = 0                                  # scale factor of minor axis
    g[26:30] = u4(0)                           # scaled value of minor axis
    if earth_shape == 1:
        # user-defined sphere: radius in METERS (wgrib2 set_radius semantics)
        r_m = axes_km[0] * 1000.0
        g[15:20] = scaled_pair(r_m)
    elif earth_shape in (3, 7):
        # code 3 declares KILOMETRES, code 7 METERS (Earth.c multiplies code 3
        # by 1000 after reading), so pass km here and adjust for code 7 below
        unit = 1.0 if earth_shape == 3 else 1000.0
        g[20:25] = scaled_pair(axes_km[0] * unit)
        g[25:30] = scaled_pair(axes_km[1] * unit)
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
    g[64] = scan_mode                          # scanning mode byte (bit 0x40: rows go north)
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


def sec4(cat, num, level_type, level_value, fcst_hour, fc_unit=1):
    """`fc_unit` is Code Table 4.4 (1=hour, 0=minute, 13=second, 2=day, ...):
    `fcst_hour` is expressed IN THOSE UNITS. Default 1 keeps every existing
    call site byte-identical; unit=0 emits fractional forecast hours (e.g.
    30 -> 0h30m), which our decoder normalizes to whole SECONDS."""
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
    p += bytes([fc_unit]) # ct 4.4 unit of time range (1=hour, 0=minute, ...)
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
               stat_hr=12, stat_min=0, stat_sec=0, fc_unit=1):
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
    p += bytes([fc_unit]) # ct 4.4 unit of time period (1=hour, 0=minute, ...)
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


def sec4_coordlist(cat, num, level_type, level_value, fcst_hour, coord_bytes=b"\x00\x00\x00\x00"):
    """Section 4 with a NON-ZERO number of coordinate values after the product
    definition template (what real GRIB2 does for ensemble/soil-layer/etc.
    constructs this point-query server will never support). Framing stays
    byte-exact, so a decoder that skips it can resume at the next message."""
    p = bytearray()
    p += u2(1)            # ONE coordinate value follows the template
    p += u2(0)            # PDT template 4.0
    p += bytes([cat])
    p += bytes([num])
    p += bytes([2])       # ct 4.3 generating process type: forecast
    p += bytes([0])       # background proc id
    p += bytes([0])       # an/fcst proc id
    p += u2(0)            # obs cutoff hours
    p += bytes([0])       # obs cutoff minutes
    p += bytes([1])       # ct 4.4 unit: hour
    p += u4(fcst_hour)
    p += bytes([level_type, 0]) + i4(level_value)
    p += bytes([255, 255, 255, 255, 255, 255])
    assert len(p) == 29
    p += coord_bytes      # the coordinate value(s)
    return section(4, bytes(p))


def sec4_layer(cat, num, ltype1, sf1, val1, ltype2, sf2, val2, fcst_hour):
    """PDT 4.0 with BOTH fixed surfaces present: a layer/range message (e.g.
    850-800 hPa). Point queries are out of scope for these (spec §13), so the
    pipeline must SKIP-AND-NOTE them without failing the file."""
    p = bytearray()
    p += u2(0)
    p += u2(0)
    p += bytes([cat, num, 2, 0, 0])
    p += u2(0) + bytes([0])
    p += bytes([1]) + u4(fcst_hour)
    p += bytes([ltype1, sf1]) + i4(val1)
    p += bytes([ltype2, sf2]) + i4(val2)
    assert len(p) == 29
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


def pack_bits_np(a, nbits):
    """numpy-vectorized twin of pack_bits (identical bytes, MSB-first, zero
    tail padding). Used only by the full-size perf fixtures where the pure-
    python loop would take tens of minutes."""
    import numpy as np
    bits = ((a[:, None] >> np.arange(nbits - 1, -1, -1)) & 1).astype(np.uint8)
    flat = bits.reshape(-1)
    pad = ((flat.size + 7) // 8) * 8 - flat.size
    if pad:
        flat = np.concatenate([flat, np.zeros(pad, dtype=np.uint8)])
    return np.packbits(flat).tobytes()


def encode_field_np(incs, base, e_exp):
    """encode_field twin for numpy int arrays (no bitmap path needed at perf
    size). Returns (sec5, sec7)."""
    import numpy as np
    mininc = int(incs.min())
    maxinc = int(incs.max())
    span = maxinc - mininc
    nbits = max(1, span.bit_length())
    shifted = incs - mininc
    ref = base + mininc * (2.0 ** e_exp)
    return (sec5_simple(len(shifted), ref, e_exp, 0, nbits),
            sec7(pack_bits_np(shifted, nbits)))


def sec5_runlength(npoints, nbits, mv, mvl, dec_scale, levels_int):
    """Section 5 for DRT **5.200 run length packing** (Code Table 5.0 template
    200). Offsets verified against wgrib2 `unpk_run_length.c`, which reads them
    from sec[5] as: n_bits=[11], mv=uint2(+12), mvl=uint2(+14), decimal_scale=
    [16], levels[i]=int2(+15+i*2) for i=1..mvl — i.e. payload offsets 6, 7, 9,
    11 and 10+i*2 here. NOTE this layout is NOT the simple-packing one: there is
    no IEEE reference value and no binary scale; the value table replaces them."""
    p = bytearray()
    p += u4(npoints)          # number of grid points (wgrib2 uses sec3's count)
    p += u2(200)              # data representation template 5.200
    p += bytes([nbits])       # bits per packed index/repetition code
    p += u2(mv)               # maximum level index (codes > mv are repetitions)
    p += u2(mvl)              # number of distinct levels in the table
    p += bytes([dec_scale])   # decimal scale (levels are value * 10^-dec_scale)
    for i in range(1, mvl + 1):
        p += i2sm(levels_int[i - 1])
    assert len(p) == 12 + 2 * mvl
    return section(5, bytes(p))


def pack_runs(runs, mv, nbits):
    """Run-length bitstream (mirror of wgrib2 unpk_run_length.c's inverse):
    a run of length L of level index v emits `v` followed by the base-`range`
    digits of (L-1), LEAST significant first, each digit d written as the
    continuation code mv+1+d, where range = 2^nbits - 1 - mv. The decoder
    recomputes n = 1 + sum(factor * (code - mv - 1)), factor *= range, so digits
    MUST be little-endian in that base or the run length comes out wrong."""
    rng = (1 << nbits) - 1 - mv
    if rng < 1:
        raise ValueError("nbits too small for mv (range must be >= 1)")
    codes = []
    for (v, length) in runs:
        if v < 1 or v > mv:
            raise ValueError("level index out of range")
        codes.append(v)
        rem = length - 1
        while rem > 0:
            codes.append(mv + 1 + (rem % rng))
            rem //= rng
    return pack_bits(codes, nbits), len(codes)


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
    ap.add_argument("--real-levels", type=int, default=37,
                    help="isobaric levels in the realistic dataset (16 = the REDUCED "
                         "variant used for the bounded pre-V1.2 measurement)")
    ap.add_argument("--real-runs", type=int, default=3,
                    help="run cycles in the realistic dataset (1 = single-run variant)")
    ap.add_argument("--real-out", default=None,
                    help="dir for the PRODUCTION-SCALE realistic GFS-like dataset "
                         "(V1.2 phase D ingest benchmark: 3 runs x 2 forecast hours)")
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

    # --------- OBLATE ELLIPSOID LCC GRID (V1.1 phase 4, spec §4.3/§4.4) ------
    # Code Table 3.2 byte 3 ("user-defined spheroid"): the axes are declared
    # INLINE in section 3, here WGS 84 (6378.137 / 6356.75231 km), so this
    # fixture exercises the oblate branch end to end -- ns/rh/eccentricity in
    # the projection AND the conformal wind-rotation angle -- and its rotation
    # is validated against pyproj's DEFAULT WGS84 ellipsoid. wgrib2 reads the
    # same inline slots (`-radius` reports the axes; gen_fixtures.sh asserts it)
    # and `-ll2ij` corroborates the projection (expected.json).
    enx, eny = 41, 31
    egrid_w = sec3_lambert(enx, eny, 40.0, 250.0, 235.0, 30.0, 30.0, 40.0,
                           12000.0, 12000.0, wind_grid=True,
                           earth_shape=3, axes_km=(6378.137, 6356.75231))
    egrid_s = sec3_lambert(enx, eny, 40.0, 250.0, 235.0, 30.0, 30.0, 40.0,
                           12000.0, 12000.0, wind_grid=False,
                           earth_shape=3, axes_km=(6378.137, 6356.75231))
    eth = sec1(2026, 10, 19, 12, 0, 0)
    erecs = []
    for pname, wgrid in (("UGRD", True), ("VGRD", True), ("TMP", False)):
        pcat, pnum = PARAMS[pname]
        incs = []
        for j in range(eny):
            for i in range(enx):
                off = {"TMP": 0, "UGRD": 4000, "VGRD": 8000}[pname]
                incs.append(off + j * 100 + i)
        s5b, s7b, s6b, _ = encode_field(incs, 255.0, 0, None)
        s4 = sec4(pcat, pnum, LEVEL_AGL, 2 if pname == "TMP" else 10, 0)
        # winds ride the grid-relative grid, scalars the earth-relative one,
        # exactly like the hrrr_small fixture; the wind messages come FIRST so
        # the startup probe sees the grid-relative flag immediately.
        erecs.append(build_record(eth, egrid_w if wgrid else egrid_s,
                                  s4, s5b, s7b, None))
    with open(os.path.join(a.out, "hrrr_ellip_small.grib2"), "wb") as f:
        for r in erecs:
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

    # ------------- FRACTIONAL FORECAST HOURS (V1.1 phase 3) -----------------
    # CT 4.4 unit = MINUTE (code 0): forecast hours 0h, 0h30m, 1h for run
    # 2026-10-19T12Z, PLUS a SECOND RUN 12:30Z inside the SAME FILE (the
    # run-directory collision regression: 12Z and 12:30Z must land in
    # `2026101912` and `202610191230`), plus one PDT 4.8 accumulation whose
    # statistical time range is 30 MINUTES. Values are analytic and LINEAR IN
    # THE FORECAST MINUTE:   value = BASE + j*nx + i + fc_minute
    # so an on-node probe at any forecast minute (incl. interpolated mid-bracket
    # ones) is exactly predictable.
    fnx, fny = 21, 11
    fgrid = sec3_latlon(fnx, fny, 350.0, 10.0, 0.5, -0.5)
    t12 = sec1(2026, 10, 19, 12, 0, 0)
    t1230 = sec1(2026, 10, 19, 12, 30, 0)

    def frac_msg(run_bytes, fc_min, base, cat, num, ltype, lval_raw, pdt48=None):
        incs = [j * fnx + i + fc_min for j in range(fny) for i in range(fnx)]
        s5b, s7b, s6b, _ = encode_field(incs, float(base), 0, None)
        if pdt48 is None:
            s4b = sec4(cat, num, ltype, lval_raw, fc_min, fc_unit=0)
        else:
            s4b = sec4_pdt48(cat, num, pdt48[0], pdt48[1], 0, 30, 0, 0,
                             fc_min, ltype, 0, lval_raw, genproc=2, fc_unit=0)
        return build_record(run_bytes, fgrid, s4b, s5b, s7b, s6b)

    frecs = []
    for fc in (0, 30, 60):                      # run 12Z: f000, f000m30, f001m00
        frecs.append(frac_msg(t12, fc, 250, PARAMS["TMP"][0], PARAMS["TMP"][1],
                              LEVEL_ISOBARIC, 850 * 100))
    for fc in (0, 30):                          # run 12:30Z: distinct run dir
        frecs.append(frac_msg(t1230, fc, 500, PARAMS["TMP"][0], PARAMS["TMP"][1],
                              LEVEL_ISOBARIC, 850 * 100))
    # APCP surface accumulation over the 30-minute window ending at fc 60 min
    frecs.append(frac_msg(t12, 30, 5, PARAMS["APCP"][0], PARAMS["APCP"][1],
                          LEVEL_SURFACE, 0, pdt48=(1, 1)))
    with open(os.path.join(a.out, "frac_small.grib2"), "wb") as f:
        for r in frecs:
            f.write(r)

    # ------------- SKIP-AND-NOTE robustness fixture (V1.1 phase 3) ----------
    # Message order: [good TMP@850] [coordinate-list message] [layer 850-800
    # message] [good TMP@2m AGL]. The decoder must decode BOTH good messages
    # byte-perfectly (proving the walk resumed at the right boundary), count
    # the coord-list message as an unsupported skip, and hand the layer message
    # to the pipeline, which skips it too. Neither may bump the error counter.
    rgrid = fgrid
    rrecs = []
    rinc = [j * fnx + i for j in range(fny) for i in range(fnx)]
    r5, r7, r6, _ = encode_field(rinc, 300.0, 0, None)
    rrecs.append(build_record(t12, rgrid,
                              sec4(PARAMS["TMP"][0], PARAMS["TMP"][1], LEVEL_ISOBARIC, 850 * 100, 0),
                              r5, r7, r6))
    rrecs.append(build_record(t12, rgrid,
                              sec4_coordlist(PARAMS["TMP"][0], PARAMS["TMP"][1],
                                             LEVEL_ISOBARIC, 925 * 100, 0),
                              r5, r7, r6))
    rrecs.append(build_record(t12, rgrid,
                              sec4_layer(PARAMS["TMP"][0], PARAMS["TMP"][1],
                                         LEVEL_ISOBARIC, 0, 850 * 100,
                                         LEVEL_ISOBARIC, 0, 800 * 100, 0),
                              r5, r7, r6))
    rrecs.append(build_record(t12, rgrid,
                              sec4(PARAMS["TMP"][0], PARAMS["TMP"][1], LEVEL_AGL, 2, 0),
                              r5, r7, r6))
    with open(os.path.join(a.out, "robust_small.grib2"), "wb") as f:
        for r in rrecs:
            f.write(r)

    # ------- DRT 5.200 run length packing (V1.2 phase D coverage close) -------
    # wgrib2 can DECODE run-length packing (`Supported decoding: ... rle ...`)
    # but has NO ENCODER for it: `-set_grib_type rle` -> "FATAL ERROR: set_grib_
    # type: bad type rle" (measured on tools/wgrib2-full v3.1.3; the encoder
    # lives in g2clib, which this build disables). So the truth chain here is
    # TWO-PART: (a) our hand-written 5.200 message is read back by WGrib2's OWN
    # rle DECODER and its `-checksum data` must equal that of the SAME field
    # written with simple packing -- an independent implementation agreeing on
    # every value; (b) our Rust decoder must reproduce those same values. Layout
    # follows Code Table 5.0 / unpk_run_length.c offsets exactly (see
    # `sec5_runlength`). No bitmap: the rle path's bitmap convention is INVERTED
    # vs simple/complex (bit=1 marks UNDEFINED), documented in grib2.rs, and
    # deliberately NOT exercised by this fixture.
    rnx, rny = 21, 11                       # 231 points = 33 runs x 7
    rl_grid = sec3_latlon(rnx, rny, 350.0, 10.0, 0.5, -0.5)
    rl_th = sec1(2026, 10, 19, 12, 0, 0)
    rl_levels_int = [2500, 2560, 2600]      # decimal scale 1 -> 250.0/256.0/260.0
    rl_dec_scale = 1
    rl_mvl = len(rl_levels_int)             # 3 distinct levels
    rl_mv = rl_mvl                          # max level INDEX (codes > mv repeat)
    rl_nbits = 4                            # range = 15 - 3 = 12 >= run-1 = 6
    rl_level_of = [(k // 7) % 3 + 1 for k in range(rnx * rny)]
    rl_runs = []
    for v in rl_level_of:
        if rl_runs and rl_runs[-1][0] == v:
            rl_runs[-1][1] += 1
        else:
            rl_runs.append([v, 1])
    assert all(len(r) == 2 and r[1] == 7 for r in rl_runs) and len(rl_runs) == 33
    rl_bits, rl_ncodes = pack_runs([(r[0], r[1]) for r in rl_runs], rl_mv, rl_nbits)
    assert rl_ncodes * rl_nbits == len(rl_bits) * 8   # no padding codewords
    rl_s5 = sec5_runlength(rnx * rny, rl_nbits, rl_mv, rl_mvl, rl_dec_scale,
                           rl_levels_int)
    rl_s4 = sec4(PARAMS["TMP"][0], PARAMS["TMP"][1], LEVEL_ISOBARIC, 850 * 100, 0)
    with open(os.path.join(a.out, "runlength_small.grib2"), "wb") as f:
        f.write(build_record(rl_th, rl_grid, rl_s4, rl_s5, sec7(rl_bits), None))

    # Same field, simple packing: the reference whose `-checksum data` the rle
    # file must match (values 250.0 / 256.0 / 260.0, exact in f32).
    rl_incs = [0, 6, 10]
    rl_plain_incs = [rl_incs[v - 1] for v in rl_level_of]
    p5b, p7b, p6b, _ = encode_field(rl_plain_incs, 250.0, 0, None)
    with open(os.path.join(a.out, "runlength_plain.grib2"), "wb") as f:
        f.write(build_record(rl_th, rl_grid, rl_s4, p5b, p7b, p6b))

    # ------- unsupported representation guard (DRT 5.1 dictionary) ---------
    # Structurally valid message whose section 5 declares a data representation
    # template no decoder in the wgrib2 ecosystem implements (dictionary packing
    # 5.1: unpk.c handles 0/2/3/4/40/41/42/200 only). Our decoder must fail with
    # the typed UnsupportedData error, never garbage. NOTE: JPEG2000 (5.40) used
    # to live here; V1.1 phase 2 decodes it for real, so the guard moved to a
    # template that is genuinely unsupported (see tests/packing_twins.rs).
    u5 = bytearray()
    u5 += u4(snx * sny)
    u5 += u2(1)           # template 5.1 dictionary packing (unsupported)
    u5 += f4be(0.0) + i2sm(0) + i2sm(0) + bytes([8, 0])
    u5 += u4(16) + bytes([0, 0, 0, 0, 0, 0, 0])
    urec = build_record(sth, sgrid, sec4(PARAMS["APCP"][0], PARAMS["APCP"][1],
                                         LEVEL_SURFACE, 0, 6), section(5, bytes(u5)),
                        sec7(bytes(range(16))), None)
    with open(os.path.join(a.out, "unsupported_drt.grib2"), "wb") as f:
        f.write(urec)

    # ------- full-size realistic perf DATASET (gitignored dir) ---------------
    # Spec §8 milestone-5 perf run: GFS-like 1440x720 with ~10 configured
    # param/level groups at f000+f006, plus HRRR-like full 1799x1059 LCC at
    # f000+f003. Values stay analytic (exact in f32/simple packing):
    #   GFS TMP@1000mb : value = 250 + row_j + fh*10      (compat with the
    #                                                     single-message twin
    #                                                     used by tests/perf_fixture.rs)
    #   other GFS      : value = BASE + ((row*1440+col) % 4096) + fh*10
    #   HRRR (affine)  : value = BASE + col + row + fh*10 (affine so bilinear
    #                                                     probes land on-node
    #                                                     within float noise)
    if a.perf_out:
        import numpy as np
        os.makedirs(a.perf_out, exist_ok=True)

        # ---- GFS-like regular lat/lon, N->S, winds earth-relative ----
        pnx, pny = 1440, 720
        pgrid_w = sec3_latlon(pnx, pny, 0.0, 89.875, 0.25, -0.25, wind_grid=False)
        pt1 = sec1(2026, 10, 19, 12, 0, 0)
        jj = np.repeat(np.arange(pny, dtype=np.int64), pnx)
        ii = np.tile(np.arange(pnx, dtype=np.int64), pny)
        mod_inc = (jj * pnx + ii) % 4096

        gfs_fields = []  # (param, level_type, level_value_native, base)
        for lv in (1000, 925, 850, 700, 500, 300, 250, 200):
            gfs_fields.append(("HGT", LEVEL_ISOBARIC, lv * 100, {
                1000: 480, 925: 1020, 850: 1560, 700: 3000,
                500: 5700, 300: 9400, 250: 10500, 200: 11800}[lv]))
        for lv in (1000, 925, 850, 700, 500, 300, 250, 200):
            gfs_fields.append(("TMP", LEVEL_ISOBARIC, lv * 100, {
                1000: 250, 925: 255, 850: 260, 700: 265,
                500: 270, 300: 275, 250: 280, 200: 285}[lv]))
        gfs_fields.append(("TMP", LEVEL_AGL, 2, 260))
        gfs_fields.append(("UGRD", LEVEL_AGL, 10, -5))
        gfs_fields.append(("VGRD", LEVEL_AGL, 10, -3))
        gfs_fields.append(("RH", LEVEL_AGL, 2, 40))
        gfs_fields.append(("PRMSL", LEVEL_SURFACE, 0, 101000))
        gfs_fields.append(("GUST", LEVEL_AGL, 10, 5))

        for fh in (0, 6):
            recs = []
            for (pname, ltype, lval, base) in gfs_fields:
                if pname == "TMP" and ltype == LEVEL_ISOBARIC and lval == 100000:
                    incs = jj + fh * 10          # legacy formula (perf_fixture.rs)
                else:
                    incs = mod_inc + fh * 10
                s5b, s7b = encode_field_np(incs, float(base), 0)
                s4 = sec4(PARAMS[pname][0], PARAMS[pname][1], ltype, lval, fh)
                recs.append(build_record(pt1, pgrid_w, s4, s5b, s7b, None))
            with open(os.path.join(a.perf_out, "gfs_perf_f%03d.grib2" % fh), "wb") as f:
                for r in recs:
                    f.write(r)

        # ---- HRRR-like LCC CONUS geometry, grid-relative winds ----
        hnx, hny = 1799, 1059
        hjj = np.repeat(np.arange(hny, dtype=np.int64), hnx)
        hii = np.tile(np.arange(hnx, dtype=np.int64), hny)
        affine = hii + hjj                       # affine in (i,j): bilinear-safe probes
        hgrid_s = sec3_lambert(hnx, hny, 21.11389, 233.44, 229.53, 26.5,
                               26.5, 35.0, 3000.0, 3000.0, wind_grid=False,
                               scan_mode=0x40)  # rows go NORTH from La1 (real HRRR)
        hgrid_w = sec3_lambert(hnx, hny, 21.11389, 233.44, 229.53, 26.5,
                               26.5, 35.0, 3000.0, 3000.0, wind_grid=True,
                               scan_mode=0x40)
        th1 = sec1(2026, 10, 19, 12, 0, 0)
        hrrr_fields = [("TMP", LEVEL_AGL, 2, 255, False)]
        for lv in (925, 850, 700, 500):
            hrrr_fields.append(("TMP", LEVEL_ISOBARIC, lv * 100,
                                {925: 258, 850: 262, 700: 268, 500: 275}[lv], False))
        for lv in (1000, 925, 850, 700, 500):
            hrrr_fields.append(("HGT", LEVEL_ISOBARIC, lv * 100,
                                {1000: 10, 925: 750, 850: 1550, 700: 3000,
                                 500: 5700}[lv], False))
        hrrr_fields.append(("UGRD", LEVEL_AGL, 10, -5, True))
        hrrr_fields.append(("VGRD", LEVEL_AGL, 10, -3, True))
        hrrr_fields.append(("RH", LEVEL_AGL, 2, 40, False))
        for fh in (0, 3):
            recs = []
            for (pname, ltype, lval, base, wgrid) in hrrr_fields:
                incs = affine + fh * 10
                s5b, s7b = encode_field_np(incs, float(base), 0)
                s4 = sec4(PARAMS[pname][0], PARAMS[pname][1], ltype, lval, fh)
                recs.append(build_record(th1, hgrid_w if wgrid else hgrid_s,
                                         s4, s5b, s7b, None))
            with open(os.path.join(a.perf_out, "hrrr_perf_f%03d.grib2" % fh), "wb") as f:
                for r in recs:
                    f.write(r)

    # ------------- PRODUCTION-SCALE REALISTIC GFS-LIKE DATASET ---------------
    # V1.2 phase D: the tester's exact pain — a REAL delivery shape at production
    # geometry, so the ingest benchmark measures the merge path at L ~ 37 levels
    # instead of L ~ 8. Everything a real NWS pgrb2 delivery has, and nothing else:
    #   * full GFS geometry: regular lat/lon 1440 x 720, lon 0..359.75, lat 89.875
    #     down to -89.875, N->S scan (template 3.0);
    #   * 37 isobaric levels: EVERY 25 hPa from 1000 to 100. That is a SUPERSET of
    #     the real NWS pgrb2 stack (25 levels: 1000,975,950,925,900,850,800,...,100);
    #     the 12 extras (875,825,725,675,625,575,525,475,425,375,325,275) exist only
    #     to make the per-group merge cost O(L^2)-vs-O(L) visible at ingest. Documented
    #     deviation from the brief's "~40 levels" (the REAL NWS list is 25).
    #   * params TMP + HGT + UGRD + VGRD, ALL on isobaric:* -> 148 messages/file;
    #   * the WIND messages carry flag-table-3.3 bit 0x08 (grid-relative components)
    #     ON a regular lat/lon grid, which exercises the wind PAIR rule, the startup
    #     preflight probe and the rotate-to-earth code path end to end. NOTE: for
    #     REGULAR grids our rotation is the IDENTITY (documented decision matching
    #     wgrib2: a lat/lon grid's components are already earth-tangent), so stored
    #     values equal decoded values -- probes prove the plumbing, not a rotation
    #     angle (angles are covered by the LCC fixtures + pyproj checks);
    #   * THREE RUNS x TWO forecast hours = SIX FILES (the "6-file reality" the
    #     retention settings must cope with): runs 2026-10-19T00Z / T06Z / T12Z,
    #     each delivering f000 and f006 as SEPARATE files, named like the real
    #     NWS products (`gfs.t191200z.pgrb2.0p25.f006.grib2`) -- deliberately WITHOUT
    #     a 10-digit YYYYMMDDHH stamp, so the phase-C filename run-hint CANNOT help
    #     and ordering falls back to path order (measured, documented);
    #   * values stay analytic and EXACT in f32: value = BASE(param,level) +
    #     ((row*1440+col) % 1024) + fh*10, so every node probe is predictable and
    #     simple packing needs only 10-11 bits (files ~150 MB, vs ~30 MB for a
    #     real JPEG-compressed NWS product -- honest about being BIGGER on disk).
    if a.real_out:
        import numpy as np
        os.makedirs(a.real_out, exist_ok=True)

        rnx, rny = 1440, 720
        rgrid_scalar = sec3_latlon(rnx, rny, 0.0, 89.875, 0.25, -0.25,
                                   wind_grid=False)
        rgrid_wind = sec3_latlon(rnx, rny, 0.0, 89.875, 0.25, -0.25,
                                 wind_grid=True)   # flag 0x08 on a lat/lon grid
        jj = np.repeat(np.arange(rny, dtype=np.int64), rnx)
        ii = np.tile(np.arange(rnx, dtype=np.int64), rny)
        mod_inc = (jj * rnx + ii) % 1024           # 10-bit span -> compact packing

        # every 25 hPa from 1000 down to 100 (37 levels, superset of the NWS list);
        # --real-levels / --real-runs slice that stack so a REDUCED-BUT-SAME-SHAPE
        # variant can be generated (used by the bounded pre-V1.2 measurement: L=16
        # keeps the geometry, params, wind flags and file shape identical while
        # cutting the O(L^2) write volume to something a strace-free run can finish)
        ALL_LEVELS = list(range(1000, 75, -25))
        assert len(ALL_LEVELS) == 37
        LEVELS_25 = ALL_LEVELS[:a.real_levels]
        assert len(LEVELS_25) == a.real_levels

        def base_of(pname, lv):
            if pname == "TMP":       # plausible troposphere->stratosphere temps
                return 290 - (1000 - lv) // 4
            if pname == "HGT":       # plausible heights (gpm) rising with height
                return (1000 - lv) * 45 // 100
            if pname == "UGRD":      # zonal flow: weak near surface, strong aloft
                return 5 + (1000 - lv) // 40
            return -3 - (1000 - lv) // 60          # VGRD meridional

        RUN_HS = (0, 6, 12)[:a.real_runs]
        for run_h in RUN_HS:
            rt = sec1(2026, 10, 19, run_h, 0, 0)
            for fh in (0, 6):
                recs = []
                for lv in LEVELS_25:
                    for pname in ("TMP", "HGT", "UGRD", "VGRD"):
                        incs = mod_inc + fh * 10
                        s5b, s7b = encode_field_np(incs, float(base_of(pname, lv)), 0)
                        wgrid = pname in ("UGRD", "VGRD")
                        s4 = sec4(PARAMS[pname][0], PARAMS[pname][1],
                                  LEVEL_ISOBARIC, lv * 100, fh)
                        recs.append(build_record(rt, rgrid_wind if wgrid else rgrid_scalar,
                                                 s4, s5b, s7b, None))
                name = "gfs.t%02d0000z.pgrb2.0p25.f%03d.grib2" % (run_h, fh)
                with open(os.path.join(a.real_out, name), "wb") as f:
                    for r in recs:
                        f.write(r)
        print("realistic dataset written (%d files, %d levels, runs %s)"
              % (len(RUN_HS) * 2, len(LEVELS_25), ",".join(str(h) for h in RUN_HS)))

    print("fixtures written")


if __name__ == "__main__":
    main()
