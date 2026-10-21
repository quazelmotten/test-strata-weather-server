// V1.2 phase A gate #5: LEVEL DEDUP + SLICE READBACK CORRECTNESS.
//
// (a) Re-ingesting a level must REPLACE its plane, never APPEND a duplicate.
//     The witness is a level coordinate that is NOT exactly representable in f32
//     (837.55 hPa): the file stores the f32 rounding (837.5498...), so the old
//     EXACT-f64 comparison missed its own stored value and duplicated the plane.
//     The canonical key now rounds BOTH sides to f32 — exactly what the format
//     can distinguish.
// (b) The mmap readback via `plane_bytes` slices must reproduce the SAME VALUES
//     as the scalar `MmapField.value` path (bit-exactness gate for the audit fix)
//     and the canonical ORDERING rule (isobaric DESCENDING, others ASCENDING)
//     must survive repeated merges.

use wx_ingest::store::{merge_field, merge_write_field};
use wx_core::storage::{open_field,};
use std::fs::read_dir;
use std::fs::remove_file;
use std::fs::remove_dir;

const NX: u32 = 4;
const NY: u32 = 3;

/// Remove a leftover scratch tree so the test is REPEATABLE across runs.
fn wipe(dir: &str) {
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
    let _d = remove_dir(&dir);
}

fn plane(base: f32) -> Vec<f32> {
    let mut v: Vec<f32> = Vec::<f32>::new();
    for k in 0 .. 12 {
        v.push(base + k as f32);
    }
    return v;
}

/// Read every value of one level plane through the SCALAR accessor.
fn scalar_plane(path: &str, li: usize) -> Vec<f32> {
    let mf = open_field(path.to_string());
    match mf {
        Err(_) => { return [].to_vec(); }
        Ok(field) => {
            let mut out: Vec<f32> = Vec::<f32>::new();
            for j in 0 .. field.header.ny as usize {
                for i in 0 .. field.header.nx as usize {
                    out.push(field.value(li, j, i));
                }
            }
            return out;
        }
    }
}

#[test]
fn reingesting_a_level_replaces_it_instead_of_duplicating() {
    let dir = "/tmp/opencode/wxpt-dedup".to_string();
    wipe(&dir);
    let _c = std::fs::create_dir_all(&dir);
    let path = format!("{0}/TMP_isobaric_f006.bin", dir);

    // 837.55 hPa: f32 storage rounds it, so disk value != requested f64.
    let r1 = merge_write_field(path.to_string(), 100, NX, NY, true, false,
                               [(837.55, plane(10.0))].to_vec());
    assert!(r1.is_ok());

    // (1) same f64 again -> still ONE level, plane REPLACED
    let r2 = merge_write_field(path.to_string(), 100, NX, NY, true, false,
                               [(837.55, plane(20.0))].to_vec());
    assert!(r2.is_ok());
    let mf1 = open_field(path.to_string());
    match mf1 {
        Err(_) => { assert!(false); }
        Ok(f) => {
            assert!(f.header.levels.len() == 1);
            let mut replaced = true;
            for k in 0 .. 12 {
                if f.value(0, k / 4, k % 4) != 20.0 + k as f32 {
                    replaced = false;
                }
            }
            assert!(replaced);
        }
    }

    // (2) THE ROUND-TRIP CASE the old code got wrong: feed back the level
    // coordinate AS THE FILE STORES IT (f32 widened to f64). Exact f64 equality
    // fails here (837.55 != 837.5498046875), f32-rounded equality holds.
    let stored = open_field(path.to_string());
    let mut stored_lv: f64 = 0.0;
    match stored {
        Err(_) => { assert!(false); }
        Ok(f) => { stored_lv = f.header.levels[0] as f64; }
    }
    assert!(stored_lv != 837.55); // proves the f64 values really differ
    let r3 = merge_write_field(path.to_string(), 100, NX, NY, true, false,
                               [(stored_lv, plane(30.0))].to_vec());
    assert!(r3.is_ok());
    let mf3 = open_field(path.to_string());
    match mf3 {
        Err(_) => { assert!(false); }
        Ok(f) => {
            assert!(f.header.levels.len() == 1); // NOT duplicated
            let mut replaced = true;
            for k in 0 .. 12 {
                if f.value(0, k / 4, k % 4) != 30.0 + k as f32 {
                    replaced = false;
                }
            }
            assert!(replaced);
        }
    }

    // (3) the named 837.5 path (exactly representable) keeps working too
    let p2 = format!("{0}/TMP2_isobaric_f000.bin", dir);
    let a = merge_write_field(p2.to_string(), 100, NX, NY, true, false,
                              [(837.5, plane(1.0)), (1000.0, plane(2.0))].to_vec());
    assert!(a.is_ok());
    let b = merge_write_field(p2.to_string(), 100, NX, NY, true, false,
                              [(837.5, plane(9.0))].to_vec());
    assert!(b.is_ok());
    match b {
        Err(_) => {}
        Ok(n) => { assert!(n == 2); }
    }
}

