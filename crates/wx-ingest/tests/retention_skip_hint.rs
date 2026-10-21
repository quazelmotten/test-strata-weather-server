// V1.2 phase C gate #3: THE FILENAME RUN HINT IS ORDERING ONLY (spec §4.2
// "never trust filenames for metadata" vs §4.6 "skip before decoding").
//
// Candidates are sorted NEWEST-HINT-FIRST so a delivered burst fills the registry
// with its newest runs BEFORE older candidates are judged (maximizing how many
// files are vetoed without ever being decoded). The SKIP VERDICT ITSELF comes
// SOLELY from the SECTION-1 reference time read out of the FILE. These two
// tests push the hint in BOTH wrong directions. Per-Pipeline counters only, so
// both behaviours share this binary.

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

// sec1 date-stamp patcher (verified pattern: day byte at offset 10, 39 hits).
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

fn cfg_n(data: &str, ing: &str, keep: usize) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = ";
    let t = format!("{0}{1}", t, keep);
    let t = t + "\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

#[allow(clippy::needless_return)]
fn has_grid(data: &str, run_dir: &str) -> bool {
    let model_dir = join_path(data, "gfs");
    let run_dir_full = join_path(&model_dir, run_dir);
    let probe = join_path(&run_dir_full, "grid.bin");
    return read_bytes(&probe).len() > 0;
}

#[test]
fn hint_says_old_run_content_is_new_run_so_it_is_still_ingested() {
    let root = "/tmp/opencode/wxpoint-hint-old".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    let base = read_bytes(&fixture("gfs_small.grib2"));
    assert!(base.len() > 0);
    // NAMES claim Oct 19 / 20 / 21; CONTENTS are Oct 24 / 25 / 23.
    write_bytes(&join_path(&ing, "gfs_2026101912.grib2"), &patch_day(&base, 24));
    write_bytes(&join_path(&ing, "gfs_2026102012.grib2"), &patch_day(&base, 25));
    write_bytes(&join_path(&ing, "gfs_2026102112.grib2"), &patch_day(&base, 23));

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg_n(&data, &ing, 2).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(p.scan_round() == 0); // observation round
    // NEWEST-HINT-FIRST order is content-23, content-25, content-24: the registry
    // is only FULL when the LAST file is judged, and its SECTION-1 time (Oct 24)
    // is NOT older than the oldest kept run (Oct 23) -> nothing may be skipped.
    assert!(p.scan_round() == 3);
    assert!(p.counters.retention_skipped_files == 0);
    assert!(p.counters.errors == 0);

    // The run that SURVIVES retention is decided by CONTENT: newest two are
    // Oct 25 and Oct 24; the content-Oct-23 run is evicted. Had the NAME been
    // trusted, the Oct-24 data would never have existed on disk at all.
    assert!(has_grid(&data, "2026102512"));
    assert!(has_grid(&data, "2026102412"));
    assert!(!has_grid(&data, "2026102312"));
    assert!(!has_grid(&data, "2026101912")); // no name-derived directory ever appears
    rm_tree(&root);
}

#[test]
fn hint_says_new_run_content_is_old_run_so_it_is_skipped() {
    let root = "/tmp/opencode/wxpoint-hint-new".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    let base = read_bytes(&fixture("gfs_small.grib2"));
    write_bytes(&join_path(&ing, "gfs_small.grib2"), &base); // run Oct 19 fills it

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg_n(&data, &ing, 1).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(p.scan_round() == 0);
    assert!(p.scan_round() == 1);
    assert!(p.counters.retention_skipped_files == 0);

    // NAME claims a much NEWER run (Oct 27); CONTENT is an OLDER run (Oct 18).
    let late = join_path(&ing, "gfs_2026102712.grib2");
    write_bytes(&late, &patch_day(&base, 18));
    assert!(p.scan_round() == 0); // observation
    assert!(p.scan_round() == 0); // judged against SECTION 1, not the name
    assert!(p.counters.retention_skipped_files == 1);
    assert!(p.counters.ingested_files == 1); // the stale content was NEVER decoded

    // the note quotes the REFERENCE TIME from the file, never the filename stamp
    let mut quoted_content = false;
    let mut quoted_name = false;
    for line in &p.log {
        if line.contains("retention skip") {
            if line.contains("2026-10-18T12:00:00Z") {
                quoted_content = true;
            }
            if line.contains("2026-10-27") {
                quoted_name = true;
            }
        }
    }
    assert!(quoted_content);
    assert!(!quoted_name);

    // neither the name-implied nor the content-implied tree exists on disk
    assert!(!has_grid(&data, "2026102712"));
    assert!(!has_grid(&data, "2026101812"));
    assert!(has_grid(&data, "2026101912"));
    rm_tree(&root);
}
