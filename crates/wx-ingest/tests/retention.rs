// Spec §4.6 retention: runs beyond retention_runs are removed from the
// in-memory snapshot FIRST (arc-swap publish), and only THEN is the tree
// deleted. A stale snapshot still held by a reader must keep answering from
// the unlinked mmapped files (Arc drop does the unmapping, spec §5).
// Extra runs are produced by byte-patching the reference date in sec1 of
// gfs_small.grib2 (day 19 -> 20/21/22), verified against wgrib2 semantics.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::pipeline::{build_snapshot};
use wx_ingest::index::{PointQueryContext, evaluate_point, SelectError};
use wx_core::interp::{TimeInterpMode};
use wx_core::tables::{lookup_level_type};
use wx_ingest::grib2::{epoch_secs};
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

fn fixture(name: &str) -> String {
    return format!("../../tests/fixtures/{name}");
}

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

fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(bytes);
            match n {
                Err(_) => { assert!(false); }
                Ok(_) => {}
            }
        }
    }
}

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

// sec1 date stamp pattern present once per record (39 records in gfs_small);
// day byte sits at offset 10 of the pattern (verified: 39 occurrences).
fn patch_day(src: &[u8], day: u8) -> Vec<u8> {
    let mut out = src.to_vec();
    let pat: [u8; 12] = [0x00, 0x07, 0x00, 0x00, 0x7D, 0x00, 0x01, 0x07, 0xEA, 0x0A, 0x13, 0x0C];
    let n = out.len();
    let mut hits = 0;
    for k in 0 .. (n - 12 + 1) {
        let mut ok = true;
        for j in 0 .. 12 {
            if out[k + j] != pat[j] {
                ok = false;
                break;
            }
        }
        if ok {
            out[k + 10] = day;
            hits += 1;
        }
    }
    assert!(hits == 39);
    return out;
}

fn cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 3\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn retention_evicts_after_publish_stale_snapshot_still_reads() {
    let root = "/tmp/opencode/wxpoint-retention".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    let base = read_bytes(&fixture("gfs_small.grib2"));
    // run A: 2026-10-19T12Z (unmodified), B: day 20, C: day 21.
    write_bytes(&join_path(&ing, "gfs_a.grib2"), &base);
    write_bytes(&join_path(&ing, "gfs_b.grib2"), &patch_day(&base, 20));
    write_bytes(&join_path(&ing, "gfs_c.grib2"), &patch_day(&base, 21));

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg(&data, &ing).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };

    let _ = p.scan_round();
    let n2 = p.scan_round();
    assert!(n2 == 3);
    assert!(p.counters.errors == 0);

    // Snapshot BEFORE the eviction: three runs, all queryable.
    let snap_old = p.shared.get_index();
    let sref_old = &*snap_old;
    let gm = sref_old.find_model("gfs");
    match gm {
        None => { assert!(false); return; }
        Some(m) => { assert!(m.runs.len() == 3); }
    }

    // Fourth run D arrives later (day 22): written AFTER round 2 so only it
    // is new in rounds 3/4.
    write_bytes(&join_path(&ing, "gfs_d.grib2"), &patch_day(&base, 22));
    let _ = p.scan_round(); // observes D (stability first sighting)
    let n4 = p.scan_round();
    assert!(n4 == 1);

    // Current snapshot: retention keeps the 3 newest (B,C,D); A evicted.
    let snap_new = p.shared.get_index();
    let sref_new = &*snap_new;
    let gm2 = sref_new.find_model("gfs");
    match gm2 {
        None => { assert!(false); return; }
        Some(m) => {
            assert!(m.runs.len() == 3);
            assert!(m.runs[0].dir_name == "2026102012".to_string());
        }
    }
    // The victim tree is gone from disk (deleted only AFTER the swap).
    let gone = read_bytes(&join_path(&join_path(&join_path(&data, "gfs"), "2026101912"), "grid.bin"));
    assert!(gone.len() == 0);

    // The stale snapshot still answers through the UNLINKED mmaps (§4.6).
    let epA = epoch_secs(2026, 10, 19, 12, 0, 0);
    match lookup_level_type("isobaric") {
        None => { assert!(false); return; }
        Some(iso) => {
            match gm2 {
                None => { assert!(false); return; }
                Some(m) => {
                    let seed = PointQueryContext {
                        grid: m.runs[0].grid.clone(),
                        units: "".to_string(),
                        run_epoch_secs: 0,
                        stat_processed: false,
                        want_level: 0.0,
                        want_fh_secs: 0.0,
                        fh_secs: (0.0, 0.0),
                        slots: [None, None],
                    };
                    let c_r = seed.build(sref_old, "gfs", Some(epA), "TMP", iso,
                                         1000.0, 0.0, TimeInterpMode::Linear);
                    match c_r {
                        Err(_) => { assert!(false); return; }
                        Ok(c) => {
                            let v = evaluate_point(&c, 10.0, 350.0);
                            match v {
                                Err(_) => { assert!(false); return; }
                                Ok(pv) => { assert!(pv.value == 250.0); }
                            }
                        }
                    }
                    // The NEW snapshot can no longer pin the evicted run.
                    let cn = seed.build(sref_new, "gfs", Some(epA), "TMP", iso,
                                        1000.0, 0.0, TimeInterpMode::Linear);
                    match cn {
                        Err(e2) => {
                            match e2 {
                                SelectError::NoRunCovers => {}
                                _ => { assert!(false); }
                            }
                        }
                        Ok(_) => { assert!(false); }
                    }
                    // Unpinned queries on the new snapshot answer from D.
                    let cd = seed.build(sref_new, "gfs", None, "TMP", iso,
                                        1000.0, 0.0, TimeInterpMode::Linear);
                    match cd {
                        Err(_) => { assert!(false); return; }
                        Ok(c) => {
                            let v = evaluate_point(&c, 10.0, 350.0);
                            match v {
                                Err(_) => { assert!(false); return; }
                                Ok(pv) => { assert!(pv.value == 250.0); }
                            }
                        }
                    }
                }
            }
        }
    }

    rm_tree(&root);
}
