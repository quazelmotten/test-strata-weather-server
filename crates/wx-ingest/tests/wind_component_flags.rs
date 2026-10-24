// Subtask-0 FLAG AUDIT (pinned): which wind messages get rotated is gated by// Code Table 3.3 bit 0x08 ("direction given relative to GRID"), and our
// decoder must read EXACTLY the octet/bit that stock tools/wgrib2 uses for its
// `-V` display (`Sec3.c: res & 8 ? "winds(grid)" : "winds(N/S)"`,
// `FlagTable.c::flag_table_3_3_location`: template 3.30 -> gds+46, templates
// 3.0/3.40 -> gds+54; mk_kgds.c kgds[5] = 128 | (flag & 8)). Our parser reads
// p[41] on template-3.30 payloads and p[49] on 3.0/3.40 payloads where p ==
// section_bytes[5:], i.e. zero-based section offsets 46 and 54 — THE SAME
// OCTETS as gds+46 / gds+54 (wgrib2's sec[] pointers address the raw section).
//
// Table below recorded 2026-10-24 against the MAIN repo's stock nolib build
// (`tools/wgrib2 <file> -V`, wind lines grepped), file by file:
//
//   tests/fixtures/hrrr_small.grib2          msg1-2 TMP  0x30 N/S  | msg3-6 UGRD/VGRD 0x38 grid
//   tests/fixtures/hrrr_small_c2.grib2       same message pattern as hrrr_small
//   tests/fixtures/hrrr_small_c3.grib2       same message pattern as hrrr_small
//   tests/fixtures/hrrr_small_ieee.grib2     same message pattern as hrrr_small
//   tests/fixtures/hrrr_small_aec.grib2      same message pattern as hrrr_small
//   tests/fixtures/hrrr_small_jpeg.grib2     same message pattern as hrrr_small
//   tests/fixtures/hrrr_small_png.grib2      same message pattern as hrrr_small
//   tests/fixtures/hrrr_small_png_f.grib2    same message pattern as hrrr_small
//   tests/fixtures/hrrr_ellip_small.grib2    msg1-2 UGRD/VGRD 0x38 grid | msg3 TMP 0x30 N/S
//   tests/fixtures/hrrr_orphan.grib2         msg1-2 TMP 0x30 N/S  | msg3-4 UGRD 0x38 grid
//   tests/fixtures/hrrr_ipolates_earth.grib2 ALL FOUR msgs 0x30 N/S (new_grid
//                                            CLEARS the flag: stored values ARE earth-relative)
//   tests/realdata/hrrr_uv10m_f06.grib2      msg1-2 UGRD/VGRD 0x08 grid (scan_mode 0x40)
//
// The audit found NO inversion/misposition: every winds_grid decision of the
// decoder equals the stock-tool display for every wind message above, and the
// raw-octet anchor below re-derives the bit straight from the file bytes so a
// future offset refactor cannot silently move it.

// Style policy follows the crate (same local-toolchain parser constraints as
// src/lib.rs and bounds_complex.rs): these cosmetic lints are silenced here.
#![allow(clippy::needless_return)]
#![allow(clippy::single_match)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::needless_range_loop)]

use std::fs;
use wx_core::grid::GridDef;
use wx_ingest::grib2::{decode_file, grid_winds, ParamFilter};

