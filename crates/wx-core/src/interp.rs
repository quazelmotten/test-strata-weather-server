//! Interpolation kernels (spec §6): horizontal bilinear, vertical between
//! levels, time between forecast hours. All arithmetic f64.
//!
//! Order per spec §6.4: horizontal at each of (up to) 2 levels x 2 times,
//! then vertical, then time.

/// Exact-node tolerance in grid units (spec §6.1).
const EXACT_NODE_EPS: f64 = 1e-9;

/// Canonical NaN for masked/missing data.
pub fn nan_f64() -> f64 {
    return (-1.0_f64).sqrt();
}

/// Bilinear interpolation over the 4 surrounding nodes of a row-major
/// `[j][i]` field (nx*ny values). Rules:
///  * exact node within 1e-9 grid units -> that node's value exactly;
///  * any NaN neighbor -> NaN (caller maps to null + reason missing_data);
///  * (di, dj) assumed already domain-checked by the projection.
pub fn bilinear(field: &[f64], nx: usize, ny: usize, di: f64, dj: f64) -> f64 {
    let i0 = floor_i(di);
    let j0 = floor_i(dj);
    let fi = di - (i0 as f64);
    let fj = dj - (j0 as f64);

    let i0c = clamp_us(i0, nx);
    let j0c = clamp_us(j0, ny);
    let i1c = clamp_us(i0 + 1, nx);
    let j1c = clamp_us(j0 + 1, ny);

    let v00 = field[j0c * nx + i0c];
    let v10 = field[j0c * nx + i1c];
    let v01 = field[j1c * nx + i0c];
    let v11 = field[j1c * nx + i1c];

    // exact-node rule: no floating-point blending when on a node
    if fi.abs() < EXACT_NODE_EPS && fj.abs() < EXACT_NODE_EPS {
        return v00;
    }
    if (1.0 - fi).abs() < EXACT_NODE_EPS && fj.abs() < EXACT_NODE_EPS {
        return v10;
    }
    if fi.abs() < EXACT_NODE_EPS && (1.0 - fj).abs() < EXACT_NODE_EPS {
        return v01;
    }
    if (1.0 - fi).abs() < EXACT_NODE_EPS && (1.0 - fj).abs() < EXACT_NODE_EPS {
        return v11;
    }

    if v00.is_nan() || v10.is_nan() || v01.is_nan() || v11.is_nan() {
        return nan_f64();
    }

    let w00 = (1.0 - fi) * (1.0 - fj);
    let w10 = fi * (1.0 - fj);
    let w01 = (1.0 - fi) * fj;
    let w11 = fi * fj;
    return w00 * v00 + w10 * v10 + w01 * v01 + w11 * v11;
}

/// Linear interpolation between two bracketing levels in `coord` (e.g. hPa).
/// `levels` must be sorted ascending; caller guarantees the request is inside
/// range (no extrapolation, spec §6.2).
pub fn vertical(levels: &[f64], values: &[f64], want: f64) -> f64 {
    for k in 0..levels.len() {
        if (levels[k] - want).abs() <= 1e-9 {
            return values[k];
        }
    }
    for k in 1..levels.len() {
        if levels[k - 1] <= want && want <= levels[k] {
            let t = (want - levels[k - 1]) / (levels[k] - levels[k - 1]);
            return values[k - 1] * (1.0 - t) + values[k] * t;
        }
    }
    unreachable!("request level must be bracketed by caller");
}

/// Linear interpolation between two bracketing forecast hours of the SAME run
/// (spec §6.3). Statistically-processed fields never take this path (caller
/// enforces exact match).
pub fn time_lerp(v0: f64, fh0: f64, v1: f64, fh1: f64, want_fh: f64) -> f64 {
    if want_fh == fh0 {
        return v0;
    }
    if want_fh == fh1 {
        return v1;
    }
    let t = (want_fh - fh0) / (fh1 - fh0);
    return v0 * (1.0 - t) + v1 * t;
}

fn floor_i(x: f64) -> i64 {
    return x.floor() as i64;
}

fn clamp_us(x: i64, n: usize) -> usize {
    if x < 0 {
        return 0;
    }
    if (x as u64) >= (n as u64) {
        return n - 1;
    }
    return x as usize;
}
