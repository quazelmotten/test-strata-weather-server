// V1.3 phase E3b DEFECT GATE #4 — STATE RECORDS ARE PER-MODEL, SO OVERLAPPING
// MODELS BOTH INGEST (AND THE SHARING IS WARNED ABOUT AT STARTUP).
//
// THE DEFECT (AUDIT-confirmed): persistent records were keyed by PATH ONLY.
// Two models sharing an ingest dir (or one nested inside another) with
// overlapping globs: whichever model scanned first wrote the record, and every
// other model saw "unchanged" and skipped the file FOREVER — silently starved,
// second model never queryable.
//
// THE FIX (two parts):
//  (a) FileRec gained a `model` field; the derived index / find / drop_under
//      are keyed (model, path). The on-disk format VERSION bumped 1 -> 2 and
//      old-version files are DISCARDED LOUDLY once (one re-ingest per upgrade,
//      documented like the E0 storage-format philosophy).
//  (b) Startup emits a LOUD non-fatal warning for every model pair whose
//      ingest dirs are equal-or-nested AND whose globs are not provably
//      disjoint (conservative literal prefix/suffix heuristic — see
//      config::globs_maybe_overlap for its documented limits).
//
// This binary drives BOTH models to queryable state from ONE shared file,
// pins distinct records + both registries, the legacy-discard re-ingest, and
// the warning presence/absence.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path, deserialize_text, REC_OK};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::read_to_string;

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
fn count_notes(p: &Pipeline, pat: &str) -> usize {
    let mut n = 0usize;
    for line in &p.log {
        if line.contains(pat) {
            n += 1;
        }
    }
    return n;
}

/// Two models, SAME dir + SAME glob (worst overlap), identical param sets.
#[allow(clippy::needless_return)]
fn twin_cfg(data: &str, ing: &str, ga: &str, gb: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"ma\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = format!("{0}{1}", t, "\"\nfile_glob = \"");
    let t = format!("{0}{1}", t, ga);
    let t = t + "\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n\n[[models]]\nname = \"mb\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = format!("{0}{1}", t, "\"\nfile_glob = \"");
    let t = format!("{0}{1}", t, gb);
    let t = t + "\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

// ---------------------------------------------------------------------------
// (a) THE STARVATION FIX: one shared file, BOTH models ingest it
// ---------------------------------------------------------------------------

#[test]
fn overlapping_models_on_one_dir_both_ingest_with_distinct_records() {
    let root = "/tmp/opencode/wxpoint-twinmodels";
    rm_tree(root);
    let ing = join_path(root, "ing");
    let data = join_path(root, "data");
    let _c = create_dir_all(&ing);
    let src = join_path(&ing, "gfs_small.grib2");
    write_bytes(&src, &read_bytes("../../tests/fixtures/gfs_small.grib2"));
    let cfg_path = join_path(root, "config.toml");
    write_bytes(&cfg_path, twin_cfg(&data, &ing, "*.grib2", "*.grib2").as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(e) => { println!("open failed: {e}"); panic!(); }
        Ok(pp) => { pp }
    };
    assert!(count_notes(&p, "config warning") == 1);
    assert!(count_notes(&p, "'ma' and 'mb'") == 1);

    // (b) the startup WARNING fired for the pair, naming both models.
    assert!(count_notes(&p, "config warning") == 1);
    assert!(count_notes(&p, "'ma' and 'mb'") == 1);

    // Drive to completion. NOTE ON ROUND SYMMETRY: the stability tracker is
    // keyed by PATH (correctly — "this file stopped changing" is a property of
    // the FILE, not of a model), so with stable_window_secs = 0 the SECOND
    // model to observe a path in a round sees an already-stamped entry and is
    // eligible in that SAME round while the first model needs the next one.
    // Pre-E3b the end state was the bug: mb recorded the path and ma (or vice
    // versa) skipped it FOREVER. Post-E3b BOTH records exist within a couple
    // of rounds regardless of which order the rounds happen to split them.
    let mut total = 0usize;
    let mut rounds = 0usize;
    while total < 2 && rounds < 4 {
        total += p.scan_round();
        rounds += 1;
    }
    assert!(total == 2);
    assert!(rounds <= 3);
    // Round 2: pre-E3b the running total would have STALLED at ONE (the other
    // model silently skipped forever). Post-E3b BOTH models consume the same
    // path and both records exist.
    assert!(p.counters.ingested_files == 2);
    assert!(p.counters.errors == 0);

    // DISTINCT records for the SAME path, one per model...
    let ra = p.state.find("ma", src.as_str());
    let rb = p.state.find("mb", src.as_str());
    match ra {
        None => { panic!(); }
        Some(r) => { assert!(r.kind == REC_OK); }
    }
    match rb {
        None => { panic!(); }
        Some(r) => { assert!(r.kind == REC_OK); }
    }
    let jpath = join_path(&join_path(&data, "state"), "files.json");
    let txt = read_to_string(&jpath);
    let doc = match txt {
        Err(_) => { panic!(); }
        Ok(t0) => {
            match deserialize_text(&t0) {
                Err(_) => { panic!(); }
                Ok(d) => { d }
            }
        }
    };
    assert!(doc.recs.len() == 2);
    let mut saw_ma = false;
    let mut saw_mb = false;
    for r in &doc.recs {
        if r.model == "ma" { saw_ma = true; }
        if r.model == "mb" { saw_mb = true; }
    }
    assert!(saw_ma && saw_mb);

    // ...and BOTH models are indexed/queryable (the point of the fix).
    assert!(p.registry.len() == 2);
    let mut both_runs = 0;
    for rm in &p.registry {
        if rm.runs.len() == 1 && rm.runs[0].dir_name == "2026101912" {
            both_runs += 1;
        }
    }
    assert!(both_runs == 2);

    // Idempotence: a third round changes nothing for either model.
    assert!(p.scan_round() == 0);
    rm_tree(root);
}

