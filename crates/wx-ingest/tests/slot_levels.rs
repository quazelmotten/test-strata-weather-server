// V1.2 phase B0 CRITICAL REGRESSION GATE (spec §6.4, correctness first).
//
// `evaluate_point` must read EACH TIME SLOT with THAT SLOT'S OWN level indices
// (`sr.li0`/`sr.li1` and `sr.field.header.levels`) and interpolate vertically
// SEPARATELY AT each forecast hour BEFORE blending in time. Pre-fix, slot 1 was
// sampled with slot 0's indices and bracketed with slot 0's level VALUES, so two
// forecast hours carrying DIFFERENT level stacks (the normal state DURING PARTIAL
// INGEST: f006 written with [1000,900,800,700] while f012 already gained 950/850)
// produced a PLAUSIBLE BUT WRONG NUMBER with no error at all.
//
// Analytic field: V(level L, forecast hour F, node j, i) =
//     L*100 + F*10 + (j*10 + i)
// which is EXACTLY representable in f32/f64 for the ranges used here AND linear
// in BOTH L and (j,i), therefore:
//   * horizontal bilinear at (i,j) = (0.5,0.5) returns L*100 + F*10 + 5.5
//     (mean of nodes 0,1,10,11 -> +5.5 exactly);
//   * ANY CORRECT vertical interpolation reproduces 875*100 + F*10 + 5.5 no matter
//     WHICH bracket is used, so the test pins the analytic value;
//   * the BUGGY path reads the WRONG PLANE of slot B and blends it with slot A's
//     weights, which CANNOT reproduce that value (hand-computed per case below).

use wx_ingest::index::{IdxField, IdxFh, IdxFieldEntry, IdxModel, IdxRun, IndexSnapshot,
                       build_context, evaluate_point};
use wx_core::grid::{GridDef, GridRegular};
use wx_core::interp::{HorizMethod, LevelInterpMethod, PointError, TimeInterpMode};
use wx_core::storage::{BinHeader, StatMeta, bits_to_f32, MmapField, open_field, write_bin_atomic};
use wx_core::tables::{lookup_level_type};
use wx_ingest::discovery::{path_to_str};
use std::sync::Arc;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::remove_file;
use std::fs::remove_dir;

const NX: u32 = 4;
const NY: u32 = 3;
const EPOCH: i64 = 1760889600; // 2026-10-19T12:00:00Z (value irrelevant here)

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

