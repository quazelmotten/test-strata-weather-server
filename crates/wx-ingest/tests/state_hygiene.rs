// V1.2 phase C gate #6: PERSISTENT-STATE HYGIENE (spec §4.1).
//
// Two problems fixed:
//   * `StateStore.find` was a LINEAR SCAN over every file ever ingested, called
//     once per candidate per scan round. It is now an O(1) HashMap keyed by
//     (model, path) — the model namespace added by V1.3 phase E3b so two
//     models sharing an ingest dir no longer starve each other (`recs` stays
//     the SERIALIZED order; on-disk format is version 2 with a `model` key per
//     record, and pre-E3b VERSION-1 FILES ARE DISCARDED LOUDLY once, costing
//     exactly one re-ingest; inside v2 a missing `kind` still defaults to OK).
//   * The record list grew MONOTONICALLY: products deleted upstream left their
//     rows forever. `prune_missing` forgets records whose SOURCE FILE VANISHED,
//     bounded to ONE stat per record located under ONE model's ingest dir per
//     scan round, and NEVER re-arms a quarantined file that still exists.

use wx_ingest::state::{StateStore, FileRec, join_path, REC_OK, REC_IGNORED, REC_FAILED};
use wx_ingest::discovery::{path_to_str};
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::read_to_string;

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

#[allow(clippy::needless_return)]
fn index_of(hay: &str, pat: &str) -> i64 {
    let h = hay.as_bytes().to_vec();
    let p = pat.as_bytes().to_vec();
    if p.len() > h.len() {
        return -1;
    }
    for s in 0 .. (h.len() - p.len() + 1) {
        let mut ok = true;
        for k in 0 .. p.len() {
            if h[s + k] != p[k] {
                ok = false;
                break;
            }
        }
        if ok {
            return s as i64;
        }
    }
    return -1;
}

#[allow(clippy::needless_return)]
fn rec(path: String, size: u64, kind: u8) -> FileRec {
    return FileRec {
        model: "m".to_string(),
        path: path.to_string(),
        size: size,
        mtime_ns: 1000,
        hash: "00000000000000ab".to_string(),
        kind: kind,
    };
}

#[allow(clippy::needless_return)]
fn touch(path: String) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut h) => {
            let w = h.write_all("x".as_bytes());
            match w {
                Err(_) => { assert!(false); }
                Ok(_) => {}
            }
        }
    }
}

#[test]
fn find_uses_the_derived_index_and_stays_correct_across_structural_changes() {
    let root = "/tmp/opencode/wxpoint-state-idx".to_string();
    rm_tree(&root);
    let _c = create_dir_all(&join_path(&root, "data"));
    let st_dir = join_path(&join_path(&root, "data"), "state");
    let _c2 = create_dir_all(&st_dir);

    let mut st = match StateStore::load(st_dir.to_string()) {
        Err(_) => { assert!(false); return; }
        Ok(s) => { s }
    };
    // three records, TWO under one ingest dir, one under another
    st.upsert(rec("/root/a/f1.grib2".to_string(), 10, REC_OK));
    st.upsert(rec("/root/a/f2.grib2".to_string(), 20, REC_FAILED));
    st.upsert(rec("/root/b/f3.grib2".to_string(), 30, REC_IGNORED));
    // upsert of an EXISTING path must UPDATE IN PLACE (no duplicate row)
    st.upsert(rec("/root/a/f1.grib2".to_string(), 11, REC_OK));
    assert!(st.recs.len() == 3);

    // index-backed lookups agree with the record vector for EVERY row
    for k in 0 .. st.recs.len() {
        let want = st.recs[k].clone();
        match st.find("m", want.path.as_str()) {
            None => { assert!(false); return; }
            Some(got) => {
                assert!(got.size == want.size);
                assert!(got.kind == want.kind);
            }
        }
    }
    match st.find("m", "/root/a/f1.grib2") {
        None => { assert!(false); return; }
        Some(g) => { assert!(g.size == 11); }
    }
    assert!(st.recs[0].path == "/root/a/f1.grib2".to_string()); // ORDER PRESERVED

    // structural change rebuilds the index: drop everything under /root/a
    // (V1.3 phase E3b: drop_under is MODEL-scoped; every record here is model "m")
    assert!(st.drop_under("m", "/root/a") == 2);
    assert!(st.recs.len() == 1);
    match st.find("m", "/root/a/f1.grib2") {
        None => {}
        Some(_) => { assert!(false); return; }
    }
    match st.find("m", "/root/b/f3.grib2") {
        None => { assert!(false); return; }
        Some(g) => { assert!(g.kind == REC_IGNORED); }
    }
    rm_tree(&root);
}

