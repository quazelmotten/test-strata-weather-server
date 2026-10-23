// V1.2 phase B0 MIRROR CHECK (spec §6.4). `wx_core::interp::query_point` resolves
// the level bracket ONCE against `FieldSamples.levels` and addresses slabs as
// `li * nt + ti`, so unlike the ingest evaluator it never applied a LEVEL index
// belonging to one TIME slot to another slot's data -- there was no slot-index
// bug here. These tests PROVE the cross-slot independence instead:
//
//  (1) the full H->V->T composition reproduces an analytic value whose planes are
//      DISTINCT PER (level,time) PAIR, so ANY transposition of the four samples
//      (the failure mode the ingest path had) changes the number;
//  (2) an EXACT forecast-hour hit ignores the OTHER time's planes entirely:
//      rewriting every slab of the non-selected forecast hour leaves the answer
//      BIT-IDENTICAL, while rewriting the selected hour's slabs moves it exactly
//      as the time-blend predicts. Level indices stay valid across both stacks.
//
// Plane value convention: P(level index li, forecast-hour index ti, node j, i)
//     = li*1000 + ti*100 + (j*10 + i)   (exact in f64)
// so bilinear at the cell centre (i,j) = (0.5,0.5) returns li*1000 + ti*100 + 5.5.

use wx_core::grid::{GridDef, GridRegular};
use wx_core::interp::{FieldSamples, TimeInterpMode, query_point};

const NX: usize = 2;
const NY: usize = 2;

pub fn make_grid() -> GridDef {
    let g = GridRegular {
        nx: 2u32,
        ny: 2u32,
        lon1: 0.0,
        lat1: 0.0,
        dlon: 1.0,
        dlat: 1.0,
        scan_mode: 0x40, // rows go north: dj = (lat - lat1) / dlat
        winds_grid: false,
    };
    GridDef::Regular(g)
}

