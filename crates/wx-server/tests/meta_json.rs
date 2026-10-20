// Milestone-5 regression: /v1/models and /v1/inventory previously emitted
// INVALID JSON (object items lacked '{' and loops double-pushed separator
// commas, e.g. [,"model" ... },,"param"). Both endpoints are now parsed with
// serde_json end-to-end through the real router, and the LCC model must be
// present in the inventory (warm-start rebuild guard fix pair).

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
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;

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

#[allow(clippy::needless_return)]
fn cfg_text(root: &str) -> String {
    let ing_gfs = join_path(&root, "ingest-gfs");
    let ing_hrr = join_path(&root, "ingest-hrrr");
    let data = join_path(&root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_gfs);
    let t = t + "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n\n[[models]]\nname = \"hrrr\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_hrr);
    let t = t + "\"\nfile_glob = \"hrrr_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n";
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
    let ing_gfs = join_path(&root, "ingest-gfs");
    let ing_hrr = join_path(&root, "ingest-hrrr");
    let _c1 = create_dir_all(&ing_gfs);
    let _c2 = create_dir_all(&ing_hrr);
    copy_file(fixture("gfs_small.grib2"), join_path(&ing_gfs, "gfs_small.grib2"));
    copy_file(fixture("hrrr_small.grib2"), join_path(&ing_hrr, "hrrr_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_text(&cfg_path, cfg_text(&root));

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

#[allow(clippy::needless_return)]
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

#[allow(clippy::needless_return)]
fn body_str(t: (u16, Vec<u8>)) -> String {
    let mut s = "".to_string();
    for k in 0 .. t.1.len() {
        s = format!("{0}{1}", s, (t.1[k] as char).to_string());
    }
    return s;
}

#[test]
fn meta_endpoints_parse_as_json() {
    let root = "/tmp/opencode/wxpoint-meta".to_string();
    let cx_o = serve_setup(root.to_string());
    match cx_o {
        None => { assert!(false); return; }
        Some(cx) => {
            let _keep = cx.p;
            let rt = cx.rt;
            let svc = cx.svc;

            let urls: Vec<String> = vec![
                "/v1/models".to_string(),
                "/v1/inventory".to_string(),
                "/v1/inventory?model=hrrr".to_string(),
            ];
            for u in &urls {
                let r = rt.block_on(fetch(svc.clone(), u.to_string()));
                assert!(r.0 == 200);
                let pv = serde_json::from_slice::<serde_json::Value>(&r.1);
                match pv {
                    Err(_e) => { assert!(false); }
                    Ok(_) => {}
                }
            }
            // LCC model visible in both meta views.
            let rm = rt.block_on(fetch(svc.clone(), "/v1/models".to_string()));
            let sm = body_str(rm);
            assert!(sm.contains("\"lcc\""));
            let ri = rt.block_on(fetch(svc.clone(), "/v1/inventory?model=hrrr".to_string()));
            let si = body_str(ri);
            assert!(si.contains("\"model\":\"hrrr\""));
            assert!(si.contains("\"forecast_hours\":[0,3]"));
        }
    }
}
