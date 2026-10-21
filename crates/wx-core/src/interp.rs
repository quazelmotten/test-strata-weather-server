//! Interpolation kernels (spec §6): horizontal bilinear, vertical between
//! levels, time between forecast hours. All arithmetic f64 (stored values
//! are f32, widened). Return type is always f64.
//!
//! Order per spec §6.4: horizontal interpolation happens at each of the
//! (up to) 2 levels x 2 times, then vertical, then time.
//!
//! Extension point (spec §6.2): a `log-pressure` vertical method could be
//! added to [`LevelInterpMethod`] later; only Linear is implemented today
//! and it is the default everywhere.

use crate::grid::GridDef;

/// Exact-node tolerance in grid units (spec §6.1).
pub const EXACT_NODE_EPS: f64 = 1e-9;

/// Tolerance for "the requested level equals a stored level" (level units).
pub const LEVEL_MATCH_EPS: f64 = 1e-9;

/// Tolerance for "the requested forecast hour equals a stored one" (hours).
pub const TIME_MATCH_EPS: f64 = 1e-9;

// ---------------------------------------------------------------------------
// result metadata + errors (shaped exactly for the /v1/point response, spec §7)
// ---------------------------------------------------------------------------

/// Horizontal method actually used: `Exact` when the query landed within
/// [`EXACT_NODE_EPS`] grid units of a node (value returned unblended).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HorizMethod {
    Exact,
    Bilinear,
}

/// Vertical interpolation method. Only Linear exists today (spec §6.2);
/// adding LogPressure later is a one-line enum extension here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LevelInterpMethod {
    Linear,
}

/// Time interpolation mode requested by the user (spec §6.3 / API param
/// `time_interp`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TimeInterpMode {
    Linear,
    None,
}

/// Bracketing levels reported in the response (`"vertical": {"method":
/// "linear", "levels": [850, 800]}`); `None` when the exact stored level was
/// used. Levels are reported in stored order.
///
/// ## Per-time brackets (V1.2 phase B0 correctness fix)
///
/// Spec §6.4 interpolates vertically SEPARATELY AT EACH forecast hour, so each
/// time slot brackets the requested level inside ITS OWN stored level stack.
/// Two forecast hours of one run can legitimately carry DIFFERENT stacks
/// (transient during partial ingest: f006 written with `[1000,850,700]`, f012
/// later gains `925` ⇒ `[1000,925,850,700]`), so there can be TWO different
/// brackets. `levels` stays the EARLIER slot's pair (spec §7 shape, back-
/// compatible); `levels_later` carries the LATER slot's pair; `per_time` is
/// true ONLY when the two pairs differ, and the server then ALSO emits
/// `"levels_per_time": [[earlier...],[later...]]` so the answer is never
/// misleading. When the brackets agree `per_time` is false and the extra key
/// is omitted (byte-identical output to V1.1 for every uniform-stack query).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VertMeta {
    pub method: LevelInterpMethod,
    pub levels: (f64, f64), // earlier forecast hour's bracket (spec §7)
    pub levels_later: (f64, f64), // later forecast hour's bracket
    pub per_time: bool, // true when the two brackets differ
}

/// Bracketing forecast hours reported in the response
/// (`"time": {"method": "linear", "forecast_hours": [6, 9]}`); `None` when
/// the exact forecast hour was used.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeMeta {
    pub method: TimeInterpMode,
    pub forecast_hours: (f64, f64),
}

/// What the interpolation did, exactly shaped for the API response.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InterpInfo {
    pub horizontal: HorizMethod,
    pub vertical: Option<VertMeta>,
    pub time: Option<TimeMeta>,
}

/// Typed failures (spec §6 + §7 status mapping in the server layer).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointError {
    OutOfDomain, // lat/lon outside the grid rectangle (server: 422)
    MissingData, // a stencil neighbor is NaN/masked (server: value null + reason)
    LevelOutOfRange, // requested level outside stored range (server: 422)
    NoLevelBracket, // cannot bracket the level among stored levels (server: 422)
    NoTimeBracket, // valid time outside stored forecast hours (server: 422)
    TimeExactOnly, // time_interp=none and no exact fh match (server: 422)
    StatTimeGuard, // statistically processed field requires exact fh (server: 422)
}

/// A successful point value plus its interpolation metadata.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointValue {
    pub value: f64,
    pub info: InterpInfo,
}

pub type QueryResult = Result<PointValue, PointError>;

