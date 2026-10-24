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
//!   typed `UnsupportedCoordLists`. V1.1 phase 3: the three STRUCTURAL cases
//!   (coordinate lists, unknown grid template, unknown PDT template) SKIP
//!   THAT MESSAGE and resume at the next message boundary instead of aborting
//!   the whole file; they are counted in `DecodeStats.messages_unsupported`
//!   and NEVER in the ingest error counter (spec §4.2: skip what you cannot
//!   use). A malformed/corrupt section 7 still fails the decode loudly.
//! * Section 6 bitmap: indicator 255 = no bitmap, 0/254 = bitmap present
//!   (bit=1 means the point is DEFINED, matching `missing.c`); masked points
//!   decode to NaN.
//! * Section 7 representations: DRT 5.0 simple packing, 5.2 complex packing
//!   WITHOUT spatial differencing, 5.3 complex packing WITH spatial
//!   differencing (first AND second order, group widths/lengths, primary
//!   missing-value management), 5.200 run length (the variant wgrib2 and
//!   NCEP actually emit — there is no 5.1/5.2 dictionary encoder anywhere in
//!   the wgrib2 ecosystem; dictionary 5.1 -> typed UnsupportedData), 5.4 IEEE,
//!   plus (V1.1 phase 2) 5.40 JPEG2000, 5.41 PNG and 5.42 CCSDS AEC decoded by
//!   the pure-Rust backends in `codecs::rs` (`hayro-jpeg2000`, `zlib-rs` + own
//!   PNG chunk/unfilter code, `rust-aec`). Any other template -> typed
//!   `UnsupportedData(tmpl)`; a well-formed stream using a construct those
//!   backends deliberately do not implement -> `CodecUnsupported(reason)`;
//!   an unparseable stream -> `CodecFailure(reason)`.
//! * Filenames are never consulted for metadata (spec §4.2): everything comes
//!   from sections 0/1/3/4.
//!
//! The skip-unpack path (spec §4.2: "Skip messages not in the configured
//! parameter list without fully decoding them") consults the section-4
//! product definition BEFORE touching section 7; `DecodeStats` records how
//! many section-7 payloads were skipped vs. actually unpacked so tests can
//! prove the filter works.

use wx_core::grid::{gaussian_latitudes, GridDef, GridGaussian, GridRegular, GridLambert};
use wx_core::tables::param_name_for;
use wx_core::grid::D2R;

