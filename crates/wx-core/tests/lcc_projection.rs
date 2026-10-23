// LCC projection validation against wgrib2 `-ll2ij` truth for
// tests/fixtures/hrrr_small.grib2 (spec §9.2: 4 corners + center + >=20
// random points, within 1e-4 grid units).
//
// The values below mirror tests/fixtures/expected.json (fixtures/hrrr_small
// .grib2/ij_checks); regenerate with scripts/regen_expected.sh if fixtures
// change. wgrib2's -ll2ij returns 1-based i and j counted from the SOUTH
// edge; our storage convention is 0-based with row 0 at La1 (the north
// edge for this fixture, scan byte 0). Mapping (documented in grid.rs):
//   i_wg = i_store + 1,  j_wg = ny - j_store.

use wx_core::grid::GridLambert;
use wx_core::grid::GridDef;

const NY: usize = 41;

// lon, lat, i_wg, j_wg (wgrib2 -ll2ij output)
const CHECKS: [[f64; 4]; 25] = [
    [233.440000, 21.113890, 1.000000, 41.000000],
    [235.064820, 20.189420, 59.473405, 8.741953],
    [235.011450, 20.417740, 57.173318, 17.217010],
    [234.929580, 20.303340, 54.502304, 12.784808],
    [235.020990, 20.551940, 57.261651, 22.266474],
    [233.961710, 20.340520, 20.376653, 12.683610],
    [235.021810, 20.591830, 57.217017, 23.763524],
    [234.955610, 20.664550, 54.761382, 26.376338],
    [234.304200, 20.845030, 31.622658, 32.099996],
    [234.712880, 20.621230, 46.322888, 24.348567],
    [233.810030, 20.223080, 15.206717, 8.066544],
    [234.503020, 20.506930, 39.148863, 19.727714],
    [234.730490, 20.558580, 47.050040, 22.028077],
    [233.816020, 20.365370, 15.213178, 13.416652],
    [234.651690, 20.781520, 43.900567, 30.258231],
    [233.737570, 20.250090, 12.615304, 8.983964],
    [234.810880, 20.906010, 49.257977, 35.181850],
    [233.944190, 21.044120, 18.719287, 39.041991],
    [233.779160, 20.860000, 13.211645, 31.919616],
    [234.693750, 20.694210, 45.525310, 27.053523],
    [233.567660, 20.369060, 6.468314, 13.230127],
    [235.066240, 19.988880, 59.896091, 1.214410],
    [233.725810, 20.752590, 11.494089, 27.822650],
    [234.178160, 20.401440, 27.899425, 15.278985],
    [233.472980, 20.156480, 3.417678, 5.128706],
];

fn make_lcc() -> GridDef {
    // fixture header: nx=61 ny=41 La1=21.11389 Lo1=233.44 LoV=229.53
    // LaD=Latin1=26.5 Latin2=35.0 Dx=Dy=3000 m, projflag=1 (normal),
    // scan byte 0 (rows southward from La1), earth byte 6 (sphere).
    let g = GridLambert::new(61, 41, 21.11389, 233.44, 229.53, 26.5,
                             26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true);
    GridDef::Lambert(g)
}

fn abs_diff(a: f64, b: f64) -> f64 {
    let d = a - b;
    if d < 0.0 {
        return -d;
    }
    d
}

#[test]
fn lcc_matches_wgrib2_ll2ij() {
    let g = make_lcc();
    for row in CHECKS {
        let lon = row[0];
        let lat = row[1];
        let i_wg = row[2];
        let j_wg = row[3];
        let p = g.project(lat, lon);
        match p {
            None => { panic!(); }
            Some((di, dj)) => {
                // storage -> wgrib2 convention
                assert!(abs_diff(di + 1.0, i_wg) <= 1e-4);
                assert!(abs_diff((NY as f64) - dj, j_wg) <= 1e-4);
            }
        }
    }
}

