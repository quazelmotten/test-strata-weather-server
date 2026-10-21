// Spec §9.9 at the HTTP level: a dedicated ingest thread churns the arc-swap
// index (scan rounds discovering byte-patched run twins, forced republishes,
// retention_runs=1 evictions) while 8 reader threads hammer /v1/point through
// the FULL axum stack. Invariants checked on every response: never a 5xx, a 200
// always carries the analytic value 341 in a well-formed body (no torn/partial
// JSON visible to any reader), and after ingest settles the queries answer 341
// deterministically. Two non-200 statuses are legitimate mid-churn and counted
// separately: 404 (no indexed run covers the valid time yet) and 422
// level_out_of_range -- the pipeline publishes field-by-field (spec §4.5
// incremental visibility), so a run can briefly hold TMP@1000 without TMP@850/700,
// leaving a NON-STORED level like 925 unbracketed; spec §7 makes that a 422
// rather than an extrapolated number.

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
use std::sync::Mutex;
use std::thread::spawn;
use libc::usleep;
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
fn read_bytes(path: &str) -> Vec<u8> {
    let f = File::open(&path);
    match f {
        Err(_) => { return Vec::<u8>::new(); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return Vec::<u8>::new(); }
                Ok(region) => {
                    let mut copy: Vec<u8> = Vec::<u8>::new();
                    for k in 0 .. region.len() {
                        copy.push(region[k]);
                    }
                    return copy;
                }
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(bytes);
            match n {
                Err(_) => { assert!(false); }
                Ok(_) => {}
            }
        }
    }
}

// sec1 date-stamp patcher (same verified pattern as wx-ingest retention.rs):
// day byte at offset 10 of a 12-byte pattern occurring 39 times.
#[allow(clippy::needless_return)]
fn patch_day(src: &[u8], day: u8) -> Vec<u8> {
    let mut out = src.to_vec();
    let pat: [u8; 12] = [0x00, 0x07, 0x00, 0x00, 0x7D, 0x00, 0x01, 0x07, 0xEA, 0x0A, 0x13, 0x0C];
    let n = out.len();
    let mut hits = 0;
    for k in 0 .. (n - 12 + 1) {
        let mut ok = true;
        for j in 0 .. 12 {
            if out[k + j] != pat[j] {
                ok = false;
                break;
            }
        }
        if ok {
            out[k + 10] = day;
            hits += 1;
        }
    }
    assert!(hits == 39);
    return out;
}

// gfs-only config with retention_runs=1 so every new run evicts its
// predecessor right after publish (maximum index churn per round).
#[allow(clippy::needless_return)]
fn cfg_gfs(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 1\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n";
    return t;
}

#[allow(clippy::needless_return)]
fn unwrap_stats(cnt: Arc<Mutex<Vec<i64>>>) -> Vec<i64> {
    let m = cnt.lock();
    match m {
        Err(_e) => { assert!(false); return Vec::<i64>::new(); }
        Ok(v) => { return v.to_vec(); }
    }
}

#[allow(clippy::needless_return)]
fn bump(cnt: Arc<Mutex<Vec<i64>>>, idx: usize) {
    let m = cnt.lock();
    match m {
        Err(_e) => { assert!(false); return; }
        Ok(v) => {
            let mut w = v;
            w[idx] = w[idx] + 1;
        }
    }
}

#[allow(clippy::needless_return)]
fn set_slot(cnt: Arc<Mutex<Vec<i64>>>, idx: usize, val: i64) {
    let m = cnt.lock();
    match m {
        Err(_e) => { assert!(false); return; }
        Ok(v) => {
            let mut w = v;
            w[idx] = val;
        }
    }
}

