// V1.1 phase 3 (§6.3 + §7 + §7 metrics): fractional forecast hours and
// non-hourly reference times through the FULL axum stack, plus the skip-and-note
// counter on /metrics. Fixtures: `frac_small.grib2` (minute-unit CT 4.4 forecast
// times, TWO runs 12Z and 12:30Z, one 30-minute accumulation) and
// `robust_small.grib2` (coordinate-list + layer messages that must be SKIPPED,
// not failed). Ground truth is the wgrib2-probed analytic field
// value = BASE + j*21 + i + fc_minute (BASE 250 run 12Z, 500 run 12:30Z,
// 5 for APCP); probe node lat 9.0 lon 351.5 -> j=2, i=3.

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

fn fixture(name: &str) -> String {
    return format!("../../tests/fixtures/{name}");
}

fn rm_tree(dir: &str) {
    match read_dir(&dir) {
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
    let _d = remove_dir(&dir);
}

fn copy_file(src: String, dst: String) {
    let f = File::open(&src);
    match f {
        Err(_) => { assert!(false); return; }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { assert!(false); return; }
                Ok(region) => {
                    let d = File::create(&dst);
                    match d {
                        Err(_) => { assert!(false); return; }
                        Ok(mut w) => {
                            let n = w.write_all(&region[0 .. region.len()]);
                            match n {
                                Err(_) => { assert!(false); }
                                Ok(_) => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

fn write_text(path: &str, text: String) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(text.as_bytes());
            match n {
                Err(_) => { assert!(false); }
                Ok(_) => {}
            }
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
    return false;
}

fn has(body: &[u8], pat: &str) -> bool {
    return bytes_contains(body, pat.as_bytes());
}

// ONE ingest dir feeds BOTH models; the per-model file_glob does the separation
// (spec §3: filenames are only ever a glob filter, never metadata).
fn cfg_text(root: &str) -> String {
    let ing = join_path(&root, "ingest");
    let data = join_path(&root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"frac\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"frac_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n\n  [[models.params]]\n  name = \"APCP\"\n  levels = [\"*\"]\n\n[[models]]\nname = \"robust\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"robust_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

struct Ctx {
    pub p: Pipeline,
    pub svc: RouterIntoService<Body>,
    pub rt: Runtime,
}

fn serve_setup(root: String) -> Option<Ctx> {
    rm_tree(&root);
    let ing = join_path(&root, "ingest");
    let _c1 = create_dir_all(&ing);
    copy_file(fixture("frac_small.grib2"), join_path(&ing, "frac_small.grib2"));
    copy_file(fixture("robust_small.grib2"), join_path(&ing, "robust_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_text(&cfg_path, cfg_text(&root));

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_e) => { assert!(false); return None; }
        Ok(pp) => { pp }
    };
    let _n1 = p.scan_round();
    let n2 = p.scan_round();
    assert!(n2 == 2); // both fixtures ingested
    assert!(p.counters.errors == 0);
    // coordinate-list + layer messages land in the SKIP counter, not errors
    assert!(p.counters.skipped_messages == 2);

    let stats = new_stats();
    // mirror what main.rs does after every scan round (spec §7 metrics)
    stats.set_ingest(p.counters.errors, p.counters.ingested_files,
                     p.counters.skipped_messages, p.counters.last_mtime_ns);
    let st = ServerState {
        index: p.shared.clone(),
        stats: stats.clone(),
        models_cfg: Arc::new(p.models.clone()),
    };
    let app = build_router(st);
    let svc = app.into_service();
    let rt_r = Runtime::new();
    let rt = match rt_r {
        Err(_e) => { assert!(false); return None; }
        Ok(rtx) => { rtx }
    };
    return Some(Ctx { p: p, svc: svc, rt: rt });
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
        Err(_e) => { return (code, Vec::<u8>::new()); }
        Ok(cc) => {
            let bb = cc.to_bytes();
            return (code, bb.to_vec());
        }
    }
}

#[test]
fn fractional_hours_over_http() {
    let root = "/tmp/opencode/wxpoint-frac-http".to_string();
    let cx_o = serve_setup(root.to_string());
    match cx_o {
        None => { assert!(false); return; }
        Some(cx) => {
            let _keep = cx.p;
            let rt = cx.rt;
            let svc = cx.svc;

            // ---- exact fractional forecast hour, NEWEST RUN WINS ---------
            // 12:30Z is BOTH run A's f000m30 slot AND run B's f000 slot; spec
            // §6.3 says the most recent run covering the valid time answers,
            // so this must be run B's node value 500 + 45 + 0 = 545.
            let q1 = "/v1/point?model=frac&param=TMP&level=isobaric:850&lat=9&lon=351.5&valid=2026-10-19T12:30:00Z";
            let r1 = rt.block_on(fetch(svc.clone(), q1.to_string()));
            assert!(r1.0 == 200);
            assert!(has(&r1.1, "\"value\":545"));
            assert!(has(&r1.1, "\"horizontal\":\"exact\""));
            assert!(has(&r1.1, "\"run\":\"2026-10-19T12:30:00Z\""));
            // an exact slot means NO time interpolation object at all
            assert!(!has(&r1.1, "\"time\":{"));

            // ---- same instant PINNED to the older run -> its own slot ----
            let q1p = "/v1/point?model=frac&param=TMP&level=isobaric:850&lat=9&lon=351.5&valid=2026-10-19T12:30:00Z&run=2026-10-19T12:00:00Z";
            let r1p = rt.block_on(fetch(svc.clone(), q1p.to_string()));
            assert!(r1p.0 == 200);
            assert!(has(&r1p.1, "\"value\":325"));
            assert!(has(&r1p.1, "\"run\":\"2026-10-19T12:00:00Z\""));

            // ---- mid-bracket between f000m30 and f001.bin of RUN A ------
            let q2 = "/v1/point?model=frac&param=TMP&level=isobaric:850&lat=9&lon=351.5&valid=2026-10-19T12:45:00Z&run=2026-10-19T12:00:00Z";
            let r2 = rt.block_on(fetch(svc.clone(), q2.to_string()));
            assert!(r2.0 == 200);
            assert!(has(&r2.1, "\"value\":340"));
            assert!(has(&r2.1, "\"time\":{\"method\":\"linear\",\"forecast_hours\":[0.5,1]}"));

            // ---- unpinned at the SAME instant: run B brackets [0,0.5]h ---
            // 545 -> 575 halfway -> 560, bracket reported in HOURS per spec §7.
            let q2u = "/v1/point?model=frac&param=TMP&level=isobaric:850&lat=9&lon=351.5&valid=2026-10-19T12:45:00Z";
            let r2u = rt.block_on(fetch(svc.clone(), q2u.to_string()));
            assert!(r2u.0 == 200);
            assert!(has(&r2u.1, "\"value\":560"));
            assert!(has(&r2u.1, "\"time\":{\"method\":\"linear\",\"forecast_hours\":[0,0.5]}"));

            // ---- newest run covering the valid time wins (spec §6.3) ------
            // 13:00Z is 60 min past run 12Z but exactly 30 min past run 12:30Z,
            // so the answer must come from the NEWER run's own slot.
            let q3 = "/v1/point?model=frac&param=TMP&level=isobaric:850&lat=9&lon=351.5&valid=2026-10-19T13:00:00Z";
            let r3 = rt.block_on(fetch(svc.clone(), q3.to_string()));
            assert!(r3.0 == 200);
            assert!(has(&r3.1, "\"value\":575"));
            assert!(has(&r3.1, "\"run\":\"2026-10-19T12:30:00Z\""));

            // ---- pinning the older run answers its own 1 h slot -----------
            let q4 = "/v1/point?model=frac&param=TMP&level=isobaric:850&lat=9&lon=351.5&valid=2026-10-19T13:00:00Z&run=2026-10-19T12:00:00Z";
            let r4 = rt.block_on(fetch(svc.clone(), q4.to_string()));
            assert!(r4.0 == 200);
            assert!(has(&r4.1, "\"value\":355"));
            assert!(has(&r4.1, "\"run\":\"2026-10-19T12:00:00Z\""));

            // ---- a valid time no run covers -> 404 no_run_covers ----------
            let q5 = "/v1/point?model=frac&param=TMP&level=isobaric:850&lat=9&lon=351.5&valid=2026-10-19T13:15:00Z";
            let r5 = rt.block_on(fetch(svc.clone(), q5.to_string()));
            assert!(r5.0 == 404);
            assert!(has(&r5.1, "no_run_covers"));

            // ---- accumulation guard on a FRACTIONAL statistical range -----
            // APCP@surface spans 12:30Z->13:00Z at fc 30 min: asking for 12:45Z
            // must be refused (422), asking for the exact valid time answers 80.
            let q6 = "/v1/point?model=frac&param=APCP&level=surface&lat=9&lon=351.5&valid=2026-10-19T12:45:00Z";
            let r6 = rt.block_on(fetch(svc.clone(), q6.to_string()));
            assert!(r6.0 == 422);
            assert!(has(&r6.1, "stat_time_guard"));
            let q7 = "/v1/point?model=frac&param=APCP&level=surface&lat=9&lon=351.5&valid=2026-10-19T12:30:00Z";
            let r7 = rt.block_on(fetch(svc.clone(), q7.to_string()));
            assert!(r7.0 == 200);
            assert!(has(&r7.1, "\"value\":80"));

            // ---- time_interp=none refuses a NON-stored fractional fh -----
            let q8 = "/v1/point?model=frac&param=TMP&level=isobaric:850&lat=9&lon=351.5&valid=2026-10-19T12:45:00Z&time_interp=none";
            let r8 = rt.block_on(fetch(svc.clone(), q8.to_string()));
            assert!(r8.0 == 422);
            assert!(has(&r8.1, "time_exact_only"));

            // ---- inventory shows both runs with their fractional hours ----
            let q9 = "/v1/inventory?model=frac";
            let r9 = rt.block_on(fetch(svc.clone(), q9.to_string()));
            assert!(r9.0 == 200);
            assert!(has(&r9.1, "\"forecast_hours\":[0,0.5,1]"));
            assert!(has(&r9.1, "2026-10-19T12:30:00Z"));

            // ---- /metrics exposes the skip counter (spec §7) -------------
            let q10 = "/metrics";
            let r10 = rt.block_on(fetch(svc.clone(), q10.to_string()));
            assert!(r10.0 == 200);
            assert!(has(&r10.1, "wxpoint_skipped_messages_total 2"));
            assert!(has(&r10.1, "wxpoint_ingest_errors_total 0"));
        }
    }
    rm_tree(&root);
}

#[test]
fn robust_model_answers_despite_skipped_messages() {
    // cargo runs test functions on separate threads, so this needs its OWN
    // tmp root and its own pipeline instance.
    let root2 = "/tmp/opencode/wxpoint-robust-http".to_string();
    let cx_o = serve_setup(root2.to_string());
    match cx_o {
        None => { assert!(false); return; }
        Some(cx) => {
            let _keep = cx.p;
            let rt = cx.rt;
            let svc = cx.svc;
            // the two surviving point-queryable fields of robust_small answer
            // 345 at the probe node even though TWO messages were skipped.
            let q1 = "/v1/point?model=robust&param=TMP&level=isobaric:850&lat=9&lon=351.5&valid=2026-10-19T12:00:00Z";
            let r1 = rt.block_on(fetch(svc.clone(), q1.to_string()));
            assert!(r1.0 == 200);
            assert!(has(&r1.1, "\"value\":345"));
            let q2 = "/v1/point?model=robust&param=TMP&level=height_above_ground:2&lat=9&lon=351.5&valid=2026-10-19T12:00:00Z";
            let r2 = rt.block_on(fetch(svc.clone(), q2.to_string()));
            assert!(r2.0 == 200);
            assert!(has(&r2.1, "\"value\":345"));
            // the 850-800 LAYER message never entered the index: 825 has no
            // bracketing pair to interpolate between.
            let q3 = "/v1/point?model=robust&param=TMP&level=isobaric:825&lat=9&lon=351.5&valid=2026-10-19T12:00:00Z";
            let r3 = rt.block_on(fetch(svc.clone(), q3.to_string()));
            assert!(r3.0 == 422);
            assert!(has(&r3.1, "level_out_of_range"));
        }
    }
}
