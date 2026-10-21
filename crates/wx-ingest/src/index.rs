//! In-memory index (spec §5): immutable snapshots published through arc-swap;
//! query threads load one `Arc<IndexSnapshot>` and walk it with no locks and
//! no allocation beyond the response body. All mmaps are opened at index-build
//! time (one `Arc<MmapField>` per .bin file, shared across snapshots).
//!
//! ## Server-facing API (milestone 4 contract)
//!
//! ```text
//! SharedIndex.get_index() -> Arc<IndexSnapshot>            (arc-swap load)
//! build_context(snapshot, model, run?, param, level_code, want_level,
//!               want_fh_secs, mode) -> PointQueryContext     (BORROWS the snapshot)
//!   (want_fh_secs = valid_time - run_time in SECONDS; every time comparison
//!   and interpolation happens in seconds, while the REPORTED
//!   InterpInfo.forecast_hours stay HOURS to match spec §7)
//! evaluate_point(ctx, lat, lon) -> wx_core QueryResult    (f64 value + InterpInfo)
//! ```
//!
//! V1.1 phase 5 (spec §5 "no heap allocation on the hot path"): the whole
//! selection/assembly/evaluation chain allocates NOTHING — contexts borrow
//! grid defs, units strings and mmap handles out of the snapshot the caller
//! already pins with its `Arc`.

use arc_swap::ArcSwap;
use std::sync::Arc;
use wx_core::grid::GridDef;
use wx_core::storage::{MmapField};
use wx_core::interp::{
    EXACT_NODE_EPS, TIME_MATCH_EPS, LevelInterpMethod, TimeInterpMode, HorizMethod,
    InterpInfo, PointError, PointValue, QueryResult, VertMeta, TimeMeta,
    bilinear4, horiz_nodes, resolve_levels_f32, time_lerp, vertical_at,
};

/// A forecast-hour slot inside one field entry. `fh_secs` is the offset from
/// the run time IN SECONDS (an exact key for fractional forecast hours); the
/// PDT 4.8 statistical range for THIS slot travels with it so a restart can
/// restore it and future endpoints can report it.
#[derive(Clone)]
pub struct IdxFh {
    pub fh_secs: i64,
    pub field: Arc<MmapField>, // opened at index build time
    pub stat_has_range: bool,
    pub stat_begin_secs: i64, // offsets from the run time
    pub stat_end_secs: i64,
    pub stat_step_secs: i64,
}

#[derive(Clone)]
pub struct IdxField {
    pub units: String,
    pub stat_processed: bool, // template 4.8 guard input
    pub stat_kind: String,
    pub fhs: Vec<IdxFh>, // sorted by fh_secs ascending
}

#[derive(Clone)]
pub struct IdxFieldEntry {
    pub param_key: String, // canonical name or "d{D}c{C}n{N}" raw override
    pub level_code: u8, // CT 4.5
    pub fd: IdxField,
}

#[derive(Clone)]
pub struct IdxRun {
    pub epoch_secs: i64,
    pub dir_name: String, // <run YYYYMMDDHH>
    pub grid: Arc<GridDef>,
    pub fields: Vec<IdxFieldEntry>,
}

#[derive(Clone)]
pub struct IdxModel {
    pub name: String,
    pub runs: Vec<IdxRun>, // ascending epoch
}

#[derive(Clone)]
pub struct IndexSnapshot {
    pub models: Vec<IdxModel>, // sorted by name
}

impl IndexSnapshot {
    pub fn find_model(self: &Self, name: &str) -> Option<&IdxModel> {
        for m in &self.models {
            if m.name == name {
                return Some(m);
            }
        }
        return None;
    }

    /// Latest run (or pinned one) whose field covers `want_fh_secs` (offset
    /// from that run's reference time in SECONDS) under `mode`; both bracketing
    /// slots must exist when interpolating (spec §6.3). V1.1 phase 5: returns
    /// BORROWED pointers into the snapshot and resolves the forecast-hour pair
    /// through [`resolve_field_times`] with NO scratch vector (the old
    /// `Vec<f64>` materialization is gone; equivalence is proven in
    /// tests/alloc_free_select.rs).
    pub fn select_run(self: &Self, model: &str, param_key: &str, level_code: u8,
                      want_fh_secs: f64, mode: TimeInterpMode,
                      pinned_epoch: Option<i64>)
        -> Result<(usize, usize, &IdxRun, &IdxFieldEntry), SelectError> {
        let m = self.find_model(&model);
        match m {
            None => { return Err(SelectError::UnknownModel); }
            Some(mm) => {
                let mut ri = mm.runs.len();
                while ri > 0 {
                    ri -= 1;
                    let run = &mm.runs[ri];
                    if pinned_epoch.is_some() && run.epoch_secs != pinned_epoch.unwrap() {
                        continue;
                    }
                    let fe = find_field(run, &param_key, level_code);
                    match fe {
                        None => { continue; }
                        Some(f) => {
                            let pair = resolve_field_times(&f.fd.fhs, want_fh_secs, mode, f.fd.stat_processed);
                            match pair {
                                Err(e) => {
                                    match e {
                                        PointError::NoTimeBracket => { continue; }
                                        _ => { return Err(map_time_err(e)); }
                                    }
                                }
                                Ok(p) => {
                                    return Ok((p.0, p.1, run, f));
                                }
                            }
                        }
                    }
                }
                return Err(SelectError::NoRunCovers);
            }
        }
    }
}

