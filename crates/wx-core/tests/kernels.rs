// Interpolation + projection kernel tests (spec §9 items 4-7 for synthetic grids).
use wx_core::grid::GridRegular;
use wx_core::grid::regular_ij;
use wx_core::interp::bilinear;
use wx_core::interp::vertical;
use wx_core::interp::time_lerp;
use wx_core::interp::nan_f64;

#[test]
fn exact_and_linear_field() {
    // field linear in x and y: v = 10*j + i
    let field = [0.0, 1.0, 2.0, 10.0, 11.0, 12.0, 20.0, 21.0, 22.0];
    // exact node -> no blending
    assert_eq!(bilinear(&field, 3, 3, 1.0, 1.0), 11.0);
    // linear-in-x field reproduced exactly at arbitrary points
    assert_eq!(bilinear(&field, 3, 3, 0.5, 0.0), 0.5);
    // bilinear of an affine field is exact everywhere
    assert_eq!(bilinear(&field, 3, 3, 0.25, 0.75), 7.75);
}

#[test]
fn nan_neighbors_return_nan() {
    let field = [nan_f64(), 1.0, 2.0, 10.0, 11.0, 12.0, 20.0, 21.0, 22.0];
    let got = bilinear(&field, 3, 3, 0.5, 0.5);
    assert!(got.is_nan());
}

#[test]
fn vertical_linear_in_pressure() {
    let levels = [700.0, 850.0, 1000.0]; // ascending (spec §4.5 sorts by pressure)
    let vals = [270.0, 260.0, 250.0];
    assert_eq!(vertical(&levels, &vals, 850.0), 260.0);
    assert_eq!(vertical(&levels, &vals, 925.0), 255.0);
}

#[test]
fn time_lerp_exact_and_mid() {
    assert_eq!(time_lerp(10.0, 0.0, 20.0, 6.0, 0.0), 10.0);
    assert_eq!(time_lerp(10.0, 0.0, 20.0, 6.0, 3.0), 15.0);
}

#[test]
fn projection_north_to_south() {
    // GFS-like geometry, N->S scan (scan_mode 0 => rows go south)
    let g = GridRegular {
        nx: 1440, ny: 720, lon1: 0.0, lat1: 89.875,
        dlon: 0.25, dlat: 0.25, scan_mode: 0, winds_grid: false,
    };
    let got = regular_ij(&g, -0.125, 359.9);
    assert!(got.is_some());
    let pair = got.unwrap();
    assert!(pair.1 >= 359.5 && pair.1 <= 360.5);
    assert!(pair.0 >= 1439.0 && pair.0 < 1440.0);
}

#[test]
fn seam_wrap() {
    let g = GridRegular {
        nx: 1440, ny: 720, lon1: 0.0, lat1: 89.875,
        dlon: 0.25, dlat: 0.25, scan_mode: 0, winds_grid: false,
    };
    // lon -0.1 normalizes to 359.9
    let got = regular_ij(&g, 0.0, -0.1);
    assert!(got.is_some());
    assert!(got.unwrap().0 > 1439.0);

    // regional grid starting at lon 350 wrapping east past 0
    let r = GridRegular {
        nx: 41, ny: 21, lon1: 350.0, lat1: 10.0,
        dlon: 0.5, dlat: 0.5, scan_mode: 0, winds_grid: false,
    };
    let got2 = regular_ij(&r, 5.0, 2.0);
    assert!(got2.is_some());
    assert!(got2.unwrap().0 > 23.0 && got2.unwrap().0 < 25.0);
}