fn fixture_path(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

/// Minimal section walker for this repo's trimmed/wgrib2-stub GRIB2 layout
/// ('GRIB' + 16-byte preheader whose last 8 bytes carry the whole-message
/// length; sections start at byte 16 of each message). Returns one entry per
/// message: (cat, num, fc_offset, flag-table-3.3 octet value, scan byte,
/// grid template number).
fn raw_sec3_audit(path: &str) -> Vec<(u8, u8, u32, u8, u8, u16)> {
    let b = match fs::read(path) { Ok(x) => x, Err(_) => { panic!("read {path}"); } };
    let mut out: Vec<(u8, u8, u32, u8, u8, u16)> = Vec::new();
    let mut i: usize = 0;
    while i + 16 <= b.len() {
        if &b[i..i + 4] != b"GRIB" { i += 4; continue; }
        let ln = u64::from_be_bytes([b[i + 8], b[i + 9], b[i + 10], b[i + 11],
                                     b[i + 12], b[i + 13], b[i + 14], b[i + 15]]) as usize;
        if ln < 24 || i + ln > b.len() { i += 4; continue; }
        let mut q = i + 16;
        let mut s3: Option<Vec<u8>> = None;
        let mut s4: Option<Vec<u8>> = None;
        while q + 5 <= i + ln {
            let slen = u32::from_be_bytes([b[q], b[q + 1], b[q + 2], b[q + 3]]) as usize;
            let num = b[q + 4];
            if slen < 6 || q + slen > i + ln { break; }
            if num == 3 { s3 = Some(b[q..q + slen].to_vec()); }
            if num == 4 { s4 = Some(b[q..q + slen].to_vec()); }
            if num == 7 { break; }
            q += slen;
        }
        match (s3, s4) {
            (Some(t3), Some(t4)) => {
                let tmpl = u16::from_be_bytes([t3[12], t3[13]]);
                // parse_sec3 receives section_bytes[5:] and reads the component
                // flags at payload p[41] (template 3.30) or p[49] (3.0/3.40);
                // scanning mode at p[59] / p[66] respectively.
                let (flag_idx, scan_idx): (usize, usize) = match tmpl {
                    30 | 31 => { (5 + 41, 5 + 59) }
                    _ => { (5 + 49, 5 + 66) }
                };
                let cat = t4[5 + 4];
                let num = t4[5 + 5];
                let fc = u32::from_be_bytes([t4[5 + 13], t4[5 + 14], t4[5 + 15], t4[5 + 16]]);
                out.push((cat, num, fc, t3[flag_idx], t3[scan_idx], tmpl));
            }
            _ => {}
        }
        i += ln;
    }
    return out;
}

fn audit(label: &str, path: &str, expect_wind: bool, expect_scan: Option<u8>) {
    let r = decode_file(path.to_string(), ParamFilter::All);
    let (msgs, _st) = match r {
        Ok(x) => x,
        Err(e) => { panic!("decode {label}: {e:?}"); }
    };
    let raw = raw_sec3_audit(path);
    if raw.len() != msgs.len() {
        panic!("{label}: raw walk {} vs decoder {} messages", raw.len(), msgs.len());
    }
    for x in 0 .. msgs.len() {
        let m = &msgs[x];
        let (cat, num, _fc_raw, flag, scan, tmpl) = raw[x];
        assert!(m.category == cat && m.number == num,
                "{label} msg{x}: ident mismatch decoder({},{}) raw({cat},{num})",
                m.category, m.number);
        // (fc offsets are compared only through the decoder, which applies the
        // Code Table 4.4 time unit; the raw walk keeps the undivided octet.)
        let is_wind = (cat, num) == (2, 2) || (cat, num) == (2, 3);
        let bit = (flag & 0x08) != 0;
        // (a) raw octet bit == what our decoder reports for this message
        let flag_octet_1based: u16 = if tmpl == 30 || tmpl == 31 { 47 } else { 55 };
        assert!(m.winds_grid == bit,
                "{label} msg{x} ({cat}/{num} tmpl{tmpl}): decoder winds_grid={} \
                 but raw section octet {flag_octet_1based} = {flag:#04x}", m.winds_grid);
        assert!(grid_winds(&m.grid) == bit, "{label} msg{x}: grid_winds disagrees");
        // (b) expected stock-wgrib2 column from the table above
        if is_wind {
            assert!(bit == expect_wind,
                    "{label} msg{x}: wind flag set={bit} unexpected (stock wgrib2 -V says {}",
                    if expect_wind { "winds(grid)" } else { "winds(N/S)" });
        }
        match expect_scan {
            Some(want) => {
                match m.grid {
                    GridDef::Lambert(ref g) => {
                        assert!(g.scan_mode == want,
                                "{label} msg{x}: scan_mode {:#04x} != {want:#04x}", g.scan_mode);
                    }
                    _ => {}
                }
                assert!(scan == want, "{label} msg{x}: raw scan byte {scan:#04x} != {want:#04x}");
            }
            None => {}
        }
    }
    println!("AUDIT OK {label}: {} messages, wind-flag expectations respected \
              (wind msgs grid-relative = {expect_wind})", msgs.len());
}

#[test]
fn wind_component_flags_match_stock_wgrib2_display() {
    // every synthetic HRRR twin: TMP messages carry the flag CLEARED ("N/S"),
    // UGRD/VGRD messages carry bit 0x08 SET ("grid") -- exactly what
    // tools/wgrib2 -V prints per file (see header table).
    let twins: [&str; 8] = ["hrrr_small.grib2", "hrrr_small_c2.grib2",
                            "hrrr_small_c3.grib2", "hrrr_small_ieee.grib2",
                            "hrrr_small_aec.grib2", "hrrr_small_jpeg.grib2",
                            "hrrr_small_png.grib2", "hrrr_small_png_f.grib2"];
    for t in twins.iter() {
        audit(t, &fixture_path(t), true, Some(0x00));   // N->S scanned fixtures
    }
    audit("hrrr_ellip_small.grib2", &fixture_path("hrrr_ellip_small.grib2"),
          true, Some(0x00));
    audit("hrrr_orphan.grib2", &fixture_path("hrrr_orphan.grib2"), true, Some(0x00));
}

#[test]
fn ipolates_golden_carries_flag_cleared_and_decodes_as_earth_relative() {
    // The golden's flag byte must read CLEAR so that decoding it AS-IS yields
    // earth-relative values with NO re-rotation (this is what pins the
    // comparison protocol of rotation_vs_ipolates.rs).
    audit("hrrr_ipolates_earth.grib2", &fixture_path("hrrr_ipolates_earth.grib2"),
          false, Some(0x00));
    let r = decode_file(fixture_path("hrrr_ipolates_earth.grib2"), ParamFilter::All);
    let (msgs, _st) = match r { Ok(x) => x, Err(e) => { panic!("{e:?}"); } };
    assert!(msgs.len() == 4);
    for m in &msgs {
        assert!(!m.winds_grid);
        match m.param_name {
            Some(s) => { assert!(s == "UGRD" || s == "VGRD"); }
            None => { panic!(); }
        }
    }
}

#[test]
fn real_hrrr_uv10m_flag_audit_if_present() {
    // production bytes (S->N scan_mode 0x40, flag SET); clean skip when
    // tests/realdata is not installed.
    let p = "../../tests/realdata/hrrr_uv10m_f06.grib2";
    if !std::path::Path::new(p).exists() {
        println!("SKIP: {p} absent");
        return;
    }
    audit("hrrr_uv10m_f06.grib2", p, true, Some(0x40));
}
