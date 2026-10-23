//! axum router + endpoint handlers (spec §7). Handlers stay thin: parse args
//! (queryapi), load the arc-swap snapshot once, delegate lookup, render body.
//! All mmaps were opened at snapshot-build time; nothing here touches the
//! filesystem per request.

use axum::routing::Router;
use axum::routing::get;
use axum::extract::State;
use axum::extract::Request;
use axum::response::Response;
use axum::body::Body;
use http::response::Builder;
use http::status::StatusCode;
use std::sync::Arc;
use wx_ingest::index::{SharedIndex, IndexSnapshot};
use wx_ingest::config::{ResolvedModel};
use wx_ingest::pipeline::{level_dir_name};
use wx_core::tables::{fmt_num};
use wx_core::storage::{stat_kind_name};
use wx_core::grid::{GridDef};
use crate::stats::{mono_ns, QueryStats};
use crate::queryapi::{parse_query, lookup_entries, new_entries, render_error_json, render_point_json, fail_http_status, fail_token, arg_token, ParsedArgs, FailTok};
use crate::timeparse::{format_iso_z};

// ---------------------------------------------------------------------------
// shared state (arc-swap index + metrics counters + config view)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ServerState {
    pub index: Arc<SharedIndex>, // clone of Pipeline.shared (same ArcSwap)
    pub stats: Arc<QueryStats>,
    pub models_cfg: Arc<Vec<ResolvedModel>>, // configured params/levels view
}

// ---------------------------------------------------------------------------
// response construction helpers
// ---------------------------------------------------------------------------

/// Build a full HTTP response (status + content-type + text body). Builder
/// failure is unreachable for our constant headers/codes; falls back to a
/// bare 200-empty so we never panic in the hot path.
pub fn make_response(status: u16, ct: String, text: String) -> Response {
    let sc_r = StatusCode::from_u16(status);
    match sc_r {
        Err(_) => { return Response::new(Body::empty()); }
        Ok(sc) => {
            let b: Body = text.into();
            let rb = Builder::new().status(sc).header("Content-Type", ct).body(b);
            match rb {
                Err(_) => { return Response::new(Body::empty()); }
                Ok(r) => { return r; }
            }
        }
    }
}

fn json_error(status: u16, token: String, message: String) -> Response {
    return make_response(status, "application/json".to_string(), render_error_json(token, message));
}

// ---------------------------------------------------------------------------
// /v1/point
// ---------------------------------------------------------------------------

pub async fn point_handler(st: State<ServerState>, req: Request) -> Response {
    let t0 = mono_ns();
    let qs_opt = req.uri().query();
    match qs_opt {
        None => {
            st.stats.record_status(400);
            return json_error(400, "missing_required".to_string(), "no query parameters".to_string());
        }
        Some(qs) => {
            let pa_r = parse_query(qs);
            match pa_r {
                Err(pf) => {
                    st.stats.record_status(400);
                    return json_error(400, arg_token(pf.tok), format!("bad query parameter: {0}", pf.detail));
                }
                Ok(pa) => {
                    let snap = st.index.get_index();
                    let pair = answer_all(snap, &pa);
                    st.stats.record_status(pair.0);
                    let t1 = mono_ns();
                    let us = (((t1 - t0) + 500) / 1000) as u64;
                    st.stats.record_latency(us);
                    return make_response(pair.0, "application/json".to_string(), pair.1);
                }
            }
        }
    }
}

