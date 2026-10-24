// V1.4 phase C gate #1 (risk register #9): the two silent-wrong-answer classes
// that live in GRID-DEPENDENT metadata, verified offline (no GRIB fixtures,
// no network).
//
// (1) HEADER-vs-GRID GEOMETRY. A field .bin header carries its own `nx`/`ny`
//     and scan flag, and NOTHING used to compare them against the run's
//     grid.bin. The query path walks the mmapped planes with the GRID's extent
//     (`evaluate_point` takes nx/ny from `GridDef`) while `MmapField::value()`
//     computes each plane offset from the HEADER's — so a header whose strides
//     disagree reads real bytes from the wrong position and answers a plausible
//     number. `wx_core::storage::header_geometry_reason` is the single predicate
//     both the index build and these pins use. Note the deliberately interesting
//     case: a swapped 61x41 <-> 41x61 pair preserves the FILE LENGTH, so the
//     exact-length check in `open_field` cannot see it either — only this
//     cross-check can.
//
// (2) THE TYPED LCC DEGENERACY ERROR. `prepare()` used to `.unwrap()` the
//     projected position of the stored point (Lo1, La1) — the ONLY
//     input-dependent unwrap in the crate. With the release profile's
//     `panic = "abort"` (spec §8), eight rotten grid.bin bytes setting La1 to a
//     cone pole aborted the whole daemon at warm boot. It now returns
//     `GridError`, and the infallible constructor POISONS the grid instead:
//     every projection is out-of-domain, never a clamped-onto-node fiction.
//     That NaN-rejection is free: the domain bounds are the same two comparisons
//     written accept-side, which for every FINITE value is bit-for-bit the old
//     reject-side form (De Morgan), while NaN/infinity — which slipped through
//     both comparisons before — now falls outside.

// Matching pedantic-but-cosmetic lints are allowed per-file, narrowly listed
// (same convention as the src modules and the phase B bounds gates).
#![allow(clippy::needless_return)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::single_match)]

use wx_core::grid::{GridDef, GridError, GridLambert, GridRegular};
use wx_core::storage::{BinHeader, StatMeta, header_geometry_reason};

// HRRR-fixture template-3.30 constants (tests/fixtures/hrrr_small.grib2 per
// NOTES-fixtures.md), identical to lcc_projection.rs / lcc_out_of_domain.rs.
fn hrrr_like() -> GridLambert {
    return GridLambert::new(61, 41, 21.11389, 233.44, 229.53, 26.5,
                            26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true);
}

fn regular(nx: u32, ny: u32, scan_mode: u8) -> GridDef {
    return GridDef::Regular(GridRegular {
        nx: nx, ny: ny, lon1: 230.0, lat1: 45.0, dlon: 0.25, dlat: 0.25,
        scan_mode: scan_mode, winds_grid: false,
    });
}

