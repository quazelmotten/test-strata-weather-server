// Spec §4.1 file stability: (a) WINDOW strategy — a half-written file must be
// SKIPPED before decoding (errors counter stays 0), and once completed it
// becomes eligible after the window has elapsed; (b) SIDECAR strategy — a
// model with sidecar_suffix ingests as soon as <file><suffix> appears, no
// window needed.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode};
use wx_core::tables::{lookup_level_type};
use wx_ingest::state::{join_path, now_ns_marker};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

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

fn cfg(data: &str, ing: &str, window_secs: u32, sidecar: bool) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = ";
    let t = format!("{0}{1}", t, window_secs);
    let t = t + "\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    if sidecar {
        let t2 = t + "\"\nsidecar_suffix = \".done\"\nfile_glob = \"gfs_sc.grib2\"";
        let t = t2 + "\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
        return t;
    }
    let t = t + "\"\nfile_glob = \"gfs_half.grib2\"";
    let t = t + "\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn window_strategy_skips_half_written() {
    let root = "/tmp/opencode/wxpoint-stab".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    let full = read_bytes(&fixture("gfs_small.grib2"));
    let half = (full.len() / 2);
    let part = join_path(&ing, "gfs_half.grib2");
    write_bytes(&part, &full[0 .. half]);

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg(&data, &ing, 1, false).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };

    // Round 1: first observation only.
    let r1 = p.scan_round();
    assert!(r1 == 0);
    // Round 2: stat stable but window (1 s) not elapsed yet -> still skipped,
    // AND never decoded (the truncated file would fail: errors stays 0).
    let r2 = p.scan_round();
    assert!(r2 == 0);
    assert!(p.counters.errors == 0);

    // Complete the delivery (size+mtime change resets the tracker).
    write_bytes(&part, &full);
    let r3 = p.scan_round();
    assert!(r3 == 0);

    // Busy-wait on the marker clock until >1 s has elapsed since round 3.
    let t0 = now_ns_marker(&p.state.dir_path);
    let one_s = 1000000000;
    while now_ns_marker(&p.state.dir_path) - t0 < one_s {
        // spin: each call re-creates the marker (advances the clock)
    }
    let r4 = p.scan_round();
    assert!(r4 == 1);
    assert!(p.counters.errors == 0);

    // Queryable and correct.
    let snap = p.shared.get_index();
    let sref = &*snap;
    match lookup_level_type("isobaric") {
        None => { assert!(false); return; }
        Some(iso) => {
            match sref.find_model("gfs") {
                None => { assert!(false); return; }
                Some(_m) => {
                    let c_r = build_context(sref, "gfs", None, "TMP", iso,
                                         1000.0, 0.0, TimeInterpMode::Linear);
                    match c_r {
                        Err(_) => { assert!(false); return; }
                        Ok(c) => {
                            let v = evaluate_point(&c, 10.0, 350.0);
                            match v {
                                Err(_) => { assert!(false); }
                                Ok(pv) => { assert!(pv.value == 250.0); }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn sidecar_strategy_eligible_immediately() {
    let root = "/tmp/opencode/wxpoint-stab-sc".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    write_bytes(&join_path(&ing, "gfs_sc.grib2"), &read_bytes(&fixture("gfs_small.grib2")));

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg(&data, &ing, 10, true).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };

    // No sidecar yet: even a complete file is NOT eligible (window irrelevant).
    let r1 = p.scan_round();
    assert!(r1 == 0);

    // Upstream writes the marker: eligible on the NEXT round, immediately.
    write_bytes(&join_path(&ing, "gfs_sc.grib2.done"), &[46]);
    let r2 = p.scan_round();
    assert!(r2 == 1);
    assert!(p.counters.errors == 0);

    let snap = p.shared.get_index();
    let sref = &*snap;
    match sref.find_model("gfs") {
        None => { assert!(false); return; }
        Some(m) => {
            assert!(m.runs.len() == 1);
        }
    }
}
