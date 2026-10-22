// V1.3 phase E0 UPGRADE-TRAP RECOVERY GATE — FIELD files (BIN_VERSION 2).
//
// Phase E0 bumped the field-file format from version 1 to 2 (the header grew
// the 32-byte PDT 4.8 stat block, see wx_core::storage). A run directory
// written by the OLD format must NOT be silently mis-parsed or silently
// skipped: the chosen compatibility strategy (docs/DECISIONS.md) reuses the
// V1.2 phase B0 STORAGE-FORMAT RECOVERY machinery — LOUD log + DELETE the
// unusable run dir + FORGET that model's ingest records, so the NEXT scan
// round re-decodes from source with the CURRENT format. The pre-check runs
// BEFORE anything is indexed (a cheap 12-byte read per field file via
// `bin_version_of_path`), so a foreign-version file never reaches a query.
//
// This test plants the REAL v1 writer-golden artifact (tests/fixtures/
// writer_golden_v1.bin, magic OK, version 1) over an ingested gfs f006 field
// and pins the whole recovery arc end-to-end. PRE-FIX DISCRIMINATOR: pre-E0
// there was no field-version pre-check at all; the v1 file OPENED FINE as a
// "v1-era current" file whose level coords were read from the WRONG offset
// once the reader moved, or (before any reader change) simply indexed
// garbage stats — none of which produce the recovery note asserted below.
//
// Single test in this binary: counter assertions are process-wide.

use wx_ingest::pipeline::{Pipeline, level_dir_name, fh_file_name};
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_core::tables::{lookup_level_type};
use wx_core::storage::BIN_VERSION;
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

#[allow(clippy::needless_return)]
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
    return false;
}

#[allow(clippy::needless_return)]
fn rm_tree(dir: &str) {
    match read_dir(&dir) {
        Err(_) => {}
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let p = path_to_str(&de.path());
                        match read_dir(&p) {
                            Err(_) => {
                                let _r = remove_file(&p);
                            }
                            Ok(_) => {
                                rm_tree(&p);
                            }
                        }
                    }
                }
            }
        }
    }
    let _d = remove_dir(&dir);
}

#[allow(clippy::needless_return)]
fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(bytes);
            match n {
                Err(_) => { assert!(false); return; }
                Ok(_) => {}
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn read_bytes(path: &str) -> Vec<u8> {
    let f = File::open(&path);
    match f {
        Err(_) => { return Vec::<u8>::new(); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return Vec::<u8>::new(); }
                Ok(region) => {
                    let mut copy: Vec<u8> = Vec::<u8>::new();
                    for k in 0 .. region.len() {
                        copy.push(region[k]);
                    }
                    return copy;
                }
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn exists_any(path: &str) -> bool {
    let f = File::open(&path);
    match f {
        Err(_) => { return false; }
        Ok(_) => { return true; }
    }
}

fn cfg_text(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

// Bounded settle loop: never pin WHICH round ingests (V1.2 phase B0 rule).
#[allow(clippy::needless_return)]
fn settle(p: &mut Pipeline, want: usize) -> usize {
    let mut got = 0usize;
    let mut rounds = 0usize;
    while got != want && rounds < 8usize {
        let one = p.scan_round();
        got += one;
        rounds += 1;
        if rounds >= 8usize {
            break;
        }
    }
    return got;
}

#[test]
fn v1_field_bin_triggers_loud_recovery_and_the_run_rebuilds_from_source() {
    let root = "/tmp/opencode/wxpoint-field-upgrade".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "gfs_small.grib2"),
                &read_bytes("../../tests/fixtures/gfs_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg_text(&data, &ing).as_bytes());

    // ---- life 1: normal ingest ---------------------------------------------
    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);
    assert!(p1.counters.errors == 0);
    let run_dir = join_path(&join_path(&data, "gfs"), "2026101912");
    let iso_dir = level_dir_name(100);
    let f006 = join_path(&join_path(&join_path(&run_dir, "TMP"), iso_dir.as_str()),
                         fh_file_name(21600).as_str());
    assert!(exists_any(&f006));

    // ---- plant the OLD-format FIELD file (real v1 golden artifact) ---------
    // Same magic, version u32 LE = 1 at byte 8. The grid.bin next to it stays
    // CURRENT: only the field-version pre-check may fire.
    let v1 = read_bytes("../../tests/fixtures/writer_golden_v1.bin");
    assert!(v1.len() == 456);
    write_bytes(&f006, &v1[..]);

    // ---- life 2: restart with the CURRENT binary over the OLD data dir ----
    let p2r = Pipeline::open(&cfg_path);
    let mut p2 = match p2r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    // (a) LOUD log naming the recovery AND the versions found vs current.
    let mut logged = false;
    let mut named_version = false;
    for line in &p2.log {
        if bytes_contains(line.as_bytes(), "STORAGE-FORMAT RECOVERY".as_bytes()) {
            logged = true;
        }
        if bytes_contains(line.as_bytes(), "found bin version 1, current 2".as_bytes()) {
            named_version = true;
        }
    }
    assert!(logged);
    assert!(named_version);
    // (b) counted as an ingest error (visible on /metrics)
    assert!(p2.counters.errors == 1);
    // (c) the unusable run dir is GONE (grid.bin too: whole-dir recovery)
    assert!(!exists_any(&join_path(&run_dir, "grid.bin")));
    // (d) the model's ingest records were FORGOTTEN so source re-decodes
    let st_txt = read_bytes(&join_path(&join_path(&data, "state"), "files.json"));
    assert!(bytes_contains(&st_txt, "\"recs\":[]".as_bytes()));
    // (e) index has NO gfs run right now (nothing queryable yet)
    let snap0 = p2.shared.get_index();
    let s0 = &*snap0;
    match s0.find_model("gfs") {
        None => {}
        Some(m) => { assert!(m.runs.is_empty()); }
    }

    // ---- life 2 continued: scan re-ingests from SOURCE and it works again --
    assert!(settle(&mut p2, 1usize) == 1usize);
    let fb2 = read_bytes(&f006);
    assert!(fb2.len() >= 56);
    let ver = (fb2[8] as u32) | ((fb2[9] as u32) << 8) | ((fb2[10] as u32) << 16) | ((fb2[11] as u32) << 24);
    assert!(ver == BIN_VERSION); // CURRENT field format on disk now

    // fully queryable with the CORRECT value (same probe grid_upgrade uses:
    // TMP@1000 at node lat=10 lon=350 -> 250 K, exact node)
    let snap1 = p2.shared.get_index();
    let sref = &*snap1;
    match lookup_level_type("isobaric") {
        None => { assert!(false); return; }
        Some(iso) => {
            let c_r = build_context(sref, "gfs", None, "TMP", iso,
                                    1000.0, 0.0, TimeInterpMode::Linear);
            match c_r {
                Err(_) => { assert!(false); return; }
                Ok(c) => {
                    let v = evaluate_point(&c, 10.0, 350.0);
                    match v {
                        Err(_) => { assert!(false); return; }
                        Ok(pv) => {
                            assert!(pv.value == 250.0);
                            assert!(pv.info.horizontal == HorizMethod::Exact);
                        }
                    }
                }
            }
        }
    }
    assert!(p2.counters.errors == 1); // recovery logged once, no new errors
    rm_tree(&root);
}