#[test]
fn prune_missing_forgets_only_records_whose_source_actually_vanished() {
    let root = "/tmp/opencode/wxpoint-state-prune".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let sib = join_path(&root, "ing-archive"); // sibling whose NAME starts with `ing`
    let _c1 = create_dir_all(&ing);
    let _c2 = create_dir_all(&sib);
    let data = join_path(&root, "data");
    let _c3 = create_dir_all(&data);
    let st_dir = join_path(&data, "state");
    let _c4 = create_dir_all(&st_dir);

    // real files on disk
    let present = join_path(&ing, "live.grib2");
    touch(present.to_string());
    let kept_sib = join_path(&sib, "keep.grib2");
    touch(kept_sib.to_string());

    let mut st = match StateStore::load(st_dir.to_string()) {
        Err(_) => { assert!(false); return; }
        Ok(s) => { s }
    };
    st.upsert(rec(present.to_string(), 1, REC_OK));                        // exists -> KEPT
    st.upsert(rec(join_path(&ing, "gone.grib2"), 2, REC_OK));              // vanished -> DROPPED
    st.upsert(rec(join_path(&ing, "bad.grib2"), 3, REC_FAILED));           // vanished too
    st.upsert(rec(kept_sib.to_string(), 4, REC_IGNORED));                  // OTHER prefix
    assert!(st.recs.len() == 4);

    // ONE model's ingest dir per round: only rows UNDER it are stat'ed. The
    // sibling `ing-archive` is NOT under `ing` (prefix matched on `<dir>/`).
    assert!(st.prune_missing(&ing) == 2);
    assert!(st.recs.len() == 2);
    match st.find("m", present.as_str()) {
        None => { assert!(false); return; }
        Some(g) => { assert!(g.kind == REC_OK); }
    }
    match st.find("m", kept_sib.as_str()) {
        None => { assert!(false); return; }
        Some(g) => { assert!(g.kind == REC_IGNORED); }
    }
    match st.find("m", join_path(&ing, "gone.grib2").as_str()) {
        None => {}
        Some(_) => { assert!(false); return; }
    }

    // A QUARANTINED file that STILL EXISTS is never re-armed by pruning.
    let poison = join_path(&ing, "poison.grib2");
    touch(poison.to_string());
    st.upsert(rec(poison.to_string(), 5, REC_FAILED));
    assert!(st.prune_missing(&ing) == 0);
    match st.find("m", poison.as_str()) {
        None => { assert!(false); return; }
        Some(g) => { assert!(g.kind == REC_FAILED); }
    }
    rm_tree(&root);
}

