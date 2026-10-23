// V1.2 phase D REGRESSION: A STABLE FILE MUST PRODUCE EXACTLY ONE INGEST *AND*
// EXACTLY ONE operator-visible progress line.
//
// WHY THIS EXISTS: the phase-D production-scale benchmark log appeared to show the
// SAME file "ingested" TWICE with IDENTICAL byte counts and IDENTICAL millisecond
// timings. Diagnosis proved NO second decode ever happened (`wxpoint_ingested_files_total`
// was 6 for 6 files, and a real re-decode cannot reproduce an identical timing), but
// the OUTPUT was genuinely misleading: per-FILE progress lines lived in `Pipeline.log`
// AND were echoed by the server's LIVE printer, so the round streamer replayed every
// one of them a SECOND time (and the LIVE printer could echo its newest message a
// THIRD time). Fix: progress lines live in `Pipeline.progress_log` + a bounded FIFO
// in `LiveCounters`; the LIVE printer DRAINS the FIFO (nothing lost, nothing repeated)
// and the round streamer only ever prints NOTES.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::discovery::{path_to_str};
use wx_ingest::state::{REC_OK};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::sync::atomic::Ordering;

fn fixture(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

fn read_bytes(path: &str) -> Vec<u8> {
    let f = File::open(path);
    match f {
        Err(_) => { Vec::<u8>::new()}
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { Vec::<u8>::new()}
                Ok(region) => {
                    let mut copy: Vec<u8> = Vec::<u8>::new();
                    for k in 0 .. region.len() {
                        copy.push(region[k]);
                    }
                    copy
                }
            }
        }
    }
}

fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!();}
        Ok(mut w) => {
            let n = w.write_all(bytes);
            if n.is_err() { panic!(); }
        }
    }
}

fn join2(a: &str, b: &str) -> String {
    format!("{0}/{1}", a, b)
}

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

fn count_in(v: &Vec<String>, marker: &str) -> usize {
    let mut hits = 0;
    for l in v {
        if l.contains(marker) {
            hits += 1;
        }
    }
    hits
}

fn tiny_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ndecode_batch_files = 4\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n"
}

#[test]
fn stable_file_ingests_once_and_prints_once() {
    let root = "/tmp/opencode/wxpoint-progressonce".to_string();
    rm_tree(&root);
    let ing = join2(&root, "ing");
    let data = join2(&root, "data");
    let _c = create_dir_all(&ing);
    let src = read_bytes(&fixture("gfs_small.grib2"));
    assert!(!src.is_empty());
    let target = join2(&ing, "a.grib2");
    write_bytes(&target, &src[..]);

    let cfg_path = join2(&root, "config.toml");
    write_bytes(&cfg_path, tiny_cfg(&data, &ing).as_bytes());
    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };

    // ---- FOUR scan rounds over an UNCHANGED file --------------------------
    assert!(p.scan_round() == 0);   // observation round (stability window)
    assert!(p.scan_round() == 1);   // the delivery is consumed ONCE
    assert!(p.scan_round() == 0);   // unchanged: identity match, NO re-decode
    assert!(p.scan_round() == 0);

    // exactly ONE decode...
    assert!(p.counters.ingested_files == 1);
    // ...and EXACTLY ONE persistent state record for that path, kind OK. (Had a
    // second DECODE really happened, this is where the evidence would live: either
    // a second record for the same path or a churned identity record.)
    let mut nrec = 0;
    for r in &p.state.recs {
        if r.path == target {
            nrec += 1;
        }
    }
    assert!(nrec == 1);
    assert!(p.state.recs.len() == 1);
    match p.state.find("gfs", target.as_str()) {
        None => { panic!(); }
        Some(r) => { assert!(r.kind == REC_OK); }
    }
    assert!(p.counters.errors == 0);
    // ... and exactly ONE progress RECORD for it, never in the streamed note log
    assert!(count_in(&p.progress_log, "ingested ") == 1);
    assert!(count_in(&p.log, "ingested ") == 0);
    assert!(count_in(&p.log, "groups,") == 0);
    assert!(p.live.progress_seq.load(Ordering::Relaxed) == 1);

    // ---- the operator-visible stream: drained ONCE, then EMPTY forever ----
    let first = p.live.drain_progress();
    assert!(first.len() == 1);
    assert!(count_in(&first, "a.grib2") == 1);
    let second = p.live.drain_progress();
    assert!(second.is_empty());
    // three more rounds on the same bytes add NOTHING to either channel
    assert!(p.scan_round() == 0);
    assert!(p.scan_round() == 0);
    assert!(p.live.progress_seq.load(Ordering::Relaxed) == 1);
    assert!(p.live.drain_progress().is_empty());
    assert!(p.counters.ingested_files == 1);

    // ---- a CONTENT CHANGE re-arms EXACTLY ONE ingest + ONE progress line --
    // (same path, different packing of the same field set: gfs_small_c2 is DRT 5.2)
    let changed = read_bytes(&fixture("gfs_small_c2.grib2"));
    assert!(!changed.is_empty());
    write_bytes(&target, &changed[..]);
    // a changed identity RESETS the stability window, so the re-consume lands in
    // whichever round first sees it STABLE again: exactly ONE across the pair.
    let r1 = p.scan_round();
    let r2 = p.scan_round();
    assert!(r1 + r2 == 1);
    assert!(p.counters.ingested_files == 2);
    assert!(count_in(&p.progress_log, "ingested ") == 2);
    let third = p.live.drain_progress();
    assert!(third.len() == 1);
    assert!(p.live.drain_progress().is_empty());

    let _f = remove_file(&cfg_path);
    rm_tree(&root);
}