/// Per-parameter selection over the shared point/level/time. V1.1 phase 5:
/// the ZERO-ALLOC lookup is queryapi.lookup_entries (parse already done;
/// entries structured into a fixed-inline buffer); only the body render after
/// it allocates. Decision (spec ambiguous for multi-param errors): the FIRST
/// failing parameter decides the error status; masked points are not errors.
/// Returns (status, body).
pub fn answer_all(snap: Arc<IndexSnapshot>, pa: &ParsedArgs) -> (u16, String) {
    let sref = &*snap;
    let mut eb = new_entries();
    let lr = lookup_entries(sref, &pa, &mut eb);
    match lr {
        Err(fi) => {
            return (fail_http_status(fi.tok), render_error_json(fail_token(fi.tok), error_message(fi.tok, pa.model, fi.param)));
        }
        Ok(_) => {}
    }
    // V1.3 phase E2: the top-level "run" is derived INSIDE the renderer from
    // the per-entry provenance (every result carries its own "run"): echoed
    // when ALL results share one run, null when they differ (defect: with two
    // indexed runs where the newest covers only one requested param, entry 0's
    // run used to stand in for all of them).
    return (200, render_point_json(pa.model, pa.valid_epoch, pa.lat, pa.lon, pa.level_code, pa.level_val, &eb));
}

/// Render a forecast-hour offset (SECONDS past the run time) the way the API
/// reports forecast hours: whole hours as integers, fractional ones via the
/// shared `fmt_num` formatter (same shape as `forecast_hours` in responses).
fn fh_hrs(secs: i64) -> String {
    if secs % 3600 == 0 {
        return format!("{0}", secs / 3600);
    }
    return fmt_num((secs as f64) / 3600.0);
}

fn error_message(ft: FailTok, model: &str, key: &str) -> String {
    match ft {
        FailTok::UnknownModel => { return format!("model '{0}' is not configured/indexed", model); }
        FailTok::UnknownParam => { return format!("parameter '{0}' has no indexed data for model '{1}'", key, model); }
        FailTok::NoRunCovers => { return "no indexed run covers the requested valid time".to_string(); }
        FailTok::LevelOutOfRange => { return "requested level is outside the stored level range (never extrapolated)".to_string(); }
        // V1.3 phase E1: the message NAMES the offending forecast hour and its
        // statistical kind (per-fh guard; spec §6.3). Sentinel payload (-1)
        // falls back to the plain pre-E1 wording.
        FailTok::StatTimeGuard(a, b, off, kind) => {
            if off < 0 {
                return "statistically processed field (template 4.8) requires an exact forecast-hour match; linear time interpolation would be wrong".to_string();
            }
            let kn = stat_kind_name(kind);
            if a < 0 {
                return format!("statistically processed data forbids linear time interpolation: this run stores no forecast-hour bracket for the requested valid time, and forecast hour {0} h is '{1}' (GRIB template 4.8); query that exact forecast hour", fh_hrs(off), kn);
            }
            return format!("linear time interpolation between forecast hours {0} h and {1} h would blend statistically processed data: forecast hour {2} h is '{3}' (GRIB template 4.8); query an exact forecast hour instead", fh_hrs(a), fh_hrs(b), fh_hrs(off), kn);
        }
        FailTok::TimeExactOnly => { return "time_interp=none and no stored forecast hour equals the requested valid time".to_string(); }
        FailTok::OutOfDomain => { return "point is outside the grid node rectangle (horizontal extrapolation is never done)".to_string(); }
    }
}

// ---------------------------------------------------------------------------
// /v1/models: models, grid info (incl LCC params), configured params+levels
// ---------------------------------------------------------------------------

pub async fn models_handler(st: State<ServerState>, _req: Request) -> Response {
    let snap = st.index.get_index();
    let sref = &*snap;
    let cfg = st.models_cfg.clone();
    let mut out = "{\"models\":[".to_string();
    for k in 0 .. cfg.len() {
        if k > 0 {
            out = format!("{0}{1}", out, ",");
        }
        let m = &cfg[k];
        out = format!("{0}{{\"name\":\"{1}\"", out, m.name);
        // grid info from the newest indexed run of this model, else null.
        let gm = sref.find_model(&m.name);
        match gm {
            None => { out = format!("{0},\"grid\":null", out); }
            Some(mm) => {
                match mm.runs.is_empty() {
                    true => { out = format!("{0},\"grid\":null", out); }
                    false => {
                        let gd = &mm.runs[mm.runs.len() - 1].grid;
                        out = format!("{0},\"grid\":{1}", out, render_grid_json(gd));
                    }
                }
            }
        }
        out = format!("{0},\"params\":[", out);
        for pk in 0 .. m.params.len() {
            if pk > 0 {
                out = format!("{0}{1}", out, ",");
            }
            let rp = &m.params[pk];
            out = format!("{0}{{\"name\":\"{1}\",\"units\":\"{2}\",\"levels\":[", out, rp.key, rp.units);
            for lk in 0 .. rp.level_specs.len() {
                if lk > 0 {
                    out = format!("{0}{1}", out, ",");
                }
                out = format!("{0}\"{1}\"", out, render_level_spec(rp.level_specs[lk].0, rp.level_specs[lk].1));
            }
            out = format!("{0}{1}", out, "]}");
        }
        out = format!("{0}{1}", out, "]}");
    }
    out = format!("{0}{1}", out, "]}");
    st.stats.record_status(200);
    return make_response(200, "application/json".to_string(), out);
}