#[test]
fn state_file_on_disk_is_byte_compatible_and_survives_a_restart_including_legacy() {
    let root = "/tmp/opencode/wxpoint-state-fmt".to_string();
    rm_tree(&root);
    let data = join_path(&root, "data");
    let st_dir = join_path(&data, "state");
    let _c1 = create_dir_all(&st_dir);

    let mut st = match StateStore::load(st_dir.to_string()) {
        Err(_) => { assert!(false); return; }
        Ok(s) => { s }
    };
    st.upsert(rec("/r/ok.grib2".to_string(), 1, REC_OK));
    st.upsert(rec("/r/ign.grib2".to_string(), 2, REC_IGNORED));
    st.upsert(rec("/r/bad.grib2".to_string(), 3, REC_FAILED));
    match st.save() {
        Err(_e) => { assert!(false); return; }
        Ok(_) => {}
    }

    // THE WRITER since V1.3 phase E3b: same keys, same ORDER, `kind` present,
    // VERSION IS NOW 2 and every record carries its `"model"` namespace key.
    let f = join_path(&st_dir, "files.json");
    let txt = match read_to_string(&f) {
        Err(_) => { assert!(false); return; }
        Ok(t) => { t }
    };
    assert!(txt.contains("{\"version\":2,\"recs\":["));
    assert!(txt.contains("\"kind\":0"));
    assert!(txt.contains("\"kind\":1"));
    assert!(txt.contains("\"kind\":2"));
    assert!(txt.contains("\"path\":\"/r/ok.grib2\""));
    assert!(txt.contains("\"hash\":\"00000000000000ab\""));
    assert!(txt.contains("\"model\":\"m\""));
    let a = index_of(&txt, "{\"path\":\"/r/ok.grib2\"");
    let b = index_of(&txt, "{\"path\":\"/r/bad.grib2\"");
    assert!(a >= 0);
    assert!(b >= 0);
    assert!(a < b); // insertion order preserved

    // restart: every verdict is remembered, so nothing is re-ingested
    let st2 = match StateStore::load(st_dir.to_string()) {
        Err(_) => { assert!(false); return; }
        Ok(s) => { s }
    };
    assert!(st2.recs.len() == 3);
    assert!(!st2.discarded_legacy);
    match st2.find("m", "/r/bad.grib2") {
        None => { assert!(false); return; }
        Some(g) => { assert!(g.kind == REC_FAILED); }
    }

    // LEGACY version-1 file (pre-E3b: no `model`, version 1): since E3b the
    // VERSION GATE DISCARDS it loudly instead of partially interpreting it —
    // one full re-ingest after the upgrade, exactly the documented migration.
    let legacy = "{\"version\":1,\"recs\":[{\"path\":\"/r/old.grib2\",\"size\":7,\"mtime_ns\":9,\"hash\":\"00000000000000cd\"}]}";
    write_bytes(&f, legacy.as_bytes());
    let st3_r = StateStore::load(st_dir.to_string());
    match st3_r {
        Err(_e) => { assert!(false); return; }
        Ok(s3) => {
            assert!(s3.discarded_legacy);
            assert!(s3.found_version == 1);
            assert!(s3.discarded_records == 1);
            assert!(s3.recs.len() == 0);
            match s3.find("m", "/r/old.grib2") {
                None => {}
                Some(_) => { assert!(false); return; }
            }
        }
    }

    // A CURRENT-version doc missing ONLY the `kind` key still defaults to OK
    // (the pre-V1.2 serde-default contract survives INSIDE version 2).
    let v2_nokind = "{\"version\":2,\"recs\":[{\"model\":\"m\",\"path\":\"/r/new.grib2\",\"size\":8,\"mtime_ns\":10,\"hash\":\"00000000000000ce\"}]}";
    write_bytes(&f, v2_nokind.as_bytes());
    let st4_r = StateStore::load(st_dir.to_string());
    match st4_r {
        Err(_e) => { assert!(false); return; }
        Ok(s4) => {
            assert!(!s4.discarded_legacy);
            assert!(s4.recs.len() == 1);
            match s4.find("m", "/r/new.grib2") {
                None => { assert!(false); return; }
                Some(g) => {
                    assert!(g.kind == REC_OK);
                    assert!(g.size == 8);
                    assert!(g.model == "m".to_string());
                }
            }
        }
    }
    rm_tree(&root);
}
