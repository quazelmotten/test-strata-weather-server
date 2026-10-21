//! Grid definitions and lat/lon -> fractional (i, j) projections (spec §4.3).
//!
//! ## Index conventions (IMPORTANT — read before using)
//!
//! * Fractional coordinates returned by [`GridDef::project`] are **0-based
//!   storage indices**: `i` is the column position along the GRIB i-scan
//!   direction and `j` is the row position in *stored order* (row 0 is the
//!   first row in the file, i.e. the La1/lat1 side). The bilinear kernels
//!   consume these directly and never have to care about N->S vs S->N
//!   scanning: the scan direction is folded into the projection once, per
//!   grid.
//! * wgrib2 `-ll2ij` returns **1-based** `(i, j)` with `j` counted from the
//!   south edge for our N->S Lambert fixtures (verified empirically against
//!   `tools/wgrib2` on `tests/fixtures/hrrr_small.grib2`). Mapping:
//!   i_wgrib2 = i_store + 1,   j_wgrib2 = ny - j_store
//!   (for a hypothetical S->N scanned grid: `j_wgrib2 = j_store + 1`).
//! * Longitudes: inputs may be given as -180..180 or 0..360 (or anything
//!   else); they are normalized with [`norm_lon`] to [0, 360). `359.9` and
//!   `-0.1` normalize identically and land adjacent across the dateline
//!   seam.
//! * Domain rule (spec §6.1 "never extrapolate horizontally"): the accepted
//!   domain is the closed rectangle spanned by the *node* coordinates,
//!   `[0, nx-1] x [0, ny-1]` in fractional storage units plus a 1e-9 node
//!   tolerance. The half-cell beyond the outermost node row/column at a
//!   non-wrapping edge is OUT of domain (stricter than wgrib2's nearest-
//!   neighbour half-cell rule; deliberate decision, see README). Longitude
//!   wraps only for global grids (`nx * dlon == 360`, e.g. GFS 1440 x 0.25):
//!   there every longitude is inside and column `nx-1` is adjacent to
//!   column `0` (see [`GridDef::lon_wrap`]).

// ---------------------------------------------------------------------------
// Earth shape (GRIB2 Code Table 3.2, spec §4.3)
// ---------------------------------------------------------------------------

/// Semantics of the Code Table 3.2 byte: which earth model the projection
/// uses. Values follow the wgrib2 reference implementation
/// (`tools/wgrib2-src/grib2/wgrib2/Earth.c::axes_earth`) so that our numbers
/// match `wgrib2 -ll2ij` bit-for-bit on the fixtures. Note the task text
/// quotes the GRIB2-spec sphere radius (6371.2266 km) for code 0 while wgrib2
/// uses 6367.47 km for 0 and 6371.229 km for code 6; we follow wgrib2 for
/// every code it knows because the fixtures (and real NMC files: HRRR/GFS use
/// code 6) are validated against it. Unknown codes fall back to the NMC-
/// convention spheroid (6378.137 / 6356.7523 km). Codes 1/3/7 carry INLINE
/// radii inside section 3; when the reader finds them it overrides this table
/// through `GridLambert::set_user_axes`, so a user-defined spheroid is
/// projected with the axes the file actually declares rather than a
/// nearest-constant guess (V1.1 phase 4: the earth model must be identical
/// for the projection AND the wind-rotation angle, which both derive from
/// ns/rh built from these axes).
pub fn earth_axes_m(shape: u8) -> (f64, f64) {
    match shape {
        0 => (6371226.6, 6371226.6), // spec sphere; see note above
        1 => (6371226.6, 6371226.6), // user-defined sphere: no inline radius stored
        2 => (6378160.0, 6356775.0),
        3 => (6378137.0, 6356752.3), // user spheroid: fallback constants
        4 => (6378137.0, 6356752.140),
        5 => (6378137.0, 6356752.245),
        6 => (6371229.0, 6371229.0), // sphere 6371.229 km (NCEP default in fixtures)
        7 => (6378137.0, 6356752.3), // user spheroid: fallback constants
        8 => (6371200.0, 6371200.0),
        9 => (6377563.396, 6356256.909),
        _ => (6378137.0, 6356752.3), // NMC-convention default
    }
}

