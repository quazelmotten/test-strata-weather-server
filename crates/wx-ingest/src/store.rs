//! Storage writing side (spec §4.5): `<data_dir>/<model>/<run YYYYMMDDHH>/`
//! with grid.bin (serialized GridDef + projection constants), manifest.json,
//! and `<param>/<level_type_dir>/f<FFF>.bin` fields written through the wx-core
//! atomic writer. Visibility is per forecast hour: each f<FFF>.bin is one
//! atomic rename, published to the index right after its write.
//!
//! ## grid.bin format (versioned, custom — "define your own serialization")
//!
//! ```text
//! magic 8B "WXPTGRID1" | version u32 LE = 2 | kind u8 (0 regular, 1 lambert)
//! regular:  nx u32 ny u32 | lon1 lat1 dlon dlat f64 | scan u8 | winds u8
//! lambert:  nx u32 ny u32 | la1 lo1 lov lad latin1 latin2 dx_m dy_m f64 x8
//!           | proj_flag u8 | scan u8 | earth_shape u8 | winds u8
//!           | ua_major_m f64 | ua_minor_m f64 | inline_axes u8   (v2 addition:
//!           the Code Table 3.2 codes 1/3/7 declare the earth axes INLINE in
//!           section 3; without these bytes a warm-boot rebuild would silently
//!           fall back to the CT table value and project/rotate on the wrong
//!           spheroid)
//! ```
//! Precomputed projection constants are NOT stored: the reader rebuilds them
//! via GridLambert::new (+ set_user_axes when inline_axes is set), keeping one
//! source of truth for both the projection and the wind-rotation angle.

use wx_core::grid::{GridDef, GridRegular, GridLambert};
use wx_core::storage::{write_bin_atomic, open_field, BinHeader, fsync_path};
use std::io::Write;

pub const GRID_MAGIC: [u8; 8] = [0x57, 0x58, 0x50, 0x54, 0x47, 0x52, 0x49, 0x44]; // WXPTGRID1
pub const GRID_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// LE primitives (local copies; wx-core only exposes f32/u32 helpers)
// ---------------------------------------------------------------------------

pub fn push_le_u32(out: &mut Vec<u8>, v: u32) {
    out.push((v & 0xFF) as u8);
    out.push(((v  >> 8) & 0xFF) as u8);
    out.push(((v  >> 16) & 0xFF) as u8);
    out.push(((v  >> 24) & 0xFF) as u8);
}

pub fn push_le_f64(out: &mut Vec<u8>, v: f64) {
    let b = v.to_bits();
    for k in 0 .. 8 {
        out.push(((b  >> (k * 8)) & 0xFF) as u8);
    }
}

pub fn le_u32_at(b: &[u8], off: usize) -> u32 {
    (b[off] as u32) | ((b[off + 1] as u32) << 8) |
        ((b[off + 2] as u32) << 16) | ((b[off + 3] as u32) << 24)
}

pub fn le_f64_at(b: &[u8], off: usize) -> f64 {
    let mut v: u64 = 0;
    for k in 0 .. 8 {
        v = v | (((b[off + k] as u64)) << (k * 8));
    }
    return f64::from_bits(v);
}

// ---------------------------------------------------------------------------
// grid.bin encode / decode
// ---------------------------------------------------------------------------

pub fn encode_grid(grid: &GridDef) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::<u8>::new();
    for c in GRID_MAGIC {
        out.push(c);
    }
    push_le_u32(&mut out, GRID_VERSION);
    match grid {
        GridDef::Regular(g) => {
            out.push(0);
            push_le_u32(&mut out, g.nx);
            push_le_u32(&mut out, g.ny);
            push_le_f64(&mut out, g.lon1);
            push_le_f64(&mut out, g.lat1);
            push_le_f64(&mut out, g.dlon);
            push_le_f64(&mut out, g.dlat);
            out.push(g.scan_mode);
            out.push(if g.winds_grid { 1 } else { 0 });
        }
        GridDef::Lambert(g) => {
            out.push(1);
            push_le_u32(&mut out, g.nx);
            push_le_u32(&mut out, g.ny);
            push_le_f64(&mut out, g.la1);
            push_le_f64(&mut out, g.lo1);
            push_le_f64(&mut out, g.lov);
            push_le_f64(&mut out, g.lad);
            push_le_f64(&mut out, g.latin1);
            push_le_f64(&mut out, g.latin2);
            push_le_f64(&mut out, g.dx_m);
            push_le_f64(&mut out, g.dy_m);
            out.push(g.proj_flag);
            out.push(g.scan_mode);
            out.push(g.earth_shape);
            out.push(if g.winds_grid { 1 } else { 0 });
            push_le_f64(&mut out, g.ua_major_m);
            push_le_f64(&mut out, g.ua_minor_m);
            out.push(if g.inline_axes { 1 } else { 0 });
        }
    }
    return out;
}