#[test]
fn duplicates_inside_one_batch_resolve_first_entry_wins() {
    let dir = "/tmp/opencode/wxpt-dedup-batch".to_string();
    wipe(&dir);
    let _c = std::fs::create_dir_all(&dir);
    let path = format!("{0}/TMP_isobaric_f012.bin", dir);
    // same level twice in ONE group: FIRST entry must win (V1.1 rule preserved)
    let grp: Vec<(f64, Vec<f32>)> = [
        (850.0, plane(11.0)),
        (850.0, plane(22.0)),
    ].to_vec();
    let r = merge_write_field(path.to_string(), 100, NX, NY, true, false, grp);
    assert!(r.is_ok());
    let mf = open_field(path.to_string());
    match mf {
        Err(_) => { assert!(false); }
        Ok(f) => {
            assert!(f.header.levels.len() == 1);
            let mut first_won = true;
            for k in 0 .. 12 {
                if f.value(0, k / 4, k % 4) != 11.0 + k as f32 {
                    first_won = false;
                }
            }
            assert!(first_won);
        }
    }
}

#[test]
fn slice_readback_matches_scalar_readback_and_order_survives_merges() {
    let dir = "/tmp/opencode/wxpt-readback".to_string();
    wipe(&dir);
    let _c = std::fs::create_dir_all(&dir);
    let path = format!("{0}/UGRD_height_above_ground_f000.bin", dir);

    // height_above_ground (code 103): canonical order is ASCENDING
    let seed: Vec<(f64, Vec<f32>)> = [
        (10.0, plane(1.0)),
        (2.0, plane(2.0)),
        (8.0, plane(3.0)),
    ].to_vec();
    let r = merge_write_field(path.to_string(), 103, NX, NY, true, false, seed);
    assert!(r.is_ok());

    // merge with NOTHING new: readback-only pass, planes must come back IDENTICAL
    let mg = merge_field(path.to_string(), 103, [].to_vec());
    assert!(mg.is_ok());
    match mg {
        Err(_) => { assert!(false); }
        Ok(pairs) => {
            assert!(pairs.len() == 3);
            // ASCENDING for a non-isobaric level type
            assert!(pairs[0].0 == 2.0 && pairs[1].0 == 8.0 && pairs[2].0 == 10.0);
            let mut all_same = true;
            for li in 0 .. 3 {
                let sc = scalar_plane(&path, li);
                if sc.len() != 12 {
                    all_same = false;
                } else {
                    for k in 0 .. 12 {
                        if pairs[li].1[k] != sc[k] {
                            all_same = false;
                        }
                    }
                }
            }
            assert!(all_same);
        }
    }

    // merge a NEW level into the existing file: order stays ASCENDING and every
    // pre-existing plane survives bit-for-bit.
    let r2 = merge_write_field(path.to_string(), 103, NX, NY, true, false,
                               [(4.0, plane(4.0))].to_vec());
    assert!(r2.is_ok());
    let mf = open_field(path.to_string());
    match mf {
        Err(_) => { assert!(false); }
        Ok(f) => {
            assert!(f.header.levels.len() == 4);
            assert!(f.header.levels[0] == 2.0);
            assert!(f.header.levels[1] == 4.0);
            assert!(f.header.levels[2] == 8.0);
            assert!(f.header.levels[3] == 10.0);
            let mut ok_planes = true;
            for li in 0 .. 4 {
                for k in 0 .. 12 {
                    // plane contents follow `plane_base(li)` (see below)
                    let expect = plane_base(li) + k as f32;
                    if f.value(li, k / 4, k % 4) != expect {
                        ok_planes = false;
                    }
                }
            }
            assert!(ok_planes);
        }
    }
}

/// Base value of the plane written for level index `li` in the fixture above.
fn plane_base(li: usize) -> f32 {
    // seed order after sorting: 2.0 -> plane(2.0), 4.0 -> plane(4.0),
    // 8.0 -> plane(3.0), 10.0 -> plane(1.0)
    if li == 0 {
        return 2.0;
    }
    if li == 1 {
        return 4.0;
    }
    if li == 2 {
        return 3.0;
    }
    return 1.0;
}
