// Milestone-5 criterion bench: the FULL point query through the arc-swap
// index accessor (PointQueryContext.build + evaluate_point) against warm
// mmapped perf-sized fields with vertical (level 775 between 850/700) AND
// time (fh 3 between 0/6) interpolation active — spec §8 "full query
// function". Run: cargo bench -p wx-ingest.

use wx_core::grid::{GridDef, GridRegular};
use wx_core::storage::{BinHeader, MmapField, open_field, write_bin_atomic};
use wx_core::interp::{TimeInterpMode, PointError};
use wx_ingest::index::{IndexSnapshot, IdxModel, IdxRun, IdxFieldEntry, IdxField,
                       IdxFh, PointQueryContext, evaluate_point};
use criterion::{criterion_group, criterion_main, Criterion};
use std::sync::Arc;

/// GFS-like: 8 isobaric levels (canonical descending order), 1440 x 720.
fn gfs_levels() -> Vec<f32> {
    let lv: [f32; 8] = [1000.0, 925.0, 850.0, 700.0, 500.0, 300.0, 250.0, 200.0];
    return lv.to_vec();
}

/// Write one f<FFF>.bin (all 8 level planes) analytically: value(li,j,i) =
/// 250 + 10*li + j so every (level,time,node) expectation is exact in f32.
fn write_fh_file(path: &str, nx: u32, ny: u32) -> bool {
    let hdr = BinHeader {
        nx: nx,
        ny: ny,
        levels: gfs_levels(),
        scan_ns: true,
        winds_grid: false,
    };
    let mut planes: Vec<Vec<f32>> = Vec::new();
    for li in 0 .. 8 {
        let mut plane: Vec<f32> = Vec::new();
        for j in 0 .. (ny as usize) {
            for _i in 0 .. (nx as usize) {
                plane.push((250.0 + 10.0 * li as f64 + j as f64) as f32);
            }
        }
        planes.push(plane);
    }
    let mut refs: Vec<&[f32]> = Vec::new();
    for p in &planes {
        refs.push(p.as_slice());
    }
    let wr = write_bin_atomic(path.to_string(), &hdr, &refs);
    match wr {
        Err(_) => { return false; }
        Ok(_) => { return true; }
    }
}

/// Open both fh files, touch pages, and hand back Arc'd mmaps.
fn open_warm(path: &str) -> Result<Arc<MmapField>, String> {
    let of = open_field(path.to_string());
    match of {
        Err(e) => { return Err(e.to_string()); }
        Ok(mf) => {
            let region = &mf.region;
            let n = region.len();
            let mut acc = 0u8;
            let mut off = 0usize;
            while off < n {
                acc = acc ^ region[off];
                off = off + 4096usize;
            }
            if acc == 1u8 && n == 0usize {
                // observer; never taken
            }
            return Ok(Arc::new(mf));
        }
    }
}

#[allow(clippy::needless_return)]
pub fn bench_query(c: &mut Criterion) {
    let dir = "/tmp/opencode/wxpoint-bench";
    use std::fs::create_dir_all;
    let mk = create_dir_all(dir);
    match mk {
        Err(_) => { return; }
        Ok(_) => {}
    }
    let f0 = format!("{0}/wxingest_q_f000.bin", dir);
    let f6 = format!("{0}/wxingest_q_f006.bin", dir);
    if !write_fh_file(&f0, 1440u32, 720u32) {
        return;
    }
    if !write_fh_file(&f6, 1440u32, 720u32) {
        return;
    }
    let fh0_r = open_warm(&f0);
    let fh0 = match fh0_r { Err(_) => { return; }, Ok(x) => { x } };
    let fh6_r = open_warm(&f6);
    let fh6 = match fh6_r { Err(_) => { return; }, Ok(x) => { x } };

    let grid = Arc::new(GridDef::Regular(GridRegular {
        nx: 1440u32,
        ny: 720u32,
        lon1: 0.0,
        lat1: 89.875,
        dlon: 0.25,
        dlat: 0.25,
        scan_mode: 0u8,
        winds_grid: false,
    }));

    let field = IdxField {
        units: "K".to_string(),
        stat_processed: false,
        stat_kind: "instantaneous".to_string(),
        fhs: [IdxFh { fh_hours: 0i64, field: fh0.clone() },
              IdxFh { fh_hours: 6i64, field: fh6.clone() }].to_vec(),
    };
    let run = IdxRun {
        epoch_secs: 1761105600i64,
        dir_name: "2026101912".to_string(),
        grid: grid.clone(),
        fields: [IdxFieldEntry {
            param_key: "TMP".to_string(),
            level_code: 100u8,
            fd: field.clone(),
        }].to_vec(),
    };
    let snap = IndexSnapshot {
        models: [IdxModel {
            name: "gfs".to_string(),
            runs: [run].to_vec(),
        }].to_vec(),
    };

    let seed = PointQueryContext {
        grid: grid.clone(),
        units: "".to_string(),
        run_epoch_secs: 0i64,
        stat_processed: false,
        want_level: 0.0,
        want_fh_hours: 0.0,
        fh_hours: (0.0, 0.0),
        slots: [None, None],
    };

    // exact-node lat/lon probes (dyadic -> no projection noise)
    let mut lats: Vec<f64> = Vec::new();
    let mut lons: Vec<f64> = Vec::new();
    for k in 0 .. 512 {
        let j = (k * 37) % 719;
        let i = (k * 101) % 1439;
        lats.push(89.875 - 0.25 * j as f64);
        lons.push(0.25 * i as f64);
    }

    let mut group_q = c.benchmark_group("query");
    let gq = &mut group_q;
    gq.bench_function("gfs point (vertical+time, warm mmap)", |b| {
        let mut k = 0usize;
        b.iter(|| {
            let ctx_r = seed.build(&snap, "gfs", None, "TMP", 100u8,
                                   775.0, 3.0, TimeInterpMode::Linear);
            match ctx_r {
                Err(_) => {
                    k = (k + 1) % lats.len();
                    return -1.0;
                }
                Ok(ctx) => {
                    let ev = evaluate_point(&ctx, lats[k], lons[k]);
                    k = (k + 1) % lats.len();
                    match ev {
                        Err(e) => {
                            match e {
                                PointError::OutOfDomain => {}
                                PointError::MissingData => {}
                                PointError::LevelOutOfRange => {}
                                PointError::NoLevelBracket => {}
                                PointError::NoTimeBracket => {}
                                PointError::TimeExactOnly => {}
                                PointError::StatTimeGuard => {}
                            }
                            return -2.0;
                        }
                        Ok(pv) => { return pv.value; }
                    }
                }
            }
        });
    });
    group_q.finish();
}

criterion_group!(benches, bench_query);
criterion_main!(benches);