/// Canonical NaN for masked/missing data.
pub fn nan_f64() -> f64 {
    let neg: f64 = -1.0;
    neg.sqrt()
}

// ---------------------------------------------------------------------------
// horizontal kernels
// ---------------------------------------------------------------------------

/// Resolve fractional storage coords (di, dj) into the four neighbor cell
/// indices plus the fractions (fi, fj) in [0,1]. `lon_wrap` must be true for
/// global grids so column nx wraps to 0 (dateline seam). The caller must
/// have domain-checked (di, dj) via GridDef::project already.
pub fn horiz_nodes(nx: usize, ny: usize, di: f64, dj: f64, lon_wrap: bool)
    -> (usize, usize, usize, usize, f64, f64)
{
    let i0f = di.floor();
    let j0f = dj.floor();
    let fi = di - i0f;
    let fj = dj - j0f;
    let mut i0 = i0f as isize;
    let mut i1 = i0 + 1;
    let mut j0 = j0f as isize;
    let mut j1 = j0 + 1;
    if lon_wrap {
        let n = nx as isize;
        i0 = ((i0 % n) + n) % n;
        i1 = ((i1 % n) + n) % n;
    } else {
        let nxm = (nx - 1) as isize;
        if i0 > nxm { i0 = nxm; }
        if i1 > nxm { i1 = nxm; }
        if i0 < 0 { i0 = 0; }
    }
    // rows never wrap for either grid kind we support (poles are edges);
    // clamp applies regardless of lon_wrap (a global grid wraps longitude
    // only).
    let nym = (ny - 1) as isize;
    if j0 > nym { j0 = nym; }
    if j1 > nym { j1 = nym; }
    if j0 < 0 { j0 = 0; }
    (i0 as usize, j0 as usize, i1 as usize, j1 as usize, fi, fj)
}

/// Core bilinear on the four neighbor values with fractions fi, fj in
/// [0,1]. Rules (spec §6.1): exact node within EXACT_NODE_EPS returns that
/// node's value with no floating-point blending; any NaN neighbor returns
/// NaN (caller maps to MissingData — weights are never renormalized).
pub fn bilinear4(v00: f64, v10: f64, v01: f64, v11: f64, fi: f64, fj: f64) -> f64 {
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
    w00 * v00 + w10 * v10 + w01 * v01 + w11 * v11
}

/// Bilinear over a row-major `[j][i]` slab of f32 (storage order), widened
/// to f64. Convenience wrapper around [`horiz_nodes`] + [`bilinear4`] used
/// by tests/benchmarks and by the server when it wants one call per slab.
pub fn bilinear_slab_f32(field: &[f32], nx: usize, ny: usize, di: f64, dj: f64,
                         lon_wrap: bool) -> f64 {
    let (i0, j0, i1, j1, fi, fj) = horiz_nodes(nx, ny, di, dj, lon_wrap);
    let v00 = field[j0 * nx + i0] as f64;
    let v10 = field[j0 * nx + i1] as f64;
    let v01 = field[j1 * nx + i0] as f64;
    let v11 = field[j1 * nx + i1] as f64;
    bilinear4(v00, v10, v01, v11, fi, fj)
}

/// Bilinear over a row-major `[j][i]` slab already widened to f64.
pub fn bilinear_slab_f64(field: &[f64], nx: usize, ny: usize, di: f64, dj: f64,
                         lon_wrap: bool) -> f64 {
    let (i0, j0, i1, j1, fi, fj) = horiz_nodes(nx, ny, di, dj, lon_wrap);
    bilinear4(field[j0 * nx + i0], field[j0 * nx + i1],
              field[j1 * nx + i0], field[j1 * nx + i1], fi, fj)
}

// ---------------------------------------------------------------------------
// vertical kernels (spec §6.2)
// ---------------------------------------------------------------------------

/// Find the stored level index pair for `want`: `(k, k)` on an exact match
/// (within LEVEL_MATCH_EPS), otherwise the bracketing consecutive pair in
/// stored order (levels may be stored descending, e.g. isobaric hPa 1000..100
/// — bracketing works either way). Returns None when `want` is outside the
/// stored range or unbracketable (never extrapolate). Gaps (e.g. only 850
/// and 500 stored) interpolate across the gap; the caller reports the
/// bracket.
pub fn resolve_levels(levels: &[f64], want: f64) -> Option<(usize, usize)> {
    if levels.is_empty() {
        return None;
    }
    for k in 0..levels.len() {
        if (levels[k] - want).abs() <= LEVEL_MATCH_EPS {
            return Some((k, k));
        }
    }
    if levels.len() == 1 {
        return None; // single stored level: must match exactly
    }
    for k in 1..levels.len() {
        let a = levels[k - 1];
        let b = levels[k];
        if a != b && (a - want) * (b - want) <= 0.0 {
            return Some((k - 1, k));
        }
    }
    None
}

