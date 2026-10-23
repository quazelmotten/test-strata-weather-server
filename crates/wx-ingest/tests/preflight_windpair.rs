// Spec §4.4 STARTUP wind-pair rule (V1.1 phase 4): config-time preflight against
// the FIRST STABLE file matching the model glob, with the decode-time rule kept
// as the second line of defense. Behavior matrix tested here:
//
//   single component + first stable file's WIND messages GRID-relative -> FAIL
//   single component + WIND messages EARTH-relative                    -> accept
//   both components / neither                                   -> no check
//   empty dir / unstable file / unreadable file                 -> defer (note)
//
// Raw code-triple configs (discipline/category/number) count as wind parameters
// too, since a config may name parameters by GRIB2 code.

use wx_ingest::config::{validate_text, startup_wind_preflight, wind_pair_gap};
use wx_ingest::grib2::{probe_first_grid};
use wx_ingest::state::{join_path, now_ns_marker};
use memmap2::Mmap;
use std::fs::File;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::io::Write;

fn fixture(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

fn bytes_contains(hay: &[u8], pat: &[u8]) -> bool {
    if pat.len() > hay.len() {
        return false;
    }
    for s in 0 .. (hay.len() - pat.len() + 1) {
        let mut ok = true;
        for k in 0 .. pat.len() {
            if hay[s + k] != pat[k] {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }
    false
}

fn has(s: &str, pat: &str) -> bool {
    bytes_contains(s.as_bytes(), pat.as_bytes())
}

fn read_all(path: &str) -> Vec<u8> {
    let f = File::open(path);
    match f {
        Err(_) => { Vec::<u8>::new()}
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { Vec::<u8>::new()}
                Ok(region) => {
                    let mut copy: Vec<u8> = Vec::<u8>::new();
                    for k in 0 .. region.len() {
                        copy.push(region[k]);
                    }
                    copy
                }
            }
        }
    }
}

fn write_all(path: &str, bytes: &[u8]) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!();}
        Ok(mut w) => {
            let n = w.write_all(bytes);
            match n {
                Err(_e) => { panic!(); }
                Ok(_c) => {}
            }
        }
    }
}

/// Fresh scratch directory under /tmp/opencode, unique per test name.
fn scratch(tag: &str) -> String {
    let base = format!("/tmp/opencode/wxpreflight_{0}", tag);
    let _c = create_dir_all(&base);
    base
}

fn cfg_text(model: &str, dir: &str, glob: &str, params: &str) -> String {
    format!("{0}{1}{2}{3}{4}{5}{6}{7}",
        "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"/tmp/opencode/wxpreflight_data\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 10\n\n[[models]]\nname = \"",
        model, "\"\ningest_dir = \"", dir, "\"\nfile_glob = \"", glob, "\"\n", params)
}

const P_U: &str = "\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n";
const P_UV: &str = "\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"height_above_ground:10\"]\n";
const P_RAW_U: &str = "\n  [[models.params]]\n  discipline = 0\n  category = 2\n  number = 2\n  levels = [\"height_above_ground:10\"]\n";
const P_RAW_V: &str = "\n  [[models.params]]\n  discipline = 0\n  category = 2\n  number = 3\n  levels = [\"*\"]\n";
const P_VVEL: &str = "\n  [[models.params]]\n  name = \"VVEL\"\n  levels = [\"height_above_ground:2\"]\n";

#[test]
fn preflight_rejects_single_component_on_grid_relative_grid() {
    // spec §4.4 wording: fail CONFIG VALIDATION at startup, naming model/file/flags
    let dir = scratch("reject");
    let b = read_all(&fixture("hrrr_small.grib2"));
    assert!(!b.is_empty());
    write_all(&join_path(&dir, "hrrr_small.grib2"), &b);
    let txt = cfg_text("hrrr", &dir, "hrrr*.grib2", P_U);
    match validate_text(&txt) {
        Err(_e) => { panic!(); }
        Ok(models) => {
            // clock far past the 10 s stability window
            let later = now_ns_marker(&dir) + 3600000000000;
            match startup_wind_preflight(models, 10, later) {
                Ok(notes) => {
                    for n in &notes {
                        println!("{0}", n);
                    }
                    panic!();
                }
                Err(msg) => {
                    assert!(has(&msg, "model 'hrrr'"));
                    assert!(has(&msg, "ONLY UGRD"));
                    assert!(has(&msg, "VGRD"));
                    assert!(has(&msg, "GRID-RELATIVE"));
                    assert!(has(&msg, "flag table 3.3 byte 0x38"));
                    assert!(has(&msg, "hrrr_small.grib2"));
                }
            }
        }
    }
    let _rf = remove_file(join_path(&dir, "hrrr_small.grib2"));
    let _rd = remove_dir(&dir);
}

#[test]
fn preflight_accepts_pair_and_earth_relative_single() {
    let dir = scratch("accept");
    let b = read_all(&fixture("hrrr_small.grib2"));
    write_all(&join_path(&dir, "hrrr_small.grib2"), &b);
    // (a) both components configured: nothing to check, no probe needed
    let t1 = cfg_text("hrrr", &dir, "hrrr*.grib2", P_UV);
    match validate_text(&t1) {
        Err(_e) => { panic!(); }
        Ok(models) => {
            match startup_wind_preflight(models, 10, now_ns_marker(&dir) + 3600000000000) {
                Err(_m) => { panic!(); }
                Ok(notes) => { assert!(notes.is_empty()); }
            }
        }
    }
    // (b) single component, but the file's wind messages are EARTH-relative
    //     (GFS-style regular lat/lon grids): accepted with a note
    let g = read_all(&fixture("gfs_small.grib2"));
    write_all(&join_path(&dir, "gfs_small.grib2"), &g);
    let t2 = cfg_text("gfslike", &dir, "gfs*.grib2", P_U);
    match validate_text(&t2) {
        Err(_e) => { panic!(); }
        Ok(models) => {
            match startup_wind_preflight(models, 10, now_ns_marker(&dir) + 3600000000000) {
                Err(_m) => { panic!(); }
                Ok(notes) => {
                    assert!(notes.len() == 1);
                    assert!(has(&notes[0], "EARTH-relative"));
                    assert!(has(&notes[0], "accepted"));
                }
            }
        }
    }
    let _r1 = remove_file(join_path(&dir, "hrrr_small.grib2"));
    let _r2 = remove_file(join_path(&dir, "gfs_small.grib2"));
    let _rd = remove_dir(&dir);
}

