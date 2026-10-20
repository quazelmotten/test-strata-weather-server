//! Purpose-built streaming GRIB2 decoder (spec §4.2), validated byte-for-byte
//! against tools/wgrib2 v3.1.3 (see docs/DECISIONS.md for why grib-rs was not
//! used). Section framing, sign-magnitude integers and the complex-packing
//! algorithm mirror wgrib2's own reader (`rd_seq_grib.c`, `int8.c::int4/int2`,
//! `unpk_complex.c`, `unpk_0.c`, `unpk_run_length.c`) — the reference binary
//! that every fixture is validated against.
//!
//! ## Supported / rejected constructs
//!
//! * Sections walked by declared byte length; message terminators accepted in
//!   BOTH framings: bare ASCII `7777` (wgrib2-native, what wgrib2 itself
//!   writes) and a LEN-prefixed end section (`u4 len | 0x08 | 0x00 | '7777'`,
//!   what the bootstrap python writer emits). Both are tolerated because
//!   wgrib2 accepts both; our fixtures use the latter, converted files the
//!   former.
//! * Section 1 template 1.0: reference time + end-of-file flag (bit 0x08 of
//!   the production-status byte, Code Table 1.3).
//! * Section 3 templates 3.0 (regular lat/lon) and 3.30 (Lambert conformal),
//!   scanning-mode flags and the Code Table 3.2 earth-shape byte; all signed
//!   grid fields decoded SIGN-MAGNITUDE like wgrib2 (NOTES-fixtures item 2).
//!   Other grid templates -> typed `UnsupportedGrid(tmpl)`.
//! * Section 4 PDT 4.0 (instantaneous) and 4.8 (statistically processed: CT
//!   4.6 period indicator + CT 4.11 interval code + time range). Other PDTs
//!   -> typed `UnsupportedPdt(tmpl)`; coordinate lists after the template ->
//!   typed `UnsupportedCoordLists`.
//! * Section 6 bitmap: indicator 255 = no bitmap, 0/254 = bitmap present
//!   (bit=1 means the point is DEFINED, matching `missing.c`); masked points
//!   decode to NaN.
//! * Section 7 representations: DRT 5.0 simple packing, 5.2 complex packing
//!   WITHOUT spatial differencing, 5.3 complex packing WITH spatial
//!   differencing (first AND second order, group widths/lengths, primary
//!   missing-value management), 5.200 run length (the variant wgrib2 and
//!   NCEP actually emit — there is no 5.1/5.2 dictionary encoder anywhere in
//!   the wgrib2 ecosystem; dictionary 5.1 -> typed UnsupportedData), 5.4 IEEE.
//!   DRT 5.40 JPEG2000 / 5.41 PNG / 5.42 AEC and everything else -> typed
//!   `UnsupportedData(tmpl)` (documented limitation).
//! * Filenames are never consulted for metadata (spec §4.2): everything comes
//!   from sections 0/1/3/4.
//!
//! The skip-unpack path (spec §4.2: "Skip messages not in the configured
//! parameter list without fully decoding them") consults the section-4
//! product definition BEFORE touching section 7; `DecodeStats` records how
//! many section-7 payloads were skipped vs. actually unpacked so tests can
//! prove the filter works.

use wx_core::grid::{GridDef, GridRegular, GridLambert};
use wx_core::tables::param_name_for;
use wx_core::grid::D2R;

use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;

use memmap2::Mmap;

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum GribErr {
    Io,
    Truncated,
    BadMagic,
    BadEdition,
    BadSectionOrder,
    BadField(&'static str),
    UnsupportedGrid(u16),
    UnsupportedData(u16),
    UnsupportedPdt(u16),
    UnsupportedCoordLists,
}

// ---------------------------------------------------------------------------
// public decoded-message API (consumed by milestone 3 storage writer)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct RunTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub epoch_secs: i64, // UTC seconds since 1970-01-01T00:00:00Z
}

#[derive(Clone, Debug, PartialEq)]
pub enum StatKind {
    Instantaneous,
    Accumulation,
    Average,
    Maximum,
    Minimum,
    Other,
}

/// Statistical-processing info (spec §4.2 PDT 4.8 requirement). For PDT 4.0
/// this is `Instantaneous` with zero ranges. Begin/end/step are seconds
/// relative to the run time. `ct4_11` keeps the raw Code Table 4.11 interval
/// code of the first time-range block for round-tripping/diagnostics.
#[derive(Clone, Debug)]
pub struct StatInfo {
    pub kind: StatKind,
    pub ct4_11: i16,
    pub begin_secs: i64,
    pub end_secs: i64,
    pub step_secs: i64,
}