#[test]
fn lcc_corner_center_coverage() {
    // the corner rows of CHECKS are the grid SW/NE-ish probes wgrib2 gave
    // us; also pin the exact stored node (0,0) == (Lo1, La1)
    let g = make_lcc();
    let p = g.project(21.11389, 233.44);
    match p {
        None => { panic!(); }
        Some((di, dj)) => {
            assert!(abs_diff(di, 0.0) <= 1e-9);
            assert!(abs_diff(dj, 0.0) <= 1e-9);
        }
    }
    // center of the domain: half-way indices
    let c = g.project(20.55194, 234.23);
    match c {
        None => { panic!(); }
        Some((di, dj)) => {
            assert!(di > 15.0 && di < 45.0);
            assert!(dj > 10.0 && dj < 30.0);
        }
    }
}

#[test]
fn lcc_inverse_roundtrip() {
    let gl = GridLambert::new(61, 41, 21.11389, 233.44, 229.53, 26.5,
                              26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true);
    let pts: [[f64; 2]; 4] = [
        [0.0, 0.0], [60.0, 40.0], [30.5, 20.25], [59.473405, 32.258047],
    ];
    for pt in pts {
        let (lat, lon) = gl.inverse(pt[0], pt[1]);
        let back = gl.project(lat, lon);
        match back {
            None => { panic!(); }
            Some((di, dj)) => {
                assert!(abs_diff(di, pt[0]) <= 1e-6);
                assert!(abs_diff(dj, pt[1]) <= 1e-6);
            }
        }
    }
}

// ===========================================================================
// V1.4 phase T additions: strict §9.2 golden pins for the FOUR CORNER NODES +
// CENTER NODE (the checks above pin only the NW node exactly and range-check
// the center area).
//
// External truth source: tests/fixtures/rotation_expected.csv -- committed
// table produced by scripts/validate_rotation.py with PYPROJ (independent
// implementation): each row stores the geographic coordinates of stored grid
// node (i,j) computed with pyproj's OWN LCC inverse on the fixture's exact
// earth shape (sphere R = 6371229 m from Code Table byte 6), origin pinned to
// (Lo1, La1) exactly like our GridLambert. expected.json's wgrib2 -ll2ij
// table covers random probe longitudes but NOT the corner/center nodes, so
// these rows are the external goldens for them; feeding the CSV lat/lon
// through OUR forward transform and requiring the printed (i,j) back within
// 1e-4 GRID UNITS is a genuine cross-implementation check (the same table
// already backs wx-ingest/tests/rotation_vs_pyproj.rs at 1e-6 deg for the
// wind angle). The CSV's i/j columns follow OUR storage convention (0-based
// i, j counted from the La1/north row -- verified: row (0,0) carries lat
// 21.11389 == La1, row (0,40) lat 20.0489 < it, so NO wgrib2 south-counting
// flip applies to this table). Rows marked SELF-CONSISTENCY below use our own
// inverse() and pin internal identities only, never external truth.
// ===========================================================================

use wx_core::grid::D2R;
use wx_core::grid::norm_lon;
use wx_core::tables::parse_f64;
use std::fs::read_to_string;

struct CsvRow {
    i: f64,
    j: f64,
    lat: f64,
    lon: f64,
}

fn hrrr_pyproj_rows() -> Vec<CsvRow> {
    let txt = match read_to_string("../../tests/fixtures/rotation_expected.csv") {
        Err(_e) => { panic!(); }
        Ok(t) => { t }
    };
    let mut out: Vec<CsvRow> = Vec::<CsvRow>::new();
    for ln0 in txt.split('\n') {
        // tolerate CRLF if the table is ever regenerated on Windows
        let ln: &str = if !ln0.is_empty() &&
                         ln0[ln0.len() - 1 .. ln0.len()] == *"\r" {
            &ln0[0 .. ln0.len() - 1]
        } else {
            ln0
        };
        let parts: Vec<&str> = ln.split(',').collect();
        if parts.len() < 6 || parts[0] != "hrrr_small.grib2" {
            continue;
        }
        let fi = parse_f64(parts[1]);
        let fj = parse_f64(parts[2]);
        let fl = parse_f64(parts[3]);
        let fo = parse_f64(parts[4]);
        if let (Some(a), Some(b), Some(la), Some(lo)) = (fi, fj, fl, fo) {
            out.push(CsvRow { i: a, j: b, lat: la, lon: lo });
        }
    }
    out
}

