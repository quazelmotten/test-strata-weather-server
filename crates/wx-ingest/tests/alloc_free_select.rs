// V1.1 phase 5 bit-exactness gate: `resolve_field_times` (the zero-alloc
// selection helper that reads forecast-hour keys straight out of the index
// slot list) must produce EXACTLY the same Result as the old path — materialize
// `Vec<f64>` of `fh_secs as f64` and call wx-core `resolve_times` — across
// every case class: exact hits, eps near-misses, fractional wants in seconds
// (post-phase-3 arithmetic), time_interp=none, the template-4.8 stat guard,
// single-slot fields, gaps, out-of-range and empty lists. Selection never
// dereferences the mmap handle, so ONE shared tiny field backs every slot.

use wx_ingest::index::{IdxFh, resolve_field_times};
use wx_core::interp::{resolve_times, TIME_MATCH_EPS, PointError, TimeInterpMode};
use wx_core::storage::{BinHeader, StatMeta, MmapField, open_field, write_bin_atomic};
use std::sync::Arc;
use std::fs::create_dir_all;
use std::fs::remove_file;

fn lcg(state: usize) -> usize {
    return state.wrapping_mul(6364136223846793005usize).wrapping_add(1442695072203450757usize);
}

#[allow(clippy::needless_return)]
pub fn make_stub(dir: String) -> Result<Arc<MmapField>, String> {
    let hdr = BinHeader {
        nx: 2u32,
        ny: 2u32,
        levels: [500.0].to_vec(),
        scan_ns: true,
        winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
    let plane: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
    let mut planes: Vec<&[f32]> = Vec::<&[f32]>::new();
    let _p = planes.push(&plane[..]);
    let path = format!("{0}/stub_f.bin", dir);
    let wr = write_bin_atomic(path.clone(), &hdr, &planes);
    match wr {
        Err(e) => { return Err(e.to_string()); }
        Ok(_) => {}
    }
    let of = open_field(path.clone());
    match of {
        Err(e) => { return Err(e.to_string()); }
        Ok(mf) => { return Ok(Arc::new(mf)); }
    }
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

#[test]
fn field_time_resolution_is_bit_identical_to_materialized_twin() {
    let dir = "/tmp/opencode/wxpoint-selstub-a".to_string();
    let _c = create_dir_all(&dir);
    let stub_r = make_stub(dir.to_string());
    match stub_r {
        Err(_e) => { assert!(false); return; }
        Ok(stub) => {
            let mut seed = 0xABCDEFusize;
            let mut checked = 0usize;
            for trial in 0 .. 3000 {
                // ascending integral fh_secs (whole AND half-hour steps mixed in)
                let n = 1usize + ((seed >> 7) % 9usize);
                seed = lcg(seed);
                let base = ((seed >> 3) % 24usize) as i64 * 3600;
                let mut fhs: Vec<IdxFh> = Vec::<IdxFh>::new();
                let mut f64s: Vec<f64> = Vec::<f64>::new();
                let mut k = 0usize;
                while k < n {
                    let step = if (trial % 5) == 0 { 1800i64 } else { 3600i64 };
                    let v = base + k as i64 * step;
                    let _p = fhs.push(mk_fh(stub.clone(), v));
                    let _q = f64s.push(v as f64);
                    k += 1;
                }
                let stat = (trial % 3) == 0;
                let modes: [TimeInterpMode; 2] = [TimeInterpMode::Linear, TimeInterpMode::None];
                let mut mi = 0usize;
                while mi < 2 {
                    let mode = modes[mi];
                    let mut wi = 0usize;
                    while wi < f64s.len() + 4usize {
                        let mut want = 1e9;
                        if wi < f64s.len() {
                            if (wi % 2) == 0 { want = f64s[wi]; } else { want = f64s[wi] + 1800.0; }
                        } else if wi == f64s.len() { want = f64s[0] - TIME_MATCH_EPS * 0.4; }
                        else if wi == f64s.len() + 1usize { want = f64s[f64s.len() - 1usize] + TIME_MATCH_EPS * 0.4; }
                        else if wi == f64s.len() + 2usize { want = -1e9; }
                        let old_r = resolve_times(&f64s, want, mode, stat);
                        let new_r = resolve_field_times(&fhs, want, mode, stat);
                        assert!(old_r == new_r);
                        match old_r {
                            Ok(p) => {
                                // reported bracket values identical doubles
                                assert!(f64s[p.0] == (fhs[p.0].fh_secs as f64));
                                assert!(f64s[p.1] == (fhs[p.1].fh_secs as f64));
                            }
                            Err(_) => {}
                        }
                        checked += 1;
                        wi += 1;
                    }
                    mi += 1;
                }
                let _t = trial;
            }
            assert!(checked > 30000);
            let _r = remove_file(format!("{0}/stub_f.bin", dir));
        }
    }
}

#[test]
fn empty_field_never_brackets() {
    let dir = "/tmp/opencode/wxpoint-selstub-b".to_string();
    let _c = create_dir_all(&dir);
    let stub_r = make_stub(dir.to_string());
    match stub_r {
        Err(_e) => { assert!(false); return; }
        Ok(stub) => {
            let empty: Vec<IdxFh> = Vec::<IdxFh>::new();
            let r = resolve_field_times(&empty, 0.0, TimeInterpMode::Linear, false);
            match r {
                Err(e) => {
                    match e {
                        PointError::NoTimeBracket => {}
                        _ => { assert!(false); }
                    }
                }
                Ok(_) => { assert!(false); }
            }
            let _u = stub.clone();
        }
    }
}