pub fn decode_grid(b: &[u8]) -> Result<GridDef, String> {
    if b.len() < 12 {
        return Err("truncated grid.bin".to_string());
    }
    for k in 0 .. 8 {
        if b[k] != GRID_MAGIC[k] {
            return Err("bad grid magic".to_string());
        }
    }
    if le_u32_at(b, 8) != GRID_VERSION {
        return Err("bad grid version".to_string());
    }
    let kind = b[12];
    match kind {
        0 => {
            if b.len() < 12 + 1 + 8 + 32 + 2 {
                return Err("truncated regular grid".to_string());
            }
            let nx = le_u32_at(b, 13);
            let ny = le_u32_at(b, 17);
            let lon1 = le_f64_at(b, 21);
            let lat1 = le_f64_at(b, 29);
            let dlon = le_f64_at(b, 37);
            let dlat = le_f64_at(b, 45);
            let scan = b[53];
            let winds = b[54];
            let g = GridRegular {
                nx: nx,
                ny: ny,
                lon1: lon1,
                lat1: lat1,
                dlon: dlon,
                dlat: dlat,
                scan_mode: scan,
                winds_grid: winds != 0,
            };
            return Ok(GridDef::Regular(g));
        }
        1 => {
            // layout: magic(8)+ver(4)+kind(1)+nx/ny(8)+8 f64 params(64)+4 flag
            // bytes+16 axes bytes+1 inline flag = 106 total. NOTE: earlier guard
            // said 72 for the f64 block (9 slots); encode_grid writes 8 (la1,
            // lo1,lov,lad,latin1,latin2,dx,dy) -> warm-boot rebuild silently
            // dropped every LCC run.
            if b.len() < 12 + 1 + 8 + 64 + 4 + 16 + 1 {
                return Err("truncated lambert grid".to_string());
            }
            let nx = le_u32_at(b, 13);
            let ny = le_u32_at(b, 17);
            let la1 = le_f64_at(b, 21);
            let lo1 = le_f64_at(b, 29);
            let lov = le_f64_at(b, 37);
            let lad = le_f64_at(b, 45);
            let latin1 = le_f64_at(b, 53);
            let latin2 = le_f64_at(b, 61);
            let dx_m = le_f64_at(b, 69);
            let dy_m = le_f64_at(b, 77);
            let proj_flag = b[85];
            let scan = b[86];
            let earth_shape = b[87];
            let winds = b[88];
            let ua_major = le_f64_at(b, 89);
            let ua_minor = le_f64_at(b, 97);
            let inline_axes = b[105];
            let g = GridLambert::new(nx, ny, la1, lo1, lov, lad, latin1, latin2,
                                     dx_m, dy_m, proj_flag, scan, earth_shape, winds != 0);
            let mut gg = g;
            if inline_axes != 0 {
                gg.set_user_axes(ua_major, ua_minor);
            }
            return Ok(GridDef::Lambert(gg));
        }
        _ => { return Err("unknown grid kind".to_string()); }
    }
}

/// Write grid.bin atomically (once per run dir).
pub fn write_grid_bin(run_dir: &str, grid: &GridDef) -> Result<(), String> {
    let path = format!("{}/grid.bin", run_dir);
    let bytes = encode_grid(grid);
    // reuse the atomic pattern: temp+fsync+rename via a raw byte writer
    return write_bytes_atomic_pub(&path, &bytes);
}

// ---------------------------------------------------------------------------
// field bins: merge existing levels with new planes, canonical order, atomic
// ---------------------------------------------------------------------------