/// Ungated twin of GridLambert::project's arithmetic: the SAME forward
/// transform + linear scaling + scan fold, WITHOUT the §6.1 closed-node-
/// rectangle gate (which would reject on-boundary CSV points that print
/// rounding pushes ~1e-8 cells past an edge -- see the artifact test below).
/// §9.2 validates PROJECTION MATH against external truth; the domain gate is
/// §6.1 territory and is pinned separately in lcc_out_of_domain.rs.
fn fwd_ij(g: &GridLambert, lat: f64, lon: f64) -> (f64, f64) {
    let xy = g.for_point(norm_lon(lon) * D2R, lat * D2R);
    match xy {
        None => { panic!(); }
        Some((x, y)) => {
            let di = (x - g.x0) * g.inv_dx;
            let dj_north = (y - g.y0) * g.inv_dy;
            let dj = if g.j_south { -dj_north } else { dj_north };
            (di, dj)
        }
    }
}

#[test]
fn lcc_corner_and_center_nodes_match_pyproj_truth_1e_4() {
    // spec §9.2 literal demand: "the four corners, the center, and at least
    // 20 random points ... to within 1e-4 grid units" vs an EXTERNAL tool.
    // Every hrrr_small row in the pyproj table (corners (0,0),(60,0),(0,40),
    // (60,40), center (30,20) plus 16 random nodes) must survive our forward
    // transform within 1e-4 storage cells. External truth: PYPROJ, not our
    // own math (self-consistency pins are the round-trip test further down).
    let g = make_lcc();
    let gl = match g {
        GridDef::Lambert(ref gg) => { gg.clone() }
        _ => { panic!(); }
    };
    let rows = hrrr_pyproj_rows();
    assert!(rows.len() >= 20);
    // the five §9.2-mandated nodes must all be present in the table
    let wanted: [[f64; 2]; 5] = [
        [0.0, 0.0], [60.0, 0.0], [0.0, 40.0], [60.0, 40.0], [30.0, 20.0],
    ];
    for w in wanted {
        let mut found = false;
        for r in &rows {
            if r.i == w[0] && r.j == w[1] {
                found = true;
                break;
            }
        }
        if !found {
            println!("corner/center node ({0},{1}) missing from pyproj table", w[0], w[1]);
            panic!();
        }
    }
    // the table's NW row must agree with the fixture's declared Lo1/La1
    for r in &rows {
        if r.i == 0.0 && r.j == 0.0 {
            assert!(abs_diff(r.lat, 21.11389) <= 1e-9);
            assert!(abs_diff(norm_lon(r.lon), 233.44) <= 1e-9);
        }
    }
    let mut worst = 0.0;
    for r in &rows {
        let p = fwd_ij(&gl, r.lat, r.lon);
        let dxi = abs_diff(p.0, r.i);
        let dxj = abs_diff(p.1, r.j);
        let d = if dxi > dxj { dxi } else { dxj };
        if d > worst {
            worst = d;
        }
        if d > 1e-4 {
            println!("node ({0},{1}): our forward gives ({2},{3}), off from pyproj by {4} cells",
                     r.i, r.j, p.0, p.1, d);
            panic!();
        }
    }
    println!("pyproj corner/center/random forwards: {0} rows, worst {1} grid units (limit 1e-4)",
             rows.len(), worst);
}