fn find_field<'r>(run: &'r IdxRun, param_key: &str, level_code: u8) -> Option<&'r IdxFieldEntry> {
    for f in &run.fields {
        if f.param_key == param_key && f.level_code == level_code {
            return Some(f);
        }
    }
    return None;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SelectError {
    UnknownModel, // server: 404
    NoRunCovers, // server: 404
    LevelOutOfRange, // server: 422
    TimeExactOnly, // server: 422
    StatTimeGuard, // server: 422
}

fn map_time_err(e: PointError) -> SelectError {
    match e {
        PointError::TimeExactOnly => { return SelectError::TimeExactOnly; }
        PointError::StatTimeGuard => { return SelectError::StatTimeGuard; }
        _ => { return SelectError::NoRunCovers; }
    }
}

/// Slice-free twin of wx-core `resolve_times` (V1.1 phase 5): resolves the
/// forecast-hour bracket straight out of the index slot list so selection
/// allocates nothing. Bit-for-bit identical to materializing `Vec<f64>` of
/// `fh_secs as f64` and calling `resolve_times`: same widening per element,
/// same TIME_MATCH_EPS exact-match scan, same guard order (exact -> none ->
/// stat -> single -> bracket). Equivalence proven in tests/alloc_free_select.rs.
pub fn resolve_field_times(fhs: &[IdxFh], want_fh_secs: f64, mode: TimeInterpMode,
                           stat_processed: bool) -> Result<(usize, usize), PointError> {
    let n = fhs.len();
    if n == 0 {
        return Err(PointError::NoTimeBracket);
    }
    let mut k = 0;
    while k < n {
        let h = fhs[k].fh_secs as f64;
        if (h - want_fh_secs).abs() <= TIME_MATCH_EPS {
            return Ok((k, k));
        }
        k += 1;
    }
    if mode == TimeInterpMode::None {
        return Err(PointError::TimeExactOnly);
    }
    if stat_processed {
        return Err(PointError::StatTimeGuard);
    }
    if n == 1 {
        return Err(PointError::NoTimeBracket);
    }
    let mut k2 = 1;
    while k2 < n {
        let a = fhs[k2 - 1].fh_secs as f64;
        let b = fhs[k2].fh_secs as f64;
        if a != b && (a - want_fh_secs) * (b - want_fh_secs) <= 0.0 {
            return Ok((k2 - 1, k2));
        }
        k2 += 1;
    }
    return Err(PointError::NoTimeBracket);
}

// ---------------------------------------------------------------------------
// point query context + evaluation
// ---------------------------------------------------------------------------

/// Up to two time slots (one per bracketing forecast hour); each references
/// the shared .bin file plus the two level indices bracketing the requested
/// level INSIDE that file. V1.1 phase 5: BORROWS the mmap handle from the
/// snapshot (no Arc clone, no refcount traffic) — the caller keeps the
/// `Arc<IndexSnapshot>` alive for the whole request, which is what makes
/// borrowing sound here.
#[derive(Clone)]
pub struct SamplesSlot<'s> {
    pub field: &'s MmapField,
    pub li0: usize,
    pub li1: usize,
}

/// Assembled samples for one point query, borrowing EVERYTHING from the
/// snapshot it was built against (grid def, units string, mmap handles):
/// zero heap allocation, zero refcount churn, zero locks (spec §5).
#[derive(Clone)]
pub struct PointQueryContext<'s> {
    pub grid: &'s GridDef,
    pub units: &'s str,
    pub run_epoch_secs: i64,
    pub stat_processed: bool,
    pub want_level: f64,
    pub want_fh_secs: f64, // offset from run time, SECONDS
    pub fh_secs: (f64, f64), // chosen slots, SECONDS
    pub slots: [Option<SamplesSlot<'s>>; 2],
}

