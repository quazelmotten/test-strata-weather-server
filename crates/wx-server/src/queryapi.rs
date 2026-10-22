//! /v1/point request parsing, run selection over the arc-swap snapshot and
//! response JSON rendering (spec §7 + §6 semantics via the milestone-3 index
//! contract). HOT PATH RULES (spec §5, V1.1 phase 5): the ENTIRE lookup path
//! (query-string parse -> snapshot walk -> PointQueryContext assembly ->
//! evaluate_point -> per-parameter entry structuring) performs ZERO heap
//! allocations, no locks beyond the arc-swap load, no file I/O beyond mmap
//! page faults and no per-request memmap open. ParsedArgs borrows its strings
//! from the request query string; QueryEntry borrows units/param names from
//! the snapshot and the request respectively; params/entries live in fixed
//! inline arrays (cap 8 each, docs/DECISIONS.md) with a documented heap
//! fallback ONLY for requests exceeding the cap. Proven by tests/hotpath_alloc.rs
//! (evaluate_point AND build_context zero-alloc) and tests/hotpath_full_alloc.rs
//! (full handler path up to, EXCLUDING, JSON body string construction).
//! Response body construction (render_point_json/render_entry/make_response)
//! allocates freely, which the spec allows and the measurement boundary excludes.

use crate::timeparse::{parse_iso_z};
use wx_ingest::index::{IndexSnapshot, build_context, evaluate_point, SelectError};
use wx_core::interp::{TimeInterpMode, HorizMethod, PointError, InterpInfo, LevelInterpMethod};
use wx_core::tables::{parse_level_str, format_level_str, parse_f64, fmt_num};

/// Inline capacity for parsed parameter tokens AND per-request result entries
/// (V1.1 phase 5). 8 covers every realistic multi-param query (spec §7 shows
/// `TMP,HGT`; operational mixes stay well under 8); requests beyond the cap
/// fall back to a heap Vec ONCE per request and are OUT OF THE ZERO-ALLOC
/// GUARANTEE (documented in docs/DECISIONS.md).
pub const MAX_INLINE_PARAMS: usize = 8;
pub const MAX_INLINE_ENTRIES: usize = 8;

/// One parsed /v1/point request (spec §7 parameter table). V1.1 phase 5:
/// BORROWS every string from the query string it parsed (lifetime 'q), so
/// parsing allocates nothing on the success path.
#[derive(Clone, Debug)]
pub struct ParsedArgs<'q> {
    pub model: &'q str, // "" when absent (required-key check below)
    pub n_params: usize, // total csv tokens (inline + extra)
    pub params: [Option<&'q str>; MAX_INLINE_PARAMS], // first N tokens, order preserved
    pub extra_params: Vec<&'q str>, // tokens beyond the inline cap (rare; allocates)
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
    pub detail: String, // offending key/value text (error path only: allocates)
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
// query-string parsing (manual walk: keys AND values borrowed as slices of
// qs; the success path touches the allocator exactly zero times — error
// paths may allocate their message text, which is response-body territory)
// ---------------------------------------------------------------------------

pub fn parse_query<'q>(qs: &'q str) -> Result<ParsedArgs<'q>, ParseFail> {
    let mut model: &'q str = "";
    let mut pa_params: [Option<&'q str>; MAX_INLINE_PARAMS] = [None, None, None, None, None, None, None, None];
    let mut pa_extra: Vec<&'q str> = Vec::<&'q str>::new();
    let mut pa_n = 0;
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
            let val = if eq < seg.len() { &seg[eq + 1 .. seg.len()] } else { &seg[0 .. 0] };
            if *key == *"model" {
                if val.is_empty() {
                    return Err(ParseFail { tok: ArgTok::EmptyValue, detail: "model".to_string() });
                }
                model = val;
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
                        if pa_n < MAX_INLINE_PARAMS {
                            pa_params[pa_n] = Some(tok);
                        } else {
                            pa_extra.push(tok);
                        }
                        pa_n += 1;
                    }
                    if pend >= plen {
                        break;
                    }
                    ppos = pend + 1;
                }
            } else if *key == *"level" {
                let lv = parse_level_str(val);
                match lv {
                    None => { return Err(ParseFail { tok: ArgTok::BadLevel, detail: val.to_string() }); }
                    Some(p) => {
                        level_code = p.0;
                        level_val = p.1;
                        has_level = true;
                    }
                }
            } else if *key == *"lat" {
                let v = parse_f64(val);
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
                let v = parse_f64(val);
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
                let v = parse_iso_z(val);
                match v {
                    None => { return Err(ParseFail { tok: ArgTok::BadTime, detail: val.to_string() }); }
                    Some(x) => {
                        valid_ep = x;
                        has_valid = true;
                    }
                }
            } else if *key == *"run" {
                let v = parse_iso_z(val);
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
    if pa_n == 0 {
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
        n_params: pa_n,
        params: pa_params,
        extra_params: pa_extra,
        level_code: level_code,
        level_val: level_val,
        lat: lat,
        lon: lon,
        valid_epoch: valid_ep,
        pin_epoch: pin,
        mode: mode,
    });
}

