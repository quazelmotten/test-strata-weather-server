// V1.2 phase C gate #1: RETENTION-AWARE SKIP BEFORE DECODE (spec §4.6 efficiency
// under the §4.2 "never trust filenames" rule).
//
// A candidate whose SECTION-1 REFERENCE TIME is STRICTLY OLDER than the oldest
// run the registry keeps — while the registry is ALREADY FULL — could only ever
// produce a run that `retention_prune` evicts again in the SAME round. It is
// therefore vetoed by a cheap header peek BEFORE the magic-byte precheck, the
// rayon decode AND any write. ONE behavior per test; counter-sensitive gates get
// their OWN binary because RENAMES_TOTAL is process-wide while cargo runs tests
// inside one binary concurrently.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use wx_core::storage::{counters_snapshot};
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

// sec1 date-stamp patcher, identical to the verified pattern in retention.rs
// (12-byte pattern, day byte at offset 10, exactly 39 hits in gfs_small.grib2).
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

// retention_runs = 2 so TWO delivered runs already FILL the registry.
fn cfg_text(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 2\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

#[allow(clippy::needless_return)]
fn has_record_for(p: &Pipeline, path: String) -> bool {
    match p.state.find(path.clone()) {
        None => { return false; }
        Some(_) => { return true; }
    }
}

#[allow(clippy::needless_return)]
fn note_count(p: &Pipeline, marker: &str) -> usize {
    let mut n = 0;
    for line in &p.log {
        if line.contains(marker) {
            n += 1;
        }
    }
    return n;
}

#[test]
fn stale_run_arriving_late_is_skipped_without_decode_or_write() {
    let root = "/tmp/opencode/wxpoint-rskip".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    let base = read_bytes(&fixture("gfs_small.grib2"));
    assert!(base.len() > 0);
    // two runs that FILL retention_runs=2: day 21 and day 22
    write_bytes(&join_path(&ing, "gfs_b.grib2"), &patch_day(&base, 21));
    write_bytes(&join_path(&ing, "gfs_c.grib2"), &patch_day(&base, 22));

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg_text(&data, &ing).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    let _r1 = p.scan_round(); // observation round
    let r2 = p.scan_round();
    assert!(r2 == 2); // registry now holds exactly the 2 kept runs
    assert!(p.counters.errors == 0);

    // ---- an OLDER run arrives late ------------------------------------------
    let old_path = join_path(&ing, "gfs_a.grib2");
    write_bytes(&old_path, &patch_day(&base, 19));
    let _r3 = p.scan_round(); // stability first sighting only

    let c_before = counters_snapshot();
    let r4 = p.scan_round();
    let c_after = counters_snapshot();

    // (a) NOT ingested, and NOTHING AT ALL was written for it: zero renames.
    assert!(r4 == 0);
    assert!((c_after.renames - c_before.renames) == 0);
    assert!((c_after.bytes_written - c_before.bytes_written) == 0);
    // (b) skipped counter bumped ONCE, and never as an error
    assert!(p.counters.retention_skipped_files == 1);
    assert!(p.counters.errors == 0);
    // (c) noted loudly
    assert!(note_count(&p, "retention skip") == 1);
    // (d) NO persistent record is written for a skipped file: the verdict is
    // re-derived every round against the CURRENT registry, so the file stays
    // eligible should the newer runs disappear upstream later.
    assert!(!has_record_for(&p, old_path));
    // (e) no run tree for the stale reference time exists on disk
    let gone = read_bytes(&join_path(&join_path(&join_path(&data, "gfs"), "2026101912"), "grid.bin"));
    assert!(gone.len() == 0);

    // ---- deduped: another round re-derives the SAME verdict WITHOUT counting
    let r5 = p.scan_round();
    assert!(r5 == 0);
    assert!(p.counters.retention_skipped_files == 1);
    assert!(note_count(&p, "retention skip") == 1);

    // ---- a NEWER run still ingests normally afterwards ---------------------
    write_bytes(&join_path(&ing, "gfs_d.grib2"), &patch_day(&base, 23));
    let _r6 = p.scan_round();
    let r7 = p.scan_round();
    assert!(r7 == 1);
    assert!(p.counters.ingested_files == 3);
    assert!(p.counters.retention_skipped_files == 1);
    let gb = read_bytes(&join_path(&join_path(&join_path(&data, "gfs"), "2026102312"), "grid.bin"));
    assert!(gb.len() >= 12);

    rm_tree(&root);
}
