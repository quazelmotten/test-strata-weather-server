// V1.4 phase B2 DEFECT GATE #5 — PERSISTENT MANIFEST FAILURE IS BOUNDED.
//
// RISK #8 (risk register): since V1.3 E0 a failed manifest write BLOCKS the
// REC_OK record so the source file is retried next round — correct fail-closed
// behavior for TRANSIENT failures (the manifest_fail_retry.rs gate pins it).
// But a PERMANENT manifest-write failure (unfixable target, permissions the
// operator has not noticed yet) made that retry INFINITE: every scan round
// re-decoded the same file forever, inflating errors and burning CPU while
// the .bin data itself was already durable, published and answering queries.
//
// THE FIX keeps fail-closed as the DEFAULT and BOUNDS it: after N=3
// CONSECUTIVE rounds whose ONLY failing step for the SAME file identity
// (model, path, size, mtime_ns, content hash) is the manifest refresh, mark
// the file REC_OK anyway, keep the per-round error already counted (the
// ingest-error counter stays truthful), and warn LOUDLY naming the bound.
// WHY ACCEPTING IS SAFE: the accumulation guard no longer depends on the
// manifest at all — stat_durability.rs pins that the PDT 4.8 guard + time
// range live in each .bin's OWN header, are written atomically with the data,
// and are VALIDATED when the file opens; a warm boot with a MISSING manifest
// still rejects mid-window accumulation queries from those headers. So an
// accepted missing manifest costs INVENTORY only, never an answer change.
// Group-write or fsync failures are NOT pure-manifest: they keep today's
// unbounded block-and-retry (disk-full recovery must not expire).
//
// PRE-FIX DISCRIMINATOR: with the trap left in place FOREVER, the state
// record never reaches kind=OK with the current hash and the FAILED progress
// line repeats past 3 rounds — both assertions below fail on b30d156.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point, SelectError};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_ingest::state::{join_path, content_hash_file, REC_OK};
use wx_ingest::discovery::{path_to_str};
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::create_dir;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

/// Same N the pipeline documents (`MANIFEST_FAIL_BOUND`).
const BOUND: usize = 3;

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

/// Per-file FAILED progress lines (one per failing CONSUME attempt).
#[allow(clippy::needless_return)]
fn count_failed_progress(p: &Pipeline, pat: &str) -> usize {
    let mut n = 0usize;
    for line in &p.progress_log {
        if line.contains(pat) {
            n += 1;
        }
    }
    return n;
}

fn mf_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"stat\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"stat_small.grib2\"\n\n  [[models.params]]\n  name = \"APCP\"\n  levels = [\"*\"]\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n"
}

