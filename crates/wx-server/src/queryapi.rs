//! /v1/point request parsing, run selection over the arc-swap snapshot and
//! response JSON rendering (spec §7 + §6 semantics via the milestone-3 index
//! contract). HOT PATH RULES (spec §5): the lookup path (snapshot load ->
//! PointQueryContext.build -> evaluate_point) performs no locks beyond the
//! arc-swap load, no file I/O beyond mmap page faults and no per-request
//! memmap open; the only allocations are tiny Vec/String clones inherited
//! from the milestone-3 contract (units string + level/fh scratch vectors),
//! measured and documented in tests/hotpath_alloc.rs. Response body
//! construction (render_point_json) allocates freely, which the spec allows.

use crate::timeparse::{parse_iso_z};
use wx_ingest::index::{IndexSnapshot, PointQueryContext, evaluate_point, SelectError};
use wx_core::interp::{TimeInterpMode, HorizMethod, PointError, InterpInfo, LevelInterpMethod};
use wx_core::tables::{parse_level_str, format_level_str, parse_f64, fmt_num};
use wx_core::grid::{GridDef};
use std::sync::Arc;

/// One parsed /v1/point request (spec §7 parameter table).
#[derive(Clone, Debug)]
pub struct ParsedArgs {
    pub model: String,
    pub params: Vec<String>, // csv split, order preserved
    pub level_code: u8, // CT 4.5
    pub level_val: f64, // numeric coordinate (0 for bare `surface`)
    pub lat: f64,
    pub lon: f64, // accepted -180..360; normalized by GridDef.project
    pub valid_epoch: i64,
    pub pin_epoch: Option<i64>, // optional `run` pin
    pub mode: TimeInterpMode,
}

#[derive(Clone, Copy, Debug)]
pub enum ArgTok {
    MissingRequired, // one of the six required keys absent
    UnknownKey, // unrecognized query key
    BadLat, // malformed or outside -90..90
    BadLon, // malformed or outside -180..360
    BadLevel, // level grammar violation (see tables.parse_level_str)
    BadTime, // not exactly YYYY-MM-DDTHH:MM:SSZ
    BadTimeInterp, // time_interp neither linear nor none
    EmptyValue, // model/param present but empty
}

#[derive(Clone, Debug)]
pub struct ParseFail {
    pub tok: ArgTok,
    pub detail: String, // offending key/value text for the message
}

/// Stable machine tokens (documented in docs/DECISIONS.md).
pub fn arg_token(t: ArgTok) -> String {
    match t {
        ArgTok::MissingRequired => { return "missing_required".to_string(); }
        ArgTok::UnknownKey => { return "unknown_query_param".to_string(); }
        ArgTok::BadLat => { return "bad_lat".to_string(); }
        ArgTok::BadLon => { return "bad_lon".to_string(); }
        ArgTok::BadLevel => { return "bad_level".to_string(); }
        ArgTok::BadTime => { return "bad_time".to_string(); }
        ArgTok::BadTimeInterp => { return "bad_time_interp".to_string(); }
        ArgTok::EmptyValue => { return "empty_value".to_string(); }
    }
}

// ---------------------------------------------------------------------------
// query-string parsing (manual walk: keys compared as slices, values owned)
// ---------------------------------------------------------------------------