/// Grid definition JSON (spec §7 /v1/models: "grid info incl LCC params").
/// V1.3 phase E4b: `pub` so the gaussian-latlon block shape is pinned by an
/// integration test without needing a synthesized template-3.40 fixture.
/// V1.3 phase E2: the component-convention flag is reported as
/// `source_winds_grid` — it describes how the SOURCE messages arriving in
/// `ingest_dir` were encoded (GRIB flag table 3.3 bit 3, taken from the first
/// message that wrote this run's grid.bin), NOT how data is stored. Stored
/// and queried U/V are ALWAYS earth-relative (spec §4.4 rotates grid-relative
/// components at ingest; field headers carry false), so the value here is
/// informational and may legitimately differ between runs of one model.
pub fn render_grid_json(gd: &GridDef) -> String {
    match gd {
        GridDef::Regular(g) => {
            let nx = fmt_num(g.nx as f64);
            let ny = fmt_num(g.ny as f64);
            let scan = fmt_num(g.scan_mode as f64);
            return format!("{{\"kind\":\"regular\",\"nx\":{0},\"ny\":{1},\"lon1\":{2},\"lat1\":{3},\"dlon\":{4},\"dlat\":{5},\"scan_mode\":{6},\"source_winds_grid\":{7}}}", nx, ny, g.lon1, g.lat1, g.dlon, g.dlat, scan, g.winds_grid);
        }
        GridDef::Lambert(g) => {
            let nx = fmt_num(g.nx as f64);
            let ny = fmt_num(g.ny as f64);
            let pf = fmt_num(g.proj_flag as f64);
            let scan = fmt_num(g.scan_mode as f64);
            let es = fmt_num(g.earth_shape as f64);
            return format!("{{\"kind\":\"lcc\",\"nx\":{0},\"ny\":{1},\"la1\":{2},\"lo1\":{3},\"lov\":{4},\"lad\":{5},\"latin1\":{6},\"latin2\":{7},\"dx_m\":{8},\"dy_m\":{9},\"proj_flag\":{10},\"scan_mode\":{11},\"earth_shape\":{12},\"source_winds_grid\":{13}}}", nx, ny, g.la1, g.lo1, g.lov, g.lad, g.latin1, g.latin2, g.dx_m, g.dy_m, pf, scan, es, g.winds_grid);
        }
        GridDef::Gaussian(g) => {
            // V1.3 phase E4b (GRIB template 3.40). la1/la2 are the row ends
            // AS RECORDED IN SECTION 3; the authoritative row table is the
            // P_ny-root table wx-core rebuilds from ny (see grid.rs docs),
            // and lat +-90 is always out of domain on such grids.
            let nx = fmt_num(g.nx as f64);
            let ny = fmt_num(g.ny as f64);
            let scan = fmt_num(g.scan_mode as f64);
            let es = fmt_num(g.earth_shape as f64);
            return format!("{{\"kind\":\"gaussian_latlon\",\"nx\":{0},\"ny\":{1},\"lon1\":{2},\"la1\":{3},\"la2\":{4},\"dlon\":{5},\"scan_mode\":{6},\"earth_shape\":{7},\"source_winds_grid\":{8}}}", nx, ny, g.lon1, g.la1, g.la2, g.dlon, scan, es, g.winds_grid);
        }
    }
}

