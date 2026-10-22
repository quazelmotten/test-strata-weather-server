// V1.2 phase C gate #12: STARTUP FAILURES ARE MACHINE-OBSERVABLE (spec §3 "fail
// loudly"). Before this change the binary PRINTED its error and STILL exited WITH
// STATUS 0, so systemd / a k8s entrypoint / a CI smoke script could not tell a
// dead boot from a healthy one. These tests SPAWN the built `wxpoint` binary with
// deliberately broken startup conditions and assert the documented NON-ZERO codes
// (main.rs exit-code table): 2 = usage, 1 = config invalid (including the NEW
// data_dir / ingest_dir OVERLAP guard).
//
// V1.3 phase E3b adds the PROVISIONAL WIND-PAIR ENFORCEMENT code 7: a model
// configuring only ONE of UGRD/VGRD whose ingest dir was empty/unstable AT BOOT
// (so the boot preflight could only DEFER) must exit non-zero once its first
// grid-relative wind file is DECODED, instead of skipping winds + inflating
// errors forever. The two tail tests pin both halves: rc == 7 with the default
// `[ingest].wind_pair_enforce = true`, and NO early exit (killed by `timeout`,
// rc 124) when the key is set false while the violation note still fires.

use std::env::var;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::fs::read as read_file_bytes;
use wx_ingest::state::{join_path, stat_size_mtime};
use wx_ingest::discovery::{path_to_str};

extern "C" {
    #[link_name = "system"]
    fn libc_system(cmd: *const i8) -> i32;
}