/// Merge one field file: existing planes (from its current .bin, if any) plus
/// `new` (level value -> plane), new values winning on collision. Returns the
/// merged (levels canonical-sorted, planes parallel) ready to write.
/// Canonical ordering predicate: isobaric descending, others ascending.
fn level_better(code: u8, a: f64, b: f64) -> bool {
    if code == 100 {
        return a > b;
    }
    return a < b;
}

pub fn merge_field(path: String, code: u8, new: Vec<(f64, Vec<f32>)>)
    -> Result<Vec<(f64, Vec<f32>)>, String> {
    let mut levels: Vec<f64> = Vec::<f64>::new();
    let mut planes: Vec<Vec<f32 >> = Vec::<Vec<f32>>::new();
    // seed from existing file when present
    match open_field(path) {
        Err(_) => {}
        Ok(mf) => {
            for li in 0 .. mf.header.levels.len() {
                let lv = mf.header.levels[li] as f64;
                levels.push(lv);
                let mut pl: Vec<f32> = Vec::<f32>::new();
                for j in 0 .. mf.header.ny as usize {
                    for i in 0 .. mf.header.nx as usize {
                        pl.push(mf.value(li, j, i));
                    }
                }
                planes.push(pl);
            }
        }
    }
    for ni in 0 .. new.len() {
        let v = new[ni].0;
        let mut found = false;
        for k in 0 .. levels.len() {
            if levels[k] == v {
                planes[k] = new[ni].1.clone();
                found = true;
                break;
            }
        }
        if !found {
            levels.push(v);
            planes.push(new[ni].1.clone());
        }
    }
    // canonical order (stable permutation of levels+planes together)
    let mut idx: Vec<usize> = Vec::<usize>::new();
    for k in 0 .. levels.len() {
        idx.push(k);
    }
    for i in 0 .. idx.len() {
        let mut best = i;
        for j in i + 1 .. idx.len() {
            let aj = levels[idx[j]];
            let ab = levels[idx[best]];
            if level_better(code, aj, ab) {
                best = j;
            }
        }
        let t = idx[i];
        idx[i] = idx[best];
        idx[best] = t;
    }
    let mut out: Vec<(f64, Vec<f32>)> = Vec::<(f64, Vec<f32>)>::new();
    for k in 0 .. idx.len() {
        out.push((levels[idx[k]], planes[idx[k]].clone()));
    }
    return Ok(out);
}

/// Serialize merged planes and write atomically through wx-core's writer.
pub fn write_field(path: String, _code: u8, nx: u32, ny: u32, scan_ns: bool,
                   winds_grid: bool, merged: Vec<(f64, Vec<f32>)>) -> Result<(), String> {
    let mut hdr = BinHeader {
        nx: nx,
        ny: ny,
        levels: Vec::<f32>::new(),
        scan_ns: scan_ns,
        winds_grid: winds_grid,
    };
    let mut refs: Vec<&[f32]> = Vec::<&[f32]>::new();
    for m in &merged {
        hdr.levels.push(m.0 as f32);
    }
    for m in &merged {
        refs.push(&m.1);
    }
    let w = write_bin_atomic(path, &hdr, &refs);
    match w {
        Err(e) => { return Err(e.to_string()); }
        Ok(_) => { return Ok(()); }
    }
}

/// Public alias of the atomic byte writer used for grid.bin/manifest.json:
/// temp file in the SAME directory, write, flush, REAL fsync of the temp inode
/// (libc via `wx_core::storage::fsync_path`; this std has no fsync API), atomic
/// rename, then REAL fsync of the parent directory so the rename survives a
/// crash. Durability failures are reported, not swallowed.
pub fn write_bytes_atomic_pub(path: &str, bytes: &[u8]) -> Result<(), String> {
    let tmp = format!("{0}.tmp", path);
    let f = std::fs::File::create(&tmp);
    match f {
        Err(_) => { return Err("create failed".to_string()); }
        Ok(mut h) => {
            let w = h.write_all(bytes);
            if w.is_err() {
                return Err("write_all failed".to_string());
            }
            let fl = h.flush();
            if fl.is_err() {
                return Err("flush failed".to_string());
            }
        }
    }
    match fsync_path(&tmp) {
        Err(e) => { return Err(format!("file fsync: {0}", e)); }
        Ok(_) => {}
    }
    let r = std::fs::rename(&tmp, &path);
    if r.is_err() {
        return Err("rename failed".to_string());
    }
    match fsync_path(&parent_dir_pub(&path)) {
        Err(e) => { return Err(format!("dir fsync: {0}", e)); }
        Ok(_) => {}
    }
    return Ok(());
}

