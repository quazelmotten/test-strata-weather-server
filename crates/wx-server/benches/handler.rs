// Milestone-5 criterion bench: END-TO-END handler level WITHOUT HTTP —
// query-string parse -> snapshot walk -> select -> evaluate -> JSON body
// render. This mirrors what /v1/point does between its two mono_ns()
// samples plus response-body construction. A manual p50/p90/p99 pass
// follows the criterion runs (criterion 0.5 prints medians only).
// Run: cargo bench -p wx-server

use wx_core::grid::{GridDef, GridRegular};
use wx_core::storage::{BinHeader, StatMeta, open_field, write_bin_atomic};
use wx_ingest::index::{IndexSnapshot, IdxModel, IdxRun, IdxFieldEntry, IdxField,
                       IdxFh};
use wx_server::queryapi::{parse_query};
use wx_server::httpapi::{answer_all};
use criterion::{criterion_group, criterion_main, Criterion};
use std::sync::Arc;
use libc::timespec;
use libc::clock_gettime;
use libc::CLOCK_MONOTONIC;

#[allow(clippy::needless_return)]
pub fn mono_ns_local() -> i64 {
    let mut ts = timespec { tv_sec: 0 as i64, tv_nsec: 0 as i64 };
    unsafe {
        clock_gettime(CLOCK_MONOTONIC, &mut ts);
    }
    return ts.tv_sec * 1000000000 + ts.tv_nsec;
}

/// Build a GFS-like 8-level x 2-fh snapshot (warm mmapped, analytic values).
pub fn build_snapshot(dir: &str) -> Result<Arc<IndexSnapshot>, String> {
    let f0 = format!("{0}/wxserver_h_f000.bin", dir);
    let f6 = format!("{0}/wxserver_h_f006.bin", dir);
    let hdr = BinHeader {
        nx: 1440u32,
        ny: 720u32,
        levels: [1000.0, 925.0, 850.0, 700.0, 500.0, 300.0, 250.0, 200.0].to_vec(),
        scan_ns: true,
        winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
    let mut planes: Vec<Vec<f32>> = Vec::new();
    for li in 0 .. 8 {
        let mut plane: Vec<f32> = Vec::new();
        for j in 0 .. 720usize {
            for _i in 0 .. 1440usize {
                plane.push((250.0 + 10.0 * li as f64 + j as f64) as f32);
            }
        }
        planes.push(plane);
    }
    let mut refs: Vec<&[f32]> = Vec::new();
    for p in &planes {
        refs.push(p.as_slice());
    }
    let wr0 = write_bin_atomic(f0.clone(), &hdr, &refs);
    match wr0 {
        Err(e) => { return Err(e.to_string()); }
        Ok(_) => {}
    }
    // fh 6 reuses identical planes: time interp still exercises both slots
    // and time_lerp (equal blend == exact value, bit-safe)
    let wr6 = write_bin_atomic(f6.clone(), &hdr, &refs);
    match wr6 {
        Err(e) => { return Err(e.to_string()); }
        Ok(_) => {}
    }

    let m0_r = open_field(f0);
    let mf0 = match m0_r { Err(e) => { return Err(e.to_string()); } Ok(x) => { x } };
    let m6_r = open_field(f6);
    let mf6 = match m6_r { Err(e) => { return Err(e.to_string()); } Ok(x) => { x } };

    // warm pages before measuring
    let touch = |mf: &[u8]| -> u8 {
        let n = mf.len();
        let mut acc = 0u8;
        let mut off = 0usize;
        while off < n {
            acc = acc ^ mf[off];
            off = off + 4096usize;
        }
        return acc;
    };
    let t0 = touch(&mf0.region);
    let t6 = touch(&mf6.region);
    if t0 == 255u8 && t6 == 255u8 && mf0.region[0] == 0u8 && mf6.region[0] == 0u8 {
        // observer; never taken
    }

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
        fhs: [IdxFh { fh_secs: 0i64, field: Arc::new(mf0), stat_processed: false, stat_kind_code: 0, stat_has_range: false, stat_begin_secs: 0, stat_end_secs: 0, stat_step_secs: 0 },
              IdxFh { fh_secs: 21600i64, field: Arc::new(mf6), stat_processed: false, stat_kind_code: 0, stat_has_range: false, stat_begin_secs: 0, stat_end_secs: 0, stat_step_secs: 0 }].to_vec(),
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
    return Ok(Arc::new(snap));
}

/// Query strings cycled per iteration (fixed point / seam / vertical+time).
pub fn make_qs() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let lats: [f64; 4] = [89.875, 0.125, 41.25, 71.625];
    let lons: [f64; 4] = [0.0, 359.9, -95.93, 120.5];
    for k in 0 .. 4 {
        let qs = format!("model=gfs&param=TMP&level=isobaric:775&valid=2026-10-19T15:00:00Z&lat={0}&lon={1}",
                         lats[k], lons[k]);
        out.push(qs);
    }
    return out;
}

#[allow(clippy::needless_return)]
pub fn bench_handler(c: &mut Criterion) {
    let dir = "/tmp/opencode/wxpoint-bench";
    use std::fs::create_dir_all;
    let mk = create_dir_all(dir);
    match mk {
        Err(_) => { return; }
        Ok(_) => {}
    }
    let snap_r = build_snapshot(dir);
    let snap = match snap_r { Err(_) => { return; } Ok(x) => { x } };

    let snp = &snap;
    let qs_list = make_qs();
    let mut group_h = c.benchmark_group("handler");
    group_h.bench_function("v1/point parse+lookup+render", |b| {
        let mut k = 0usize;
        b.iter(|| {
            let pa_r = parse_query(qs_list[k].as_str());
            match pa_r {
                Err(_) => { return -1.0; }
                Ok(pa) => {
                    let pr = answer_all(snp.clone(), &pa);
                    k = (k + 1) % qs_list.len();
                    return pr.0 as f64 * 1000000.0 + pr.1.len() as f64;
                }
            }
        });
    });
    group_h.finish();

    // ---- manual p50/p90/p99 pass (criterion 0.5 prints medians only) ------
    let n = 200000usize;
    let mut samples: Vec<u64> = Vec::new();
    let mut sink = 0usize;
    let mut k = 0usize;
    for _m in 0 .. n {
        let t0 = mono_ns_local();
        let pa_r = parse_query(qs_list[k].as_str());
        match pa_r {
            Err(_) => {}
            Ok(pa) => {
                let pr = answer_all(snp.clone(), &pa);
                sink = sink + pr.1.len();
            }
        }
        k = (k + 1) % qs_list.len();
        let t1 = mono_ns_local();
        samples.push((((t1 - t0) + 500) / 1000) as u64);
    }
    samples.sort();
    let p50 = samples[n / 2];
    let p90 = samples[(n * 9) / 10];
    let p99 = samples[(n * 99) / 100];
    let total_us = samples.iter().fold(0u64, |a, b| a + b);
    println!("handler-level percentiles (no HTTP, warm page cache):");
    println!("{0}  p50={1} us  p90={2} us  p99={3} us  max={4} us",
             "", p50, p90, p99, samples[n - 1]);
    println!("{0}  mean={1} us  single-thread throughput={2} ops/s",
             "", (total_us as f64) / (n as f64),
             (n as f64) / ((total_us as f64) / 1000000.0));
    if sink == 0usize && n == 0usize {
        // observer
    }
}

criterion_group!(benches, bench_handler);
criterion_main!(benches);
