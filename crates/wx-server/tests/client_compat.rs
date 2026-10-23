// V1.4 phase A CLIENT-COMPAT GATES — the behaviors AUDIT_1.3 flagged P1/P2 on
// the HTTP surface, pinned end-to-end through the real axum Router:
//
//   1. UNIFORM PERCENT-DECODING (P1). Pre-A only param/level got targeted
//      %3A/%2C decoding, so valid=2026-10-19T15%3A00%3A00Z answered 400
//      bad_time while bare colons answered 200 — standard clients (python
//      requests et al) percent-encode ':' in EVERY value. Now ONE single-pass
//      decode covers every value: encoded == bare BYTE-IDENTICAL; malformed
//      escapes (%zz, trailing %) become the typed 400 bad_percent_escape
//      INSTEAD OF the pre-A literal-passthrough 404/400 guesses; '+' stays
//      literal (never a space); %% and %25 yield literal '%' text ONCE (the
//      pass never re-examines its own output: wire %2548 decodes to the
//      literal text "%48", proven via the unknown_param echo); the STRICT ISO
//      calendar validator stays intact THROUGH decoding (a decoded Feb-31 is
//      still 400 bad_time — timeparse.rs untouched).
//
//   2. JSON-SAFE ECHOES (P2 cluster). Every body is hand-built (NO endpoint
//      serializes through serde_json), so request/config text landing inside
//      quoted slots must be escaped. Reproduced pre-A: model=a\qb produced an
//      INVALID-JSON 404 body. Pinned here by parsing every echoed response
//      with serde_json AND asserting the echo round-trips verbatim — incl. a
//      CONFIG model name containing quote+backslash rendered by /v1/models
//      and queryable end-to-end through percent-encoded request values.
//
//   3. PINNED SEMANTICS (behavior existed pre-A; now CONTRACTUAL). A run= pin
//      to an ABSENT or NON-COVERING run answers 404 no_run_covers — NO silent
//      fallback to another indexed run (the 18Z run exists but starts AFTER
//      valid 15Z; pinning it must NOT answer from the 12Z run). For multi-
//      param lists the FIRST FAILING PARAMETER decides the whole-request
//      status (spec §7 ambiguity resolved since milestone 4, documented at
//      queryapi::lookup_entries) and all-good lists keep REQUEST ORDER with
//      duplicates preserved.

use wx_server::httpapi::{ServerState, build_router};
use wx_server::stats::new_stats;
use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use axum::routing::RouterIntoService;
use axum::body::Body;
use http::request::Builder;
use http_body_util::BodyExt;
use tower_service::Service;
use tokio::runtime::Runtime;
use std::sync::Arc;
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

#[allow(clippy::needless_return)]
fn fixture(name: &str) -> String {
    return format!("../../tests/fixtures/{name}");
}

#[allow(clippy::needless_return)]
fn rm_tree(dir: &str) {
    match read_dir(dir) {
        Err(_) => {}
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let p = path_to_str(&de.path());
                        match read_dir(&p) {
                            Err(_) => {
                                let _r = remove_file(&p);
                            }
                            Ok(_) => {
                                rm_tree(&p);
                            }
                        }
                    }
                }
            }
        }
    }
    let _d = remove_dir(dir);
}

