// V1.3 phase E0 DEFECT GATE — statistical-metadata durability.
//
// THE DEFECT (AUDIT_1.2.md P1, reproduced by the orchestrator): stat_processed,
// stat_kind and the PDT 4.8 time range lived ONLY in manifest.json. A warm start
// with a missing or unparsable manifest silently treated an ACCUMULATION as an
// instantaneous field and TIME-INTERPOLATED it (spec §6.3 violation): normal
// APCP fh3 answered 422 `stat_time_guard`, but after `rm manifest.json` +
// restart it answered 200 with linear interpolation between forecast_hours
// [0, 6] — a plausible, wrong number. The amplifier: a FAILED manifest write
// counted an error but still recorded the source file as ingested, so a crash
// between the durable .bin writes and the manifest produced the same state.
//
// THE FIX: the metadata now lives in each field file's own header
// (`wx_core::storage::StatMeta`, BIN_VERSION 2), and the warm-start rebuild
// reads the guard FROM THAT HEADER. These tests pin:
//   * the COLD path writes the window into the .bin bytes (not only JSON);
//   * a warm boot with NO manifest still rejects mid-fh accumulation queries;
//   * a warm boot with a CORRUPT manifest says so LOUDLY, counts one error, and
//     still answers correctly from the headers.

use wx_ingest::pipeline::{Pipeline, level_dir_name};
use wx_ingest::index::{build_context, evaluate_point, SelectError};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_ingest::state::join_path;
use wx_ingest::discovery::{path_to_str};
use wx_core::tables::{lookup_level_type};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