/// Parent-directory portion of a path (split at the last '/').
fn parent_dir_pub(path: &str) -> String {
    let mut last: Option<usize> = None;
    for k in 0 .. path.len() {
        if path[k .. k + 1] == *"/" {
            last = Some(k);
        }
    }
    match last {
        None => { return ".".to_string(); }
        Some(0) => { return "/".to_string(); }
        Some(k) => { return path[0 .. k].to_string(); }
    }
}

// ---------------------------------------------------------------------------
// manifest.json (human-facing inventory; index is authoritative)
// ---------------------------------------------------------------------------

/// One field entry for the manifest. `fhs` are decimal HOURS (human-facing);
/// `stat_ranges_json` is the pre-rendered per-fh statistical range array
/// (`[]` when nothing is statistically processed) — spec §4.2 requires the
/// PDT 4.8 time RANGE to be stored, and this is where it lands on disk.
pub fn manifest_field_json(param: &str, level_dir: &str, units: &str,
                           stat_processed: bool, stat_kind: String,
                           levels: Vec<f64>, fhs: Vec<f64>,
                           stat_ranges_json: String) -> String {
    let mut lv = "".to_string();
    for k in 0 .. levels.len() {
        if k > 0 {
            lv = lv + ",";
        }
        lv = format!("{0}{1}", lv, fmt_level(levels[k]));
    }
    let mut fh = "".to_string();
    for k in 0 .. fhs.len() {
        if k > 0 {
            fh = fh + ",";
        }
        fh = format!("{0}{1}", fh, fmt_hours(fhs[k]));
    }
    let mut st = "false";
    if stat_processed {
        st = "true";
    }
    return format!("{{\"param\":\"{0}\",\"level_type\":\"{1}\",\"units\":\"{2}\",\"stat_processed\":{3},\"stat_kind\":\"{4}\",\"levels\":[{5}],\"forecast_hours\":[{6}],\"stat_ranges\":{7}}}",
        param, level_dir, units, st, stat_kind, lv, fh, stat_ranges_json);
}

/// Decimal-hour rendering for manifest/inventory lists: integral values print
/// without a fraction (0, 6), fractional ones with trimmed decimals (0.5,
/// 1.25). Same shape as wx-core tables.fmt_num but local to avoid a
/// dependency edge from the storage writer.
fn fmt_hours(v: f64) -> String {
    let ip = v.trunc();
    if ip == v {
        return format!("{}", ip);
    }
    let mut digits: String = "".to_string();
    let mut fr = v - ip;
    let mut k = 0;
    while k < 6 {
        fr = fr * 10.0;
        let d = fr.trunc();
        fr = fr - d;
        digits = format!("{0}{1}", digits, d);
        k += 1;
    }
    while digits.len() > 0 && digits[digits.len() - 1 ..] == *"0" {
        digits = digits[0 .. digits.len() - 1].to_string();
    }
    return format!("{0}.{1}", ip, digits);
}

// ---------------------------------------------------------------------------
// ISO-8601 UTC <-> epoch seconds (manifest stat ranges, spec §4.2/§7)
// ---------------------------------------------------------------------------

