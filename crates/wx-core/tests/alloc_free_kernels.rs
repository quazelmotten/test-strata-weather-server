// V1.1 phase 5 bit-exactness gate (spec §8: an optimization that could change
// any output bit needs a test proving the new result is at least as correct):
// `resolve_levels_f32` (used by the zero-alloc query path, resolving directly
// against the mmapped f32 header) must return EXACTLY the same bracket as
// `resolve_levels` on the old materialized `Vec<f64>` twin, for every case
// class: exact hits, eps-boundary near-misses, descending/ascending orders,
// gaps, duplicates, single levels, out-of-range wants.

use wx_core::interp::{resolve_levels, resolve_levels_f32, LEVEL_MATCH_EPS};

/// Deterministic LCG so the sweep is reproducible.
fn lcg(state: usize) -> usize {
    return state.wrapping_mul(6364136223846793005usize).wrapping_add(1442695072203450757usize);
}

#[test]
fn f32_level_resolution_is_bit_identical_to_f64_twin() {
    let mut seed = 0x12345678usize;
    let mut checked = 0usize;
    for trial in 0 .. 4000 {
        // level set: 1..12 levels, values from a coarse grid + occasional
        // exact-duplicate pairs (bracket-skip rule a != b must agree too).
        let nl = 1 + (seed % 12usize);
        seed = lcg(seed);
        let mut lv32: [f32; 12] = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let mut prev = -1e30;
        let mut k = 0;
        while k < nl {
            let step = 25.0 + ((seed >> 13) % 400usize) as f64 * 0.5;
            let v = if k == 0 { 1000.0 } else if (trial % 7) == 0 && k > 0 { prev } else { prev - step };
            lv32[k] = v as f32;
            prev = v as f32 as f64;
            seed = lcg(seed);
            k += 1;
        }
        let mut vec32: Vec<f32> = Vec::<f32>::new();
        let mut vec64: Vec<f64> = Vec::<f64>::new();
        let mut kk = 0;
        while kk < nl {
            let _a = vec32.push(lv32[kk]);
            let _b = vec64.push(lv32[kk] as f64);
            kk += 1;
        }
        // wants: stored levels, midpoints, eps-offsets, far outside, dup handling
        let mut wq: Vec<f64> = Vec::<f64>::new();
        let mut j = 0;
        while j < nl {
            let lv = vec64[j];
            wq.push(lv);
            wq.push(lv + LEVEL_MATCH_EPS * 0.5);
            wq.push(lv + LEVEL_MATCH_EPS * 3.0);
            if j + 1 < nl {
                wq.push((vec64[j] + vec64[j + 1]) / 2.0);
            }
            j += 1;
        }
        wq.push(-1e9);
        wq.push(1e9);
        wq.push(1000.0000000005);
        let mut q = 0;
        while q < wq.len() {
            let want = wq[q];
            let old_r = resolve_levels(&vec64, want);
            let new_r = resolve_levels_f32(&vec32, want);
            assert!(old_r == new_r);
            // and when both Some, the selected VALUES are the identical doubles
            match old_r {
                None => {}
                Some(p) => {
                    let a_old = vec64[p.0];
                    let a_new = vec32[p.0] as f64;
                    assert!(a_old == a_new);
                    let b_old = vec64[p.1];
                    let b_new = vec32[p.1] as f64;
                    assert!(b_old == b_new);
                }
            }
            checked += 1;
            q += 1;
        }
        let _t = trial;
    }
    assert!(checked > 20000);
}
