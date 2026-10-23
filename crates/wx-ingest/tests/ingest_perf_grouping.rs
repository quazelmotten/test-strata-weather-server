// V1.2 phase B gate #2: WRITE-ONCE GROUPING AT PRODUCTION SCALE.
//
// One GFS forecast hour of the perf dataset (tests/perfdata/gfs_perf_f00*.grib2,
// 22 messages each: HGT x8 isobaric, TMP x8 isobaric, TMP@2 m, UGRD@10 m,
// VGRD@10 m, RH@2 m, PRMSL surface, GUST@10 m) must cost EIGHT field writes, not
// twenty-two, and the bytes written must stay within 1.1x of the final store size
// (pre-B, writing the same file after every message amplified plane bytes ~9.8x).
// Skips silently when the perf dataset was not generated on this machine.
//
// Single test in this binary: storage counters are process-wide atomics.

use wx_ingest::pipeline::Pipeline;
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

pub struct WalkAcc {
    pub field_files: usize,
    pub aux_writes: usize,
    pub bin_dirs: usize,
    pub bytes: u64,
}

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

fn walk_one(dir: &str) -> WalkAcc {
    let mut acc = WalkAcc { field_files: 0, aux_writes: 0, bin_dirs: 0, bytes: 0_u64 };
    walk_store(dir, &mut acc);
    acc
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

fn perf_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ndecode_batch_files = 4\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"gfs_perf_*.grib2\"\n\n  [[models.params]]\n  name = \"HGT\"\n  levels = [\"isobaric:*\"]\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\", \"isobaric:*\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"RH\"\n  levels = [\"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"PRMSL\"\n  levels = [\"surface\"]\n\n  [[models.params]]\n  name = \"GUST\"\n  levels = [\"height_above_ground:10\"]\n"
}

#[test]
fn perf_hour_costs_eight_group_writes_not_twenty_two() {
    // skip when the perf fixture was not generated on this machine
    if stat_size_mtime("../../tests/perfdata/gfs_perf_f000.grib2").is_err() { return; }
    if stat_size_mtime("../../tests/perfdata/gfs_perf_f006.grib2").is_err() { return; }

    let root = "/tmp/opencode/wxpoint-perfgroup".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, perf_cfg(&data, &ing).as_bytes());

    counters_reset();
    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(p.decode_batch_files == 4);

    // ---- forecast hour f000 -------------------------------------------------
    write_bytes(&join_path(&ing, "gfs_perf_f000.grib2"),
                &read_bytes("../../tests/perfdata/gfs_perf_f000.grib2"));
    let a0 = counters_snapshot();
    assert!(p.scan_round() == 0);   // observation round
    assert!(p.scan_round() == 1);   // ingest round
    let a1 = counters_snapshot();
    let r1_renames = a1.renames - a0.renames;
    let r1_fsyncs = a1.fsyncs - a0.fsyncs;

    // 8 GROUPS + grid.bin + manifest.json = 10 renames (pre-B: 22 field writes
    // + 22 manifest rewrites = 44).
    assert!(r1_renames == 10_u64);
    assert!(p.counters.groups_written == 8);

    // ---- forecast hour f006 (same run dir: grid.bin already present) ------
    write_bytes(&join_path(&ing, "gfs_perf_f006.grib2"),
                &read_bytes("../../tests/perfdata/gfs_perf_f006.grib2"));
    let b0 = counters_snapshot();
    assert!(p.scan_round() == 0);
    assert!(p.scan_round() == 1);
    let b1 = counters_snapshot();
    let r2_renames = b1.renames - b0.renames;
    assert!(r2_renames == 9_u64);      // 8 groups + ONE manifest refresh
    assert!(p.counters.groups_written == 16);
    assert!(p.counters.errors == 0);

    // ---- final store shape + byte amplification bound ---------------------
    let acc = walk_one(&join_path(&data, "gfs"));
    assert!(acc.field_files == 16);      // 8 param/level dirs x 2 forecast hours
    assert!(acc.bin_dirs == 8);
    assert!(acc.aux_writes == 2);        // grid.bin + ONE current manifest
    let total_bytes = b1.bytes_written - a0.bytes_written;
    assert!(total_bytes <= 11_u64 * acc.bytes / 10_u64);

    println!("MEASURE gfs_perf renames_f000={0} renames_f006={1} bytes={2} fsyncs_f000={3} store_bytes={4} field_files={5}",
             r1_renames, r2_renames, total_bytes, r1_fsyncs, acc.bytes, acc.field_files);

    // live mirror reflects the same totals (phase B: per-FILE updates)
    let lv = p.live.snapshot();
    assert!(lv.groups_written == 16);
    assert!(lv.ingested_files == 2);

    rm_tree(&root);
}
