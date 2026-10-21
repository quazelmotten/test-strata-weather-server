//! In-memory index (spec §5): immutable snapshots published through arc-swap;
//! query threads load one `Arc<IndexSnapshot>` and walk it with no locks and
//! no allocation beyond the response body. All mmaps are opened at index-build
//! time (one `Arc<MmapField>` per .bin file, shared across snapshots).
//!
//! ## Server-facing API (milestone 4 contract)
//!
//! ```text
//! SharedIndex.get_index() -> Arc<IndexSnapshot>            (arc-swap load)
//! IndexSnapshot.point_context(model, run?, param, level_code, want_level,
//!                             want_fh_secs, mode) -> PointQueryContext
//!   (want_fh_secs = valid_time - run_time in SECONDS; every time comparison
//!   and interpolation happens in seconds, while the REPORTED
//!   InterpInfo.forecast_hours stay HOURS to match spec §7)
//! evaluate_point(ctx, lat, lon) -> wx_core QueryResult    (f64 value + InterpInfo)
//! ```

use arc_swap::ArcSwap;
use std::sync::Arc;
use wx_core::grid::GridDef;
use wx_core::storage::{MmapField};
use wx_core::interp::{
    EXACT_NODE_EPS, LevelInterpMethod, TimeInterpMode, HorizMethod,
    InterpInfo, PointError, PointValue, QueryResult, VertMeta, TimeMeta,
    bilinear4, horiz_nodes, resolve_levels, resolve_times, time_lerp, vertical_at,
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
    /// slots must exist when interpolating (spec §6.3).
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
                            let mut fhs: Vec<f64> = Vec::<f64>::new();
                            for e in &f.fd.fhs {
                                fhs.push(e.fh_secs as f64);
                            }
                            let pair = resolve_times(&fhs, want_fh_secs, mode, f.fd.stat_processed);
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

// ---------------------------------------------------------------------------
// point query context + evaluation
// ---------------------------------------------------------------------------

/// Up to two time slots (one per bracketing forecast hour); each references
/// the shared .bin file plus the two level indices bracketing the requested
/// level INSIDE that file.
#[derive(Clone)]
pub struct SamplesSlot {
    pub field: Arc<MmapField>,
    pub li0: usize,
    pub li1: usize,
}

#[derive(Clone)]
pub struct PointQueryContext {
    pub grid: Arc<GridDef>,
    pub units: String,
    pub run_epoch_secs: i64,
    pub stat_processed: bool,
    pub want_level: f64,
    pub want_fh_secs: f64, // offset from run time, SECONDS
    pub fh_secs: (f64, f64), // chosen slots, SECONDS
    pub slots: [Option<SamplesSlot>; 2],
}

impl PointQueryContext {
    /// Assemble the up-to-4 (level,time) samples for one point query.
    pub fn build(self: &Self, snapshot: &IndexSnapshot, model: &str,
                 pinned_epoch: Option<i64>, param_key: &str, level_code: u8,
                 want_level: f64, want_fh_secs: f64, mode: TimeInterpMode)
        -> Result<PointQueryContext, SelectError> {
        let sel = snapshot.select_run(&model, &param_key, level_code, want_fh_secs, mode, pinned_epoch);
        match sel {
            Err(e) => { return Err(e); }
            Ok(t) => {
                let run = t.2;
                let fe = t.3;
                let mut ctx = PointQueryContext {
                    grid: run.grid.clone(),
                    units: fe.fd.units.clone(),
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
                    let mf = fe.fd.fhs[ti_k].field.clone();
                    let mut lv: Vec<f64> = Vec::<f64>::new();
                    for v in &mf.header.levels {
                        lv.push(*v as f64);
                    }
                    let pair = resolve_levels(&lv, want_level);
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
}

/// Evaluate the assembled context at (lat, lon): horizontal bilinear at each
/// of the (up to) 2 levels x 2 times, then vertical, then time (spec §6.4),
/// all f64. Mirrors wx-core interp.query_point but reads through mmapped
/// fields (no slab copies).
pub fn evaluate_point(ctx: &PointQueryContext, lat: f64, lon: f64) -> QueryResult {
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
                    let v00 = h_at(&ss0, ss0.li0);
                    let mut v01 = v00;
                    match s1 {
                        None => {}
                        Some(sr) => { v01 = h_at(&sr, ss0.li0); }
                    }
                    let l0 = ss0.field.header.levels[ss0.li0] as f64;
                    let mut l1 = l0;
                    let mut v10 = v00;
                    let mut v11 = v01;
                    if ss0.li1 != ss0.li0 {
                        l1 = ss0.field.header.levels[ss0.li1] as f64;
                        v10 = h_at(&ss0, ss0.li1);
                        match s1 {
                            None => { v11 = v10; }
                            Some(sr) => { v11 = h_at(&sr, ss0.li1); }
                        }
                    }
                    if v00.is_nan() || v01.is_nan() || v10.is_nan() || v11.is_nan() {
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
                    let f0 = if ss0.li1 == ss0.li0 { v00 } else { vertical_at(v00, l0, v10, l1, ctx.want_level) };
                    let f1 = if ss0.li1 == ss0.li0 { v01 } else { vertical_at(v01, l0, v11, l1, ctx.want_level) };
                    if ss0.li1 != ss0.li0 {
                        info.vertical = Some(VertMeta {
                            method: LevelInterpMethod::Linear,
                            levels: (l0, l1),
                        });
                    }
                    // Time arithmetic runs in SECONDS: whole-second offsets are
                    // exact integers there (fractional forecast hours like 1800 s
                    // are not exactly representable as decimal hours), so the
                    // interpolation is at least as accurate as the hour-based
                    // form it replaces. The REPORTED bracket converts back to
                    // HOURS because spec §7 shows forecast_hours in hours.
                    let val = if ctx.fh_secs.0 == ctx.fh_secs.1 {
                        f0
                    } else {
                        time_lerp(f0, ctx.fh_secs.0, f1, ctx.fh_secs.1, ctx.want_fh_secs)
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
