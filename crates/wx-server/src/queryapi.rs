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

use std::borrow::Cow;
use crate::timeparse::{parse_iso_z};
use wx_ingest::index::{IndexSnapshot, build_context, evaluate_point, SelectError};
use wx_core::interp::{TimeInterpMode, HorizMethod, PointError, InterpInfo, LevelInterpMethod};
use wx_core::tables::{parse_level_str, format_level_str, parse_f64, fmt_num};

// ---------------------------------------------------------------------------
// V1.4 phase A UNIFORM PERCENT-DECODING (supersedes the V1.3 E6 targeted rule
// documented in docs/DECISIONS.md §19.3, itself a follow-up to §18.6's raw-
// query observation). AUDIT_1.3 P1: E6 decoded %3A/%2C ONLY inside param/
// level VALUES, so valid=2026-10-19T15%3A00%3A00Z answered 400 bad_time while
// bare colons answered 200 — standard clients (python requests and friends)
// percent-encode ':' in EVERY value and were locked out of the API. Now ONE
// single-pass percent-decode runs in the PARSE LAYER over EVERY query value
// before the key/value walk. The contract stays raw-first — the canonical
// wire form remains BARE colons/commas (curl style) — but any complete
// escape is now understood, uniformly:
//   %XY (X,Y ASCII hex, either case) -> that byte (%3A -> ':' , %2c -> ',' ,
//                                        %20 -> ' ', %E2%82%AC -> U+20AC)
//   %%               -> literal '%'  (so the wire text "%%3A" decodes ONCE
//                                      into the literal TEXT "%3A", never ':')
//   '+'              -> literal '+' ALWAYS. Deliberate (deviation note): the
//                      form-encoding "+ = space" rule is HTML-form specific,
//                      RFC 3986 defines no such thing, applying it would make
//                      a literal '+' unrepresentable without %2B, and it
//                      would silently mutate values like TM+P that resolve as
//                      unknown_param today (pinned by point_http.rs /
//                      client_compat.rs). Clients wanting a space send %20.
//   any other '%'    -> typed 400 token bad_percent_escape (%zz, trailing %,
//                      %3z): every '%' MUST open a complete escape. Guessing
//                      "leave it literal" was the pre-A behavior and pushed
//                      client encoding bugs behind confusing 404-class answers.
//   escapes that do not reassemble into valid UTF-8 (a lone %FF) -> same 400.
// NO DOUBLE DECODING: exactly one left-to-right pass per value; the pass
// never re-examines its own output, so wire %253A yields the literal text
// "%3A" (NOT ':'), and the old E6 special-case decoder INSIDE param/level
// handling is REMOVED (decode_targeted/is_target_escape/has_target deleted) —
// there is exactly one decode path. Because decoding happens BEFORE
// parse_query, %2C splits param tokens EXACTLY like a bare comma and %3A
// feeds parse_level_str / parse_iso_z exactly like real colons: encoded and
// bare requests produce BYTE-IDENTICAL responses (pinned end-to-end).
// KEYS stay byte-literal: the vocabulary (model,param,level,lat,lon,valid,
// run,time_interp) is fixed ASCII and clients never need to encode it.
// Splitting on '&' and the FIRST '=' happens on RAW bytes, so a wire %26/%3D
// inside a value cannot fake a separator; the documented consequence is that
// a DECODED '&' re-enters the separator vocabulary when parse_query walks the
// REWRITTEN string (model=a%26b -> 400 unknown_query_param "b"), which can
// only ever reject, never answer wrong: no configured model/param name
// contains '&'. A decoded '=' always stays inside its value (first-'=' split).
// THE STRICT ISO VALIDATOR IS NOT LOOSENED (timeparse.rs untouched): decoded
// valid/run values still must match the exact 20-char YYYY-MM-DDTHH:MM:SSZ
// form with the calendar table, pinned both ways by client_compat.rs (a
// %3A-encoded real timestamp answers 200; a decoded Feb-31 still 400 bad_time).
// HOT-PATH RULE (§5): when the query contains no '%' AT ALL — every curl-
// style request — the original borrow is returned and NOTHING allocates; an
// owned rewrite happens only when the query actually carries escapes. The
// rewrite lives at the httpapi boundary rather than as ParsedArgs storage
// surgery because Cow-ing the whole borrow chain would ripple through
// FailInfo/'r lifetimes on the zero-alloc hot path for a cold-path nicety.
// ---------------------------------------------------------------------------