#[test]
fn persistent_manifest_failure_is_bounded_and_records_ok_after_three_rounds() {
    let root = "/tmp/opencode/wxpoint-b2-manifest-bound".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = data_root(&root);
    let _c = create_dir_all(&ing);
    let src = join_path(&ing, "stat_small.grib2");
    write_bytes(&src, &read_bytes("../../tests/fixtures/stat_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, mf_cfg(&data, &ing).as_bytes());

    // ---- life 1: healthy ingest ------------------------------------------
    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    let mut settled = 0usize;
    let mut rounds = 0usize;
    while settled == 0 && rounds < 8 {
        settled = p1.scan_round();
        rounds += 1;
    }
    assert!(settled == 1);
    assert!(p1.counters.errors == 0);

    // ---- arm the PERMANENT trap: a DIRECTORY named manifest.json ----------
    // The atomic writer renames its temp FILE onto that name and fails, while
    // every .bin write + fsync succeeds — isolating the manifest step forever.
    let run_dir = join_path(&join_path(&data, "stat"), "2026101912");
    let man = join_path(&run_dir, "manifest.json");
    let d0 = remove_file(&man);
    assert!(d0.is_ok());
    let dt = create_dir(&man);
    assert!(dt.is_ok());

    // Re-arm exactly one ingest with a content change (same fields, DRT 5.40).
    write_bytes(&src, &read_bytes("../../tests/fixtures/stat_small_jpeg.grib2"));
    let cur_hash_r = content_hash_file(&src);
    let cur_hash = match cur_hash_r {
        Err(_) => { panic!(); }
        Ok(hv) => { hv }
    };

    // ---- grind until the bound fires (never past BOUND+2 attempts) --------
    // The consuming rounds land on consecutive scan rounds once the identity
    // is stable; wait BOUNDEDLY and STOP at the bounded acceptance.
    let mut total_ingested = 0usize;
    let mut rounds_used = 0usize;
    while count_failed_progress(&p1, "write step(s) failed") < BOUND && rounds_used < 12 {
        total_ingested += p1.scan_round();
        rounds_used += 1;
    }
    let failed_lines = count_failed_progress(&p1, "write step(s) failed");
    assert!(failed_lines == BOUND);       // RED pre-fix: grows past 3 forever
    assert!(total_ingested == 0);         // never counted as successfully done

    // LOUD bound note naming the accept, exactly once.
    assert!(count_notes(&p1, "MANIFEST-Failure BOUND") == 1);

    // THE STATE RECORD now carries the CURRENT hash as ingested (pre-fix it
    // stayed stale forever and every round re-decoded).
    match p1.state.find("stat", src.as_str()) {
        None => { panic!(); }
        Some(rc) => {
            assert!(rc.kind == REC_OK);
            assert!(rc.hash == cur_hash);
        }
    }

    // One error per failing round was already counted by the file-with-
    // -failure branch (three rounds -> three errors): the ingest-error
    // counter stays truthful across the bounded accept.
    assert!(p1.counters.errors == BOUND as u64);

    // A further round finds NOTHING to do: the re-decode storm has a bound.
    assert!(p1.scan_round() == 0);
    assert!(p1.counters.errors == BOUND as u64);
    assert!(count_failed_progress(&p1, "write step(s) failed") == BOUND);

    // ---- restart with the trap STILL in place: headers carry the guard -----
    // This is the stat_durability claim APPLIED post-bound: no readable
    // manifest exists, yet the warm boot indexes from the .bin headers and
    // still REJECTS a mid-window accumulation query (422 stat_time_guard),
    // while an exact forecast hour still answers the analytic value.
    drop(p1);
    let p2r = Pipeline::open(&cfg_path);
    let mut p2 = match p2r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    // The missing manifest is still a REPORTED anomaly (loud + counted)...
    assert!(count_notes(&p2, "manifest.json MISSING") == 1);
    assert!(p2.counters.errors == 1);
    let snap = p2.shared.get_index();
    let sref = &*snap;
    let cg = build_context(sref, "stat", None, "APCP", 1, 0.0, 10800.0, TimeInterpMode::Linear);
    match cg {
        Ok(_c) => { panic!(); }
        Err(e) => {
            match e {
                SelectError::StatTimeGuard(..) => {}
                _ => { panic!(); }
            }
        }
    }
    let ce = build_context(sref, "stat", None, "APCP", 1, 0.0, 0.0, TimeInterpMode::Linear);
    match ce {
        Err(_) => { panic!(); }
        Ok(c) => {
            let v = evaluate_point(&c, 20.0, 240.0);
            match v {
                Err(_) => { panic!(); }
                Ok(pv) => {
                    assert!(pv.value == 10.0);
                    assert!(pv.info.horizontal == HorizMethod::Exact);
                }
            }
        }
    }
    // Bounded ACROSS restarts too: the accepted record means idle rounds here.
    assert!(p2.scan_round() == 0);
    assert!(p2.scan_round() == 0);
    rm_tree(&root);
}

#[allow(clippy::needless_return)]
fn data_root(root: &str) -> String {
    return format!("{0}/data", root);
}
