// V1.4 phase B2 DEFECT GATE #3 — data_dir FAIL-FAST + SINGLE-INSTANCE LOCK.
//
// (a) FAIL-FAST (AUDIT risk): Pipeline::open IGNORED the create_dir_all error
// for <data_dir>/state (pipeline.rs ~:1066 `let _c = ...`). An operator typo
// (data path under a regular file, unwritable mount, ENOSPC) produced a
// pipeline that BOOTS, re-decodes EVERYTHING every round because the state
// store cannot persist, and answers from a registry that dies at restart.
// THE FIX: hard-create data_dir + state dir, PROBE writability by writing a
// temp file under state/, and return Err (main's startup path keeps exit code
// 3) naming the failing path. The blob-under-a-file case is uid-independent
// (ENOTDIR even as root); the probe case uses chmod 000 and self-skips when
// euid == 0 would ignore it.
//
// (b) SINGLE-INSTANCE LOCK: two PROCESSES over one data_dir race state saves,
// retention deletions and atomic renames in ways nothing else guards. A
// best-effort advisory flock(2) LOCK_EX|LOCK_NB on <data_dir>/state/.wxpoint.lock
// is taken at open and held for the Pipeline lifetime (released when the LAST
// in-process handle drops: the fd close releases the kernel lock). Cross-process
// it is EXCLUSIVE with a clear error NAMING THE LOCK PATH.
//
// WHY A PROCESS-LOCAL REGISTRY: reality demanded it — MANY existing gates hold
// TWO live pipelines over ONE data_dir in the SAME process to simulate a
// restart without dropping the old one first (restart.rs opens p2 while p1 is
// STILL ALIVE; likewise field_upgrade.rs, poison_quarantine.rs, stat_durability
// .rs, hybrid_decode.rs, grid_upgrade.rs...). A plain per-open flock would
// EWOULDBLOCK the second handle IN-PROCESS and break those established warm-
// start gates. So opens are REF-COUNTED per canonicalized lock path: N opens
// OK in-process, exactly one flock holder, cross-process still exclusive.
//
// PRE-FIX DISCRIMINATORS: (1) and (3)'s first child run panic on b30d156
// (open SUCCEEDED where it must refuse); (2) panics pre-fix too (Err never
// came). The in-process coexistence test is green before AND after on purpose
// — it is the regression guard that KEEPS the registry honest.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::discovery::{path_to_str};
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::process::Command;

