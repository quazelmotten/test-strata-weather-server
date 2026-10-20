// Interpolation + projection kernel tests (spec §9 items 4-7 for synthetic
// grids): analytic exactness through query_point, exact-node rule, missing
// data, domain edges, dateline seam, poles, vertical gap bracket, time guard.

use wx_core::grid::GridRegular;
use wx_core::grid::GridDef;
use wx_core::interp::query_point;
use wx_core::interp::bilinear4;
use wx_core::interp::horiz_nodes;
use wx_core::interp::resolve_levels;
use wx_core::interp::nan_f64;
use wx_core::interp::TimeInterpMode;
use wx_core::interp::HorizMethod;
use wx_core::interp::PointError;
use wx_core::interp::FieldSamples;

const NX: usize = 9;
const NY: usize = 7;

fn fill_affine(out: &mut Vec<f64>) {
    let levels = [1000.0, 500.0];
    let ths = [6.0, 9.0];
    for li in 0 .. 2 {
        for ti in 0 .. 2 {
            for j in 0 .. NY {
                for i in 0 .. NX {
                    let v = 100.0 + 3.0 * i as f64 - 2.0 * j as f64
                          + 0.25 * levels[li] + 0.5 * ths[ti];
                    out.push(v);
                }
            }
        }
    }
}

fn make_grid() -> GridDef {
    let g = GridRegular {
        nx: NX as u32, ny: NY as u32, lon1: 0.0, lat1: 30.0,
        dlon: 2.0, dlat: 1.0, scan_mode: 0, winds_grid: false,
    };
    return GridDef::Regular(g);
}

fn vec1(a: f64) -> Vec<f64> {
    let mut v: Vec<f64> = Vec::<f64>::new();
    v.push(a);
    return v;
}

fn vec2(a: f64, b: f64) -> Vec<f64> {
    let mut v: Vec<f64> = Vec::<f64>::new();
    v.push(a);
    v.push(b);
    return v;
}

fn vec3(a: f64, b: f64, c: f64) -> Vec<f64> {
    let mut v: Vec<f64> = Vec::<f64>::new();
    v.push(a);
    v.push(b);
    v.push(c);
    return v;
}
fn abs_diff(a: f64, b: f64) -> f64 {
    let d = a - b;
    if d < 0.0 {
        return -d;
    }
    return d;
}