// ---------------------------------------------------------------------------
// helpers (same shapes the other pipeline tests use)
// ---------------------------------------------------------------------------

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
fn exists_any(path: &str) -> bool {
    let f = File::open(&path);
    match f {
        Err(_) => { return false; }
        Ok(_) => { return true; }
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

/// Bounded settle loop: never pin WHICH round ingests (V1.2 phase B0 rule).
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

// Same config plus a second model carrying an INSTANTANEOUS field, so a
// manifest-less warm boot can prove time interpolation still WORKS (the
// guard must not over-block).
fn stat_cfg_with_gfs(data: &str, ing: &str) -> String {
    let t = stat_cfg(data, ing);
    let t = t + "\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n";
    return t;
}

// ---------------------------------------------------------------------------
// (a) warm boot with NO manifest.json
// ---------------------------------------------------------------------------

#[test]
fn warm_boot_without_manifest_keeps_the_accumulation_guard() {
    let root = "/tmp/opencode/wxpoint-stat-durability".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "stat_small.grib2"),
                &read_bytes("../../tests/fixtures/stat_small.grib2"));
    write_bytes(&join_path(&ing, "gfs_small.grib2"),
                &read_bytes("../../tests/fixtures/gfs_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, stat_cfg_with_gfs(&data, &ing).as_bytes());

    // ---- life 1: normal ingest (two models, two files) -------------------
    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 2usize) == 2usize);
    assert!(p1.counters.errors == 0);

    // ---- THE METADATA MUST BE IN THE FIELD FILE'S OWN BYTES --------------
    // APCP/surface/f000.bin header: version 2 at byte 8, stat_kind =
    // accumulation (1) at 23, stat_processed = 1 at 24, begin i64 = 0 at 28,
    // end i64 = 21600 at 36. Read as RAW BYTES on purpose: an mmap of a file
    // this same process just renamed is a known test-harness limitation here
    // (see docs/DECISIONS.md V1.2 phase B notes), and bytes are the contract.
    let surf = level_dir_name(1);
    let run_dir = join_path(&join_path(&data, "stat"), "2026101912");
    let f000 = join_path(&join_path(&run_dir, "APCP"), &join_path(&surf, "f000.bin"));
    let hb = read_bytes(&f000);
    assert!(hb.len() > 56);
    let ver = (hb[8] as u32) | ((hb[9] as u32) << 8) | ((hb[10] as u32) << 16) | ((hb[11] as u32) << 24);
    assert!(ver == 2); // BIN_VERSION
    assert!(hb[23] == 1); // STAT_ACCUMULATION
    assert!(hb[24] == 1); // stat_processed = true
    let le_i64 = |b: &[u8], off: usize| -> i64 {
        let mut v: u64 = 0;
        for k in 0 .. 8 {
            v = v | (((b[off + k] as u64)) << (k * 8));
        }
        return v as i64;
    };
    assert!(le_i64(&hb, 28) == 0);      // begin_secs
    assert!(le_i64(&hb, 36) == 21600);  // end_secs   (0-6 h window)
    assert!(le_i64(&hb, 44) == 0);      // step_secs

    // manifest present at this point (and it stays human-facing inventory only)
    let man = join_path(&run_dir, "manifest.json");
    assert!(exists_any(&man));

    // ---- delete the manifest: the pre-fix silent-wrong-answer trigger -----
    let d = remove_file(&man);
    assert!(d.is_ok());

    // ---- life 2: warm start with NO manifest ------------------------------
    let p2r = Pipeline::open(&cfg_path);
    let p2 = match p2r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    // LOUD once per run + counted, but indexing proceeds from the headers.
    assert!(count_notes(&p2, "manifest.json MISSING") == 1);
    assert!(p2.counters.errors == 1);

    let snap = p2.shared.get_index();
    let sref = &*snap;
    let sm = sref.find_model("stat");
    match sm {
        None => { assert!(false); return; }
        Some(m) => {
            assert!(m.runs.len() == 1);
            let mut found = false;
            for fe in &m.runs[0].fields {
                if fe.param_key != "APCP" {
                    continue;
                }
                found = true;
                // The guard + kind come from the .bin HEADER now, so `/ready`,
                // `/v1/inventory` and the query path all stay truthful without
                // any manifest. Pre-fix these two assertions were FALSE.
                assert!(fe.fd.stat_processed);
                assert!(fe.fd.stat_kind == "accumulation".to_string());
                assert!(fe.fd.fhs.len() == 2);
                for sl in &fe.fd.fhs {
                    if sl.fh_secs == 0 {
                        assert!(sl.stat_has_range);
                        assert!(sl.stat_begin_secs == 0 && sl.stat_end_secs == 21600);
                    }
                    if sl.fh_secs == 21600 {
                        assert!(sl.stat_has_range);
                        assert!(sl.stat_begin_secs == 21600 && sl.stat_end_secs == 43200);
                    }
                }
            }
            assert!(found);
        }
    }

    // MID-FORECAST-HOUR QUERY ON AN ACCUMULATION: 422 `stat_time_guard`.
    // Pre-fix this returned Ok and the caller blended two accumulations.
    let cg = build_context(sref, "stat", None, "APCP", 1, 0.0, 10800.0, TimeInterpMode::Linear);
    match cg {
        Ok(_c) => { assert!(false); return; }
        Err(e) => {
            match e {
                SelectError::StatTimeGuard(..) => {}
                _ => { assert!(false); return; }
            }
        }
    }
    // An EXACT forecast hour still answers (the guard never blocks exactness),
    // and APCP f000 at node (lat 20, lon 240) is still the analytic 10.
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
    // AN INSTANTANEOUS FIELD (different model/store) MUST STILL INTERPOLATE IN
    // TIME after a manifest-less warm boot: TMP@1000 fh 3 h between f000 and
    // f006 at node (lat 10, lon 350) is the analytic midpoint 280.0 with the
    // [0, 6] bracket reported. Pre-fix everything interpolated (that was the
    // bug); now the accumulation next door stays guarded while this answers.
    let iso_r = lookup_level_type("isobaric");
    let iso = match iso_r {
        None => { assert!(false); return; }
        Some(vv) => { vv }
    };
    let ci = build_context(sref, "gfs", None, "TMP", iso,
                           1000.0, 10800.0, TimeInterpMode::Linear);
    match ci {
        Err(_) => { assert!(false); return; }
        Ok(c) => {
            let v = evaluate_point(&c, 10.0, 350.0);
            match v {
                Err(_) => { assert!(false); return; }
                Ok(pv) => {
                    assert!(pv.value == 280.0);
                    assert!(pv.info.horizontal == HorizMethod::Exact);
                    match pv.info.time {
                        None => { assert!(false); return; }
                        Some(tm) => {
                            assert!(tm.forecast_hours.0 == 0.0 && tm.forecast_hours.1 == 6.0);
                        }
                    }
                }
            }
        }
    }
    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// (b) warm boot with a CORRUPT manifest.json
// ---------------------------------------------------------------------------

#[test]
fn corrupt_manifest_boot_is_loud_counted_and_still_answers_right() {
    let root = "/tmp/opencode/wxpoint-stat-corrupt-manifest".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "stat_small.grib2"),
                &read_bytes("../../tests/fixtures/stat_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, stat_cfg(&data, &ing).as_bytes());

    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);

    // Garbage where the manifest should be: unparsable JSON, NOT just empty.
    let run_dir = join_path(&join_path(&data, "stat"), "2026101912");
    write_bytes(&join_path(&run_dir, "manifest.json"),
                "{\"model\":\"stat\",\"fields\":[{\"param\":".as_bytes());

    let p2r = Pipeline::open(&cfg_path);
    let mut p2 = match p2r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    // LOUD note naming the file + exactly one error count (visible on /metrics).
    assert!(count_notes(&p2, "UNPARSABLE") == 1);
    assert!(p2.counters.errors == 1);

    let snap = p2.shared.get_index();
    let sref = &*snap;
    let sm = sref.find_model("stat");
    match sm {
        None => { assert!(false); return; }
        Some(m) => {
            let mut found = false;
            for fe in &m.runs[0].fields {
                if fe.param_key == "APCP" {
                    found = true;
                    assert!(fe.fd.stat_processed);
                    assert!(fe.fd.stat_kind == "accumulation".to_string());
                }
            }
            assert!(found);
        }
    }
    // Guard intact: mid-fh accumulation query is rejected, not blended.
    let cg = build_context(sref, "stat", None, "APCP", 1, 0.0, 10800.0, TimeInterpMode::Linear);
    match cg {
        Ok(_) => { assert!(false); return; }
        Err(e) => {
            match e {
                SelectError::StatTimeGuard(..) => {}
                _ => { assert!(false); return; }
            }
        }
    }
    // And the next ingest of this run rewrites the manifest from the headers.
    // (Content change re-arms exactly one ingest.)
    write_bytes(&join_path(&ing, "stat_small.grib2"),
                &read_bytes("../../tests/fixtures/stat_small_jpeg.grib2"));
    let n = settle(&mut p2, 1usize);
    assert!(n == 1usize);
    let man = read_bytes(&join_path(&run_dir, "manifest.json"));
    assert!(bytes_contains(&man, "\"stat_kind\":\"accumulation\"".as_bytes()));
    assert!(bytes_contains(&man, "\"fh_secs\":0,\"begin\":\"2026-10-19T12:00:00Z\",\"end\":\"2026-10-19T18:00:00Z\"".as_bytes()));
    rm_tree(&root);
}