/// Assemble the up-to-4 (level,time) samples for one point query. V1.1
/// phase 5: replaces the old `seed.build(...)` contract — the seed literal
/// was pure ceremony (build overwrote every field), and its owned `units`
/// String plus the per-slot level Vec were the last query-path allocations.
/// Now everything is borrowed or stack-resolved; ZERO allocations (proven
/// by tests/hotpath_alloc.rs and the full-handler test).
pub fn build_context<'s>(snapshot: &'s IndexSnapshot, model: &str,
                         pinned_epoch: Option<i64>, param_key: &str, level_code: u8,
                         want_level: f64, want_fh_secs: f64, mode: TimeInterpMode)
    -> Result<PointQueryContext<'s>, SelectError> {
    let sel = snapshot.select_run(&model, &param_key, level_code, want_fh_secs, mode, pinned_epoch);
    match sel {
        Err(e) => { return Err(e); }
        Ok(t) => {
            let run = t.2;
            let fe = t.3;
            let mut ctx = PointQueryContext {
                grid: &run.grid,
                units: &fe.fd.units,
                run_epoch_secs: run.epoch_secs,
                stat_processed: fe.fd.stat_processed,
                want_level: want_level,
                want_fh_secs: want_fh_secs,
                fh_secs: (0.0, 0.0),
                slots: [None, None],
            };
            let ti = (t.0, t.1);
            let mut k = 0;
            let idxs = |a: usize| -> usize { if a == 0 { return ti.0; } return ti.1; };
            while k < 2 {
                let ti_k = idxs(k);
                let slot_fh = fe.fd.fhs[ti_k].fh_secs as f64;
                if k == 1 && ti.1 == ti.0 {
                    ctx.slots[1] = ctx.slots[0].clone();
                    ctx.fh_secs = (slot_fh, slot_fh);
                    return Ok(ctx);
                }
                let mf = &fe.fd.fhs[ti_k].field;
                // resolve directly against the mmapped header levels (f32):
                // no scratch vector; widening happens once per compared element
                // exactly like the removed Vec<f64> copy did.
                let pair = resolve_levels_f32(&mf.header.levels, want_level);
                match pair {
                    None => { return Err(SelectError::LevelOutOfRange); }
                    Some(p) => {
                        ctx.slots[k] = Some(SamplesSlot {
                            field: mf,
                            li0: p.0,
                            li1: p.1,
                        });
                        if k == 0 {
                            ctx.fh_secs = (slot_fh, slot_fh);
                        } else {
                            ctx.fh_secs = (ctx.fh_secs.0, slot_fh);
                        }
                    }
                }
                k += 1;
            }
            return Ok(ctx);
        }
    }
}