/// The two shapes the task requires us to support explicitly (sphere and
/// oblate spheroid). See [`earth_axes_m`] for the full CT 3.2 table.
pub const SPHERE_R_M: f64 = 6371226.6;
pub const SPHEROID_MAJOR_M: f64 = 6378137.0;
pub const SPHEROID_MINOR_M: f64 = 6356752.3;

// ---------------------------------------------------------------------------
// Grid definitions
// ---------------------------------------------------------------------------

/// Regular lat/lon grid (GRIB template 3.0). Angles in degrees.
#[derive(Clone, Debug)]
pub struct GridRegular {
    pub nx: u32,
    pub ny: u32,
    pub lon1: f64, // longitude of stored point (0,0), normalized [0,360)
    pub lat1: f64, // latitude of stored point (0,0)
    pub dlon: f64, // cell size, degrees (magnitude; direction in scan_mode)
    pub dlat: f64, // cell size, degrees (magnitude)
    pub scan_mode: u8, // CT 3.5 bits: 0x80 = i decreases eastward, 0x40 = rows go north
    pub winds_grid: bool, // flag table 3.3 bit 0x08 (grid-relative component flag)
}

/// Lambert Conformal Conic grid (GRIB template 3.30). Raw GRIB fields in
/// degrees/meters plus precomputed projection constants (filled by
/// [`GridLambert::new`] / [`GridLambert::prepare`]).
#[derive(Clone, Debug)]
pub struct GridLambert {
    // raw GRIB2 template 3.30 fields
    pub nx: u32,
    pub ny: u32,
    pub la1: f64,
    pub lo1: f64,
    pub lov: f64, // longitude of the pole (cone axis is at lov +/- 180 deg)
    pub lad: f64, // latitude of projection origin ("LaD")
    pub latin1: f64, // first standard parallel
    pub latin2: f64, // second standard parallel
    pub dx_m: f64, // meters
    pub dy_m: f64, // meters
    pub proj_flag: u8, // CT 3.20: 1 = normal
    pub scan_mode: u8, // template 3.30 scanning byte (see `j_south` below)
    pub earth_shape: u8, // Code Table 3.2 byte
    pub winds_grid: bool,

    // precomputed constants (spec §4.3: "precompute once per grid")
    pub r_major: f64, // semi-major axis, meters
    pub r_minor: f64, // semi-minor axis, meters
    pub es: f64, // eccentricity squared
    pub e: f64, // eccentricity
    pub ns: f64, // cone constant n
    pub f0: f64, // Snyder "f0"
    pub rh: f64, // slant height of cone at LaD (meters)
    pub lov_rad: f64, // lov in radians
    pub x0: f64, // projected easting of stored point (0,0) = (lo1, la1)
    pub y0: f64, // projected northing of stored point (0,0)
    pub inv_dx: f64,
    pub inv_dy: f64,
    pub j_south: bool, // true => stored rows advance toward the south pole (N->S)

    // Inline user-defined earth axes (Code Table 3.2 codes 1/3/7), meters.
    // `inline_axes` false => the axes come from [`earth_axes_m`] instead.
    pub ua_major_m: f64,
    pub ua_minor_m: f64,
    pub inline_axes: bool,
}