/// ASCII hex digit value, None otherwise.
fn hex_val(b: u8) -> Option<u8> {
    if b >= b'0' && b <= b'9' {
        return Some(b - b'0');
    }
    if b >= b'a' && b <= b'f' {
        return Some(b - b'a' + 10);
    }
    if b >= b'A' && b <= b'F' {
        return Some(b - b'A' + 10);
    }
    return None;
}

/// Decode ONE query value in a SINGLE left-to-right pass (rules in the header
/// block above). Borrowed when the value has no '%'. Err(()) signals a
/// malformed escape ('%' not doubling itself and not followed by two hex
/// digits, or escapes that do not reassemble into UTF-8); the caller turns
/// that into the typed 400 bad_percent_escape.
fn decode_value_once<'v>(val: &'v str) -> Result<Cow<'v, str>, ()> {
    if val.find('%').is_none() {
        return Ok(Cow::Borrowed(val));
    }
    let b = val.as_bytes();
    let mut out: Vec<u8> = Vec::<u8>::with_capacity(val.len());
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c != b'%' {
            out.push(c);
            i += 1;
            continue;
        }
        // Every '%' must open a COMPLETE escape: %% or %XY. Anything else is
        // a client encoding bug -> typed 400, never a silent literal.
        if i + 1 >= b.len() {
            return Err(()); // trailing '%'
        }
        if b[i + 1] == b'%' {
            out.push(b'%');
            i += 2;
            continue;
        }
        if i + 2 >= b.len() {
            return Err(()); // "%X" at end of value
        }
        let h1_r = hex_val(b[i + 1]);
        let h2_r = hex_val(b[i + 2]);
        match (h1_r, h2_r) {
            (Some(h1), Some(h2)) => {
                out.push(h1 * 16 + h2);
                i += 3;
            }
            _ => { return Err(()); } // %zz / %3z / %2G ...
        }
    }
    // Escapes may recombine arbitrary bytes; a sequence that is not valid
    // UTF-8 (a lone %FF splitting the &str contract) gets the same typed 400.
    match String::from_utf8(out) {
        Ok(s) => { return Ok(Cow::Owned(s)); }
        Err(_e) => { return Err(()); }
    }
}

