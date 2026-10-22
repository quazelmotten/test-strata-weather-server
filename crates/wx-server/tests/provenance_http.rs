// V1.3 phase E2 DEFECT GATE — PER-RESULT run provenance + source_winds_grid.
//
// THE DEFECT (AUDIT-confirmed): answer_all echoed ONE top-level "run" taken
// from entries_at(&eb, 0).run_epoch while every param resolves its run
// INDEPENDENTLY. With two indexed runs where the newest covers only one of
// the requested params, the response echoed the NEWER run even though another
// result came from an older one — provenance lost or silently wrong, and the
// per-result objects carried no run at all.
//
// THE FIX: every result object now carries its own "run" (ISO-8601 Z, same
// format as the top level). The top-level "run" is echoed when ALL results
// share one run (back compatible) and emitted as JSON null when they differ.
//
// FIXTURES: gfs_small.grib2 (run 2026-10-19T12Z: TMP/UGRD/VGRD fh 0/6/12 on
// the 41x21 grid, node (10 N, 350 E) = base 250 + table offsets) plus
// prov_new_small.grib2 (NEWER run 2026-10-19T18Z carrying ONLY TMP@1000 hPa
// fh 0, node value 300). At valid 18Z TMP answers 300 from the NEW run while
// UGRD answers 1310 (fh 6) from the OLD one — mixed provenance in ONE
// response. Pre-E2 this response echoed "2026-10-19T18:00:00Z" for BOTH and
// named no per-result run: the per-result assertions below simply did not
// exist pre-fix (the strings were absent), which IS the discrimination.
//
// Also pinned here: /v1/models reports the SOURCE component convention as
// "source_winds_grid" (renamed key; stored U/V are ALWAYS earth-relative).

use wx_server::httpapi::{ServerState, build_router};
use wx_server::stats::new_stats;
use wx_ingest::pipeline::Pipeline;
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

use wx_ingest::discovery::path_to_str;

#[allow(clippy::needless_return)]
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
                                Err(_) => { assert!(false); return; }
                                Ok(_) => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn write_text(path: &str, text: String) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(text.as_bytes());
            match n {
                Err(_) => { assert!(false); return; }
                Ok(_) => {}
            }
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

#[allow(clippy::needless_return)]
fn count_occ(body: &[u8], pat: &str) -> usize {
    let pb = pat.as_bytes();
    if pb.len() > body.len() {
        return 0;
    }
    let mut n = 0usize;
    for s in 0 .. (body.len() - pb.len() + 1) {
        let mut ok = true;
        for k in 0 .. pb.len() {
            if body[s + k] != pb[k] {
                ok = false;
                break;
            }
        }
        if ok {
            n += 1;
        }
    }
    return n;
}

use wx_ingest::state::join_path;