pub fn parse_query(qs: &str) -> Result<ParsedArgs, ParseFail> {
    let mut model = "".to_string();
    let mut params: Vec<String> = Vec::<String>::new();
    let mut has_level = false;
    let mut level_code = 0;
    let mut level_val = 0.0;
    let mut has_lat = false;
    let mut has_lon = false;
    let mut has_valid = false;
    let mut lat = 0.0;
    let mut lon = 0.0;
    let mut valid_ep = 0;
    let mut pin: Option<i64> = None;
    let mut mode = TimeInterpMode::Linear;

    let n = qs.len();
    let mut pos = 0;
    while pos <= n {
        let mut end = pos;
        while end < n && qs[end .. end + 1] != *"&" {
            end += 1;
        }
        let seg = &qs[pos .. end];
        if !seg.is_empty() {
            // split key=value at first '='
            let mut eq = seg.len();
            let mut k = 0;
            while k < seg.len() {
                if seg[k .. k + 1] != *"=" {
                    k += 1;
                    continue;
                }
                eq = k;
                break;
            }
            let key = &seg[0 .. eq];
            let val = if eq < seg.len() { seg[eq + 1 .. seg.len()].to_string() } else { "".to_string() };
            if *key == *"model" {
                if val.is_empty() {
                    return Err(ParseFail { tok: ArgTok::EmptyValue, detail: "model".to_string() });
                }
                model = val.to_string();
            } else if *key == *"param" {
                if val.is_empty() {
                    return Err(ParseFail { tok: ArgTok::EmptyValue, detail: "param".to_string() });
                }
                let mut ppos = 0;
                loop {
                    let plen = val.len();
                    let mut pend = ppos;
                    while pend < plen && val[pend .. pend + 1] != *"," {
                        pend += 1;
                    }
                    let tok = &val[ppos .. pend];
                    if !tok.is_empty() {
                        params.push(tok.to_string());
                    }
                    if pend >= plen {
                        break;
                    }
                    ppos = pend + 1;
                }
            } else if *key == *"level" {
                let lv = parse_level_str(&val);
                match lv {
                    None => { return Err(ParseFail { tok: ArgTok::BadLevel, detail: val.to_string() }); }
                    Some(p) => {
                        level_code = p.0;
                        level_val = p.1;
                        has_level = true;
                    }
                }
            } else if *key == *"lat" {
                let v = parse_f64(&val);
                match v {
                    None => { return Err(ParseFail { tok: ArgTok::BadLat, detail: val.to_string() }); }
                    Some(x) => {
                        if x < -90.0 || x > 90.0 {
                            return Err(ParseFail { tok: ArgTok::BadLat, detail: val.to_string() });
                        }
                        lat = x;
                        has_lat = true;
                    }
                }
            } else if *key == *"lon" {
                let v = parse_f64(&val);
                match v {
                    None => { return Err(ParseFail { tok: ArgTok::BadLon, detail: val.to_string() }); }
                    Some(x) => {
                        if x < -180.0 || x > 360.0 {
                            return Err(ParseFail { tok: ArgTok::BadLon, detail: val.to_string() });
                        }
                        lon = x;
                        has_lon = true;
                    }
                }
            } else if *key == *"valid" {
                let v = parse_iso_z(&val);
                match v {
                    None => { return Err(ParseFail { tok: ArgTok::BadTime, detail: val.to_string() }); }
                    Some(x) => {
                        valid_ep = x;
                        has_valid = true;
                    }
                }
            } else if *key == *"run" {
                let v = parse_iso_z(&val);
                match v {
                    None => { return Err(ParseFail { tok: ArgTok::BadTime, detail: val.to_string() }); }
                    Some(x) => { pin = Some(x); }
                }
            } else if *key == *"time_interp" {
                if *val == *"linear" {
                    mode = TimeInterpMode::Linear;
                } else if *val == *"none" {
                    mode = TimeInterpMode::None;
                } else {
                    return Err(ParseFail { tok: ArgTok::BadTimeInterp, detail: val.to_string() });
                }
            } else {
                return Err(ParseFail { tok: ArgTok::UnknownKey, detail: key.to_string() });
            }
        }
        pos = end + 1;
    }

    if model.is_empty() {
        return Err(ParseFail { tok: ArgTok::MissingRequired, detail: "model".to_string() });
    }
    if params.is_empty() {
        return Err(ParseFail { tok: ArgTok::MissingRequired, detail: "param".to_string() });
    }
    if !has_level {
        return Err(ParseFail { tok: ArgTok::MissingRequired, detail: "level".to_string() });
    }
    if !has_lat {
        return Err(ParseFail { tok: ArgTok::MissingRequired, detail: "lat".to_string() });
    }
    if !has_lon {
        return Err(ParseFail { tok: ArgTok::MissingRequired, detail: "lon".to_string() });
    }
    if !has_valid {
        return Err(ParseFail { tok: ArgTok::MissingRequired, detail: "valid".to_string() });
    }
    return Ok(ParsedArgs {
        model: model,
        params: params,
        level_code: level_code,
        level_val: level_val,
        lat: lat,
        lon: lon,
        valid_epoch: valid_ep,
        pin_epoch: pin,
        mode: mode,
    });
}

// ---------------------------------------------------------------------------
// selection + evaluation for one parameter (spec §6.3 run selection: latest
// run covering the valid time; per-run fh recomputed from that run's epoch)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum FailTok {
    UnknownModel, // 404
    UnknownParam, // 404: no indexed field (param_key, level type) in any run
    NoRunCovers, // 404: valid time outside every run's stored forecast hours
    LevelOutOfRange, // 422: requested level outside stored range
    StatTimeGuard, // 422: template 4.8 field, exact fh required
    TimeExactOnly, // 422: time_interp=none and no exact fh
    OutOfDomain, // 422: lat/lon outside grid node rectangle
}

