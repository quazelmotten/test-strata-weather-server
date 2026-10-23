// V1.4 phase T: OFFLINE LCC out-of-domain coverage (spec §9.6 "poles and
// edges defined and tested"). Before this file, Lambert domain behaviour was
// gated behind network-dependent realdata probes and manual smoke scripts;
// kernels.rs `out_of_domain_edges` covers the REGULAR grid only. This test
// drives GridDef::project with pure math (no file I/O), so it runs offline.
//
// Geometry: tests/fixtures/hrrr_small.grib2's template-3.30 header
// (NOTES-fixtures.md): nx=61 ny=41 La1=21.11389 Lo1=233.44 LoV=229.53
// LaD=Latin1=26.5 Latin2=35.0 Dx=Dy=3000 m projflag=1 scan byte 0 (stored
// rows run SOUTH from La1, so dj=0 is the NORTH row and dj=ny-1 the south
// row), earth byte 6 (sphere 6371229 m), grid-relative winds. Identical
// constructor to lcc_projection.rs's make_lcc().
//
// Rule pinned (grid.rs module docs lines 21-29, README interpolation
// semantics): the accepted domain is the CLOSED node rectangle
// [0, nx-1] x [0, ny-1] in fractional storage units plus PROJ_NODE_EPS=1e-9
// node tolerance; the last HALF-CELL beyond the outermost node row/column at
// a non-wrapping edge is OUT of domain (deliberately stricter than wgrib2's
// nearest-neighbour half-cell rule). Longitude never wraps on LCC
// (GridDef::lon_wrap() == false); lat +-90 are cone poles -> None.
//
// Probe derivation: every REJECT is paired with an adjacent ACCEPT at the
// same coordinate line, +-0.001 grid cells (3 m) across the boundary, placed
// via GridLambert::inverse to obtain geographic coordinates (probe POSITIONS
// self-derived; the asserted property is the documented boundary itself).
// Measured inverse->forward round-trip noise <= ~1e-10 cells, ~1e7x below
// the 0.001-cell offset, so no probe can straddle the eps gate by accident.
// The eps-band test uses ONLY the declared fixture constants (raw La1/Lo1
// arithmetic), no inverse round-trips at all.

use wx_core::grid::GridDef;
use wx_core::grid::GridLambert;

fn hrrr_like() -> GridLambert {
    GridLambert::new(61, 41, 21.11389, 233.44, 229.53, 26.5,
                     26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true)
}

const NX_LAST: f64 = 60.0; // nx - 1
const NY_LAST: f64 = 40.0; // ny - 1
const MID_I: f64 = 30.0;
const MID_J: f64 = 20.0;
const HAIR: f64 = 1e-3; // 3 m at Dx=Dy=3000 m
const HAIR_TOL: f64 = 1e-4; // accept-side reproduction tolerance (cells)

fn abs_diff(a: f64, b: f64) -> f64 {
    let d = a - b;
    if d < 0.0 {
        return -d;
    }
    d
}

// Project through BOTH dispatch paths (struct method and GridDef enum) and
// pin that they agree bit-for-bit before handing the value back.
fn proj_pair(gl: &GridLambert, gd: &GridDef, lat: f64, lon: f64) -> Option<(f64, f64)> {
    let a = gl.project(lat, lon);
    let b = gd.project(lat, lon);
    if a != b {
        println!("dispatch drift at ({0},{1}): struct {2:?} vs enum {3:?}", lat, lon, a, b);
        panic!();
    }
    a
}

// Assert the node position (di_in,dj_in)'s geographic coords project INSIDE
// the node rectangle near (di_want,dj_want).
fn accept(gl: &GridLambert, gd: &GridDef, di_in: f64, dj_in: f64,
          di_want: f64, dj_want: f64, label: &str) {
    let pos = gl.inverse(di_in, dj_in);
    match proj_pair(gl, gd, pos.0, pos.1) {
        None => {
            println!("{0}: inverse({1},{2})=({3},{4}) rejected but must be IN",
                     label, di_in, dj_in, pos.0, pos.1);
            panic!();
        }
        Some((di, dj)) => {
            if abs_diff(di, di_want) > HAIR_TOL || abs_diff(dj, dj_want) > HAIR_TOL {
                println!("{0}: landed at ({1},{2}), want ~({3},{4})",
                         label, di, dj, di_want, dj_want);
                panic!();
            }
        }
    }
}