/// Configured level spec back to its string form (config grammar).
/// Render a parsed CONFIG level spec back to its canonical string for
/// /v1/models. Public so the hybrid/wildcard shapes can be pinned at unit
/// level (same rationale as `render_grid_json` being public since E4b).
pub fn render_level_spec(code: u8, val: Option<f64>) -> String {
    if code == 255 {
        return "*".to_string();
    }
    let nm = level_dir_name(code); // canonical dir name == canonical spelling
    match val {
        None => { return format!("{0}:*", nm); }
        Some(x) => {
            if code == 1 && x == 0.0 {
                return nm;
            }
            let num = fmt_num(x);
            return format!("{0}:{1}", nm, num);
        }
    }
}

// ---------------------------------------------------------------------------
// /v1/inventory?model= : runs + per-run queryable fhs/params/levels
// (derived from the snapshot on demand; not a hot path)
// ---------------------------------------------------------------------------

pub async fn inventory_handler(st: State<ServerState>, req: Request) -> Response {
    let mut want_model: String = "".to_string();
    let qs_opt = req.uri().query();
    match qs_opt {
        None => {}
        Some(qs) => {
            let mv = qs_value(qs, "model");
            match mv {
                None => {}
                Some(v) => {
                    if !v.is_empty() {
                        want_model = v;
                    }
                }
            }
        }
    }
    let snap = st.index.get_index();
    let sref = &*snap;
    if !want_model.is_empty() {
        match sref.find_model(&want_model) {
            None => {
                st.stats.record_status(404);
                return json_error(404, "unknown_model".to_string(), format!("model '{0}' is not indexed", want_model));
            }
            Some(_) => {}
        }
    }
    // NOTE (bug fixed this milestone): each object item carries its own
    // leading separator comma AND must open with '{'; earlier code emitted
    // neither consistently (invalid JSON like [,"model" ... },,"param").
    let mut out = "{\"inventory\":[".to_string();
    let mut first_model = true;
    for mi in 0 .. sref.models.len() {
        let mm = &sref.models[mi];
        if !want_model.is_empty() && mm.name != want_model {
            continue;
        }
        if !first_model {
            out = format!("{0}{1}", out, ",");
        }
        first_model = false;
        out = format!("{0}{{\"model\":\"{1}\",\"runs\":[", out, mm.name);
        for ri in 0 .. mm.runs.len() {
            if ri > 0 {
                out = format!("{0}{1}", out, ",");
            }
            let rr = &mm.runs[ri];
            let run_s = format_iso_z(rr.epoch_secs);
            out = format!("{0}{{\"run\":\"{1}\",\"dir\":\"{2}\",\"fields\":[", out, run_s, rr.dir_name);
            for fi in 0 .. rr.fields.len() {
                if fi > 0 {
                    out = format!("{0}{1}", out, ",");
                }
                let fe = &rr.fields[fi];
                let lvl_dir = level_dir_name(fe.level_code);
                out = format!("{0}{{\"param\":\"{1}\",\"level_type\":\"{2}\",\"units\":\"{3}\",\"stat_processed\":{4},\"stat_kind\":\"{5}\"", out, fe.param_key, lvl_dir, fe.fd.units, fe.fd.stat_processed, fe.fd.stat_kind);
                out = format!("{0},\"forecast_hours\":[", out);
                for kh in 0 .. fe.fd.fhs.len() {
                    if kh > 0 {
                        out = format!("{0}{1}", out, ",");
                    }
                    // decimal HOURS in the inventory (fractional forecast
                    // hours print as 0.5, integral ones stay 0/6/12)
                    out = format!("{0}{1}", out, fmt_num(fe.fd.fhs[kh].fh_secs as f64 / 3600.0));
                }
                out = format!("{0}{1}", out, "]");
                // levels of the newest fh file for this field (informational,
                // same convention as manifest.json).
                out = format!("{0},\"levels\":[", out);
                if !fe.fd.fhs.is_empty() {
                    let mf = &fe.fd.fhs[fe.fd.fhs.len() - 1].field;
                    for lv in 0 .. mf.header.levels.len() {
                        if lv > 0 {
                            out = format!("{0}{1}", out, ",");
                        }
                        let lnum = fmt_num(mf.header.levels[lv] as f64);
                        out = format!("{0}{1}", out, lnum);
                    }
                }
                out = format!("{0}{1}", out, "]}");
            }
            out = format!("{0}{1}", out, "]}");
        }
        out = format!("{0}{1}", out, "]}");
    }
    out = format!("{0}{1}", out, "]}");
    st.stats.record_status(200);
    return make_response(200, "application/json".to_string(), out);
}

