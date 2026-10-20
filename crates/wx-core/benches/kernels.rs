// Milestone-5 criterion benches for wx-core hot kernels (spec §8:
// "projection math, bilinear lookup"). Run with:
//   cargo bench -p wx-core            (bench profile = release minus panic=off)
// Criterion reports min/MEDIAN (=p50)/max + mean CI per benchmark; ops/s is
// derived in the README as 1e6/median_us. True p50/p99 under load come from
// the /metrics histogram (scripts/loadtest.sh).

use wx_core::grid::{GridRegular, GridLambert};
use wx_core::interp::{bilinear_slab_f32, bilinear4, horiz_nodes};
use wx_core::storage::{MmapField, open_field, write_bin_atomic, BinHeader};
use criterion::{criterion_group, criterion_main, Criterion};

/// Deterministic xorshift64* PRNG (no rand dependency in wx-core).
fn rng_next(seed: u64) -> u64 {
    let mut x = seed;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    return x.wrapping_mul(2685821657363691481u64);
}

/// GFS-like regular grid inputs: n pseudo-random interior points.
fn gen_regular_inputs(n: usize) -> Vec<(f64, f64)> {
    let mut seed = 0x243F6A8885A308D3u64;
    let mut out: Vec<(f64, f64)> = Vec::new();
    for _k in 0 .. n {
        seed = rng_next(seed);
        let r1 = (seed >> 11) as f64 / 9007199254740992.0;
        seed = rng_next(seed);
        let r2 = (seed >> 11) as f64 / 9007199254740992.0;
        out.push((r1 * 179.0 - 89.5, r2 * 359.9 - 179.95));
    }
    return out;
}

/// HRRR CONUS-like LCC inputs: n pseudo-random interior points.
fn gen_lamcc_inputs(n: usize) -> Vec<(f64, f64)> {
    let mut seed = 0x13198A2E03707344u64;
    let mut out: Vec<(f64, f64)> = Vec::new();
    for _k in 0 .. n {
        seed = rng_next(seed);
        let r1 = (seed >> 11) as f64 / 9007199254740992.0;
        seed = rng_next(seed);
        let r2 = (seed >> 11) as f64 / 9007199254740992.0;
        out.push((24.0 + r1 * 24.0, -124.0 + r2 * 40.0));
    }
    return out;
}

/// Fractional (di, dj) pairs strictly inside the node rectangle.
fn gen_cells(nx: usize, ny: usize, n: usize) -> Vec<(f64, f64)> {
    let mut seed = 0x9E3779B97F4A7C15u64;
    let mut out: Vec<(f64, f64)> = Vec::new();
    for _k in 0 .. n {
        seed = rng_next(seed);
        let di = ((seed >> 11) as f64 / 9007199254740992.0) * ((nx - 1) as f64);
        seed = rng_next(seed);
        let dj = ((seed >> 11) as f64 / 9007199254740992.0) * ((ny - 1) as f64);
        out.push((di, dj));
    }
    return out;
}

#[allow(clippy::needless_return)]
pub fn bench_projection(c: &mut Criterion) {
    let greg = GridRegular {
        nx: 1440u32,
        ny: 720u32,
        lon1: 0.0,
        lat1: 89.875,
        dlon: 0.25,
        dlat: 0.25,
        scan_mode: 0u8,
        winds_grid: false,
    };
    let glam = GridLambert::new(1799u32, 1059u32, 21.11389, 233.44, 229.53, 26.5,
                                26.5, 35.0, 3000.0, 3000.0, 1u8, 0x40u8, 6u8, false);
    let rin = gen_regular_inputs(4096);
    let lin = gen_lamcc_inputs(4096);
    let mut group_r = c.benchmark_group("projection");
    let gm = &mut group_r;
    gm.bench_function("regular_ll project", |b| {
        let mut k = 0usize;
        b.iter(|| {
            let pr = greg.project(rin[k].0, rin[k].1);
            k = (k + 1) % rin.len();
            return pr;
        });
    });
    gm.bench_function("lamcc project (precomputed consts)", |b| {
        let mut k = 0usize;
        b.iter(|| {
            let pr = glam.project(lin[k].0, lin[k].1);
            k = (k + 1) % lin.len();
            return pr;
        });
    });
    group_r.finish();
}

