// V1.4 phase T: §9.5 dateline-seam coverage WITHOUT tests/perfdata/. The only
// pre-existing full-stack seam test (point_http::gfs_seam_through_http) early-
// returns when perfdata/gfs_perf_f000.grib2 is absent, so offline CI verified
// nothing end-to-end across the 0/360 meridian. This file drives the REAL axum
// router over the COMMITTED fixture tests/fixtures/gfs_small.grib2, whose
// regular lat/lon grid deliberately spans lon 350 -> 10 at 0.5 deg, i.e.
// ACROSS the seam (NOTES-fixtures.md: "lon 350 -> 10 (0.5, wraps across 0)",
// scan N->S, lat 10 -> 0; nx=41 ny=21).
//
// Ground truth: the analytic field scripts/gen_grib2.py wrote into the
// fixture -- value = 250 + base_i + lv_i + fh*10 + j*4 + i with storage index
// k = j*41 + i, TMP base_i=0, level offsets lv_i = 0 (@1000mb), 100 (@850),
// 200 (@700), 300 (@2m AGL) (same formula documented in wx-ingest/tests/
// gfs_decode.rs and cross-checked message-by-message against wgrib2 -lon via
// expected.json point_checks). Node coordinates: i = ((lon-350+360)%360)/0.5,
// j = (10-lat)/0.5. Seam cell = columns 19 (lon 359.5) <-> 20 (lon 0.0).
//
// FOUND BUG (V1.4 phase T), FIXED IN PHASE R: GridRegular::project's
// shortest-delta form ((lon-lon1+180)%360)-180 kept Rust's dividend-signed
// remainder, so queries more than 180 deg EAST of lon1 produced di near
// -2*nx instead of the true small positive delta; the fixture's stored
// columns 20..40 (lon 0..10) answered 422 out_of_domain even though
// grid.rs (lines 21-29) documents the domain as the CLOSED node rectangle
// and wgrib2 itself snaps req_lon 0.0/10.0/-0.1 onto real nodes in
// expected.json (e.g. TMP_1000mb_f000 (lon=0.0, lat=5.0) -> 310.0). Fixed
// with rem_euclid (grid.rs); results are IDENTICAL everywhere else, incl.
// every global lon1=0 grid. Spec §9.5 demands queries at lon 359.9 / -0.1 /
// 0.0 across the seam: west-side pins stay below, and the once-ignored
// east-side lon-0.0 test is now an active pin.

use wx_server::httpapi::{ServerState, build_router};
use wx_server::stats::new_stats;
use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use wx_core::tables::parse_f64;
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

fn fixture(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

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
    false
}

fn has(body: &[u8], pat: &str) -> bool {
    bytes_contains(body, pat.as_bytes())
}

// Extract the FIRST "value":<number> token from a point-response body and
// parse it as f64 (exact Display round-trip; avoids brittle digit strings for
// blends like 309.79999999999995 while still pinning the NUMBER exactly to
// within 1e-9 -- every expected value here is analytic).
fn body_value(body: &[u8]) -> f64 {
    let pat = b"\"value\":";
    let mut pos = 0usize;
    let mut found = false;
    while pos + pat.len() <= body.len() {
        if bytes_contains(&body[pos .. pos + pat.len()], pat) {
            found = true;
            break;
        }
        pos += 1;
    }
    if !found {
        println!("no value token in body: {0}", String::from_utf8_lossy(body));
        panic!();
    }
    let s = pos + pat.len();
    let mut e = s;
    while e < body.len() && body[e] != b',' && body[e] != b'}' && body[e] != b']' {
        e += 1;
    }
    let txt = match std::str::from_utf8(&body[s .. e]) {
        Err(_er) => { println!("non-utf8 value slice"); panic!(); }
        Ok(t) => { t }
    };
    match parse_f64(txt) {
        None => { println!("unparsable value token '{0}'", txt); panic!(); }
        Some(v) => { v }
    }
}

