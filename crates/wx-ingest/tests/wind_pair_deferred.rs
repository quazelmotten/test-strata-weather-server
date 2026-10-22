// V1.3 phase E3b DEFECT GATE #5 — THE WIND-PAIR RULE IS ENFORCED EVEN WHEN
// THE BOOT PREFLIGHT COULD NOT DECIDE.
//
// THE DEFECT (AUDIT-confirmed): the spec §4.4 wind-pair check ran ONLY at
// startup, against the first STABLE file matching the model glob. A model
// whose ingest dir was EMPTY (or unstable) at boot started fine; when its
// first grid-relative HRRR-style file later arrived, the decode-time rule
// noted the violation once, skipped every wind message, and inflated the
// errors counter FOREVER — the process limped on serving half-a-model with no
// machine-observable failure.
//
// THE FIX: the decode-time firing is now PROVISIONAL ENFORCEMENT — the
// pipeline sets the sticky `wind_violation` flag (already existed) AND main's
// ingest thread exits NON-ZERO (code 7, see exit_codes.rs) after the scan
// round that set it, unless `[ingest].wind_pair_enforce = false`. This binary
// pins the LIBRARY half of that contract: deferral at boot with an empty dir,
// the decoded-evidence flag + note + counters after the first consuming
// round, one-note stickiness, and the config flag default/override. The
// process-exit half is exercised by the spawned-binary tests in
// crates/wx-server/tests/exit_codes.rs.

use wx_ingest::config::{validate_text, startup_wind_preflight};
use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::File;
use std::io::Write;

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
        Err(_) => { assert!(false); return Vec::<u8>::new(); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { assert!(false); return Vec::<u8>::new(); }
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

/// One hrrr model configuring ONLY UGRD (the audit shape), plus TMP so the
/// scalar side still ingests and proves partial progress. `extra` injects the
/// optional [ingest] wind_pair_enforce key.
#[allow(clippy::needless_return)]
fn wd_cfg(data: &str, ing: &str, extra: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n";
    let t = format!("{0}{1}", t, extra);
    let t = t + "\n[[models]]\nname = \"hrrr\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n";
    return t;
}

// ---------------------------------------------------------------------------
// (a) empty dir at boot -> preflight DEFERS (Ok, not fatal); the first decode
//     of a grid-relative wind sets the sticky flag + note + counters
// ---------------------------------------------------------------------------

#[test]
fn empty_at_boot_defers_then_decoded_evidence_flags_the_violation() {
    let root = "/tmp/opencode/wxpoint-winddefer";
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    let cfg_text = wd_cfg(&data, &ing, "");
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg_text.as_bytes());

    // BOOT HALF: with an empty ingest dir the preflight cannot decide — it
    // must return Ok(deferral notes), NOT Err. Pre-E3b this deferral was the
    // END of the story (nothing ever revisited it); post-E3b it is provisional.
    let models_r = validate_text(&cfg_text);
    let models = match models_r {
        Err(e) => { println!("validate failed: {e}"); assert!(false); return; }
        Ok(mm) => { mm }
    };
    let now_ns = 1_760_000_000i64 * 1_000_000_000; // ~2025-10 epoch ns
    let pf = startup_wind_preflight(models, 0, now_ns);
    match pf {
        Err(msg) => { println!("preflight unexpectedly fatal: {msg}"); assert!(false); return; }
        Ok(notes) => {
            let joined = notes.join("\n");
            assert!(joined.contains("deferring"));
            assert!(joined.contains("no matching file present"));
        }
    }

    // The pipeline itself boots fine on the deferred verdict...
    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(e) => { println!("open failed: {e}"); assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(!p.wind_violation);
    // ...and ENFORCE IS ON BY DEFAULT (fail-closed choice, DECISIONS §14).
    assert!(p.wind_pair_enforce);

    // DELIVER the grid-relative fixture (LCC HRRR-style winds, flag bit 0x08).
    write_bytes(&join_path(&ing, "hrrr_small.grib2"), &read_bytes("../../tests/fixtures/hrrr_small.grib2"));
    assert!(p.scan_round() == 0);      // observation round
    let n2 = p.scan_round();
    assert!(n2 == 1);                  // consumed: scalars land, winds skipped

    // DECODED EVIDENCE fired: sticky flag (main's exit-7 trigger), exactly ONE
    // violation note, one error, and the dropped components counted as SKIPS
    // (not extra errors). Pre-E3b this same firing was the whole story — no
    // process-level consequence at all.
    assert!(p.wind_violation);
    assert!(count_notes(&p, "wind pair rule violated") == 1);
    assert!(p.counters.errors >= 1);
    assert!(p.counters.skipped_messages >= 1);
    // The scalar side still indexed (partial progress preserved).
    assert!(p.registry[0].runs.len() == 1);

    // A further round re-decodes nothing (record unchanged) and the note does
    // NOT repeat per round (sticky suppression).
    assert!(p.scan_round() == 0);
    assert!(count_notes(&p, "wind pair rule violated") == 1);
    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// (b) the opt-out key resolves into the pipeline flag
// ---------------------------------------------------------------------------

#[test]
fn wind_pair_enforce_key_defaults_true_and_honors_false() {
    let root = "/tmp/opencode/wxpoint-windenforce";
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    // default (key absent): TRUE
    let cfg1 = wd_cfg(&data, &ing, "");
    let c1 = join_path(&root, "c1.toml");
    write_bytes(&c1, cfg1.as_bytes());
    let pr1 = Pipeline::open(&c1);
    let p1 = match pr1 {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(p1.wind_pair_enforce);
    drop(p1);

    // explicit false: honored
    let cfg2 = wd_cfg(&data, &ing, "wind_pair_enforce = false\n");
    let c2 = join_path(&root, "c2.toml");
    write_bytes(&c2, cfg2.as_bytes());
    let pr2 = Pipeline::open(&c2);
    let p2 = match pr2 {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(!p2.wind_pair_enforce);
    rm_tree(&root);
}
