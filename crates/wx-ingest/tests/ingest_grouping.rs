// V1.2 phase B gate #1: WRITE-ONCE GROUPING.
//
// ONE source file's messages are grouped per (run, param, level type, forecast
// hour) and each group is merged + written ONCE (`merge_write_field_batch`), so
// the number of renames equals (#groups + grid.bin + manifest.json) instead of
// (#messages + #messages), the BYTES_WRITTEN_TOTAL delta equals the final on-disk
// store size EXACTLY (zero write amplification), and directory fsyncs happen once
// per DISTINCT directory (TouchedDirs) rather than once per rename.
//
// Fixture inventory (tests/fixtures/gfs_small.grib2, wgrib2-derived): 39 messages
// = TMP{1000,850,700 mb, 2 m AGL, 10 m AGL} x {f000,f006,f012} plus UGRD/VGRD
// {1000,850,700 mb, 10 m AGL} x the same three forecast hours. With TMP configured
// without 10 m AGL, 36 messages are stored and the expected GROUP count is
//   per forecast hour: TMP/isobaric(3 levels) + TMP/2m + UGRD/isobaric(3)
//                      + UGRD/10m + VGRD/isobaric(3) + VGRD/10m  = 6 groups
//   x 3 forecast hours = 18 groups, i.e. 18 field files for 36 stored messages.
//
// Single test in this binary: storage counters are process-wide atomics.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_core::tables::{lookup_level_type};
use wx_core::storage::{counters_snapshot, counters_reset};
use wx_ingest::state::{join_path, stat_size_mtime};
use wx_ingest::discovery::{path_to_str, base_name};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

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

/// Accumulator for the store walk.
pub struct WalkAcc {
    pub field_files: usize, // f*.bin (one per GROUP)
    pub aux_writes: usize,  // grid.bin + manifest.json
    pub bin_dirs: usize,    // DISTINCT directories holding >=1 field file
    pub bytes: u64,         // every byte of the model's store tree
}