#[allow(clippy::needless_return)]
pub fn bench_bilinear(c: &mut Criterion) {
    let nx = 1440usize;
    let ny = 720usize;
    let slab: Vec<f32> = build_slab(nx, ny);
    let cells = gen_cells(nx, ny, 4096);
    let mut group_h = c.benchmark_group("bilinear");
    let gh = &mut group_h;
    gh.bench_function("slab f32 warm (heap)", |b| {
        let mut k = 0usize;
        b.iter(|| {
            let v = bilinear_slab_f32(&slab, nx, ny, cells[k].0, cells[k].1, true);
            k = (k + 1) % cells.len();
            return v;
        });
    });
    group_h.finish();

    let mm = mmap_warm_field(nx, ny);
    match mm {
        Err(_) => { return; }
        Ok(mf) => {
            let mfp = &mf;
            let mut group_m = c.benchmark_group("bilinear");
            let gm2 = &mut group_m;
            gm2.bench_function("mmap field (server h_at pattern)", |b| {
                let mut k = 0usize;
                b.iter(|| {
                    // identical shape to wx-ingest index.evaluate_point h_at
                    let nd = horiz_nodes(nx, ny, cells[k].0, cells[k].1, true);
                    let v00 = mfp.value(0usize, nd.1, nd.0) as f64;
                    let v10 = mfp.value(0usize, nd.1, nd.2) as f64;
                    let v01 = mfp.value(0usize, nd.3, nd.0) as f64;
                    let v11 = mfp.value(0usize, nd.3, nd.2) as f64;
                    k = (k + 1) % cells.len();
                    return bilinear4(v00, v10, v01, v11, nd.4, nd.5);
                });
            });
            group_m.finish();
        }
    }
}

/// Analytic GFS-like plane: value = 250 + row j.
fn build_slab(nx: usize, ny: usize) -> Vec<f32> {
    let mut out: Vec<f32> = Vec::new();
    for j in 0 .. ny {
        for _i in 0 .. nx {
            out.push((250.0 + j as f64) as f32);
        }
    }
    return out;
}

/// Write one single-level .bin via the atomic writer, open it with mmap and
/// touch every page so the bench measures warm-page latency (spec §8).
fn mmap_warm_field(nx: usize, ny: usize) -> Result<MmapField, String> {
    let dir = "/tmp/opencode/wxpoint-bench";
    if std_fs_mkdir(dir) {
        return Err("mkdir failed".to_string());
    }
    let path = format!("{0}/wxcore_kernels_single.bin", dir);
    let hdr = BinHeader {
        nx: nx as u32,
        ny: ny as u32,
        levels: [250.0].to_vec(),
        scan_ns: true,
        winds_grid: false,
    };
    let plane = build_slab(nx, ny);
    let pl = &plane;
    let wr = write_bin_atomic(path.clone(), &hdr, &[pl]);
    match wr {
        Err(e) => { return Err(e.to_string()); }
        Ok(_) => {}
    }
    let of = open_field(path);
    match of {
        Err(e) => { return Err(e.to_string()); }
        Ok(mf) => {
            // touch all pages (warm page cache before measuring)
            let region = &mf.region;
            let n = region.len();
            let mut acc = 0u8;
            let mut off = 0usize;
            while off < n {
                acc = acc ^ region[off];
                off = off + 4096usize;
            }
            if acc == 0u8 && n > 0usize && region[0] != 0u8 {
                // touch-loop observer; never taken in practice
            }
            return Ok(mf);
        }
    }
}

/// mkdir helper (returns true on failure).
fn std_fs_mkdir(path: &str) -> bool {
    use std::fs::create_dir_all;
    let r = create_dir_all(path);
    match r {
        Err(_) => { return true; }
        Ok(_) => { return false; }
    }
}

criterion_group!(benches, bench_projection, bench_bilinear);
criterion_main!(benches);
