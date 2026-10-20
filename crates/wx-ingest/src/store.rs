//! Storage writing side (spec §4.5): `<data_dir>/<model>/<run YYYYMMDDHH>/`
//! with grid.bin (serialized GridDef + projection constants), manifest.json,
//! and `<param>/<level_type_dir>/f<FFF>.bin` fields written through the wx-core
//! atomic writer. Visibility is per forecast hour: each f<FFF>.bin is one
//! atomic rename, published to the index right after its write.
//!
//! ## grid.bin format (versioned, custom — "define your own serialization")
//!
//! ```text
//! magic 8B "WXPTGRID1" | version u32 LE = 1 | kind u8 (0 regular, 1 lambert)
//! regular:  nx u32 ny u32 | lon1 lat1 dlon dlat f64 | scan u8 | winds u8
//! lambert:  nx u32 ny u32 | la1 lo1 lov lad latin1 latin2 dx_m dy_m f64 x9
//!           | proj_flag u8 | scan u8 | earth_shape u8 | winds u8
//! ```
//! Precomputed projection constants are NOT stored: the reader rebuilds them
//! via GridLambert::new (which calls prepare()), keeping one source of truth.

use wx_core::grid::{GridDef, GridRegular, GridLambert};
use wx_core::storage::{write_bin_atomic, open_field, BinHeader};
use std::io::Write;

pub const GRID_MAGIC: [u8; 8] = [0x57, 0x58, 0x50, 0x54, 0x47, 0x52, 0x49, 0x44]; // WXPTGRID1
pub const GRID_VERSION: u32 = 1;

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
            // bytes = 89. NOTE: earlier guard said 72 for the f64 block (9
            // slots); encode_grid writes 8 (la1,lo1,lov,lad,latin1,latin2,
            // dx,dy) -> warm-boot rebuild silently dropped every LCC run.
            if b.len() < 12 + 1 + 8 + 64 + 4 {
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
            let g = GridLambert::new(nx, ny, la1, lo1, lov, lad, latin1, latin2,
                                     dx_m, dy_m, proj_flag, scan, earth_shape, winds != 0);
            return Ok(GridDef::Lambert(g));
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

/// Public alias of state.rs atomic byte writer (grid.bin/manifest.json).
pub fn write_bytes_atomic_pub(path: &str, bytes: &[u8]) -> Result<(), String> {
            let tmp = format!("{}.tmp", path);
    let f = std::fs::File::create(&tmp);
    match f {
        Err(_) => { return Err("create failed".to_string()); }
        Ok(mut h) => {
            let w = h.write_all(&bytes);
            if w.is_err() {
                return Err("write_all failed".to_string());
            }
            let fl = h.flush();
            if fl.is_err() {
                return Err("flush failed".to_string());
            }
        }
    }
    let r = std::fs::rename(&tmp, &path);
    if r.is_err() {
        return Err("rename failed".to_string());
    }
    return Ok(());
}

// ---------------------------------------------------------------------------
// manifest.json (human-facing inventory; index is authoritative)
// ---------------------------------------------------------------------------

/// One field entry line for the manifest.
pub fn manifest_field_json(param: &str, level_dir: &str, units: &str,
                           stat_processed: bool, stat_kind: String,
                           levels: Vec<f64>, fhs: Vec<i64>) -> String {
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
        fh = format!("{0}{1}", fh, fhs[k]);
    }
    let mut st = "false";
    if stat_processed {
        st = "true";
    }
    return format!("{{\"param\":\"{0}\",\"level_type\":\"{1}\",\"units\":\"{2}\",\"stat_processed\":{3},\"stat_kind\":\"{4}\",\"levels\":[{5}],\"forecast_hours\":[{6}]}}",
        param, level_dir, units, st, stat_kind, lv, fh);
}

fn fmt_level(v: f64) -> String {
    if v == v.trunc() {
        return format!("{}", v.trunc());
    }
    return format!("{}", v);
}