// ---------------------------------------------------------------------------
// (c) legacy version-1 state: discarded loudly ONCE, then full re-ingest
// ---------------------------------------------------------------------------

#[test]
fn legacy_v1_state_is_discarded_loudly_and_everything_reingests_once() {
    let root = "/tmp/opencode/wxpoint-legacystate";
    rm_tree(root);
    let ing = join_path(root, "ing");
    let data = join_path(root, "data");
    let _c = create_dir_all(&ing);
    let src = join_path(&ing, "gfs_small.grib2");
    write_bytes(&src, &read_bytes("../../tests/fixtures/gfs_small.grib2"));
    let cfg_path = join_path(root, "config.toml");
    write_bytes(&cfg_path, twin_cfg(&data, &ing, "*.grib2", "*.grib2").as_bytes());

    // Pre-seed a VERSION-1 files.json claiming everything is already ingested
    // (exactly what an upgraded deployment carries). Pre-E3b these records
    // would load and SILENTLY suppress every ingest; E3b discards them loudly.
    let st_dir = join_path(&data, "state");
    let _c2 = create_dir_all(&st_dir);
    let fake = "{\"version\":1,\"recs\":[";
    let fake = format!("{0}{{\"path\":\"{1}\",\"size\":999999,\"mtime_ns\":1,\"hash\":\"deadbeefdeadbeef\",\"kind\":0}}", fake, src);
    let fake = fake + "]}";
    write_bytes(&join_path(&st_dir, "files.json"), fake.as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    // exactly one loud upgrade note naming the discard...
    assert!(count_notes(&p, "STATE UPGRADE") == 1);
    assert!(count_notes(&p, "DISCARDED 1 record(s)") == 1);
    // ...NOT counted as an error (expected upgrade event)...
    assert!(p.counters.errors == 0);
    // ...and the store really starts EMPTY.
    assert!(p.state.recs.is_empty());

    // The documented cost: ONE full re-ingest despite identical content (the
    // shared-path stability tracker splits the two models across rounds the
    // same way as the twin test above — drive to completion, bounded).
    let mut total = 0usize;
    let mut rounds = 0usize;
    while total < 2 && rounds < 4 {
        total += p.scan_round();
        rounds += 1;
    }
    assert!(total == 2);
    assert!(rounds <= 3);
    // After that, warm start resumes normally (further rounds: zero work).
    assert!(p.scan_round() == 0);
    rm_tree(root);
}

// ---------------------------------------------------------------------------
// (d) the heuristic: provably disjoint globs share a dir WITHOUT a warning
// ---------------------------------------------------------------------------

#[test]
fn disjoint_globs_on_one_dir_produce_no_warning_but_overlap_does() {
    let root = "/tmp/opencode/wxpoint-twinglobs";
    rm_tree(root);
    let ing = join_path(root, "ing");
    let data = join_path(root, "data");
    let _c = create_dir_all(&ing);
    let cfg_path = join_path(root, "config.toml");

    // gfs.t*z.pgrb2* vs hrrr.t*z.wrf*: fixed prefixes "gfs.t" / "hrrr.t"
    // differ immediately -> PROVABLY disjoint -> no warning expected.
    write_bytes(&cfg_path, twin_cfg(&data, &ing, "gfs.t*z.pgrb2*", "hrrr.t*z.wrf*").as_bytes());
    let pr = Pipeline::open(&cfg_path);
    let p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(count_notes(&p, "config warning") == 0);
    drop(p);

    // Same dirs, ambiguous globs ("*.grib2" vs "gfs*.grib2"): the fixed
    // prefix of the first is EMPTY (wildcard-first), so co-match is possible
    // -> warn (over-warning is the documented safe direction).
    rm_tree(&data);
    let _c3 = create_dir_all(&data);
    write_bytes(&cfg_path, twin_cfg(&data, &ing, "*.grib2", "gfs*.grib2").as_bytes());
    let pr2 = Pipeline::open(&cfg_path);
    let p2 = match pr2 {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(count_notes(&p2, "config warning") == 1);
    rm_tree(root);
}