#[test]
fn affine_exact_interior() {
    let mut data: Vec<f64> = Vec::<f64>::new();
    fill_affine(&mut data);
    let fd = FieldSamples {
        levels: vec2(1000.0, 500.0),
        fhs: vec2(6.0, 9.0),
        data: &data[0 .. data.len()],
        stat_processed: false,
    };
    // di = 4.15 (lon 8.3), dj = 2.7 (lat 27.3), level 712.5, fh 7.25
    let got = query_point(&make_grid(), &fd, 27.3, 8.3, 712.5, 7.25,
                          TimeInterpMode::Linear);
    match got {
        Err(_) => { assert!(false); }
        Ok(pv) => {
            let exp = 100.0 + 3.0 * 4.15 - 2.0 * 2.7 + 0.25 * 712.5 + 0.5 * 7.25;
            assert!(abs_diff(pv.value, exp) < 1e-8);
            assert!(pv.info.horizontal == HorizMethod::Bilinear);
            match pv.info.vertical {
                None => { assert!(false); }
                Some(vm) => { assert!(vm.levels.0 == 1000.0 && vm.levels.1 == 500.0); }
            }
            match pv.info.time {
                None => { assert!(false); }
                Some(tm) => { assert!(tm.forecast_hours.0 == 6.0 && tm.forecast_hours.1 == 9.0); }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// exact-node rule: on a node (and within EXACT_NODE_EPS of one) the stored
// node value is returned bit-for-bit, no blending.
// ---------------------------------------------------------------------------

#[test]
fn exact_node_bitexact() {
    let mut data: Vec<f64> = Vec::<f64>::new();
    fill_affine(&mut data);
    let fd = FieldSamples {
        levels: vec2(1000.0, 500.0),
        fhs: vec2(6.0, 9.0),
        data: &data[0 .. data.len()],
        stat_processed: false,
    };
    // exactly node (i=4, j=2, level 1000, fh 6): F = 100+12-4+250+3 = 361
    let got = query_point(&make_grid(), &fd, 28.0, 8.0, 1000.0, 6.0,
                          TimeInterpMode::Linear);
    match got {
        Err(_) => { assert!(false); }
        Ok(pv) => {
            assert!(pv.value == 361.0);
            assert!(pv.info.horizontal == HorizMethod::Exact);
            assert!(pv.info.vertical.is_none());
            assert!(pv.info.time.is_none());
        }
    }
    // within 1e-9 grid units of the same node: still unblended
    let got2 = query_point(&make_grid(), &fd, 28.0 - 4e-10, 8.0 + 8e-10,
                           1000.0, 6.0, TimeInterpMode::Linear);
    match got2 {
        Err(_) => { assert!(false); }
        Ok(pv) => {
            assert!(pv.value == 361.0);
            assert!(pv.info.horizontal == HorizMethod::Exact);
        }
    }
}

// ---------------------------------------------------------------------------
// low-level bilinear kernel: affine reproduction, exact nodes, NaN poison.
// ---------------------------------------------------------------------------

#[test]
fn bilinear_kernel_affine_nan() {
    // v(x,y) = x + y over unit cell corners
    let v = bilinear4(0.0, 1.0, 1.0, 2.0, 0.25, 0.75);
    assert!(v == 1.0);
    // exact node returns the node value with no arithmetic at all
    assert!(bilinear4(1.1, 2.2, 3.3, 4.4, 0.0, 0.0) == 1.1);
    assert!(bilinear4(1.1, 2.2, 3.3, 4.4, 1.0, 0.0) == 2.2);
    // any NaN neighbor poisons the result (no weight renormalization)
    let n = nan_f64();
    assert!(bilinear4(n, 2.0, 3.0, 4.0, 0.5, 0.5).is_nan());
    assert!(bilinear4(1.0, 2.0, 3.0, n, 0.5, 0.5).is_nan());
}

#[test]
fn horiz_nodes_wrap_indices() {
    // seam cell: di = 359.9 on a wrapping 360-column grid blends col 359
    // with col 0
    let t = horiz_nodes(360, 5, 359.9, 0.0, true);
    assert!(t.0 == 359);
    assert!(t.2 == 0);
    assert!(t.4 > 0.5);
}

// ---------------------------------------------------------------------------
// domain edges: closed node rectangle; half-cells beyond outer nodes at
// non-wrapping edges are OutOfDomain (never extrapolate horizontally).
// ---------------------------------------------------------------------------

#[test]
fn out_of_domain_edges() {
    let mut data: Vec<f64> = Vec::<f64>::new();
    fill_affine(&mut data);
    let fd = FieldSamples {
        levels: vec2(1000.0, 500.0),
        fhs: vec2(6.0, 9.0),
        data: &data[0 .. data.len()],
        stat_processed: false,
    };
    let g = make_grid();
    // west of lon1 (node rectangle edge at lon 0)
    let e1 = query_point(&g, &fd, 27.0, -0.5, 1000.0, 6.0, TimeInterpMode::Linear);
    match e1 {
        Ok(_) => { assert!(false); }
        Err(pe) => { assert!(pe == PointError::OutOfDomain); }
    }
    // east of last column (lon 16): even a hair outside is rejected
    let e2 = query_point(&g, &fd, 27.0, 16.0001, 1000.0, 6.0, TimeInterpMode::Linear);
    match e2 {
        Ok(_) => { assert!(false); }
        Err(pe) => { assert!(pe == PointError::OutOfDomain); }
    }
    // north of lat1 and south of last row
    let e3 = query_point(&g, &fd, 30.5, 8.0, 1000.0, 6.0, TimeInterpMode::Linear);
    match e3 {
        Ok(_) => { assert!(false); }
        Err(pe) => { assert!(pe == PointError::OutOfDomain); }
    }
    let e4 = query_point(&g, &fd, 22.5, 8.0, 1000.0, 6.0, TimeInterpMode::Linear);
    match e4 {
        Ok(_) => { assert!(false); }
        Err(pe) => { assert!(pe == PointError::OutOfDomain); }
    }
}

// ---------------------------------------------------------------------------
// dateline seam (spec §9.5): GFS-style global grid, longitudes wrap.
// nx=360 columns at lon i degrees; field v(i,j) = i is NOT periodic, so the
// seam blend value pins the exact neighbor pairing across 359 -> 0.
// ---------------------------------------------------------------------------

#[test]
fn seam_blend_values() {
    let nx = 360;
    let ny = 5;
    let mut data: Vec<f64> = Vec::<f64>::new();
    for _j in 0 .. ny {
        for i in 0 .. nx {
            data.push(i as f64);
        }
    }
    let fd = FieldSamples {
        levels: vec1(1000.0),
        fhs: vec1(0.0),
        data: &data[0 .. data.len()],
        stat_processed: false,
    };
    let g0 = GridRegular {
        nx: 360, ny: 5, lon1: 0.0, lat1: 80.0,
        dlon: 1.0, dlat: 10.0, scan_mode: 0, winds_grid: false,
    };
    let g = GridDef::Regular(g0);
    // lon 359.9: between col 359 (v=359) and col 0 (v=0), fi=0.9 -> 35.9
    let a = query_point(&g, &fd, 80.0, 359.9, 1000.0, 0.0, TimeInterpMode::None);
    match a {
        Err(_) => { assert!(false); }
        Ok(pv) => { assert!(abs_diff(pv.value, 35.9) < 1e-9); }
    }
    // lon -0.1 normalizes to 359.9: same value
    let b = query_point(&g, &fd, 80.0, -0.1, 1000.0, 0.0, TimeInterpMode::None);
    match b {
        Err(_) => { assert!(false); }
        Ok(pv) => { assert!(abs_diff(pv.value, 35.9) < 1e-9); }
    }
    // lon 0.0 is exactly column 0: exact node, value 0
    let c = query_point(&g, &fd, 80.0, 0.0, 1000.0, 0.0, TimeInterpMode::None);
    match c {
        Err(_) => { assert!(false); }
        Ok(pv) => {
            assert!(pv.value == 0.0);
            assert!(pv.info.horizontal == HorizMethod::Exact);
        }
    }
    // lon 359.5: half-cell blend -> 359*0.5 + 0*0.5 = 179.5
    let d = query_point(&g, &fd, 80.0, 359.5, 1000.0, 0.0, TimeInterpMode::None);
    match d {
        Err(_) => { assert!(false); }
        Ok(pv) => { assert!(pv.value == 179.5); }
    }
}

// ---------------------------------------------------------------------------
// poles/edges on a global-latitude grid (spec §9.6): lat1=+90, rows N->S,
// last row at -90. Poles are real nodes; beyond +-90 is OutOfDomain.
// ---------------------------------------------------------------------------

#[test]
fn poles_global_grid() {
    let ny = 19;
    let nx = 36;
    let mut data: Vec<f64> = Vec::<f64>::new();
    for j in 0 .. ny {
        for _i in 0 .. nx {
            data.push(j as f64);
        }
    }
    let fd = FieldSamples {
        levels: vec1(1000.0),
        fhs: vec1(0.0),
        data: &data[0 .. data.len()],
        stat_processed: false,
    };
    let g0 = GridRegular {
        nx: 36, ny: 19, lon1: 0.0, lat1: 90.0,
        dlon: 10.0, dlat: 10.0, scan_mode: 0, winds_grid: false,
    };
    let g = GridDef::Regular(g0);
    // north pole row: dj = 0 exactly, lon on a column node -> exact
    let a = query_point(&g, &fd, 90.0, 120.0, 1000.0, 0.0, TimeInterpMode::None);
    match a {
        Err(_) => { assert!(false); }
        Ok(pv) => {
            assert!(pv.value == 0.0);
            assert!(pv.info.horizontal == HorizMethod::Exact);
        }
    }
    // south pole row: dj = 18 = ny-1 exactly, lon on a column node
    let b = query_point(&g, &fd, -90.0, 120.0, 1000.0, 0.0, TimeInterpMode::None);
    match b {
        Err(_) => { assert!(false); }
        Ok(pv) => {
            assert!(pv.value == 18.0);
            assert!(pv.info.horizontal == HorizMethod::Exact);
        }
    }
    // interior near pole: dj = 0.45 -> v = 0.45 (field affine in j)
    let c = query_point(&g, &fd, 85.5, 5.0, 1000.0, 0.0, TimeInterpMode::None);
    match c {
        Err(_) => { assert!(false); }
        Ok(pv) => { assert!(abs_diff(pv.value, 0.45) < 1e-9); }
    }
    // beyond the pole: OutOfDomain
    let d = query_point(&g, &fd, 90.0000001, 0.0, 1000.0, 0.0, TimeInterpMode::None);
    match d {
        Ok(_) => { assert!(false); }
        Err(pe) => { assert!(pe == PointError::OutOfDomain); }
    }
}

// ---------------------------------------------------------------------------
// missing data (spec §9.7): a NaN stencil neighbor yields MissingData, never
// a renormalized blend of the surviving neighbors.
// ---------------------------------------------------------------------------

#[test]
fn nan_neighbor_missing_data() {
    let mut data: Vec<f64> = Vec::<f64>::new();
    fill_affine(&mut data);
    let n = nan_f64();
    // poison node (i=4, j=2) of slab (li=0, ti=0): slab stride = NT*NX*NY
    let idx = 0 * (2 * NX * NY) + 0 * (NX * NY) + 2 * NX + 4;
    data[idx] = n;
    let fd = FieldSamples {
        levels: vec2(1000.0, 500.0),
        fhs: vec2(6.0, 9.0),
        data: &data[0 .. data.len()],
        stat_processed: false,
    };
    // query whose stencil includes the poisoned node
    let got = query_point(&make_grid(), &fd, 27.3, 8.3, 712.5, 7.25,
                          TimeInterpMode::Linear);
    match got {
        Ok(_) => { assert!(false); }
        Err(pe) => { assert!(pe == PointError::MissingData); }
    }
    // a query far away is unaffected
    let ok = query_point(&make_grid(), &fd, 25.4, 2.2, 1000.0, 6.0,
                         TimeInterpMode::Linear);
    match ok {
        Err(_) => { assert!(false); }
        Ok(pv) => {
            let exp = 100.0 + 3.0 * 1.1 - 2.0 * 4.6 + 250.0 + 3.0;
            assert!(abs_diff(pv.value, exp) < 1e-8);
        }
    }
}

// ---------------------------------------------------------------------------
// vertical interpolation across a configured gap (spec §6.2 / §9): only
// 850 and 500 stored; request 700 -> linear in pressure, bracket reported.
// ---------------------------------------------------------------------------

#[test]
fn vertical_gap_bracket() {
    let direct = resolve_levels(&vec2(850.0, 500.0), 700.0);
    match direct {
        None => { assert!(false); }
        Some(p) => { assert!(p.0 == 0 && p.1 == 1); }
    }
    assert!(resolve_levels(&vec2(850.0, 500.0), 900.0).is_none());
    assert!(resolve_levels(&vec3(1000.0, 925.0, 850.0), 850.0) == Some((2, 2)));

    let nx = 3;
    let ny = 3;
    let mut data: Vec<f64> = Vec::<f64>::new();
    for li in 0 .. 2 {
        for _k in 0 .. nx * ny {
            if li == 0 {
                data.push(260.0);
            } else {
                data.push(240.0);
            }
        }
    }
    let fd = FieldSamples {
        levels: vec2(850.0, 500.0),
        fhs: vec1(0.0),
        data: &data[0 .. data.len()],
        stat_processed: false,
    };
    let g0 = GridRegular {
        nx: 3, ny: 3, lon1: 0.0, lat1: 10.0,
        dlon: 1.0, dlat: 1.0, scan_mode: 0, winds_grid: false,
    };
    let g = GridDef::Regular(g0);
    let got = query_point(&g, &fd, 9.0, 1.0, 700.0, 0.0,
                          TimeInterpMode::None);
    match got {
        Err(_) => { assert!(false); }
        Ok(pv) => {
            let exp = 260.0 + (700.0 - 850.0) / (500.0 - 850.0) * (240.0 - 260.0);
            assert!(abs_diff(pv.value, exp) < 1e-9);
            match pv.info.vertical {
                None => { assert!(false); }
                Some(vm) => { assert!(vm.levels.0 == 850.0 && vm.levels.1 == 500.0); }
            }
        }
    }
    // outside the stored level range: LevelOutOfRange, never extrapolate
    let bad = query_point(&g, &fd, 9.0, 1.0, 400.0, 0.0,
                          TimeInterpMode::None);
    match bad {
        Ok(_) => { assert!(false); }
        Err(pe) => { assert!(pe == PointError::LevelOutOfRange); }
    }
}

// ---------------------------------------------------------------------------
// time guard (spec §6.3 / §9.8): statistically processed fields (template
// 4.8 accumulations etc.) reject non-exact forecast hours; time_interp=none
// requires exact matches; outside stored fhs -> NoTimeBracket.
// ---------------------------------------------------------------------------

#[test]
fn time_guard_stat_and_none() {
    let mut data: Vec<f64> = Vec::<f64>::new();
    fill_affine(&mut data);
    // same field but flagged as statistically processed
    let fd_stat = FieldSamples {
        levels: vec2(1000.0, 500.0),
        fhs: vec2(6.0, 9.0),
        data: &data[0 .. data.len()],
        stat_processed: true,
    };
    let g = make_grid();
    let a = query_point(&g, &fd_stat, 27.3, 8.3, 1000.0, 7.5, TimeInterpMode::Linear);
    match a {
        Ok(_) => { assert!(false); }
        Err(pe) => { assert!(pe == PointError::StatTimeGuard); }
    }
    // exact fh still works on a stat field
    let b = query_point(&g, &fd_stat, 27.3, 8.3, 1000.0, 9.0, TimeInterpMode::Linear);
    match b {
        Err(_) => { assert!(false); }
        Ok(pv) => {
            let exp = 100.0 + 3.0 * 4.15 - 2.0 * 2.7 + 250.0 + 4.5;
            assert!(abs_diff(pv.value, exp) < 1e-8);
            assert!(pv.info.time.is_none());
        }
    }
    // time_interp=none with an inexact fh -> TimeExactOnly
    let fd_plain = FieldSamples {
        levels: vec2(1000.0, 500.0),
        fhs: vec2(6.0, 9.0),
        data: &data[0 .. data.len()],
        stat_processed: false,
    };
    let c = query_point(&g, &fd_plain, 27.3, 8.3, 1000.0, 7.5, TimeInterpMode::None);
    match c {
        Ok(_) => { assert!(false); }
        Err(pe) => { assert!(pe == PointError::TimeExactOnly); }
    }
    // outside the stored forecast-hour range -> NoTimeBracket
    let d = query_point(&g, &fd_plain, 27.3, 8.3, 1000.0, 20.0, TimeInterpMode::Linear);
    match d {
        Ok(_) => { assert!(false); }
        Err(pe) => { assert!(pe == PointError::NoTimeBracket); }
    }
    // linear time interpolation between 6 and 9 at fh=7.5 reproduces the
    // affine-in-t field exactly (covered by affine_exact_interior above, but
    // pin one more point here):
    let e = query_point(&g, &fd_plain, 27.3, 8.3, 1000.0, 7.5, TimeInterpMode::Linear);
    match e {
        Err(_) => { assert!(false); }
        Ok(pv) => {
            let exp = 100.0 + 3.0 * 4.15 - 2.0 * 2.7 + 250.0 + 0.5 * 7.5;
            assert!(abs_diff(pv.value, exp) < 1e-8);
            match pv.info.time {
                None => { assert!(false); }
                Some(tm) => { assert!(tm.forecast_hours.0 == 6.0 && tm.forecast_hours.1 == 9.0); }
            }
        }
    }
}
