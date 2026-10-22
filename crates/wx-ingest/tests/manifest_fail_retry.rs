// V1.3 phase E0 DEFECT GATE #3 — a FAILED manifest write must block REC_OK.
//
// THE DEFECT (amplifier of AUDIT_1.2.md P1): when the manifest refresh failed
// but every GROUP write succeeded, ingest_file logged and counted the error yet
// returned failed_groups == 0, so scan_round recorded the source file as REC_OK
// ("fully ingested"). A crash — or any persistent manifest-write failure —
// therefore left the run permanently without inventory until the file happened
// to be re-delivered. THE FIX: the manifest refresh increments the SAME
// `failed` counter a failed group write does, so the file is NOT marked
// ingested and the NEXT round retries it (idempotent via merge dedup + atomic
// rename). Exactly ONE error is counted per file-with-failure (the outer
// branch), never one per failed step.
//
// INJECTION: a DIRECTORY named manifest.json inside the run dir. The atomic
// writer renames its temp FILE onto that name, which fails (EISDIR/ENOTDIR),
// while every .bin write and fsync still succeeds — isolating the manifest
// step. PRE-FIX DISCRIMINATOR: pre-fix round A recorded REC_OK with the NEW
// content hash, so round B ingested NOTHING and the manifest stayed absent
// forever once the trap was removed; the assertions below fail there.
//
// Single test in this binary: counter assertions are process-wide.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_ingest::state::{join_path, content_hash_file, REC_OK};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::create_dir;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

#[allow(clippy::needless_return)]
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

#[allow(clippy::needless_return)]
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

