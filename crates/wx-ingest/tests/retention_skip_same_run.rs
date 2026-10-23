// V1.2 phase C gate #4: EQUALITY IS NEVER SKIPPED (spec §4.6 + §4.2).
//
// The retention-skip rule uses a STRICT comparison: a candidate is obsolete only
// when its newest SECTION-1 reference time is OLDER than the oldest kept run.
// EQUAL means the file belongs to a run that is ALREADY KEPT — the classic case
// being a partially delivered product re-delivered, or a second file of the SAME
// run (the `_c3` packing twin of the fixture, identical run and levels, different
// data representation). Skipping such a file would silently DROP the rest of a
// run the user can already query, i.e. data loss dressed up as an optimization.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path, REC_OK};
use wx_ingest::discovery::{path_to_str};
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

// retention_runs = 1 so ONE ingested run already FILLS the registry: every later
// candidate is peeked and judged.
fn one_run_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 1\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n"
}

#[test]
fn same_run_second_file_is_never_skipped_and_merges_into_the_kept_run() {
    let root = "/tmp/opencode/wxpoint-samerun".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    // run 2026-10-19T12Z fills the registry
    write_bytes(&join_path(&ing, "gfs_small.grib2"), &read_bytes(&fixture("gfs_small.grib2")));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, one_run_cfg(&data, &ing).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(p.scan_round() == 0);
    assert!(p.scan_round() == 1);
    assert!(p.counters.retention_skipped_files == 0);

    // ---- SAME reference time arrives late (the complex-packing twin) ------
    let twin = join_path(&ing, "gfs_small_c3.grib2");
    write_bytes(&twin, &read_bytes(&fixture("gfs_small_c3.grib2")));
    assert!(p.scan_round() == 0); // observation
    // EQUAL reference time => NOT strictly older => MUST be decoded.
    assert!(p.scan_round() == 1);
    assert!(p.counters.retention_skipped_files == 0);
    assert!(p.counters.errors == 0);
    assert!(p.counters.ingested_files == 2);
    match p.state.find("gfs", twin.as_str()) {
        None => { panic!(); }
        Some(r) => { assert!(r.kind == REC_OK); }
    }

    // both files fed the SAME kept run tree (merge dedup, phase A/B), which is
    // still queryable through the published index.
    let gb = read_bytes(&join_path(&join_path(&join_path(&data, "gfs"), "2026101912"), "grid.bin"));
    assert!(gb.len() >= 12);

    // ---- a re-delivery of an ALREADY-ingested file also stays unskipped ---
    // (identical content hash => skipped by the §4.1 unchanged-record rule,
    // which is a DIFFERENT mechanism from the retention skip: no counter moves)
    assert!(p.scan_round() == 0);
    assert!(p.counters.retention_skipped_files == 0);
    assert!(p.counters.ingested_files == 2);

    rm_tree(&root);
}
