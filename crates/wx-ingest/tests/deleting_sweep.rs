// V1.3 phase E3b DEFECT GATE #3 — RETENTION DELETION IS QUARANTINED-FIRST AND
// CRASHES LEAVE NOTHING BEHIND THAT CAN ANSWER.
//
// THE DEFECT (AUDIT-confirmed): retention called delete_tree directly and
// IGNORED every error; a crash MID-DELETE left a PARTIAL oldest run whose name
// still parsed, so it RE-INDEXED on restart and answered (incomplete) queries
// until the next prune evicted it.
//
// THE FIX: rename the victim to `<name>.deleting.<stamp>` FIRST (one atomic
// operation; parse_run_dir rejects the name, so the run is instantly invisible
// to indexing), then delete recursively. At STARTUP, before anything is
// indexed, every `*.deleting.*` entry under a model's data dir is swept away
// with a loud count; sweep failures are noted + counted. This binary plants a
// quarantined husk (the crash residue) and pins the boot behavior, pins the
// name-parsing rule the design depends on, and shows the healthy prune path
// leaves zero residue and zero errors.

use wx_ingest::pipeline::{Pipeline, parse_run_dir};
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::File;
use std::io::Write;

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
fn base_name_of(p: String) -> String {
    match p.rfind('/') {
        None => { return p; }
        Some(ix) => { return p[ix + 1 ..].to_string(); }
    }
}

#[allow(clippy::needless_return)]
fn exists_any(p: &str) -> bool {
    // True when p names EITHER a directory or a regular file that exists.
    if read_dir(p).is_ok() { return true; }
    let ix = match p.rfind('/') {
        None => { return false; }
        Some(v) => { v }
    };
    match read_dir(&p[0 .. ix]) {
        Err(_) => { return false; }
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        if path_to_str(&de.path()) == p {
                            return true;
                        }
                    }
                }
            }
            return false;
        }
    }
}

#[allow(clippy::needless_return)]
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
fn read_bytes(path: &str) -> Vec<u8> {
    let f = File::open(path);
    match f {
        Err(_) => { panic!(); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { panic!(); }
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

/// Patch the section-1 reference-time DAY octet of every message (same trick
/// as tests/retention.rs): 39 messages in gfs_small.grib2.
#[allow(clippy::needless_return)]
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

// ---------------------------------------------------------------------------
// (a) boot sweep: the crash husk is gone BEFORE anything indexes, loudly
// ---------------------------------------------------------------------------

#[test]
fn quarantined_husk_is_swept_at_boot_before_anything_indexes() {
    let root = "/tmp/opencode/wxpoint-desweep";
    rm_tree(root);
    let ing = join_path(root, "ing");
    let data = join_path(root, "data");
    let _c = create_dir_all(&ing);
    let cfg_path = join_path(root, "config.toml");
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    write_bytes(&cfg_path, t.as_bytes());

    // Plant the crash residue: a renamed husk WITH junk inside (the state a
    // kill between rename and rmdir leaves) under the model's data dir.
    let husk = format!("{0}/gfs/2026101900.deleting.123456", data);
    let _m1 = create_dir_all(format!("{0}/TMP/isobaric", husk));
    write_bytes(&format!("{0}/TMP/isobaric/f006.bin", husk), b"half-written garbage");
    write_bytes(&format!("{0}/manifest.json", husk), b"{\"broken\":");

    let pr = Pipeline::open(&cfg_path);
    let p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    // gone from disk...
    assert!(!exists_any(husk.as_str()));
    // ...announced once with the sweep vocabulary...
    assert!(count_notes(&p, "startup sweep") == 1);
    assert!(count_notes(&p, "finished deleting 1 quarantined run dir(s)") == 1);
    // ...and NOT counted as an error (the deletion SUCCEEDED).
    assert!(p.counters.errors == 0);
    // Nothing indexed (empty ingest dir: a model earns its registry entry
    // only by having an INDEXED run, and the swept husk never indexed).
    assert!(p.registry.is_empty());
    rm_tree(root);
}

// ---------------------------------------------------------------------------
// (b) the parser rule the whole design rests on
// ---------------------------------------------------------------------------

#[test]
fn quarantine_names_never_parse_as_run_dirs() {
    // The victim must become invisible to indexing the MOMENT the rename
    // lands, before the recursive delete even starts.
    assert!(parse_run_dir("2026101912.deleting.99").is_none());
    assert!(parse_run_dir("20261019123.deleting.99").is_none());
    assert!(parse_run_dir("2026101912.deleting.7.3").is_none());
    // sanity: the plain names still parse.
    assert!(parse_run_dir("2026101912").is_some());
}

// ---------------------------------------------------------------------------
// (c) healthy prune: rename+delete completes inline, zero residue, zero noise
// ---------------------------------------------------------------------------

#[test]
fn retention_quarantine_leaves_no_residue_on_the_happy_path() {
    let root = "/tmp/opencode/wxpoint-desweep-retention";
    rm_tree(root);
    let ing = join_path(root, "ing");
    let data = join_path(root, "data");
    let _c = create_dir_all(&ing);
    let base = read_bytes("../../tests/fixtures/gfs_small.grib2");
    // run A: 2026-10-19T12Z, run B: 2026-10-20T12Z (section-1 day patched).
    write_bytes(&join_path(&ing, "gfs_a.grib2"), &base);
    write_bytes(&join_path(&ing, "gfs_b.grib2"), &patch_day(&base, 20));
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 1\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    let cfg_path = join_path(root, "config.toml");
    write_bytes(&cfg_path, t.as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(p.scan_round() == 0);
    let n2 = p.scan_round();
    assert!(n2 == 2);
    // retention_runs = 1: run A is pruned as soon as B indexes.
    assert!(p.registry[0].runs.len() == 1);
    assert!(p.registry[0].runs[0].dir_name == "2026102012");
    // the victim tree is GONE and no quarantine husk lingers...
    assert!(!exists_any(&format!("{0}/gfs/2026101912", data)));
    let mut residue = 0usize;
    match read_dir(format!("{0}/gfs", data)) {
        Err(_) => {}
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        if base_name_of(path_to_str(&de.path())).contains(".deleting.") {
                            residue += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(residue == 0);
    // ...and the whole flow cost NO errors and NO fallback/sweep notes: the
    // happy path renames, deletes fully, and says nothing beyond progress.
    assert!(p.counters.errors == 0);
    assert!(count_notes(&p, "startup sweep") == 0);
    assert!(count_notes(&p, "quarantine rename FAILED") == 0);
    assert!(count_notes(&p, "partial delete") == 0);
    rm_tree(root);
}