/// `YYYY-MM-DDTHH:MM:SSZ` for an epoch-seconds instant (Howard Hinnant civil
/// date from days, Gregorian proleptic, non-negative epochs only). Returns
/// the empty string for negative input (pre-1970 GRIB products do not exist
/// here; callers treat "" as "no range").
pub fn iso_utc(epoch: i64) -> String {
    if epoch < 0 {
        return "".to_string();
    }
    let days = epoch / 86400;
    let rem = epoch - days * 86400;
    let hh = rem / 3600;
    let mm = (rem - hh * 3600) / 60;
    let ss = rem - hh * 3600 - mm * 60;
    // Shift so era boundaries line up with 0000-03-01 (the Hinnant convention
    // that makes the 400-year cycle exactly 146097 days).
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097; // [0, 146096]
    // Year-of-era: the largest yoe in [0, 399] whose era-relative day count
    // (365*yoe + yoe/4 - yoe/100) does not exceed `doe`. `doe / 365` is always
    // >= the true value (365 <= 365.2425), so one correction loop suffices.
    // The upper bound matters: at doe = 146096 (era end, i.e. a leap year's
    // Feb 29) the formula for yoe = 400 matches exactly, so the search must
    // stop at 399 instead of stepping into the next era. This runs at INGEST
    // time only (manifest writing), so the loops cost nothing measurable.
    let mut yoe = doe / 365; // never below the true value (365 <= 365.2425)
    if yoe > 399 {
        yoe = 399; // year-of-era is bounded at 399 by construction
    }
    while yoe < 399 && 365 * (yoe + 1) + (yoe + 1) / 4 - (yoe + 1) / 100 <= doe {
        yoe += 1;
    }
    while yoe > 0 && 365 * yoe + yoe / 4 - yoe / 100 > doe {
        yoe -= 1;
    }
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let yy = y + (if m <= 2 { 1 } else { 0 });
    return format!("{0}-{1}-{2}T{3}:{4}:{5}Z", pad4(yy), pad2(m), pad2(d), pad2(hh), pad2(mm), pad2(ss));
}

/// Strict parse of `YYYY-MM-DDTHH:MM:SSZ` back to epoch seconds (inverse of
/// [`iso_utc`); None on any deviation from the canonical form.
pub fn parse_iso_utc(t: &str) -> Option<i64> {
    if t.len() != 20 {
        return None;
    }
    if t[4 .. 5] != *"-" || t[7 .. 8] != *"-" || t[10 .. 11] != *"T" ||
       t[13 .. 14] != *":" || t[16 .. 17] != *":" || t[19 .. 20] != *"Z" {
        return None;
    }
    let yv = num_at(&t, 0, 4);
    let mo = num_at(&t, 5, 2);
    let da = num_at(&t, 8, 2);
    let hh = num_at(&t, 11, 2);
    let mi = num_at(&t, 14, 2);
    let ss = num_at(&t, 17, 2);
    match yv {
        None => { return None; }
        Some(y) => {
            match mo {
                None => { return None; }
                Some(mo2) => {
                    match da {
                        None => { return None; }
                        Some(da2) => {
                            match hh {
                                None => { return None; }
                                Some(hh2) => {
                                    match mi {
                                        None => { return None; }
                                        Some(mi2) => {
                                            match ss {
                                                None => { return None; }
                                                Some(ss2) => {
                                                    if mo2 < 1 || mo2 > 12 || da2 < 1 || da2 > 31 || hh2 > 23 || mi2 > 59 || ss2 > 59 {
                                                        return None;
                                                    }
                                                    return Some(days_from_civil(y, mo2, da2) * 86400 + hh2 * 3600 + mi2 * 60 + ss2);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn num_at(t: &str, off: usize, len: usize) -> Option<i64> {
    let ub: Vec<u8> = t.as_bytes().to_vec();
    if off + len > ub.len() {
        return None;
    }
    let mut v: i64 = 0;
    for k in 0 .. len {
        let b = ub[off + k];
        if b < 48 || b > 57 {
            return None;
        }
        v = v * 10 + ((b as i64) - 48);
    }
    return Some(v);
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let yy = if m <= 2 { y - 1 } else { y };
    let era = (if yy < 0 { (yy / 400) - 1 } else { yy / 400 }) as i64;
    let yoe = yy - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    return era * 146097 + doe - 719468;
}

fn pad4(v: i64) -> String {
    let s = format!("{}", v);
    if s.len() >= 4 {
        return s;
    }
    let mut zeros = "".to_string();
    for _ in 0 .. (4 - s.len()) {
        zeros = zeros + "0";
    }
    return format!("{0}{1}", zeros, s);
}

fn pad2(v: i64) -> String {
    if v < 10 {
        return format!("0{0}", v);
    }
    return format!("{0}", v);
}

fn fmt_level(v: f64) -> String {
    if v == v.trunc() {
        return format!("{}", v.trunc());
    }
    return format!("{}", v);
}
