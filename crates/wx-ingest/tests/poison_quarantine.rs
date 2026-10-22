// V1.2 phase B gate #3: POISON QUARANTINE.
//
// Two classes of unusable candidate are recorded IN the persistent state file
// instead of being retried forever:
//   * magic-byte precheck fails (a `.idx` sidecar delivered next to the product
//     and matching the model glob) -> kind=IGNORED, NEVER DECODED, logged ONCE;
//   * decode fails (truncated GRIB stream) -> kind=FAILED recorded WITH its
//     content hash, so later rounds skip it until its CONTENT changes.
// `errors` therefore advances ONCE per new/changed corrupt file, not once per
// scan round, and a restart preserves both verdicts.
//
// Single test in this binary: counter assertions are process-wide.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path, REC_OK, REC_IGNORED, REC_FAILED};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

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

// V1.2 phase D: per-FILE OUTCOME lines (ingested / failed-quarantined / ignored /
// skipped) moved out of `log` into `progress_log`, because they were printed TWICE
// (LIVE printer + round streamer). NOTES (magic-byte precheck, recovery, skips
// counted per message) stay in `log`. Both channels are asserted separately below.
fn count_progress(p: &Pipeline, marker: &str) -> usize {
    let mut hits = 0;
    for l in &p.progress_log {
        if l.contains(marker) {
            hits += 1;
        }
    }
    return hits;
}

/// How many note-log lines mention a given marker.
fn count_notes(p: &Pipeline, marker: &str) -> usize {
    let mut hits = 0;
    for l in &p.log {
        if l.contains(marker) {
            hits += 1;
        }
    }
    return hits;
}

fn quarantine_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_*\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn poison_files_are_quarantined_not_retried() {
    let root = "/tmp/opencode/wxpoint-poison".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    let good = join_path(&ing, "gfs_small.grib2");
    let sidecar = join_path(&ing, "gfs_small.idx");
    let bad = join_path(&ing, "gfs_bad.grib2");

    write_bytes(&good, &read_bytes(&fixture("gfs_small.grib2")));
    // sidecar matching the glob, NOT a GRIB stream (no `GRIB` magic)
    write_bytes(&sidecar, "GRIDLINES=4\nNX=8\nNY=5\n".as_bytes());
    // truncated GRIB stream: valid magic, incomplete section 7
    let whole = read_bytes(&fixture("gfs_small.grib2"));
    let half = whole.len() / 2;
    let mut head: Vec<u8> = Vec::<u8>::new();
    let mut k = 0;
    while k < half {
        head.push(whole[k]);
        k += 1;
    }
    write_bytes(&bad, &head[..]);

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, quarantine_cfg(&data, &ing).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };

    assert!(p.scan_round() == 0);   // observation round
    assert!(p.scan_round() == 1);   // ONE file ingested

    // ---- verdicts after the FIRST ingest round ----------------------------
    assert!(p.counters.ingested_files == 1);
    assert!(p.counters.ignored_files == 1);
    assert!(p.counters.quarantined_files == 1);
    assert!(p.counters.errors == 1);
    assert!(count_notes(&p, "never decoded") == 1);
    assert!(count_progress(&p, "quarantined") == 1);
    assert!(count_notes(&p, "quarantined") == 0);   // live channel only, never replayed

    let rg = p.state.find(good.to_string());
    let rs = p.state.find(sidecar.to_string());
    let rb = p.state.find(bad.to_string());
    match rg {
        None => { assert!(false); return; }
        Some(r) => { assert!(r.kind == REC_OK); }
    }
    match rs {
        None => { assert!(false); return; }
        Some(r) => { assert!(r.kind == REC_IGNORED); }
    }
    match rb {
        None => { assert!(false); return; }
        Some(r) => { assert!(r.kind == REC_FAILED); }
    }

    // ---- rounds 2..5: NOTHING is re-decoded, NO error is re-counted ------
    let mut rounds = 0;
    while rounds < 4 {
        assert!(p.scan_round() == 0);
        rounds += 1;
    }
    assert!(p.counters.errors == 1);
    assert!(p.counters.quarantined_files == 1);
    assert!(p.counters.ignored_files == 1);
    assert!(p.counters.ingested_files == 1);
    assert!(count_progress(&p, "quarantined") == 1);
    assert!(count_notes(&p, "quarantined") == 0);   // live channel only, never replayed
    assert!(count_notes(&p, "never decoded") == 1);

    // ---- content change RE-ARMS the quarantined file ---------------------
    let mut head2: Vec<u8> = Vec::<u8>::new();
    let third = whole.len() / 3;
    let mut j = 0;
    while j < third {
        head2.push(whole[j]);
        j += 1;
    }
    write_bytes(&bad, &head2[..]);
    assert!(p.scan_round() == 0);  // size changed -> stability window resets
    assert!(p.scan_round() == 0);  // retried, still broken
    assert!(p.counters.errors == 2);            // ONCE more, not once per round
    assert!(p.counters.quarantined_files == 2); // new FAILED record w/ new hash

    // ---- the state file itself carries the kind column -------------------
    let stj = read_bytes(&join_path(&join_path(&data, "state"), "files.json"));
    assert!(contains_bytes(&stj, "\"kind\":1".as_bytes()));
    assert!(contains_bytes(&stj, "\"kind\":2".as_bytes()));
    assert!(contains_bytes(&stj, "\"kind\":0".as_bytes()));

    // ---- restart preserves the verdicts ---------------------------------
    let p2r = Pipeline::open(&cfg_path);
    let mut p2 = match p2r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(p2.scan_round() == 0);
    assert!(p2.counters.errors == 0);           // no NEW decode attempt
    assert!(p2.counters.ingested_files == 0);
    match p2.state.find(bad.to_string()) {
        None => { assert!(false); return; }
        Some(r) => { assert!(r.kind == REC_FAILED); }
    }
    match p2.state.find(sidecar.to_string()) {
        None => { assert!(false); return; }
        Some(r) => { assert!(r.kind == REC_IGNORED); }
    }

    rm_tree(&root);
}

fn contains_bytes(hay: &[u8], pat: &[u8]) -> bool {
    if pat.len() > hay.len() {
        return false;
    }
    for s in 0 .. (hay.len() - pat.len() + 1) {
        let mut ok = true;
        let mut k = 0;
        while k < pat.len() {
            if hay[s + k] != pat[k] {
                ok = false;
                break;
            }
            k += 1;
        }
        if ok {
            return true;
        }
    }
    return false;
}