/// Evaluate the assembled context at (lat, lon): horizontal bilinear at each
/// of the (up to) 2 levels x 2 times, then vertical, then time (spec §6.4),
/// all f64. Mirrors wx-core interp.query_point but reads through mmapped
/// fields (no slab copies).
pub fn evaluate_point<'s>(ctx: &PointQueryContext<'s>, lat: f64, lon: f64) -> QueryResult {
    let gd = &ctx.grid;
    let proj = gd.project(lat, lon);
    match proj {
        None => { return Err(PointError::OutOfDomain); }
        Some((di, dj)) => {
            let nx = gd.nx() as usize;
            let ny = gd.ny() as usize;
            let wrap = gd.lon_wrap();
            let nodes = horiz_nodes(nx, ny, di, dj, wrap);

            let h_at = |slot: &SamplesSlot, li: usize| -> f64 {
                let mf = &slot.field;
                let v00 = mf.value(li, nodes.1, nodes.0) as f64;
                let v10 = mf.value(li, nodes.1, nodes.2) as f64;
                let v01 = mf.value(li, nodes.3, nodes.0) as f64;
                let v11 = mf.value(li, nodes.3, nodes.2) as f64;
                return bilinear4(v00, v10, v01, v11, nodes.4, nodes.5);
            };

            let s0 = &ctx.slots[0];
            match s0 {
                None => { return Err(PointError::MissingData); }
                Some(ss0) => {
                    let s1 = &ctx.slots[1];

                    // ---- SLOT A (earlier forecast hour): OWN level indices ----
                    let a_lv0 = ss0.field.header.levels[ss0.li0] as f64;
                    let a_lv1 = ss0.field.header.levels[ss0.li1] as f64;
                    let a_brk = ss0.li1 != ss0.li0;
                    let va0 = h_at(&ss0, ss0.li0);
                    let mut va1 = va0;
                    if a_brk {
                        va1 = h_at(&ss0, ss0.li1);
                    }

                    // ---- SLOT B (later forecast hour): OWN indices + OWN header.
                    // V1.2 phase B0 CRITICAL FIX: pre-change this slot was read
                    // with `ss0.li0` / `ss0.li1`, so whenever the two forecast
                    // hours carried DIFFERENT level stacks (e.g. f006 [1000,850,
                    // 700] vs f012 [1000,925,850,700]) slot B read the WRONG
                    // PLANE and was vertically interpolated on slot A's pressure
                    // bracket: a plausible-but-wrong number, no error (§6.4
                    // violated). `SamplesSlot` carries li0/li1 PER SLOT for
                    // exactly this reason. Regression test: tests/slot_levels.rs.
                    let mut b_lv0 = a_lv0;
                    let mut b_lv1 = a_lv1;
                    let mut vb0 = va0;
                    let mut vb1 = va1;
                    let mut b_brk = a_brk;
                    match s1 {
                        None => {}
                        Some(sr) => {
                            b_lv0 = sr.field.header.levels[sr.li0] as f64;
                            b_lv1 = sr.field.header.levels[sr.li1] as f64;
                            vb0 = h_at(&sr, sr.li0);
                            vb1 = vb0;
                            b_brk = sr.li1 != sr.li0;
                            if b_brk {
                                vb1 = h_at(&sr, sr.li1);
                            }
                        }
                    }

                    if va0.is_nan() || va1.is_nan() || vb0.is_nan() || vb1.is_nan() {
                        return Err(PointError::MissingData);
                    }
                    let fi = di - di.floor();
                    let fj = dj - dj.floor();
                    let on_node = (fi.abs() < EXACT_NODE_EPS || (1.0 - fi).abs() < EXACT_NODE_EPS)
                               && (fj.abs() < EXACT_NODE_EPS || (1.0 - fj).abs() < EXACT_NODE_EPS);
                    let mut info = InterpInfo {
                        horizontal: if on_node { HorizMethod::Exact } else { HorizMethod::Bilinear },
                        vertical: None,
                        time: None,
                    };
                    // Spec §6.4 ORDER: vertical happens SEPARATELY at each time
                    // slot (each inside that slot's own bracket), THEN the two
                    // vertical results are blended in time. Degenerate cases:
                    // an exact level hit in one slot (li1 == li0) skips that
                    // slot's vertical step (no divide-by-zero); a level can be
                    // exact-present in ONE slot and bracketed in the other.
                    let fa = if a_brk { vertical_at(va0, a_lv0, va1, a_lv1, ctx.want_level) } else { va0 };
                    let fb = if b_brk { vertical_at(vb0, b_lv0, vb1, b_lv1, ctx.want_level) } else { vb0 };
                    if a_brk || b_brk {
                        // Report slot A's pair in "levels" (spec §7 shape, back-
                        // compatible). When the two slots bracket DIFFERENTLY the
                        // single pair would misdescribe slot B, so the response
                        // ADDITIVELY gains "levels_per_time" (documented in
                        // docs/DECISIONS.md + README API section).
                        let differs = !(a_lv0 == b_lv0 && a_lv1 == b_lv1);
                        info.vertical = Some(VertMeta {
                            method: LevelInterpMethod::Linear,
                            levels: (a_lv0, a_lv1),
                            levels_later: (b_lv0, b_lv1),
                            per_time: differs,
                        });
                    }
                    // Time arithmetic runs in SECONDS: whole-second offsets are
                    // exact integers there (fractional forecast hours like 1800 s
                    // are not exactly representable as decimal hours), so the
                    // interpolation is at least as accurate as the hour-based
                    // form it replaces. The REPORTED bracket converts back to
                    // HOURS because spec §7 shows forecast_hours in hours.
                    let val = if ctx.fh_secs.0 == ctx.fh_secs.1 {
                        fa
                    } else {
                        time_lerp(fa, ctx.fh_secs.0, fb, ctx.fh_secs.1, ctx.want_fh_secs)
                    };
                    if ctx.fh_secs.0 != ctx.fh_secs.1 {
                        info.time = Some(TimeMeta {
                            method: TimeInterpMode::Linear,
                            forecast_hours: (ctx.fh_secs.0 / 3600.0, ctx.fh_secs.1 / 3600.0),
                        });
                    }
                    return Ok(PointValue { value: val, info: info });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// publication wrapper
// ---------------------------------------------------------------------------

/// ArcSwap-published index; ingest builds a NEW snapshot and swaps atomically
/// (spec §5). Readers hold the returned Arc for the whole request; mmap files
/// stay mapped until every snapshot referencing them is gone (§4.6 retention
/// relies exactly on this).
pub struct SharedIndex {
    pub store: ArcSwap<IndexSnapshot>,
}

impl SharedIndex {
    /// Load the current snapshot (no locks, no allocation).
    pub fn get_index(self: &Self) -> Arc<IndexSnapshot> {
        return self.store.load_full();
    }

    /// Atomically publish a new snapshot.
    pub fn publish(self: &Self, snap: Arc<IndexSnapshot>) {
        self.store.swap(snap);
    }
}
