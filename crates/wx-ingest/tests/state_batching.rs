// V1.3 phase E3a DEFECT GATE #2 — STATE PERSISTENCE IS BATCHED, NOT PER FILE.
//
// THE DEFECT (AUDIT-confirmed): state.save() cloned + serialized EVERY record
// and fsynced after EVERY ingested / ignored / failed file. At thousands of
// files that is O(N^2) bytes written and fsynced per round for data that
// changes only linearly.
//
// THE FIX: mutations set a DIRTY flag; save() is a cheap no-op while clean.
// The pipeline persists ONCE PER DECODE CHUNK plus at end of round (which
// also serves as the pre-retention-prune persist). A FAILED save keeps the
// store dirty, logs loudly and counts ONE error without blocking ingestion —
// the next opportunity retries the same records. Worst case a crash between
// two opportunities loses at most one chunk's records, which re-ingest
// idempotently (merge dedup + atomic renames).
//
// This binary pins the exact save COUNTS through the new dedicated counters
// (wxpoint_state_writes_total's process-wide atomics) and the failure path.
// Single test: the counters are process-wide, so phases run sequentially.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path, state_counters_snapshot, deserialize_text};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::set_permissions;
use std::os::unix::fs::PermissionsExt;
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

#[allow(clippy::needless_return)]
fn batch_cfg(data: &str, ing: &str, batch: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ndecode_batch_files = ";
    let t = format!("{0}{1}", t, batch);
    let t = t + "\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn state_saves_are_per_chunk_not_per_file_and_failures_retry() {
    let root = "/tmp/opencode/wxpoint-statebatch".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    let src = read_bytes("../../tests/fixtures/gfs_small.grib2");
    write_bytes(&join_path(&ing, "a.grib2"), &src[..]);
    write_bytes(&join_path(&ing, "b.grib2"), &src[..]);
    write_bytes(&join_path(&ing, "c.grib2"), &src[..]);
    write_bytes(&join_path(&ing, "d.grib2"), &src[..]);
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, batch_cfg(&data, &ing, "2").as_bytes());

    let w0 = state_counters_snapshot().0;

    // ---- boot: rebuild_from_disk offers saves but NOTHING is dirty ---------
    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(e) => { println!("open failed: {e}"); panic!(); }
        Ok(pp) => { pp }
    };
    assert!(state_counters_snapshot().0 - w0 == 0);

    // ---- round 1: observation only — no records touched, zero writes -------
    assert!(p.scan_round() == 0);
    assert!(state_counters_snapshot().0 - w0 == 0);

    // ---- round 2: FOUR files consumed as TWO chunks of K=2 -----------------
    // Exactly TWO state writes: one per decode chunk. The round-end save is a
    // DIRTY-GATED NO-OP here because the second chunk's save already persisted
    // everything — that is the honest granularity E3a documents (pre-E3a this
    // was FOUR writes plus one final = FIVE full-file rewrites).
    assert!(p.scan_round() == 4);
    let w2 = state_counters_snapshot().0 - w0;
    assert!(w2 == 2);
    assert!(p.counters.errors == 0);
    assert!(!p.state.dirty);

    // ---- round 3: nothing changed — even the offered saves cost nothing ----
    assert!(p.scan_round() == 0);
    assert!(state_counters_snapshot().0 - w0 == 2);

    // on-disk inventory is complete despite the coarser cadence
    let jpath = join_path(&join_path(&data, "state"), "files.json");
    let txt = read_to_string(&jpath);
    match txt {
        Err(_) => { panic!(); }
        Ok(t0) => {
            let dr = deserialize_text(&t0);
            match dr {
                Err(_) => { panic!(); }
                Ok(doc) => { assert!(doc.recs.len() == 4); }
            }
        }
    }

    // ---- failure path: an unwritable state dir keeps records dirty ---------
    write_bytes(&join_path(&ing, "e.grib2"), &src[..]);
    let st_dir = join_path(&data, "state");
    let pm = set_permissions(&st_dir, PermissionsExt::from_mode(0o000));
    if pm.is_err() { panic!(); }
    // observe e (no upsert yet: still zero saves), then consume it: the
    // chunk-end AND round-end attempts both fail loudly, ingestion proceeds.
    assert!(p.scan_round() == 0);
    assert!(p.scan_round() == 1);
    assert!(p.state.dirty);
    assert!(p.counters.ingested_files == 5);
    // two failed ATTEMPTS this round (chunk end + round end), each ONE note
    // and ONE error — the retry semantics are visible, not silent.
    assert!(p.counters.errors == 2);
    assert!(count_notes(&p, "state save FAILED") == 2);

    // writable again: the NEXT round-end save flushes the same records —
    // nothing was lost, the store simply stayed dirty meanwhile.
    let pr2 = set_permissions(&st_dir, PermissionsExt::from_mode(0o755));
    if pr2.is_err() { panic!(); }
    assert!(p.scan_round() == 0);
    assert!(!p.state.dirty);
    let txt2 = read_to_string(&jpath);
    match txt2 {
        Err(_) => { panic!(); }
        Ok(t1) => {
            let dr2 = deserialize_text(&t1);
            match dr2 {
                Err(_) => { panic!(); }
                Ok(doc2) => { assert!(doc2.recs.len() == 5); }
            }
        }
    }
    // bytes counter moved with the write counter (both gauges exist for the
    // /metrics exposition; exact byte counts depend on path lengths).
    let snap = state_counters_snapshot();
    assert!(snap.0 > w2);
    assert!(snap.1 > 0);

    rm_tree(&root);
}