#[allow(clippy::needless_return)]
fn copy_file(src: String, dst: String) {
    let f = File::open(&src);
    match f {
        Err(_) => { panic!(); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { panic!(); }
                Ok(region) => {
                    let d = File::create(&dst);
                    match d {
                        Err(_) => { panic!(); }
                        Ok(mut w) => {
                            let n = w.write_all(&region[0 .. region.len()]);
                            if n.is_err() { panic!(); }
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn write_text(path: &str, text: String) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!(); }
        Ok(mut w) => {
            let n = w.write_all(text.as_bytes());
            if n.is_err() { panic!(); }
        }
    }
}

#[allow(clippy::needless_return)]
fn bytes_contains(hay: &[u8], pat: &[u8]) -> bool {
    if pat.len() > hay.len() {
        return false;
    }
    for s in 0 .. (hay.len() - pat.len() + 1) {
        let mut ok = true;
        for k in 0 .. pat.len() {
            if hay[s + k] != pat[k] {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }
    return false;
}

#[allow(clippy::needless_return)]
fn has(body: &[u8], pat: &str) -> bool {
    return bytes_contains(body, pat.as_bytes());
}

// Parse a body as JSON (panic carrying the raw bytes on failure: THAT abort
// IS the JSON-safety assertion — one unescaped quote/backslash/control char
// inside an echoed string makes every probe below die right here).
#[allow(clippy::needless_return)]
fn jparse(b: &[u8]) -> serde_json::Value {
    let v_r = serde_json::from_slice::<serde_json::Value>(b);
    match v_r {
        Err(_e) => { panic!("body is not valid JSON: {0}", String::from_utf8_lossy(b)); }
        Ok(v) => { return v; }
    }
}

#[allow(clippy::needless_return)]
fn jstr(v: &serde_json::Value, key: &str) -> String {
    let g = v.get(key);
    match g {
        None => { panic!("no key {0}", key); }
        Some(x) => {
            match x.as_str() {
                None => { panic!("key {0} is not a string", key); }
                Some(s) => { return s.to_string(); }
            }
        }
    }
}

// TWO runs in ONE ingest dir (gfs_small run 2026-10-19T12Z: TMP/UGRD/VGRD fh
// 0/6/12, node (10N,350E) @1000 hPa = 250 at fh0 / 310 at fh6; prov_new run
// 18Z carrying ONLY TMP@1000 value 300), so run-selection and pin semantics
// are exercised against BOTH an absent run and an indexed-but-non-covering
// one. The model NAME is a parameter because the weird-name probes need the
// same fixture pipeline; mname_toml is the ready TOML literal incl. quotes
// (e.g. "\"gfs\"" or "\"we\\\"ird\\\\x\"").
#[allow(clippy::needless_return)]
fn cfg_text(root: &str, mname_toml: &str, extra_params: &str) -> String {
    let ing = join_path(root, "ingest-gfs");
    let data = join_path(root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = ";
    let t = format!("{0}{1}", t, mname_toml);
    let t = t + "\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"isobaric:*\"]\n";
    return t + extra_params;
}

struct Ctx {
    // held (never read) to keep the pipeline alive beside its router
    #[allow(dead_code)]
    pub p: Pipeline,
    pub svc: RouterIntoService<Body>,
    pub rt: Runtime,
}

// Fresh root, BOTH fixture runs, two scan rounds (stable_window_secs = 0),
// then wire the real router around the indexed pipeline.
#[allow(clippy::needless_return)]
fn serve_setup(root: String, mname_toml: &str, extra_params: &str) -> Option<Ctx> {
    rm_tree(&root);
    let ing = join_path(&root, "ingest-gfs");
    let _c1 = create_dir_all(&ing);
    copy_file(fixture("gfs_small.grib2"), join_path(&ing, "gfs_small.grib2"));
    copy_file(fixture("prov_new_small.grib2"), join_path(&ing, "prov_new_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_text(&cfg_path, cfg_text(&root, mname_toml, extra_params));

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    let _n1 = p.scan_round();
    let n2 = p.scan_round();
    assert!(n2 == 2);
    assert!(p.counters.errors == 0);

    let stats = new_stats();
    let st = ServerState {
        index: p.shared.clone(),
        stats: stats.clone(),
        models_cfg: Arc::new(p.models.clone()),
    };
    let app = build_router(st);
    let svc = app.into_service();
    let rt_r = Runtime::new();
    let rt = match rt_r {
        Err(_e) => { panic!(); }
        Ok(rtx) => { rtx }
    };
    return Some(Ctx { p, svc, rt });
}

async fn fetch(svc: RouterIntoService<Body>, url: String) -> (u16, Vec<u8>) {
    let rb = Builder::new()
        .method("GET")
        .uri(url)
        .body(Body::empty());
    let req = match rb {
        Err(_e) => { return (0, Vec::<u8>::new()); }
        Ok(r) => { r }
    };
    let mut ms = svc;
    let cf = ms.call(req);
    let resp = match cf.await {
        Err(_e) => { return (0, Vec::<u8>::new()); }
        Ok(r) => { r }
    };
    let sc = resp.status();
    let code = sc.as_u16();
    let b = resp.into_body();
    let col = b.collect().await;
    match col {
        Err(_e) => { (code, Vec::<u8>::new())}
        Ok(cc) => {
            let bb = cc.to_bytes();
            (code, bb.to_vec())
        }
    }
}

// ---------------------------------------------------------------------------
// 1a. THE P1 PROBE: %3A-encoded timestamps answer 200 and are BYTE-IDENTICAL
// to the bare-colon form. Pre-A this returned 400 bad_time (valid/run were
// never decoded). Also pins the encoded `run=` pin resolving the SAME run.
// ---------------------------------------------------------------------------

#[test]
fn percent_encoded_timestamp_answers_200_identical_to_bare_form() {
    let root = "/tmp/opencode/wxpoint-ccompat-ts".to_string();
    let ct_r = serve_setup(root.to_string(), "\"gfs\"", "");
    let ct = match ct_r {
        None => { panic!(); }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let svc = ct.svc.clone();

    // mid-cell time interpolation at 15Z (fh 3 between 0 and 6): value 280.
    let bare = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T15:00:00Z";
    let r_b = rt.block_on(fetch(svc.clone(), bare.to_string()));
    assert!(r_b.0 == 200);
    assert!(has(&r_b.1, "\"value\":280"));

    // uppercase %3A: byte-identical body (the uniform decode feeds
    // parse_iso_z REAL colons; nothing else about the request changed).
    let enc = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T15%3A00%3A00Z";
    let r_e = rt.block_on(fetch(svc.clone(), enc.to_string()));
    assert!(r_e.0 == 200 && r_e.1 == r_b.1);
    // and the canonical timestamp echo inside the body carries BARE colons
    // (rendering stays canonical; decoding only happens on the way in).
    let v = jparse(&r_e.1);
    assert!(jstr(&v, "valid") == "2026-10-19T15:00:00Z");

    // lowercase %3a rides the same path.
    let encl = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T15%3a00%3a00Z";
    let r_l = rt.block_on(fetch(svc.clone(), encl.to_string()));
    assert!(r_l.0 == 200 && r_l.1 == r_b.1);

    // run= is decoded too (E6 left it raw -> this answered 400 bad_time
    // pre-A): the encoded pin selects the 12Z run exactly like bare.
    let pin_bare = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T15:00:00Z&run=2026-10-19T12:00:00Z";
    let r_pb = rt.block_on(fetch(svc.clone(), pin_bare.to_string()));
    assert!(r_pb.0 == 200);
    let pin_enc = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T15%3A00%3A00Z&run=2026-10-19T12%3A00%3A00Z";
    let r_pe = rt.block_on(fetch(svc.clone(), pin_enc.to_string()));
    assert!(r_pe.0 == 200 && r_pe.1 == r_pb.1);

    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// 1b. %2C-encoded param lists split EXACTLY like bare commas (decode runs
// before the csv walk), byte-identical bodies, both params present.
// ---------------------------------------------------------------------------

#[test]
fn percent_decoded_param_list_behaves_like_bare_commas() {
    let root = "/tmp/opencode/wxpoint-ccompat-list".to_string();
    let ct_r = serve_setup(root.to_string(), "\"gfs\"", "");
    let ct = match ct_r {
        None => { panic!(); }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let svc = ct.svc.clone();

    let bare = "/v1/point?model=gfs&param=TMP,UGRD&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r_b = rt.block_on(fetch(svc.clone(), bare.to_string()));
    assert!(r_b.0 == 200);
    let vb = jparse(&r_b.1);
    // sanity on the control: TWO results in REQUEST order TMP then UGRD.
    let ab = match vb.get("results") {
        None => { panic!(); }
        Some(serde_json::Value::Array(a)) => { a.clone() }
        Some(_) => { panic!("results not an array"); }
    };
    assert!(ab.len() == 2);
    assert!(jstr(&ab[0], "param") == "TMP");
    assert!(jstr(&ab[1], "param") == "UGRD");

    let enc = "/v1/point?model=gfs&param=TMP%2CUGRD&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r_e = rt.block_on(fetch(svc.clone(), enc.to_string()));
    assert!(r_e.0 == 200 && r_e.1 == r_b.1);

    // lowercase %2c + mixed with an encoded level colon in ONE request.
    let enc2 = "/v1/point?model=gfs&param=TMP%2cUGRD&level=isobaric%3A1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r_e2 = rt.block_on(fetch(svc.clone(), enc2.to_string()));
    assert!(r_e2.0 == 200 && r_e2.1 == r_b.1);

    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// 1c. MALFORMED escapes are the typed 400 bad_percent_escape on EVERY key,
// reported BEFORE domain lookup (so they preempt even unknown-model/param
// and bad_level). Bodies parse as JSON. Pre-A these rode through as literal
// bytes and surfaced as 404-class guesses (see point_http.rs for the pinned
// transition of the E6-era probes).
// ---------------------------------------------------------------------------

#[test]
fn malformed_percent_escapes_answer_typed_400_bad_percent_escape() {
    let root = "/tmp/opencode/wxpoint-ccompat-esc".to_string();
    let ct_r = serve_setup(root.to_string(), "\"gfs\"", "");
    let ct = match ct_r {
        None => { panic!(); }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let svc = ct.svc.clone();

    // %zz in param (pre-A: 404 unknown_param over garbage)
    let e1 = "/v1/point?model=gfs&param=TMP%zz&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r1 = rt.block_on(fetch(svc.clone(), e1.to_string()));
    assert!(r1.0 == 400);
    let v1 = jparse(&r1.1);
    assert!(jstr(&v1, "error") == "bad_percent_escape");

    // trailing % in valid
    let e2 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12%3A00%3A00%";
    let r2 = rt.block_on(fetch(svc.clone(), e2.to_string()));
    assert!(r2.0 == 400);
    let v2 = jparse(&r2.1);
    assert!(jstr(&v2, "error") == "bad_percent_escape");

    // short escape %2 at end of value
    let e3 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=35%2&valid=2026-10-19T12:00:00Z";
    let r3 = rt.block_on(fetch(svc.clone(), e3.to_string()));
    assert!(r3.0 == 400 && has(&r3.1, "bad_percent_escape"));

    // uniform rule reaches OPTIONAL keys too: time_interp=none% -> 400
    let e4 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z&time_interp=none%";
    let r4 = rt.block_on(fetch(svc.clone(), e4.to_string()));
    assert!(r4.0 == 400 && has(&r4.1, "bad_percent_escape"));

    // a bad escape PREEMPTS every later stage: unknown model AND bad_time
    let e5 = "/v1/point?model=no%zzpe&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r5 = rt.block_on(fetch(svc.clone(), e5.to_string()));
    assert!(r5.0 == 400 && has(&r5.1, "bad_percent_escape") && !has(&r5.1, "unknown_model"));

    // non-UTF-8 reassembly gets the same token (a lone %FF cannot split the
    // &str contract; guessing "literal" here would corrupt downstream parses)
    let e6 = "/v1/point?model=gfs&param=TMP%FF&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r6 = rt.block_on(fetch(svc.clone(), e6.to_string()));
    assert!(r6.0 == 400 && has(&r6.1, "bad_percent_escape"));

    // CONTROL proving %25 IS a complete escape: decodes to literal "TMP%" ->
    // unknown_param 404 (well-formed request, genuinely unknown parameter).
    let e7 = "/v1/point?model=gfs&param=TMP%25&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r7 = rt.block_on(fetch(svc.clone(), e7.to_string()));
    assert!(r7.0 == 404);
    let v7 = jparse(&r7.1);
    assert!(jstr(&v7, "error") == "unknown_param");
    assert!(jstr(&v7, "message").find("TMP%").is_some());

    rm_tree(&root);
}

#[allow(clippy::needless_return)]
fn jarr(v: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    let g = v.get(key);
    match g {
        None => { panic!("no key {0}", key); }
        Some(serde_json::Value::Array(a)) => { return a.clone(); }
        Some(_) => { panic!("key {0} is not an array", key); }
    }
}

// ---------------------------------------------------------------------------
// 1d. '+' STAYS LITERAL (documented deviation from form-encoding): TM+P is
// an unknown PARAMETER (not "TM P", not "TMP"), and lat=+10 parses as the
// signed number 10 — if '+' were decoded to a space lat would be bad_lat.
// ---------------------------------------------------------------------------

#[test]
fn plus_never_becomes_a_space() {
    let root = "/tmp/opencode/wxpoint-ccompat-plus".to_string();
    let ct_r = serve_setup(root.to_string(), "\"gfs\"", "");
    let ct = match ct_r {
        None => { panic!(); }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let svc = ct.svc.clone();

    // param name keeps its literal '+': looked up as-is -> unknown_param.
    let p1 = "/v1/point?model=gfs&param=TM+P&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r1 = rt.block_on(fetch(svc.clone(), p1.to_string()));
    assert!(r1.0 == 404);
    let v1 = jparse(&r1.1);
    assert!(jstr(&v1, "error") == "unknown_param");
    // the echoed KEY in the message is the untouched wire text "TM+P"
    // (round-trip through JSON proves neither decoding nor escaping altered it).
    let m1 = jstr(&v1, "message");
    assert!(m1.find("TM+P").is_some());
    assert!(m1.find("TM P").is_none());

    // '+' as a NUMERIC SIGN survives verbatim into parse_f64: byte-identical
    // answers for lat=+10 and lat=10 (both exact grid node, value 250).
    let pl = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=+10&lon=350&valid=2026-10-19T12:00:00Z";
    let pn = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let rl = rt.block_on(fetch(svc.clone(), pl.to_string()));
    let rn = rt.block_on(fetch(svc.clone(), pn.to_string()));
    assert!(rl.0 == 200 && rn.0 == 200 && rl.1 == rn.1);
    assert!(has(&rn.1, "\"value\":250"));

    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// 2a. JSON-SAFE ECHOES on /v1/point error paths. THE repro from AUDIT_1.3:
// model=a\qb answered a 404 whose body was INVALID JSON. Every probe below
// jparse()-es its body (unescaped echo = immediate panic = red test) AND
// asserts the echoed string round-trips back to the ORIGINAL request text.
// ---------------------------------------------------------------------------

#[test]
fn backslash_and_quote_echoes_parse_as_valid_json() {
    let root = "/tmp/opencode/wxpoint-ccompat-bs".to_string();
    let ct_r = serve_setup(root.to_string(), "\"gfs\"", "");
    let ct = match ct_r {
        None => { panic!(); }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let svc = ct.svc.clone();

    // RAW backslash in the query value (the mission's repro): the http crate
    // accepts '\' in the request target, so this reaches the server verbatim.
    let b1 = "/v1/point?model=a\\qb&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r1 = rt.block_on(fetch(svc.clone(), b1.to_string()));
    assert!(r1.0 == 404);
    let v1 = jparse(&r1.1); // pre-A this PANICKED: dangling backslash escape
    assert!(jstr(&v1, "error") == "unknown_model");
    // round-trip: JSON-decoded message contains the ORIGINAL a\qb text.
    let m1 = jstr(&v1, "message");
    assert!(m1.find("model 'a\\qb' is not configured/indexed").is_some());
    // and the WIRE bytes show the backslash doubled (proper JSON escaping).
    assert!(has(&r1.1, "model 'a\\\\qb'"));

    // QUOTE via wire %22 (an unencoded '"' in a URL is itself dodgy; the
    // point here is the ESCAPER): decodes to gf"s, echoes escaped.
    let b2 = "/v1/point?model=gf%22s&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r2 = rt.block_on(fetch(svc.clone(), b2.to_string()));
    assert!(r2.0 == 404);
    let v2 = jparse(&r2.1);
    assert!(jstr(&v2, "error") == "unknown_model");
    assert!(jstr(&v2, "message").find("gf\"s").is_some());

    // quote inside a PARAM name too (unknown_param echo): %22 -> '"'.
    let b3 = "/v1/point?model=gfs&param=TM%22P&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r3 = rt.block_on(fetch(svc.clone(), b3.to_string()));
    assert!(r3.0 == 404);
    let v3 = jparse(&r3.1);
    assert!(jstr(&v3, "error") == "unknown_param");
    assert!(jstr(&v3, "message").find("TM\"P").is_some());

    // backslash via wire %5C as well (proves decode + escaper compose):
    // decodes to aqb-with-one-backslash, echoes doubled on the wire.
    let b4 = "/v1/point?model=x%5Cy&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r4 = rt.block_on(fetch(svc.clone(), b4.to_string()));
    assert!(r4.0 == 404);
    let v4 = jparse(&r4.1);
    assert!(jstr(&v4, "message").find("x\\y").is_some());
    assert!(has(&r4.1, "x\\\\y"));

    // SINGLE PASS proof at the echo layer: wire %2548 decodes ONCE to the
    // literal text "%48" (a second pass would have produced 'H'); the
    // unknown_param message therefore names '%48'.
    let b5 = "/v1/point?model=gfs&param=%2548&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r5 = rt.block_on(fetch(svc.clone(), b5.to_string()));
    assert!(r5.0 == 404);
    let v5 = jparse(&r5.1);
    let m5 = jstr(&v5, "message");
    assert!(m5.find("parameter '%48'").is_some());
    assert!(m5.find("parameter 'H'").is_none());

    // %% opens a complete escape and yields ONE literal '%': model "gfs%%x"
    // (wire) decodes to the TEXT gfs%x (never touching the following 3Ax).
    let b6 = "/v1/point?model=gfs%%x&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let r6 = rt.block_on(fetch(svc.clone(), b6.to_string()));
    assert!(r6.0 == 404);
    let v6 = jparse(&r6.1);
    assert!(jstr(&v6, "message").find("model 'gfs%x'").is_some());

    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// 2b. /v1/models + /v1/inventory with a CONFIG model name containing QUOTE +
// BACKSLASH (AUDIT_1.3 P2 repro class: config strings interpolated into JSON
// unescaped). The name we"ird\x must (i) render VALID JSON from /v1/models
// with the exact name round-tripping, (ii) filter /v1/inventory when sent
// percent-encoded, and (iii) answer point queries end-to-end — proving the
// escaper covers meta renderers AND the uniform decode reaches meta queries.
// ---------------------------------------------------------------------------

#[test]
fn models_config_quote_backslash_units_renders_valid_json() {
    // V1.4 phase B2 pinned MODEL NAMES to [A-Za-z0-9_-]+ (traversal guard),
    // so the arbitrary-string vector surviving into CONFIG-rendered JSON is
    // UNITS on raw-triple params (user-overridable since E2). Retargeted
    // from phase A's quote/backslash NAME proof -- the charset rule now
    // rejects such names at startup, pinned by its sibling test below.
    let root = "/tmp/opencode/wxpoint-ccompat-weird".to_string();
    // TOML literal appended after the two named params: units = "m\"x\\z"
    let extra = "\n  [[models.params]]\n  discipline = 0\n  category = 2\n  number = 1\n  units = \"m\\\"x\\\\z\"\n  levels = [\"height_above_ground:10\"]\n";
    let ct_r = serve_setup(root.to_string(), "\"gfs\"", extra);
    let ct = match ct_r {
        None => { panic!(); }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let svc = ct.svc.clone();
    let want_u = "m\"x\\z"; // decoded units: m " x \ z

    let rm = rt.block_on(fetch(svc.clone(), "/v1/models".to_string()));
    assert!(rm.0 == 200);
    let vm = jparse(&rm.1); // pre-A escaper: INVALID JSON (raw '"' in value)
    let am = jarr(&vm, "models");
    assert!(am.len() == 1);
    assert!(jstr(&am[0], "name") == "gfs");
    let ap = jarr(&am[0], "params");
    let mut found = false;
    for e in &ap {
        if jstr(e, "units") == want_u {
            found = true;
        }
    }
    assert!(found); // parsed round-trip proves the JSON is valid AND correct
    // wire bytes prove the escaper (not serde) did it: quote as \", doubled.
    assert!(has(&rm.1, "m\\\"x\\\\z"));

    rm_tree(&root);
}

#[test]
fn meta_endpoints_percent_decode_the_model_selector() {
    // Uniform decode reaches META endpoints too (phase A extended
    // normalize_qs to /v1/inventory): g%66s decodes to gfs and must select
    // exactly the one indexed model -- inventory AND an end-to-end point
    // query through the encoded selector answer like the bare form.
    let root = "/tmp/opencode/wxpoint-ccompat-meta".to_string();
    let ct_r = serve_setup(root.to_string(), "\"gfs\"", "");
    let ct = match ct_r {
        None => { panic!(); }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let svc = ct.svc.clone();

    let ri = rt.block_on(fetch(svc.clone(), "/v1/inventory?model=g%66s".to_string()));
    assert!(ri.0 == 200);
    let vi = jparse(&ri.1);
    let ai = jarr(&vi, "inventory");
    assert!(ai.len() == 1);
    assert!(jstr(&ai[0], "model") == "gfs");

    let qp = "/v1/point?model=g%66s&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
    let rq = rt.block_on(fetch(svc.clone(), qp.to_string()));
    assert!(rq.0 == 200);
    let vq = jparse(&rq.1);
    assert!(jstr(&vq, "model") == "gfs"); // success-body echo of decoded value
    assert!(has(&rq.1, "\"value\":250")); // same node as the bare-name probe

    rm_tree(&root);
}

#[test]
fn model_name_out_of_grammar_fails_open_loudly() {
    // Phase B2 replaced phase A's runtime escape-of-weird-names scenario
    // with a STARTUP refusal: a name carrying quote/backslash (or '/', '.',
    // whitespace) never reaches any renderer because validate_config
    // rejects it. The old payload stays here as the regression pin.
    let root = "/tmp/opencode/wxpoint-ccompat-badname".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ingest-gfs");
    let _c1 = create_dir_all(&ing);
    write_text(&join_path(&root, "config.toml"), cfg_text(&root, "\"we\\\"ird\\\\x\"", ""));
    let pr = Pipeline::open(&join_path(&root, "config.toml"));
    match pr {
        Ok(_p) => { panic!("out-of-grammar model name opened OK"); }
        Err(e) => {
            // B2 message: names must match [A-Za-z0-9_-]+ exactly.
            if !e.contains("must match [A-Za-z0-9_-]+") {
                println!("rejection message: {0}", e);
                panic!();
            }
        }
    }
    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// 3a. PINNED SEMANTICS (contractual since V1.4 phase A; the behavior existed,
// the TEST did not). Fixture holds TWO runs: gfs_small 2026-10-19T12Z (fh
// 0/6/12) and prov_new 18Z (TMP only, fh 0). Rule pinned here: a `run` pin
// selects EXACTLY that run; if it is ABSENT or does NOT cover `valid`, the
// answer is 404 no_run_covers — there is NEVER a silent fallback to another
// indexed run, even when an unpinned query would happily answer 200.
// ---------------------------------------------------------------------------

#[test]
fn pinned_run_absent_or_noncovering_answers_404_no_fallback() {
    let root = "/tmp/opencode/wxpoint-ccompat-pin".to_string();
    let ct_r = serve_setup(root.to_string(), "\"gfs\"", "");
    let ct = match ct_r {
        None => { panic!(); }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let svc = ct.svc.clone();
    let q0 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350";

    // CONTROL: unpinned valid 15Z answers 280 from the 12Z run's [0,6]h
    // bracket (latest-run-wins picks 12Z because 18Z starts AFTER 15Z).
    let c1 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-10-19T15:00:00Z", q0)));
    assert!(c1.0 == 200 && has(&c1.1, "\"value\":280"));

    // pin to the COVERING run: identical answer, provenance echoed.
    let c2 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-10-19T15:00:00Z&run=2026-10-19T12:00:00Z", q0)));
    assert!(c2.0 == 200 && c2.1 == c1.1);

    // pin to a run that IS INDEXED but begins after `valid` (18Z > 15Z):
    // 404 no_run_covers, NOT a fallback to the 12Z run's 280.
    let p1 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-10-19T15:00:00Z&run=2026-10-19T18:00:00Z", q0)));
    assert!(p1.0 == 404);
    let vp1 = jparse(&p1.1);
    assert!(jstr(&vp1, "error") == "no_run_covers");

    // pin to an ABSENT run -> same token.
    let p2 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-10-19T15:00:00Z&run=2026-10-19T06:00:00Z", q0)));
    assert!(p2.0 == 404);
    let vp2 = jparse(&p2.1);
    assert!(jstr(&vp2, "error") == "no_run_covers");

    // absent pin sent PERCENT-ENCODED: decode precedes the pin match, so the
    // token is still no_run_covers (pre-A this was 400 bad_time — the pin
    // value was never decoded).
    let p3 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-10-19T15%3A00%3A00Z&run=2026-10-19T06%3A00%3A00Z", q0)));
    assert!(p3.0 == 404);
    let vp3 = jparse(&p3.1);
    assert!(jstr(&vp3, "error") == "no_run_covers");

    // pin to an OLDER run whose stored forecast horizon ends BEFORE `valid`
    // (12Z tops out at fh 12; asking for fh 18) -> no_run_covers, again with
    // no drift to the other run (unpinned is 404 here too: NEITHER covers).
    let p4 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-10-20T06:00:00Z&run=2026-10-19T12:00:00Z", q0)));
    assert!(p4.0 == 404 && has(&p4.1, "no_run_covers"));
    let p5 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-10-20T06:00:00Z", q0)));
    assert!(p5.0 == 404 && has(&p5.1, "no_run_covers"));

    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// 3b. MULTI-PARAM ERROR CONTRACT (contractual since V1.4 phase A; behavior
// exists since milestone 4 and is documented at queryapi::lookup_entries):
// one bad parameter poisons the WHOLE request, and the FIRST failing param
// decides both status and message — even when an earlier param would succeed
// AND when a later param fails differently. All-good lists keep REQUEST ORDER
// and duplicates (results echo every token, in order, dedup-free).
// ---------------------------------------------------------------------------

#[test]
fn multi_param_first_failing_param_decides_the_whole_request() {
    let root = "/tmp/opencode/wxpoint-ccompat-multi".to_string();
    let ct_r = serve_setup(root.to_string(), "\"gfs\"", "");
    let ct = match ct_r {
        None => { panic!(); }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let svc = ct.svc.clone();
    let q0 = "/v1/point?model=gfs&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";

    // good-then-bad: the BAD param names the 404 (the good TMP answer is
    // discarded entirely — the request fails atomically).
    let m1 = rt.block_on(fetch(svc.clone(), format!("{0}&param=TMP,NOPE", q0)));
    assert!(m1.0 == 404);
    let vm1 = jparse(&m1.1);
    assert!(jstr(&vm1, "error") == "unknown_param");
    let mm1 = jstr(&vm1, "message");
    assert!(mm1.find("parameter 'NOPE'").is_some());
    assert!(mm1.find("parameter 'TMP'").is_none());

    // bad-first ordering names the SAME bad param (it is simply first).
    let m2 = rt.block_on(fetch(svc.clone(), format!("{0}&param=NOPE,TMP", q0)));
    assert!(m2.0 == 404);
    let vm2 = jparse(&m2.1);
    assert!(jstr(&vm2, "message").find("parameter 'NOPE'").is_some());

    // two DIFFERENT bad params: the FIRST one decides.
    let m3 = rt.block_on(fetch(svc.clone(), format!("{0}&param=NOPE1,NOPE2", q0)));
    assert!(m3.0 == 404);
    let vm3 = jparse(&m3.1);
    assert!(jstr(&vm3, "message").find("parameter 'NOPE1'").is_some());

    // all-good control: request order preserved, duplicate token kept twice.
    let m4 = rt.block_on(fetch(svc.clone(), format!("{0}&param=UGRD,TMP,TMP", q0)));
    assert!(m4.0 == 200);
    let vm4 = jparse(&m4.1);
    let ar = jarr(&vm4, "results");
    assert!(ar.len() == 3);
    assert!(jstr(&ar[0], "param") == "UGRD");
    assert!(jstr(&ar[1], "param") == "TMP");
    assert!(jstr(&ar[2], "param") == "TMP");

    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// 1e. THE STRICT ISO VALIDATOR IS INTACT THROUGH DECODING: uniform decoding
// FEEDS parse_iso_z real colons but the exact-20-char grammar + calendar
// table still reject impossible dates and out-of-range hours that were
// percent-encoded on the wire (deviation check the mission demanded).
// ---------------------------------------------------------------------------

#[test]
fn strict_calendar_validator_holds_through_percent_decoding() {
    let root = "/tmp/opencode/wxpoint-ccompat-cal".to_string();
    let ct_r = serve_setup(root.to_string(), "\"gfs\"", "");
    let ct = match ct_r {
        None => { panic!(); }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let svc = ct.svc.clone();
    let q0 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350";

    // Feb 31 with ENCODED colons: escapes are complete (no BadEscape), yet
    // the decoded text fails the CALENDAR -> bad_time, not bad_percent_escape.
    let k1 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-02-31T12%3A00%3A00Z", q0)));
    assert!(k1.0 == 400);
    let vk1 = jparse(&k1.1);
    assert!(jstr(&vk1, "error") == "bad_time");

    // non-leap Feb 29 in 2026, encoded hour.
    let k2 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-02-29T12%3A00%3A00Z", q0)));
    assert!(k2.0 == 400 && has(&k2.1, "bad_time"));

    // hour 25 after decoding.
    let k3 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-10-19T25%3A00%3A00Z", q0)));
    assert!(k3.0 == 400 && has(&k3.1, "bad_time"));

    // the same strictness applies to a PIN value: impossible-date run pin is
    // bad_time even though valid= is fine.
    let k4 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-10-19T15:00:00Z&run=2026-02-31T12%3A00%3A00Z", q0)));
    assert!(k4.0 == 400 && has(&k4.1, "bad_time"));

    // CONTROL proving the escape path itself is healthy: a REAL date with
    // encoded colons answers 404 no_run_covers (parses fine; fixture simply
    // has no February data) — definitely NOT bad_time/bad_percent_escape.
    let k5 = rt.block_on(fetch(svc.clone(), format!("{0}&valid=2026-02-28T12%3A00%3A00Z", q0)));
    assert!(k5.0 == 404);
    let vk5 = jparse(&k5.1);
    assert!(jstr(&vk5, "error") == "no_run_covers");

    rm_tree(&root);
}