/// k-th parameter token in request order (inline first, heap tail after).
pub fn param_at<'q>(pa: &ParsedArgs<'q>, k: usize) -> &'q str {
    if k >= pa.n_params {
        return "";
    }
    if k < MAX_INLINE_PARAMS {
        match pa.params[k] {
            Some(s) => { return s; }
            None => { return ""; }
        }
    }
    return pa.extra_params[k - MAX_INLINE_PARAMS];
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
    /// 422: template 4.8 data forbids linear time interpolation. V1.3 phase
    /// E1 payload (SECONDS past the run time): `(fh_a, fh_b, offender, kind
    /// code)` — the refused bracket (−1 when nothing brackets at all), which
    /// forecast hour triggered the guard, and its STAT_* kind code. The
    /// message names them so the caller sees WHICH hour forbids blending.
    StatTimeGuard(i64, i64, i64, u8),
    TimeExactOnly, // 422: time_interp=none and no exact fh
    OutOfDomain, // 422: lat/lon outside grid node rectangle
}

pub fn fail_http_status(t: FailTok) -> u16 {
    match t {
        FailTok::UnknownModel => { return 404; }
        FailTok::UnknownParam => { return 404; }
        FailTok::NoRunCovers => { return 404; }
        FailTok::LevelOutOfRange => { return 422; }
        FailTok::StatTimeGuard(..) => { return 422; }
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
        FailTok::StatTimeGuard(..) => { return "stat_time_guard".to_string(); }
        FailTok::TimeExactOnly => { return "time_exact_only".to_string(); }
        FailTok::OutOfDomain => { return "out_of_domain".to_string(); }
    }
}

/// One parameter's answer entry (masked data is NOT an error: value null +
/// reason missing_data per spec §7). V1.1 phase 5: fully borrowed + Copy —
/// constructing it allocates nothing (param name comes from the request,
/// units from the snapshot).
#[derive(Clone, Copy, Debug)]
pub struct QueryEntry<'r> {
    pub param: &'r str,
    pub units: &'r str,
    pub run_epoch: i64, // selected run (echoed at top level)
    pub masked: bool,
    pub value: f64,
    pub info: Option<InterpInfo>,
}

pub fn answer_param<'r>(snap: &'r IndexSnapshot, model: &'r str, pin: Option<i64>, key: &'r str, code: u8, lval: f64, valid_ep: i64, mode: TimeInterpMode, lat: f64, lon: f64)
    -> Result<QueryEntry<'r>, FailTok> {
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
            // V1.3 E1: first guard occurrence carries the naming detail
            // (bracket + offending forecast hour + its kind code).
            let mut g_a = -1i64;
            let mut g_b = -1i64;
            let mut g_off = -1i64;
            let mut g_kind = 255u8;
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
                let cb = build_context(snap, model, Some(ep), key, code, lval, fh_secs, mode);
                match cb {
                    Ok(ctx) => {
                        let ev = evaluate_point(&ctx, lat, lon);
                        match ev {
                            Ok(pv) => {
                                return Ok(QueryEntry {
                                    param: key,
                                    units: ctx.units,
                                    run_epoch: ctx.run_epoch_secs,
                                    masked: false,
                                    value: pv.value,
                                    info: Some(pv.info),
                                });
                            }
                            Err(pe2) => {
                                match pe2 {
                                    PointError::MissingData => {
                                        return Ok(QueryEntry {
                                            param: key,
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
                            SelectError::StatTimeGuard(a, b, o, kk) => {
                                if !acc_stat {
                                    g_a = a;
                                    g_b = b;
                                    g_off = o;
                                    g_kind = kk;
                                }
                                acc_stat = true;
                            }
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
                return Err(FailTok::StatTimeGuard(g_a, g_b, g_off, g_kind));
            }
            if acc_exact {
                return Err(FailTok::TimeExactOnly);
            }
            return Err(FailTok::NoRunCovers);
        }
    }
}

// ---------------------------------------------------------------------------
// per-request entry buffer + lookup driver (the ZERO-ALLOC measurement window:
// everything up to but EXCLUDING JSON body string construction)
// ---------------------------------------------------------------------------

/// Fixed-inline result buffer for one /v1/point request. Entries are Copy
/// borrows; the inline array covers <= MAX_INLINE_ENTRIES params (heap tail
/// only beyond the cap, documented). `fail_param` records the offending param
/// when lookup_entries fails so the error message can name it without an
/// allocation on the success path.
#[derive(Debug)]
pub struct PointEntries<'r> {
    pub n: usize,
    pub inline: [Option<QueryEntry<'r>>; MAX_INLINE_ENTRIES],
    pub extra: Vec<QueryEntry<'r>>,
}

/// Which parameter failed a lookup (token + borrowed param name).
#[derive(Debug)]
pub struct FailInfo<'q> {
    pub tok: FailTok,
    pub param: &'q str,
}

pub fn new_entries<'r>() -> PointEntries<'r> {
    return PointEntries {
        n: 0,
        inline: [None, None, None, None, None, None, None, None],
        extra: Vec::<QueryEntry<'r>>::new(),
    };
}

pub fn entries_push<'r>(eb: &mut PointEntries<'r>, e: QueryEntry<'r>) {
    if eb.n < MAX_INLINE_ENTRIES {
        eb.inline[eb.n] = Some(e);
    } else {
        eb.extra.push(e);
    }
    eb.n += 1;
}

pub fn entries_at<'r>(eb: &PointEntries<'r>, k: usize) -> QueryEntry<'r> {
    if k < MAX_INLINE_ENTRIES {
        match eb.inline[k] {
            Some(e) => { return e; }
            None => {}
        }
    }
    return eb.extra[k - MAX_INLINE_ENTRIES];
}

