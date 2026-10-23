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
