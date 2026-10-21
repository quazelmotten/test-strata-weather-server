// V1.2 phase C gate #11: STRICT ISO DATES AT THE HTTP BOUNDARY (spec §7 error
// contract). The audit found `parse_iso_z` validated the DAY as `d < 1 || d > 31`,
// so "2026-02-31" was ACCEPTED and silently NORMALIZED into early MARCH by the
// Hinnant day formula: a typo'd query answered with the WRONG DAY'S data instead
// of failing. Days are now validated against a per-month table WITH the leap-year
// rule, shared with the manifest parser (wx-core::tables), and an impossible date
// is a 400 `bad_time`.

use wx_server::httpapi::{ServerState, build_router};
use wx_server::stats::new_stats;
use wx_server::timeparse::{parse_iso_z, format_iso_z};
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
                        Err(_) => { assert!(false); }
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
fn cfg_text(root: &str) -> String {
    let ing = join_path(&root, "ingest-gfs");
    let data = join_path(&root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
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
    let cfg_path = join_path(&root, "config.toml");
    write_text(&cfg_path, cfg_text(&root));

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_e) => { assert!(false); return None; }
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
fn the_parser_validates_days_against_the_real_calendar_including_leap_years() {
    // leap years: divisible by 4, except centuries unless divisible by 400
    assert!(parse_iso_z("2024-02-29T00:00:00Z") != None);
    assert!(parse_iso_z("2000-02-29T00:00:00Z") != None);
    assert!(parse_iso_z("2026-02-29T00:00:00Z") == None);
    assert!(parse_iso_z("2100-02-29T00:00:00Z") == None);
    // impossible days
    assert!(parse_iso_z("2026-02-30T00:00:00Z") == None);
    assert!(parse_iso_z("2026-02-31T00:00:00Z") == None);
    assert!(parse_iso_z("2026-04-31T00:00:00Z") == None);
    assert!(parse_iso_z("2026-11-31T00:00:00Z") == None);
    assert!(parse_iso_z("2026-01-00T00:00:00Z") == None);
    assert!(parse_iso_z("2026-13-01T00:00:00Z") == None);
    // last valid day of a 31-day month and of February in a leap year
    assert!(parse_iso_z("2026-12-31T23:59:59Z") != None);
    assert!(parse_iso_z("2024-02-29T23:59:59Z") != None);

    // NO SILENT ROLLOVER: Feb 28 + 1 DAY is exactly Mar 1, and the pre-fix
    // behaviour would have made "2026-02-31" equal "2026-03-03".
    let feb28 = match parse_iso_z("2026-02-28T00:00:00Z") {
        None => { assert!(false); return; }
        Some(e) => { e }
    };
    let mar1 = match parse_iso_z("2026-03-01T00:00:00Z") {
        None => { assert!(false); return; }
        Some(e) => { e }
    };
    assert!(mar1 - feb28 == 86400);
    // round trip still exact for every accepted value above
    assert!(format_iso_z(mar1) == "2026-03-01T00:00:00Z".to_string());
}

#[test]
fn an_impossible_date_answers_400_bad_time_instead_of_answering_wrong_data() {
    let root = "/tmp/opencode/wxpoint-dates".to_string();
    let cx_o = serve_setup(root.to_string());
    match cx_o {
        None => { assert!(false); return; }
        Some(cx) => {
            let _keep = cx.p;
            let rt = cx.rt;
            let svc = cx.svc;

            // CONTROL: the same query with a REAL date answers 200
            let ok = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-02-28T12:00:00Z";
            let r_ok = rt.block_on(fetch(svc.clone(), ok.to_string()));
            // no run covers that valid time -> 404, but NOT a bad_time rejection
            assert!(r_ok.0 == 404);
            assert!(!bytes_contains(&r_ok.1, "bad_time".as_bytes()));

            // THE BUG: February 31 used to be ACCEPTED and normalized to March 3
            let bad = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-02-31T12:00:00Z";
            let r_bad = rt.block_on(fetch(svc.clone(), bad.to_string()));
            assert!(r_bad.0 == 400);
            assert!(bytes_contains(&r_bad.1, "\"error\":\"bad_time\"".as_bytes()));

            // non-leap Feb 29 in 2026 is rejected the SAME way
            let nl = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-02-29T12:00:00Z";
            let r_nl = rt.block_on(fetch(svc.clone(), nl.to_string()));
            assert!(r_nl.0 == 400);
            assert!(bytes_contains(&r_nl.1, "bad_time".as_bytes()));

            // and a REAL date inside the fixture's own run still answers 200
            let good = "/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=10&lon=350&valid=2026-10-19T12:00:00Z";
            let r_good = rt.block_on(fetch(svc.clone(), good.to_string()));
            assert!(r_good.0 == 200);
            rm_tree(&root);
        }
    }
}
