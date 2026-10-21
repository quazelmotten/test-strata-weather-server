// V1.2 phase A gate #2: WRITER INSTRUMENTATION + DIRECTORY-FSYNC BATCHING.
//
// This file holds exactly ONE test on purpose: the counters are process-wide,
// and cargo runs the tests inside one binary CONCURRENTLY, so every counter
// assertion lives in this single sequential test (other phase-A gates that need
// counters get their own files/binaries too).

use wx_core::storage::{BinHeader, write_bin_atomic, write_bin_atomic_stream, PlaneSource,
                       TouchedDirs, counters_snapshot, counters_reset, expected_size};
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::metadata;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

struct OnePlane<'a> {
    pub data: &'a [f32],
    pub n: usize
}

impl<'a> PlaneSource for OnePlane<'a> {
    fn next_plane(self: &mut Self) -> Option<&[f32]> {
        if self.n <= 0 {
            return None;
        }
        self.n -= 1;
        return Some(&self.data[..]);
    }
}

/// How many files whose name contains `.tmp.` exist under `root` (depth 2).
fn count_tmp(root: &str) -> usize {
    let mut hits: usize = 0;
    match read_dir(&root) {
        Err(_) => { return hits; }
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let pp = de.path();
                        let p = pp.to_str();
                        match p {
                            None => {}
                            Some(s) => {
                                let name = s.to_string();
                                if name.contains(".tmp.") {
                                    hits += 1;
                                } else {
                                    // descend one level
                                    match read_dir(&name) {
                                        Err(_) => {}
                                        Ok(it2) => {
                                            for e2 in it2 {
                                                match e2 {
                                                    Err(_) => {}
                                                    Ok(de2) => {
                                                        let pp2 = de2.path();
                                                        let p2 = pp2.to_str();
                                                        match p2 {
                                                            None => {}
                                                            Some(s2) => {
                                                                if s2.contains(".tmp.") {
                                                                    hits += 1;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
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

// ---------------------------------------------------------------------------
// test
// ---------------------------------------------------------------------------

#[test]
fn writer_counters_and_batched_dir_fsync() {
    let root = "/tmp/opencode/wxpt-counters".to_string();
    let da = format!("{0}/a", root);
    let db = format!("{0}/b", root);
    let _c1 = create_dir_all(&da);
    let _c2 = create_dir_all(&db);

    counters_reset();
    let before = counters_snapshot();
    assert!(before.renames == 0 && before.bytes_written == 0 && before.fsyncs == 0);

    let hdr = BinHeader {
        nx: 4,
        ny: 3,
        levels: [850.0].to_vec(),
        scan_ns: true,
        winds_grid: false,
    };
    let plane: [f32; 12] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0];
    let want_bytes = expected_size(&hdr) as u64;
    let one_ref: Vec<&[f32]> = [&plane[..]].to_vec();

    // FOUR atomic field writes spread over TWO directories.
    let mut touched = TouchedDirs { dirs: [].to_vec() };
    for k in 0 .. 4 {
        let sub = if k < 2 { &da } else { &db };
        let p = format!("{0}/f{1}.bin", sub, k);
        let w = write_bin_atomic(p.to_string(), &hdr, &one_ref);
        assert!(w.is_ok());
        touched.note(p);
    }
    let mid = counters_snapshot();
    // ONE rename per write ...
    assert!(mid.renames == 4);
    // ... bytes = header + ONE plane each, no padding, no double write
    assert!(mid.bytes_written == 4 * want_bytes);
    // ... and EXACTLY ONE fsync per write (the temp inode): the per-rename
    // DIRECTORY fsync is GONE from the primitive.
    assert!(mid.fsyncs == 4);
    assert!(touched.pending_count() == 2);

    // BATCHED directory fsync: TWO distinct directories, ONE flush.
    let fl = touched.flush();
    assert!(fl.is_ok());
    let after_flush = counters_snapshot();
    assert!(after_flush.fsyncs == 6); // 4 temp-file + 2 directories
    assert!(after_flush.renames == 4);
    assert!(touched.pending_count() == 0);
    // a second flush costs NOTHING (list was cleared)
    let fl2 = touched.flush();
    assert!(fl2.is_ok());
    assert!(counters_snapshot().fsyncs == 6);

    // streaming entry point over a borrowed source counts the SAME WAY
    let p2 = format!("{0}/stream.bin", db);
    let w9 = write_bin_atomic_stream(p2, &hdr, 1, OnePlane { data: &plane[..], n: 1 });
    assert!(w9.is_ok());
    let s2 = counters_snapshot();
    assert!(s2.renames == 5);
    assert!(s2.bytes_written == 5 * want_bytes);
    assert!(s2.fsyncs == 7);

    // CONTRACT VIOLATIONS: fewer planes / more planes / wrong plane length must
    // FAIL, must NOT rename, and must leave NO temp litter behind.
    let p_bad = format!("{0}/bad.bin", da);
    let few = write_bin_atomic_stream(p_bad.to_string(), &hdr, 1,
                                      OnePlane { data: &plane[..], n: 0 });
    assert!(few.is_err());
    let many = write_bin_atomic_stream(format!("{0}/many.bin", da), &hdr, 1,
                                       OnePlane { data: &plane[..], n: 2 });
    assert!(many.is_err());
    let hdr2 = BinHeader {
        nx: 5,
        ny: 3,
        levels: [850.0].to_vec(),
        scan_ns: true,
        winds_grid: false,
    };
    let wronglen = write_bin_atomic_stream(format!("{0}/wrong.bin", da), &hdr2, 1,
                                           OnePlane { data: &plane[..], n: 1 });
    assert!(wronglen.is_err());
    let after_bad = counters_snapshot();
    assert!(after_bad.renames == 5); // no extra renames from failed writes
    assert!(count_tmp(&root) == 0); // and no .tmp.* files anywhere

    // the rejected targets do not exist either
    let b1 = format!("{0}/a/bad.bin", root);
    let b2 = format!("{0}/a/many.bin", root);
    let b3 = format!("{0}/a/wrong.bin", root);
    assert!(metadata(&b1).is_err());
    assert!(metadata(&b2).is_err());
    assert!(metadata(&b3).is_err());
}
