// V1.3 phase E1 DEFECT GATE — LOUD rejection of STAT COLLISIONS.
//
// THE DEFECT (AUDIT-confirmed (1)): `find_group` keyed store groups on
// (run epoch, param, level type, forecast hour) ONLY. Two PDT 4.8 messages
// for the SAME param/level/fh but DIFFERENT statistical windows (a very
// plausible real delivery: a 0-6 h AND a 0-3 h accumulation for one valid
// time) landed in ONE group: `merge_field` kept the FIRST plane silently and
// the survivor's window was applied to data that did not come from it. No
// note, no counter, nothing observable.
//
// THE FIX (orchestrator decision, docs/DECISIONS.md V1.3 E1): the group key
// gained the full STATISTICAL IDENTITY (processed, kind, begin, end, step),
// so such messages are now DISTINCT groups — but storage layout is ONE file
// per (param, level_type, fh) and the query API has no range-selection
// parameter yet, so both cannot be stored under the current scheme. The
// write phase therefore CLAIMS each target path in MESSAGE ORDER: the first
// group wins, every later distinct group targeting an already-claimed path
// within the same source file is REJECTED LOUDLY — one note per occurrence
// naming both windows + the dedicated `stat_collisions` counter (live mirror
// + Prometheus `wxpoint_stat_collisions_total`) — never merged silently and
// never counted as an error. Range selection is documented future work.
//
// FIXTURE collide_small.grib2: APCP@surface fh 0 accumulation [0,6] h (plane
// value j*nx+i, node 10), then APCP@surface fh 0 accumulation [0,3] h (plane
// value j*nx+i+500, node 510 — visibly DIFFERENT data), then an unaffected
// TMP@2m instantaneous message (node 250). Pre-fix there was NO note and NO
// counter to assert (the fields did not exist); post-fix exactly one of each.

use wx_ingest::pipeline::{Pipeline, level_dir_name};
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_ingest::state::join_path;
use wx_ingest::discovery::{path_to_str};
use wx_core::tables::{lookup_level_type};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::fs::create_dir_all;
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

#[allow(clippy::needless_return)]
fn le_i64_at(b: &[u8], off: usize) -> i64 {
    let mut v: u64 = 0;
    for k in 0 .. 8 {
        v = v | (((b[off + k] as u64)) << (k * 8));
    }
    return v as i64;
}

fn col_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"col\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"collide_small.grib2\"\n\n  [[models.params]]\n  name = \"APCP\"\n  levels = [\"surface\"]\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn two_accumulation_windows_one_valid_time_first_wins_loudly() {
    let root = "/tmp/opencode/wxpoint-stat-collision".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "collide_small.grib2"),
                &read_bytes("../../tests/fixtures/collide_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, col_cfg(&data, &ing).as_bytes());

    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    // the file is still REC_OK material (a collision is a SKIP, not a
    // failure): exactly one consumed FILE, zero errors.
    assert!(settle(&mut p1, 1usize) == 1usize);
    assert!(p1.counters.errors == 0);

    // ---- THE LOUD REJECTION (pre-fix: nothing existed to assert) ----------
    assert!(p1.counters.stat_collisions == 1);
    assert!(p1.live.stat_collisions.load(Ordering::Relaxed) == 1);
    assert!(count_notes(&p1, "stat-collision") == 1);
    // only TWO field files were written (APCP f000 + TMP f000); the rejected
    // group counts neither as written nor as failed.
    assert!(p1.counters.groups_written == 2);

    // ---- FIRST-IN-MESSAGE-ORDER WINS: durable bytes prove it --------------
    // APCP/surface/f000.bin header carries the FIRST window [0, 21600] s and
    // the first plane (node value 10.0 — the loser's plane would read 510).
    let surf = level_dir_name(1);
    let run_dir = join_path(&join_path(&data, "col"), "2026101912");
    let f000 = join_path(&join_path(&run_dir, "APCP"), &join_path(&surf, "f000.bin"));
    let hb = read_bytes(&f000);
    assert!(hb.len() > 56);
    let ver = (hb[8] as u32) | ((hb[9] as u32) << 8) | ((hb[10] as u32) << 16) | ((hb[11] as u32) << 24);
    assert!(ver == 2); // BIN_VERSION
    assert!(hb[23] == 1); // STAT_ACCUMULATION
    assert!(le_i64_at(&hb, 28) == 0);      // begin_secs of the WINNER
    assert!(le_i64_at(&hb, 36) == 21600);  // end_secs   of the WINNER ([0,6] h)

    // queryable value = winner's plane at node (20 N, 240 E)
    let snap = p1.shared.get_index();
    let sref = &*snap;
    let c = build_context(sref, "col", None, "APCP", 1, 0.0, 0.0, TimeInterpMode::Linear);
    match c {
        Err(_) => { assert!(false); return; }
        Ok(ctx) => {
            let v = evaluate_point(&ctx, 20.0, 240.0);
            match v {
                Err(_) => { assert!(false); return; }
                Ok(qr) => {
                    assert!(qr.value == 10.0);
                    assert!(qr.info.horizontal == HorizMethod::Exact);
                }
            }
        }
    }

    // ---- manifest inventory shows ONLY the winner's window ----------------
    // [0,6] h ends 18:00Z; the rejected [0,3] h window ended 15:00Z and must
    // NOT appear anywhere.
    let man = join_path(&run_dir, "manifest.json");
    assert!(exists_any(&man));
    let mb = read_bytes(&man);
    assert!(bytes_contains(&mb, b"\"2026-10-19T18:00:00Z\""));
    assert!(!bytes_contains(&mb, b"\"2026-10-19T15:00:00Z\""));

    // ---- OTHER PARAMS IN THE SAME FILE UNAFFECTED -------------------------
    let agl = lookup_level_type("height_above_ground");
    match agl {
        None => { assert!(false); return; }
        Some(code) => {
            let t = build_context(sref, "col", None, "TMP", code, 2.0, 0.0,
                                  TimeInterpMode::Linear);
            match t {
                Err(_) => { assert!(false); return; }
                Ok(ctx) => {
                    let v = evaluate_point(&ctx, 20.0, 240.0);
                    match v {
                        Err(_) => { assert!(false); return; }
                        Ok(qr) => { assert!(qr.value == 250.0); }
                    }
                }
            }
        }
    }
}
