// V1.4 phase B2 DEFECT GATE #2 — a CORRUPT files.json QUARANTINES, not kills.
//
// THE DEFECT (auditors reproduced live): StateStore::load returned Err for any
// malformed/truncated files.json, Pipeline::open propagated it, and main exited
// with code 3 ON EVERY BOOT until an operator manually removed the file. One
// torn write (crash mid-rename on a filesystem without atomic rename of the
// temp, hand edit, NFS glitch) turned a self-healing service into a dead one.
//
// THE FIX: rename the bad file aside to `files.json.corrupt.<UTC-stamp>`
// (compact YYYYMMDDTHHMMSSZ-nnnnnnnnnns so two boots in one second cannot
// collide), boot with EMPTY state, warn LOUDLY naming both paths, continue.
// An empty state costs exactly one idempotent re-ingest (merge dedup + atomic
// renames), the same philosophy as every other recovery trap in this crate.
// The VERSION GATE for well-formed LEGACY docs (discarded_legacy note, one
// re-ingest, NO quarantine file) stays EXACTLY as state_hygiene.rs pins it;
// only UNPARSABLE text — malformed legacy included — takes the new path.
//
// PRE-FIX DISCRIMINATORS: tests (a) and (c) panic at Pipeline::open (Err ->
// boot death) on b30d156; test (b) guards the legacy story against regression
// and is green before AND after by design.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path, REC_OK};
use wx_ingest::discovery::{path_to_str};
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::read_to_string;

fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!(); }
        Ok(mut w) => {
            let n = w.write_all(bytes);
            if n.is_err() { panic!(); }
        }
    }
}

#[allow(clippy::needless_return)]
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
fn count_notes(p: &Pipeline, pat: &str) -> usize {
    let mut n = 0usize;
    for line in &p.log {
        if line.contains(pat) {
            n += 1;
        }
    }
    return n;
}

/// Names under `dir` starting with `prefix` (the quarantined siblings).
#[allow(clippy::needless_return)]
fn names_with_prefix(dir: &str, prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::<String>::new();
    match read_dir(dir) {
        Err(_) => { return out; }
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let bn = match de.file_name().to_str() {
                            None => { "".to_string() }
                            Some(s) => { s.to_string() }
                        };
                        if bn.len() >= prefix.len() && bn[0 .. prefix.len()] == *prefix {
                            out.push(bn);
                        }
                    }
                }
            }
        }
    }
    out.sort();
    return out;
}

#[allow(clippy::needless_return)]
fn cs_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

/// root scaffold: ingest dir holding gfs_small + config.toml; returns
/// (cfg_path, data_dir, ingest_dir, source path).
#[allow(clippy::needless_return)]
fn scaffold(root: &str) -> (String, String, String, String) {
    let ing = join_path(root, "ing");
    let data = join_path(root, "data");
    let _c = create_dir_all(&ing);
    let src = join_path(&ing, "gfs_small.grib2");
    write_bytes(&src, read_bytes("../../tests/fixtures/gfs_small.grib2").as_slice());
    let cfg_path = join_path(root, "config.toml");
    write_bytes(&cfg_path, cs_cfg(&data, &ing).as_bytes());
    return (cfg_path, data, ing, src);
}

