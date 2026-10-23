// V1.2 phase B gate #4: WRITE FAILURE PROPAGATION (simulated ENOSPC).
//
// When a group's atomic write cannot happen (here: the level directory is made
// read-only with libc `chmod`, since this std has no permission API), the pipeline
// must:
//   * keep the OTHER groups of the same file intact (per-group error collection),
//   * NOT mark the source file as ingested (no state upsert) so the NEXT round
//     retries the whole file idempotently,
//   * count EXACTLY ONE error for the file-with-failure (not one per group),
//   * leave NO `.tmp.` litter anywhere (phase-A unlink-on-failure),
//   * leave the published index free of the unwritable field.
// After the directory is writable again the retry must succeed and answer queries.
//
// Single test in this binary: counter assertions are process-wide.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode};
use wx_core::tables::{lookup_level_type};
use wx_core::storage::{counters_reset};
use wx_ingest::state::{join_path, stat_size_mtime};
use wx_ingest::discovery::{path_to_str, base_name};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

// libc chmod (same binding style as wx-core's durability tests).
extern "C" {
    #[link_name = "chmod"]
    fn libc_chmod(path: *const i8, mode: u32) -> i32;
}

fn c_str(s: &str) -> Vec<i8> {
    let mut buf: Vec<i8> = Vec::<i8>::new();
    for b in s.as_bytes().to_vec() {
        buf.push(b as i8);
    }
    buf.push(0);
    buf
}

fn fixture(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

fn read_bytes(path: &str) -> Vec<u8> {
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

fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!();}
        Ok(mut w) => {
            let n = w.write_all(bytes);
            if n.is_err() { panic!(); }
        }
    }
}

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

/// Count entries whose name contains ".tmp." anywhere under `dir`.
fn count_tmp_litter(dir: &str) -> usize {
    let mut hits = 0;
    match read_dir(dir) {
        Err(_) => { return hits; }
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let p = path_to_str(&de.path());
                        let nm = base_name(p.to_string());
                        if nm.contains(".tmp.") {
                            hits += 1;
                        }
                        match read_dir(&p) {
                            Err(_) => {}
                            Ok(_) => {
                                hits += count_tmp_litter(&p);
                            }
                        }
                    }
                }
            }
        }
    }
    hits
}

fn grouping_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n"
}

#[test]
fn unwritable_group_dir_fails_once_and_retries_cleanly() {
    let root = "/tmp/opencode/wxpoint-diskfull".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    let src = join_path(&ing, "gfs_small.grib2");
    write_bytes(&src, &read_bytes(&fixture("gfs_small.grib2")));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, grouping_cfg(&data, &ing).as_bytes());

    // Pre-create the level directory that will be made read-only. Run dir of
    // the fixture is 2026101912; TMP/isobaric is one of its GROUP targets.
    let run_dir = join_path(&join_path(&data, "gfs"), "2026101912");
    let lvl_dir = join_path(&join_path(&run_dir, "TMP"), "isobaric");
    let _m1 = create_dir_all(&lvl_dir);
    let ro = c_str(&lvl_dir);
    let cm_ro = unsafe { libc_chmod(&ro[0], 0o555) };
    assert!(cm_ro == 0);

    counters_reset();
    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };

    assert!(p.scan_round() == 0);   // observation round
    assert!(p.scan_round() == 0);   // attempted: groups fail -> NOT ingested

    // ---- exactly ONE error for the FILE, not one per failed group. V1.3
    // phase E3b adds a SECOND, DISTINCT error source here: this test pre-
    // creates the run dir skeleton (no grid.bin) before Pipeline::open, and
    // boot now LOUDLY reports a missing grid.bin instead of returning silently
    // (audit defect 2) — so total errors == 1 (boot MISSING GRID note for the
    // planted skeleton) + 1 (this file's group-write failure). Assert both
    // explicitly rather than loosening the original intent.
    assert!(count_notes(&p, "MISSING GRID") == 1);
    assert!(p.counters.errors == 2);
    assert!(count_notes(&p, "group write failed") >= 1);

    // ---- the source file is NOT marked ingested -> next round retries ----
    if p.state.find("gfs", src.as_str()).is_some() { panic!(); }

    // ---- no temp litter, and the unwritable field really is absent ------
    assert!(count_tmp_litter(&data) == 0);
    if stat_size_mtime(&join_path(&lvl_dir, "f000.bin")).is_ok() { panic!(); }
    // the writable groups (TMP/height_above_ground) did land: partial progress
    // is allowed because each GROUP is atomic on its own.
    let agl_dir = join_path(&join_path(&run_dir, "TMP"), "height_above_ground");
    if stat_size_mtime(&join_path(&agl_dir, "f000.bin")).is_err() { panic!(); }

    // ---- index must not expose the unwritable field ---------------------
    let snap = p.shared.get_index();
    let sref = &*snap;
    match lookup_level_type("isobaric") {
        None => { panic!(); }
        Some(iso) => {
            let c_r = build_context(sref, "gfs", None, "TMP", iso, 850.0, 0.0,
                                    TimeInterpMode::Linear);
            match c_r {
                Ok(_) => { panic!(); }
                Err(_) => {
                    // expected: the unwritable level is simply NOT in the index
                }
            }
        }
    }

    // ---- restore writability: the retry succeeds and answers -----------
    let rw = c_str(&lvl_dir);
    let cm_rw = unsafe { libc_chmod(&rw[0], 0o755) };
    assert!(cm_rw == 0);
    assert!(p.scan_round() == 1);   // retried IN FULL, idempotent merge
    assert!(p.counters.errors == 2); // no NEW error (baseline: boot MISSING GRID + group failure)
    assert!(p.counters.ingested_files == 1);

    let snap2 = p.shared.get_index();
    let s2 = &*snap2;
    match lookup_level_type("isobaric") {
        None => { panic!(); }
        Some(iso) => {
            let c_r = build_context(s2, "gfs", None, "TMP", iso, 850.0, 21600.0,
                                    TimeInterpMode::Linear);
            match c_r {
                Err(_) => { panic!(); }
                Ok(c) => {
                    let v = evaluate_point(&c, 9.0, 351.5);
                    match v {
                        Err(_) => { panic!(); }
                        Ok(pv) => { assert!(pv.value == 421.0); }
                    }
                }
            }
        }
    }
    assert!(count_tmp_litter(&data) == 0);

    rm_tree(&root);
}

fn count_notes(p: &Pipeline, marker: &str) -> usize {
    let mut hits = 0;
    for l in &p.log {
        if l.contains(marker) {
            hits += 1;
        }
    }
    hits
}