/// Tiny raw-query value lookup for non-/point endpoints.
fn qs_value(qs: &str, key: &str) -> Option<String> {
    let n = qs.len();
    let mut pos = 0;
    while pos <= n {
        let mut end = pos;
        while end < n && qs[end .. end + 1] != *"&" {
            end += 1;
        }
        let seg = &qs[pos .. end];
        let eqk = key.len();
        if seg.len() > eqk && seg[0 .. eqk] == *key && (seg[eqk .. eqk + 1] == *":" || seg[eqk .. eqk + 1] == *"=") {
            return Some(seg[eqk + 1 .. seg.len()].to_string());
        }
        pos = end + 1;
    }
    return None;
}

// ---------------------------------------------------------------------------
// /health /ready /metrics
// ---------------------------------------------------------------------------

pub async fn health_handler(st: State<ServerState>, _req: Request) -> Response {
    st.stats.record_status(200);
    return make_response(200, "application/json".to_string(), "{\"ok\":true}".to_string());
}

/// Ready once at least one indexed run exists per configured model (spec §7).
pub async fn ready_handler(st: State<ServerState>, _req: Request) -> Response {
    let snap = st.index.get_index();
    let sref = &*snap;
    let cfg = st.models_cfg.clone();
    let mut missing: Vec<String> = Vec::<String>::new();
    for k in 0 .. cfg.len() {
        let m = &cfg[k];
        let gm = sref.find_model(&m.name);
        match gm {
            None => { missing.push(m.name.to_string()); }
            Some(mm) => {
                if mm.runs.is_empty() {
                    missing.push(m.name.to_string());
                }
            }
        }
    }
    if missing.is_empty() {
        st.stats.record_status(200);
        return make_response(200, "application/json".to_string(), "{\"ready\":true}".to_string());
    }
    let mut joined = "".to_string();
    for k in 0 .. missing.len() {
        if k > 0 {
            joined = format!("{0}{1}", joined, ",");
        }
        joined = format!("{0}{1}", joined, missing[k]);
    }
    st.stats.record_status(503);
    return make_response(503, "application/json".to_string(), render_error_json("not_ready".to_string(), format!("models awaiting first indexed run: {0}", joined)));
}

pub async fn metrics_handler(st: State<ServerState>, _req: Request) -> Response {
    st.stats.record_status(200);
    return make_response(200, "text/plain; version=0.0.4".to_string(), st.stats.render_prometheus());
}

/// Fallback for unknown paths: JSON error with a stable token.
pub async fn not_found_handler(st: State<ServerState>, _req: Request) -> Response {
    st.stats.record_status(404);
    return json_error(404, "unknown_endpoint".to_string(), "path is not a served endpoint".to_string());
}

// ---------------------------------------------------------------------------
// router assembly
// ---------------------------------------------------------------------------

pub fn build_router(state: ServerState) -> Router {
    return Router::new()
        .route("/v1/point", get(point_handler))
        .route("/v1/models", get(models_handler))
        .route("/v1/inventory", get(inventory_handler))
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler))
        .route("/metrics", get(metrics_handler))
        .fallback(not_found_handler)
        .with_state(state);
}