/// A grid is either regular lat/lon or LCC; downstream code passes this
/// around.
#[derive(Clone, Debug)]
pub enum GridDef {
    Regular(GridRegular),
    Lambert(GridLambert),
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Normalize a longitude to [0, 360). Accepts -180..180, 0..360 and anything
/// else (e.g. 359.9 and -0.1 both map to 359.9 -> adjacent across seam).
pub fn norm_lon(lon: f64) -> f64 {
    let x = lon % 360.0;
    if x < 0.0 { x + 360.0 } else { x }
}

/// Node-rectangle tolerance in grid units.
pub const PROJ_NODE_EPS: f64 = 1e-9;

impl GridRegular {
    /// True when the grid covers the full 360 degrees of longitude (GFS):
    /// longitudes wrap and column nx-1 is adjacent to column 0.
    pub fn lon_wrap(self: &Self) -> bool {
        ((self.nx as f64) * self.dlon - 360.0).abs() <= 1e-9
    }

    /// Fractional (i, j) in storage order, or None outside the node
    /// rectangle (never extrapolates; see module docs).
    pub fn project(self: &Self, lat: f64, lon: f64) -> Option<(f64, f64)> {
        if lat < -90.0 || lat > 90.0 {
            return None;
        }
        let lon = norm_lon(lon);
        let wraps = self.lon_wrap();

        // signed shortest delta from lon1 in cells (positive = east of lon1)
        let delta = ((lon - self.lon1 + 180.0) % 360.0) - 180.0;
        let mut di = delta / self.dlon;
        if (self.scan_mode & 0x80) != 0 {
            di = -di; // i decreases eastward: columns run westward from lon1
        }
        if wraps {
            // global grid: fold into [0, nx). di in (nx-1, nx) is the seam
            // cell blending column nx-1 with column 0 (spec §9.5); horiz_nodes
            // wraps the neighbor index, so keep di unclamped there.
            if di < 0.0 {
                di += self.nx as f64;
            }
            if di >= (self.nx as f64) {
                di -= self.nx as f64;
            }
        } else if di < -PROJ_NODE_EPS || di > (self.nx as f64) - 1.0 + PROJ_NODE_EPS {
            return None;
        }

        // dj follows the stored row order: bit 0x40 set => rows go north
        // (lat increases), clear => rows go south (GFS/HRRR fixture N->S).
        let step = if (self.scan_mode & 0x40) != 0 { self.dlat } else { -self.dlat };
        let dj = (lat - self.lat1) / step;
        if dj < -PROJ_NODE_EPS || dj > (self.ny as f64) - 1.0 + PROJ_NODE_EPS {
            return None;
        }
        let di_out = if wraps { di } else { di.clamp(0.0, (self.nx as f64) - 1.0) };
        Some((
            di_out,
            dj.clamp(0.0, (self.ny as f64) - 1.0),
        ))
    }
}

impl GridLambert {
    /// Build from raw GRIB fields and precompute all projection constants
    /// once (spec §4.3). `scan_mode` bit 0x40 set means stored rows advance
    /// toward the north pole; clear (the GFS/HRRR-fixture convention,
    /// verified empirically against `wgrib2 -ll2ij`) means rows advance
    /// southward from La1, i.e. La1 is the northern edge.
    pub fn new(nx: u32, ny: u32, la1: f64, lo1: f64, lov: f64, lad: f64,
               latin1: f64, latin2: f64, dx_m: f64, dy_m: f64, proj_flag: u8,
               scan_mode: u8, earth_shape: u8, winds_grid: bool) -> Self {
        let mut g = Self {
            nx: nx, ny: ny, la1: la1, lo1: lo1, lov: lov, lad: lad,
            latin1: latin1, latin2: latin2, dx_m: dx_m, dy_m: dy_m,
            proj_flag: proj_flag, scan_mode: scan_mode, earth_shape: earth_shape,
            winds_grid: winds_grid,
            r_major: 0.0, r_minor: 0.0, es: 0.0, e: 0.0, ns: 0.0, f0: 0.0,
            rh: 0.0, lov_rad: 0.0, x0: 0.0, y0: 0.0, inv_dx: 0.0, inv_dy: 0.0,
            j_south: false,
            ua_major_m: 0.0, ua_minor_m: 0.0, inline_axes: false,
        };
        g.prepare();
        g
    }