/// Resolve EVERY parameter of the request against the shared point/level/time
/// and structure the result entries. This is the hot path proper: zero
/// allocations, zero locks beyond the caller's snapshot load. Decision (spec
/// ambiguous for multi-param errors): the FIRST failing parameter decides the
/// error status; masked points are not errors. On failure the offending
/// parameter name travels back in [`FailInfo`] (borrowed from the request).
pub fn lookup_entries<'q: 'r, 'r>(snap: &'r IndexSnapshot, pa: &ParsedArgs<'q>, eb: &mut PointEntries<'r>) -> Result<(), FailInfo<'q>> {
    let np = pa.n_params;
    let mut k = 0;
    while k < np {
        let key = param_at(pa, k);
        let one = answer_param(snap, pa.model, pa.pin_epoch, key, pa.level_code, pa.level_val, pa.valid_epoch, pa.mode, pa.lat, pa.lon);
        match one {
            Err(ft) => {
                return Err(FailInfo { tok: ft, param: key });
            }
            Ok(entry) => { entries_push(eb, entry); }
        }
        k += 1;
    }
    return Ok(());
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
                    let pair_a = format!("[{0},{1}]", lv0, lv1);
                    // V1.2 phase B0: the two TIME slots can bracket DIFFERENT
                    // level pairs (transient during partial ingest). "levels"
                    // stays the EARLIER slot's pair (spec §7 shape, back-
                    // compatible); when the pairs differ the vertical object
                    // ADDITIVELY carries "levels_per_time" holding BOTH pairs
                    // so the answer is never misleading. Output is byte-identical
                    // to V1.1 whenever the two stacks agree.
                    if vv.per_time {
                        let lb0 = fmt_num(vv.levels_later.0);
                        let lb1 = fmt_num(vv.levels_later.1);
                        let pair_b = format!("[{0},{1}]", lb0, lb1);
                        obj = format!("{0},\"vertical\":{{\"method\":\"{1}\",\"levels\":{2},\"levels_per_time\":[{3},{4}]}}",
                            obj, meth, pair_a, pair_a, pair_b);
                    } else {
                        obj = format!("{0},\"vertical\":{{\"method\":\"{1}\",\"levels\":{2}}}", obj, meth, pair_a);
                    }
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
/// V1.1 phase 5: takes the PointEntries buffer instead of a Vec — byte-identical
/// output to the old renderer (same field order, same fmt_num behavior).
pub fn render_point_json(model: &str, run_epoch: i64, valid_epoch: i64, lat: f64, lon: f64, code: u8, val: f64, eb: &PointEntries) -> String {
    use crate::timeparse::format_iso_z;
    let run_s = format_iso_z(run_epoch);
    let valid_s = format_iso_z(valid_epoch);
    let lvl_s = format_level_str(code, val);
    let mut out = format!("{{\"model\":\"{0}\",\"run\":\"{1}\",\"valid\":\"{2}\",\"lat\":{3},\"lon\":{4},\"level\":\"{5}\",\"results\":[", model, run_s, valid_s, lat, lon, lvl_s);
    for k in 0 .. eb.n {
        if k > 0 {
            out = format!("{0}{1}", out, ",");
        }
        out = format!("{0}{1}", out, render_entry(entries_at(eb, k)));
    }
    out = format!("{0}{1}", out, "]}");
    return out;
}