#[allow(clippy::needless_return)]
fn flag_get(cnt: Arc<Mutex<Vec<i64>>>, idx: usize) -> i64 {
    let m = cnt.lock();
    match m {
        Err(_e) => { return 0; }
        Ok(v) => { return v[idx]; }
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

// Drive one GET through the real router (same helper shape as point_http.rs).
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
fn ingest_churn_vs_queries_http() {
    let root = "/tmp/opencode/wxpoint-conc-http".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    let base = read_bytes(&fixture("gfs_small.grib2"));
    assert!(base.len() > 0);
    write_bytes(&join_path(&ing, "gfs_a.grib2"), &base);
    write_bytes(&join_path(&ing, "gfs_b.grib2"), &patch_day(&base, 20));

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg_gfs(&data, &ing).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_e) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    let _r1 = p.scan_round();
    let r2 = p.scan_round();
    assert!(r2 == 2);
    assert!(p.counters.errors == 0);

    let stats = new_stats();
    let st = ServerState {
        index: p.shared.clone(),
        stats: stats.clone(),
        models_cfg: Arc::new(p.models.clone()),
    };
    let app = build_router(st);
    let svc = app.into_service();

    // counters: [ok, bad, total, finished_readers, ingested_files, ing_errors,
    // ingest_done, transient_422]
    let cnt = Arc::new(Mutex::new(Vec::<i64>::new()));
    {
        let m0 = cnt.lock();
        match m0 {
            Err(_e) => { assert!(false); return; }
            Ok(mut v0) => {
                for _z in 0 .. 8 {
                    v0.push(0);
                }
            }
        }
    }
    // done flag: set by main after readers finish; ingest loop exits on it.
    let done = Arc::new(Mutex::new(Vec::<i64>::new()));
    {
        let m0 = done.lock();
        match m0 {
            Err(_e) => { assert!(false); return; }
            Ok(mut v0) => { v0.push(0); }
        }
    }

    // Ingest churn thread: deliver twins day 21/22/23 progressively, scan,
    // force a republish every round (arc-swap swap even without new data),
    // retention evicts the previous run each time.
    let base_c = base.clone();
    let ing_c = ing.clone();
    let cnt_i = cnt.clone();
    let done_i = done.clone();
    let _th_ingest = spawn(move || {
        let mut next_day: u8 = 21;
        loop {
            if next_day <= 23 {
                let nm = format!("{0}{1}{2}", "gfs_t", next_day, ".grib2");
                write_bytes(&join_path(&ing_c, &nm), &patch_day(&base_c, next_day));
                next_day = next_day + 1;
            }
            let _ = p.scan_round();
            p.publish();
            unsafe { usleep(30000); }
            let ing_n = p.counters.ingested_files;
            if flag_get(done_i.clone(), 0) != 0 && next_day > 23 && ing_n >= 5 {
                break;
            }
        }
        set_slot(cnt_i.clone(), 4, p.counters.ingested_files as i64);
        set_slot(cnt_i.clone(), 5, p.counters.errors as i64);
        set_slot(cnt_i, 6, 1);
    });

    // 8 reader threads hammer /v1/point through the router. Each owns its own
    // tokio runtime (Runtime is not Clone in this toolchain).
    let nreaders: i64 = 8;
    let nqueries: i64 = 1500;
    let url = "/v1/point?model=gfs&param=TMP&level=isobaric:925&lat=9.0&lon=351.5&valid=2026-10-23T15:00:00Z";
    for _t in 0 .. nreaders {
        let svc_c = svc.clone();
        let cnt_c = cnt.clone();
        let _h = spawn(move || {
            let rt_r = Runtime::new();
            let rt = match rt_r {
                Err(_e) => { bump(cnt_c.clone(), 1); bump(cnt_c.clone(), 3); return; }
                Ok(rtx) => { rtx }
            };
            let mut nbad: i64 = 0;
            for _k in 0 .. nqueries {
                let out = rt.block_on(fetch(svc_c.clone(), url.to_string()));
                bump(cnt_c.clone(), 2);
                if out.0 == 200 {
                    if bytes_contains(&out.1, b"\"value\":341") && bytes_contains(&out.1, b"]}") {
                        bump(cnt_c.clone(), 0);
                    } else {
                        bump(cnt_c.clone(), 1);
                        if nbad < 2 {
                            println!("BAD 200 body len={0} bytes={1:?}", out.1.len(), &out.1[..]);
                            nbad += 1;
                        }
                    }
                } else if out.0 == 404 {
                    bump(cnt_c.clone(), 0);
                } else if out.0 == 422 && bytes_contains(&out.1, b"level_out_of_range") {
                    // Documented transient: ingest publishes field-by-field
                    // (pipeline.rs "publish after every field write", spec §4.5
                    // incremental visibility), so for part of a file's delivery a
                    // run can hold TMP@1000 but not yet TMP@850/700. A request for
                    // a NON-STORED level (925) then has no bracketing pair, and
                    // §7 says that is a 422, never extrapolation. Values themselves
                    // are always complete: this is not torn data.
                    bump(cnt_c.clone(), 7);
                } else {
                    bump(cnt_c.clone(), 1);
                    if nbad < 2 {
                        println!("BAD status {0} len={1} bytes={2:?}", out.0, out.1.len(), &out.1[..]);
                        nbad += 1;
                    }
                }
            }
            bump(cnt_c.clone(), 3);
        });
    }
    // wait until all readers finished (slot 3 counts finished readers).
    while flag_get(cnt.clone(), 3) < nreaders {
        unsafe { usleep(20000); }
    }

    // signal the ingest loop to exit; wait for it to record its final state.
    {
        let dm = done.lock();
        match dm {
            Err(_e) => { assert!(false); return; }
            Ok(v) => {
                let mut w = v;
                w[0] = 1;
            }
        }
    }
    while flag_get(cnt.clone(), 6) == 0 {
        unsafe { usleep(20000); }
    }

    let fin = unwrap_stats(cnt.clone());
    // [ok, bad, total, finished_readers, ingested_files, ing_errors, ingest_done,
    // transient_422]
    println!("counters {0:?}", fin);
    assert!(fin[4] == 5); // gfs_a/b + twins t21/t22/t23 all ingested
    assert!(fin[5] == 0);
    assert!(fin[2] == nreaders * nqueries);
    assert!(fin[1] == 0); // never a 5xx, never a torn/partial 200 body, never an
    // unexpected status code

    // Deterministic final state: day-23 run indexed, answers 341.
    let rt_r = Runtime::new();
    let rt = match rt_r {
        Err(_e) => { assert!(false); return; }
        Ok(rtx) => { rtx }
    };
    let out = rt.block_on(fetch(svc.clone(), url.to_string()));
    assert!(out.0 == 200);
    assert!(bytes_contains(&out.1, b"\"value\":341"));

    // Settled state is stable: the transient 422 window is gone once every field
    // of every delivered file is indexed, so 100 repeats must all answer 200/341.
    let mut ksettled: i64 = 0;
    while ksettled < 100 {
        let o2 = rt.block_on(fetch(svc.clone(), url.to_string()));
        assert!(o2.0 == 200);
        assert!(bytes_contains(&o2.1, b"\"value\":341"));
        ksettled += 1;
    }

    rm_tree(&root);
}

