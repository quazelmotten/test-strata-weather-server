// Spec §9.3 wind-rotation validation (V1.1 phase 4).
//
// The `wgrib2 -new_grid_winds earth` golden is environment-blocked on this
// machine (IPOLATES is Fortran-only; see docs/DECISIONS.md), so the expected
// angles come from an INDEPENDENT pyproj measurement of the conformal angle
// with the earth shape matched EXACTLY to each fixture's Code Table 3.2 byte
// (sphere R=6371229 m for hrrr_small, INLINE WGS84 axes for hrrr_ellip_small).
// Regenerate the table with scripts/validate_rotation.py.
//
// Tolerance: 1e-6 degrees. The previous finite-difference method could not do
// better than ~2.4e-2 deg and was tested at 5e-2.

use wx_ingest::grib2::{decode_file, ParamFilter, grid_winds};
use wx_core::grid::{GridDef, GridLambert};
use wx_core::tables::parse_f64;
use std::fs::read_to_string;

fn fixture_path(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

fn abs_diff(a: f64, b: f64) -> f64 {
    let d = a - b;
    if d < 0.0 {
        return -d;
    }
    d
}

/// Wrap into (-180, 180].
fn wrap_deg(d: f64) -> f64 {
    let w = d % 360.0;
    if w > 180.0 {
        return w - 360.0;
    }
    if w < -180.0 {
        return w + 360.0;
    }
    w
}

#[derive(Clone, Debug)]
pub struct Row {
    pub fixture: String,
    pub i: f64,
    pub j: f64,
    pub lat: f64,
    pub lon: f64,
    pub beta: f64,
}

fn load_rows() -> Vec<Row> {
    let t = read_to_string(fixture_path("rotation_expected.csv"));
    match t {
        Err(_e) => {
            panic!();
        }
        Ok(txt) => {
            let mut out: Vec<Row> = Vec::<Row>::new();
            let lines: Vec<&str> = txt.split('\n').collect();
            for ln0 in lines {
                // tolerate CRLF if the table is ever regenerated on Windows
                let ln: &str = if !ln0.is_empty() &&
                                 ln0[ln0.len() - 1 .. ln0.len()] == *"\r" {
                    &ln0[0 .. ln0.len() - 1]
                } else {
                    ln0
                };
                let parts: Vec<&str> = ln.split(',').collect();
                if parts.len() < 6 || parts[0] == "fixture" {
                    continue;
                }
                let fi = parse_f64(parts[1]);
                let fj = parse_f64(parts[2]);
                let fl = parse_f64(parts[3]);
                let fo = parse_f64(parts[4]);
                let fb = parse_f64(parts[5]);
                if let (Some(a), Some(b), Some(la), Some(lo), Some(bb)) = (fi, fj, fl, fo, fb) {
                    out.push(Row {
                        fixture: parts[0].to_string(),
                        i: a,
                        j: b,
                        lat: la,
                        lon: lo,
                        beta: bb,
                    });
                }
            }
            out
        }
    }
}

/// The Lambert grid of a fixture that carries the grid-relative wind flag.
fn wind_grid_of(fixture: &str) -> GridLambert {
    let r = decode_file(fixture_path(fixture), ParamFilter::All);
    match r {
        Err(_e) => {
            panic!();
        }
        Ok((msgs, _st)) => {
            for m in &msgs {
                if grid_winds(&m.grid) {
                    match m.grid {
                        GridDef::Lambert(ref gl) => { return gl.clone(); }
                        GridDef::Regular(ref _rg) => { panic!(); }
                        GridDef::Gaussian(ref _gg) => { panic!(); }
                    }
                }
            }
            panic!();
        }
    }
}

#[test]
fn rotation_matches_pyproj_within_1e_6_deg() {
    // spec §9.3: agreement with an independent pyproj measurement at >=20
    // nodes per fixture (corners + center + random), <= 1e-6 degrees.
    let rows = load_rows();
    assert!(rows.len() >= 40);
    let gs = wind_grid_of("hrrr_small.grib2");
    let go = wind_grid_of("hrrr_ellip_small.grib2");
    let mut n_sphere = 0;
    let mut n_oblate = 0;
    let mut worst = 0.0;
    for row in &rows {
        let got = if row.fixture == "hrrr_small.grib2" {
            n_sphere += 1;
            gs.rot_angle_deg(row.i, row.j)
        } else {
            n_oblate += 1;
            go.rot_angle_deg(row.i, row.j)
        };
        let d = abs_diff(got, row.beta);
        if d > worst {
            worst = d;
        }
        assert!(d <= 1e-6);
    }
    assert!(n_sphere >= 20);
    assert!(n_oblate >= 20);
    println!("rotation vs pyproj: {0} sphere + {1} oblate nodes, worst delta {2} deg",
             n_sphere, n_oblate, worst);
}

#[test]
fn rotation_equals_convergence_relation_of_own_projection() {
    // ONE truth: beta must equal 90 + ns*(lon - LoV) using the SAME ns/LoV
    // constants `for_point` uses, at every node of BOTH earth shapes.
    let rows = load_rows();
    let gs = wind_grid_of("hrrr_small.grib2");
    let go = wind_grid_of("hrrr_ellip_small.grib2");
    for row in &rows {
        let gl = if row.fixture == "hrrr_small.grib2" { &gs } else { &go };
        // longitude from the grid's OWN inverse transform (the CSV's printed
        // lon is rounded to 9 decimals, too coarse for a 1e-12 assertion)
        let inv = gl.inverse(row.i, row.j);
        let lon = inv.1;
        let dl = (lon - gl.lov + 540.0) % 360.0 - 180.0;
        let want = wrap_deg(90.0 + gl.ns * dl);
        let got = gl.rot_angle_deg(row.i, row.j);
        assert!(abs_diff(got, want) <= 1e-12);
    }
}

#[test]
fn rotation_shape_axes_are_distinct_earths() {
    // The two fixtures must really exercise BOTH earth models end to end:
    // CT 3.2 = 6 sphere, and CT 3.2 = 3 with the INLINE WGS84 axes parsed out
    // of section 3. If the cone constant did not differ, the oblate branch
    // would not be tested at all.
    let gs = wind_grid_of("hrrr_small.grib2");
    assert!(gs.earth_shape == 6);
    assert!(!gs.inline_axes);
    assert!(abs_diff(gs.r_major, 6371229.0) <= 1e-6);
    assert!(abs_diff(gs.r_minor, 6371229.0) <= 1e-6);
    assert!(abs_diff(gs.e, 0.0) <= 1e-12);
    let go = wind_grid_of("hrrr_ellip_small.grib2");
    assert!(go.earth_shape == 3);
    assert!(go.inline_axes);
    assert!(abs_diff(go.r_major, 6378137.0) <= 1e-6);
    assert!(abs_diff(go.r_minor, 6356752.31) <= 1e-6);
    assert!(go.e > 0.05 && go.e < 0.1);
    let dns = go.ns - gs.ns;
    assert!(if dns < 0.0 { -dns } else { dns } > 1e-4);
}
