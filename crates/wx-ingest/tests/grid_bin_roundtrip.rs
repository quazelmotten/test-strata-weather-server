// V1.3 phase E4b: grid.bin v3 round-trip. Pins that every GridDef variant
// survives encode_grid -> decode_grid BIT-EXACTLY through the custom binary
// format (fields AND projected coordinates), that the gaussian row-latitude
// table rebuilds bit-identically from ny (it is DERIVED data, not stored),
// and that a stale-version blob is REJECTED loudly (the pipeline's B0
// STORAGE-FORMAT RECOVERY trap turns that rejection into exactly one
// re-ingest per model — docs/DECISIONS.md).

use wx_core::grid::{GridDef, GridGaussian, GridLambert, GridRegular};
use wx_ingest::store::{decode_grid, encode_grid, GRID_MAGIC, GRID_VERSION};

fn gfs_like_gaussian() -> GridGaussian {
    GridGaussian::new(3072, 1536, 0.0, 89.910324, -89.910324,
                      360.0 / 3072.0, 0x00, 6, false)
}

fn sample_points() -> Vec<(f64, f64)> {
    // nodes, midpoints, seam, far edge, poles, out-of-domain probes
    let mut v: Vec<(f64, f64)> = Vec::new();
    v.push((40.006506, -95.0390625));
    v.push((89.910324, 0.0));
    v.push((-89.910324, 12.5));
    v.push((40.0, 359.9));
    v.push((40.0, -0.1));
    v.push((40.00001, 264.9609375));
    v.push((89.95, 0.0));
    v.push((90.0, 0.0));
    v.push((-90.0, 100.0));
    v.push((0.0, 180.0));
    return v;
}

#[test]
fn grid_bin_v3_gaussian_roundtrip_bitexact() {
    let g = GridDef::Gaussian(gfs_like_gaussian());
    let bytes = encode_grid(&g);
    if bytes.len() < 56 || bytes[..8] != GRID_MAGIC {
        println!("bad envelope: len {0}", bytes.len());
        assert!(false);
        return;
    }
    // version + kind slots pinned
    if bytes[8] as u32 + ((bytes[9] as u32) << 8) != GRID_VERSION {
        println!("stored version bytes {0:?} != GRID_VERSION {1}",
                 &bytes[8 .. 12], GRID_VERSION);
        assert!(false);
    }
    if bytes[12] != 2 {
        println!("stored kind byte {0}, want 2", bytes[12]);
        assert!(false);
    }
    let back = match decode_grid(&bytes) {
        Err(e) => { println!("decode failed: {0}", e); assert!(false); return; }
        Ok(gd) => { gd }
    };
    match (&g, &back) {
        (GridDef::Gaussian(a), GridDef::Gaussian(b)) => {
            assert!(a.nx == b.nx && a.ny == b.ny);
            assert!(a.lon1 == b.lon1 && a.dlon == b.dlon);
            assert!(a.la1 == b.la1 && a.la2 == b.la2);
            assert!(a.scan_mode == b.scan_mode && a.earth_shape == b.earth_shape);
            assert!(a.winds_grid == b.winds_grid);
            // derived table rebuilt bit-identically
            if a.row_lats().len() != b.row_lats().len() {
                println!("row table length drift");
                assert!(false);
                return;
            }
            let mut k = 0usize;
            while k < a.row_lats().len() {
                if a.row_lats()[k] != b.row_lats()[k] {
                    println!("row table differs at {0}: {1} vs {2}",
                             k, a.row_lats()[k], b.row_lats()[k]);
                    assert!(false);
                    return;
                }
                k += 1;
            }
        }
        _ => { println!("variant changed across roundtrip"); assert!(false); return; }
    }
    // projections must agree bit-for-bit at every probe
    let mut k = 0usize;
    while k < sample_points().len() {
        let (la, lo) = sample_points()[k];
        let p1 = a_proj(&g, la, lo);
        let p2 = a_proj(&back, la, lo);
        if p1 != p2 {
            println!("projection drift at ({0},{1}): {2:?} vs {3:?}", la, lo, p1, p2);
            assert!(false);
        }
        k += 1;
    }
}

fn a_proj(g: &GridDef, lat: f64, lon: f64) -> Option<(f64, f64)> {
    return g.project(lat, lon);
}

#[test]
fn grid_bin_v3_regular_and_lambert_still_roundtrip() {
    let reg = GridDef::Regular(GridRegular {
        nx: 1440, ny: 721, lon1: 0.0, lat1: 90.0, dlon: 0.25, dlat: 0.25,
        scan_mode: 0x40, winds_grid: false,
    });
    let rb = match decode_grid(&encode_grid(&reg)) {
        Err(e) => { println!("regular decode: {0}", e); assert!(false); return; }
        Ok(v) => { v }
    };
    let mut k = 0usize;
    while k < sample_points().len() {
        let (la, lo) = sample_points()[k];
        if a_proj(&reg, la, lo) != a_proj(&rb, la, lo) {
            println!("regular projection drift at ({0},{1})", la, lo);
            assert!(false);
            return;
        }
        k += 1;
    }
    let lam = GridDef::Lambert(GridLambert::new(
        1799, 1059, 21.11389, 233.44, 265.0, 21.11389, 25.0, 25.0,
        3000.0, 3000.0, 1, 0x00, 2, true));
    let lb = match decode_grid(&encode_grid(&lam)) {
        Err(e) => { println!("lambert decode: {0}", e); assert!(false); return; }
        Ok(v) => { v }
    };
    match (&lam, &lb) {
        (GridDef::Lambert(x), GridDef::Lambert(y)) => {
            assert!(x.ns == y.ns && x.rh == y.rh && x.x0 == y.x0 && x.y0 == y.y0);
            assert!(x.winds_grid == y.winds_grid && x.j_south == y.j_south);
        }
        _ => { assert!(false); }
    }
}

#[test]
fn grid_bin_stale_version_is_rejected_for_the_recovery_trap() {
    // the B0 trap keys off ANY decode error; pin that a version-2 blob (the
    // pre-E4b layout) is rejected rather than silently misread as v3
    let g = GridDef::Gaussian(gfs_like_gaussian());
    let mut bytes = encode_grid(&g);
    bytes[8] = 2; bytes[9] = 0; bytes[10] = 0; bytes[11] = 0;
    match decode_grid(&bytes) {
        Err(e) => {
            if e.find("version").is_none() {
                println!("stale-version error text: {0}", e);
                assert!(false);
            }
        }
        Ok(_) => { println!("v2 version slot accepted by v3 reader"); assert!(false); }
    }
    // unknown kind (e.g. a future variant read by this build) also rejects
    let mut bytes2 = encode_grid(&g);
    bytes2[12] = 9;
    match decode_grid(&bytes2) {
        Err(e) => {
            if e.find("kind").is_none() {
                println!("unknown-kind error text: {0}", e);
                assert!(false);
            }
        }
        Ok(_) => { assert!(false); }
    }
}
