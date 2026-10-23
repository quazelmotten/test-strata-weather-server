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

/// Gaussian-latitude lat/lon grid (GRIB2 grid definition template 3.40 as
/// NCEP uses it for the operational FV3GFS line: quasi-regular, non-thinned,
/// global; today 3072 x 1536). Equal-spaced longitudes, rows sit on Gaussian
/// latitudes (see [`gaussian_latitudes`]); storage layout is the plain
/// `[row][col]` rectangle like the regular grid, so the bilinear kernels are
/// shared. There are NO pole nodes: the topmost/bottommost rows are at
/// +/- asin(largest root of P_ny) (~ +/-89.91 deg for GFS), and lat +-90 is
/// therefore always OUT of domain — defined behavior (spec §6.1: never
/// extrapolate). Angles in degrees.
#[derive(Clone, Debug)]
pub struct GridGaussian {
    pub nx: u32,
    pub ny: u32, // total rows; must be even (symmetric about the equator)
    pub lon1: f64, // longitude of stored point (0,0), normalized [0,360)
    pub dlon: f64, // cell size, degrees (magnitude; direction in scan_mode)
    pub la1: f64, // northernmost row latitude from section 3 (informational)
    pub la2: f64, // southernmost row latitude from section 3 (informational)
    pub scan_mode: u8, // CT 3.5 bits: 0x80 = i decreases eastward, 0x40 = rows go north
    pub earth_shape: u8, // Code Table 3.2 byte (reported through /v1/models)
    pub winds_grid: bool, // flag table 3.3 bit 0x08 (NCEP gaussian: false)

    // precomputed once per grid (spec §4.3): row latitudes in STORAGE order,
    // strictly monotonic; plus the coarsest row spacing in degrees, which
    // anchors the node-exactness epsilon so it means the same fraction of a
    // cell as PROJ_NODE_EPS does on the regular grid.
    lats: Vec<f64>,
    max_row_deg: f64,
}

