// V1.2 phase B0 HTTP-LAYER GATE for the per-slot vertical bracket metadata (spec
// §7 response shape). Drives the REAL axum router over a hand-built index whose
// two forecast hours carry DIFFERENT level stacks, and pins the ADDITIVE JSON
// extension chosen for the fix:
//
//   "vertical":{"method":"linear","levels":[900,800],
//               "levels_per_time":[[900,800],[950,850]]}
//
// * "levels" stays the EARLIER forecast hour's pair -> every pre-existing client
//   keeps parsing the documented spec §7 shape (back compatible);
// * "levels_per_time" appears ONLY when the two TIME slots bracket differently,
//   so uniform-stack output stays BYTE-IDENTICAL to V1.1 (also asserted here).
//
// Values follow the same analytic convention as tests/slot_levels.rs:
// V(L,F,j,i) = L*100 + F*10 + (j*10+i); at cell centre (0.5,0.5) the horizontal
// step yields L*100 + F*10 + 5.5, so the correct composition for level 875 at
// fh 3h is mean(87505.5, 87565.5) = 87535.5 (the pre-fix code returned 85660.5).

use wx_server::httpapi::{ServerState, build_router};
use wx_server::stats::new_stats;
use wx_ingest::index::{IdxField, IdxFh, IdxFieldEntry, IdxModel, IdxRun, IndexSnapshot, SharedIndex};
use wx_core::grid::{GridDef, GridRegular};
use wx_core::storage::{BinHeader, StatMeta, MmapField, open_field, write_bin_atomic};
use wx_core::tables::{lookup_level_type};
use axum::routing::RouterIntoService;
use axum::body::Body;
use http::request::Builder;
use http_body_util::BodyExt;
use tower_service::Service;
use tokio::runtime::Runtime;
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::remove_file;
use std::fs::remove_dir;

const NX: u32 = 4;
const NY: u32 = 3;
const EPOCH: i64 = 1760889600; // 2025-10-19T16:00:00Z