// ONE model, TWO runs in the same ingest dir (gfs_small 12Z + prov_new 18Z).
#[allow(clippy::needless_return)]
fn prov_cfg(root: &str) -> String {
    let ing = join_path(&root, "ingest-gfs");
    let data = join_path(&root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:1000\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"isobaric:1000\"]\n";
    return t;
}

struct Ctx {
    pub p: Pipeline,
    pub svc: RouterIntoService<Body>,
    pub rt: Runtime,
}

#[allow(clippy::needless_return)]
fn serve_setup(root: String) -> Option<Ctx> {
    rm_tree(&root);
    let ing = join_path(&root, "ingest-gfs");
    let _c1 = create_dir_all(&ing);
    copy_file(fixture("gfs_small.grib2"), join_path(&ing, "gfs_small.grib2"));
    copy_file(fixture("prov_new_small.grib2"), join_path(&ing, "prov_new_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_text(&cfg_path, prov_cfg(&root));

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_e) => { assert!(false); return None; }
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
    let resp = ms.call(req).await;
    match resp {
        Err(_) => { return (0, Vec::<u8>::new()); }
        Ok(rx) => {
            let code = rx.status().as_u16();
            let bd = rx.into_body().collect().await;
            match bd {
                Err(_) => { return (code, Vec::<u8>::new()); }
                Ok(collected) => {
                    let buf = collected.to_bytes();
                    let mut v: Vec<u8> = Vec::new();
                    for k in 0 .. buf.len() {
                        v.push(buf[k]);
                    }
                    return (code, v);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (a) MIXED provenance: TMP from the NEW run, UGRD from the OLD one
// ---------------------------------------------------------------------------

#[test]
fn mixed_run_provenance_names_each_results_run() {
    let root = "/tmp/opencode/wxpoint-provenance-mixed".to_string();
    let ct_r = serve_setup(root);
    let ct = match ct_r {
        None => { assert!(false); return; }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let q = "/v1/point?model=gfs&param=TMP,UGRD&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T18:00:00Z";
    let r = rt.block_on(fetch(ct.svc.clone(), q.to_string()));
    assert!(r.0 == 200);
    // TMP answers from the NEWER run (node value 300), UGRD from the OLDER
    // run's fh 6 (250 + 1000 + 60 = 1310).
    assert!(has(&r.1, "\"value\":300"));
    assert!(has(&r.1, "\"value\":1310"));
    // PER-RESULT provenance: each hour named exactly once, on its result.
    assert!(count_occ(&r.1, "\"run\":\"2026-10-19T18:00:00Z\"") == 1);
    assert!(count_occ(&r.1, "\"run\":\"2026-10-19T12:00:00Z\"") == 1);
    // TOP LEVEL null when they differ (pre-E2: echoed 18Z for BOTH results —
    // a wrong echo for UGRD — and no per-result run existed at all).
    assert!(count_occ(&r.1, "\"run\":null") == 1);
    assert!(!has(&r.1, "\"run\":\"2026-10-19T18:00:00Z\",\"valid\""));
}

// ---------------------------------------------------------------------------
// (b) COMMON run: top-level echo unchanged (back compatible) + per-result added
// ---------------------------------------------------------------------------

#[test]
fn common_run_echo_unchanged_and_per_result_added() {
    let root = "/tmp/opencode/wxpoint-provenance-common".to_string();
    let ct_r = serve_setup(root);
    let ct = match ct_r {
        None => { assert!(false); return; }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    // valid 15Z = fh 3 of the 12Z run; the 18Z run cannot bracket it, so
    // BOTH params answer from 12Z: TMP lerp(250,310)=280 (increments are
    // fh*10), UGRD lerp(1250,1310)=1280.
    let q = "/v1/point?model=gfs&param=TMP,UGRD&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T15:00:00Z";
    let r = rt.block_on(fetch(ct.svc.clone(), q.to_string()));
    assert!(r.0 == 200);
    assert!(has(&r.1, "\"value\":280"));
    assert!(has(&r.1, "\"value\":1280"));
    // top-level echo + TWO per-result echoes = exactly three occurrences.
    assert!(count_occ(&r.1, "\"run\":\"2026-10-19T12:00:00Z\"") == 3);
    assert!(!has(&r.1, "\"run\":null"));
}

// ---------------------------------------------------------------------------
// (c) SINGLE param from the new run: back-compatible echo, one per-result too
// ---------------------------------------------------------------------------

#[test]
fn single_param_from_new_run_back_compat_echo() {
    let root = "/tmp/opencode/wxpoint-provenance-single".to_string();
    let ct_r = serve_setup(root);
    let ct = match ct_r {
        None => { assert!(false); return; }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let q = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T18:00:00Z";
    let r = rt.block_on(fetch(ct.svc, q.to_string()));
    assert!(r.0 == 200);
    assert!(has(&r.1, "\"value\":300"));
    assert!(count_occ(&r.1, "\"run\":\"2026-10-19T18:00:00Z\"") == 2);
    assert!(!has(&r.1, "\"run\":null"));
}

// ---------------------------------------------------------------------------
// (d) /v1/models: renamed source-convention key
// ---------------------------------------------------------------------------

#[test]
fn models_reports_source_winds_grid_not_winds_grid() {
    let root = "/tmp/opencode/wxpoint-provenance-models".to_string();
    let ct_r = serve_setup(root);
    let ct = match ct_r {
        None => { assert!(false); return; }
        Some(x) => { x }
    };
    let rt = &ct.rt;
    let r = rt.block_on(fetch(ct.svc, "/v1/models".to_string()));
    assert!(r.0 == 200);
    // The grid block reports how the SOURCE messages were encoded...
    assert!(has(&r.1, "\"source_winds_grid\":"));
    // ...and the old bare key is GONE (the pattern starts with a quote, so
    // "source_winds_grid" does not match it).
    assert!(!has(&r.1, "\"winds_grid\":"));
}