/// One decoded GRIB2 message: everything milestone 3 needs to write storage
/// plus wind rotation inputs. `data` is scan-order `[j][i]` f32 with NaN at
/// masked/missing points.
#[derive(Clone, Debug)]
pub struct DecodedMessage {
    pub run: RunTime,
    pub fc_secs: i64,          // forecast offset from run (CT 4.4 units -> s)
    pub valid_epoch_secs: i64, // run + fc_secs ("valid time")
    pub discipline: u8,        // section 0 discipline (GRIB2 product discipline)
    pub category: u8,          // CT 4.1
    pub number: u8,            // CT 4.2
    pub param_name: Option<&'static str>, // via wx-core tables reverse lookup
    /// (CT 4.5 type, normalized value): isobaric levels normalized Pa -> hPa
    /// (matching config/API strings); other level types keep their GRIB units.
    pub levels: Vec<(u8, f64)>,
    pub grid: GridDef,
    pub stat: StatInfo,
    pub winds_grid: bool, // flag table 3.3 bit 0x08 (grid-relative components)
    pub data: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct DecodeStats {
    pub messages_total: u32,
    pub messages_decoded: u32,
    pub messages_skipped: u32,
    pub sec7_bytes_decoded: usize,
    pub sec7_bytes_skipped: usize,
}

/// A parameter allow-list: (discipline, category, number) tuples from the
/// config. `None` = accept every message.
#[derive(Clone, Debug)]
pub enum ParamFilter {
    All,
    Only(Vec<(u8, u8, u8)>),
}

impl ParamFilter {
    pub fn allows(self: &Self, disc: u8, cat: u8, num: u8) -> bool {
        match self {
            ParamFilter::All => { return true; }
            ParamFilter::Only(list) => {
                for t in list {
                    if t.0 == disc && t.1 == cat && t.2 == num {
                        return true;
                    }
                }
                return false;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// byte / bit primitives (big-endian GRIB; sign-magnitude per wgrib2 int8.c)
// ---------------------------------------------------------------------------

pub fn be_u16(b: &[u8], off: usize) -> u16 {
    ((b[off] as u16) << 8) | (b[off + 1] as u16)
}

pub fn be_u32(b: &[u8], off: usize) -> u32 {
    ((b[off] as u32) << 24) | ((b[off + 1] as u32) << 16) |
        ((b[off + 2] as u32) << 8) | (b[off + 3] as u32)
}

pub fn be_u64(b: &[u8], off: usize) -> u64 {
    let hi = be_u32(b, off) as u64;
    let lo = be_u32(b, off + 4) as u64;
    (hi << 32) | lo
}

/// int2 SIGN-MAGNITUDE (wgrib2 int8.c::int2): bit 15 = sign, bits 0..14
/// magnitude. Real NCEP/wgrib2 files encode negative latitudes this way.
pub fn sm_i2(b: &[u8], off: usize) -> i16 {
    let raw = be_u16(b, off);
    let mag = (raw & 0x7FFF) as i16;
    if (raw & 0x8000) != 0 { -mag } else { mag }
}

/// int4 SIGN-MAGNITUDE (wgrib2 int8.c::int4).
pub fn sm_i4(b: &[u8], off: usize) -> i32 {
    let raw = be_u32(b, off);
    let mag = (raw & 0x7FFFFFFF) as i32;
    if (raw & 0x80000000) != 0 { -mag } else { mag }
}

/// big-endian IEEE 754 float -> f32.
pub fn ieee_f4(b: &[u8], off: usize) -> f32 {
    f32::from_bits(be_u32(b, off))
}

/// Read `nbits` (1..=32) from a MSB-first bitstream at absolute bit `pos`.
pub fn get_bits(b: &[u8], pos: usize, nbits: u8) -> u32 {
    let mut v: u32 = 0;
    let mut p = pos;
    for _ in 0..nbits {
        let byte = b[p >> 3];
        let bit = (byte >> (7 - (p & 7))) & 1;
        v = (v << 1) | (bit as u32);
        p += 1;
    }
    v
}

/// Unsigned big-endian integer of exactly `nbytes` bytes (wgrib2 uint_n).
pub fn uint_n(b: &[u8], off: usize, nbytes: usize) -> i64 {
    let mut v: i64 = 0;
    for k in 0..nbytes {
        v = (v << 8) | (b[off + k] as i64);
    }
    v
}

/// Sign-magnitude integer of exactly `nbytes` bytes (wgrib2 int_n): top bit
/// of the FIRST byte is the sign (complex_pk writes `-vmn | (1<<(bits-1))`).
pub fn int_n(b: &[u8], off: usize, nbytes: usize) -> i64 {
    if nbytes == 0 {
        return 0;
    }
    let top = b[off] & 0x80;
    let mut v: i64 = 0;
    for k in 0..nbytes {
        let mask: i64 = if k == 0 { 0x7F } else { 0xFF };
        v = (v << 8) | ((b[off + k] as i64) & mask);
    }
    if top != 0 { -v } else { v }
}

/// Seconds-of-time conversion for Code Table 4.4 units. Month/year-like
/// units are ambiguous and rejected upstream. Returns None for those.
pub fn ct44_secs(unit: u8) -> Option<i64> {
    match unit {
        0 => { return Some(60); }          // minute
        1 => { return Some(3600); }        // hour (GFS/HRRR convention)
        2 => { return Some(86400); }       // day
        10 => { return Some(10800); }      // 3 hours
        11 => { return Some(21600); }      // 6 hours
        12 => { return Some(43200); }      // 12 hours
        13 => { return Some(1); }          // second
        _ => { return None; }              // month/year/etc: unsupported
    }
}

/// UTC epoch seconds for a Gregorian date (Howard Hinnant days_from_civil).
pub fn epoch_secs(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> i64 {
    let y: i64 = if month <= 2 { year as i64 - 1 } else { year as i64 };
    let era: i64 = if y < 0 { (y / 400) - 1 } else { y / 400 };
    let yoe: i64 = y - era * 400;
    let mp: i64 = (if month > 2 { month - 3 } else { month + 9 }) as i64;
    let doy: i64 = ((153 * mp + 2) / 5 + (day as i64) - 1 + 365 * yoe + yoe / 4 - yoe / 100) as i64;
    let doe: i64 = doy + yoe * 365 + yoe / 4 - yoe / 100;
    let se: i64 = era * 146097 + doe - 100000; // days since 1970-01-01
    ((((se * 24) + (hour as i64)) * 60 + (minute as i64)) * 60) + (second as i64)
}

/// Normalize longitude to [0, 360) (mirrors wx-core norm_lon).
pub fn norm_lon_f(x: f64) -> f64 {
    let v = x % 360.0;
    if v < 0.0 { v + 360.0 } else { v }
}

pub fn f64_abs(x: f64) -> f64 {
    if x < 0.0 { -x } else { x }
}

// ---------------------------------------------------------------------------
// section parsers
// ---------------------------------------------------------------------------

/// Section 1 template 1.0 -> reference time + end-of-file flag.
/// Payload offsets: [7..8] year u2, [9] mon, [10] day, [11] hr, [12] min,
/// [13] sec, [14] production status (CT 1.3, bit 0x08 = end of file).
pub fn parse_sec1(p: &[u8]) -> Result<(RunTime, bool), GribErr> {
    if p.len() < 15 {
        return Err(GribErr::Truncated);
    }
    let ep = epoch_secs(be_u16(p, 7), p[9], p[10], p[11], p[12], p[13]);
    let rt = RunTime {
        year: be_u16(p, 7),
        month: p[9],
        day: p[10],
        hour: p[11],
        minute: p[12],
        second: p[13],
        epoch_secs: ep,
    };
    let eof = (p[14] & 0x08) != 0;
    Ok((rt, eof))
}

/// Section 3 -> wx-core GridDef. Templates 3.0 and 3.30 only. Offsets follow
/// the NCEP spec layout as written by the bootstrap writer (which wgrib2
/// reads correctly); wgrib2's own Sec3 accessors are one byte off on the
/// number-of-points field but agree because the source-of-grid byte is 0.
pub fn parse_sec3(p: &[u8]) -> Result<GridDef, GribErr> {
    if p.len() < 10 {
        return Err(GribErr::Truncated);
    }
    let tmpl = be_u16(p, 7);
    match tmpl {
        0 => { return parse_grid_regular(p); }
        30 => { return parse_grid_lambert(p); }
        _ => { return Err(GribErr::UnsupportedGrid(tmpl)); }
    }
}

fn parse_grid_regular(p: &[u8]) -> Result<GridDef, GribErr> {
    if p.len() < 67 {
        return Err(GribErr::Truncated);
    }
    let nx = be_u32(p, 25);
    let ny = be_u32(p, 29);
    let lat1 = sm_i4(p, 41) as f64 / 1.0e6;
    let lon1 = norm_lon_f(be_u32(p, 45) as f64 / 1.0e6);
    let dlon = f64_abs(sm_i4(p, 58) as f64 / 1.0e6);
    let dlat = f64_abs(sm_i4(p, 62) as f64 / 1.0e6);
    let flag33 = p[49];
    let scan = p[66];
    if dlon <= 0.0 || dlat <= 0.0 || nx == 0 || ny == 0 {
        return Err(GribErr::BadField("zero grid increment or size"));
    }
    let g = GridRegular {
        nx: nx,
        ny: ny,
        lon1: lon1,
        lat1: lat1,
        dlon: dlon,
        dlat: dlat,
        scan_mode: scan,
        winds_grid: (flag33 & 0x08) != 0,
    };
    Ok(GridDef::Regular(g))
}

fn parse_grid_lambert(p: &[u8]) -> Result<GridDef, GribErr> {
    if p.len() < 76 {
        return Err(GribErr::Truncated);
    }
    let nx = be_u32(p, 25);
    let ny = be_u32(p, 29);
    let la1 = sm_i4(p, 33) as f64 / 1.0e6;
    let lo1 = be_u32(p, 37) as f64 / 1.0e6;
    let flag33 = p[41];
    let lad = sm_i4(p, 42) as f64 / 1.0e6;
    let lov = be_u32(p, 46) as f64 / 1.0e6;
    let dx_mm = sm_i4(p, 50);
    let dy_mm = sm_i4(p, 54);
    let proj_flag = p[58];
    let scan = p[59];
    let latin1 = sm_i4(p, 60) as f64 / 1.0e6;
    let latin2 = sm_i4(p, 64) as f64 / 1.0e6;
    if dx_mm <= 0 || dy_mm <= 0 || nx == 0 || ny == 0 {
        return Err(GribErr::BadField("non-positive Lambert dx/dy or size"));
    }
    // earth shape byte (Code Table 3.2) at payload offset 9
    let g = GridLambert::new(nx, ny, la1, lo1, lov, lad, latin1, latin2,
                             dx_mm as f64 / 1000.0, dy_mm as f64 / 1000.0,
                             proj_flag, scan, p[9], (flag33 & 0x08) != 0);
    Ok(GridDef::Lambert(g))
}

/// Section 4 product definition info (internal).
pub struct ProdInfo {
    pub category: u8,
    pub number: u8,
    pub fc_secs: i64,
    pub levels: Vec<(u8, f64)>,
    pub stat: StatInfo,
}

pub fn parse_sec4(p: &[u8]) -> Result<ProdInfo, GribErr> {
    if p.len() < 29 {
        return Err(GribErr::Truncated);
    }
    let ncoord = be_u16(p, 0);
    if ncoord != 0 {
        return Err(GribErr::UnsupportedCoordLists);
    }
    let tmpl = be_u16(p, 2);
    if tmpl != 0 && tmpl != 8 {
        return Err(GribErr::UnsupportedPdt(tmpl));
    }
    let cat = p[4];
    let num = p[5];
    let unit = p[12];
    let fc_raw = be_u32(p, 13);
    let us = match ct44_secs(unit) {
        None => { return Err(GribErr::BadField("ambiguous CT 4.4 time unit")); }
        Some(us) => { us }
    };
    let fc_secs = (fc_raw as i64) * us;

    // fixed surfaces (CT 4.5): first at payload 17, second at 23.
    let mut levels = Vec::<(u8, f64)>::new();
    let t1 = p[17];
    if t1 != 255 {
        let sf = p[18] as i8;
        let scaled = sm_i4(p, 19) as f64;
        levels.push(norm_level(t1, sf, scaled));
    }
    let t2 = p[23];
    if t2 != 255 {
        let sf = p[24] as i8;
        let scaled = sm_i4(p, 25) as f64;
        levels.push(norm_level(t2, sf, scaled));
    }

    let stat = match tmpl {
        0 => {
            StatInfo { kind: StatKind::Instantaneous, ct4_11: -1, begin_secs: 0,
                       end_secs: 0, step_secs: 0 }
        }
        8 => {
            let sr = parse_stat_block(p, fc_secs);
            match sr {
                Err(e) => { return Err(e); }
                Ok(si) => { si }
            }
        }
        _ => { return Err(GribErr::UnsupportedPdt(tmpl)); }
    };
    let out = ProdInfo {
        category: cat,
        number: num,
        fc_secs: fc_secs,
        levels: levels,
        stat: stat,
    };
    Ok(out)
}

/// Normalize one fixed-surface level: value = scaled * 10^-scalefactor; isobaric
/// (CT 4.5 code 100) arrives in Pa and is normalized to hPa so it matches the
/// config/API convention ("isobaric:850" = hPa). Documented decision.
fn norm_level(code: u8, scale_factor: i8, scaled: f64) -> (u8, f64) {
    let raw = scaled * pow10(-scale_factor as i32);
    if code == 100 {
        return (code, raw / 100.0);
    }
    return (code, raw);
}

pub fn pow10(e: i32) -> f64 {
    let mut v: f64 = 1.0;
    let mut k = 0;
    while k < e.abs() {
        v *= 10.0;
        k += 1;
    }
    if e < 0 { 1.0 / v } else { v }
}

/// PDT 4.8 stat block (layout validated empirically against wgrib2 readback,
/// see docs/DECISIONS.md): [29..35] valid fmtd time, [36] n ranges,
/// [37..40] opaque, then 12-byte blocks: CT4.6 period flag, CT4.11 interval
/// code, range unit, u4 range length, step unit, u4 step length.
fn parse_stat_block(p: &[u8], fc_secs: i64) -> Result<StatInfo, GribErr> {
    if p.len() < 53 {
        return Err(GribErr::Truncated);
    }
    let nrange = p[36];
    if nrange < 1 {
        return Err(GribErr::BadField("zero stat time ranges"));
    }
    let b0 = 41; // first range block
    let period = p[b0];
    let ct411 = p[b0 + 1];
    let run_unit = p[b0 + 2];
    let rlen = be_u32(p, b0 + 3);
    let su_unit = p[b0 + 7];
    let slen = be_u32(p, b0 + 8);
    let rus = match ct44_secs(run_unit) {
        None => { return Err(GribErr::BadField("ambiguous CT 4.4 range unit")); }
        Some(us) => { us }
    };
    let sus = match ct44_secs(su_unit) {
        None => { rus }
        Some(us) => { us }
    };
    let kind = match period {
        0 => { StatKind::Average }
        1 => { StatKind::Accumulation }
        2 => { StatKind::Maximum }
        3 => { StatKind::Minimum }
        _ => { StatKind::Other }
    };
    Ok(StatInfo {
        kind: kind,
        ct4_11: ct411 as i16,
        begin_secs: fc_secs,
        end_secs: fc_secs + (rlen as i64) * rus,
        step_secs: (slen as i64) * sus,
    })
}

// ---------------------------------------------------------------------------
// section 7 representation decoders
// ---------------------------------------------------------------------------

/// Sentinel for "this point is missing" inside integer pipelines (mirrors
/// wgrib2's INT_MAX marker).
pub const MISSING_SENTINEL: i64 = 0x7FFFFFFF;

/// Decode one section 7 payload into `ngrid` f32 values (NaN = masked/missing).
/// `s5` is the section 5 payload, `s6` the section 6 payload (may be None),
/// `s7` the section 7 payload.
pub fn decode_sec7(s3: &GridDef, s5: &[u8], s6: Option<&[u8]>, s7: &[u8]) -> Result<Vec<f32>, GribErr> {
    let tmpl = be_u16(s5, 4);
    let ngrid = (wx_nx(s3) as usize) * (wx_ny(s3) as usize);
    match tmpl {
        0 => { return dec_simple(s5, s6, s7, ngrid); }
        2 => { return dec_complex(s5, s6, s7, ngrid, false); }
        3 => { return dec_complex(s5, s6, s7, ngrid, true); }
        200 => { return dec_runlength(s5, s6, s7, ngrid); }
        4 => { return dec_ieee(s5, s6, s7, ngrid); }
        _ => { return Err(GribErr::UnsupportedData(tmpl)); }
    }
}

pub fn wx_nx(g: &GridDef) -> u32 {
    match g {
        GridDef::Regular(r) => { return r.nx; }
        GridDef::Lambert(r) => { return r.nx; }
    }
}

pub fn wx_ny(g: &GridDef) -> u32 {
    match g {
        GridDef::Regular(r) => { return r.ny; }
        GridDef::Lambert(r) => { return r.ny; }
    }
}

/// Bitmap presence + accessor. Returns Some(bitmap-bytes) when indicator is
/// 0/254 (bit=1 means DEFINED per wgrib2 missing.c), None for 255.
pub fn bitmap_of(s6: Option<&[u8]>) -> Option<&[u8]> {
    match s6 {
        None => { return None; }
        Some(p) => {
            if p.len() < 1 {
                return None;
            }
            match p[0] {
                255 => { return None; }
                0 => { return Some(&p[1..]); }
                254 => { return Some(&p[1..]); }
                _ => { return None; }
            }
        }
    }
}

pub fn bm_defined(bm: &[u8], k: usize) -> bool {
    ((bm[k >> 3] >> (7 - (k & 7))) & 1) != 0
}

/// DRT 5.0 simple packing: v = (ref + n * 2^E) * 10^-S (wgrib2 unpk_0.c).
fn dec_simple(s5: &[u8], s6: Option<&[u8]>, s7: &[u8], ngrid: usize) -> Result<Vec<f32>, GribErr> {
    if s5.len() < 15 {
        return Err(GribErr::Truncated);
    }
    let ref0 = ieee_f4(s5, 6) as f64;
    let e_exp = sm_i2(s5, 10) as i32;
    let s_dec = sm_i2(s5, 12) as i32;
    let nbits = s5[14];
    if nbits < 1 || nbits > 32 {
        return Err(GribErr::BadField("simple packing nbits"));
    }
    let bm = bitmap_of(s6);
    let f2 = pow2(e_exp);
    let f10 = pow10(-s_dec);
    let mut out = Vec::<f32>::new();
    let mut bitpos = 0;
    for k in 0..ngrid {
        if bm_is_missing(bm, k) {
            out.push(nan_f32());
            continue;
        }
        let n = get_bits(s7, bitpos, nbits) as f64;
        bitpos += nbits as usize;
        out.push(((ref0 + n * f2) * f10) as f32);
    }
    Ok(out)
}

pub fn bm_is_missing(bm: Option<&[u8]>, k: usize) -> bool {
    match bm {
        None => { return false; }
        Some(b) => { return !bm_defined(b, k); }
    }
}

pub fn pow2(e: i32) -> f64 {
    let mut v: f64 = 1.0;
    let mut k = 0;
    while k < e.abs() {
        v *= 2.0;
        k += 1;
    }
    if e < 0 { 1.0 / v } else { v }
}

/// Canonical NaN for masked/missing data (same trick as wx-core nan_f64).
pub fn nan_f32() -> f32 {
    let neg: f32 = -1.0;
    neg.sqrt()
}

/// DRT 5.2/5.3 complex packing, full wgrib2 `unpk_complex.c` semantics:
/// group reference values, per-group widths & lengths, one continuous value
/// bitstream, primary missing-value management, first/second-order spatial
/// differencing with sign-magnitude vmn and unsigned extra seed values.
fn dec_complex(s5: &[u8], s6: Option<&[u8]>, s7: &[u8], ngrid: usize,
               allow_spatial: bool) -> Result<Vec<f32>, GribErr> {
    if s5.len() < 44 {
        return Err(GribErr::Truncated);
    }
    let np = be_u32(s5, 0) as usize;      // defined points (or ngrid, no bm)
    let ref0 = ieee_f4(s5, 6) as f64;
    let e_exp = sm_i2(s5, 10) as i32;
    let s_dec = sm_i2(s5, 12) as i32;
    let nbits = s5[14];                   // group-reference bit width
    let nsub = s5[17];                    // CT 5.5 missing mgmt: 0/1/2
    let ngroups = be_u32(s5, 26) as usize;
    let ref_w = s5[30] as usize;
    let wbits = s5[31];
    let ref_len = be_u32(s5, 32) as usize;
    let len_factor = s5[36] as i64;
    let len_last = be_u32(s5, 37) as usize;
    let lbits = s5[41];
    let spatial = s5[42];                 // CT 5.6: 0 none, 1 first, 2 second
    let eo = s5[43];                      // extra octets (seeds + vmn)
    if spatial != 0 && !allow_spatial {
        return Err(GribErr::BadField("spatial diff flag on template 5.2"));
    }
    if spatial > 2 {
        return Err(GribErr::BadField("bad CT 5.6 value"));
    }
    if nbits > 32 || wbits > 32 || lbits > 32 {
        return Err(GribErr::BadField("complex bit widths too wide"));
    }

    let bm = bitmap_of(s6);

    // degenerate: zero groups -> every defined point is the reference value
    if ngroups == 0 {
        let outv = ((ref0) * pow10(-s_dec)) as f32;
        let mut out = Vec::<f32>::new();
        for k in 0..ngrid {
            out.push(if bm_is_missing(bm, k) { nan_f32() } else { outv });
        }
        return Ok(out);
    }

    // ---- header arrays (byte-aligned phases) ----
    let mut pos = 0; // bit cursor into s7
    let mut extra0: i64 = 0;
    let mut extra1: i64 = 0;
    let mut vmn: i64 = 0;
    if eo > 0 {
        let nb = pos >> 3;
        let nseed = if spatial == 2 { 2 } else { 1 };
        let eoz = eo as usize;
        extra0 = uint_n(s7, nb, eoz);
        if spatial == 2 {
            extra1 = uint_n(s7, nb + eoz, eoz);
        }
        vmn = int_n(s7, nb + eoz * nseed, eoz);
        pos += eoz * 8 * (nseed + 1);
    }
    let mut refs = Vec::<i64>::new();
    for _ in 0..ngroups {
        refs.push(get_bits(s7, pos, nbits) as i64);
        pos += nbits as usize;
    }
    pos = ((pos + 7) / 8) * 8;
    let mut widths = Vec::<usize>::new();
    for _ in 0..ngroups {
        widths.push(get_bits(s7, pos, wbits) as usize + ref_w);
        pos += wbits as usize;
    }
    pos = ((pos + 7) / 8) * 8;
    let mut lens = Vec::<usize>::new();
    let lphase = pos; // length phase start (byte-aligned)
    for _ in 0..(ngroups - 1) {
        let d = get_bits(s7, pos, lbits) as i64;
        pos += lbits as usize;
        lens.push((d * len_factor + (ref_len as i64)) as usize);
    }
    lens.push(len_last);
    // complex_pk.c RESERVES ceil(ngroups * lbits / 8) bytes for this phase
    // (one slot more than the ngroups-1 stored values); mirror that padding.
    pos = ((lphase + ngroups * (lbits as usize) + 7) / 8) * 8;

    let total = {
        let mut t = 0;
        for v in &lens { t += v; }
        t
    };
    if total != np {
        return Err(GribErr::BadField("complex packing point count mismatch"));
    }

    // ---- continuous value bitstream ----
    let mut ud = Vec::<i64>::new();
    for gi in 0..ngroups {
        let w = widths[gi];
        let wu8 = w as u8;
        for _ in 0..lens[gi] {
            ud.push(if w == 0 { 0 } else { get_bits(s7, pos, wu8) as i64 });
            pos += w;
        }
    }

    // ---- missing substitution + group reference addition ----
    let all_m = (((1 as u64) << (nbits as u32)) - 1) as i64;
    for gi in 0..ngroups {
        let off = {
            let mut o = 0;
            for k in 0..gi { o += lens[k]; }
            o
        };
        let w = widths[gi];
        let m1 = if w == 0 { all_m } else { ((((1 as u64) << (w as u32)) - 1)) as i64 };
        let m2 = if nsub >= 2 && w != 0 { m1 - 1 } else { -2 };
        if w == 0 && (refs[gi] == m1 || (nsub >= 2 && refs[gi] == m2)) {
            for k in 0..lens[gi] { ud[off + k] = MISSING_SENTINEL; }
            continue;
        }
        for k in 0..lens[gi] {
            let v = ud[off + k];
            if nsub >= 1 && (v == m1 || (nsub >= 2 && v == m2)) {
                ud[off + k] = MISSING_SENTINEL;
            } else {
                ud[off + k] = v + refs[gi];
            }
        }
    }

    // ---- spatial differencing post-processing (unpk_complex.c lines ~390+) ----
    if spatial == 1 {
        let n = ud.len();
        let mut i = 0;
        while i < n {
            if ud[i] == MISSING_SENTINEL { i += 1; }
            else { ud[i] = extra0; i += 1; break; }
        }
        let mut last = extra0;
        while i < n {
            if ud[i] != MISSING_SENTINEL {
                ud[i] = ud[i] + vmn + last;
                last = ud[i];
            }
            i += 1;
        }
    } else if spatial == 2 {
        let n = ud.len();
        let mut i = 0;
        while i < n {
            if ud[i] == MISSING_SENTINEL { i += 1; }
            else { ud[i] = extra0; i += 1; break; }
        }
        while i < n {
            if ud[i] == MISSING_SENTINEL { i += 1; }
            else { ud[i] = extra1; i += 1; break; }
        }
        let mut pen = extra0;
        let mut last = extra1;
        while i < n {
            if ud[i] != MISSING_SENTINEL {
                ud[i] = ud[i] + vmn + last + last - pen;
                pen = last;
                last = ud[i];
            }
            i += 1;
        }
    }

    // ---- final scaling + bitmap interleave ----
    let f2 = pow2(e_exp);
    let f10 = pow10(-s_dec);
    let mut out = Vec::<f32>::new();
    let mut j = 0;
    for k in 0..ngrid {
        if bm_is_missing(bm, k) {
            out.push(nan_f32());
            continue;
        }
        let v = ud[j];
        j += 1;
        out.push(if v == MISSING_SENTINEL { nan_f32() }
                 else { ((ref0 + (v as f64) * f2) * f10) as f32 });
    }
    Ok(out)
}

/// DRT 5.200 run length (wgrib2 unpk_run_length.c semantics mirrored exactly,
/// including its bitmap convention where bit=1 marks UNDEFINED positions —
/// note this is INVERTED vs. the simple/complex paths; documented quirk).
fn dec_runlength(s5: &[u8], s6: Option<&[u8]>, s7: &[u8], ngrid: usize) -> Result<Vec<f32>, GribErr> {
    if s5.len() < 12 {
        return Err(GribErr::Truncated);
    }
    let nbits = s5[6];
    let mv = be_u16(s5, 7);
    let mvl = be_u16(s5, 9);
    let ds_raw = s5[11];
    let ds = if ds_raw > 127 { -(ds_raw as i16 - 128) } else { ds_raw as i16 };
    if nbits < 1 || nbits > 32 || mv as u32 > mvl as u32 {
        return Err(GribErr::BadField("run-length params inconsistent"));
    }
    let mut levels = Vec::<f64>::new();
    let mut i = 1;
    while i <= mvl as u16 {
        let v = sm_i2(s5, 10 + i as usize * 2) as f64;
        levels.push(v * pow10(-ds as i32));
        i += 1;
    }
    let nvals = (s7.len() * 8) / (nbits as usize);
    let range = ((1 as u64) << (nbits as u32)) - 1 - (mv as u64);
    if nvals == 0 || mv == 0 {
        let mut out = Vec::<f32>::new();
        for _ in 0..ngrid { out.push(nan_f32()); }
        return Ok(out);
    }
    if range == 0 || mvl as u64 > 4096 {
        return Err(GribErr::BadField("run-length range/level count absurd"));
    }
    // read index stream
    let mut vals = Vec::<u32>::new();
    let mut pos = 0;
    for _ in 0..nvals {
        vals.push(get_bits(s7, pos, nbits));
        pos += nbits as usize;
    }
    let bm = bitmap_of(s6);
    let mut out = Vec::<f32>::new();
    let mut i = 0;
    while i < nvals {
        let v0 = vals[i];
        if v0 > mv as u32 {
            return Err(GribErr::BadField("run-length bad level index"));
        }
        i += 1;
        let mut n: usize = 1;
        let mut factor: u64 = 1;
        while i < nvals && vals[i] > mv as u32 {
            n += (factor * ((vals[i] as u64) - (mv as u64) - 1)) as usize;
            factor = factor * range;
            i += 1;
        }
        let val = if v0 == 0 { nan_f32() } else { levels[(v0 as usize) - 1] as f32 };
        for _ in 0..n {
            match bm {
                None => { out.push(val); }
                Some(b) => {
                    while out.len() < b.len() * 8 && bm_defined(b, out.len()) {
                        out.push(nan_f32());
                    }
                    out.push(val);
                }
            }
            if out.len() > ngrid {
                return Err(GribErr::BadField("run-length overflow"));
            }
        }
    }
    while out.len() < ngrid {
        out.push(nan_f32());
    }
    if out.len() != ngrid {
        return Err(GribErr::BadField("run-length count mismatch"));
    }
    Ok(out)
}

/// DRT 5.4 IEEE raw float32 values (precision byte must be 1, like wgrib2).
fn dec_ieee(s5: &[u8], s6: Option<&[u8]>, s7: &[u8], ngrid: usize) -> Result<Vec<f32>, GribErr> {
    if s5.len() < 7 || s5[6] != 1 {
        return Err(GribErr::BadField("ieee packing precision unsupported"));
    }
    let bm = bitmap_of(s6);
    let mut out = Vec::<f32>::new();
    let mut j = 0;
    for k in 0..ngrid {
        if bm_is_missing(bm, k) {
            out.push(nan_f32());
            continue;
        }
        out.push(ieee_f4(s7, j * 4));
        j += 1;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// message walk over a whole file buffer
// ---------------------------------------------------------------------------

/// Decode every message in `bytes`. Section framing is followed by declared
/// lengths; both terminators (`7777` bare and LEN-prefixed end section) are
/// accepted. Messages failing the `filter` never touch section 7 decoding.
pub fn decode_stream(bytes: &[u8], filter: ParamFilter) -> Result<(Vec<DecodedMessage>, DecodeStats), GribErr> {
    let mut msgs = Vec::<DecodedMessage>::new();
    let mut st = DecodeStats {
        messages_total: 0,
        messages_decoded: 0,
        messages_skipped: 0,
        sec7_bytes_decoded: 0,
        sec7_bytes_skipped: 0,
    };
    let n = bytes.len();
    let mut pos = 0;
    while pos < n {
        if n - pos < 16 {
            return Err(GribErr::Truncated);
        }
        if !(bytes[pos] == 0x47 && bytes[pos + 1] == 0x52 &&
             bytes[pos + 2] == 0x49 && bytes[pos + 3] == 0x42) {
            return Err(GribErr::BadMagic);
        }
        if bytes[pos + 7] != 2 {
            return Err(GribErr::BadEdition);
        }
        let discipline = bytes[pos + 6];
        let msglen = be_u64(bytes, pos + 8);
        if msglen < 16 || (pos as u64) + msglen > (n as u64) {
            return Err(GribErr::Truncated);
        }
        let body_end = pos + (msglen as usize);

        let mut run_time: Option<RunTime> = None;
        let mut grid: Option<GridDef> = None;
        let mut prod: Option<ProdInfo> = None;
        let mut s5: Option<&[u8]> = None;
        let mut s6: Option<&[u8]> = None;
        let mut s7: Option<&[u8]> = None;
        let mut eof_flag = false;

        let mut q = pos + 16;
        while q < body_end {
            if q + 4 <= body_end && bytes[q] == 0x37 && bytes[q + 1] == 0x37 &&
               bytes[q + 2] == 0x37 && bytes[q + 3] == 0x37 {
                break; // bare wgrib2 terminator
            }
            if q + 5 > body_end {
                return Err(GribErr::Truncated);
            }
            let ln = be_u32(bytes, q) as usize;
            let num = bytes[q + 4];
            if ln < 6 || q + ln > body_end {
                return Err(GribErr::Truncated);
            }
            let payload = &bytes[q + 5..q + ln];
            match num {
                1 => {
                    let pr = parse_sec1(payload);
                    match pr {
                        Err(e) => { return Err(e); }
                        Ok(rt_eof) => {
                            run_time = Some(rt_eof.0);
                            eof_flag = rt_eof.1;
                        }
                    }
                }
                2 => {}
                3 => {
                    let pg = parse_sec3(payload);
                    match pg {
                        Err(e) => { return Err(e); }
                        Ok(gd) => { grid = Some(gd); }
                    }
                }
                4 => {
                    let pp = parse_sec4(payload);
                    match pp {
                        Err(e) => { return Err(e); }
                        Ok(pi) => { prod = Some(pi); }
                    }
                }
                5 => { s5 = Some(payload); }
                6 => { s6 = Some(payload); }
                7 => { s7 = Some(payload); }
                8 => {}
                _ => {}
            }
            q += ln;
        }
        let rt = match run_time {
            Some(x) => { x }
            None => { return Err(GribErr::BadSectionOrder); }
        };
        let gd = match grid {
            Some(x) => { x }
            None => { return Err(GribErr::BadSectionOrder); }
        };
        let pi = match prod {
            Some(x) => { x }
            None => { return Err(GribErr::BadSectionOrder); }
        };
        let p5 = match s5 {
            Some(x) => { x }
            None => { return Err(GribErr::BadSectionOrder); }
        };
        let p7 = match s7 {
            Some(x) => { x }
            None => { return Err(GribErr::BadSectionOrder); }
        };

        st.messages_total += 1;
        let keep = filter.allows(discipline, pi.category, pi.number);
        if keep {
            let dec = decode_sec7(&gd, p5, s6, p7);
            match dec {
                Err(e) => { return Err(e); }
                Ok(data) => {
                    st.sec7_bytes_decoded += p7.len();
                    st.messages_decoded += 1;
                    let wg = grid_winds(&gd);
                    let valid = rt.epoch_secs + pi.fc_secs;
                    let pname = param_name_for(discipline, pi.category, pi.number);
                    let m = DecodedMessage {
                        run: rt,
                        fc_secs: pi.fc_secs,
                        valid_epoch_secs: valid,
                        discipline: discipline,
                        category: pi.category,
                        number: pi.number,
                        param_name: pname,
                        levels: pi.levels,
                        grid: gd,
                        stat: pi.stat,
                        winds_grid: wg,
                        data: data,
                    };
                    msgs.push(m);
                }
            }
        } else {
            st.messages_skipped += 1;
            st.sec7_bytes_skipped += p7.len();
        }

        pos = body_end;
        if eof_flag {
            break;
        }
    }
    return Ok((msgs, st));
}

/// True when the parsed grid carries flag table 3.3 bit 0x08 (grid-relative
/// vector components).
pub fn grid_winds(g: &GridDef) -> bool {
    match g {
        GridDef::Regular(r) => { return r.winds_grid; }
        GridDef::Lambert(r) => { return r.winds_grid; }
    }
}

// ---------------------------------------------------------------------------
// public file-level API
// ---------------------------------------------------------------------------

/// Decode one GRIB2 file into messages + stats. `filter` decides which
/// messages get section-7 unpacked (spec §4.2 skip-unpack requirement).
pub fn decode_file(path: String, filter: ParamFilter) -> Result<(Vec<DecodedMessage>, DecodeStats), GribErr> {
    let f = std::fs::File::open(path);
    match f {
        Err(_) => { return Err(GribErr::Io); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return Err(GribErr::Io); }
                Ok(region) => { return decode_stream(&region[0..region.len()], filter); }
            }
        }
    }
}

/// Decode several files in parallel (rayon). The ingest agent will wire this
/// into a dedicated thread pool in milestone 3; per-message work inside
/// `decode_stream` is sequential but file-level parallelism is enough to
/// saturate decode across files without starving the query path.
pub fn decode_files_parallel(paths: Vec<String>, filter: ParamFilter) -> Vec<Result<(Vec<DecodedMessage>, DecodeStats), GribErr>> {
    let out = paths.par_iter().map(|p| decode_file(p.to_string(), filter.clone())).collect();
    return out;
}

// ---------------------------------------------------------------------------
// wind rotation (spec §4.4): grid-relative U/V -> earth-relative U/V
// ---------------------------------------------------------------------------

/// Rotate grid-relative components to earth-relative using the LCC rotation
/// angle beta = bearing of the grid +i direction from true north (wx-core
/// `GridLambert.rot_angle_deg`). Grid u is along +i, v along +j. With
/// g_i = sin(beta) e_E + cos(beta) e_N and g_j = cos(beta) e_E - sin(beta) e_N
/// (rows scan southward in GFS/HRRR convention):
///     u_east  = u_g * sin(beta) + v_g * cos(beta)
///     v_north = u_g * cos(beta) - v_g * sin(beta)
/// This is an orthonormal rotation: magnitudes are preserved exactly up to
/// float rounding. Regular lat/lon grids are returned unchanged (wgrib2
/// treats their winds as earth-relative; documented decision).
pub fn rotate_winds_to_earth(g: &GridDef, u_in: &[f32], v_in: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let mut ue = Vec::<f32>::new();
    let mut vn = Vec::<f32>::new();
    match g {
        GridDef::Regular(_) => {
            for x in u_in { ue.push(*x); }
            for x in v_in { vn.push(*x); }
            return (ue, vn);
        }
        GridDef::Lambert(g) => {
            let nxi = (g.nx) as usize;
            let n = u_in.len();
            for k in 0..n {
                let i = k % nxi;
                let j = k / nxi;
                let beta = g.rot_angle_deg(i as f64, j as f64) * D2R;
                let s = beta.sin();
                let c = beta.cos();
                let ug = u_in[k];
                let vg = v_in[k];
                ue.push(ug * s as f32 + vg * c as f32);
                vn.push(ug * c as f32 - vg * s as f32);
            }
            return (ue, vn);
        }
    }
}

/// Config validation helper (spec §4.4 rule): a model whose grid carries
/// grid-relative vector flags must configure BOTH UGRD and VGRD — rotating a
/// single component is meaningless. Returns Some(message) on violation.
pub fn check_wind_pair(params: Vec<String>, winds_grid: bool) -> Option<String> {
    if !winds_grid {
        return None;
    }
    let mut has_u = false;
    let mut has_v = false;
    for p in params {
        if p == "UGRD" { has_u = true; }
        if p == "VGRD" { has_v = true; }
    }
    if has_u != has_v {
        return Some("grid-relative model configures only one of UGRD/VGRD: earth-relative rotation requires both components".to_string());
    }
    return None;
}
