// V1.3 phase E3b DEFECT GATE #2 — A MISSING grid.bin IS LOUD, NOT SILENT.
//
// THE DEFECT (AUDIT-confirmed): a CORRUPT grid.bin gets the storage-format
// recovery treatment (loud note + delete + forget records), but ingest_run_dir
// answered a MISSING (or unreadable) grid.bin with a bare `None => { return; }`
// — an orphan run tree sat there invisible to /v1/inventory with zero notes and
// zero counters.
//
// THE FIX: ONE loud note per run per rebuild naming the dir + one `errors`
// increment; deliberately NO auto-delete (the source may re-deliver the grid;
// unlike the version-mismatch case nothing is wrong to REPAIR, something is
// merely ABSENT). This binary plants both shapes — a real run whose grid.bin is
// deleted, and a bare skeleton run dir — and pins note text, counter, index
// absence, and that HEALTHY sibling runs still index normally.

use wx_ingest::pipeline::Pipeline;
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
fn count_notes(p: &Pipeline, pat: &str) -> usize {
    let mut n = 0usize;
    for line in &p.log {
        if line.contains(pat) {
            n += 1;
        }
    }
    return n;
}

#[allow(clippy::needless_return)]
fn mg_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

// ---------------------------------------------------------------------------
// (a) a REAL run whose grid.bin vanishes: loud at boot, absent from the index
// ---------------------------------------------------------------------------

#[test]
fn deleting_grid_bin_makes_the_run_loudly_invisible_not_silent() {
    let root = "/tmp/opencode/wxpoint-missgrid";
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "gfs_small.grib2"), &read_bytes("../../tests/fixtures/gfs_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, mg_cfg(&data, &ing).as_bytes());

    // ingest fully with a healthy pipeline...
    let p0_r = Pipeline::open(&cfg_path);
    let mut p0 = match p0_r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(p0.scan_round() == 0);
    assert!(p0.scan_round() == 1);
    assert!(p0.registry.len() == 1);
    assert!(p0.registry[0].runs.len() == 1);
    drop(p0);

    // ...then simulate the orphan: delete ONLY grid.bin.
    let gp = format!("{0}/gfs/2026101912/grid.bin", data);
    let rf = remove_file(&gp);
    match rf {
        Err(_) => { assert!(false); return; }
        Ok(_) => {}
    }

    // RESTART: the rebuild must be LOUD and COUNTED, and the run must not
    // index (pre-E3b: silent note-free nothing, errors stayed 0).
    let p1_r = Pipeline::open(&cfg_path);
    let p1 = match p1_r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(count_notes(&p1, "MISSING GRID") == 1);
    assert!(p1.counters.errors == 1);
    // the orphan is NOT deleted on purpose (source may re-deliver)...
    let dir_still_there = read_dir(&format!("{0}/gfs/2026101912", data));
    match dir_still_there {
        Err(_) => { assert!(false); return; }
        Ok(_) => {}
    }
    // ...and it stays OUT of the registry/inventory while grid.bin is absent
    // (a model earns its registry entry only by having an INDEXED run, so the
    // orphaned model drops out entirely once its sole run goes invisible).
    assert!(p1.registry.len() == 0);
    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// (b) bare skeleton next to a HEALTHY run: sibling indexes normally
// ---------------------------------------------------------------------------

#[test]
fn skeleton_run_is_noted_once_and_sibling_run_indexes_fine() {
    let root = "/tmp/opencode/wxpoint-missgrid2";
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "gfs_small.grib2"), &read_bytes("../../tests/fixtures/gfs_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, mg_cfg(&data, &ing).as_bytes());

    let p0_r = Pipeline::open(&cfg_path);
    let mut p0 = match p0_r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(p0.scan_round() == 0);
    assert!(p0.scan_round() == 1);
    drop(p0);

    // plant a BARE skeleton run dir (mkdir only — no grid.bin, no fields).
    let skel = format!("{0}/gfs/2026101913", data);
    let _m = create_dir_all(&skel);

    let p1_r = Pipeline::open(&cfg_path);
    let p1 = match p1_r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    // exactly ONE MISSING GRID note, naming the SKELETON only...
    assert!(count_notes(&p1, "MISSING GRID") == 1);
    let named_skel = match &p1.log {
        l => {
            let mut found = false;
            for line in l {
                if line.contains("MISSING GRID") && line.contains("2026101913") {
                    found = true;
                }
            }
            found
        }
    };
    assert!(named_skel);
    assert!(p1.counters.errors == 1);
    // ...while the HEALTHY sibling run rebuilds and indexes as always.
    assert!(p1.registry[0].runs.len() == 1);
    assert!(p1.registry[0].runs[0].dir_name == "2026101912".to_string());
    rm_tree(&root);
}