#[test]
fn preflight_defers_when_nothing_stable_or_readable() {
    let dir = scratch("defer");
    let t0 = cfg_text("hrrr", &dir, "hrrr*.grib2", P_U);
    // empty directory -> defer
    match validate_text(&t0) {
        Err(_e) => { panic!(); }
        Ok(models) => {
            match startup_wind_preflight(models, 10, now_ns_marker(&dir)) {
                Err(_m) => { panic!(); }
                Ok(notes) => {
                    assert!(notes.len() == 1);
                    assert!(has(&notes[0], "deferring"));
                    assert!(has(&notes[0], "no matching file present"));
                }
            }
        }
    }
    // file present but JUST written (inside the stability window) -> defer
    let b = read_all(&fixture("hrrr_small.grib2"));
    write_all(&join_path(&dir, "hrrr_fresh.grib2"), &b);
    match validate_text(&t0) {
        Err(_e) => { panic!(); }
        Ok(models) => {
            match startup_wind_preflight(models, 10, now_ns_marker(&dir)) {
                Err(_m) => { panic!(); }
                Ok(notes) => { assert!(has(&notes[0], "not stable yet")); }
            }
        }
    }
    // garbage file matching the glob: probe fails -> defer, never panic
    let _rf = remove_file(join_path(&dir, "hrrr_fresh.grib2"));
    write_all(&join_path(&dir, "hrrr_junk.grib2"), "GRIBjunk".to_string().as_bytes());
    match validate_text(&t0) {
        Err(_e) => { panic!(); }
        Ok(models) => {
            match startup_wind_preflight(models, 10, now_ns_marker(&dir) + 3600000000000) {
                Err(_m) => { panic!(); }
                Ok(notes) => {
                    assert!(has(&notes[0], "could not probe") ||
                            has(&notes[0], "cannot decide"));
                }
            }
        }
    }
    let _rj = remove_file(join_path(&dir, "hrrr_junk.grib2"));
    let _rd = remove_dir(&dir);
}

#[test]
fn wind_pair_gap_recognizes_raw_code_triples() {
    // U by raw triple 0/2/2 alone -> gap
    let t1 = cfg_text("m", "/tmp/opencode/wxpreflight_gap", "x*", P_RAW_U);
    match validate_text(&t1) {
        Err(_e) => { panic!(); }
        Ok(models) => {
            match wind_pair_gap(&models[0].params) {
                None => { panic!(); }
                Some(p) => {
                    assert!(p.0 == "UGRD");
                    assert!(p.1 == "VGRD");
                }
            }
        }
    }
    // V by raw triple 0/2/3 alone -> gap the other way
    let t2 = cfg_text("m", "/tmp/opencode/wxpreflight_gap", "x*", P_RAW_V);
    match validate_text(&t2) {
        Err(_e) => { panic!(); }
        Ok(models) => {
            match wind_pair_gap(&models[0].params) {
                Some(p) => {
                    assert!(p.0 == "VGRD");
                    assert!(p.1 == "UGRD");
                }
                None => { panic!(); }
            }
        }
    }
    // VVEL (category 2, number 8) is NOT part of the pair
    let t3 = cfg_text("m", "/tmp/opencode/wxpreflight_gap", "x*", P_VVEL);
    match validate_text(&t3) {
        Err(_e) => { panic!(); }
        Ok(models) => {
            if let Some(_p) = wind_pair_gap(&models[0].params) { panic!(); }
        }
    }
}

#[test]
fn probe_reports_wind_message_flags() {
    // The probe classifies the WIND messages specifically: hrrr_small mixes an
    // earth-relative scalar grid with a grid-relative vector grid, so "the first
    // grid in the file" alone would give the WRONG answer.
    match probe_first_grid(fixture("hrrr_small.grib2")) {
        Err(_e) => { panic!(); }
        Ok(pp) => {
            assert!(pp.messages == 6);
            assert!(pp.wind_msg == 2);
            assert!((pp.wind_flag33 as u32) & 8 != 0);
            assert!(pp.winds_true == 4);
            assert!(pp.winds_false == 2);
        }
    }
    match probe_first_grid(fixture("gfs_small.grib2")) {
        Err(_e) => { panic!(); }
        Ok(pp) => {
            assert!(pp.wind_msg == 1);
            assert!((pp.wind_flag33 as u32) & 8 == 0);
            assert!(pp.winds_true == 0);
        }
    }
    match probe_first_grid(fixture("hrrr_ellip_small.grib2")) {
        Err(_e) => { panic!(); }
        Ok(pp) => {
            assert!(pp.wind_msg == 2);
            assert!(pp.tmpl == 30);
        }
    }
    // a file with no wind message at all cannot decide
    match probe_first_grid(fixture("stat_small.grib2")) {
        Err(_e) => { panic!(); }
        Ok(pp) => { assert!(pp.wind_msg == 0); }
    }
}