/// A grid is regular lat/lon, LCC, or gaussian-lat-lon; downstream code
/// passes this around.
#[derive(Clone, Debug)]
pub enum GridDef {
    Regular(GridRegular),
    Lambert(GridLambert),
    Gaussian(GridGaussian),
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
        // V1.4 phase R: rem_euclid, NOT %. Rust % keeps the dividend's sign,
        // so for lon < lon1 - 180 (query east of the antimeridian on a
        // NON-wrapping grid whose longitudes cross it) the old form returned
        // ~-350 deg instead of ~+10 deg and rejected real stored columns
        // (found by phase T; see seam_small_http.rs). Identical results
        // everywhere else, incl. all global lon1=0 grids.
        let delta = (lon - self.lon1 + 180.0).rem_euclid(360.0) - 180.0;
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

// ---------------------------------------------------------------------------
// Gaussian latitudes (GRIB template 3.40, V1.3 phase E4b)
// ---------------------------------------------------------------------------

/// Positive (north) Gaussian latitudes in degrees, DESCENDING, for a full
/// global gaussian grid with `ny` rows: asin of the positive roots of the
/// Legendre polynomial **P_ny** (ny even: exactly ny/2 positive roots, the
/// southern rows are their mirror). NOTE the degree: the classic "full
/// gaussian" grid places ALL ny rows on the ny roots of P_ny — NOT the ny/2
/// roots of P_{ny/2}. Verified three ways (docs/DECISIONS.md V1.3-E4b):
/// against `numpy.polynomial.legendre.leggauss`, against wgrib2's reference
/// generator (`tools/wgrib2-full-src/grib2/wgrib2/geo.c::gauss2lats`, Newton
/// on an exact cosine-series Legendre evaluator), and against LIVE GFS sflux
/// bytes: the file's La1 = 89.910324 deg matches asin(top root of P_1536) to
/// 5.4e-7 deg, and the recorded probe row 40.006506 IS a table entry (delta
/// -2.5e-7 deg — six-decimal print rounding).
///
/// Algorithm: for k = 1..=ny/2 start from the asymptotic Chebyshev-Gauss
/// seed x0 = cos(pi (k - 1/4) / (n + 1/2)) and polish with Newton iterations
/// on P_n evaluated by the three-term recurrence ((1-x^2) P'_n = n(P_{n-1} -
/// x P_n)), stopping at |dx| <= 1e-15. Safeguards: iterates that leave (0,1),
/// stall, or finish with a non-tiny residual |P_n(x)| fall back to bisection
/// on a sign change of P_n bracketed by walking down from the previously
/// found larger root. The builder additionally requires strict monotonicity
/// and returns None on any anomaly, so a decoder meeting a weird grid skips
/// it loudly instead of interpolating wrong rows.
pub fn gaussian_latitudes(ny: u32) -> Option<Vec<f64>> {
    let n = ny as usize;
    if !(2..=65536).contains(&n) || !n.is_multiple_of(2) {
        return None; // odd ny would need an equator row: not produced by NCEP
    }
    let npos = n / 2;
    let mut xs: Vec<f64> = vec![0.0; npos];
    for k in 0..npos {
        let j = (k + 1) as f64;
        let seed = ((PI * (j - 0.25)) / (n as f64 + 0.5)).cos();
        let prev = if k == 0 { 1.0 } else { xs[k - 1] };
        let x = match refine_legendre_root(n, seed, prev) {
            Some(v) => v,
            None => { return None; }
        };
        // named bool so the NOT applies to a plain boolean: NaN must FAIL
        // (both comparisons false -> && false -> !true -> None), which the
        // direct `x <= 0.0 || x >= prev` spelling would silently pass.
        let strict_between = x > 0.0 && x < prev;
        if !strict_between {
            return None;
        }
        xs[k] = x;
    }
    let lats: Vec<f64> = xs.iter().map(|xk| xk.asin() * R2D).collect();
    return Some(lats);
}

/// (P_n(x), P_{n-1}(x)) via the stable three-term recurrence.
fn legendre_pair(n: usize, x: f64) -> (f64, f64) {
    let mut p0 = 1.0f64; // P_0
    let mut p1 = x; // P_1
    let mut k = 2usize;
    while k <= n {
        let kf = k as f64;
        let pk = ((2.0 * kf - 1.0) * x * p1 - (kf - 1.0) * p0) / kf;
        p0 = p1;
        p1 = pk;
        k += 1;
    }
    return (p1, p0);
}

/// Polish one root of P_n started from its asymptotic seed. `hi_prev` is the
/// previously found (larger) root or 1.0 for the first one and anchors the
/// fallback bracket. Newton first (quadratic convergence from the seed); if
/// any iterate leaves (0, hi_prev) or the result keeps a visible residual,
/// bisect on the sign change of P_n inside (0, hi_prev) instead.
fn refine_legendre_root(n: usize, seed: f64, hi_prev: f64) -> Option<f64> {
    let mut x = seed;
    let mut converged = false;
    let mut it = 0usize;
    let nf = n as f64;
    while it < 60 && x > 0.0 && x < hi_prev {
        let (pn, pm1) = legendre_pair(n, x);
        let den = 1.0 - x * x;
        if den.abs() < 1e-300 {
            break;
        }
        // P'_n = n (P_{n-1} - x P_n) / (1 - x^2)
        let dpn = nf * (pm1 - x * pn) / den;
        if dpn.abs() < 1e-300 {
            break;
        }
        let dx = pn / dpn;
        let xn = x - dx;
        let adx = if dx < 0.0 { -dx } else { dx };
        x = xn;
        if adx <= 1e-15 {
            converged = true;
            break;
        }
        it += 1;
    }
    let in_bracket = x > 0.0 && x < hi_prev;
    if !converged || !in_bracket {
        // bisection fallback: walk the interval (0, hi_prev) downward from
        // hi_prev in seed-sized steps until P_n changes sign, then bisect.
        let step = if seed < hi_prev { hi_prev - seed } else { 0.05 };
        let mut a = hi_prev;
        let mut fa = legendre_pair(n, a).0;
        let mut found = false;
        let mut b = hi_prev - (step + step);
        let mut guard = 0usize;
        while b > 0.0 && guard < 4 * n + 64 {
            let fb = legendre_pair(n, b).0;
            if fa == 0.0 {
                x = a;
                found = true;
                break;
            }
            if (fa > 0.0) != (fb > 0.0) {
                // bisect [b, a], keeping the sign-change invariant
                let mut lo = b;
                let mut flo = fb;
                let mut hi = a;
                let mut j = 0usize;
                while j < 200 && hi - lo > 1e-15 {
                    let m = 0.5 * (lo + hi);
                    let fm = legendre_pair(n, m).0;
                    if (fm > 0.0) == (flo > 0.0) {
                        lo = m;
                        flo = fm;
                    } else {
                        hi = m;
                    }
                    j += 1;
                }
                x = 0.5 * (lo + hi);
                found = true;
                break;
            }
            a = b;
            fa = fb;
            b -= step;
            guard += 1;
        }
        if !found {
            return None;
        }
    }
    let (pn, _) = legendre_pair(n, x);
    let apn = if pn < 0.0 { -pn } else { pn };
    if apn > 1e-9 {
        return None;
    }
    return Some(x);
}

impl GridGaussian {
    /// Build the grid and precompute the row-latitude table ONCE (spec
    /// §4.3). Storage-order convention matches the other grids: row 0 is
    /// the La1 side; scan bit 0x40 clear => rows advance southward (the
    /// operational GFS layout), set => northward. The row latitudes come
    /// from [`gaussian_latitudes`] (mathematically authoritative); la1/la2
    /// are stored verbatim for reporting and validated by the DECODER
    /// against the table before construction (see wx-ingest grib2.rs).
    pub fn new(nx: u32, ny: u32, lon1: f64, la1: f64, la2: f64, dlon: f64,
               scan_mode: u8, earth_shape: u8, winds_grid: bool) -> Self {
        let posv = gaussian_latitudes(ny).unwrap_or_default();
        let mut lats: Vec<f64> = Vec::<f64>::with_capacity(ny as usize);
        if (posv.len() * 2) as u32 == ny {
            let mut k = 0usize;
            while k < posv.len() {
                lats.push(posv[k]); // north half: descending
                k += 1;
            }
            k = posv.len();
            while k > 0 {
                k -= 1;
                lats.push(-posv[k]); // south half: ascending negatives
            }
        }
        if (scan_mode & 0x40) != 0 {
            lats.reverse();
        }
        let mut max_row_deg = 0.0f64;
        let mut i = 1usize;
        while i < lats.len() {
            let d = lats[i - 1] - lats[i];
            if d > max_row_deg {
                max_row_deg = d;
            }
            i += 1;
        }
        return Self {
            nx: nx, ny: ny, lon1: lon1, la1: la1, la2: la2, dlon: dlon,
            scan_mode: scan_mode, earth_shape: earth_shape,
            winds_grid: winds_grid, lats: lats, max_row_deg: max_row_deg,
        };
    }