/// Recursive walk of one model's store directory.
fn walk_store(dir: &str, acc: &mut WalkAcc) {
    let mut subdirs: Vec<String> = Vec::<String>::new();
    let mut local_bins: usize = 0;
    match read_dir(dir) {
        Err(_) => { return; }
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let p = path_to_str(&de.path());
                        match read_dir(&p) {
                            Err(_) => {
                                // a FILE: classify by name, add its size
                                let nm = base_name(p.to_string());
                                let sz = match stat_size_mtime(&p) {
                                    Err(_) => { 0_u64 }
                                    Ok(v0) => { v0.0 }
                                };
                                acc.bytes += sz;
                                if nm == "grid.bin" || nm == "manifest.json" {
                                    acc.aux_writes += 1;
                                } else {
                                    acc.field_files += 1;
                                    local_bins += 1;
                                }
                            }
                            Ok(_) => {
                                subdirs.push(p.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    if local_bins > 0 {
        acc.bin_dirs += 1;
    }
    for s in &subdirs {
        walk_store(s.as_str(), &mut *acc);
    }
}

fn grouping_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"isobaric:*\", \"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"isobaric:*\", \"height_above_ground:10\"]\n"
}

#[test]
fn one_file_writes_groups_once_not_messages() {
    let root = "/tmp/opencode/wxpoint-grouping".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "gfs_small.grib2"),
                &read_bytes(&fixture("gfs_small.grib2")));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, grouping_cfg(&data, &ing).as_bytes());

    counters_reset();
    let c0 = counters_snapshot();

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    // stable_window_secs = 0: round 1 OBSERVES, round 2 INGESTS.
    assert!(p.scan_round() == 0);
    assert!(p.scan_round() == 1);

    let c1 = counters_snapshot();
    let renames = c1.renames - c0.renames;
    let bytes = c1.bytes_written - c0.bytes_written;
    let fsyncs = c1.fsyncs - c0.fsyncs;

    // ---- what is actually on disk ------------------------------------------
    let mdl_dir = join_path(&data, "gfs");
    let acc = walk_one(&mdl_dir);
    assert!(acc.field_files == 18);
    assert!(acc.aux_writes == 2);   // grid.bin + manifest.json
    assert!(acc.bin_dirs == 6);     // TMP/{iso,agl}, UGRD/{iso,agl}, VGRD/{iso,agl}

    // ---- RENAMES: groups + grid.bin + manifest, NOT messages --------------
    // Pre-B this file cost 36 field renames + 36 manifest renames = 72.
    assert!(renames == 18_u64 + 2_u64);
    assert!(renames < 39_u64);

    // ---- BYTES: ZERO amplification (each byte of the store written ONCE) --
    assert!(bytes == acc.bytes);
    assert!(bytes <= 11_u64 * acc.bytes / 10_u64);

    println!("MEASURE renames={0} bytes={1} fsyncs={2} field_files={3} aux={4} bin_dirs={5}", renames, bytes, fsyncs, acc.field_files, acc.aux_writes, acc.bin_dirs);
    // ---- FSYNCS: 1 per field inode + 2 per aux writer + 1 per DISTINCT dir
    // (TouchedDirs.flush) + 2 per STATE SAVE (files.json + parent dir). V1.3
    // phase E3a changed the save CADENCE from per-file to per-chunk plus a
    // dirty-gated round end: in this measured window (one file, two rounds)
    // exactly ONE save now happens — the chunk-end persist of the ingest
    // round. The observation round writes nothing (store never dirty) and the
    // round-end offer is a no-op once the chunk already flushed. Pre-E3a this
    // was 3 full rewrites (per-file + per-round). Durability contract after
    // E3a: a crash loses at most one unfinished chunk's records, which
    // re-ingest idempotently (see restart_midchunk.rs + docs/DECISIONS.md).
    let state_saves: u64 = 1;
    assert!(fsyncs == 18_u64 + 2_u64 + 2_u64 + 6_u64 + 2_u64 * state_saves);

    // ---- counters + live mirror agree --------------------------------------
    assert!(p.counters.groups_written == 18);
    assert!(p.counters.errors == 0);
    assert!(p.counters.ignored_files == 0);
    assert!(p.counters.quarantined_files == 0);
    let lv = p.live.snapshot();
    assert!(lv.groups_written == 18);
    assert!(lv.ingested_files == 1);

    // ---- correctness is untouched by the batching -------------------------
    // TMP@850 f006 at node j=2,i=3 (k=85, unmasked) = 250+100+60+8+3 = 421.
    let snap = p.shared.get_index();
    let sref = &*snap;
    match lookup_level_type("isobaric") {
        None => { panic!(); }
        Some(iso) => {
            let c_r = build_context(sref, "gfs", None, "TMP", iso, 850.0, 21600.0,
                                    TimeInterpMode::Linear);
            match c_r {
                Err(_) => { panic!(); }
                Ok(c) => {
                    let v = evaluate_point(&c, 9.0, 351.5);
                    match v {
                        Err(_) => { panic!(); }
                        Ok(pv) => {
                            assert!(pv.value == 421.0);
                            assert!(pv.info.horizontal == HorizMethod::Exact);
                        }
                    }
                }
            }
        }
    }
    // UGRD/VGRD pairs were grouped and written independently (regular GFS grid
    // -> components stored as-is, no rotation).
    match sref.find_model("gfs") {
        None => { panic!(); }
        Some(m) => {
            assert!(m.runs[0].fields.len() == 6);
        }
    }

    rm_tree(&root);
}

fn walk_one(dir: &str) -> WalkAcc {
    let mut acc = WalkAcc { field_files: 0, aux_writes: 0, bin_dirs: 0, bytes: 0_u64 };
    walk_store(dir, &mut acc);
    acc
}