// Assert (di_out,dj_out)'s geographic coords -- a hair (or deeper) beyond
// the node rectangle -- are rejected (None) on BOTH dispatch paths.
fn reject(gl: &GridLambert, gd: &GridDef, di_out: f64, dj_out: f64, label: &str) {
    let pos = gl.inverse(di_out, dj_out);
    if let Some((di, dj)) = proj_pair(gl, gd, pos.0, pos.1) {
        println!("{0}: inverse({1},{2})=({3},{4}) accepted at ({5},{6}) but must be OUT",
                 label, di_out, dj_out, pos.0, pos.1, di, dj);
        panic!();
    }
}

#[test]
fn lcc_four_edges_reject_a_hair_outside_accept_a_hair_inside() {
    // spec §6.1/§9.6 + grid.rs domain docs: closed node rectangle, never
    // extrapolate. Every reject below is paired with an accept 0.001 cells
    // (~3 m) deeper on the same coordinate line.
    let gl = hrrr_like();
    let gd = GridDef::Lambert(gl.clone());
    // WEST edge (i = 0 column): inside col +0.001, outside -0.001.
    accept(&gl, &gd, HAIR, MID_J, HAIR, MID_J, "west-in");
    reject(&gl, &gd, -HAIR, MID_J, "west-out");
    // EAST edge (i = nx-1 column): inside 60-0.001, outside 60+0.001.
    accept(&gl, &gd, NX_LAST - HAIR, MID_J, NX_LAST - HAIR, MID_J, "east-in");
    reject(&gl, &gd, NX_LAST + HAIR, MID_J, "east-out");
    // NORTH edge (dj = 0 row == La1 side because scan byte 0 stores rows
    // southward): inside +0.001, outside -0.001.
    accept(&gl, &gd, MID_I, HAIR, MID_I, HAIR, "north-in");
    reject(&gl, &gd, MID_I, -HAIR, "north-out");
    // SOUTH edge (dj = ny-1 row): inside 40-0.001, outside 40+0.001.
    accept(&gl, &gd, MID_I, NY_LAST - HAIR, MID_I, NY_LAST - HAIR, "south-in");
    reject(&gl, &gd, MID_I, NY_LAST + HAIR, "south-out");
}

#[test]
fn lcc_last_half_cell_beyond_outer_nodes_is_out_of_domain() {
    // The DOCUMENTED stricter-than-wgrib2 rule (grid.rs lines 24-26, README
    // "incl. the last half-cell at non-wrapping edges"): wgrib2's nearest-
    // neighbour lookup would ACCEPT anything within half a cell of the outer
    // node row/column; we reject it. Each rejection stays paired with an
    // interior mid-cell accept on the same line (0.5 cells INSIDE the node
    // rectangle, between the outermost and second-outermost nodes).
    let gl = hrrr_like();
    let gd = GridDef::Lambert(gl.clone());
    reject(&gl, &gd, -0.5, MID_J, "west half-cell");
    accept(&gl, &gd, 0.5, MID_J, 0.5, MID_J, "west mid-cell in");
    reject(&gl, &gd, NX_LAST + 0.5, MID_J, "east half-cell");
    accept(&gl, &gd, NX_LAST - 0.5, MID_J, NX_LAST - 0.5, MID_J, "east mid-cell in");
    reject(&gl, &gd, MID_I, -0.5, "north half-cell");
    accept(&gl, &gd, MID_I, 0.5, MID_I, 0.5, "north mid-cell in");
    reject(&gl, &gd, MID_I, NY_LAST + 0.5, "south half-cell");
    accept(&gl, &gd, MID_I, NY_LAST - 0.5, MID_I, NY_LAST - 0.5, "south mid-cell in");
}