#[allow(clippy::needless_return)]
fn c_str(s: String) -> Vec<i8> {
    let mut buf: Vec<i8> = Vec::<i8>::new();
    for b in s.as_bytes().to_vec() {
        buf.push(b as i8);
    }
    buf.push(0);
    return buf;
}

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
fn write_text(path: &str, text: &str) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(text.as_bytes());
            match n {
                Err(_) => { assert!(false); }
                Ok(_) => {}
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn file_exists(p: &str) -> bool {
    match stat_size_mtime(p) {
        Err(_) => { return false; }
        Ok(_) => { return true; }
    }
}

#[allow(clippy::needless_return)]
fn binary_path() -> String {
    // cargo exports CARGO_BIN_EXE_<name> to integration tests; fall back to the
    // debug output directory relative to this package's root.
    let v = var("CARGO_BIN_EXE_wxpoint");
    match v {
        Err(_) => {
            let fb = "../../target/debug/wxpoint".to_string();
            assert!(file_exists(&fb));
            return fb;
        }
        Ok(s) => {
            let p = s.to_string();
            assert!(file_exists(&p));
            return p;
        }
    }
}

#[allow(clippy::needless_return)]
fn cfg_with(data: &str, ing: &str, param: &str) -> String {
    return format!("[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"{0}\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"{1}\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"{2}\"\n  levels = [\"isobaric:*\"]\n", data, ing, param);
}

// Run `<bin> <args>` through the shell, capturing rc into a readable file.
#[allow(clippy::needless_return)]
fn run_and_capture(args: String, root: String) -> i64 {
    let bin = binary_path();
    return run_cmd(format!("env -u WXPOINT_CONFIG {0} {1}", bin, args), root);
}

// V1.3 phase E3b: the shared runner behind run_and_capture, taking the FULL
// shell command body so a test can prefix it (e.g. `timeout 12 ...` for a
// process that must NOT exit early). rc capture + parsing are identical.
#[allow(clippy::needless_return)]
fn run_cmd(cmd_body: String, root: String) -> i64 {
    let logf = join_path(&root, "run.log");
    let rcf = join_path(&root, "rc.txt");
    let cmd = format!("{0} > {1} 2>&1; echo rc=$? > {2}", cmd_body, logf, rcf);
    let buf = c_str(cmd);
    let _sys = unsafe { libc_system(&buf[0]) };
    let txt = match read_to_string(&rcf) {
        Err(_) => { assert!(false); return -999; }
        Ok(t) => { t }
    };
    // parse the digits following "rc="
    let mut v: i64 = 0;
    let mut seen = false;
    for k in 0 .. txt.len() {
        if txt[k .. k + 1] == *"=" {
            seen = true;
            continue;
        }
        if seen {
            match digit_of(&txt, k) {
                None => { break; }
                Some(dv) => { v = v * 10 + dv; }
            }
        }
    }
    return v;
}

#[allow(clippy::needless_return)]
fn digit_of(s: &str, at: usize) -> Option<i64> {
    if s[at .. at + 1] == *"0" { return Some(0); }
    if s[at .. at + 1] == *"1" { return Some(1); }
    if s[at .. at + 1] == *"2" { return Some(2); }
    if s[at .. at + 1] == *"3" { return Some(3); }
    if s[at .. at + 1] == *"4" { return Some(4); }
    if s[at .. at + 1] == *"5" { return Some(5); }
    if s[at .. at + 1] == *"6" { return Some(6); }
    if s[at .. at + 1] == *"7" { return Some(7); }
    if s[at .. at + 1] == *"8" { return Some(8); }
    if s[at .. at + 1] == *"9" { return Some(9); }
    return None;
}

#[test]
fn missing_config_argument_exits_with_the_usage_code() {
    let root = "/tmp/opencode/wxpoint-exit-usage".to_string();
    rm_tree(&root);
    let _c = create_dir_all(&root);
    let rc = run_and_capture("".to_string(), root.to_string());
    assert!(rc != 0);
    assert!(rc == 2);
    rm_tree(&root);
}

#[test]
fn a_nonexistent_config_file_exits_nonzero_instead_of_zero() {
    let root = "/tmp/opencode/wxpoint-exit-missing".to_string();
    rm_tree(&root);
    let _c = create_dir_all(&root);
    let args = format!("--config {0}/nope.toml", root);
    let rc = run_and_capture(args, root.to_string());
    assert!(rc != 0);
    assert!(rc == 1);
    rm_tree(&root);
}

#[test]
fn the_overlap_guard_rejects_startup_with_a_nonzero_code_naming_both_paths() {
    // THE PHASE-C GUARD END TO END: a config whose store lives INSIDE the delivery
    // directory must fail startup, not boot and self-ingest its own output.
    let root = "/tmp/opencode/wxpoint-exit-overlap".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "incoming");
    let _c = create_dir_all(&ing);
    let data = join_path(&ing, "store"); // nested inside ingest_dir
    let cfg = join_path(&root, "config.toml");
    write_text(&cfg, cfg_with(&data, &ing, "TMP").as_str());
    let args = format!("--config {0}", cfg);
    let rc = run_and_capture(args, root.to_string());
    assert!(rc != 0);
    assert!(rc == 1);

    // and the failure message names BOTH paths, as the spec's loud-failure rule asks
    let logf = join_path(&root, "run.log");
    let txt = match read_to_string(&logf) {
        Err(_) => { assert!(false); return; }
        Ok(t) => { t }
    };
    assert!(txt.contains("OVERLAP"));
    assert!(txt.contains(data.as_str()));
    assert!(txt.contains(ing.as_str()));
    rm_tree(&root);
}

#[test]
fn an_unknown_parameter_name_in_the_config_exits_with_the_config_code() {
    let root = "/tmp/opencode/wxpoint-exit-param".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "incoming");
    let _c = create_dir_all(&ing);
    let data = join_path(&root, "store");
    let cfg = join_path(&root, "config.toml");
    write_text(&cfg, cfg_with(&data, &ing, "NOTAPARAM").as_str());
    let args = format!("--config {0}", cfg);
    let rc = run_and_capture(args, root.to_string());
    assert!(rc != 0);
    assert!(rc == 1);
    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// V1.3 phase E3b — PROVISIONAL WIND-PAIR ENFORCEMENT (exit code 7)
// ---------------------------------------------------------------------------