pub fn fail_http_status(t: FailTok) -> u16 {
    match t {
        FailTok::UnknownModel => { return 404; }
        FailTok::UnknownParam => { return 404; }
        FailTok::NoRunCovers => { return 404; }
        FailTok::LevelOutOfRange => { return 422; }
        FailTok::StatTimeGuard => { return 422; }
        FailTok::TimeExactOnly => { return 422; }
        FailTok::OutOfDomain => { return 422; }
    }
}

pub fn fail_token(t: FailTok) -> String {
    match t {
        FailTok::UnknownModel => { return "unknown_model".to_string(); }
        FailTok::UnknownParam => { return "unknown_param".to_string(); }
        FailTok::NoRunCovers => { return "no_run_covers".to_string(); }
        FailTok::LevelOutOfRange => { return "level_out_of_range".to_string(); }
        FailTok::StatTimeGuard => { return "stat_time_guard".to_string(); }
        FailTok::TimeExactOnly => { return "time_exact_only".to_string(); }
        FailTok::OutOfDomain => { return "out_of_domain".to_string(); }
    }
}

/// One parameter's answer entry (masked data is NOT an error: value null +
/// reason missing_data per spec §7).
#[derive(Clone, Debug)]
pub struct QueryEntry {
    pub param: String,
    pub units: String,
    pub run_epoch: i64, // selected run (echoed at top level)
    pub masked: bool,
    pub value: f64,
    pub info: Option<InterpInfo>,
}

/// Seed context whose fields build() overwrites entirely (milestone-3
/// contract takes a seed only as a type template).
fn make_seed(grid: Arc<GridDef>) -> PointQueryContext {
    return PointQueryContext {
        grid: grid,
        units: "".to_string(),
        run_epoch_secs: 0,
        stat_processed: false,
        want_level: 0.0,
        want_fh_secs: 0.0,
        fh_secs: (0.0, 0.0),
        slots: [None, None],
    };
}

pub fn answer_param(snap: &IndexSnapshot, model: &str, pin: Option<i64>, key: &str, code: u8, lval: f64, valid_ep: i64, mode: TimeInterpMode, lat: f64, lon: f64)
    -> Result<QueryEntry, FailTok> {
    let gm = snap.find_model(model);
    match gm {
        None => { return Err(FailTok::UnknownModel); }
        Some(m) => {
            // does ANY run carry this (param, level type)? else unknown_param.
            let mut has_field = false;
            for r in &m.runs {
                for fe in &r.fields {
                    if fe.param_key == key && fe.level_code == code {
                        has_field = true;
                        break;
                    }
                }
            }
            if !has_field {
                return Err(FailTok::UnknownParam);
            }
            // newest -> oldest; recompute fh per candidate run.
            let mut acc_level = false;
            let mut acc_stat = false;
            let mut acc_exact = false;
            let mut ri = m.runs.len();
            while ri > 0 {
                ri -= 1;
                let run = &m.runs[ri];
                let ep = run.epoch_secs;
                match pin {
                    None => {}
                    Some(pe) => {
                        if ep != pe {
                            continue; // pinned run: only that epoch is a candidate
                        }
                    }
                }
                // Offset from the run's reference time IN SECONDS (exact for
                // whole seconds, incl. fractional forecast hours). The API's
                // `valid` granularity is already per-second (timeparse), so no
                // precision is lost anywhere on this path.
                let fh_secs = (valid_ep - ep) as f64;
                if fh_secs < -1e-9 {
                    continue; // valid time before this run starts: older runs are worse
                }
                let seed = make_seed(run.grid.clone());
                let cb = seed.build(snap, model, Some(ep), key, code, lval, fh_secs, mode);
                match cb {
                    Ok(ctx) => {
                        let ev = evaluate_point(&ctx, lat, lon);
                        match ev {
                            Ok(pv) => {
                                return Ok(QueryEntry {
                                    param: key.to_string(),
                                    units: ctx.units,
                                    run_epoch: ctx.run_epoch_secs,
                                    masked: false,
                                    value: pv.value,
                                    info: Some(pv.info),
                                });
                            }
                            Err(pe) => {
                                match pe {
                                    PointError::MissingData => {
                                        return Ok(QueryEntry {
                                            param: key.to_string(),
                                            units: ctx.units,
                                            run_epoch: ctx.run_epoch_secs,
                                            masked: true,
                                            value: 0.0,
                                            info: None,
                                        });
                                    }
                                    PointError::OutOfDomain => {
                                        return Err(FailTok::OutOfDomain);
                                    }
                                    _ => {
                                        return Err(FailTok::NoRunCovers);
                                    }
                                }
                            }
                        }
                    }
                    Err(se) => {
                        match se {
                            SelectError::LevelOutOfRange => { acc_level = true; }
                            SelectError::StatTimeGuard => { acc_stat = true; }
                            SelectError::TimeExactOnly => { acc_exact = true; }
                            _ => {}
                        }
                        continue;
                    }
                }
            }
            if acc_level {
                return Err(FailTok::LevelOutOfRange);
            }
            if acc_stat {
                return Err(FailTok::StatTimeGuard);
            }
            if acc_exact {
                return Err(FailTok::TimeExactOnly);
            }
            return Err(FailTok::NoRunCovers);
        }
    }
}

