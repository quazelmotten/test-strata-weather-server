// Spec §4.1 file stability: (a) WINDOW strategy — a half-written file must be
// SKIPPED before decoding (errors counter stays 0), and once completed it
// becomes eligible after the window has elapsed; (b) SIDECAR strategy — a
// model with sidecar_suffix ingests as soon as <file><suffix> appears, no
// window needed.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode};
use wx_core::tables::{lookup_level_type};
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::thread;
use std::time::Duration;

// Bounded, SLEEPING wait helper (V1.2 phase B0): replaces the unbounded busy-spin
// that could hang the suite when the wall-clock marker stopped advancing.
#[allow(clippy::needless_return)]
pub fn thread_sleep_ms(ms: i64) {
    let d = Duration::from_millis(ms as u64);
    thread::sleep(d);
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
    
    t + "\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n"
}

#[test]
fn window_strategy_skips_half_written() {
    let root = "/tmp/opencode/wxpoint-stab".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    let full = read_bytes(&fixture("gfs_small.grib2"));
    let half = full.len() / 2;
    let part = join_path(&ing, "gfs_half.grib2");
    write_bytes(&part, &full[0 .. half]);

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg(&data, &ing, 1, false).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
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

    // Bounded wait on the MONOTONIC clock (V1.2 phase B0): the pre-fix version
    // spun on the marker-file wall clock with NO deadline and NO sleep, so a
    // non-advancing clock (File::create failure returns 0, coarse mtime
    // granularity, backward jump) hung the test FOREVER. Now: sleep between
    // polls, hard deadline, and an explicit assertion that the deadline was not
    // hit. The window is measured from the first observation of the COMPLETED
    // file (recorded inside round 3), so waiting >= window + margin from HERE
    // guarantees eligibility in round 4.
    let base = p.clock.elapsed_ns();
    let one_s = 1000000000i64;
    let margin = 200000000i64; // 200 ms slack for scheduling jitter
    let deadline_ms = 10000i64;
    let mut waited_ms = 0i64;
    while p.clock.elapsed_ns() - base < one_s + margin {
        thread_sleep_ms(50);
        waited_ms += 50;
        if waited_ms >= deadline_ms {
            break;
        }
    }
    assert!(waited_ms < deadline_ms); // never spins forever
    let r4 = p.scan_round();
    assert!(r4 == 1);
    assert!(p.counters.errors == 0);

    // Queryable and correct.
    let snap = p.shared.get_index();
    let sref = &*snap;
    match lookup_level_type("isobaric") {
        None => { panic!();}
        Some(iso) => {
            match sref.find_model("gfs") {
                None => { panic!();}
                Some(_m) => {
                    let c_r = build_context(sref, "gfs", None, "TMP", iso,
                                         1000.0, 0.0, TimeInterpMode::Linear);
                    match c_r {
                        Err(_) => { panic!();}
                        Ok(c) => {
                            let v = evaluate_point(&c, 10.0, 350.0);
                            match v {
                                Err(_) => { panic!(); }
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
        Err(_) => { panic!(); }
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
        None => { panic!();}
        Some(m) => {
            assert!(m.runs.len() == 1);
        }
    }
}