    /// Row latitudes in storage order (row 0 first), strictly monotonic.
    pub fn row_lats(self: &Self) -> &[f64] {
        return &self.lats;
    }

    /// True when column nx-1 wraps to column 0 (global gaussian grids).
    pub fn lon_wrap(self: &Self) -> bool {
        return ((self.nx as f64) * self.dlon - 360.0).abs() <= 1e-9;
    }

    /// Fractional (i, j) in storage order, or None outside the node
    /// rectangle. Longitude handling is IDENTICAL to the regular grid
    /// (wrap across the seam, never extrapolate at non-wrapping edges).
    /// Latitude: binary search the monotonic row table, then interpolate
    /// LINEARLY IN j BETWEEN NODES (spec §6.1 works in grid-index space;
    /// this is the same rule the regular/Lambert projections apply). A
    /// latitude within PROJ_NODE_EPS of the coarsest row spacing of a node
    /// returns that node's exact integer j. Beyond the topmost/bottommost
    /// row — INCLUDING both poles — the point is out of domain.
    pub fn project(self: &Self, lat: f64, lon: f64) -> Option<(f64, f64)> {
        if lat < -90.0 || lat > 90.0 {
            return None;
        }
        if self.lats.len() != self.ny as usize || self.dlon <= 0.0 {
            return None;
        }
        let lon = norm_lon(lon);
        let wraps = self.lon_wrap();

        // signed shortest delta from lon1 in cells (positive = east of lon1)
        // V1.4 phase R: rem_euclid, NOT %. Rust % keeps the dividend's sign,
        // so for lon < lon1 - 180 (query east of the antimeridian on a
        // NON-wrapping grid whose longitudes cross it) the old form returned
        // ~-350 deg instead of ~+10 deg and rejected real stored columns
        // (found by phase T; see seam_small_http.rs). Identical results
        // everywhere else, incl. all global lon1=0 grids.
        let delta = (lon - self.lon1 + 180.0).rem_euclid(360.0) - 180.0;
        let mut di = delta / self.dlon;
        if (self.scan_mode & 0x80) != 0 {
            di = -di; // i decreases eastward: columns run westward from lon1
        }
        if wraps {
            if di < 0.0 {
                di += self.nx as f64;
            }
            if di >= (self.nx as f64) {
                di -= self.nx as f64;
            }
        } else if di < -PROJ_NODE_EPS || di > (self.nx as f64) - 1.0 + PROJ_NODE_EPS {
            return None;
        }

        // Work in NORTH-INDEX space: nk = row counted from the northernmost
        // row, whose latitudes strictly decrease. The stored dj follows the
        // scan direction.
        let ns = (self.scan_mode & 0x40) == 0;
        let len = self.lats.len();
        let get = |nk: usize| -> f64 {
            if ns { self.lats[nk] } else { self.lats[len - 1 - nk] }
        };
        let epsd = PROJ_NODE_EPS * self.max_row_deg;
        let top = get(0);
        let bot = get(len - 1);
        if lat > top + epsd || lat < bot - epsd {
            return None; // no pole nodes exist on a gaussian grid
        }
        // binary search: largest k with get(k) >= lat (clamped at the ends)
        let mut lo = 0usize;
        let mut hi = len - 1;
        if lat >= top {
            hi = 0;
        } else {
            while hi - lo > 1 {
                let m = (lo + hi) / 2;
                if get(m) >= lat {
                    lo = m;
                } else {
                    hi = m;
                }
            }
        }
        let v0 = get(lo);
        let v1 = get(hi);
        let d0 = if lat > v0 { lat - v0 } else { v0 - lat };
        let nk_f: f64;
        if d0 <= epsd || hi == lo {
            nk_f = lo as f64; // exactly on a node (or degenerate clamp)
        } else {
            let d1 = if lat > v1 { lat - v1 } else { v1 - lat };
            if d1 <= epsd {
                nk_f = hi as f64;
            } else {
                let w = (v0 - lat) / (v0 - v1);
                nk_f = (lo as f64) + w;
            }
        }
        let dj = if ns { nk_f } else { (len - 1) as f64 - nk_f };
        let di_out = if wraps { di } else { di.clamp(0.0, (self.nx as f64) - 1.0) };
        return Some((
            di_out,
            dj.clamp(0.0, (self.ny as f64) - 1.0),
        ));
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
            GridDef::Gaussian(g) => g.project(lat, lon),
        }
    }

    pub fn nx(self: &Self) -> u32 {
        match self {
            GridDef::Regular(g) => g.nx,
            GridDef::Lambert(g) => g.nx,
            GridDef::Gaussian(g) => g.nx,
        }
    }

    pub fn ny(self: &Self) -> u32 {
        match self {
            GridDef::Regular(g) => g.ny,
            GridDef::Lambert(g) => g.ny,
            GridDef::Gaussian(g) => g.ny,
        }
    }

    /// True when column nx-1 wraps to column 0 (global regular grids only).
    pub fn lon_wrap(self: &Self) -> bool {
        match self {
            GridDef::Regular(g) => g.lon_wrap(),
            GridDef::Lambert(_) => false,
            GridDef::Gaussian(g) => g.lon_wrap(),
        }
    }

    /// Row 0 of stored data corresponds to La1/lat1; this accessor tells
    /// callers whether stored rows go southward (the GFS/HRRR convention).
    pub fn scan_ns(self: &Self) -> bool {
        match self {
            GridDef::Regular(g) => (g.scan_mode & 0x40) == 0,
            GridDef::Lambert(g) => g.j_south,
            GridDef::Gaussian(g) => (g.scan_mode & 0x40) == 0,
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
