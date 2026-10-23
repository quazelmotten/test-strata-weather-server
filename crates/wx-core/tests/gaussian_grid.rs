// V1.3 phase E4b: GRIB template 3.40 gaussian-lat-lon grid validation.
//
// Ground truth chain (docs/DECISIONS.md §16): the operational FV3GFS sflux
// line rides a FULL GLOBAL GAUSSIAN grid whose row latitudes are the roots
// of Legendre P_ny (ny/2 positive, mirrored south) — NOT roots of P_{ny/2}.
// Verified against numpy.polynomial.legendre.leggauss (goldens pinned below
// as literals), against wgrib2's reference generator (geo.c::gauss2lats),
// and against LIVE GFS sflux bytes via tools/wgrib2-full -grid:
//   Gaussian grid: (3072 x 1536) units 1e-06 input WE:NS output WE:SN
//     number of latitudes between pole-equator=768 #points=4718592
//     lat 89.910324 to -89.910324
//     lon 0.000000 to 359.882813 by 0.117188
// The recorded Oct-22 probe row 40.006506 IS one of these table entries.

use wx_core::grid::gaussian_latitudes;
use wx_core::grid::GridDef;
use wx_core::grid::GridGaussian;

fn abs_diff(a: f64, b: f64) -> f64 {
    if a > b { a - b } else { b - a }
}

// ---------------------------------------------------------------------------
// Table goldens: small-nj values computed ONCE with
//   numpy.degrees(numpy.arcsin(numpy.sort(numpy.polynomial.legendre.leggauss(ny)[0])[::-1][:ny//2]))
// (Python 3 / numpy 2.5.3, Oct 22 2026) and pasted here verbatim. Tolerance
// 1e-8 degrees: our Newton iteration reproduces them far tighter, the pin
// guards against future regressions in seed/recurrence/refinement.
// ---------------------------------------------------------------------------

#[test]
fn gaussian_table_goldens_small_nj() {
    let cases: Vec<(u32, Vec<f64>)> = vec![
        (2, vec![35.26438968]),
        (4, vec![59.44440829, 19.87571915]),
        (6, vec![68.82309876, 41.39217282, 13.80505794]),
        (8, vec![73.79921363, 52.81294319, 31.70409175, 10.56988231]),
    ];
    for c in cases.iter() {
        let got = match gaussian_latitudes(c.0) {
            Some(v) => v,
            None => {
                println!("ny={0}: table missing", c.0);
                panic!();
            }
        };
        if got.len() != c.1.len() {
            println!("ny={0}: len {1} vs {2}", c.0, got.len(), c.1.len());
            panic!();
        }
        let mut k = 0usize;
        while k < got.len() {
            if abs_diff(got[k], c.1[k]) > 1e-8 {
                println!("ny={0} idx {1}: {2:.10} vs golden {3:.10}",
                         c.0, k, got[k], c.1[k]);
                panic!();
            }
            k += 1;
        }
    }
}

#[test]
fn gaussian_table_odd_ny_and_bounds() {
    // odd ny would require an equator row we do not model: rejected loudly
    if let Some(v) = gaussian_latitudes(1537) { println!("odd ny accepted: {0:?}", v); panic!(); }
    if gaussian_latitudes(1).is_some() { panic!(); }
    // ny=0 likewise
    if gaussian_latitudes(0).is_some() { panic!(); }
}

// ---------------------------------------------------------------------------
// Real-data anchor: the live GFS sflux grid (3072 x 1536). Values from
// `tools/wgrib2-full tests/realdata/gfs_sflux_f000.grib2 -grid` (pinned in
// DECISIONS §16); tolerance 1e-4 deg per task spec.
// ---------------------------------------------------------------------------

#[test]
fn gaussian_table_1536_matches_live_gfs() {
    let pos = match gaussian_latitudes(1536) {
        Some(v) => v,
        None => { panic!(); }
    };
    if pos.len() != 768 {
        println!("expected 768 positive rows, got {0}", pos.len());
        panic!();
    }
    // topmost row: file La1 = 89.910324 (wgrib2 printed; measured delta was
    // +5.3e-7 deg — well inside this 1e-4 guard)
    if abs_diff(pos[0], 89.910324) > 1e-4 {
        println!("top row {0:.6} vs file 89.910324", pos[0]);
        panic!();
    }
    // strictly descending
    let mut k = 1usize;
    while k < pos.len() {
        // NaN-safe exact equivalent of !(a < b): partial_cmp yields None for
        // NaN and map_or(true, ..) keeps the original "not less" verdict.
        if pos[k].partial_cmp(&pos[k - 1]) != Some(std::cmp::Ordering::Less) {
            println!("table not monotonic at {0}", k);
            panic!();
        }
        k += 1;
    }
    // the recorded probe latitude 40.006506 (E4a node probe, wgrib2 -lon)
    // must be a table entry within print rounding (< 1e-6 deg)
    let mut best = 1e9f64;
    k = 0usize;
    while k < pos.len() {
        let d = abs_diff(pos[k], 40.006506);
        if d < best { best = d; }
        k += 1;
    }
    if best > 1e-6 {
        println!("no table row within 1e-6 of 40.006506 (closest {0:.8})", best);
        panic!();
    }
}