struct Ctx {
    pub p: Pipeline,
    pub svc: RouterIntoService<Body>,
    pub rt: Runtime,
}

// ONE model fed by the committed seam fixture; two scan rounds with
// stable_window_secs=0 make it queryable (pattern of frac_http.rs).
#[allow(clippy::needless_return)]
fn serve_setup(root: String) -> Option<Ctx> {
    rm_tree(&root);
    let ing = join_path(&root, "ingest");
    let data = join_path(&root, "data");
    let _c1 = create_dir_all(&ing);
    copy_file(fixture("gfs_small.grib2"), join_path(&ing, "gfs_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ningest_threads = 2\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    write_text(&cfg_path, t);

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    let _n1 = p.scan_round();
    let n2 = p.scan_round();
    assert!(n2 == 1);
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

// Drive one GET through the real router; returns (status, body bytes).
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
        Err(_e) => { (code, Vec::<u8>::new()) }
        Ok(cc) => {
            let bb = cc.to_bytes();
            (code, bb.to_vec())
        }
    }
}

#[test]
fn gfs_seam_blend_pair_norm_and_stacks_through_http() {
    // Everything the seam CAN answer offline today, pinned through the real
    // router. All expected numbers derive from the fixture's analytic field
    // (header of this file): row j=10 (lat 5) carries TMP@1000 f000 values
    // v(i) = 250 + 40 + i, so column 19 (lon 359.5) = 309 and column 20
    // (lon 0.0) = 310 -- those two node values are themselves wgrib2 ground
    // truth in expected.json (point_checks rows req_lon 359.9 and 0.0 snap
    // to 310.0 there). A blend landing on 309.8 can ONLY come from pairing
    // columns 19 <-> 20 across the seam (an edge-clamped stencil would give
    // 309.0; a wrong wrap index would throw or land elsewhere).
    let root = "/tmp/opencode/wxpoint-seam-small".to_string();
    let cx_o = serve_setup(root.to_string());
    match cx_o {
        None => { panic!(); }
        Some(cx) => {
            let _keep = cx.p;
            let rt = cx.rt;
            let svc = cx.svc;
            let base = "/v1/point?model=gfs&param=TMP&level=isobaric:1000";
            let v12 = "&valid=2026-10-19T12:00:00Z";

            // ---- mid-blend just WEST of the meridian: 0.2*309 + 0.8*310 ---
            let q1 = format!("{0}&lat=5&lon=359.9{1}", base, v12);
            let r1 = rt.block_on(fetch(svc.clone(), q1));
            assert!(r1.0 == 200);
            let val1 = body_value(&r1.1);
            if (val1 - 309.8).abs() > 1e-9 { println!("seam blend {0} want 309.8", val1); panic!(); }
            assert!(has(&r1.1, "\"horizontal\":\"bilinear\""));

            // ---- same physical point as -0.1: identical value (norm) ------
            let q2 = format!("{0}&lat=5&lon=-0.1{1}", base, v12);
            let r2 = rt.block_on(fetch(svc.clone(), q2));
            assert!(r2.0 == 200);
            let val2 = body_value(&r2.1);
            if (val2 - 309.8).abs() > 1e-9 { println!("-0.1 blend {0} want 309.8", val2); panic!(); }
            // norm_lon(-0.1) folds onto the SAME double as 359.9 -> bit-equal
            if val2 != val1 { println!("-0.1 {0} not bit-equal to 359.9 {1}", val2, val1); panic!(); }

            // ---- full 4-node stencil across the seam at lat 5.25 ---------
            // affine field 250 + j*4 + i evaluated at i=19.8, j=9.5 = 307.8
            let q3 = format!("{0}&lat=5.25&lon=359.9{1}", base, v12);
            let r3 = rt.block_on(fetch(svc.clone(), q3));
            assert!(r3.0 == 200);
            let val3 = body_value(&r3.1);
            if (val3 - 307.8).abs() > 1e-9 { println!("4-node seam blend {0} want 307.8", val3); panic!(); }

            // ---- exact half-cell mid-point: 309.5 is binary-exact --------
            let q4 = format!("{0}&lat=5&lon=359.75{1}", base, v12);
            let r4 = rt.block_on(fetch(svc.clone(), q4));
            assert!(r4.0 == 200);
            if body_value(&r4.1) != 309.5 { println!("half-cell {0} want 309.5", body_value(&r4.1)); panic!(); }
            assert!(has(&r4.1, "\"value\":309.5"));

            // ---- exact grid node on the west side of the seam cell -------
            let q5 = format!("{0}&lat=5&lon=359.5{1}", base, v12);
            let r5 = rt.block_on(fetch(svc.clone(), q5));
            assert!(r5.0 == 200);
            if body_value(&r5.1) != 309.0 { println!("node 359.5 {0} want 309", body_value(&r5.1)); panic!(); }
            assert!(has(&r5.1, "\"value\":309,"));
            assert!(has(&r5.1, "\"horizontal\":\"exact\""));

            // ---- west node-column edge: hair out rejected, hair in ok ----
            let q6 = format!("{0}&lat=5&lon=349.9{1}", base, v12);
            let r6 = rt.block_on(fetch(svc.clone(), q6));
            assert!(r6.0 == 422);
            assert!(has(&r6.1, "\"error\":\"out_of_domain\""));
            let q7 = format!("{0}&lat=5&lon=350.1{1}", base, v12);
            let r7 = rt.block_on(fetch(svc.clone(), q7));
            assert!(r7.0 == 200);
            let val7 = body_value(&r7.1);
            if (val7 - 290.2).abs() > 1e-9 { println!("col0/1 blend {0} want 290.2", val7); panic!(); }

            // ---- lon 350 and its -180..180 form -10 answer identically ---
            let q8 = format!("{0}&lat=5&lon=350{1}", base, v12);
            let r8 = rt.block_on(fetch(svc.clone(), q8));
            assert!(r8.0 == 200);
            let val8 = body_value(&r8.1);
            if val8 != 290.0 { println!("node 350 {0} want 290", val8); panic!(); }
            let q9 = format!("{0}&lat=5&lon=-10{1}", base, v12);
            let r9 = rt.block_on(fetch(svc.clone(), q9));
            assert!(r9.0 == 200);
            let val9 = body_value(&r9.1);
            if val9 != 290.0 || val9 != val8 { println!("-10 form {0} vs 350 form {1}", val9, val8); panic!(); }
            assert!(has(&r9.1, "\"horizontal\":\"exact\""));

            // ---- vertical interpolation stack anchored at the seam cell --
            // 925 sits midway between 1000 (blend 309.8) and 850 (each node
            // +100 -> blend 409.8): midpoint 359.8, bracket echoed per spec.
            let q10 = "/v1/point?model=gfs&param=TMP&level=isobaric:925&lat=5&lon=359.9&valid=2026-10-19T12:00:00Z";
            let r10 = rt.block_on(fetch(svc.clone(), q10.to_string()));
            assert!(r10.0 == 200);
            let val10 = body_value(&r10.1);
            if (val10 - 359.8).abs() > 1e-9 { println!("vertical@seam {0} want 359.8", val10); panic!(); }
            assert!(has(&r10.1, "\"vertical\":{\"method\":\"linear\",\"levels\":[1000,850]}"));

            // ---- time interpolation stack anchored at the seam cell ------
            // f006 row j=10 adds +60: blend 369.8; fh 3 is the f000/f006
            // midpoint: (309.8 + 369.8)/2 = 339.8 with the bracket echoed.
            let q11 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=5&lon=359.9&valid=2026-10-19T15:00:00Z";
            let r11 = rt.block_on(fetch(svc.clone(), q11.to_string()));
            assert!(r11.0 == 200);
            let val11 = body_value(&r11.1);
            if (val11 - 339.8).abs() > 1e-9 { println!("time@seam {0} want 339.8", val11); panic!(); }
            assert!(has(&r11.1, "\"time\":{\"method\":\"linear\",\"forecast_hours\":[0,6]}"));
        }
    }
    rm_tree(&root);
}

#[test]
fn gfs_beyond_the_last_east_node_column_stays_out_of_domain() {
    // EAST BOUNDARY pin (retargeted by phase R from the found-bug probe):
    // column 40 (lon 10.0) is the LAST stored node column, so lon 10.25 /
    // 10.5 / 11 lie past the CLOSED node rectangle and must answer 422
    // out_of_domain -- no extrapolation (§6.1). Pre-R these three rejected
    // too, but for the WRONG reason (the dateline asymmetry above); post-R
    // they are honest boundary rejections. The acceptance side of the fix
    // (lon 0.0 / 0.25 / 10 answering 200) is pinned by the NEXT test.
    let root = "/tmp/opencode/wxpoint-seam-small-east".to_string();
    let cx_o = serve_setup(root.to_string());
    match cx_o {
        None => { panic!(); }
        Some(cx) => {
            let _keep = cx.p;
            let rt = cx.rt;
            let svc = cx.svc;
            let base = "/v1/point?model=gfs&param=TMP&level=isobaric:1000";
            let v12 = "&valid=2026-10-19T12:00:00Z";
            let lons: [&str; 3] = ["10.25", "10.5", "11"];
            for lo in lons {
                let q = format!("{0}&lat=5&lon={1}{2}", base, lo, v12);
                let r = rt.block_on(fetch(svc.clone(), q));
                if r.0 != 422 || !has(&r.1, "\"error\":\"out_of_domain\"") {
                    println!("lon {0}: got {1} {2}", lo, r.0, String::from_utf8_lossy(&r.1));
                    panic!();
                }
            }
        }
    }
    rm_tree(&root);
}

#[test]

fn gfs_seam_exact_node_at_lon_zero_spec_expected() {
    // Spec-correct behaviour (V1.4 phase R: Euclidean rem_euclid delta):
    // lon 0.0 is the EXACT node column 20 -> value 310 with horizontal
    // "exact"; lon 0.25 blends columns 20/21 -> 310.5; lon 10 is exact node
    // column 40 -> 290 at lat 10. First/third expectations are external
    // wgrib2 truth rows in tests/fixtures/expected.json.
    let root = "/tmp/opencode/wxpoint-seam-small-lon0".to_string();
    let cx_o = serve_setup(root.to_string());
    match cx_o {
        None => { panic!(); }
        Some(cx) => {
            let _keep = cx.p;
            let rt = cx.rt;
            let svc = cx.svc;
            let base = "/v1/point?model=gfs&param=TMP&level=isobaric:1000";
            let v12 = "&valid=2026-10-19T12:00:00Z";
            let q1 = format!("{0}&lat=5&lon=0.0{1}", base, v12);
            let r1 = rt.block_on(fetch(svc.clone(), q1));
            assert!(r1.0 == 200);
            if body_value(&r1.1) != 310.0 { println!("lon 0.0 node {0} want 310", body_value(&r1.1)); panic!(); }
            assert!(has(&r1.1, "\"horizontal\":\"exact\""));
            let q2 = format!("{0}&lat=5&lon=0.25{1}", base, v12);
            let r2 = rt.block_on(fetch(svc.clone(), q2));
            assert!(r2.0 == 200);
            let val2 = body_value(&r2.1);
            if (val2 - 310.5).abs() > 1e-9 { println!("lon 0.25 blend {0} want 310.5", val2); panic!(); }
            let q3 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=10&valid=2026-10-19T12:00:00Z";
            let r3 = rt.block_on(fetch(svc.clone(), q3.to_string()));
            assert!(r3.0 == 200);
            if body_value(&r3.1) != 290.0 { println!("lon 10 node {0} want 290", body_value(&r3.1)); panic!(); }
        }
    }
    rm_tree(&root);
}
