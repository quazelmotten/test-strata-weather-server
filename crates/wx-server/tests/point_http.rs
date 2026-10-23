// Milestone 4 (§7 + §9): every HTTP endpoint driven IN-PROCESS through the
// real axum Router (tower-service Service.call + tokio block_on), bodies
// compared as BYTES against the fixture ground truth re-probed from
// milestones 1-3 (tests/fixtures/expected.json values).

use wx_server::httpapi::{ServerState, build_router};
use wx_server::stats::new_stats;
use wx_ingest::pipeline::Pipeline;
use wx_ingest::grib2::epoch_secs;
use wx_server::timeparse::{format_iso_z, parse_iso_z};
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

// e2e-style three-model config (gfs gets BOTH twins so the merge path is
// exercised end-to-end through the server too).
#[allow(clippy::needless_return)]
fn cfg_text(root: &str) -> String {
    let ing_gfs = join_path(root, "ingest-gfs");
    let ing_hrr = join_path(root, "ingest-hrrr");
    let ing_stat = join_path(root, "ingest-stat");
    let data = join_path(root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ningest_threads = 2\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_gfs);
    let t = t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"isobaric:*\", \"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"isobaric:*\", \"height_above_ground:10\"]\n\n[[models]]\nname = \"hrrr\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_hrr);
    let t = t + "\"\nfile_glob = \"hrrr_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"height_above_ground:10\"]\n\n[[models]]\nname = \"stat\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_stat);
    let t = t + "\"\nfile_glob = \"stat_*.grib2\"\n\n  [[models.params]]\n  name = \"APCP\"\n  levels = [\"*\"]\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n";
    return t;
}

struct Ctx {
    pub p: Pipeline,
    pub svc: RouterIntoService<Body>,
    pub rt: Runtime,
}