// ---------------------------------------------------------------------------
// Storage-order table + symmetry via GridGaussian::new
// ---------------------------------------------------------------------------

fn gfs_like_grid() -> GridGaussian {
    // exact global spacing: 360/3072 (dyadic), NS storage (scan bit clear)
    GridGaussian::new(3072, 1536, 0.0, 89.910324, -89.910324, 360.0 / 3072.0,
                      0x00, 6, false)
}

#[test]
fn gaussian_rows_monotonic_symmetric_full_circle() {
    let g = gfs_like_grid();
    let ts = g.row_lats();
    if ts.len() != 1536 {
        println!("row table len {0}", ts.len());
        panic!();
    }
    let mut k = 1usize;
    while k < ts.len() {
        if ts[k].partial_cmp(&ts[k - 1]) != Some(std::cmp::Ordering::Less) {
            println!("rows not decreasing at {0}", k);
            panic!();
        }
        k += 1;
    }
    // hemisphere mirror: ts[j] == -ts[ny-1-j] exactly (construction is
    // symmetric by sign flip, so bit-exact equality holds)
    k = 0usize;
    while k < ts.len() {
        if ts[k] != -ts[ts.len() - 1 - k] {
            println!("mirror broken at {0}: {1} vs {2}", k, ts[k], ts[ts.len() - 1 - k]);
            panic!();
        }
        k += 1;
    }
    // small-nj grid: full-table order for an NS-scanned ny=4 grid is
    // [+59.44, +19.88, -19.88, -59.44]
    let g4 = GridGaussian::new(8, 4, 0.0, 59.44440829, -59.44440829, 45.0, 0x00, 6, false);
    let t4 = g4.row_lats();
    if abs_diff(t4[0], 59.44440829) > 1e-8 || abs_diff(t4[1], 19.87571915) > 1e-8
       || abs_diff(t4[2], -19.87571915) > 1e-8 || abs_diff(t4[3], -59.44440829) > 1e-8 {
        println!("ny=4 ordered table wrong: {0:?}", t4);
        panic!();
    }
    // SN scan mirrors the ORDER too (row 0 = La1 side = southernmost here)
    let g4n = GridGaussian::new(8, 4, 0.0, -59.44440829, 59.44440829, 45.0, 0x40, 6, false);
    let t4n = g4n.row_lats();
    let mut j = 0usize;
    while j < 4 {
        if t4n[j] != t4[3 - j] {
            println!("SN ordering wrong at {0}", j);
            panic!();
        }
        j += 1;
    }
}

// ---------------------------------------------------------------------------
// Forward transform: node-exact hits return integer j (and i), jittered
// in-betweens land strictly between nodes and stay monotone in lat.
// ---------------------------------------------------------------------------