/// Slice-typed twin of [`resolve_levels`] for levels stored as f32 in a .bin
/// header (V1.1 phase 5: the query path resolves directly against the mmapped
/// header without materializing an f64 Vec, so the hot path allocates nothing;
/// each element is widened exactly once, which makes the result bit-identical
/// to `resolve_levels(vec_of_f64)` — proven by tests/alloc_free_kernels.rs).
pub fn resolve_levels_f32(levels: &[f32], want: f64) -> Option<(usize, usize)> {
    if levels.is_empty() {
        return None;
    }
    for k in 0..levels.len() {
        let lv = levels[k] as f64;
        if (lv - want).abs() <= LEVEL_MATCH_EPS {
            return Some((k, k));
        }
    }
    if levels.len() == 1 {
        return None; // single stored level: must match exactly
    }
    for k in 1..levels.len() {
        let a = levels[k - 1] as f64;
        let b = levels[k] as f64;
        if a != b && (a - want) * (b - want) <= 0.0 {
            return Some((k - 1, k));
        }
    }
    None
}

/// Linear combination in the level coordinate (hPa for isobaric, meters for
/// height_above_ground, ...; spec §6.2 default). Requires l0 != l1.
pub fn vertical_at(v0: f64, l0: f64, v1: f64, l1: f64, want: f64) -> f64 {
    let t = (want - l0) / (l1 - l0);
    v0 * (1.0 - t) + v1 * t
}

// ---------------------------------------------------------------------------
// time kernels (spec §6.3)
// ---------------------------------------------------------------------------

/// Find the stored forecast-hour index pair for `want_fh` under `mode`.
/// Exact match => `(k, k)`. Otherwise:
///   * `TimeInterpMode::None`          => Err(TimeExactOnly)
///   * stat-processed field            => Err(StatTimeGuard)
///     (linearly interpolating accumulations/averages/max/min from template
///     4.8 produces wrong answers; ingest sets `stat_processed`)
///   * no bracket inside stored range  => Err(NoTimeBracket)
///   * else                            => Ok((k-1, k)) bracketing pair
pub fn resolve_times(fhs: &[f64], want_fh: f64, mode: TimeInterpMode,
                     stat_processed: bool) -> Result<(usize, usize), PointError> {
    if fhs.is_empty() {
        return Err(PointError::NoTimeBracket);
    }
    for k in 0..fhs.len() {
        if (fhs[k] - want_fh).abs() <= TIME_MATCH_EPS {
            return Ok((k, k));
        }
    }
    if mode == TimeInterpMode::None {
        return Err(PointError::TimeExactOnly);
    }
    if stat_processed {
        return Err(PointError::StatTimeGuard);
    }
    if fhs.len() == 1 {
        return Err(PointError::NoTimeBracket);
    }
    for k in 1..fhs.len() {
        let a = fhs[k - 1];
        let b = fhs[k];
        if a != b && (a - want_fh) * (b - want_fh) <= 0.0 {
            return Ok((k - 1, k));
        }
    }
    Err(PointError::NoTimeBracket)
}

/// Linear interpolation between two forecast hours of the SAME run
/// (spec §6.3). The caller guarantees fh0 < fh1 and want within range.
pub fn time_lerp(v0: f64, fh0: f64, v1: f64, fh1: f64, want_fh: f64) -> f64 {
    if want_fh == fh0 {
        return v0;
    }
    if want_fh == fh1 {
        return v1;
    }
    let t = (want_fh - fh0) / (fh1 - fh0);
    v0 * (1.0 - t) + v1 * t
}

// ---------------------------------------------------------------------------
// full point query (spec §6.4 order: horizontal x (<=2 levels x <=2 times),
// then vertical, then time)
// ---------------------------------------------------------------------------