fn hdr(nx: u32, ny: u32, scan_ns: bool, levels: &[f32]) -> BinHeader {
    return BinHeader {
        nx: nx,
        ny: ny,
        levels: levels.to_vec(),
        scan_ns: scan_ns,
        winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
}

// ---------------------------------------------------------------------------
// (1) geometry cross-validation predicate
// ---------------------------------------------------------------------------

#[test]
fn matching_header_and_grid_are_accepted() {
    let g = regular(720, 600, 0x00); // rows go south: scan_ns true
    let h = hdr(720, 600, true, &[1000.0, 850.0]);
    assert!(header_geometry_reason(&h, &g).is_none());
    // north-scanning variant agrees with a scan_ns=false header
    let gn = regular(720, 600, 0x40);
    let hn = hdr(720, 600, false, &[1000.0, 850.0]);
    assert!(header_geometry_reason(&hn, &gn).is_none());
    // and the Lambert fixture agrees with itself
    let gl = GridDef::Lambert(hrrr_like());
    let hl = hdr(61, 41, true, &[1000.0]);
    assert!(header_geometry_reason(&hl, &gl).is_none());
}

#[test]
fn every_geometry_disagreement_is_named_by_class() {
    let g = regular(720, 600, 0x00);
    let why_nx = header_geometry_reason(&hdr(721, 600, true, &[1000.0]), &g);
    let why_ny = header_geometry_reason(&hdr(720, 599, true, &[1000.0]), &g);
    let why_scan = header_geometry_reason(&hdr(720, 600, false, &[1000.0]), &g);
    match why_nx {
        None => { panic!("nx mismatch went unnoticed"); }
        Some(t) => { assert!(t.contains("nx")); }
    }
    match why_ny {
        None => { panic!("ny mismatch went unnoticed"); }
        Some(t) => { assert!(t.contains("ny")); }
    }
    match why_scan {
        None => { panic!("scan-flag mismatch went unnoticed"); }
        Some(t) => { assert!(t.contains("scan")); }
    }
    // THE class the file-length check is blind to: swapping the extent keeps
    // nx*ny (hence the byte count) identical, so only this comparison catches
    // the transposed read.
    let gl = GridDef::Lambert(hrrr_like());
    match header_geometry_reason(&hdr(41, 61, true, &[1000.0]), &gl) {
        None => { panic!("transposed 61x41 <-> 41x61 extent went unnoticed"); }
        Some(_) => {}
    }
    // A rotted scan bit silently mirrors every row north-south: named too.
    match header_geometry_reason(&hdr(61, 41, false, &[1000.0]), &gl) {
        None => { panic!("scan-flag mismatch on lambert went unnoticed"); }
        Some(t) => { assert!(t.contains("scan")); }
    }
}

// ---------------------------------------------------------------------------
// (2) typed degeneracy error
// ---------------------------------------------------------------------------

#[test]
fn degenerate_lambert_construction_reports_a_value_not_a_panic() {
    // La1 exactly on the north pole: no anchor exists (`for_point` refuses the
    // cone poles), which is precisely the input that used to abort.
    let polarn = GridLambert::try_new(61, 41, 90.0, 233.44, 229.53, 26.5,
                                      26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true);
    match polarn {
        Ok(_g) => { panic!("La1=+90 built a usable projection"); }
        Err(e) => { assert!(e == GridError::DegenerateAnchor); }
    }
    let polars = GridLambert::try_new(61, 41, -90.0, 233.44, 229.53, 26.5,
                                      26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true);
    match polars {
        Ok(_g) => { panic!("La1=-90 built a usable projection"); }
        Err(e) => { assert!(e == GridError::DegenerateAnchor); }
    }
    // Zero cell size: inv_dx/inv_dy go infinite. No anchor problem at all, but
    // equally unusable — and pre-phase-C this was NOT even an unwrap site, it
    // simply produced inf strides that answered wrong forever.
    let zdx = GridLambert::try_new(61, 41, 21.11389, 233.44, 229.53, 26.5,
                                   26.5, 35.0, 0.0, 3000.0, 1, 0, 6, true);
    match zdx {
        Ok(_g) => { panic!("dx_m = 0 built a usable projection"); }
        Err(e) => { assert!(e == GridError::NonFiniteConstants); }
    }
    // Bit rot that lands NaN (or an infinity) in one of the f64 template slots.
    let nala = GridLambert::try_new(61, 41, f64::NAN, 233.44, 229.53, 26.5,
                                    26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true);
    match nala {
        Ok(_g) => { panic!("NaN La1 built a usable projection"); }
        Err(e) => { assert!(e == GridError::NonFiniteConstants); }
    }
    let inflat1 = GridLambert::try_new(61, 41, 21.11389, 233.44, 229.53, 26.5,
                                       f64::INFINITY, 35.0, 3000.0, 3000.0, 1, 0, 6, true);
    match inflat1 {
        Ok(_g) => { panic!("infinite Latin1 built a usable projection"); }
        Err(_e) => {}
    }
    // The healthy fixture still constructs AND reports Ok: the guards must not
    // turn real HRRR/GFS grids into errors.
    let good = GridLambert::try_new(61, 41, 21.11389, 233.44, 229.53, 26.5,
                                    26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true);
    match good {
        Err(e) => { println!("healthy HRRR grid rejected: {0:?}", e); panic!(); }
        Ok(g) => { assert!(g.anchors_ok()); }
    }
}

#[test]
fn infallible_constructor_poisons_instead_of_aborting() {
    // This call is the regression pin: pre-phase-C it PANICKED (and under
    // `panic = "abort"` took the process with it).
    let g = GridLambert::new(61, 41, 90.0, 233.44, 229.53, 26.5,
                             26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true);
    assert!(!g.anchors_ok());
    let gd = GridDef::Lambert(g);
    // Every probe, including the point that WOULD have been the anchor, is out
    // of domain. A poisoned grid never returns a number, wrong or right.
    let probes_lat = [21.11389, 26.5, 35.0, 41.0, 0.0, -30.0, 89.9];
    let probes_lon = [233.44, 229.53, 260.0, 275.5, 0.0, 359.9];
    for k in 0 .. probes_lat.len() {
        for m in 0 .. probes_lon.len() {
            if gd.project(probes_lat[k], probes_lon[m]).is_some() {
                panic!("poisoned lambert answered ({0},{1})", probes_lat[k], probes_lon[m]);
            }
        }
    }
    // Its RAW geometry stays honest (only derived constants are poisoned), so
    // the index cross-check still compares against the declared extent and row
    // order rather than against zeros.
    match gd {
        GridDef::Lambert(ref gg) => {
            assert!(gg.nx == 61 && gg.ny == 41);
            assert!(gg.j_south); // scan byte 0: stored rows run south from La1
        }
        _ => { panic!(); }
    }
    assert!(gd.nx() == 61 && gd.ny() == 41 && gd.scan_ns());
}

#[test]
fn poisoning_does_not_move_the_domain_of_a_healthy_grid() {
    // The accept-form rewrite must be conservative: reproduce the documented
    // boundary behaviour on a HEALTHY grid (module docs lines 21-29 and
    // lcc_out_of_domain.rs): node-exact corners land exactly, a hair beyond the
    // last node is out, and nothing in between shifts.
    let g = hrrr_like();
    let c0 = g.project(21.11389, 233.44);
    match c0 {
        None => { panic!("the (Lo1, La1) node left its own grid"); }
        Some((di, dj)) => {
            if di != 0.0 || dj != 0.0 {
                println!("corner drift: {0} {1}", di, dj);
                panic!();
            }
        }
    }
    // inverse -> forward round trip at the far corner still lands inside
    let (lat_n, lon_n) = g.inverse(60.0, 40.0);
    match g.project(lat_n, lon_n) {
        None => { panic!("node (60,40) left the grid through a round trip"); }
        Some((di, dj)) => {
            let ei = if di - 60.0 < 0.0 { 60.0 - di } else { di - 60.0 };
            let ej = if dj - 40.0 < 0.0 { 40.0 - dj } else { dj - 40.0 };
            if ei > 1e-6 || ej > 1e-6 {
                println!("round-trip drift: {0} {1}", di, dj);
                panic!();
            }
        }
    }
    // A point far outside the cone stays out (no clamping fiction).
    assert!(g.project(-45.0, 15.0).is_none());
    assert!(g.project(89.999999, 229.53).is_none());
}
