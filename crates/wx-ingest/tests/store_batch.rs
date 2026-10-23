// V1.2 phase A gate #4: BATCH MERGE WRITE COST.
//
// Merging a GROUP of K new levels into a field file that already holds L levels
// must cost ONE file write (ONE temp file + ONE rename), not K, and must write
// EXACTLY the resulting file's bytes once. This is the write-amplification fix:
// pre-change, every incoming message re-read the whole file (element by element)
// and rewrote it whole (twice materialized: planes + a full byte buffer).
//
// Single test in this binary: the counters are process-wide and cargo runs tests
// inside one binary concurrently.

use wx_ingest::store::{merge_write_field, merge_field};
use wx_core::storage::{open_field, expected_size, counters_snapshot, counters_reset,
                       StatMeta};
use std::fs::read_dir;
use std::fs::remove_file;
use std::fs::remove_dir;

const NX: u32 = 4;
const NY: u32 = 3;
const PLANE_BYTES: usize = 4 * 3 * 4; // nx*ny f32

/// Remove a leftover scratch tree so the test is REPEATABLE (the assertions
/// count renames/levels relative to an EMPTY field file).
fn wipe(dir: &str) {
    match read_dir(dir) {
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
                                        wipe(&name);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let _d = remove_dir(dir);
}

fn plane(base: f32) -> Vec<f32> {
    let mut v: Vec<f32> = Vec::<f32>::new();
    for k in 0 .. 12 {
        v.push(base + k as f32);
    }
    v
}

#[test]
fn batch_merge_of_k_levels_onto_l_existing_costs_one_write() {
    let dir = "/tmp/opencode/wxpt-batch".to_string();
    wipe(&dir);
    let _c = std::fs::create_dir_all(&dir);
    let path = format!("{0}/TMP_isobaric_f000.bin", dir);

    counters_reset();

    // ---- L = 3 existing levels, written as ONE batch (ONE rename) ----------
    let first: Vec<(f64, Vec<f32>)> = [
        (1000.0, plane(100.0)),
        (925.0, plane(200.0)),
        (850.0, plane(300.0)),
    ].to_vec();
    let r1 = merge_write_field(path.to_string(), 100, NX, NY, true, false, StatMeta::instantaneous(), first);
    assert!(r1.is_ok());
    match r1 {
        Err(_) => {}
        Ok(n) => { assert!(n == 3); }
    }
    let s1 = counters_snapshot();
    assert!(s1.renames == 1);
    // store.write_field still fsyncs the temp inode AND its directory (the
    // primitive no longer does the directory one; Phase B batches those).
    assert!(s1.fsyncs == 2);
    let bytes_after_first = s1.bytes_written;

    // ---- K = 2 new levels merged onto the L existing: ONE write ------------
    // 837.55 hPa is NOT exactly representable in f32 (that is the dedup bug's
    // witness, exercised in store_dedup.rs); 850.0 collides with an EXISTING
    // level so the group must REPLACE it, not append a duplicate.
    let second: Vec<(f64, Vec<f32>)> = [
        (837.55, plane(400.0)),
        (850.0, plane(900.0)),
    ].to_vec();
    let before = counters_snapshot();
    let r2 = merge_write_field(path.to_string(), 100, NX, NY, true, false, StatMeta::instantaneous(), second);
    assert!(r2.is_ok());
    let after = counters_snapshot();

    // ONE rename for the WHOLE group (this is the amplification fix) ...
    assert!(after.renames == before.renames + 1);
    // ... and bytes written bounded by the RESULTING FILE: L+K slots minus the
    // replaced level = 4 planes, plus the header. Nothing is written twice.
    let grew = after.bytes_written - before.bytes_written;
    assert!(grew as usize <= (4 + 1) * PLANE_BYTES + 64); // +1 slot of slack
    // exactness: the delta equals the resulting file size, byte for byte
    let mf = open_field(path.to_string());
    match mf {
        Err(_) => { panic!(); }
        Ok(field) => {
            assert!(field.header.levels.len() == 4);
            assert!(grew == expected_size(&field.header) as u64);
            // canonical DESCENDING order for isobaric levels preserved
            assert!(field.header.levels[0] == 1000.0);
            assert!(field.header.levels[1] == 925.0);
            assert!(field.header.levels[2] == 850.0);
            // 837.55 stored as f32 rounds to 837.5498...; compare as f32
            assert!(field.header.levels[3] == 837.55_f32);
            // the colliding level was REPLACED (new value wins), not duplicated
            let mut same_new = true;
            let mut same_old = false;
            for k in 0 .. 12 {
                let v = field.value(2, k / 4, k % 4);
                if v != 900.0 + k as f32 {
                    same_new = false;
                }
                if v == 300.0 + k as f32 {
                    same_old = true;
                }
            }
            assert!(same_new);
            assert!(!same_old);
            // the new level carries its own plane
            let mut ok_lvl3 = true;
            for k in 0 .. 12 {
                if field.value(3, k / 4, k % 4) != 400.0 + k as f32 {
                    ok_lvl3 = false;
                }
            }
            assert!(ok_lvl3);
        }
    }
    assert!(bytes_after_first > 0);

    // ---- merge_field alone performs NO write at all (read + merge only) ----
    let s_mid = counters_snapshot();
    let mg = merge_field(path.to_string(), 100, [(700.0, plane(700.0))].to_vec());
    assert!(mg.is_ok());
    let s_after_merge_only = counters_snapshot();
    assert!(s_after_merge_only.renames == s_mid.renames);
    assert!(s_after_merge_only.bytes_written == s_mid.bytes_written);
    match mg {
        Err(_) => {}
        Ok(pairs) => { assert!(pairs.len() == 5); }
    }
}