/// RAW byte reader (GRIB fixtures are not valid UTF-8, so no read_to_string).
#[allow(clippy::needless_return)]
fn read_bytes(path: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::<u8>::new();
    let f = File::open(path);
    match f {
        Err(_) => { return buf; }
        Ok(mut h) => {
            let r = std::io::Read::read_to_end(&mut h, &mut buf);
            match r {
                Err(_) => { return Vec::<u8>::new(); }
                Ok(_n) => { return buf; }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (a) garbage in files.json: boot RUNS, garbage preserved beside it
// ---------------------------------------------------------------------------

#[test]
fn garbage_files_json_boots_a_running_pipeline_and_quarantines_the_file() {
    let root = "/tmp/opencode/wxpoint-b2-corruptstate-a";
    rm_tree(root);
    let (cfg_path, data, _ing, src) = scaffold(root);
    let st_dir = join_path(&data, "state");
    let _c = create_dir_all(&st_dir);
    let fpath = join_path(&st_dir, "files.json");
    let garbage = "}{ this is NOT json {{{ truncated".to_string();
    write_bytes(&fpath, garbage.as_bytes());

    // RED pre-fix: Pipeline::open returns Err -> panic here.
    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    // LOUD warn naming the offender and the destination.
    assert!(count_notes(&p, "STATE QUARANTINE") == 1);
    let names = names_with_prefix(&st_dir, "files.json.corrupt.");
    assert!(names.len() == 1);
    // The bad bytes are PRESERVED (operator forensics), files.json is gone.
    let qfull = join_path(&st_dir, names[0].as_str());
    match read_to_string(&qfull) {
        Err(_) => { panic!(); }
        Ok(t) => { assert!(t == garbage); }
    }
    if read_to_string(&fpath).is_ok() { panic!(); }
    // The pipeline WORKS: empty state re-ingests exactly once, idempotently.
    assert!(p.scan_round() == 0);
    assert!(p.scan_round() == 1);
    drop(p);

    // Restart with the now-clean store: no fresh quarantine, warm start holds.
    let p2r = Pipeline::open(&cfg_path);
    let p2 = match p2r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(count_notes(&p2, "STATE QUARANTINE") == 0);
    match p2.state.find("gfs", src.as_str()) {
        None => { panic!(); }
        Some(rc) => { assert!(rc.kind == REC_OK); }
    }
    // Exactly ONE quarantined sibling still exists (no rename churn per boot).
    assert!(names_with_prefix(&st_dir, "files.json.corrupt.").len() == 1);
    rm_tree(root);
}

// ---------------------------------------------------------------------------
// (b) WELL-FORMED legacy v1 doc: the version-gate story is UNCHANGED
// ---------------------------------------------------------------------------

#[test]
fn well_formed_legacy_doc_keeps_the_version_gate_not_a_quarantine() {
    let root = "/tmp/opencode/wxpoint-b2-corruptstate-b";
    rm_tree(root);
    let (cfg_path, data, _ing, _src) = scaffold(root);
    let st_dir = join_path(&data, "state");
    let _c = create_dir_all(&st_dir);
    let fpath = join_path(&st_dir, "files.json");
    let legacy = "{\"version\":1,\"recs\":[{\"path\":\"/r/old.grib2\",\"size\":7,\"mtime_ns\":9,\"hash\":\"00000000000000cd\"}]}";
    write_bytes(&fpath, legacy.as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    // E3b upgrade story: DISCARD loudly via the STATE UPGRADE note...
    assert!(count_notes(&p, "STATE UPGRADE") == 1);
    // ...NOT the new corruption path: NO quarantine note, NOTHING renamed.
    assert!(count_notes(&p, "STATE QUARANTINE") == 0);
    assert!(names_with_prefix(&st_dir, "files.json.corrupt.").is_empty());
    // The legacy file itself stays PUT (it parses; the gate just ignores it).
    match read_to_string(&fpath) {
        Err(_) => { panic!(); }
        Ok(t) => { assert!(t == legacy); }
    }
    rm_tree(root);
}

// ---------------------------------------------------------------------------
// (c) MALFORMED legacy doc: truncated v1 JSON ALSO quarantines, boot lives
// ---------------------------------------------------------------------------

#[test]
fn malformed_legacy_doc_quarantines_and_boot_continues() {
    let root = "/tmp/opencode/wxpoint-b2-corruptstate-c";
    rm_tree(root);
    let (cfg_path, data, _ing, _src) = scaffold(root);
    let st_dir = join_path(&data, "state");
    let _c = create_dir_all(&st_dir);
    let fpath = join_path(&st_dir, "files.json");
    // Claims to be a v1 doc but the BYTES are truncated mid-record: the
    // version gate cannot even run because the doc never parses.
    write_bytes(&fpath, "{\"version\":1,\"recs\":[{\"path\":\"/x".as_bytes());

    // RED pre-fix: parse Err -> exit-3-on-every-boot death here.
    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(count_notes(&p, "STATE QUARANTINE") == 1);
    assert!(names_with_prefix(&st_dir, "files.json.corrupt.").len() == 1);
    // Boot CONTINUES and serves: the fixture ingests normally.
    assert!(p.scan_round() == 0);
    assert!(p.scan_round() == 1);
    rm_tree(root);
}
