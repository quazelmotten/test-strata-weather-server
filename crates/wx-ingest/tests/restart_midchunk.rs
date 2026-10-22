// V1.2 phase B gate #5 (granularity updated by V1.3 phase E3a): STATE SAVES
// SURVIVE A CRASH MID-CHUNK — at CHUNK granularity.
//
// `[ingest].decode_batch_files` (default 4) bounds how many candidate files are
// DECODED before ANY is written. Pre-E3a the persistent record was upserted AND
// SAVED after EVERY consumed FILE; E3a replaced that with a DIRTY flag persisted
// ONCE PER DECODE CHUNK plus at end of round (the per-file saves were O(N^2)
// bytes+fsyncs at scale). The crash-safety contract therefore now reads: a crash
// loses AT MOST the records of ONE unfinished chunk, and those re-ingest
// idempotently. This test simulates a crash inside the second chunk by truncating
// the on-disk state file to the prefix the FIRST chunk's save had durably
// written, then reopening: exactly the unsaved chunk's file is re-ingested —
// identical observable outcome to the pre-E3a per-file granularity for this
// scenario.
//
// Also gates the config validation of the new key (`decode_batch_files = 0` must
// fail loudly at startup).
//
// Single test in this binary: counter assertions are process-wide.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path, deserialize_text, serialize_text, StateDoc, FileRec};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::sync::atomic::Ordering;

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

// V1.2 phase D: per-FILE progress lines moved OUT of `log` into `progress_log`
// (they were being printed TWICE: once by the LIVE printer, once by the round
// streamer). Notes (skips, failures, recovery) stay in `log`.
fn count_progress(p: &Pipeline, marker: &str) -> usize {
    let mut hits = 0;
    for l in &p.progress_log {
        if l.contains(marker) {
            hits += 1;
        }
    }
    return hits;
}

fn count_notes(p: &Pipeline, marker: &str) -> usize {
    let mut hits = 0;
    for l in &p.log {
        if l.contains(marker) {
            hits += 1;
        }
    }
    return hits;
}

fn midchunk_cfg(data: &str, ing: &str, batch: &str) -> String {
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
fn per_file_saves_make_a_midchunk_restart_partial() {
    let root = "/tmp/opencode/wxpoint-midchunk".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    let src = read_bytes(&fixture("gfs_small.grib2"));
    write_bytes(&join_path(&ing, "a.grib2"), &src[..]);
    write_bytes(&join_path(&ing, "b.grib2"), &src[..]);
    write_bytes(&join_path(&ing, "c.grib2"), &src[..]);

    // ---- the new key is validated -----------------------------------------
    let bad_cfg = join_path(&root, "bad.toml");
    write_bytes(&bad_cfg, midchunk_cfg(&data, &ing, "0").as_bytes());
    match Pipeline::open(&bad_cfg) {
        Ok(_) => { assert!(false); return; }
        Err(e) => {
            let m = e.to_string();
            assert!(m.contains("decode_batch_files"));
        }
    }

    // ---- K = 2: three files arrive as chunks [a,b] then [c] --------------
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, midchunk_cfg(&data, &ing, "2").as_bytes());
    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(p.decode_batch_files == 2);

    assert!(p.scan_round() == 0);   // observation round
    assert!(p.scan_round() == 3);   // all three consumed, ONE publish each
    assert!(count_progress(&p, "ingested ") == 3);
    assert!(count_notes(&p, "ingested ") == 0);   // NOT in the streamed log
    assert!(p.live.progress_seq.load(Ordering::Relaxed) >= 3);
    assert!(p.counters.ingested_files == 3);
    assert!(p.counters.errors == 0);

    // ---- the state file on disk carries all THREE records ----------------
    let jpath = join_path(&join_path(&data, "state"), "files.json");
    let txt_r = read_to_string(&jpath);
    let txt = match txt_r {
        Err(_) => { assert!(false); return; }
        Ok(t0) => { t0.to_string() }
    };
    let doc_r = deserialize_text(&txt);
    let doc = match doc_r {
        Err(_) => { assert!(false); return; }
        Ok(d0) => { d0 }
    };
    assert!(doc.recs.len() == 3);

    // ---- simulated crash mid-chunk-2: only the FIRST chunk's batched save
    // (records for two files) reached the disk; chunk 2's save never ran -----
    let mut keep: Vec<FileRec> = Vec::<FileRec>::new();
    keep.push(doc.recs[0].clone());
    keep.push(doc.recs[1].clone());
    let nd = StateDoc { version: doc.version, recs: keep };
    write_bytes(&jpath, serialize_text(&nd).as_bytes());

    // ---- restart: ONLY the unsaved file is re-ingested ------------------
    let p2r = Pipeline::open(&cfg_path);
    let mut p2 = match p2r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    // Fresh StabilityTracker: the first round after a restart only OBSERVES.
    assert!(p2.scan_round() == 0);
    assert!(p2.scan_round() == 1);
    assert!(p2.counters.ingested_files == 1);
    assert!(p2.counters.errors == 0);
    assert!(count_progress(&p2, "ingested ") == 1);
    assert!(count_notes(&p2, "ingested ") == 0);
    // and a further round changes nothing at all
    assert!(p2.scan_round() == 0);
    assert!(p2.counters.ingested_files == 1);

    rm_tree(&root);
}