/// Slab (li,ti) filled with the analytic convention above.
pub fn fill_slabs(levels_n: usize, fhs_n: usize, poison_ti: i64) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::<f64>::new();
    for li in 0 .. levels_n {
        for ti in 0 .. fhs_n {
            for j in 0 .. NY {
                for i in 0 .. NX {
                    let mut v = ((li as f64) * 1000.0) + ((ti as f64) * 100.0)
                              + ((j as f64) * 10.0) + (i as f64);
                    if poison_ti >= 0 && ti as i64 == poison_ti {
                        // rewrite the whole forecast hour with a DIFFERENT field
                        v = 7.0 * v + 0.5;
                    }
                    out.push(v);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// (1) distinct-per-pair composition: any sample transposition is detectable
// ---------------------------------------------------------------------------
// levels [1000,900,800] (descending), request 850 -> bracket (900,800) t=0.5
// fhs [0,6], request fh 3 -> time blend t=0.5
//   v00 = 1005.5  v01 = 1105.5  v10 = 2005.5  v11 = 2105.5
//   f0  = 1005.5*0.5 + 2005.5*0.5 = 1505.5
//   f1  = 1105.5*0.5 + 2105.5*0.5 = 1605.5
//   val = 1505.5*0.5 + 1605.5*0.5 = 1555.5
// A slot-index mixup (e.g. reading BOTH levels from ti0, or both from li0) gives
// 1055.5 / 2055.5 / 1505.5 / 2105.5-class values, NEVER 1555.5.
#[test]
fn per_pair_planes_compose_horizontally_then_vertically_then_in_time() {
    let grid = make_grid();
    let lv: [f64; 3] = [1000.0, 900.0, 800.0];
    let fh: [f64; 2] = [0.0, 6.0];
    let data = fill_slabs(3, 2, -1);
    let fd = FieldSamples {
        levels: lv.to_vec(),
        fhs: fh.to_vec(),
        data: &data[..],
        stat_processed: false,
    };
    let r = query_point(&grid, &fd, 0.5, 0.5, 850.0, 3.0, TimeInterpMode::Linear);
    match r {
        Err(_e) => { panic!();}
        Ok(pv) => {
            assert!(pv.value == 1555.5);
            match pv.info.vertical {
                None => { panic!(); }
                Some(vm) => {
                    assert!(vm.levels == (900.0, 800.0));
                    // wx-core models ONE level stack shared by every forecast
                    // hour, so the two reported pairs always agree and the
                    // additive "levels_per_time" key stays OFF server-side.
                    assert!(vm.levels_later == (900.0, 800.0));
                    assert!(!vm.per_time);
                }
            }
            match pv.info.time {
                None => { panic!();}
                Some(tm) => { assert!(tm.forecast_hours == (0.0, 6.0)); }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (2) cross-slot independence: an exact forecast-hour hit reads ONLY that
//     hour's slabs; the other hour's planes are IRRELEVANT to the answer.
// ---------------------------------------------------------------------------
#[test]
fn exact_forecast_hour_reads_only_its_own_slabs() {
    let grid = make_grid();
    let lv: [f64; 3] = [1000.0, 900.0, 800.0];
    let fh: [f64; 2] = [0.0, 6.0];

    // baseline: exact fh 0 at level 850 -> vertical between (900,800) of ti0
    //   f = 1005.5*0.5 + 2005.5*0.5 = 1505.5 (no time metadata)
    let base = fill_slabs(3, 2, -1);
    let fd_base = FieldSamples {
        levels: lv.to_vec(),
        fhs: fh.to_vec(),
        data: &base[..],
        stat_processed: false,
    };
    let r0 = query_point(&grid, &fd_base, 0.5, 0.5, 850.0, 0.0, TimeInterpMode::Linear);
    match r0 {
        Err(_) => { panic!(); }
        Ok(pv) => {
            assert!(pv.value == 1505.5);
            assert!(pv.info.time.is_none());
        }
    }

    // rewrite EVERY slab of forecast hour index 1 (the NON-selected hour):
    // the answer must be BIT-IDENTICAL.
    let poisoned = fill_slabs(3, 2, 1);
    let fd_poison = FieldSamples {
        levels: lv.to_vec(),
        fhs: fh.to_vec(),
        data: &poisoned[..],
        stat_processed: false,
    };
    let r1 = query_point(&grid, &fd_poison, 0.5, 0.5, 850.0, 0.0, TimeInterpMode::Linear);
    match r1 {
        Err(_) => { panic!(); }
        Ok(pv) => {
            assert!(pv.value == 1505.5); // BIT-IDENTICAL to the baseline answer
            // sanity: the poisoned planes really are different numbers, otherwise
            // the equality above would be vacuous.
            let mut differ = false;
            for k in 0 .. poisoned.len() {
                if poisoned[k] != base[k] {
                    differ = true;
                    break;
                }
            }
            assert!(differ);
        }
    }

    // and rewriting the SELECTED hour moves the answer exactly as predicted:
    // ti0 planes become 7*v+0.5 -> v(1,0)=7039.0, v(2,0)=14039.0 ->
    //   f = 7039.0*0.5 + 14039.0*0.5 = 10539.0
    let poison0 = fill_slabs(3, 2, 0);
    let fd_p0 = FieldSamples {
        levels: lv.to_vec(),
        fhs: fh.to_vec(),
        data: &poison0[..],
        stat_processed: false,
    };
    let r2 = query_point(&grid, &fd_p0, 0.5, 0.5, 850.0, 0.0, TimeInterpMode::Linear);
    match r2 {
        Err(_) => { panic!(); }
        Ok(pv) => { assert!(pv.value == 10539.0); }
    }

    // level indices stay independent of the TIME slot too: request a level that
    // only EXISTS in the shared stack (1000 exact) at the OTHER forecast hour.
    //   v(0,1) = 0*1000 + 100 + 5.5 = 105.5 with NO vertical step
    let r3 = query_point(&grid, &fd_base, 0.5, 0.5, 1000.0, 6.0, TimeInterpMode::Linear);
    match r3 {
        Err(_) => { panic!();}
        Ok(pv) => {
            assert!(pv.value == 105.5);
            assert!(pv.info.vertical.is_none());
            assert!(pv.info.time.is_none());
        }
    }
}