// Build the pipeline over a fresh tmp root and ingest everything (two scan
// rounds with stable_window_secs=0), then wire the axum router around it.
#[allow(clippy::needless_return)]
fn serve_setup(root: String) -> Option<Ctx> {
    rm_tree(&root);
    let ing_gfs = join_path(&root, "ingest-gfs");
    let ing_hrr = join_path(&root, "ingest-hrrr");
    let ing_stat = join_path(&root, "ingest-stat");
    let _c1 = create_dir_all(&ing_gfs);
    let _c2 = create_dir_all(&ing_hrr);
    let _c3 = create_dir_all(&ing_stat);
    copy_file(fixture("gfs_small.grib2"), join_path(&ing_gfs, "gfs_small.grib2"));
    copy_file(fixture("gfs_small_c3.grib2"), join_path(&ing_gfs, "gfs_small_c3.grib2"));
    copy_file(fixture("hrrr_small.grib2"), join_path(&ing_hrr, "hrrr_small.grib2"));
    copy_file(fixture("stat_small.grib2"), join_path(&ing_stat, "stat_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_text(&cfg_path, cfg_text(&root));

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    let _n1 = p.scan_round();
    let n2 = p.scan_round();
    assert!(n2 == 4);
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
        Err(_e) => { (code, Vec::<u8>::new())}
        Ok(cc) => {
            let bb = cc.to_bytes();
            (code, bb.to_vec())
        }
    }
}

#[test]
fn point_endpoint_truth_table() {
    let root = "/tmp/opencode/wxpoint-http".to_string();
    let cx_o = serve_setup(root.to_string());
    match cx_o {
        None => { panic!(); }
        Some(cx) => {
            let _keep = cx.p;
            let rt = cx.rt;
            let svc = cx.svc;

            // ---- exact grid node: 250, horizontal "exact" -----------------
            let q1 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
            let r1 = rt.block_on(fetch(svc.clone(), q1.to_string()));
            assert!(r1.0 == 200);
            assert!(has(&r1.1, "\"value\":250,"));
            assert!(has(&r1.1, "\"horizontal\":\"exact\""));
            assert!(has(&r1.1, "\"run\":\"2026-10-19T12:00:00Z\""));
            assert!(has(&r1.1, "\"units\":\"K\""));

            // ---- bilinear mid-cell: 252.5 ---------------------------------
            let q2 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=9.75&lon=350.25&valid=2026-10-19T12:00:00Z";
            let r2 = rt.block_on(fetch(svc.clone(), q2.to_string()));
            assert!(r2.0 == 200);
            assert!(has(&r2.1, "\"value\":252.5"));
            assert!(has(&r2.1, "\"horizontal\":\"bilinear\""));

            // ---- vertical interpolation with bracket echo [1000,850] ------
            let q3 = "/v1/point?model=gfs&param=TMP&level=isobaric:925&lat=9&lon=351.5&valid=2026-10-19T12:00:00Z";
            let r3 = rt.block_on(fetch(svc.clone(), q3.to_string()));
            assert!(r3.0 == 200);
            assert!(has(&r3.1, "\"value\":311"));
            assert!(has(&r3.1, "\"vertical\":{\"method\":\"linear\",\"levels\":[1000,850]}"));

            // ---- time interpolation fh [0,6] -> 291 -----------------------
            let q4 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=9&lon=351.5&valid=2026-10-19T15:00:00Z";
            let r4 = rt.block_on(fetch(svc.clone(), q4.to_string()));
            assert!(r4.0 == 200);
            assert!(has(&r4.1, "\"value\":291"));
            assert!(has(&r4.1, "\"time\":{\"method\":\"linear\",\"forecast_hours\":[0,6]}"));

            // ---- masked point -> null + reason (§9.7) ---------------------
            let q5 = "/v1/point?model=gfs&param=TMP&level=isobaric:850&lat=10&lon=350&valid=2026-10-19T18:00:00Z";
            let r5 = rt.block_on(fetch(svc.clone(), q5.to_string()));
            assert!(r5.0 == 200);
            assert!(has(&r5.1, "\"value\":null"));
            assert!(has(&r5.1, "\"reason\":\"missing_data\""));

            // ---- HRRR LCC exact node 260 ----------------------------------
            let q6 = "/v1/point?model=hrrr&param=TMP&level=height_above_ground:2&lat=21.11389&lon=233.44&valid=2026-10-19T12:00:00Z";
            let r6 = rt.block_on(fetch(svc.clone(), q6.to_string()));
            assert!(r6.0 == 200);
            assert!(has(&r6.1, "\"value\":260"));

            // ---- csv multi-param: both entries present --------------------
            let q7 = "/v1/point?model=gfs&param=TMP,UGRD&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
            let r7 = rt.block_on(fetch(svc.clone(), q7.to_string()));
            assert!(r7.0 == 200);
            assert!(has(&r7.1, "\"param\":\"TMP\""));
            assert!(has(&r7.1, "\"param\":\"UGRD\""));
            assert!(has(&r7.1, "\"units\":\"m/s\""));

            // ---- pinned run parameter accepted (same run available) -------
            let q8 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z&run=2026-10-19T12:00:00Z";
            let r8 = rt.block_on(fetch(svc.clone(), q8.to_string()));
            assert!(r8.0 == 200);
            assert!(has(&r8.1, "\"value\":250,"));
        }
    }
    rm_tree(&root);
}

#[test]
fn error_status_table() {
    let root = "/tmp/opencode/wxpoint-http-err".to_string();
    let cx_o = serve_setup(root.to_string());
    match cx_o {
        None => { panic!(); }
        Some(cx) => {
            let _keep = cx.p;
            let rt = cx.rt;
            let svc = cx.svc;

            // 404 unknown model
            let e1 = "/v1/point?model=nope&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
            let r1 = rt.block_on(fetch(svc.clone(), e1.to_string()));
            assert!(r1.0 == 404);
            assert!(has(&r1.1, "\"error\":\"unknown_model\""));

            // 404 no run covers (valid far in the future)
            let e2 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2030-01-01T00:00:00Z";
            let r2 = rt.block_on(fetch(svc.clone(), e2.to_string()));
            assert!(r2.0 == 404);
            assert!(has(&r2.1, "\"error\":\"no_run_covers\""));

            // 422 level outside stored range [1000..250] for TMP isobaric
            let e3 = "/v1/point?model=gfs&param=TMP&level=isobaric:100&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
            let r3 = rt.block_on(fetch(svc.clone(), e3.to_string()));
            assert!(r3.0 == 422);
            assert!(has(&r3.1, "\"error\":\"level_out_of_range\""));

            // 422 stat guard (§9.8): APCP fh 3 must not time-interpolate
            let e4 = "/v1/point?model=stat&param=APCP&level=surface&lat=20&lon=240&valid=2026-10-19T15:00:00Z";
            let r4 = rt.block_on(fetch(svc.clone(), e4.to_string()));
            assert!(r4.0 == 422);
            assert!(has(&r4.1, "\"error\":\"stat_time_guard\""));
            // V1.3 phase E1: the message NAMES the refused bracket and WHICH
            // forecast hour forbids blending (per-fh guard payload). Pre-E1
            // the message was a static string naming no hour at all.
            assert!(has(&r4.1, "between forecast hours 0 h and 6 h"));
            assert!(has(&r4.1, "forecast hour 0 h is 'accumulation'"));

            // APCP exact fh 0 answers 10 (guard does not break exact matches)
            let e5 = "/v1/point?model=stat&param=APCP&level=surface&lat=20&lon=240&valid=2026-10-19T12:00:00Z";
            let r5 = rt.block_on(fetch(svc.clone(), e5.to_string()));
            assert!(r5.0 == 200);
            assert!(has(&r5.1, "\"value\":10"));

            // 422 time_interp=none with no exact fh
            let e6 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=9&lon=351.5&valid=2026-10-19T15:00:00Z&time_interp=none";
            let r6 = rt.block_on(fetch(svc.clone(), e6.to_string()));
            assert!(r6.0 == 422);
            assert!(has(&r6.1, "\"error\":\"time_exact_only\""));

            // 422 out of domain (GFS small grid spans lat -10..10)
            let e7 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=12&lon=350&valid=2026-10-19T12:00:00Z";
            let r7 = rt.block_on(fetch(svc.clone(), e7.to_string()));
            assert!(r7.0 == 422);
            assert!(has(&r7.1, "\"error\":\"out_of_domain\""));

            // 400 bad_lat
            let e8 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=99&lon=350&valid=2026-10-19T12:00:00Z";
            let r8 = rt.block_on(fetch(svc.clone(), e8.to_string()));
            assert!(r8.0 == 400);
            assert!(has(&r8.1, "\"error\":\"bad_lat\""));

            // 400 bad_time
            let e9 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=nonsense";
            let r9 = rt.block_on(fetch(svc.clone(), e9.to_string()));
            assert!(r9.0 == 400);
            assert!(has(&r9.1, "\"error\":\"bad_time\""));

            // 400 bad_level
            let e10 = "/v1/point?model=gfs&param=TMP&level=bogus:1&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
            let r10 = rt.block_on(fetch(svc.clone(), e10.to_string()));
            assert!(r10.0 == 400);
            assert!(has(&r10.1, "\"error\":\"bad_level\""));

            // 400 bad_time_interp
            let e11 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z&time_interp=cubic";
            let r11 = rt.block_on(fetch(svc.clone(), e11.to_string()));
            assert!(r11.0 == 400);
            assert!(has(&r11.1, "\"error\":\"bad_time_interp\""));

            // 400 unknown query key
            let e12 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&zzz=1&valid=2026-10-19T12:00:00Z";
            let r12 = rt.block_on(fetch(svc.clone(), e12.to_string()));
            assert!(r12.0 == 400);
            assert!(has(&r12.1, "\"error\":\"unknown_query_param\""));

            // 400 missing required (no lon at all)
            let e12b = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&valid=2026-10-19T12:00:00Z";
            let r12c = rt.block_on(fetch(svc.clone(), e12b.to_string()));
            assert!(r12c.0 == 400);
            assert!(has(&r12c.1, "\"error\":\"missing_required\""));

            // 400 empty value
            let e13 = "/v1/point?model=&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
            let r13 = rt.block_on(fetch(svc.clone(), e13.to_string()));
            assert!(r13.0 == 400);
            assert!(has(&r13.1, "\"error\":\"empty_value\""));

            // bare /v1/point without query string
            let e14 = "/v1/point";
            let r14 = rt.block_on(fetch(svc.clone(), e14.to_string()));
            assert!(r14.0 == 400);
            assert!(has(&r14.1, "\"error\":\"missing_required\""));
        }
    }
    rm_tree(&root);
}

#[test]
fn meta_endpoints_and_metrics() {
    let root = "/tmp/opencode/wxpoint-http-meta".to_string();
    let cx_o = serve_setup(root.to_string());
    match cx_o {
        None => { panic!(); }
        Some(cx) => {
            let _keep = cx.p;
            let rt = cx.rt;
            let svc = cx.svc;

            // /health exact body
            let rh = rt.block_on(fetch(svc.clone(), "/health".to_string()));
            assert!(rh.0 == 200);
            assert!(bytes_contains(&rh.1, "{\"ok\":true}".as_bytes()));

            // /ready after ingest
            let rd = rt.block_on(fetch(svc.clone(), "/ready".to_string()));
            assert!(rd.0 == 200);
            assert!(bytes_contains(&rd.1, "{\"ready\":true}".as_bytes()));

            // /v1/models: grid info incl LCC params + configured levels
            let rm = rt.block_on(fetch(svc.clone(), "/v1/models".to_string()));
            assert!(rm.0 == 200);
            assert!(has(&rm.1, "\"kind\":\"regular\""));
            assert!(has(&rm.1, "\"nx\":41"));
            assert!(has(&rm.1, "\"kind\":\"lcc\""));
            assert!(has(&rm.1, "\"latin1\":"));
            assert!(has(&rm.1, "\"isobaric:*\""));

            // /v1/inventory?model=gfs
            let ri = rt.block_on(fetch(svc.clone(), "/v1/inventory?model=gfs".to_string()));
            assert!(ri.0 == 200);
            assert!(has(&ri.1, "\"run\":\"2026-10-19T12:00:00Z\""));
            assert!(has(&ri.1, "\"forecast_hours\":[0,6,12]"));
            assert!(has(&ri.1, "\"stat_processed\":false"));

            // inventory unknown model -> 404
            let ru = rt.block_on(fetch(svc.clone(), "/v1/inventory?model=nope".to_string()));
            assert!(ru.0 == 404);
            assert!(has(&ru.1, "\"error\":\"unknown_model\""));

            // stat model inventory shows the 4.8 marker
            let rs = rt.block_on(fetch(svc.clone(), "/v1/inventory?model=stat".to_string()));
            assert!(rs.0 == 200);
            assert!(has(&rs.1, "\"stat_processed\":true"));
            assert!(has(&rs.1, "\"stat_kind\":\"accumulation\""));

            // unknown endpoint fallback -> JSON 404 token
            let rf = rt.block_on(fetch(svc.clone(), "/bogus".to_string()));
            assert!(rf.0 == 404);
            assert!(has(&rf.1, "\"error\":\"unknown_endpoint\""));

            // /metrics: histogram + status counts + lag present; queries counted.
            let rq = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(rq.0 == 200);
            let rx = rt.block_on(fetch(svc.clone(), "/metrics".to_string()));
            assert!(rx.0 == 200);
            assert!(has(&rx.1, "wxpoint_queries_total{status=\"2xx\"}"));
            assert!(has(&rx.1, "wxpoint_query_latency_us_bucket{le=\"1\"}"));
            assert!(has(&rx.1, "wxpoint_query_latency_us_bucket{le=\"+inf\"}"));
            assert!(has(&rx.1, "wxpoint_data_age_seconds"));
            // V1.3 phase E6: the TRUE latency pair must be exposed too (this
            // harness indexes without an attached pipeline -> legitimate 0
            // VALUES; presence + well-formedness is what we pin here).
            assert!(has(&rx.1, "wxpoint_ingest_publish_latency_seconds"));
            assert!(has(&rx.1, "wxpoint_ingest_publish_latency_seconds_max"));
            // ---- V1.3 phase E6 TARGETED %-DECODE EQUIVALENCE ----------------
            // %3A/%3a inside a level VALUE and %2C/%2c inside a param VALUE must
            // behave EXACTLY like their bare counterparts: byte-identical
            // responses (queryapi header documents the rule).
            let d_plain = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(d_plain.0 == 200);
            let d_up = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TMP&level=isobaric%3A1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(d_up.0 == 200 && d_up.1 == d_plain.1);
            let d_lo = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TMP&level=isobaric%3a1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(d_lo.0 == 200 && d_lo.1 == d_plain.1);
            // %2C splits tokens exactly like a bare comma (TMP,TMP keeps the
            // probe valid on whatever single-param fixture this harness holds).
            let dm_plain = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TMP,TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(dm_plain.0 == 200);
            let dm_up = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TMP%2CTMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(dm_up.0 == 200 && dm_up.1 == dm_plain.1);
            let dm_lo = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TMP%2cTMP&level=isobaric%3A1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(dm_lo.0 == 200 && dm_lo.1 == dm_plain.1); // lowercase-hex %2c + uppercase-hex %3A together
            // Passthrough rules: unknown/malformed % sequences stay LITERAL
            // bytes (so they look up as an unknown parameter -> 404), '+' is
            // never a space, and %-decoding never invents a valid level.
            let dq_zz = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TMP%zz&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(dq_zz.0 == 404 && has(&dq_zz.1, "unknown_param"));
            let dq_tail = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TMP%&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(dq_tail.0 == 404 && has(&dq_tail.1, "unknown_param"));
            let dq_plus = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TM+P&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(dq_plus.0 == 404 && has(&dq_plus.1, "unknown_param"));
            let dq_lvl = rt.block_on(fetch(svc.clone(), "/v1/point?model=gfs&param=TMP&level=isobaric%3A85%zz&lat=10&lon=350&valid=2026-10-19T12:00:00Z".to_string()));
            assert!(dq_lvl.0 == 400 && has(&dq_lvl.1, "bad_level"));
            assert!(has(&rx.1, "wxpoint_ingested_files_total"));
            // V1.3 phase E1: stat-collision rejection is observable too.
            assert!(has(&rx.1, "wxpoint_stat_collisions_total"));
        }
    }
    rm_tree(&root);
}

#[test]
fn epoch_civil_roundtrip_across_boundaries() {
    // format_iso_z must be the exact inverse of grib2.epoch_secs across
    // leap days, month/year boundaries and the 400-year era boundary.
    let cases: [&str; 10] = [
        "2026-10-19T12:00:00Z", "2026-10-19T15:00:00Z", "2026-10-19T18:00:00Z",
        "2000-02-29T00:00:00Z", "1970-01-01T00:00:00Z", "2024-12-31T23:59:59Z",
        "2021-01-01T00:00:00Z", "2026-03-01T00:00:00Z", "2026-02-28T00:00:00Z",
        "2026-10-19T00:00:00Z",
    ];
    for &s in cases.iter() {
        let ep_o = parse_iso_z(s);
        match ep_o {
            None => { panic!(); }
            Some(ep) => {
                let back = format_iso_z(ep);
                assert!(bytes_contains(back.as_bytes(), s.as_bytes()));
                assert!(back.len() == s.len());
            }
        }
    }
    // forward direction from civil fields must agree with parse_iso_z too
    let ep2 = epoch_secs(2026, 10, 19, 12, 0, 0);
    let ep1 = parse_iso_z("2026-10-19T12:00:00Z");
    match ep1 {
        None => { panic!(); }
        Some(x) => { assert!(ep2 == x); }
    }
}

#[test]
fn ready_before_ingest_is_503() {
    // /ready must report 503 until every configured model has >=1 indexed run.
    let root = "/tmp/opencode/wxpoint-http-notready".to_string();
    rm_tree(&root);
    let _c = create_dir_all(join_path(&root, "ingest-gfs"));
    let cfgp = join_path(&root, "config.toml");
    write_text(&cfgp, cfg_text(&root));
    let pr = Pipeline::open(&cfgp);
    match pr {
        Err(_e) => { panic!(); }
        Ok(pp) => {
            // NO scan rounds: nothing indexed yet.
            let stats = new_stats();
            let st = ServerState {
                index: pp.shared.clone(),
                stats: stats.clone(),
                models_cfg: Arc::new(pp.models.clone()),
            };
            let app = build_router(st);
            let svc = app.into_service();
            let rt_r = Runtime::new();
            let rt = match rt_r {
                Err(_e) => { panic!(); }
                Ok(rtx) => { rtx }
            };
            let rr = rt.block_on(fetch(svc.clone(), "/ready".to_string()));
            assert!(rr.0 == 503);
            assert!(bytes_contains(&rr.1, "{\"error\":\"not_ready\",\"message\":\"models awaiting first indexed run: gfs,hrrr,stat\"}".as_bytes()));
        }
    }
    rm_tree(&root);
}

#[test]
fn gfs_seam_through_http() {
    // §9.5: lon 359.9 / -0.1 / 0.0 interpolate across the 359.75->0 seam on
    // the full GFS geometry. Conditional-skip like wx-ingest perf_fixture.
    let perf = "../../tests/perfdata/gfs_perf_f000.grib2".to_string();
    let pr0 = File::open(&perf);
    if pr0.is_err() { return; }
    let root = "/tmp/opencode/wxpoint-seam".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    copy_file(perf, join_path(&ing, "gfs_perf_f000.grib2"));
    let cfgp = join_path(&root, "config.toml");
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ningest_threads = 4\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_perf_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n";
    write_text(&cfgp, t);

    let pr = Pipeline::open(&cfgp);
    let mut p = match pr {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    let _r1 = p.scan_round();
    let r2 = p.scan_round();
    assert!(r2 == 1);

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

    // lat 0 -> rows 359/360 -> value 250+j interpolates to 609.5 at ANY lon,
    // including across the seam and with negative longitudes.
    let q1 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=0&lon=359.9&valid=2026-10-19T12:00:00Z";
    let s1 = rt.block_on(fetch(svc.clone(), q1.to_string()));
    assert!(s1.0 == 200);
    assert!(has(&s1.1, "\"value\":609.5"));
    let q2 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=0&lon=-0.1&valid=2026-10-19T12:00:00Z";
    let s2 = rt.block_on(fetch(svc.clone(), q2.to_string()));
    assert!(s2.0 == 200);
    assert!(has(&s2.1, "\"value\":609.5"));
    let q3 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=0&lon=0.0&valid=2026-10-19T12:00:00Z";
    let s3 = rt.block_on(fetch(svc.clone(), q3.to_string()));
    assert!(s3.0 == 200);
    assert!(has(&s3.1, "\"value\":609.5"));
    // lon normalization: 360 boundary accepted (-180..360 domain)
    let q4 = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=0&lon=360&valid=2026-10-19T12:00:00Z";
    let s4 = rt.block_on(fetch(svc.clone(), q4.to_string()));
    assert!(s4.0 == 200);
    assert!(has(&s4.1, "\"value\":609.5"));

    rm_tree(&root);
}
