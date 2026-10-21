// V1.2 phase A gate #3: TEMP-FILE HYGIENE ON FAILURE PATHS.
//
// A failed atomic write must leave NO `.tmp.*` litter and must NOT touch the
// existing target. Two independent failure classes are exercised:
//   (a) the temp file cannot even be CREATED (read-only directory, via libc
//       `chmod`, since this std has no permission API);
//   (b) the write FAILS MID-FLIGHT (a plane whose length disagrees with nx*ny,
//       detected after earlier planes were already streamed into the temp file).
// Also pins the temp-name scheme itself (`<path>.tmp.<pid>.<seq>` uniqueness).
//
// Single test again: process-wide counters are asserted here too.

use wx_core::storage::{BinHeader, write_bin_atomic, open_field, unique_tmp_path,
                       counters_snapshot, counters_reset};
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::remove_file;
use std::fs::remove_dir;

// libc chmod: this toolchain's std exposes no permission API. Declared the same
// way as wx-core's own durability bindings (explicit #[link_name], i8 C string).
extern "C" {
    #[link_name = "chmod"]
    fn libc_chmod(path: *const i8, mode: u32) -> i32;
}

fn c_str(s: &str) -> Vec<i8> {
    let mut buf: Vec<i8> = Vec::<i8>::new();
    for b in s.as_bytes().to_vec() {
        buf.push(b as i8);
    }
    buf.push(0);
    return buf;
}

/// Count `.tmp.` entries directly inside `dir`.
fn count_tmp_in(dir: &str) -> usize {
    let mut hits: usize = 0;
    match read_dir(&dir) {
        Err(_) => { return hits; }
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let pp = de.path();
                        let ps = pp.to_str();
                        match ps {
                            None => {}
                            Some(s) => {
                                if s.contains(".tmp.") {
                                    hits += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    return hits;
}

/// Read every stored value of a field file (scalar path, independent of the
/// merge readback helper) so "the original is INTACT" is checked VALUE BY VALUE.
fn all_values(path: &str) -> Vec<f32> {
    let mf = open_field(path.to_string());
    match mf {
        Err(_) => { return [].to_vec(); }
        Ok(mf2) => {
            let mut out: Vec<f32> = Vec::<f32>::new();
            for li in 0 .. mf2.header.levels.len() {
                for j in 0 .. mf2.header.ny as usize {
                    for i in 0 .. mf2.header.nx as usize {
                        out.push(mf2.value(li, j, i));
                    }
                }
            }
            return out;
        }
    }
}

#[test]
fn failed_writes_leave_no_temp_litter_and_keep_the_original() {
    let root = "/tmp/opencode/wxpt-fail".to_string();
    let ro = format!("{0}/readonly", root);
    let rw = format!("{0}/writable", root);
    let _c1 = create_dir_all(&ro);
    let _c2 = create_dir_all(&rw);

    counters_reset();

    // ---- baseline field file in the WRITABLE directory -------------------
    let hdr = BinHeader {
        nx: 4,
        ny: 3,
        levels: [850.0].to_vec(),
        scan_ns: true,
        winds_grid: false,
    };
    let target = format!("{0}/field.bin", rw);
    let good: [f32; 12] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0];
    let refs: Vec<&[f32]> = [&good[..]].to_vec();
    let w0 = write_bin_atomic(target.to_string(), &hdr, &refs);
    assert!(w0.is_ok());
    let before = all_values(&target);
    assert!(before.len() == 12);
    let snap0 = counters_snapshot();
    assert!(snap0.renames == 1 && snap0.fsyncs == 1);

    // ---- (a) CREATE fails: read-only directory --------------------------
    let cs = c_str(&ro);
    let cm = unsafe { libc_chmod(&cs[0], 0o500) };
    assert!(cm == 0);
    let bad_a = format!("{0}/field.bin", ro);
    let ra = write_bin_atomic(bad_a, &hdr, &refs);
    assert!(ra.is_err());
    assert!(count_tmp_in(&ro) == 0);
    // nothing renamed, nothing fsynced (the open never happened)
    let snap_a = counters_snapshot();
    assert!(snap_a.renames == 1 && snap_a.fsyncs == 1);
    let _rest = unsafe { libc_chmod(&cs[0], 0o700) };

    // ---- (b) FAILURE mid-write: plane length disagrees with nx*ny -------
    // A 12-value plane against a 5x3 header: the writer streams what it got,
    // then rejects it, and must UNLINK the partially written temp file.
    let hdr_bad = BinHeader {
        nx: 5,
        ny: 3,
        levels: [850.0].to_vec(),
        scan_ns: true,
        winds_grid: false,
    };
    let rb = write_bin_atomic(target.to_string(), &hdr_bad, &refs);
    assert!(rb.is_err());
    // original file untouched, byte for byte / value for value
    let after = all_values(&target);
    assert!(after.len() == 12);
    let mut same = true;
    for k in 0 .. 12 {
        if after[k] != before[k] {
            same = false;
            break;
        }
    }
    assert!(same);
    assert!(count_tmp_in(&rw) == 0);
    let snap_b = counters_snapshot();
    assert!(snap_b.renames == 1); // still only the baseline rename
    assert!(snap_b.fsyncs == 1);  // the failed attempt never reached fsync

    // ---- (c) temp-name scheme: pid-tagged AND unique per call -----------
    let t1 = unique_tmp_path("/x/y/f.bin");
    let t2 = unique_tmp_path("/x/y/f.bin");
    assert!(t1 != t2);
    assert!(t1.contains(".tmp."));
    let pid = format!("{0}", std::process::id());
    assert!(t1.contains(&pid));
    assert!(t1.starts_with("/x/y/f.bin.tmp."));

    // teardown
    let _r1 = remove_file(&target);
    rm_tree(&root);
}

fn rm_tree(dir: &str) {
    match read_dir(&dir) {
        Err(_) => {}
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let pp = de.path();
                        let ps = pp.to_str();
                        match ps {
                            None => {}
                            Some(s) => {
                                let name = s.to_string();
                                match read_dir(&name) {
                                    Err(_) => {
                                        let _r = remove_file(&name);
                                    }
                                    Ok(_) => {
                                        rm_tree(&name);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let _d = remove_dir(&dir);
}