/// One parameter's sampled field data on one grid: up to L levels x T
/// forecast-hour slabs. Slab `li * nt + ti` is row-major `[j][i]` in
/// storage order with nx*ny values (already widened to f64 by the caller —
/// the server keeps mmapped f32 and may use the low-level kernels directly
/// instead of this entry point; see its doc).
///
/// MIRROR CONTRACT (V1.2 phase B0): this kernel resolves the level bracket ONCE
/// against `levels` and applies it to EVERY forecast hour, i.e. it models ONE
/// level stack SHARED by all times. That is NOT the slot-index bug the ingest
/// evaluator had (slabs are addressed `li * nt + ti`, so a level index can never
/// be applied to the wrong TIME), but it CANNOT express per-hour stacks. The
/// PRODUCTION path (`wx_ingest::index::evaluate_point`) resolves the bracket
/// INSIDE each time slot's own header and interpolates vertically PER SLOT before
/// blending in time, exactly per spec §6.4. A caller holding per-hour stacks
/// must feed the union stack with duplicated slabs (equal values where a level
/// is absent) or evaluate hour-by-hour here and blend itself. Cross-slot slab
/// independence is pinned by `tests/slot_independence.rs`.
#[derive(Clone, Debug)]
pub struct FieldSamples<'d> {
    pub levels: Vec<f64>, // numeric level coords (see tables.rs)
    pub fhs: Vec<f64>, // forecast hours present, any order
    pub data: &'d [f64], // slabs: data[(li * nt + ti) * nx * ny ...]
    pub stat_processed: bool, // template 4.8 flag set by ingest
}

/// Entry point: value at (lat, lon, level, fh) for one parameter.
/// `grid` supplies projection + nx/ny/lon_wrap; `fd` the slabs.
pub fn query_point(grid: &GridDef, fd: &FieldSamples<'_>, lat: f64, lon: f64,
                   level: f64, fh: f64, mode: TimeInterpMode) -> QueryResult {
    let (di, dj) = match grid.project(lat, lon) {
        None => return Err(PointError::OutOfDomain),
        Some((di, dj)) => (di, dj),
    };
    let nx = grid.nx() as usize;
    let ny = grid.ny() as usize;
    let wrap = grid.lon_wrap();

    let (li0, li1) = match resolve_levels(&fd.levels, level) {
        None => return Err(PointError::LevelOutOfRange),
        Some(p) => p,
    };
    let (ti0, ti1) = match resolve_times(&fd.fhs, fh, mode, fd.stat_processed) {
        Err(e) => return Err(e),
        Ok(p) => p,
    };

    let nt = fd.fhs.len();
    let plane = nx * ny;
    let hval = |li: usize, ti: usize| -> f64 {
        let off = (li * nt + ti) * plane;
        bilinear_slab_f64(&fd.data[off .. off + plane], nx, ny, di, dj, wrap)
    };

    // horizontal at each (level, time)
    let v00 = hval(li0, ti0);
    let v01 = hval(li0, ti1);
    let v10 = hval(li1, ti0);
    let v11 = hval(li1, ti1);
    if v00.is_nan() || v01.is_nan() || v10.is_nan() || v11.is_nan() {
        return Err(PointError::MissingData);
    }

    // exact-node detection for metadata: point within eps of a node
    let fi = di - di.floor();
    let fj = dj - dj.floor();
    let on_node = (fi.abs() < EXACT_NODE_EPS || (1.0 - fi).abs() < EXACT_NODE_EPS)
               && (fj.abs() < EXACT_NODE_EPS || (1.0 - fj).abs() < EXACT_NODE_EPS);

    // vertical first, then time (spec §6.4)
    let mut info = InterpInfo {
        horizontal: if on_node { HorizMethod::Exact } else { HorizMethod::Bilinear },
        vertical: None,
        time: None,
    };
    let l0 = fd.levels[li0];
    let l1 = fd.levels[li1];
    let f0 = if li0 == li1 { v00 } else { vertical_at(v00, l0, v10, l1, level) };
    let f1 = if li0 == li1 { v01 } else { vertical_at(v01, l0, v11, l1, level) };
    if li0 != li1 {
        info.vertical = Some(VertMeta {
            method: LevelInterpMethod::Linear,
            levels: (l0, l1),
            levels_later: (l0, l1),
            per_time: false,
        });
    }
    let h0 = fd.fhs[ti0];
    let h1 = fd.fhs[ti1];
    let val = if ti0 == ti1 { f0 } else { time_lerp(f0, h0, f1, h1, fh) };
    if ti0 != ti1 {
        info.time = Some(TimeMeta {
            method: mode,
            forecast_hours: (h0, h1),
        });
    }
    Ok(PointValue { value: val, info: info })
}