    /// Attach the inline user-defined earth axes (meters) declared by Code
    /// Table 3.2 codes 1/3/7 in section 3 and recompute every projection
    /// constant. Called by the GRIB2 reader when those bytes are present;
    /// until then the CT table value in [`earth_axes_m`] is used.
    pub fn set_user_axes(self: &mut Self, major_m: f64, minor_m: f64) {
        if major_m <= 0.0 || minor_m <= 0.0 || minor_m > major_m {
            return; // nonsense axes: keep the table value
        }
        self.ua_major_m = major_m;
        self.ua_minor_m = minor_m;
        self.inline_axes = true;
        self.prepare();
    }

    pub fn prepare(self: &mut Self) {
        let (maj, min) = if self.inline_axes && self.ua_major_m > 0.0 {
            (self.ua_major_m, self.ua_minor_m)
        } else {
            earth_axes_m(self.earth_shape)
        };
        self.r_major = maj;
        self.r_minor = min;
        self.es = 1.0 - (self.r_minor / self.r_major).powf(2.0);
        self.e = self.es.sqrt();
        self.lov_rad = self.lov * D2R;

        let l1 = self.latin1 * D2R;
        let l2 = self.latin2 * D2R;
        let sin1 = l1.sin();
        let ms1 = msfnz(self.e, sin1, l1.cos());
        let ts1 = tsfnz(self.e, l1, sin1);
        let sin2 = l2.sin();
        let ms2 = msfnz(self.e, sin2, l2.cos());
        let ts2 = tsfnz(self.e, l2, sin2);
        if (self.latin1 - self.latin2).abs() > 1e-10 {
            self.ns = (ms1 / ms2).ln() / (ts1 / ts2).ln();
        } else {
            self.ns = sin1; // tangent cone (single standard parallel)
        }
        self.f0 = ms1 / (self.ns * ts1.powf(self.ns));
        let lad_rad = self.lad * D2R;
        let ts0 = tsfnz(self.e, lad_rad, lad_rad.sin());
        self.rh = self.r_major * self.f0 * ts0.powf(self.ns);

        // projected position of stored point (0,0) == (lo1, la1)
        let (x, y) = self.for_point(norm_lon(self.lo1) * D2R, self.la1 * D2R).unwrap();
        self.x0 = x;
        self.y0 = y;
        self.inv_dx = 1.0 / self.dx_m;
        self.inv_dy = 1.0 / self.dy_m;
        self.j_south = (self.scan_mode & 0x40) == 0;
    }

    /// Snyder/gctpc Lambert Conformal Conic forward transform (mirrors
    /// `lamccfor.c` from the gctpc copy vendored in wgrib2). Returns
    /// (easting, northing) meters, or None for unprojectable points (poles
    /// of the cone).
    pub fn for_point(self: &Self, lon_rad: f64, lat_rad: f64) -> Option<(f64, f64)> {
        if (lat_rad.abs() - HALF_PI).abs() <= 1e-10 {
            return None; // pole: defined as failure for our purposes
        }
        let sinphi = lat_rad.sin();
        let ts = tsfnz(self.e, lat_rad, sinphi);
        let rh1 = self.r_major * self.f0 * ts.powf(self.ns);
        let theta = self.ns * adjust_lon(lon_rad - self.lov_rad);
        Some((rh1 * theta.sin(), self.rh - rh1 * theta.cos()))
    }

    /// Forward transform lat/lon -> fractional storage (i, j), or None when
    /// outside the node rectangle (never extrapolates).
    pub fn project(self: &Self, lat: f64, lon: f64) -> Option<(f64, f64)> {
        if lat < -90.0 || lat > 90.0 {
            return None;
        }
        let (x, y) = match self.for_point(norm_lon(lon) * D2R, lat * D2R) {
            None => { return None; }
            Some((x, y)) => (x, y),
        };
        let di = (x - self.x0) * self.inv_dx;
        let dj_north = (y - self.y0) * self.inv_dy;
        let dj = if self.j_south { -dj_north } else { dj_north };
        if di < -PROJ_NODE_EPS || di > (self.nx as f64) - 1.0 + PROJ_NODE_EPS {
            return None;
        }
        if dj < -PROJ_NODE_EPS || dj > (self.ny as f64) - 1.0 + PROJ_NODE_EPS {
            return None;
        }
        Some((
            di.clamp(0.0, (self.nx as f64) - 1.0),
            dj.clamp(0.0, (self.ny as f64) - 1.0),
        ))
    }

