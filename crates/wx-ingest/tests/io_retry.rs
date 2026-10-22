// V1.3 phase E3a DEFECT GATE #1 — TRANSIENT I/O IS NEVER PERMANENTLY QUARANTINED.
//
// THE DEFECT (AUDIT-confirmed): decode_file returns GribErr::Io for
// File::open / Mmap::map failures, and the scan_round Err branch recorded
// REC_FAILED keyed by content hash for ANY error INCLUDING Io. A transient
// EMFILE / ENOMEM / EIO / NFS-ESTALE / permission hiccup therefore quarantined
// a perfectly good file until its CONTENT changed — even though nothing about
// the content had ever been observed. It was also inconsistent with
// head_is_grib, which treats I/O failure as retryable.
//
// THE FIX: Io is split from content errors at every read stage (content hash,
// magic precheck, decode). An Io failure bumps a bounded IN-MEMORY streak per
// (path,size,mtime,hash): ONE note at the first occurrence, silent retries
// until IO_FAIL_ESCALATION_ROUNDS (5) consecutive failed rounds, then a LOUD
// escalation note + one `errors` + one `quarantined_files` increment + a
// kind=FAILED record so a permanently unreadable file stops hammering. Success
// or identity change resets the streak; a restart clears it. Records written
// at a stage where no hash could be taken carry an EMPTY hash, which makes
// the quarantine SELF-CLEAR once reading works again.
//
// DETERMINISTIC INJECTION: chmod 000 after the file has been OBSERVED but
// before it is consumed — the very next round's open fails with
// PermissionDenied, which must classify as Io (pinned directly against
// decode_file below). Rounds 2..5 retry with no record; round 6 escalates;
// restoring the mode re-ingests WITHOUT any content change.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path, REC_OK, REC_FAILED};
use wx_ingest::discovery::{path_to_str};
use wx_ingest::grib2::{decode_file, ParamFilter, GribErr};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::set_permissions;
use std::os::unix::fs::PermissionsExt;

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
fn io_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

// ---------------------------------------------------------------------------
// (a) classification: PermissionDenied from decode_file IS GribErr::Io
// ---------------------------------------------------------------------------

#[test]
fn permission_denied_decodes_as_io_not_content() {
    let root = "/tmp/opencode/wxpoint-io-classify".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let _c = create_dir_all(&ing);
    let src = read_bytes("../../tests/fixtures/gfs_small.grib2");
    let xp = join_path(&ing, "x.grib2");
    write_bytes(&xp.to_string(), &src);

    // readable: decodes fine (sanity).
    match decode_file(xp.to_string(), ParamFilter::All) {
        Err(e) => { println!("clean decode failed: {e:?}"); assert!(false); return; }
        Ok(_) => {}
    }
    let pm = set_permissions(&xp, PermissionsExt::from_mode(0o000));
    match pm {
        Err(_) => { assert!(false); return; }
        Ok(_) => {}
    }
    // unreadable: MUST be the Io class — never BadField/BadMagic/etc.
    match decode_file(xp.to_string(), ParamFilter::All) {
        Ok(_) => { println!("chmod-000 file decoded?!"); assert!(false); return; }
        Err(GribErr::Io) => {}
        Err(other) => { println!("wrong error class: {other:?}"); assert!(false); return; }
    }
    let pr = set_permissions(&xp, PermissionsExt::from_mode(0o644));
    match pr {
        Err(_) => { assert!(false); return; }
        Ok(_) => {}
    }
    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// (b) streak policy end to end: retry silently, escalate once, self-heal
// ---------------------------------------------------------------------------

#[test]
fn transient_io_retries_then_escalates_then_self_clears() {
    let root = "/tmp/opencode/wxpoint-io-retry".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    let src = read_bytes("../../tests/fixtures/gfs_small.grib2");
    let xp = join_path(&ing, "x.grib2");
    write_bytes(&xp.to_string(), &src);
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, io_cfg(&data, &ing).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(e) => { println!("open failed: {e}"); assert!(false); return; }
        Ok(pp) => { pp }
    };
    // round 1: OBSERVATION only (window 0 still needs one re-observation).
    assert!(p.scan_round() == 0);

    // The producer-side hiccup: unreadable from here on.
    let pm = set_permissions(&xp, PermissionsExt::from_mode(0o000));
    match pm {
        Err(_) => { assert!(false); return; }
        Ok(_) => {}
    }

    // Rounds 2..5: FOUR consecutive I/O failures — NO record, NO counters,
    // exactly ONE note total (the first occurrence).
    for r in 2 .. 6usize {
        assert!(p.scan_round() == 0, "round {r} unexpectedly consumed the file");
        match p.state.find(xp.to_string()) {
            Some(rec) => { println!("round {r} wrote record kind {} for an I/O failure", rec.kind); assert!(false); return; }
            None => {}
        }
        assert!(count_notes(&p, "retrying next round") == 1);
        assert!(count_notes(&p, "repeated I/O failure") == 0);
        assert!(p.counters.errors == 0);
        assert!(p.counters.quarantined_files == 0);
    }
    // the streak is real and bounded
    assert!(p.io_streaks.len() == 1);
    assert!(p.io_streaks[0].4 == 4);

    // Round 6: the FIFTH consecutive failed round escalates — loud note naming
    // the policy, one error + one quarantine, kind=FAILED with an EMPTY hash
    // (nothing about the bytes was ever observed), persisted by the round-end
    // batched save.
    assert!(p.scan_round() == 0);
    let rec = match p.state.find(xp.to_string()) {
        None => { println!("escalation wrote no record"); assert!(false); return; }
        Some(rr) => { rr }
    };
    assert!(rec.kind == REC_FAILED);
    assert!(rec.hash == "".to_string());
    assert!(count_notes(&p, "repeated I/O failure") == 1);
    assert!(p.counters.errors == 1);
    assert!(p.counters.quarantined_files == 1);
    let stxt = read_bytes(&join_path(&join_path(&data, "state"), "files.json"));
    let stxt_s = String::from_utf8_lossy(&stxt).to_string();
    assert!(stxt_s.contains("\"kind\":2"));

    // Reading works again WITHOUT any content change: the empty-hash record
    // cannot equal a real digest, so the file re-enters the normal path and
    // ingests — the pre-E3a design would have sat on it forever.
    let pr2 = set_permissions(&xp, PermissionsExt::from_mode(0o644));
    match pr2 {
        Err(_) => { assert!(false); return; }
        Ok(_) => {}
    }
    assert!(p.scan_round() == 1);
    assert!(p.counters.ingested_files == 1);
    let rec2 = match p.state.find(xp.to_string()) {
        None => { assert!(false); return; }
        Some(rr) => { rr }
    };
    assert!(rec2.kind == REC_OK);
    assert!(rec2.hash != "".to_string());
    // success cleared the streak; counters stay at the single escalation.
    assert!(p.io_streaks.is_empty());
    assert!(p.counters.errors == 1);
    // and it never comes back.
    assert!(p.scan_round() == 0);

    rm_tree(&root);
}