/// Write one field .bin whose planes follow the analytic formula. `nan_level`
/// poisons ONE whole plane with NaN to exercise the masked-point rule.
#[allow(clippy::needless_return)]
fn write_field_file(dir: String, name: String, levels: &[f64], fh_hours: f64,
                    nan_level: f64) {
    let mut planes: Vec<Vec<f32>> = Vec::<Vec<f32>>::new();
    for lv in levels {
        let mut plane: Vec<f32> = Vec::<f32>::new();
        for j in 0 .. (NY as usize) {
            for i in 0 .. (NX as usize) {
                if *lv == nan_level {
                    let _q = plane.push(bits_to_f32(0x7fc00000));
                    continue;
                }
                let v = lv * 100.0 + fh_hours * 10.0 + ((j as f64) * 10.0 + (i as f64));
                let _p = plane.push(v as f32);
            }
        }
        let _m = planes.push(plane);
    }
    let mut refs: Vec<&[f32]> = Vec::<&[f32]>::new();
    for pl in &planes {
        let _r = refs.push(&pl[..]);
    }
    let mut lv32: Vec<f32> = Vec::<f32>::new();
    for lv in levels {
        let _z = lv32.push(*lv as f32);
    }
    let hdr = BinHeader {
        nx: NX,
        ny: NY,
        levels: lv32,
        scan_ns: false, // scan_mode 0x40: rows advance northward
        winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
    let path = format!("{0}/{1}", dir, name);
    let wr = write_bin_atomic(path, &hdr, &refs);
    match wr {
        Err(_e) => { assert!(false); return; }
        Ok(_) => {}
    }
}

pub fn iso_code() -> u8 {
    match lookup_level_type("isobaric") {
        None => { assert!(false); return 0; }
        Some(c) => { return c; }
    }
}

pub fn make_grid() -> GridDef {
    let g = GridRegular {
        nx: NX,
        ny: NY,
        lon1: 0.0,
        lat1: 0.0,
        dlon: 1.0,
        dlat: 1.0,
        scan_mode: 0x40, // rows go north: dj = (lat - lat1) / dlat
        winds_grid: false,
    };
    return GridDef::Regular(g);
}

pub fn mk_fh(mf: Arc<MmapField>, fh_secs: i64) -> IdxFh {
    return IdxFh {
        fh_secs: fh_secs,
        field: mf.clone(),
        stat_has_range: false,
        stat_begin_secs: 0,
        stat_end_secs: 0,
        stat_step_secs: 0,
    };
}

pub fn mk_entry(fa: Arc<MmapField>, fb: Arc<MmapField>) -> IdxFieldEntry {
    return IdxFieldEntry {
        param_key: "TMP".to_string(),
        level_code: iso_code(),
        fd: IdxField {
            units: "K".to_string(),
            stat_processed: false,
            stat_kind: "instantaneous".to_string(),
            fhs: [mk_fh(fa, 0), mk_fh(fb, 21600)].to_vec(),
        },
    };
}

pub fn open_two(dir: String, n1: String, n2: String) -> Option<(Arc<MmapField>, Arc<MmapField>)> {
    let a = open_field(format!("{0}/{1}", dir, n1));
    let b = open_field(format!("{0}/{1}", dir, n2));
    match a {
        Err(_) => { return None; }
        Ok(ma) => {
            match b {
                Err(_) => { return None; }
                Ok(mb) => { return Some((Arc::new(ma), Arc::new(mb))); }
            }
        }
    }
}

pub fn make_snapshot(entry: IdxFieldEntry) -> IndexSnapshot {
    let run = IdxRun {
        epoch_secs: EPOCH,
        dir_name: "2026101912".to_string(),
        grid: Arc::new(make_grid()),
        fields: [entry].to_vec(),
    };
    return IndexSnapshot {
        models: [IdxModel { name: "gfs".to_string(), runs: [run].to_vec() }].to_vec(),
    };
}

// ---------------------------------------------------------------------------
// (a) DIFFERENT level stacks across the time bracket -- THE BUG CASE
// ---------------------------------------------------------------------------
// slot A (fh 0):   [1000, 900, 800, 700] -> request 875 brackets (900,800), t=0.25
// slot B (fh 6):   [ 950, 850, 800, 700] -> request 875 brackets (950,850), t=0.75
// DELIBERATE MISALIGNMENT: slot A's indices (1,2) point at the 950 and 850 PLANES
// of slot B, which is NOT what slot A's VALUES (900,800) describe.
//
// CORRECT (vertical per slot, THEN time):
//   f_A = 90005.5*0.75 + 80005.5*0.25 = 67504.125 + 20001.375 = 87505.5
//   f_B = 95065.5*0.25 + 85065.5*0.75 = 23766.375 + 63799.125 = 87565.5
//   val = mean(f_A, f_B)              = 87535.5
// PRE-FIX (slot B read through slot A's indices AND bracket):
//   v_wrong = 85065.5*0.75 + 80065.5*0.25 = 63799.125 + 20016.375 = 83815.5
//   -> val_wrong = mean(87505.5, 83815.5) = 85660.5   (asserted away)
#[test]
fn differing_level_stacks_across_time_bracket_interpolate_per_slot() {
    let dir = "/tmp/opencode/wxpoint-slotlevels-diff";
    rm_tree(&dir);
    let _c = create_dir_all(&dir);
    let a_levels: [f64; 4] = [1000.0, 900.0, 800.0, 700.0];
    let b_levels: [f64; 4] = [950.0, 850.0, 800.0, 700.0];
    write_field_file(dir.to_string(), "f000.bin".to_string(), &a_levels[..], 0.0, -1.0);
    write_field_file(dir.to_string(), "f006.bin".to_string(), &b_levels[..], 6.0, -1.0);
    let pair = open_two(dir.to_string(), "f000.bin".to_string(), "f006.bin".to_string());
    match pair {
        None => { assert!(false); return; }
        Some(pr) => {
            let snap = make_snapshot(mk_entry(pr.0, pr.1));
            let c_r = build_context(&snap, "gfs", None, "TMP", iso_code(), 875.0, 10800.0, TimeInterpMode::Linear);
            match c_r {
                Err(_e) => { assert!(false); return; }
                Ok(c) => {
                    let v = evaluate_point(&c, 0.5, 0.5);
                    match v {
                        Err(_) => { assert!(false); return; }
                        Ok(pv) => {
                            assert!(pv.value == 87535.5);
                            assert!(pv.value != 85660.5); // the pre-fix answer
                            assert!(pv.info.horizontal == HorizMethod::Bilinear);
                            match pv.info.vertical {
                                None => { assert!(false); return; }
                                Some(vm) => {
                                    assert!(vm.method == LevelInterpMethod::Linear);
                                    assert!(vm.levels == (900.0, 800.0)); // earlier slot
                                    assert!(vm.levels_later == (950.0, 850.0)); // later slot
                                    assert!(vm.per_time);
                                }
                            }
                            match pv.info.time {
                                None => { assert!(false); return; }
                                Some(tm) => {
                                    assert!(tm.method == TimeInterpMode::Linear);
                                    assert!(tm.forecast_hours == (0.0, 6.0));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    rm_tree(&dir);
}

// ---------------------------------------------------------------------------
// (b) IDENTICAL stacks across the bracket keep V1.1 behavior: single reported
//     pair, `per_time` false (so the JSON stays byte-identical to V1.1).
// f_A = 87505.5 ; f_B = 90065.5*0.75 + 80065.5*0.25 = 67549.125+20016.375 =
//   87565.5 ; val = 87535.5 (same analytic value: linear in level).
#[test]
fn identical_stacks_keep_single_vertical_bracket_metadata() {
    let dir = "/tmp/opencode/wxpoint-slotlevels-same";
    rm_tree(&dir);
    let _c = create_dir_all(&dir);
    let lv: [f64; 4] = [1000.0, 900.0, 800.0, 700.0];
    write_field_file(dir.to_string(), "f000.bin".to_string(), &lv[..], 0.0, -1.0);
    write_field_file(dir.to_string(), "f006.bin".to_string(), &lv[..], 6.0, -1.0);
    let pair = open_two(dir.to_string(), "f000.bin".to_string(), "f006.bin".to_string());
    match pair {
        None => { assert!(false); return; }
        Some(pr) => {
            let snap = make_snapshot(mk_entry(pr.0, pr.1));
            let c_r = build_context(&snap, "gfs", None, "TMP", iso_code(), 875.0, 10800.0, TimeInterpMode::Linear);
            match c_r {
                Err(_) => { assert!(false); return; }
                Ok(c) => {
                    let v = evaluate_point(&c, 0.5, 0.5);
                    match v {
                        Err(_) => { assert!(false); return; }
                        Ok(pv) => {
                            assert!(pv.value == 87535.5);
                            match pv.info.vertical {
                                None => { assert!(false); return; }
                                Some(vm) => {
                                    assert!(vm.levels == (900.0, 800.0));
                                    assert!(vm.levels_later == (900.0, 800.0));
                                    assert!(!vm.per_time);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    rm_tree(&dir);
}

// ---------------------------------------------------------------------------
// (c) exact-present in ONE slot, bracketed in the OTHER (degenerate li1==li0 on
//     one side only -- no divide-by-zero, per-slot vertical step still correct).
// request 850 hPa: slot A brackets (900,800) t=0.5 ->
//   f_A = 90005.5*0.5 + 80005.5*0.5 = 85005.5
//   slot B has 850 EXACTLY -> f_B = H(850,6) = 85065.5
//   val = mean = 85035.5 ; metadata levels (900,800), levels_later (850,850),
//   per_time TRUE (the pairs genuinely differ).
#[test]
fn exact_level_in_one_slot_bracket_in_the_other() {
    let dir = "/tmp/opencode/wxpoint-slotlevels-mix";
    rm_tree(&dir);
    let _c = create_dir_all(&dir);
    let a_levels: [f64; 4] = [1000.0, 900.0, 800.0, 700.0];
    let b_levels: [f64; 4] = [950.0, 850.0, 800.0, 700.0];
    write_field_file(dir.to_string(), "f000.bin".to_string(), &a_levels[..], 0.0, -1.0);
    write_field_file(dir.to_string(), "f006.bin".to_string(), &b_levels[..], 6.0, -1.0);
    let pair = open_two(dir.to_string(), "f000.bin".to_string(), "f006.bin".to_string());
    match pair {
        None => { assert!(false); return; }
        Some(pr) => {
            let snap = make_snapshot(mk_entry(pr.0, pr.1));
            let c_r = build_context(&snap, "gfs", None, "TMP", iso_code(), 850.0, 10800.0, TimeInterpMode::Linear);
            match c_r {
                Err(_) => { assert!(false); return; }
                Ok(c) => {
                    let v = evaluate_point(&c, 0.5, 0.5);
                    match v {
                        Err(_) => { assert!(false); return; }
                        Ok(pv) => {
                            assert!(pv.value == 85035.5);
                            match pv.info.vertical {
                                None => { assert!(false); return; }
                                Some(vm) => {
                                    assert!(vm.levels == (900.0, 800.0));
                                    assert!(vm.levels_later == (850.0, 850.0));
                                    assert!(vm.per_time);
                                }
                            }
                        }
                    }
                }
            }
            // exact TIME hit with differing stacks: ONE slot (cloned), so NO time
            // metadata and the LATER stack's OWN pair is reported (request 875 at fh 6).
            let c2_r = build_context(&snap, "gfs", None, "TMP", iso_code(), 875.0, 21600.0, TimeInterpMode::Linear);
            match c2_r {
                Err(_) => { assert!(false); return; }
                Ok(c2) => {
                    let v2 = evaluate_point(&c2, 0.5, 0.5);
                    match v2 {
                        Err(_) => { assert!(false); return; }
                        Ok(pv2) => {
                            assert!(pv2.value == 87565.5);
                            assert!(pv2.info.time == None);
                            match pv2.info.vertical {
                                None => { assert!(false); return; }
                                Some(vm2) => {
                                    assert!(vm2.levels == (950.0, 850.0));
                                    assert!(!vm2.per_time);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    rm_tree(&dir);
}

// ---------------------------------------------------------------------------
// (d) NaN propagation UNCHANGED: a masked plane in EITHER slot yields MissingData
//     (server renders null + reason), never a blended number.
#[test]
fn masked_plane_in_either_slot_still_returns_missing_data() {
    let dir = "/tmp/opencode/wxpoint-slotlevels-nan";
    let a_levels: [f64; 4] = [1000.0, 900.0, 800.0, 700.0];
    let b_levels: [f64; 4] = [950.0, 850.0, 800.0, 700.0];

    rm_tree(&dir);
    let _c = create_dir_all(&dir);
    write_field_file(dir.to_string(), "f000.bin".to_string(), &a_levels[..], 0.0, 900.0);
    write_field_file(dir.to_string(), "f006.bin".to_string(), &b_levels[..], 6.0, -1.0);
    let pair = open_two(dir.to_string(), "f000.bin".to_string(), "f006.bin".to_string());
    match pair {
        None => { assert!(false); return; }
        Some(pr) => {
            let snap = make_snapshot(mk_entry(pr.0, pr.1));
            let c_r = build_context(&snap, "gfs", None, "TMP", iso_code(), 875.0, 10800.0, TimeInterpMode::Linear);
            match c_r {
                Err(_) => { assert!(false); return; }
                Ok(c) => {
                    let v = evaluate_point(&c, 0.5, 0.5);
                    match v {
                        Ok(_) => { assert!(false); return; }
                        Err(e) => {
                            match e {
                                PointError::MissingData => {}
                                _ => { assert!(false); return; }
                            }
                        }
                    }
                }
            }
        }
    }

    // same, poisoned in the LATER slot instead
    rm_tree(&dir);
    let _c2 = create_dir_all(&dir);
    write_field_file(dir.to_string(), "f000.bin".to_string(), &a_levels[..], 0.0, -1.0);
    write_field_file(dir.to_string(), "f006.bin".to_string(), &b_levels[..], 6.0, 850.0);
    let pair2 = open_two(dir.to_string(), "f000.bin".to_string(), "f006.bin".to_string());
    match pair2 {
        None => { assert!(false); return; }
        Some(pr2) => {
            let snap2 = make_snapshot(mk_entry(pr2.0, pr2.1));
            let c3_r = build_context(&snap2, "gfs", None, "TMP", iso_code(), 875.0, 10800.0, TimeInterpMode::Linear);
            match c3_r {
                Err(_) => { assert!(false); return; }
                Ok(c3) => {
                    let v3 = evaluate_point(&c3, 0.5, 0.5);
                    match v3 {
                        Ok(_) => { assert!(false); return; }
                        Err(e3) => {
                            match e3 {
                                PointError::MissingData => {}
                                _ => { assert!(false); return; }
                            }
                        }
                    }
                }
            }
        }
    }
    rm_tree(&dir);
}