    /// Projected plane coordinates (meters, same frame as `x0`/`y0`) of a
    /// fractional storage position. Shared by `inverse` and `rot_angle_deg`
    /// so the two can never disagree about where a grid node sits.
    fn plane_xy(self: &Self, di: f64, dj: f64) -> (f64, f64) {
        let x = self.x0 + di * self.dx_m;
        let dj_north = if self.j_south { -dj } else { dj };
        (x, self.y0 + dj_north * self.dy_m)
    }

    /// Cone-polar coordinates of a projected point: the SIGNED slant height
    /// rh1 and the polar angle theta (radians) about the cone apex, exactly as
    /// `lamccinv.c` derives them. theta is the SAME quantity `for_point`
    /// computes (`ns * (lon - LoV)` wrapped into (-pi, pi]): `for_point` places
    /// a point at apex + rh1*(sin theta, cos theta), so with the apex at plane
    /// (0, rh) the polar angle of (x, rh - y) recovers it. The sign handling
    /// (rh1 < 0 together with theta flipped when ns < 0) mirrors lamccinv, so
    /// southern-hemisphere grids resolve to the theta the forward transform
    /// actually used.
    fn cone_polar(self: &Self, x: f64, y: f64) -> (f64, f64) {
        let yy = self.rh - y;
        let pair: (f64, f64) = if self.ns > 0.0 {
            ((x * x + yy * yy).sqrt(), 1.0)
        } else {
            (-((x * x + yy * yy).sqrt()), -1.0)
        };
        let (rh1, con) = pair;
        let theta = if rh1 != 0.0 { (con * x).atan2(con * yy) } else { 0.0 };
        (rh1, theta)
    }

    /// Inverse transform: fractional storage (i, j) -> (lat, lon) degrees
    /// (lon normalized [0,360)). Mirrors `lamccinv.c`. Used for round-trip
    /// self-consistency tests and for the wind-rotation angle (spec §4.4).
    pub fn inverse(self: &Self, di: f64, dj: f64) -> (f64, f64) {
        let (x, y) = self.plane_xy(di, dj);
        let (rh1, theta) = self.cone_polar(x, y);
        let ts = (rh1 / (self.r_major * self.f0)).powf(1.0 / self.ns);
        let lat_rad = phi2z(self.e, ts);
        let lon_rad = adjust_lon(theta / self.ns + self.lov_rad);
        (lat_rad * R2D, norm_lon(lon_rad * R2D))
    }

    /// Grid-relative wind rotation angle (degrees) at fractional storage
    /// position (spec §4.4): bearing of the grid +i direction relative to
    /// TRUE north. wx-ingest uses this to rotate grid U/V to earth U/V.
    ///
    /// DERIVATION (shares one truth with the projection, V1.1 phase 4): in
    /// the Snyder PP-1395 conformal Lambert grid implemented by `for_point`,
    /// a point's offset from the cone apex is rh1*(sin theta, cos theta) with
    /// theta = ns*(lon - LoV), so MERIDIANS map to exact radial rays from the
    /// apex. The projection is CONFORMAL, hence true north at the point maps
    /// to that radial direction, while the grid +i axis maps to the plane
    /// +x unit vector. The clockwise angle from north to +x is therefore
    /// beta = 90 deg + theta, where theta comes from `cone_polar` -- the very
    /// same quantity `for_point`/`inverse` use. No finite differences and no
    /// local planar approximation: the earth model enters only through ns/rh
    /// (built from the CT 3.2 axes, including inline user-defined radii), so
    /// spherical and oblate grids are both handled exactly. Measured against
    /// pyproj with the earth shape matched exactly: <= 1.7e-13 deg (was up to
    /// 2.4e-2 deg with the previous two-inverse-difference method; see
    /// docs/DECISIONS.md and scripts/validate_rotation.py).
    pub fn rot_angle_deg(self: &Self, di: f64, dj: f64) -> f64 {
        let (x, y) = self.plane_xy(di, dj);
        let (_rh1, theta) = self.cone_polar(x, y);
        wrap_deg(90.0 + theta * R2D)
    }
}

/// Wrap a bearing-like angle into (-180, 180] degrees.
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

impl GridDef {
    pub fn project(self: &Self, lat: f64, lon: f64) -> Option<(f64, f64)> {
        match self {
            GridDef::Regular(g) => g.project(lat, lon),
            GridDef::Lambert(g) => g.project(lat, lon),
        }
    }

