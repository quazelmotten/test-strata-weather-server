// V1.2 phase B0 UPGRADE-TRAP RECOVERY GATE.
//
// `store.rs` bumped GRID_VERSION to 2 when grid.bin gained the Code Table 3.2
// earth axes (V1.1 phase 4). Pre-change, a run directory written by the OLD
// format was rejected SILENTLY on warm start: `ingest_run_dir` returned without
// logging, `store_plane` only writes grid.bin when MISSING (so the v1 file was
// never replaced), and the persistent state still marked the SOURCE files as
// ingested (so `scan_round` skipped them). Result: `/ready` stayed false FOREVER
// with no clue and inventory was EMPTY.
//
// Chosen recovery (docs/DECISIONS.md + README limitations): LOUD log + DELETE the
// unusable run dir + FORGET that MODEL's ingest records, so the NEXT scan round
// re-decodes from source with the CURRENT format. This test pins that behavior
// end-to-end: plant a v1 grid.bin over an ingested run, restart, assert the run
// dir is gone AND logged AND the state was forgotten, then assert the run comes
// BACK fully queryable with the right values.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_core::tables::{lookup_level_type};
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use wx_ingest::store::{GRID_MAGIC, GRID_VERSION};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

// substring search over raw bytes (same helper shape as restart.rs)
#[allow(clippy::needless_return)]
pub fn bytes_contains(hay: &[u8], pat: &[u8]) -> bool {
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
    match read_dir(dir) {
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
    let _d = remove_dir(dir);
}

#[allow(clippy::needless_return)]
fn copy_file(src: String, dst: String) {
    let f = File::open(&src);
    match f {
        Err(_) => { panic!(); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { panic!(); }
                Ok(region) => {
                    let d = File::create(&dst);
                    match d {
                        Err(_) => { panic!(); }
                        Ok(mut w) => {
                            let n = w.write_all(&region[0 .. region.len()]);
                            if n.is_err() { panic!(); }
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!(); }
        Ok(mut w) => {
            let n = w.write_all(bytes);
            if n.is_err() { panic!(); }
        }
    }
}

#[allow(clippy::needless_return)]
fn read_bytes(path: &str) -> Vec<u8> {
    let f = File::open(path);
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
    let f = File::open(path);
    match f {
        Err(_) => { return false; }
        Ok(_) => { return true; }
    }
}

pub fn cfg_text(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n"
}

// Bounded settle loop: never pin WHICH round ingests (V1.2 phase B0 rule).
#[allow(clippy::needless_return)]
pub fn settle(p: &mut Pipeline, want: usize) -> usize {
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
fn v1_grid_bin_triggers_loud_recovery_and_the_run_rebuilds_from_source() {
    let root = "/tmp/opencode/wxpoint-grid-upgrade".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    copy_file("../../tests/fixtures/gfs_small.grib2".to_string(), join_path(&ing, "gfs_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg_text(&data, &ing).as_bytes());

    // ---- life 1: normal ingest ---------------------------------------------
    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    let n1 = settle(&mut p1, 1usize);
    assert!(n1 == 1usize);
    let run_dir = join_path(&join_path(&data, "gfs"), "2026101912");
    assert!(exists_any(&join_path(&run_dir, "grid.bin")));

    // ---- plant the OLD-format grid.bin (magic OK, version 1) --------------
    // bytes: magic(8) | version u32 LE = 1 | kind byte + junk
    let mut v1: Vec<u8> = Vec::<u8>::new();
    for b in GRID_MAGIC {
        v1.push(b);
    }
    // little-endian u32 version = 1 (bytes identical to the shift/mask form)
    v1.extend_from_slice(&[1u8, 0u8, 0u8, 0u8]);
    for _k in 0 .. 64 {
        v1.push(0xABu8);
    }
    write_bytes(&join_path(&run_dir, "grid.bin"), &v1[..]);

    // ---- life 2: restart with the CURRENT binary over the OLD data dir ----
    let p2r = Pipeline::open(&cfg_path);
    let mut p2 = match p2r {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    // (a) LOUD log naming the recovery
    let mut logged = false;
    for line in &p2.log {
        if bytes_contains(line.as_bytes(), "STORAGE-FORMAT RECOVERY".as_bytes()) {
            logged = true;
            break;
        }
    }
    assert!(logged);
    // (b) counted as an ingest error (visible on /metrics)
    assert!(p2.counters.errors == 1);
    // (c) the unusable run dir is GONE
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
    let n2 = settle(&mut p2, 1usize);
    assert!(n2 == 1usize);
    let gb2 = read_bytes(&join_path(&run_dir, "grid.bin"));
    assert!(gb2.len() >= 12);
    // current format version on disk now
    let ver = (gb2[8] as u32) | ((gb2[9] as u32) << 8) | ((gb2[10] as u32) << 16) | ((gb2[11] as u32) << 24);
    assert!(ver == GRID_VERSION);

    // fully queryable with the CORRECT value (same probe restart.rs uses:
    // TMP@1000 at node lat=10 lon=350 -> 250 K, exact node)
    let snap1 = p2.shared.get_index();
    let sref = &*snap1;
    match lookup_level_type("isobaric") {
        None => { panic!(); }
        Some(iso) => {
            let c_r = build_context(sref, "gfs", None, "TMP", iso,
                                    1000.0, 0.0, TimeInterpMode::Linear);
            match c_r {
                Err(_) => { panic!(); }
                Ok(c) => {
                    let v = evaluate_point(&c, 10.0, 350.0);
                    match v {
                        Err(_) => { panic!(); }
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

#[test]
fn healthy_run_dirs_are_left_untouched_by_the_recovery_path() {
    // A warm start over CURRENT-format data must NOT delete or re-ingest anything.
    let root = "/tmp/opencode/wxpoint-grid-upgrade-clean".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    copy_file("../../tests/fixtures/gfs_small.grib2".to_string(), join_path(&ing, "gfs_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg_text(&data, &ing).as_bytes());

    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);

    let p2r = Pipeline::open(&cfg_path);
    let mut p2 = match p2r {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    // rebuilt from disk, NOT deleted: grid.bin still present, no log, no error
    assert!(exists_any(&join_path(&join_path(&join_path(&data, "gfs"), "2026101912"), "grid.bin")));
    assert!(p2.log.is_empty());
    assert!(p2.counters.errors == 0);
    // and unchanged files are NOT re-ingested (record hit)
    let a = p2.scan_round();
    let b = p2.scan_round();
    assert!(a == 0);
    assert!(b == 0);
    rm_tree(&root);
}