// ---------------------------------------------------------------------------
// response rendering (allocation allowed: this IS the response body)
// ---------------------------------------------------------------------------

pub fn render_error_json(token: String, message: String) -> String {
    return format!("{{\"error\":\"{0}\",\"message\":\"{1}\"}}", token, message);
}

/// Entry-shaped result object for one parameter.
fn render_entry(e: QueryEntry) -> String {
    if e.masked {
        return format!("{{\"param\":\"{0}\",\"value\":null,\"reason\":\"missing_data\",\"units\":\"{1}\",\"interpolation\":null}}", e.param, e.units);
    }
    let info = e.info;
    let interp: String = match info {
        None => { "null".to_string() }
        Some(ii) => {
            let horiz = match ii.horizontal {
                HorizMethod::Exact => { "exact".to_string() }
                HorizMethod::Bilinear => { "bilinear".to_string() }
            };
            let mut obj = format!("{{\"horizontal\":\"{0}\"", horiz);
            match ii.vertical {
                None => { obj = format!("{0},\"vertical\":null", obj); }
                Some(vv) => {
                    let meth = match vv.method {
                        LevelInterpMethod::Linear => { "linear".to_string() }
                    };
                    let lv0 = fmt_num(vv.levels.0);
                    let lv1 = fmt_num(vv.levels.1);
                    obj = format!("{0},\"vertical\":{{\"method\":\"{1}\",\"levels\":[{2},{3}]}}", obj, meth, lv0, lv1);
                }
            }
            match ii.time {
                None => { obj = format!("{0},\"time\":null", obj); }
                Some(tt) => {
                    let meth = match tt.method {
                        TimeInterpMode::Linear => { "linear".to_string() }
                        TimeInterpMode::None => { "none".to_string() }
                    };
                    let f0 = fmt_num(tt.forecast_hours.0);
                    let f1 = fmt_num(tt.forecast_hours.1);
                    obj = format!("{0},\"time\":{{\"method\":\"{1}\",\"forecast_hours\":[{2},{3}]}}", obj, meth, f0, f1);
                }
            }
            obj = format!("{0}}}", obj);
            obj
        }
    };
    return format!("{{\"param\":\"{0}\",\"value\":{1},\"units\":\"{2}\",\"interpolation\":{3}}}", e.param, e.value, e.units, interp);
}

/// Top-level /v1/point success body, spec §7 example shape (values in native
/// GRIB units; `run` echoes the selected run, `level` the canonical level).
pub fn render_point_json(model: &str, run_epoch: i64, valid_epoch: i64, lat: f64, lon: f64, code: u8, val: f64, entries: &[QueryEntry]) -> String {
    use crate::timeparse::format_iso_z;
    let run_s = format_iso_z(run_epoch);
    let valid_s = format_iso_z(valid_epoch);
    let lvl_s = format_level_str(code, val);
    let mut out = format!("{{\"model\":\"{0}\",\"run\":\"{1}\",\"valid\":\"{2}\",\"lat\":{3},\"lon\":{4},\"level\":\"{5}\",\"results\":[", model, run_s, valid_s, lat, lon, lvl_s);
    for k in 0 .. entries.len() {
        if k > 0 {
            out = format!("{0}{1}", out, ",");
        }
        out = format!("{0}{1}", out, render_entry(entries[k].clone()));
    }
    out = format!("{0}{1}", out, "]}");
    return out;
}