/// Rewrite the ENTIRE query string applying the uniform single-pass decode to
/// EVERY `key=value` VALUE (keys and separators byte-literal; keyless
/// segments ride along verbatim and still reach parse_query's unknown-key
/// rejection). Fast path: no '%' anywhere -> the original borrow, ZERO
/// allocations (hot-path rule §5). Err(ParseFail { BadEscape, detail }) names
/// the offending VALUE text of the first malformed escape, mirroring how
/// bad_time/bad_level details echo the offending text.
pub fn normalize_qs<'q>(qs: &'q str) -> Result<Cow<'q, str>, ParseFail> {
    if qs.find('%').is_none() {
        return Ok(Cow::Borrowed(qs));
    }
    let mut out = String::with_capacity(qs.len());
    let mut first_seg = true;
    for seg in qs.split('&') {
        if !first_seg {
            out.push('&');
        }
        first_seg = false;
        let eq_r = seg.find('=');
        match eq_r {
            None => { out.push_str(seg); }
            Some(eq) => {
                let key = &seg[0 .. eq];
                let val = &seg[eq + 1 .. seg.len()];
                match decode_value_once(val) {
                    Err(_) => { return Err(ParseFail { tok: ArgTok::BadEscape, detail: val.to_string() }); }
                    Ok(dv) => {
                        out.push_str(key);
                        out.push('=');
                        out.push_str(dv.as_ref());
                    }
                }
            }
        }
    }
    return Ok(Cow::Owned(out));
}

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
    /// V1.4 phase A: a '%' inside a query value that opens NO complete escape
    /// (%zz, trailing %, %3z) or whose escapes do not reassemble into UTF-8 —
    /// see the uniform-decoder header block. Wire-level defect, reported
    /// BEFORE any domain lookup, token `bad_percent_escape`.
    BadEscape,
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
        // V1.4 phase A token name chosen per the mission ("pick a name,
        // document it"): bad_percent_escape pairs with the existing bad_*
        // family and names the DEFECT, not the key, because the defect lives
        // in the VALUE's escaping (header block above documents the rule).
        ArgTok::BadEscape => { return "bad_percent_escape".to_string(); }
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
    pub run_epoch: i64, // the run THIS result came from (echoed per result;
                        // top level only when all results agree)
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
//
// JSON-SAFE ECHOES (V1.4 phase A, AUDIT_1.3 P2 cluster): EVERY body this
// server serves is HAND-BUILT with format!/push_str — NO endpoint serializes
// through serde_json (the dependency exists only for tests/benches), so any
// external string landing inside a quoted JSON slot must be escaped or one
// stray quote/backslash/control char corrupts the whole body. Reproduced
// pre-fix: GET /v1/point?model=a\qb&... answered a 404 whose body was
// INVALID JSON (the raw backslash became a dangling escape). External-text
// sources: request query echoes (unknown_model/unknown_param messages and
// the "model" field of point responses; parse-fail details carry raw value
// text) and CONFIG-DERIVED strings (model names, param keys, units — units
// are config-overridable since E2 — surfaced by /v1/models, /v1/inventory
// and entries). Canonical internal renderings (format_iso_z timestamps,
// fmt_num numbers, level_dir_name / format_level_str grammar, dir_name
// YYYYMMDDHH) need no escaping and json_escape_cow returns them BORROWED
// (zero cost). Escape at these choke points once:
//   * render_error_json      -> token AND message (covers every error path,
//                               incl. arg-parse details and the /ready list)
//   * render_point_json      -> "model" echo
//   * render_entry           -> per-result "param" + "units"
//   * httpapi models_handler -> model name, param key, units
//   * httpapi inventory      -> model name, param key, units (dir_name +
//                               stat_kind defensive, see comments there)

/// Append `s` to `out` with JSON STRING-BODY escaping (RFC 8259 §7): '"' and
/// '\' get backslash escapes; C0 control chars become \b \t \n \f \r or a
/// generic \u00XX; everything else (including multi-byte UTF-8) rides along
/// byte-for-byte. Does NOT emit the surrounding quotes — callers keep their
/// literal \" bytes. (&mut String target rather than &mut impl Write: every
/// echo site already builds into a String, keeping the escaper monomorphic
/// and infallible.)
pub fn json_escape_into(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '"' => { out.push_str("\\\""); }
            '\\' => { out.push_str("\\\\"); }
            '\n' => { out.push_str("\\n"); }
            '\r' => { out.push_str("\\r"); }
            '\t' => { out.push_str("\\t"); }
            '\u{8}' => { out.push_str("\\b"); }
            '\u{c}' => { out.push_str("\\f"); }
            _ => {
                if (ch as u32) < 0x20 {
                    out.push_str(&format!("\\u00{0:02x}", ch as u32));
                } else {
                    out.push(ch);
                }
            }
        }
    }
}

/// Cow convenience OVER [`json_escape_into`] (still ONE escaper): Borrowed
/// when nothing needs escaping — every canonical name today — owned escaped
/// text otherwise, so format! sites stay readable.
pub fn json_escape_cow<'a>(s: &'a str) -> Cow<'a, str> {
    let mut need = false;
    for ch in s.chars() {
        let u = ch as u32;
        if u < 0x20 || u == 0x22 || u == 0x5C {
            need = true;
            break;
        }
    }
    if !need {
        return Cow::Borrowed(s);
    }
    let mut t = String::with_capacity(s.len() + 8);
    json_escape_into(&mut t, s);
    return Cow::Owned(t);
}