#[allow(clippy::needless_return)]
fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(bytes);
            match n {
                Err(_) => { assert!(false); return; }
                Ok(_) => {}
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn bytes_contains(hay: &[u8], pat: &[u8]) -> bool {
    if pat.len() > hay.len() {
        return false;
    }
    for s in 0 .. (hay.len() - pat.len() + 1) {
        let mut ok = true;
        for k in 0 .. pat.len() {
            if hay[s + k] != pat[k] {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }
    return false;
}

#[allow(clippy::needless_return)]
fn count_notes(p: &Pipeline, pat: &str) -> usize {
    let mut n = 0usize;
    for line in &p.log {
        if bytes_contains(line.as_bytes(), pat.as_bytes()) {
            n += 1;
        }
    }
    return n;
}

// operator-visible PROGRESS stream (distinct channel from `log`: see
// progress_once_per_file.rs) — the per-file FAILED line lives here.
#[allow(clippy::needless_return)]
fn count_progress(p: &Pipeline, pat: &str) -> usize {
    let mut n = 0usize;
    for line in &p.progress_log {
        if bytes_contains(line.as_bytes(), pat.as_bytes()) {
            n += 1;
        }
    }
    return n;
}

#[allow(clippy::needless_return)]
fn settle(p: &mut Pipeline, want: usize) -> usize {
    let mut got = 0usize;
    let mut rounds = 0usize;
    while got != want && rounds < 8usize {
        let one = p.scan_round();
        got += one;
        rounds += 1;
        if rounds >= 8usize {
            break;
        }
    }
    return got;
}

fn stat_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"stat\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"stat_small.grib2\"\n\n  [[models.params]]\n  name = \"APCP\"\n  levels = [\"*\"]\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn manifest_write_failure_blocks_state_record_and_retries_next_round() {
    let root = "/tmp/opencode/wxpoint-manifest-fail-retry".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    let src = join_path(&ing, "stat_small.grib2");
    write_bytes(&src, &read_bytes("../../tests/fixtures/stat_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, stat_cfg(&data, &ing).as_bytes());

    // ---- life 1: healthy ingest ------------------------------------------
    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);
    assert!(p1.counters.errors == 0);
    let run_dir = join_path(&join_path(&data, "stat"), "2026101912");
    let man = join_path(&run_dir, "manifest.json");
    assert!(bytes_contains(&read_bytes(&man), "\"stat_kind\":\"accumulation\"".as_bytes()));

    // ---- arm the trap ------------------------------------------------------
    // Remove the good manifest, then plant a DIRECTORY with its exact name:
    // rename(temp FILE -> manifest.json) fails, every .bin write succeeds.
    let d0 = remove_file(&man);
    assert!(d0.is_ok());
    let dt = create_dir(&man);
    assert!(dt.is_ok());

    // Re-arm exactly one ingest with a content change (same fields, DRT 5.40).
    write_bytes(&src, &read_bytes("../../tests/fixtures/stat_small_jpeg.grib2"));
    let cur_hash_r = content_hash_file(&src);
    let cur_hash = match cur_hash_r {
        Err(_) => { assert!(false); return; }
        Ok(hv) => { hv }
    };

    // ---- round A: groups succeed, manifest write FAILS --------------------
    // A changed identity RESETS the stability window (see progress_once_per_
    // file), so the consuming round may be the first or a later one; wait
    // BOUNDEDLY for the failure round and STOP THERE, so the trap can only
    // bite exactly once no matter which round consumes the candidate.
    let mut got_a = 0usize;
    let mut rounds_a = 0usize;
    while p1.counters.errors == 0 && rounds_a < 8usize {
        let one = p1.scan_round();
        got_a += one;
        rounds_a += 1;
    }
    assert!(got_a == 0usize); // NOT counted as ingested
    // EXACTLY ONE error for the file-with-failure (inner manifest branch does
    // not double-count; pre-E0 this was the only count but REC_OK still stuck).
    assert!(p1.counters.errors == 1);
    assert!(count_notes(&p1, "manifest update failed for run") == 1);
    assert!(count_progress(&p1, "write step(s) failed") == 1);

    // THE STATE RECORD MUST STILL CARRY THE OLD HASH (stale REC_OK from life
    // 1): pre-fix this upserted the NEW hash and the retry could never happen.
    let rec_r = p1.state.find(src.clone());
    match rec_r {
        None => { assert!(false); return; }
        Some(rc) => {
            assert!(rc.kind == REC_OK);          // the stale life-1 record
            assert!(rc.hash != cur_hash);        // NOT re-marked with new hash
        }
    }

    // The trap is still in place: no readable manifest bytes at that path.
    let trapped = read_bytes(&man);
    assert!(trapped.len() == 0);

    // Meanwhile the DATA side stayed truthful and queryable: the groups were
    // durably rewritten and published even though the state record lagged.
    let snap = p1.shared.get_index();
    let sref = &*snap;
    let ce = build_context(sref, "stat", None, "APCP", 1, 0.0, 0.0, TimeInterpMode::Linear);
    match ce {
        Err(_) => { assert!(false); return; }
        Ok(c) => {
            let v = evaluate_point(&c, 20.0, 240.0);
            match v {
                Err(_) => { assert!(false); return; }
                Ok(pv) => {
                    assert!(pv.value == 10.0);
                    assert!(pv.info.horizontal == HorizMethod::Exact);
                }
            }
        }
    }

    // ---- disarm: next round retries and completes -------------------------
    let dd = remove_dir(&man);
    assert!(dd.is_ok());
    let got_b = settle(&mut p1, 1usize);
    assert!(got_b == 1usize); // retried, because round A never marked it done
    assert!(p1.counters.errors == 1); // the retry itself added no error

    // Manifest rewritten FROM THE .BIN HEADERS (the durable source of truth).
    let man_now = read_bytes(&man);
    assert!(bytes_contains(&man_now, "\"stat_kind\":\"accumulation\"".as_bytes()));
    assert!(bytes_contains(&man_now, "\"fh_secs\":0,\"begin\":\"2026-10-19T12:00:00Z\",\"end\":\"2026-10-19T18:00:00Z\"".as_bytes()));

    // State now records the CURRENT hash as ingested.
    let rec2_r = p1.state.find(src.clone());
    match rec2_r {
        None => { assert!(false); return; }
        Some(rc2) => {
            assert!(rc2.kind == REC_OK);
            assert!(rc2.hash == cur_hash);
        }
    }

    // And a third round finds nothing left to do (no infinite retry loop).
    let got_c = p1.scan_round();
    assert!(got_c == 0usize);
    assert!(p1.counters.errors == 1);

    rm_tree(&root);
}