#[test]
fn lcc_corner_diagonals_reject_outside_and_accept_inside() {
    // All four CORNERS of the node rectangle: diagonal probes a hair outside
    // both coordinates must be rejected (either violated coordinate alone
    // suffices; the diagonal violates two), and the mirrored diagonal a hair
    // inside both must be accepted near (0,0)/(60,0)/(0,40)/(60,40).
    let gl = hrrr_like();
    let gd = GridDef::Lambert(gl.clone());
    reject(&gl, &gd, -HAIR, -HAIR, "NW corner out");
    accept(&gl, &gd, HAIR, HAIR, HAIR, HAIR, "NW corner in");
    reject(&gl, &gd, NX_LAST + HAIR, -HAIR, "NE corner out");
    accept(&gl, &gd, NX_LAST - HAIR, HAIR, NX_LAST - HAIR, HAIR, "NE corner in");
    reject(&gl, &gd, -HAIR, NY_LAST + HAIR, "SW corner out");
    accept(&gl, &gd, HAIR, NY_LAST - HAIR, HAIR, NY_LAST - HAIR, "SW corner in");
    reject(&gl, &gd, NX_LAST + HAIR, NY_LAST + HAIR, "SE corner out");
    accept(&gl, &gd, NX_LAST - HAIR, NY_LAST - HAIR, NX_LAST - HAIR, NY_LAST - HAIR, "SE corner in");
}

#[test]
fn lcc_node_eps_band_pinned_at_declared_northwest_corner() {
    // Pure declared-fixture-constant arithmetic (NO inverse round-trip), so
    // the PROJ_NODE_EPS = 1e-9 gate and the clamp-to-node behaviour are hit
    // exactly. At the NORTH edge dj = (y0 - y)/dy: a latitude north of La1
    // gives dj < 0 and is rejected once |dj| exceeds the eps band (~1e-9
    // cells ~ 3 micrometres); south of La1 within the band the point is
    // ACCEPTED and clamped onto dj = 0.
    let gl = hrrr_like();
    let gd = GridDef::Lambert(gl.clone());
    let la1 = 21.11389;
    let lo1 = 233.44;
    // exact declared node (Lo1, La1) -> exactly (0, 0)
    match proj_pair(&gl, &gd, la1, lo1) {
        Some((di, dj)) => {
            if abs_diff(di, 0.0) > 1e-9 || abs_diff(dj, 0.0) > 1e-9 {
                println!("NW node landed at ({0},{1})", di, dj);
                panic!();
            }
        }
        None => {
            println!("declared node (Lo1,La1) rejected");
            panic!();
        }
    }
    // ~1 cm north of the node ROW (1e-7 deg ~ 3.7e-6 cells >> eps): REJECT.
    // wgrib2's nearest-neighbour rule would snap this onto row 0; we do not.
    if proj_pair(&gl, &gd, la1 + 1e-7, lo1).is_some() {
        println!("lat La1+1e-7 accepted but sits ~3.7e-6 cells north of row 0");
        panic!();
    }
    // same magnitude SOUTH of the row: accept, dj just inside the rectangle.
    // Measured dj ~= 3.746e-6 cells (conformal scale k ~ 1.008 at this lat).
    match proj_pair(&gl, &gd, la1 - 1e-7, lo1) {
        Some((di, dj)) => {
            if abs_diff(di, 0.0) > 1e-4 || !(dj > 1e-6 && dj < 1e-4) {
                println!("lat La1-1e-7 landed at ({0},{1}), want dj in (1e-6,1e-4)", di, dj);
                panic!();
            }
        }
        None => {
            println!("lat La1-1e-7 rejected but should be just INSIDE row 0");
            panic!();
        }
    }
    // Inside the eps band (1e-12 deg ~ 3.7e-11 cells < eps): both sides are
    // ACCEPTED. North side: dj negative-but-tolerated, CLAMPED to exactly 0
    // (and di for the same reason); south side: raw tiny positive values
    // (measured 1.28e-12 / 3.66e-11 -- returned unclamped, both < eps).
    match proj_pair(&gl, &gd, la1 + 1e-12, lo1) {
        Some((di, dj)) => {
            if di != 0.0 || dj != 0.0 {
                println!("lat La1+1e-12 landed at ({0},{1}); both must clamp to exactly 0", di, dj);
                panic!();
            }
        }
        None => {
            println!("lat La1+1e-12 rejected but is within PROJ_NODE_EPS of the node");
            panic!();
        }
    }
    match proj_pair(&gl, &gd, la1 - 1e-12, lo1) {
        Some((di, dj)) => {
            if abs_diff(di, 0.0) > 1e-9 || abs_diff(dj, 0.0) > 1e-9 {
                println!("lat La1-1e-12 landed at ({0},{1}), want both within 1e-9 of 0", di, dj);
                panic!();
            }
        }
        None => {
            println!("lat La1-1e-12 rejected but is within PROJ_NODE_EPS of the node");
            panic!();
        }
    }
}