    pub fn nx(self: &Self) -> u32 {
        match self {
            GridDef::Regular(g) => g.nx,
            GridDef::Lambert(g) => g.nx,
        }
    }

    pub fn ny(self: &Self) -> u32 {
        match self {
            GridDef::Regular(g) => g.ny,
            GridDef::Lambert(g) => g.ny,
        }
    }

    /// True when column nx-1 wraps to column 0 (global regular grids only).
    pub fn lon_wrap(self: &Self) -> bool {
        match self {
            GridDef::Regular(g) => g.lon_wrap(),
            GridDef::Lambert(_) => false,
        }
    }

    /// Row 0 of stored data corresponds to La1/lat1; this accessor tells
    /// callers whether stored rows go southward (the GFS/HRRR convention).
    pub fn scan_ns(self: &Self) -> bool {
        match self {
            GridDef::Regular(g) => (g.scan_mode & 0x40) == 0,
            GridDef::Lambert(g) => g.j_south,
        }
    }
}

// ---------------------------------------------------------------------------
// projection math primitives (Snyder PP 1395, as vendored in wgrib2 gctpc)
// ---------------------------------------------------------------------------

const PI: f64 = 3.141592653589793;
const TAU: f64 = 6.283185307179586;
pub const D2R: f64 = 0.017453292519943296;
pub const R2D: f64 = 57.29577951308232;
const HALF_PI: f64 = 1.5707963267948967;

/// normalize an angle to (-pi, pi]
fn adjust_lon(x: f64) -> f64 {
    let mut x = x;
    while x.abs() > PI {
        x = if x > 0.0 { x - TAU } else { x + TAU };
    }
    x
}

/// "small m" of the spheroid: m(phi) = cos phi / sqrt(1 - (e sin phi)^2)
fn msfnz(e: f64, sinphi: f64, cosphi: f64) -> f64 {
    let con = e * sinphi;
    cosphi / (1.0 - con * con).sqrt()
}

/// "small t" of the spheroid:
/// t(phi) = tan(pi/4 - phi/2) * ((1 + e sin phi)/(1 - e sin phi))^(e/2)
fn tsfnz(e: f64, phi: f64, sinphi: f64) -> f64 {
    let con = e * sinphi;
    let com = 0.5 * e;
    (0.5 * (HALF_PI - phi)).tan() / ((1.0 - con) / (1.0 + con)).powf(com)
}

/// inverse "small t": latitude from t (Newton iteration, mirrors phi2z.c)
fn phi2z(e: f64, ts: f64) -> f64 {
    let com = 0.5 * e;
    let mut phi = HALF_PI - 2.0 * ts.atan();
    for _ in 0..=15 {
        let con = e * phi.sin();
        let dphi = HALF_PI - 2.0 * (ts * ((1.0 - con) / (1.0 + con)).powf(com)).atan() - phi;
        phi += dphi;
        if dphi.abs() <= 1e-10 {
            break;
        }
    }
    phi
}