use crate::codecs;

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
    /// A section-7 codec backend could not parse the stream at all (corrupt,
    /// truncated, or rejected by the third-party decoder).
    CodecFailure(&'static str),
    /// The stream parsed but uses a construct the backend deliberately does
    /// not implement (see codecs::rs module docs and docs/DECISIONS.md).
    CodecUnsupported(&'static str),
}

/// Why a message was SKIPPED for structural reasons (V1.1 phase 3). These
/// never abort the file: the walk resumes at the next message boundary
/// (`pos = body_end`, i.e. the length declared in section 0), the message is
/// counted in `DecodeStats.messages_unsupported`, and the pipeline turns it
/// into a note + `skipped_messages` increment — NOT an ingest error.
#[derive(Clone, Debug)]
pub enum SkipReason {
    CoordLists, // section 4 carries coordinate values after the template
    Grid(u16), // section 3 template this decoder does not implement
    Pdt(u16), // section 4 product-definition template not implemented
}

pub fn skip_reason_text(r: &SkipReason) -> String {
    match r {
        SkipReason::CoordLists => {
            return "coordinate list after the product-definition template".to_string();
        }
        SkipReason::Grid(t) => { return format!("grid template 3.{0}", t); }
        SkipReason::Pdt(t) => { return format!("product definition template 4.{0}", t); }
    }
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
/// relative to the run time: an ACCUMULATION window ENDS at the message's
/// interval end (its forecast time) and clamps at the run time when shorter,
/// while averages/max/min run FORWARD from it — see `parse_stat_block`
/// (AUDIT_1.3 P2 corrected this; the shifted form used to be stored for every
/// kind). `ct4_11` keeps the raw Code Table 4.11 interval code of the first
/// time-range block for round-tripping/diagnostics.
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
    pub messages_skipped: u32, // filtered out by the parameter allow-list
    pub messages_unsupported: u32, // structurally unparseable: SKIP-AND-NOTE
    /// V1.3 phase E4a SKIP-REASON OBSERVABILITY: the unsupported total broken
    /// down BY KIND so operators can tell "this product line needs template
    /// 3.40" from "this producer emits coordinate lists" without re-running a
    /// decoder by hand. The Vecs are (template number -> count) in first-seen
    /// order; expected length is tiny (one or two distinct templates per
    /// product line). Flat totals derive from them; the pipeline mirrors the
    /// three KINDS into flat Prometheus counters (no label machinery exists).
    pub skip_coordlists: u32,
    pub skip_grid: Vec<(u16, u32)>,
    pub skip_pdt: Vec<(u16, u32)>,
    pub sec7_bytes_decoded: usize,
    pub sec7_bytes_skipped: usize,
}

/// Bump one (template -> count) tally list (V1.3 phase E4a).
fn bump_kind(list: &mut Vec<(u16, u32)>, t: u16) {
    for k in 0 .. list.len() {
        if list[k].0 == t {
            list[k].1 += 1;
            return;
        }
    }
    list.push((t, 1));
}

/// One-line breakdown of WHY messages were skipped as structurally
/// unsupported (V1.3 phase E4a), e.g. "2 skipped: 2 grid template 3.40".
/// Empty string when nothing was unsupported (callers append conditionally).
pub fn skip_tally_text(st: &DecodeStats) -> String {
    if st.messages_unsupported == 0 {
        return "".to_string();
    }
    let mut parts: Vec<String> = Vec::<String>::new();
    if st.skip_coordlists > 0 {
        parts.push(format!("{0} coordinate-list", st.skip_coordlists));
    }
    for g in &st.skip_grid {
        parts.push(format!("{0} grid template 3.{1}", g.1, g.0));
    }
    for p in &st.skip_pdt {
        parts.push(format!("{0} PDT 4.{1}", p.1, p.0));
    }
    let joined = parts.join(", ");
    return format!("{0} skipped: {1}", st.messages_unsupported, joined);
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
    let doy: i64 = ((153 * mp + 2) / 5 + (day as i64) - 1) as i64;
    let doe: i64 = (yoe * 365 + yoe / 4 - yoe / 100 + doy) as i64;
    let se: i64 = era * 146097 + doe - 719468; // days since 1970-01-01
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

/// V1.4 phase B1 GLOBAL GRID-POINT CEILING. A grid definition is trusted
/// content: before this cap existed a forged 0x000F4240 x 0x000F4240 template
/// 3.0 (nx = ny = 1e6) parsed cleanly and every downstream plane allocation
/// sized itself from nx*ny — 1e12 points, ~4 TB per parameter field, an
/// instant OOM/abort on the ingest pool (release profile runs overflow
/// checks OFF, so even the multiply itself wrapped silently; see the red-run
/// captures in tests/bounds_grid_cap.rs). 2^26 = 67.1M points is ~64x the
/// largest grid any current NCEP/AI product ships (GFS 0.25 deg = 1,038,240;
/// HRRR 3 km CONUS = 1,904,901), so real producers clear it by two orders of
/// magnitude while absurd shapes get a typed reject the content-hash
/// quarantine machinery absorbs instead of an abort. CONFIG PLUMBING DEFERRED
/// ON PURPOSE: raising/lowering this knob from config.toml is a follow-up
/// (config.rs owned by another agent this wave); until then it is a
/// compile-time constant.
pub const MAX_GRID_POINTS: u64 = 1 << 26;

/// Shared grid-size ceiling check for every section-3 template (V1.4 phase
/// B1). nx, ny arrive as unsigned u32 slots, so the u64 product cannot wrap;
/// the comparison is one multiply + one compare per MESSAGE parse, not per
/// point — hot-path cost is nil.
fn grid_points_ok(nx: u32, ny: u32) -> Result<(), GribErr> {
    if (nx as u64) * (ny as u64) > MAX_GRID_POINTS {
        return Err(GribErr::BadField("grid point count exceeds MAX_GRID_POINTS"));
    }
    return Ok(());
}

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

/// GRIB "scaled value" pair: INT1 scale factor then SIGN-MAGNITUDE INT4
/// value, decoded as value * 10^(-factor) (wgrib2 `int8.c::scaled2dbl`).
/// None when the slot is absent or marked undefined (factor 255).
fn scaled_val(p: &[u8], off: usize) -> Option<f64> {
    if off + 5 > p.len() {
        return None;
    }
    let fac = p[off];
    if fac == 255 || fac > 20 {
        return None;
    }
    let v = sm_i4(p, off + 1);
    let mut d = v as f64;
    let mut k = 0;
    while k < fac as usize {
        d = d / 10.0;
        k += 1;
    }
    return Some(d);
}

/// Raw Code Table 3.3 byte of a section-3 payload plus its grid template,
/// for diagnostics only (the parsed meaning lives in `GridDef.winds_grid`).
pub fn flag33_byte(p: &[u8]) -> (u16, u8) {
    if p.len() < 10 {
        return (0xFFFF, 0);
    }
    let tmpl = be_u16(p, 7);
    match tmpl {
        0 => {
            if p.len() >= 50 { return (0, p[49]); }
            return (0, 0);
        }
        30 => {
            if p.len() >= 42 { return (30, p[41]); }
            return (30, 0);
        }
        40 => {
            // NCEP gaussian usage: the flag byte sits between Lo1 and La2
            // (offsets per wgrib2 grb2.h GDS_Gaussian_* macros, cross-checked
            // against live GFS sflux bytes in V1.3 phase E4b)
            if p.len() >= 50 { return (40, p[49]); }
            return (40, 0);
        }
        _ => { return (tmpl, 0); }
    }
}

/// Section 3 -> wx-core GridDef. Templates 3.0, 3.30 and 3.40 only. Offsets
/// follow the NCEP spec layout as written by the bootstrap writer (which
/// wgrib2 reads correctly); wgrib2's own Sec3 accessors are one byte off on
/// the number-of-points field but agree because the source-of-grid byte is 0.
pub fn parse_sec3(p: &[u8]) -> Result<GridDef, GribErr> {
    if p.len() < 10 {
        return Err(GribErr::Truncated);
    }
    let tmpl = be_u16(p, 7);
    match tmpl {
        0 => { return parse_grid_regular(p); }
        30 => { return parse_grid_lambert(p); }
        40 => { return parse_grid_gaussian(p); }
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
    match grid_points_ok(nx, ny) {
        Err(e) => { return Err(e); }
        Ok(()) => {}
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
    // V1.4 phase B1: same ceiling as template 3.0 (HRRR 1799 x 1059 clears it
    // by ~35x; a forged 1e6 x 1e6 LCC would otherwise size every plane from
    // the product and OOM the ingest pool before any DRT unpack runs).
    match grid_points_ok(nx, ny) {
        Err(e) => { return Err(e); }
        Ok(()) => {}
    }
    // earth shape byte (Code Table 3.2) at payload offset 9
    let mut g = GridLambert::new(nx, ny, la1, lo1, lov, lad, latin1, latin2,
                                 dx_mm as f64 / 1000.0, dy_mm as f64 / 1000.0,
                                 proj_flag, scan, p[9], (flag33 & 0x08) != 0);
    // Codes 1/3/7 declare the axes INLINE in section 3 (see wx-core
    // grid.rs docs): code 1 a user sphere, codes 3/7 a user spheroid.
    // wgrib2 reads them from exactly these slots (`Earth.c::axes_earth`,
    // offsets relative to the shape byte: +1/+2 sphere radius, +6/+7 major,
    // +11/+12 minor), code 3 in km and code 7 in meters.
    match p[9] {
        1 => {
            let r = scaled_val(p, 10);
            match r {
                Some(rr) => {
                    if rr > 1000000.0 { g.set_user_axes(rr, rr); }
                }
                None => {}
            }
        }
        3 => {
            let maj = scaled_val(p, 15);
            let min = scaled_val(p, 20);
            match (maj, min) {
                (Some(a), Some(b)) => { g.set_user_axes(a * 1000.0, b * 1000.0); }
                _ => {}
            }
        }
        7 => {
            let maj = scaled_val(p, 15);
            let min = scaled_val(p, 20);
            match (maj, min) {
                (Some(a), Some(b)) => { g.set_user_axes(a, b); }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(GridDef::Lambert(g))
}

/// GRIB2 grid definition template 3.40 as NCEP uses it for the operational
/// FV3GFS line: global gaussian lat-lon, quasi-regular NON-thinned (V1.3
/// phase E4b; docs/DECISIONS.md §16 has the live-fire byte map). Field
/// offsets are wgrib2's own (`grb2.h::GDS_Gaussian_*`, gds+N == payload
/// N-5) and were cross-checked value-by-value against
/// `tools/wgrib2-full <file> -grid` on production bytes:
///   nx@25 ny@29 basic@33 subangle@37 la1@41 lo1@45 [flags@49] la2@50
///   lo2@54 di@58 nlat@62
/// Acceptance rules (documented limitations):
/// * FULL GLOBAL ONLY: require ny == 2*nlat. Thinned/reduced shapes (CT
///   3.11 = 1/2, where wgrib2 itself consults the flag because dims are
///   missing) and regional gaussian grids skip as UnsupportedGrid(40). Note
///   the live files carry CT 3.11 == 0 ("global full") — the task text's
///   "quasi-regular != 1 => skip" would have skipped every current-GFS
///   message; wgrib2 reads CT 3.11 only when nx/ny are absent, so we do.
/// * Row latitudes must match the P_ny-root table at both ends within
///   1e-3 deg (live bytes agree to 5.4e-7); anything else is an unknown row
///   convention and skips loudly instead of interpolating wrong rows.
/// * Units: honor basic/sub angle exactly like wgrib2 (basic == 0 => 1e-6,
///   the live case; sub-angle follows wgrib2's int8.c::sub_angle quirks).
/// * Global wrap: when lo2-lo1+di spans the globe, use dlon = 360/nx
///   EXACTLY (the stored Di is rounded to 1e-6 deg: 117188 vs the true
///   117187.5; accumulating that error across 3072 columns breaks node
///   exactness at the far edge — wgrib2 likewise recomputes from endpoints).
fn parse_grid_gaussian(p: &[u8]) -> Result<GridDef, GribErr> {
    if p.len() < 66 {
        return Err(GribErr::Truncated);
    }
    let tmpl_skip = |why: &str| -> GribErr {
        // Every template-3.40 shape we decline reports under the SAME bucket
        // (skip kind "grid template 3.40") so counters stay tidy (§15 flat
        // skip buckets); `why` rides in the per-file note text upstream.
        let _ = why;
        return GribErr::UnsupportedGrid(40);
    };
    let nx = be_u32(p, 25);
    let ny = be_u32(p, 29);
    let nlat = be_u32(p, 62);
    if nx == 0 || ny == 0 || nlat == 0 || ny != 2 * nlat {
        return Err(tmpl_skip("thinned/regional or missing dimensions"));
    }
    // V1.4 phase B1: operational gaussian grids clear the ceiling by ~19x
    // (T-coefficients x row counts stay near 3-4M points); the cap fires
    // only on absurd declared dimensions. Placed BEFORE the basic-angle and
    // latitude-table work so a huge shape never reaches any later stage.
    match grid_points_ok(nx, ny) {
        Err(e) => { return Err(e); }
        Ok(()) => {}
    }
    // basic angle / direction-derivative ratio -> units (wgrib2 semantics)
    let basic = sm_i4(p, 33);
    let mut subv = sm_i4(p, 37);
    if p[37] == 0 && p[38] == 0 && p[39] == 0 && p[40] == 0 {
        subv = 1;
    } else if p[37] == 0xFF && p[38] == 0xFF && p[39] == 0xFF && p[40] == 0xFF {
        subv = 1_000_000;
    }
    let units = if basic == 0 {
        1.0e-6
    } else if subv > 0 {
        (basic as f64) / (subv as f64)
    } else {
        return Err(GribErr::BadField("gaussian basic/sub angle ratio nonpositive"));
    };
    let la1 = sm_i4(p, 41) as f64 * units;
    let lon1 = norm_lon_f(be_u32(p, 45) as f64 * units);
    let la2 = sm_i4(p, 50) as f64 * units; // UNALIGNED slot (octet 56), like wgrib2
    let lon2 = norm_lon_f(be_u32(p, 54) as f64 * units);
    let di_raw = sm_i4(p, 58) as f64 * units;
    if di_raw <= 0.0 {
        return Err(GribErr::BadField("zero or negative gaussian di"));
    }
    // orientation: row 0 sits at La1. NS storage (GFS) has La1 north.
    let ns = la1 > la2;
    if !ns && la1 == la2 {
        return Err(tmpl_skip("degenerate row span"));
    }
    // validate the producer's end latitudes against the authoritative table
    let top = match gaussian_latitudes(ny) {
        Some(v) => v[0],
        None => { return Err(tmpl_skip("unsupported row count")); }
    };
    let want_hi = if ns { la1 } else { la2 };
    let want_lo = if ns { la2 } else { la1 };
    let dhi = if want_hi - top >= 0.0 { want_hi - top } else { top - want_hi };
    let dlo = if want_lo + top >= 0.0 { want_lo + top } else { -(want_lo + top) };
    if dhi > 1e-3 || dlo > 1e-3 {
        return Err(tmpl_skip("row ends disagree with P_ny table"));
    }
    // longitude spacing: reconstruct the exact global increment when the
    // grid spans the full circle (stored value carries 1e-6-deg rounding)
    let mut span = lon2 - lon1;
    if span < 0.0 {
        span += 360.0;
    }
    let dlon = if (span + di_raw - 360.0).abs() <= 0.01 {
        360.0 / (nx as f64)
    } else {
        di_raw
    };
    let scan_mode: u8 = if ns { 0x00 } else { 0x40 };
    let flag33 = p[49];
    Ok(GridDef::Gaussian(GridGaussian::new(
        nx, ny, lon1, la1, la2, dlon, scan_mode, p[9], (flag33 & 0x08) != 0)))
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
/// HYBRID (code 105, V1.3 phase E4c) deliberately gets NO conversion: the
/// hybrid/sigma-pressure coordinate is DIMENSIONLESS, so the stored level
/// value is the raw GRIB coordinate as-is (config `hybrid:1` matches what the
/// file says; nothing to convert). Sign handling rides `sm_i4` (sign-magnitude,
/// same as every other level field). Unknown types pass through unchanged and
/// simply never match a configured spec (decode-ok / store-skip, spec §4.2).
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
/// code, range unit, u4 range length, step unit, u4 step length. `fc_secs` is
/// the section-4 period field, which for template 4.8 IS the END of the
/// overall statistical interval relative to the reference time; the window is
/// derived from it per kind below (AUDIT_1.3 P2).
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
    // AUDIT_1.3 P2 THE STATISTICAL WINDOW IS ANCHORED ON THE INTERVAL END.
    // The section-4 period field the caller passes in as `fc_secs` IS the PDT
    // octet pair that carries the END of the overall statistical interval
    // relative to the reference time — verified by OCTET-LEVEL probing against
    // tools/wgrib2 v3.1.3, which renders it as "(N hour fcst)" and moves that
    // text whenever these octets move. So:
    //   * ACCUMULATION: the plane covers the interval that ENDS there,
    //     [end - len, end]. A window reaching before the run time clamps to
    //     the run time, which is exactly the "0-N hour acc anl" product
    //     wgrib2 shows for an fh-0 accumulation (an accumulation can never
    //     include data older than the run).
    //   * AVERAGE / MAXIMUM / MINIMUM / OTHER: the statistic runs FORWARD from
    //     that instant, [fc, fc + len] — what wgrib2 prints for the fixture's
    //     average ("3-6 hour ave fcst") and what the real HRRR 5-6 h WIND
    //     maximum pin in realdata_gate.rs requires.
    // Pre-fix the decoder stored [fc, fc + len] UNIFORMLY, i.e. an accumulation
    // window shifted forward by its own length (fh-6 APCP: 18Z-00Z instead of
    // 12Z-18Z). Values stayed right because the §6.3 guard keys off exact
    // forecast hours; only the §4.2 time-range metadata was wrong.
    // The length keeps its OWN CT 4.4 unit indicator (`rus`), and so does the
    // step (`sus`): nothing here assumes hours.
    let len_secs = (rlen as i64) * rus;
    let (begin_secs, end_secs) = match kind {
        StatKind::Accumulation => {
            let mut b = fc_secs - len_secs;
            if b < 0 { b = 0; }
            (b, b + len_secs)
        }
        _ => { (fc_secs, fc_secs + len_secs) }
    };
    Ok(StatInfo {
        kind: kind,
        ct4_11: ct411 as i16,
        begin_secs: begin_secs,
        end_secs: end_secs,
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
    if s5.len() < 6 {
        return Err(GribErr::Truncated);
    }
    let ngrid = (wx_nx(s3) as usize) * (wx_ny(s3) as usize);
    // V1.4 phase B1 BITMAP LENGTH GUARD: bm_defined indexes bm[k >> 3] for
    // every k < ngrid (simple/complex/ieee interleave AND the codec backends'
    // defined_count), and NO call site ever proved the bitmap actually covers
    // the grid — a section 6 truncated by even one octet walked off the end
    // mid-interleave -> slice panic -> whole-process abort under release
    // panic="abort" (red-run captures: bounds_simple_ieee.rs, both bitmap
    // tests panicked at grib2.rs:866 pre-fix). The GRIB format requires
    // ceil(npoints/8) bitmap octets, so this rejects ONLY corrupt streams
    // (clean fixtures byte-identical; a WELL-FORMED full-set bitmap decodes
    // exactly like no-bitmap, pinned by that same test file). Constructed
    // once per message, covering every DRT uniformly.
    match bitmap_of(s6) {
        None => {}
        Some(b) => {
            if b.len() < (ngrid + 7) / 8 {
                return Err(GribErr::BadField("bitmap too short"));
            }
        }
    }
    // V1.3 phase E4a DEGENERATE SECTION 7 (zero data octets): the framing
    // layer accepts a 5-octet section 7 (see the `floor_ln` sites in
    // decode_stream), and here it becomes a full-size plane WITHOUT running
    // any DRT unpacker (the section-5 template fields of such files are
    // themselves minimal/absent — the real HRRR sample carries only 16
    // payload octets). Semantics, pinned by tests/degenerate_sec7.rs:
    //  * bitmap present  -> EVERY point NaN: zero value octets exist, so even
    //    bitmap-"defined" points have no value; queries answer null
    //    missing_data (spec §4.2 "missing points must be stored as NaN").
    //  * NO bitmap       -> 0.0 everywhere, EXACTLY matching tools/wgrib2-full
    //    on the live 212-byte file (`mean=0:min=0:max=0 undef=0`). The known
    //    producer of this shape is a zero-window accumulation (HRRR APCP with
    //    begin==end==run time) intending a constant zero field; matching the
    //    reference implementation is the correctness bar set by spec §9.
    // Output size ALWAYS comes from the grid definition (ngrid), never from
    // section 5's npoints, so a degenerate message can never yield a silently
    // wrong-shaped plane.
    if s7.is_empty() {
        let fill: f32 = match bitmap_of(s6) {
            Some(_) => { f32::NAN }
            None => { 0.0 }
        };
        return Ok(vec![fill; ngrid]);
    }
    let tmpl = be_u16(s5, 4);
    match tmpl {
        0 => { return dec_simple(s5, s6, s7, ngrid); }
        2 => { return dec_complex(s5, s6, s7, ngrid, false); }
        3 => { return dec_complex(s5, s6, s7, ngrid, true); }
        200 => { return dec_runlength(s5, s6, s7, ngrid); }
        4 => { return dec_ieee(s5, s6, s7, ngrid); }
        40 => { return dec_jpeg(s5, s6, s7, ngrid); }
        41 => { return dec_png(s5, s6, s7, ngrid); }
        42 => { return dec_aec(s5, s6, s7, ngrid); }
        _ => { return Err(GribErr::UnsupportedData(tmpl)); }
    }
}

pub fn wx_nx(g: &GridDef) -> u32 {
    match g {
        GridDef::Regular(r) => { return r.nx; }
        GridDef::Lambert(r) => { return r.nx; }
        GridDef::Gaussian(r) => { return r.nx; }
    }
}

pub fn wx_ny(g: &GridDef) -> u32 {
    match g {
        GridDef::Regular(r) => { return r.ny; }
        GridDef::Lambert(r) => { return r.ny; }
        GridDef::Gaussian(r) => { return r.ny; }
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
    // V1.4 phase B1 EXTENT PRE-CHECK (the anti-abort net for DRT 5.0): the
    // bit-walk below reads exactly ndef * nbits bits, one per DEFINED point,
    // and get_bits indexes b[p >> 3] with NO bounds proof of its own. Before
    // this check a forged/truncated section 7 walked off the payload end ->
    // slice-index panic -> process abort under release panic="abort" (red
    // run: bounds_simple_ieee.rs panicked at grib2.rs:291 pre-fix). One
    // multiply + one compare here makes EVERY inner-loop read provably in
    // range (highest touched bit need_bits-1 lives in byte (need_bits-1)>>3
    // <= len-1 when need_bits <= len*8), so the hot loop stays untouched.
    // With no bitmap defined_count returns ngrid in O(1); with one it costs
    // the same O(ngrid) byte scan the interleave loop performs anyway.
    let ndef = codecs::defined_count(bm, ngrid);
    let need_bits = (ndef as u64) * (nbits as u64);
    if need_bits > (s7.len() as u64) * 8 {
        return Err(GribErr::Truncated);
    }
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
    // Section-5 LENGTH differs by template (measured on tools/wgrib2-full output
    // and read off complex_pk.c, which writes a 47-BYTE section 5 for DRT 5.2
    // vs 49 for DRT 5.3): a 5.2 payload ENDS at the group-length-width byte
    // (offset 41), i.e. it has NO Code Table 5.6 spatial-differencing byte (42)
    // and NO extra-octets byte (43) -- and correspondingly its section 7 has NO
    // seed/min-value phase. `unpk_complex.c` mirrors exactly that: `extra_octets
    // = (pack == 2) ? 0 : sec[5][48]`. So a SHORT section 5 means "template 5.2:
    // no spatial differencing, zero extra octets", NOT a truncated file.
    if s5.len() < 42 {
        return Err(GribErr::Truncated);
    }
    let no_spatial_fields = s5.len() < 44;
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
    let spatial = if no_spatial_fields { 0 } else { s5[42] };  // CT 5.6: 0 none, 1 first, 2 second
    let eo = if no_spatial_fields { 0 } else { s5[43] };       // extra octets (seeds + vmn)
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

    // V1.4 phase B1 SCALAR SANITY, all of it a function of ALREADY-CHECKED
    // bytes and executed BEFORE any Vec growth or bit-walk:
    //   * np (section-5 "number of data points") must equal the bitmap-defined
    //     count: the format requires it (the SAME cross-check dec_jpeg /
    //     dec_png / dec_aec already run), it bounds np by ngrid hence by
    //     MAX_GRID_POINTS, and without it the final interleave indexed
    //     ud[j] past the end when the stream stored FEWER values than the
    //     grid declared -> slice panic (red run: grib2.rs:1110 pre-fix).
    //   * every group covers at least one value, so ngroups <= np bounds the
    //     refs/widths/lens allocations. A forged 0xFFFFFFFF ngroups used to
    //     push three vectors toward multi-GB (OOM-class) while the group walk
    //     ran the bit cursor off the payload (red run: grib2.rs:291 pre-fix);
    //     the exact header-extent proof below additionally refuses any
    //     ngroups whose three fixed phases cannot fit section 7, which is
    //     the "ngroups vs s7.len()" sanity computed from validated scalars.
    let ndef = codecs::defined_count(bm, ngrid);
    if np != ndef {
        return Err(GribErr::BadField("complex packing defined-point count mismatch"));
    }
    if ngroups > np {
        return Err(GribErr::BadField("complex packing group count exceeds point count"));
    }

    // V1.4 phase B1 HEADER-EXTENT PRE-CHECK: extra-octets seeds, group
    // references, group widths and the RESERVED group-length slots sit at bit
    // offsets that are pure functions of the section-5 scalars checked above,
    // so the end offset of the length phase is computable BEFORE reading any
    // of it. Prove it fits in s7 once per message; the walks below then touch
    // only provably-in-range bytes. Zero-width phases legitimately consume
    // nothing and need no special case. The eo region bound also covers the
    // uint_n/int_n seed reads (they index b[off + k] unchecked -- red run:
    // grib2.rs:303 pre-fix).
    let eoz = eo as usize;
    let nseed = if spatial == 2 { 2 } else { 1 };
    if eoz > 0 && eoz * (nseed + 1) > s7.len() {
        return Err(GribErr::Truncated);
    }
    let avail_bits = (s7.len() as u64) * 8;
    let mut hb: u64 = (eoz * (nseed + 1)) as u64 * 8;
    hb += (ngroups as u64) * (nbits as u64);
    hb = ((hb + 7) / 8) * 8;                    // byte-align after refs
    hb += (ngroups as u64) * (wbits as u64);
    hb = ((hb + 7) / 8) * 8;                    // byte-align before lens
    hb += (ngroups as u64) * (lbits as u64);    // complex_pk.c reserves a slot
    hb = ((hb + 7) / 8) * 8;                    // for ALL ngroups lengths
    if hb > avail_bits {
        return Err(GribErr::Truncated);
    }

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
        // V1.4 B1: saturating, NOT wrapping -- release builds run with
        // overflow checks OFF, so a forged lens array could wrap this sum
        // right back onto np (the `total != np` gate then passed on a stream
        // conceptually holding np + k*2^64 values) and the ud push loop
        // below iterated the TRUE sum: OOM-class hang. Clean streams never
        // come within 2^63 of the limit, so saturation changes nothing for
        // them (checksum goldens byte-identical).
        let mut t: usize = 0;
        for v in &lens { t = t.saturating_add(*v); }
        t
    };
    if total != np {
        return Err(GribErr::BadField("complex packing point count mismatch"));
    }

    // V1.4 phase B1 VALUE-PHASE EXTENT PROOF: the continuous bitstream reads
    // exactly sum(lens[gi] * widths[gi]) bits starting where the reserved
    // length phase ended (bit hb computed above). Both factors are content
    // scalars already validated in range, so ONE pass over the group arrays
    // here proves EVERY get_bits in the group walk below in range; the walk
    // itself stays untouched. Saturating arithmetic can only tighten rejects,
    // never loosen a proof (an absurd product saturates high -> refused).
    let mut val_bits: u64 = 0;
    for gi in 0..ngroups {
        val_bits = val_bits.saturating_add((lens[gi] as u64) * (widths[gi] as u64));
    }
    if hb.saturating_add(val_bits) > avail_bits {
        return Err(GribErr::Truncated);
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
    // V1.4 phase B1 LEVEL-TABLE EXTENT PRE-CHECK: the table lives INSIDE
    // section 5 at payload offsets 10+2i .. 12+2i for i = 1..mvl, and mvl is
    // a forged u16 read from s5 itself. Before this check the walk ran
    // sm_i2(s5, 10 + i*2) -- an unchecked be_u16 -- for slots past a tiny
    // payload: slice panic -> process abort under panic="abort" (red run:
    // bounds_runlength.rs panicked at grib2.rs:252 pre-fix on BOTH forgeries).
    // One compare against the declared payload length closes it, and it also
    // bounds the `levels` allocation at <= (s5.len()-10)/2 entries.
    if 12 + (mvl as usize) * 2 > s5.len() {
        return Err(GribErr::Truncated);
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
    // V1.4 phase B1 EXTENT PRE-CHECK: ieee_f4 reads 4 unchecked bytes per
    // DEFINED point; a truncated or short section 7 walked off the payload
    // end -> slice panic -> abort (red run: bounds_simple_ieee.rs panicked
    // at grib2.rs:256 pre-fix). Proving ndef * 4 <= len once here makes
    // every loop read in range (ngrid is already capped at MAX_GRID_POINTS,
    // so the product cannot wrap usize).
    let ndef = codecs::defined_count(bm, ngrid);
    match ndef.checked_mul(4) {
        None => { return Err(GribErr::Truncated); }
        Some(n) => {
            if n > s7.len() {
                return Err(GribErr::Truncated);
            }
        }
    }
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
// DRT 5.40 / 5.41 / 5.42 wrappers (codec backends live in codecs::rs)
// ---------------------------------------------------------------------------

/// Shared shape of the three compressed templates: section 5 carries the same
/// reference/binary/decimal scalars as simple packing (plus, for AEC, the CCSDS
/// parameters), and the codec stream holds ONLY the packed integers of the
/// DEFINED points, which are scaled and re-interleaved with the bitmap exactly
/// like DRT 5.0 (`jpeg_pk.c`/`aec_pk.c` write `mk_bms` output; `unpk.c`
/// re-interleaves).
fn dec_jpeg(s5: &[u8], s6: Option<&[u8]>, s7: &[u8], ngrid: usize) -> Result<Vec<f32>, GribErr> {
    let sc_r = codecs::scalars_of(s5);
    match sc_r {
        Err(e) => { return Err(e); }
        Ok(sc) => {
            let bm = bitmap_of(s6);
            let ndef = codecs::defined_count(bm, ngrid);
            let npts = be_u32(s5, 0) as usize;
            if npts != ndef {
                return Err(GribErr::BadField("jpeg defined-point count mismatch"));
            }
            let ud_r = if sc.nbits == 0 {
                codecs::zero_samples(ndef)
            } else {
                codecs::jpeg_samples(s7, ndef)
            };
            match ud_r {
                Err(e) => { return Err(e); }
                Ok(ud) => { return Ok(codecs::finish_values(&sc, bm, ud, ngrid)); }
            }
        }
    }
}

fn dec_png(s5: &[u8], s6: Option<&[u8]>, s7: &[u8], ngrid: usize) -> Result<Vec<f32>, GribErr> {
    let sc_r = codecs::scalars_of(s5);
    match sc_r {
        Err(e) => { return Err(e); }
        Ok(sc) => {
            let bm = bitmap_of(s6);
            let ndef = codecs::defined_count(bm, ngrid);
            let npts = be_u32(s5, 0) as usize;
            if npts != ndef {
                return Err(GribErr::BadField("png defined-point count mismatch"));
            }
            let ud_r = if sc.nbits == 0 {
                codecs::zero_samples(ndef)
            } else {
                codecs::png_samples(s7, ndef)
            };
            match ud_r {
                Err(e) => { return Err(e); }
                Ok(ud) => { return Ok(codecs::finish_values(&sc, bm, ud, ngrid)); }
            }
        }
    }
}

fn dec_aec(s5: &[u8], s6: Option<&[u8]>, s7: &[u8], ngrid: usize) -> Result<Vec<f32>, GribErr> {
    let sc_r = codecs::scalars_of(s5);
    match sc_r {
        Err(e) => { return Err(e); }
        Ok(sc) => {
            let bm = bitmap_of(s6);
            let ndef = codecs::defined_count(bm, ngrid);
            let npts = be_u32(s5, 0) as usize;
            if npts != ndef {
                return Err(GribErr::BadField("aec defined-point count mismatch"));
            }
            let ud_r = if sc.nbits == 0 {
                codecs::zero_samples(ndef)
            } else {
                codecs::aec_samples(s5, s7, ndef)
            };
            match ud_r {
                Err(e) => { return Err(e); }
                Ok(ud) => { return Ok(codecs::finish_values(&sc, bm, ud, ngrid)); }
            }
        }
    }
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
        messages_unsupported: 0,
        skip_coordlists: 0,
        skip_grid: Vec::<(u16, u32)>::new(),
        skip_pdt: Vec::<(u16, u32)>::new(),
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
        // structural skip reason for THIS message (V1.1 phase 3): when set the
        // message is counted as unsupported and the walk continues at
        // `body_end` instead of failing the file.
        let mut skip: Option<SkipReason> = None;

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
            // V1.3 phase E4a DEGENERATE SECTION 7 TOLERANCE: a real HRRR
            // zero-window APCP message carries section 7 as exactly 5 octets
            // (length + number, ZERO data values): the producer encoded a
            // constant field by omitting every value octet, and the reference
            // decoder (tools/wgrib2-full verified on the live file) accepts
            // it. ONLY section 7 gets this floor of 5; every other section
            // keeps 6 (their parsers length-check payloads anyway, and a
            // 5-octet section 3/4/5 would mean a genuinely broken stream).
            // A TRULY truncated stream still fails loudly: `q + ln > body_end`
            // and the message-length-vs-file check are UNCHANGED.
            let floor_ln = if num == 7 { 5 } else { 6 };
            if ln < floor_ln || q + ln > body_end {
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
                        Ok(gd) => { grid = Some(gd); }
                        Err(e) => {
                            match e {
                                GribErr::UnsupportedGrid(t) => {
                                    skip = Some(SkipReason::Grid(t));
                                    break;
                                }
                                _ => { return Err(e); }
                            }
                        }
                    }
                }
                4 => {
                    let pp = parse_sec4(payload);
                    match pp {
                        Ok(pi) => { prod = Some(pi); }
                        Err(e) => {
                            match e {
                                GribErr::UnsupportedCoordLists => {
                                    skip = Some(SkipReason::CoordLists);
                                    break;
                                }
                                GribErr::UnsupportedPdt(t) => {
                                    skip = Some(SkipReason::Pdt(t));
                                    break;
                                }
                                _ => { return Err(e); }
                            }
                        }
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
        // SKIP-AND-NOTE path: an unparseable-by-design construct makes this
        // ONE message invisible to storage while the file keeps decoding. The
        // boundary comes from section 0's declared length, so a skipped message
        // cannot desynchronize the walk (proof: robust_skips.rs decodes the
        // messages on BOTH SIDES of a coord-list and a layer message).
        match skip {
            Some(rs) => {
                st.messages_total += 1;
                st.messages_unsupported += 1;
                // V1.3 phase E4a: tally BY KIND so the pipeline note can say
                // WHY (template numbers included), not just how many.
                match rs {
                    SkipReason::CoordLists => { st.skip_coordlists += 1; }
                    SkipReason::Grid(t) => { bump_kind(&mut st.skip_grid, t); }
                    SkipReason::Pdt(t) => { bump_kind(&mut st.skip_pdt, t); }
                }
                pos = body_end;
                if eof_flag {
                    break;
                }
                continue;
            }
            None => {}
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
        GridDef::Gaussian(r) => { return r.winds_grid; }
    }
}

// ---------------------------------------------------------------------------
// startup grid probe (spec §4.4 wind-pair preflight)
// ---------------------------------------------------------------------------

/// Result of the cheap startup probe: the FIRST grid definition in a file, how
/// many grids carried which component flag, and -- decisively -- what the FIRST
/// message that actually IS a wind component (section 4 category/number 2/2 or
/// 2/3) declared for ITS grid.
#[derive(Clone, Debug)]
pub struct GridProbe {
    pub grid: GridDef,        // section 3 of the first message that has one
    pub flag33: u8,           // raw Code Table 3.3 byte behind that grid
    pub tmpl: u16,            // section 3 template number (0 or 30)
    pub messages: u32,        // messages walked
    pub winds_true: u32,      // grids with flag bit 0x08 set
    pub winds_false: u32,     // grids without it
    pub wind_msg: u8,         // 0 = no wind message in the file, 1 = wind
    //   message on an EARTH-relative grid, 2 = wind message on a GRID-relative
    //   grid (this is the answer the spec §4.4 rule needs)
    pub wind_flag33: u8,      // flag byte of that wind message's grid
}

/// Cheap read-only probe of ONE file: walk the message framing and interpret
/// ONLY sections 1 and 3. Sections 4/5/6/7 are stepped over by their declared
/// lengths and NEVER unpacked, so this costs microseconds even on a full-size
/// GFS file. Used by the spec §4.4 startup wind-pair rule to learn whether a
/// model's grid declares grid-relative vector components BEFORE any data is
/// ingested. Stops after the first grid-bearing message unless later messages
/// disagree about the flag (then the counts expose the mix).
pub fn probe_first_grid(path: String) -> Result<GridProbe, GribErr> {
    let f = std::fs::File::open(path);
    match f {
        Err(_) => { return Err(GribErr::Io); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return Err(GribErr::Io); }
                Ok(region) => { return probe_stream(&region[0..region.len()]); }
            }
        }
    }
}

fn probe_stream(bytes: &[u8]) -> Result<GridProbe, GribErr> {
    let n = bytes.len();
    let mut pos = 0;
    let mut first: Option<GridProbe> = None;
    let mut walked: u32 = 0;
    let mut t_true: u32 = 0;
    let mut t_false: u32 = 0;
    let mut wind_msg: u8 = 0;
    let mut wind_flag: u8 = 0;
    while pos < n {
        if n - pos < 16 {
            break; // trailing junk: whatever was seen so far stands
        }
        if !(bytes[pos] == 0x47 && bytes[pos + 1] == 0x52 &&
             bytes[pos + 2] == 0x49 && bytes[pos + 3] == 0x42) {
            return Err(GribErr::BadMagic);
        }
        if bytes[pos + 7] != 2 {
            return Err(GribErr::BadEdition);
        }
        let msglen = be_u64(bytes, pos + 8);
        if msglen < 16 || (pos as u64) + msglen > (n as u64) {
            return Err(GribErr::Truncated);
        }
        let body_end = pos + (msglen as usize);
        // per-message state: the grid this message declares, plus whether the
        // message IS a wind component (section 4 category/number 2/2 or 2/3).
        let mut msg_grid: Option<GridDef> = None;
        let mut msg_flag: u8 = 0;
        let mut msg_wg = false;
        let mut msg_is_wind = false;
        let mut q = pos + 16;
        while q < body_end {
            if q + 4 <= body_end && bytes[q] == 0x37 && bytes[q + 1] == 0x37 &&
               bytes[q + 2] == 0x37 && bytes[q + 3] == 0x37 {
                break;
            }
            if q + 5 > body_end {
                return Err(GribErr::Truncated);
            }
            let ln = be_u32(bytes, q) as usize;
            let num = bytes[q + 4];
            // V1.3 phase E4a DEGENERATE SECTION 7 TOLERANCE: a real HRRR
            // zero-window APCP message carries section 7 as exactly 5 octets
            // (length + number, ZERO data values): the producer encoded a
            // constant field by omitting every value octet, and the reference
            // decoder (tools/wgrib2-full verified on the live file) accepts
            // it. ONLY section 7 gets this floor of 5; every other section
            // keeps 6 (their parsers length-check payloads anyway, and a
            // 5-octet section 3/4/5 would mean a genuinely broken stream).
            // A TRULY truncated stream still fails loudly: `q + ln > body_end`
            // and the message-length-vs-file check are UNCHANGED.
            let floor_ln = if num == 7 { 5 } else { 6 };
            if ln < floor_ln || q + ln > body_end {
                return Err(GribErr::Truncated);
            }
            if num == 3 {
                let payload = &bytes[q + 5..q + ln];
                let pg = parse_sec3(payload);
                match pg {
                    Ok(gd) => {
                        let (tm, fb) = flag33_byte(payload);
                        msg_flag = fb;
                        msg_wg = grid_winds(&gd);
                        msg_grid = Some(gd.clone());
                        if msg_wg { t_true += 1 } else { t_false += 1 }
                        match first {
                            None => {
                                first = Some(GridProbe {
                                    grid: gd,
                                    flag33: fb,
                                    tmpl: tm,
                                    messages: 1,
                                    winds_true: t_true,
                                    winds_false: t_false,
                                    wind_msg: 0,
                                    wind_flag33: 0,
                                });
                            }
                            Some(_) => {}
                        }
                    }
                    Err(e) => {
                        match e {
                            // an unsupported grid template in a later message
                            // never invalidates what was already learned
                            GribErr::UnsupportedGrid(_) => {}
                            _ => { return Err(e); }
                        }
                    }
                }
            }
            if num == 4 {
                let payload = &bytes[q + 5..q + ln];
                let pp = parse_sec4(payload);
                match pp {
                    Ok(pi) => {
                        msg_is_wind = pi.category == 2 &&
                                      (pi.number == 2 || pi.number == 3);
                    }
                    Err(_e) => {} // coordinate lists etc: unclassifiable message
                }
            }
            q += ln;
        }
        match msg_grid {
            Some(_) => {
                if msg_is_wind && wind_msg == 0 {
                    wind_msg = if msg_wg { 2 } else { 1 };
                    wind_flag = msg_flag;
                }
            }
            None => {}
        }
        walked += 1;
        pos = body_end;
    }
    match first {
        None => { return Err(GribErr::BadSectionOrder); } // no section 3 at all
        Some(pp) => {
            return Ok(GridProbe {
                grid: pp.grid,
                flag33: pp.flag33,
                tmpl: pp.tmpl,
                messages: walked,
                winds_true: t_true,
                winds_false: t_false,
                wind_msg: wind_msg,
                wind_flag33: wind_flag,
            });
        }
    }
}


// ---------------------------------------------------------------------------
// cheap SECTION-0/SECTION-1 header peek (V1.2 phase C retention-aware skip)
// ---------------------------------------------------------------------------

/// What a header peek learned about ONE FILE: the OLDEST and NEWEST SECTION-1
/// REFERENCE TIMES it declares, plus the number of messages walked. A file can
/// carry SEVERAL runs (`tests/fixtures/frac_small.grib2` has two), so BOTH ends
/// of that range matter: the retention-skip rule asks whether the NEWEST run in
/// the file is still older than everything the registry keeps.
#[derive(Clone, Debug)]
pub struct RefPeek {
    pub min_epoch: i64, // oldest reference time declared
    pub max_epoch: i64, // newest reference time declared
    pub messages: u32, // messages whose section 1 was read
}

/// Cheap HEADER PEEK over one file: walks ONLY the message/section FRAMING and
/// interprets ONLY SECTION-1 payloads (reference time + end-of-file flag).
/// Sections 2..7 are stepped over by their DECLARED byte lengths — no grid,
/// product or data-section parsing, no unpacking — so the cost is a handful of
/// bytes per section (the payload pages are never faulted in), which is why this
/// may gate the expensive decode. Big-endian framing, edition byte must be 2,
/// both terminators accepted (bare ASCII `7777` and the LEN-prefixed end
/// section), exactly like the full decoder and the wind-pair probe.
/// ANY structural problem returns Err: the caller MUST fall through to the
/// normal decode path rather than skip, because skipping on an uncertain
/// reference time could DROP data.
pub fn peek_ref_times(path: String) -> Result<RefPeek, GribErr> {
    let f = std::fs::File::open(path);
    match f {
        Err(_) => { return Err(GribErr::Io); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return Err(GribErr::Io); }
                Ok(region) => { return peek_stream(&region[0 .. region.len()]); }
            }
        }
    }
}

fn peek_stream(bytes: &[u8]) -> Result<RefPeek, GribErr> {
    let n = bytes.len();
    let mut pos = 0;
    let mut mn: i64 = 0;
    let mut mx: i64 = 0;
    let mut seen: u32 = 0;
    while pos < n {
        if n - pos < 16 {
            break; // trailing junk: what was learned stands
        }
        if !(bytes[pos] == 0x47 && bytes[pos + 1] == 0x52 &&
             bytes[pos + 2] == 0x49 && bytes[pos + 3] == 0x42) {
            return Err(GribErr::BadMagic);
        }
        if bytes[pos + 7] != 2 {
            return Err(GribErr::BadEdition);
        }
        let msglen = be_u64(bytes, pos + 8);
        if msglen < 16 || (pos as u64) + msglen > (n as u64) {
            return Err(GribErr::Truncated);
        }
        let body_end = pos + (msglen as usize);
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
            // V1.3 phase E4a DEGENERATE SECTION 7 TOLERANCE: a real HRRR
            // zero-window APCP message carries section 7 as exactly 5 octets
            // (length + number, ZERO data values): the producer encoded a
            // constant field by omitting every value octet, and the reference
            // decoder (tools/wgrib2-full verified on the live file) accepts
            // it. ONLY section 7 gets this floor of 5; every other section
            // keeps 6 (their parsers length-check payloads anyway, and a
            // 5-octet section 3/4/5 would mean a genuinely broken stream).
            // A TRULY truncated stream still fails loudly: `q + ln > body_end`
            // and the message-length-vs-file check are UNCHANGED.
            let floor_ln = if num == 7 { 5 } else { 6 };
            if ln < floor_ln || q + ln > body_end {
                return Err(GribErr::Truncated);
            }
            if num == 1 {
                let payload = &bytes[q + 5 .. q + ln];
                let pr = parse_sec1(payload);
                match pr {
                    Err(e) => { return Err(e); }
                    Ok(rt_eof) => {
                        if seen == 0 {
                            mn = rt_eof.0.epoch_secs;
                            mx = rt_eof.0.epoch_secs;
                        } else {
                            if rt_eof.0.epoch_secs < mn { mn = rt_eof.0.epoch_secs; }
                            if rt_eof.0.epoch_secs > mx { mx = rt_eof.0.epoch_secs; }
                        }
                        seen += 1;
                        eof_flag = rt_eof.1;
                    }
                }
            }
            q += ln;
        }
        pos = body_end;
        if eof_flag {
            break;
        }
    }
    if seen == 0 {
        return Err(GribErr::BadSectionOrder);
    }
    return Ok(RefPeek { min_epoch: mn, max_epoch: mx, messages: seen });
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
/// `GridLambert.rot_angle_deg`). THE V AXIS IS THE PROJECTION-PLANE +y AXIS,
/// NOT THE STORAGE ROW-TRAVERSAL DIRECTION: template 3.30 defines the grid +j
/// axis by Dy in PLANE coordinates, and the scanning-mode byte only chooses
/// the ORDER rows are enumerated in (bit 0x40 set => rows stored S->N, i.e.
/// storage j maps to plane +y; clear => N->S, storage j maps to plane -y with
/// the FIRST stored row still anchored at La1). A stored vector component is
/// always read along the plane axes, so the earth-frame decomposition is
/// SCAN-INDEPENDENT -- exactly what wgrib2/IPOLATES do (`ipolatev.F90` +
/// `gdswzd_lambert_conf_mod.f90`: ue = CROT*ug + SROT*vg with the documented
/// grid<-earth convention UGRID = CROT*UEARTH - SROT*VEARTH, VGRID =
/// SROT*UEARTH + CROT*VEARTH, CROT/SROT = cos/sin(AN*(lon-LoV))). In terms of
/// beta (= 90 deg + AN*(lon-LoV), hence sin beta = CROT, cos beta = -SROT):
///     u_east  = u_g * sin(beta) - v_g * cos(beta)
///     v_north = u_g * cos(beta) + v_g * sin(beta)
/// This matrix M2 = [[sin b, -cos b], [cos b, sin b]] is a PROPER ROTATION
/// (det +1, the rotation by the convergence angle beta - 90 deg): magnitudes
/// are preserved exactly up to float rounding, and it degenerates to IDENTITY
/// on the central meridian (beta = 90 deg), where no rotation is needed.
///
/// HISTORY (V1.4 phase X defect fix, docs/DECISIONS.md section 21): this step
/// used to compute [[sin b, cos b], [cos b, -sin b]] after hard-coding
/// "+j = south" for every Lambert grid. That matrix has det -1: it is a
/// REFLECTION (mirror about the +i axis), not a rotation -- the old docstring
/// called it an orthonormal rotation, which is wrong -- and it never reduced
/// to identity at LoV. On S->N-scanned grids (every REAL HRRR file,
/// scan_mode 0x40) it additionally mirrored V-heavy vectors outright. Pinned
/// against the wgrib2 v3.1.3+IPOLATES golden tests/fixtures/hrrr_ipolates_
/// earth.grib2 (see tests/rotation_vs_ipolates.rs). Regular lat/lon grids are
/// returned unchanged (wgrib2 treats their winds as earth-relative; documented
/// decision).
pub fn rotate_winds_to_earth(g: &GridDef, u_in: &[f32], v_in: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let mut ue = Vec::<f32>::new();
    let mut vn = Vec::<f32>::new();
    match g {
        GridDef::Regular(_) => {
            for x in u_in { ue.push(*x); }
            for x in v_in { vn.push(*x); }
            return (ue, vn);
        }
        GridDef::Gaussian(_) => {
            // lat/lon geometry: U/V arrive earth-relative on NCEP template
            // 3.40 (wgrib2 hardcodes winds N/S for this grid: mk_kgds.c
            // kgds[5]=128; the live flag byte agrees, bit 0x08 clear), so no
            // rotation applies — same treatment as regular lat/lon grids.
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
                ue.push(ug * s as f32 - vg * c as f32);
                vn.push(ug * c as f32 + vg * s as f32);
            }
            return (ue, vn);
        }
    }
}

/// Legacy (pre-V1.4-phase-X) reflection matrix, kept ONLY as the reference
/// implementation the external golden test uses to reproduce the historical
/// mismatch: ue = ug*sin(b) + vg*cos(b); vn = ug*cos(b) - vg*sin(b). Det -1.
pub fn rotate_winds_reflection_legacy(g: &GridDef, u_in: &[f32], v_in: &[f32])
    -> (Vec<f32>, Vec<f32>) {
    let mut ue = Vec::<f32>::new();
    let mut vn = Vec::<f32>::new();
    match g {
        GridDef::Lambert(gl) => {
            let nxi = (gl.nx) as usize;
            for k in 0..u_in.len() {
                let beta = gl.rot_angle_deg((k % nxi) as f64, (k / nxi) as f64) * D2R;
                let s = beta.sin();
                let c = beta.cos();
                ue.push(u_in[k] * s as f32 + v_in[k] * c as f32);
                vn.push(u_in[k] * c as f32 - v_in[k] * s as f32);
            }
        }
        _ => {
            for x in u_in { ue.push(*x); }
            for x in v_in { vn.push(*x); }
        }
    }
    return (ue, vn);
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
