// V1.2 phase C gate #2: AN UNCERTAIN PEEK NEVER SKIPS (spec §4.2 correctness
// first). The retention-skip decision is taken from a cheap SECTION-1 header
// peek. If the peek cannot establish a reference time — bad edition byte, bad
// magic, broken framing, unreadable file — the candidate MUST fall through to
// the NORMAL path, where the magic-byte precheck and the poison quarantine
// apply exactly as in phase B. Skipping on uncertainty could silently DROP
// data, so the failure mode is "decode it normally", never "veto it".
//
// Per-Pipeline counters only here, so several behaviours share this binary.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path, REC_OK, REC_FAILED};
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

// retention_runs = 1: ONE ingested run already FILLS the registry, so every
// later candidate IS peeked.
fn one_run_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 1\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n"
}

#[test]
fn unpeekable_header_falls_through_to_the_normal_quarantine_path() {
    let root = "/tmp/opencode/wxpoint-peekfail".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    // good run fills the registry (retention_runs = 1)
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

    // ---- a file with GRIB magic but an INVALID EDITION BYTE ---------------
    // offset 7 of section 0 must be 2; 5 makes BOTH the peek and the decoder
    // refuse it. The peek's Err must NOT become a skip verdict.
    let whole = read_bytes(&fixture("gfs_small.grib2"));
    assert!(whole[0] == 0x47 && whole[1] == 0x52 && whole[2] == 0x49 && whole[3] == 0x42);
    let mut broken: Vec<u8> = whole.to_vec();
    broken[7] = 5;
    let bad = join_path(&ing, "gfs_broken.grib2");
    write_bytes(&bad, &broken[..]);

    assert!(p.scan_round() == 0); // stability first sighting
    assert!(p.scan_round() == 0); // attempted, refused by the decoder
    // (a) NOT skipped: the peek could not establish a reference time
    assert!(p.counters.retention_skipped_files == 0);
    // (b) it went down the NORMAL path: quarantined as a FAILED decode
    assert!(p.counters.errors == 1);
    assert!(p.counters.quarantined_files == 1);
    match p.state.find("gfs", bad.as_str()) {
        None => { panic!(); }
        Some(r) => { assert!(r.kind == REC_FAILED); }
    }
    // (c) the fall-through is explained ONCE, not once per round
    assert!(note_count(&p, "peek failed") == 1);
    assert!(p.scan_round() == 0);
    assert!(note_count(&p, "peek failed") == 1);
    assert!(p.counters.errors == 1);
    assert!(p.counters.retention_skipped_files == 0);

    // ---- the good run is still queryable and unaffected ------------------
    match p.state.find("gfs", join_path(&ing, "gfs_small.grib2").as_str()) {
        None => { panic!(); }
        Some(r) => { assert!(r.kind == REC_OK); }
    }
    rm_tree(&root);
}