#[allow(clippy::needless_return)]
fn rm_tree(dir: &str) {
    match read_dir(&dir) {
        Err(_) => {}
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let p = path_str(&de.path());
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

use std::path::Path;
pub fn path_str(p: &Path) -> String {
    match p.to_str() {
        None => { return "".to_string(); }
        Some(s) => { return s.to_string(); }
    }
}

/// Analytic planes for one forecast hour (see header comment).
#[allow(clippy::needless_return)]
fn write_field_file(dir: String, name: String, levels: &[f64], fh_hours: f64) {
    let mut planes: Vec<Vec<f32>> = Vec::<Vec<f32>>::new();
    for lv in levels {
        let mut plane: Vec<f32> = Vec::<f32>::new();
        for j in 0 .. (NY as usize) {
            for i in 0 .. (NX as usize) {
                let v = lv * 100.0 + fh_hours * 10.0 + ((j as f64) * 10.0 + (i as f64));
                let _p = plane.push(v as f32);
            }
        }
        let _m = planes.push(plane);
    }
    let mut refs: Vec<&[f32]> = Vec::<&[f32]>::new();
    for pl in &planes {
        let _r = refs.push(&pl[..]);
    }
    let mut lv32: Vec<f32> = Vec::<f32>::new();
    for lv in levels {
        let _z = lv32.push(*lv as f32);
    }
    let hdr = BinHeader {
        nx: NX,
        ny: NY,
        levels: lv32,
        scan_ns: false,
        winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
    let wr = write_bin_atomic(format!("{0}/{1}", dir, name), &hdr, &refs);
    match wr {
        Err(_e) => { assert!(false); return; }
        Ok(_) => {}
    }
}

struct Ctx {
    pub svc: RouterIntoService<Body>,
    pub rt: Runtime,
}

// Hand-built index: ONE run, ONE field (TMP / isobaric) whose two forecast hours
// carry DIFFERENT level stacks -- exactly the partial-ingest state that exposed
// the bug. No GRIB files involved: this is a pure HTTP-shape gate.
#[allow(clippy::needless_return)]
fn serve_snapshot(root: String, a_levels: &[f64], b_levels: &[f64]) -> Option<Ctx> {
    let dir = root.to_string();
    rm_tree(&dir);
    let _c = create_dir_all(&dir);
    write_field_file(dir.to_string(), "f000.bin".to_string(), a_levels, 0.0);
    write_field_file(dir.to_string(), "f006.bin".to_string(), b_levels, 6.0);
    let ma = open_field(format!("{0}/f000.bin", dir));
    let mb = open_field(format!("{0}/f006.bin", dir));
    match ma {
        Err(_) => { return None; }
        Ok(mfa) => {
            match mb {
                Err(_) => { return None; }
                Ok(mfb) => {
                    let entry = IdxFieldEntry {
                        param_key: "TMP".to_string(),
                        level_code: iso_or_zero(),
                        fd: IdxField {
                            units: "K".to_string(),
                            stat_processed: false,
                            stat_kind: "instantaneous".to_string(),
                            fhs: [mk_fh(Arc::new(mfa), 0), mk_fh(Arc::new(mfb), 21600)].to_vec(),
                        },
                    };
                    let grid = GridDef::Regular(GridRegular {
                        nx: NX,
                        ny: NY,
                        lon1: 0.0,
                        lat1: 0.0,
                        dlon: 1.0,
                        dlat: 1.0,
                        scan_mode: 0x40,
                        winds_grid: false,
                    });
                    let run = IdxRun {
                        epoch_secs: EPOCH,
                        dir_name: "2025101916".to_string(),
                        grid: Arc::new(grid),
                        fields: [entry].to_vec(),
                    };
                    let snap = IndexSnapshot {
                        models: [IdxModel { name: "gfs".to_string(), runs: [run].to_vec() }].to_vec(),
                    };
                    let shared = Arc::new(SharedIndex { store: ArcSwap::from_pointee(snap) });
                    let st = ServerState {
                        index: shared,
                        stats: new_stats(),
                        models_cfg: Arc::new(Vec::<wx_ingest::config::ResolvedModel>::new()),
                    };
                    let app = build_router(st);
                    let svc = app.into_service();
                    let rt_r = Runtime::new();
                    match rt_r {
                        Err(_e) => { return None; }
                        Ok(rt) => { return Some(Ctx { svc: svc, rt: rt }); }
                    }
                }
            }
        }
    }
}

pub fn iso_or_zero() -> u8 {
    match lookup_level_type("isobaric") {
        None => { return 0; }
        Some(c) => { return c; }
    }
}

pub fn mk_fh(mf: Arc<MmapField>, fh_secs: i64) -> IdxFh {
    return IdxFh {
        fh_secs: fh_secs,
        field: mf.clone(),
        stat_has_range: false,
        stat_begin_secs: 0,
        stat_end_secs: 0,
        stat_step_secs: 0,
    };
}

// Drive one GET through the real router; returns (status, body bytes).
async fn fetch(svc: RouterIntoService<Body>, url: String) -> (u16, Vec<u8>) {
    let rb = Builder::new().method("GET").uri(url).body(Body::empty());
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
    let code = resp.status().as_u16();
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
fn has(body: &[u8], pat: &str) -> bool {
    let pat_b = pat.as_bytes();
    if pat_b.len() > body.len() {
        return false;
    }
    for s in 0 .. (body.len() - pat_b.len() + 1) {
        let mut ok = true;
        for k in 0 .. pat_b.len() {
            if body[s + k] != pat_b[k] {
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

#[test]
fn point_json_reports_both_brackets_when_stacks_differ() {
    let root = "/tmp/opencode/wxpoint-json-brackets";
    let a_levels: [f64; 4] = [1000.0, 900.0, 800.0, 700.0];
    let b_levels: [f64; 4] = [950.0, 850.0, 800.0, 700.0];
    let cx_o = serve_snapshot(root.to_string(), &a_levels[..], &b_levels[..]);
    match cx_o {
        None => { assert!(false); return; }
        Some(cx) => {
            let rt = cx.rt;
            // mid-level 875 hPa + mid-time 3Z (fh offset 10800 s) at cell centre
            let q = "/v1/point?model=gfs&param=TMP&level=isobaric:875&lat=0.5&lon=0.5&valid=2025-10-19T19:00:00Z";
            let r = rt.block_on(fetch(cx.svc.clone(), q.to_string()));
            assert!(r.0 == 200);
            // correct per-slot composition value (pre-fix would be 85660.5)
            assert!(has(&r.1, "\"value\":87535.5"));
            assert!(!has(&r.1, "85660.5"));
            // additive JSON extension: BOTH brackets, earlier slot first
            assert!(has(&r.1, "\"vertical\":{\"method\":\"linear\",\"levels\":[900,800],\"levels_per_time\":[[900,800],[950,850]]}"));
            assert!(has(&r.1, "\"time\":{\"method\":\"linear\",\"forecast_hours\":[0,6]}"));
            assert!(has(&r.1, "\"horizontal\":\"bilinear\""));
            rm_tree(&root);
        }
    }
}

#[test]
fn point_json_omits_levels_per_time_when_brackets_agree() {
    let root = "/tmp/opencode/wxpoint-json-brackets-same";
    let lv: [f64; 4] = [1000.0, 900.0, 800.0, 700.0];
    let cx_o = serve_snapshot(root.to_string(), &lv[..], &lv[..]);
    match cx_o {
        None => { assert!(false); return; }
        Some(cx) => {
            let rt = cx.rt;
            let q = "/v1/point?model=gfs&param=TMP&level=isobaric:875&lat=0.5&lon=0.5&valid=2025-10-19T19:00:00Z";
            let r = rt.block_on(fetch(cx.svc.clone(), q.to_string()));
            assert!(r.0 == 200);
            assert!(has(&r.1, "\"value\":87535.5"));
            // V1.1-byte-identical vertical object: NO extra key when the pairs agree
            assert!(has(&r.1, "\"vertical\":{\"method\":\"linear\",\"levels\":[900,800]}"));
            assert!(!has(&r.1, "levels_per_time"));
            rm_tree(&root);
        }
    }
}