// libc surface via extern, the established style (disk_full_retry.rs,
// writer_failures.rs, exit_codes.rs all bind syscalls this way).
extern "C" {
    #[link_name = "chmod"]
    fn libc_chmod(path: *const i8, mode: u32) -> i32;
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

#[allow(clippy::needless_return)]
fn c_str(s: &str) -> Vec<i8> {
    let mut buf: Vec<i8> = Vec::<i8>::new();
    for b in s.as_bytes().to_vec() {
        buf.push(b as i8);
    }
    buf.push(0);
    return buf;
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
fn dd_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

// ---------------------------------------------------------------------------
// (1) data_dir path lies UNDER A REGULAR FILE: create fails -> open MUST Err
// ---------------------------------------------------------------------------

#[test]
fn data_dir_under_a_regular_file_fails_fast_at_open() {
    let root = "/tmp/opencode/wxpoint-b2-ddguard-blob";
    rm_tree(root);
    let _c = create_dir_all(root);
    // <root>/blob is a FILE; <root>/blob/data can NEVER be created (ENOTDIR
    // even for uid 0), so this discriminator is machine-independent.
    let blob = format!("{0}/blob", root);
    write_bytes(&blob, "not a directory".as_bytes());
    let ing = format!("{0}/ing", root);
    let _ci = create_dir_all(&ing);
    let cfg_path = format!("{0}/config.toml", root);
    write_bytes(&cfg_path, dd_cfg(blob.as_str(), &ing).as_bytes());

    // RED pre-fix: open() IGNORED the mkdir failure and returned Ok.
    match Pipeline::open(&cfg_path) {
        Ok(_p) => { panic!(); }
        Err(e) => {
            assert!(e.contains("data_dir"));
        }
    }
    rm_tree(root);
}

// ---------------------------------------------------------------------------
// (2) state dir exists but is NOT WRITABLE: the probe write must fail fast
// ---------------------------------------------------------------------------

#[test]
fn unwritable_state_dir_probe_fails_fast_at_open() {
    // chmod-000 only denies when we are NOT root; skip honestly if we are.
    if unsafe { libc_geteuid() } == 0 {
        println!("skip: euid 0 ignores chmod-000");
        return;
    }
    let root = "/tmp/opencode/wxpoint-b2-ddguard-probe";
    rm_tree(root);
    let ing = format!("{0}/ing", root);
    let data = format!("{0}/data", root);
    let _c = create_dir_all(&ing);
    let st_dir = format!("{0}/state", data);
    let _cs = create_dir_all(&st_dir);
    let cfg_path = format!("{0}/config.toml", root);
    write_bytes(&cfg_path, dd_cfg(&data, &ing).as_bytes());
    let cm = unsafe { libc_chmod(c_str(st_dir.as_str()).as_ptr(), 0o000) };
    assert!(cm == 0);

    // RED pre-fix: the store merely failed to LOAD/SAVE later; boot was Ok.
    let res = Pipeline::open(&cfg_path);
    let cm2 = unsafe { libc_chmod(c_str(st_dir.as_str()).as_ptr(), 0o755) };
    assert!(cm2 == 0);
    match res {
        Ok(_p) => { panic!(); }
        Err(e) => {
            assert!(e.contains("writable"));
        }
    }
    rm_tree(root);
}

// ---------------------------------------------------------------------------
// (3) cross-process single instance + in-process refcount coexistence
// ---------------------------------------------------------------------------

/// CHILD ENTRY POINT: re-executed as `--exact child_open_attempt_under_lock`
/// with WXPOINT_B2_CHILD_CFG=<config path>. Prints OPEN_OK or OPEN_ERR <msg>
/// so the parent can OBSERVE what a second process sees. When the harness runs
/// this test normally (env unset) it is a trivial pass.
#[test]
#[allow(clippy::needless_return)]
fn child_open_attempt_under_lock() {
    let cfgv = std::env::var("WXPOINT_B2_CHILD_CFG");
    match cfgv {
        Err(_) => { return; }
        Ok(cfg_path) => {
            match Pipeline::open(&cfg_path) {
                Ok(p) => {
                    println!("OPEN_OK lockpath={0}", p.data_dir);
                    drop(p);
                }
                Err(e) => {
                    println!("OPEN_ERR {0}", e);
                }
            }
            return;
        }
    }
}

#[allow(clippy::needless_return)]
fn spawn_child(cfg_path: String) -> String {
    let exe = std::env::current_exe();
    let ex = match exe {
        Err(_) => { panic!(); }
        Ok(v) => { v }
    };
    let out = Command::new(ex)
        .arg("--exact")
        .arg("child_open_attempt_under_lock")
        .arg("--nocapture")
        .env("WXPOINT_B2_CHILD_CFG", cfg_path.as_str())
        .output();
    match out {
        Err(_e) => { panic!(); }
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            let er = String::from_utf8_lossy(&o.stderr).to_string();
            s = format!("{0}{1}", s, er);
            return s;
        }
    }
}

#[test]
fn second_process_is_refused_while_the_first_holds_the_data_dir() {
    let root = "/tmp/opencode/wxpoint-b2-ddguard-lock";
    rm_tree(root);
    let ing = format!("{0}/ing", root);
    let data = format!("{0}/data", root);
    let _c = create_dir_all(&ing);
    let src = format!("{0}/gfs_small.grib2", ing);
    write_bytes(&src, read_bytes("../../tests/fixtures/gfs_small.grib2").as_slice());
    let cfg_path = format!("{0}/config.toml", root);
    write_bytes(&cfg_path, dd_cfg(&data, &ing).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };

    // A SECOND PROCESS must be refused, LOUDLY and by NAME of the lock file.
    // RED pre-fix: no lock exists, the child prints OPEN_OK -> panic here.
    let first = spawn_child(cfg_path.to_string());
    assert!(first.contains("OPEN_ERR"));
    assert!(first.contains(".wxpoint.lock"));
    // The lock file itself is the documented path under state/.
    match read_dir(format!("{0}/state", data)) {
        Err(_) => { panic!(); }
        Ok(it) => {
            let mut seen = false;
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let bn = match de.file_name().to_str() {
                            None => { "".to_string() }
                            Some(s) => { s.to_string() }
                        };
                        if bn == ".wxpoint.lock" {
                            seen = true;
                        }
                    }
                }
            }
            assert!(seen);
        }
    }
    // After the holder DROPS, the same child boots normally.
    drop(p);
    let second = spawn_child(cfg_path.to_string());
    assert!(second.contains("OPEN_OK"));
    rm_tree(root);
}

#[test]
fn same_process_may_hold_several_pipelines_over_one_data_dir() {
    // The warm-start gates (restart.rs et al.) rely on this; the registry must
    // NOT turn an in-process restart simulation into a lock refusal.
    let root = "/tmp/opencode/wxpoint-b2-ddguard-inproc";
    rm_tree(root);
    let ing = format!("{0}/ing", root);
    let data = format!("{0}/data", root);
    let _c = create_dir_all(&ing);
    let cfg_path = format!("{0}/config.toml", root);
    write_bytes(&cfg_path, dd_cfg(&data, &ing).as_bytes());

    let pa_r = Pipeline::open(&cfg_path);
    let _pa = match pa_r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    let pb_r = Pipeline::open(&cfg_path);
    let pb = match pb_r {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    // One more while TWO are alive: still fine (refcounted, one flock holder).
    drop(pb);
    let pc_r = Pipeline::open(&cfg_path);
    let _pc = match pc_r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    rm_tree(root);
}

#[allow(clippy::needless_return)]
fn read_bytes(path: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::<u8>::new();
    let f = File::open(path);
    match f {
        Err(_) => { return buf; }
        Ok(mut h) => {
            let r = std::io::Read::read_to_end(&mut h, &mut buf);
            match r {
                Err(_) => { return Vec::<u8>::new(); }
                Ok(_n) => { return buf; }
            }
        }
    }
}
