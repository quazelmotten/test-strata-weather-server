// V1.2 phase C gate #12: STARTUP FAILURES ARE MACHINE-OBSERVABLE (spec §3 "fail
// loudly"). Before this change the binary PRINTED its error and STILL exited WITH
// STATUS 0, so systemd / a k8s entrypoint / a CI smoke script could not tell a
// dead boot from a healthy one. These tests SPAWN the built `wxpoint` binary with
// deliberately broken startup conditions and assert the documented NON-ZERO codes
// (main.rs exit-code table): 2 = usage, 1 = config invalid (including the NEW
// data_dir / ingest_dir OVERLAP guard).

use std::env::var;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::read_to_string;
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
    let logf = join_path(&root, "run.log");
    let rcf = join_path(&root, "rc.txt");
    let cmd = format!("env -u WXPOINT_CONFIG {0} {1} > {2} 2>&1; echo rc=$? > {3}", bin, args, logf, rcf);
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