/// The single error-body choke point (`{"error":T,"message":M}` per spec §7).
/// V1.4 phase A: token AND message are escaped HERE, so every caller's echo
/// of request/config text (model names, param keys, raw parse-fail values,
/// /ready's missing-model list) is JSON-safe without touching call sites.
pub fn render_error_json(token: String, message: String) -> String {
    let mut out = String::with_capacity(24 + token.len() + message.len());
    out.push_str("{\"error\":\"");
    json_escape_into(&mut out, &token);
    out.push_str("\",\"message\":\"");
    json_escape_into(&mut out, &message);
    out.push_str("\"}");
    return out;
}

/// Entry-shaped result object for one parameter.
fn render_entry(e: QueryEntry) -> String {
    // V1.3 phase E2 PER-RESULT PROVENANCE: every result object carries the
    // ISO-8601 Z timestamp of the run THAT RESULT came from. With several
    // indexed runs a multi-param request can legitimately mix provenance
    // (newest run covering only TMP, older run supplying HGT); before E2 the
    // single top-level echo was silently wrong for the other params.
    use crate::timeparse::format_iso_z;
    let run_s = format_iso_z(e.run_epoch);
    // V1.4 phase A JSON-safe echoes: "param" here is REQUEST TEXT and units
    // are config-overridable since E2 (raw-triple params carry their config
    // `units` string verbatim).
    let p_e = json_escape_cow(e.param);
    let u_e = json_escape_cow(e.units);
    if e.masked {
        return format!("{{\"param\":\"{0}\",\"value\":null,\"reason\":\"missing_data\",\"units\":\"{1}\",\"interpolation\":null,\"run\":\"{2}\"}}", p_e, u_e, run_s);
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
    return format!("{{\"param\":\"{0}\",\"value\":{1},\"units\":\"{2}\",\"interpolation\":{3},\"run\":\"{4}\"}}", p_e, e.value, u_e, interp, run_s);
}

/// Top-level /v1/point success body, spec §7 example shape (values in native
/// GRIB units; `level` the canonical level). V1.1 phase 5: takes the
/// PointEntries buffer instead of a Vec.
/// V1.3 phase E2: EVERY result object additionally carries its own `"run"`
/// (per-result provenance). The TOP-LEVEL `"run"` is echoed when all results
/// share one run (back compatible) and emitted as JSON `null` when they
/// differ — the provenance is then fully visible per result and nothing is
/// ever echoed that did not answer (defect: entry 0's run used to stand in
/// for the whole response).
pub fn render_point_json(model: &str, valid_epoch: i64, lat: f64, lon: f64, code: u8, val: f64, eb: &PointEntries) -> String {
    use crate::timeparse::format_iso_z;
    let run_tok: String = if eb.n == 0 {
        "null".to_string()
    } else {
        let first = entries_at(eb, 0).run_epoch;
        let mut same = true;
        for k in 1 .. eb.n {
            if entries_at(eb, k).run_epoch != first {
                same = false;
                break;
            }
        }
        if same {
            format!("\"{0}\"", format_iso_z(first))
        } else {
            "null".to_string()
        }
    };
    let valid_s = format_iso_z(valid_epoch);
    let lvl_s = format_level_str(code, val); // canonical grammar: needs no escaping
    // V1.4 phase A: "model" here is REQUEST TEXT (echoed verbatim per spec
    // §7), escaped like every other external string. lat/lon/run/valid/level
    // are numbers or canonical renderings — safe untouched.
    let m_e = json_escape_cow(model);
    let mut out = format!("{{\"model\":\"{0}\",\"run\":{1},\"valid\":\"{2}\",\"lat\":{3},\"lon\":{4},\"level\":\"{5}\",\"results\":[", m_e, run_tok, valid_s, lat, lon, lvl_s);
    for k in 0 .. eb.n {
        if k > 0 {
            out = format!("{0}{1}", out, ",");
        }
        out = format!("{0}{1}", out, render_entry(entries_at(eb, k)));
    }
    out = format!("{0}{1}", out, "]}");
    return out;
}