/// hrrr model with ONLY UGRD configured (spec §4.4 half-pair). `extra` injects
/// the optional [ingest].wind_pair_enforce key; `bind` keeps parallel tests
/// off each other's port. stable_window_secs = 4 makes the FRESHLY-COPIED
/// fixture UNSTABLE at preflight time (mtime ~0 s old < 4 s window) so the
/// boot probe can only DEFER — the enforcement then has to come from DECODED
/// evidence, which is exactly the audit-defect-5 scenario.
#[allow(clippy::needless_return)]
fn wind_cfg(data: &str, ing: &str, bind: &str, extra: &str) -> String {
    let t = "[server]\nbind = \"";
    let t = format!("{0}{1}", t, bind);
    let t = t + "\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 1\nretention_runs = 4\nstable_window_secs = 4\n";
    let t = format!("{0}{1}", t, extra);
    let t = t + "\n[[models]]\nname = \"hrrr\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n";
    return t;
}

#[allow(clippy::needless_return)]
fn write_bytes(path: String, bytes: &[u8]) {
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

#[test]
fn deferred_wind_violation_exits_seven_after_the_decoding_round() {
    let root = "/tmp/opencode/wxpoint-exit-wind7".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "incoming");
    let data = join_path(&root, "store");
    let _c = create_dir_all(&ing);
    // Copy the grid-relative fixture IMMEDIATELY before spawn so its mtime is
    // ~now: the boot preflight sees one unstable candidate and defers (no rc
    // 4), while the scan rounds make it eligible ~window seconds later and the
    // decode fires the violation.
    let fx = read_file_bytes("../../tests/fixtures/hrrr_small.grib2");
    let bytes = match fx {
        Err(_) => { assert!(false); return; }
        Ok(b) => { b }
    };
    write_bytes(join_path(&ing, "hrrr_small.grib2"), &bytes[..]);
    let cfg = join_path(&root, "config.toml");
    write_text(&cfg, wind_cfg(&data, &ing, "127.0.0.1:8123", "").as_str());
    let args = format!("--config {0}", cfg);
    // run_and_capture BLOCKS until the process exits: default enforce=true
    // must kill it with code 7 once round ~window+1 decodes the winds. If the
    // defect regressed, the server would NEVER exit and this test would hang
    // (bounded by the outer cargo/CI timeout, as intended for a regression).
    let rc = run_and_capture(args, root.to_string());
    assert!(rc == 7);

    // The operator saw WHY: the decode-time note AND the FATAL exit line.
    let logf = join_path(&root, "run.log");
    let txt = match read_to_string(&logf) {
        Err(_) => { assert!(false); return; }
        Ok(t) => { t }
    };
    assert!(txt.contains("wind pair rule violated"));
    assert!(txt.contains("exit 7"));
    rm_tree(&root);
}

#[test]
fn wind_pair_enforce_false_keeps_the_note_but_never_exits_early() {
    let root = "/tmp/opencode/wxpoint-exit-windoff".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "incoming");
    let data = join_path(&root, "store");
    let _c = create_dir_all(&ing);
    let fx = read_file_bytes("../../tests/fixtures/hrrr_small.grib2");
    let bytes = match fx {
        Err(_) => { assert!(false); return; }
        Ok(b) => { b }
    };
    write_bytes(join_path(&ing, "hrrr_small.grib2"), &bytes[..]);
    let cfg = join_path(&root, "config.toml");
    write_text(&cfg, wind_cfg(&data, &ing, "127.0.0.1:8124",
                              "wind_pair_enforce = false\n").as_str());
    let bin = binary_path();
    // `timeout 12`: comfortably past the window(4 s)+rounds(~5-6 s) at which
    // the DECODED violation fires, yet the opted-out process must still be
    // RUNNING -> timeout kills it -> shell rc 124 (never 7).
    let body = format!("timeout 12 env -u WXPOINT_CONFIG {0} --config {1}", bin, cfg);
    let rc = run_cmd(body, root.to_string());
    assert!(rc == 124);

    // The documented sticky-note behavior survives opt-out: the violation IS
    // reported, only the process-level exit is declined.
    let logf = join_path(&root, "run.log");
    let txt = match read_to_string(&logf) {
        Err(_) => { assert!(false); return; }
        Ok(t) => { t }
    };
    assert!(txt.contains("wind pair rule violated"));
    assert!(!txt.contains("exit 7"));
    rm_tree(&root);
}