#[test]
fn gaussian_project_node_exact_and_roundtrip() {
    let g = gfs_like_grid();
    let gd = GridDef::Gaussian(g.clone());
    let ts = g.row_lats();
    let dlon = 360.0 / 3072.0;
    // dyadic columns on dyadic dlon => di must come back EXACTLY integral
    let rows = [0usize, 1, 100, 767, 768, 1000, 1535];
    let cols = [0u32, 1, 1536, 2261, 3071];
    let mut r = 0usize;
    while r < rows.len() {
        let mut c = 0usize;
        while c < cols.len() {
            let colf = cols[c] as f64;
            let lon = colf * dlon; // exact in f64 (both dyadic)
            let pr = match gd.project(ts[rows[r]], lon) {
                Some(v) => v,
                None => {
                    println!("node ({0},{1}) projected out", rows[r], cols[c]);
                    panic!();
                }
            };
            if pr.0 != colf || pr.1 != rows[r] as f64 {
                println!("node ({0},{1}) -> ({2:?},{3:?}) not exact",
                         rows[r], cols[c], pr.0, pr.1);
                panic!();
            }
            c += 1;
        }
        r += 1;
    }
    // same nodes addressed through the -180..180 window (negative lons):
    // lon -95.0390625 == 264.9609375 bit-exactly after normalization
    let pr = match gd.project(ts[400], -95.0390625) {
        Some(v) => v,
        None => { panic!(); }
    };
    let want_i = ((2261.0 * dlon) - 360.0) + 360.0; // 264.9609375 east form
    let _ = want_i;
    if pr.0 != 2261.0 || pr.1 != 400.0 {
        println!("negative-lon node -> {0:?}", pr);
        panic!();
    }
    // jittered midpoints: dj strictly increases as lat walks south, always
    // bracketed by the enclosing node pair
    let mut prev_dj = -1.0f64;
    let mut k = 0usize;
    let frac = [0.125, 0.5, 0.875];
    while k < 200 {
        let rowbase = (k * 7) % 1535; // spread across rows deterministically
        let f = frac[k % 3];
        let lat = ts[rowbase] + (ts[rowbase + 1] - ts[rowbase]) * f;
        let pr = match gd.project(lat, 100.0) {
            Some(v) => v,
            None => {
                println!("midpoint lat {0} projected out", lat);
                panic!();
            }
        };
        let dj = pr.1;
        if !(dj > (rowbase as f64) && dj < (rowbase as f64 + 1.0)) {
            println!("dj {0} outside bracket [{1},{2}] (lat {3})",
                     dj, rowbase, rowbase + 1, lat);
            panic!();
        }
        if abs_diff(dj, rowbase as f64 + f) > 1e-9 {
            println!("dj {0} vs expected {1}", dj, rowbase as f64 + f);
            panic!();
        }
        if k > 0 && rowbase == ((k - 1) * 7) % 1535 && frac[k % 3] > frac[(k - 1) % 3] && dj <= prev_dj {
            println!("non-monotone within same row pair at k={0}", k);
            panic!();
        }
        prev_dj = dj;
        k += 1;
    }
}

#[test]
fn gaussian_out_of_domain_poles_and_beyond_rows() {
    let gd = GridDef::Gaussian(gfs_like_grid());
    // poles are ALWAYS out of domain on a gaussian grid (no pole nodes)
    if let Some(v) = gd.project(90.0, 0.0) { println!("north pole accepted: {0:?}", v); panic!(); }
    if let Some(v) = gd.project(-90.0, 123.4) { println!("south pole accepted: {0:?}", v); panic!(); }
    // beyond the outermost ROW (but inside the pole) is also out
    if let Some(v) = gd.project(89.95, 0.0) { println!("lat 89.95 accepted: {0:?}", v); panic!(); }
    if let Some(v) = gd.project(-89.95, 0.0) { println!("lat -89.95 accepted: {0:?}", v); panic!(); }
    // just inside the top row answers (tiny positive j)
    let pr = match gd.project(89.910324 - 1e-6, 0.0) {
        Some(v) => v,
        None => { panic!(); }
    };
    if !(pr.1 > 0.0 && pr.1 < 0.05) {
        println!("inside-top-row dj {0}", pr.1);
        panic!();
    }
    // lat outside [-90,90] rejected like every other grid
    if gd.project(91.0, 0.0).is_some() { panic!(); }
}

#[test]
fn gaussian_seam_wrap_semantics_match_regular() {
    let gd = GridDef::Gaussian(gfs_like_grid());
    if !gd.lon_wrap() {
        println!("3072 x 360/3072 grid must wrap");
        panic!();
    }
    // -0.1 and 359.9 normalize identically -> bit-identical projections
    let a = match gd.project(40.0, -0.1) { Some(v) => v, None => { panic!(); } };
    let b = match gd.project(40.0, 359.9) { Some(v) => v, None => { panic!(); } };
    if a != b {
        println!("seam mismatch: {0:?} vs {1:?}", a, b);
        panic!();
    }
    // one quarter-cell west of column 0 lands in the seam cell between
    // column nx-1 and column 0 with weight 0.75/0.25 (spec §9.5 semantics,
    // identical to the regular GFS grids)
    let dlon = 360.0 / 3072.0;
    let c = match gd.project(40.0, -0.25 * dlon) {
        Some(v) => v,
        None => { panic!(); }
    };
    if abs_diff(c.0, 3071.75) > 1e-12 {
        println!("seam di {0} want 3071.75", c.0);
        panic!();
    }
}