#[test]
fn lcc_pyproj_negative_lon_forms_project_identically() {
    // The CSV prints longitudes in -180..180 form; §4.3 requires 0..360 to
    // behave identically. Through the PUBLIC domain-checked project(), both
    // conventions must return the SAME Option bit-for-bit on every external
    // row (Some==Some or None==None -- the boundary artifact below applies
    // equally to both forms).
    let g = make_lcc();
    let gl = match g {
        GridDef::Lambert(ref gg) => { gg.clone() }
        _ => { panic!(); }
    };
    for r in hrrr_pyproj_rows() {
        let a = gl.project(r.lat, r.lon);
        let b = gl.project(r.lat, r.lon + 360.0);
        if a != b {
            println!("row ({0},{1}): -180 form {2:?} vs 0..360 form {3:?}", r.i, r.j, a, b);
            panic!();
        }
    }
}

#[test]
fn lcc_pyproj_boundary_node_print_artifact_pinned() {
    // Pin a documented HEAD subtlety so nobody silently widens PROJ_NODE_EPS:
    // pyproj printed the NE node (60,0) to 9 decimals; that quantization
    // (~1.1e-8 grid cells ~ 32 um north of the La1 row, measured) sits
    // OUTSIDE the closed node rectangle's 1e-9 eps, so the DOMAIN-GATED
    // project() rejects the printed coordinate while the ungated projection
    // MATH still reproduces (60,0) at 1.1e-8 << 1e-4 (golden test above).
    // This is a FIXTURE PRINT artifact, not a projection error and not a
    // spec behaviour change. The other four goldened nodes land slightly on
    // the INSIDE of the rectangle and are accepted within 1e-4 -- pinned too.
    let g = make_lcc();
    let gl = match g {
        GridDef::Lambert(ref gg) => { gg.clone() }
        _ => { panic!(); }
    };
    let rows = hrrr_pyproj_rows();
    let mut ne_seen = false;
    for r in &rows {
        let corner_or_center = ((r.i == 0.0 || r.i == 60.0) && (r.j == 0.0 || r.j == 40.0))
                             || (r.i == 30.0 && r.j == 20.0);
        if !corner_or_center {
            continue;
        }
        if r.i == 60.0 && r.j == 0.0 {
            ne_seen = true;
            if gl.project(r.lat, r.lon).is_some() {
                println!("NE-node printed coord accepted: did PROJ_NODE_EPS widen?");
                panic!();
            }
        } else {
            match gl.project(r.lat, r.lon) {
                None => {
                    println!("node ({0},{1}) printed coord rejected", r.i, r.j);
                    panic!();
                }
                Some((di, dj)) => {
                    assert!(abs_diff(di, r.i) <= 1e-4 && abs_diff(dj, r.j) <= 1e-4);
                }
            }
        }
    }
    if !ne_seen {
        println!("no (60,0) row in table");
        panic!();
    }
}

#[test]
fn lcc_corner_and_center_nodes_self_roundtrip() {
    // SELF-CONSISTENCY pin (explicitly NOT external truth): inverse() at each
    // of the five mandated nodes yields full-f64 coordinates (no 9-decimal
    // print quantization) that the public domain-checked project() must send
    // back to the exact node within 1e-6 cells -- i.e. all five corner/center
    // NODES lie inside the accepted rectangle and round-trip exactly (all
    // five errors measured <= 7.4e-13). External-truth coverage for the same
    // points is the pyproj golden test above.
    let gl = GridLambert::new(61, 41, 21.11389, 233.44, 229.53, 26.5,
                              26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true);
    let pts: [[f64; 2]; 5] = [
        [0.0, 0.0], [60.0, 0.0], [0.0, 40.0], [60.0, 40.0], [30.0, 20.0],
    ];
    for pt in pts {
        let pos = gl.inverse(pt[0], pt[1]);
        match gl.project(pos.0, pos.1) {
            None => {
                println!("exact node ({0},{1}) rejected via its own inverse coords", pt[0], pt[1]);
                panic!();
            }
            Some((di, dj)) => {
                assert!(abs_diff(di, pt[0]) <= 1e-6);
                assert!(abs_diff(dj, pt[1]) <= 1e-6);
            }
        }
    }
}