#[test]
fn lcc_poles_on_hrrr_geometry_are_out_of_domain() {
    // §9.6 poles on LCC: lat +-90 are the cone's unprojectable points
    // (for_point returns None by pole guard) -> defined as failure, whatever
    // the longitude. Beyond +-90 the plain range guard rejects first.
    let gl = hrrr_like();
    let gd = GridDef::Lambert(gl.clone());
    let probes: [[f64; 2]; 6] = [
        [90.0, 233.44], [90.0, 229.53], [-90.0, 229.53],
        [-90.0, 233.44], [90.0000001, 0.0], [-90.0000001, 0.0],
    ];
    for p in probes {
        if proj_pair(&gl, &gd, p[0], p[1]).is_some() {
            println!("pole probe ({0},{1}) accepted on HRRR geometry", p[0], p[1]);
            panic!();
        }
    }
}

#[test]
fn lcc_lon_normalization_both_conventions_around_domain() {
    // §4.3: "-180..180 and 0..360 both accepted". On LCC the two forms of the
    // SAME meridian must project BIT-IDENTICALLY (norm_lon folds them onto
    // one double before any math), and a point beyond the horizontal edges
    // stays OUT in either form -- longitude never wraps here (unlike global
    // grids), so longitudes far outside cannot fold back into the domain.
    let gl = hrrr_like();
    let gd = GridDef::Lambert(gl.clone());
    if gd.lon_wrap() {
        println!("lon_wrap true on Lambert grid");
        panic!();
    }
    // nodes: exact corners + center, geographic coords from our own inverse
    let nodes: [[f64; 2]; 5] = [
        [0.0, 0.0], [NX_LAST, 0.0], [0.0, NY_LAST], [NX_LAST, NY_LAST], [MID_I, MID_J],
    ];
    for nd in nodes {
        let pos = gl.inverse(nd[0], nd[1]);
        let a = proj_pair(&gl, &gd, pos.0, pos.1);
        let b = proj_pair(&gl, &gd, pos.0, pos.1 - 360.0);
        if a != b {
            println!("node ({0},{1}): 0..360 form {2:?} vs -180..180 form {3:?}",
                     nd[0], nd[1], a, b);
            panic!();
        }
    }
    // hair-outside edge probes in BOTH longitude conventions
    let west = gl.inverse(-HAIR, MID_J);
    if proj_pair(&gl, &gd, west.0, west.1).is_some()
        || proj_pair(&gl, &gd, west.0, west.1 - 360.0).is_some() {
        println!("west hair-out accepted in some lon convention");
        panic!();
    }
    let east = gl.inverse(NX_LAST + HAIR, MID_J);
    if proj_pair(&gl, &gd, east.0, east.1).is_some()
        || proj_pair(&gl, &gd, east.0, east.1 - 360.0).is_some() {
        println!("east hair-out accepted in some lon convention");
        panic!();
    }
    // far-off longitudes (+-45 deg from the edge nodes) stay out in both
    // forms: no wrapping rescue on a non-global grid.
    let w_node = gl.inverse(0.0, MID_J);
    if proj_pair(&gl, &gd, w_node.0, w_node.1 - 45.0).is_some()
        || proj_pair(&gl, &gd, w_node.0, w_node.1 - 45.0 + 360.0).is_some() {
        println!("west-45deg longitude folded into a non-wrapping domain");
        panic!();
    }
    let e_node = gl.inverse(NX_LAST, MID_J);
    if proj_pair(&gl, &gd, e_node.0, e_node.1 + 45.0).is_some()
        || proj_pair(&gl, &gd, e_node.0, e_node.1 + 45.0 - 360.0).is_some() {
        println!("east+45deg longitude folded into a non-wrapping domain");
        panic!();
    }
}
