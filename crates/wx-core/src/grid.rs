//! Grid definitions and lat/lon -> fractional (i, j) projections.
//!
//! Conventions: i indexes longitude (0-based along scan direction), j indexes
//! rows (0-based in stored scan order). Fractional coordinates are returned in
//! stored scan order so the bilinear kernel never has to care about N->S.

/// Regular lat/lon grid (GRIB template 3.0). Angles in degrees, f64.
#[derive(Clone, Debug)]
pub struct GridRegular {
    pub nx: u32,
    pub ny: u32,
    pub lon1: f64, // first point longitude (scan start), normalized 0..360
    pub lat1: f64,
    pub dlon: f64, // magnitude; direction carried by scan_mode
    pub dlat: f64, // magnitude
    pub scan_mode: u8, // GRIB code table 3.5 bits: 0x80 lon-, 0x40 lat+
    pub winds_grid: bool, // flag table 3.3 bit 0x08
}

/// Lambert Conformal Conic grid (GRIB template 3.30). Raw angles degrees.
#[derive(Clone, Debug)]
pub struct GridLambert {
    pub nx: u32,
    pub ny: u32,
    pub la1: f64,
    pub lo1: f64,
    pub lov: f64,
    pub lad: f64,
    pub latin1: f64,
    pub latin2: f64,
    pub dx_m: f64, // meters
    pub dy_m: f64,
    pub proj_flag: u8, // code table 3.20: 1 = normal, 2 = polar
    pub scan_mode: u8,
    pub winds_grid: bool,
    pub rot_deg: f64, // precomputed rotation angle (filled by lambert_prepare)
}

/// Normalize a longitude to [0, 360). Accepts -180..180 and 0..360 inputs.
pub fn norm_lon(lon: f64) -> f64 {
    let x = lon % 360.0;
    if x < 0.0 {
        return x + 360.0;
    }
    return x;
}

/// Forward transform for the regular grid. Returns fractional (i, j) in
/// stored scan order, or None if the point is outside the grid domain
/// (including the outer half-cells; we never extrapolate horizontally).
/// Wraps across the dateline when the grid spans it (GFS 0..359.75).
pub fn regular_ij(g: &GridRegular, lat: f64, lon: f64) -> Option<(f64, f64)> {
    if lat < -90.0 || lat > 90.0 {
        return None;
    }
    let lon = norm_lon(lon);
    let nlon = 360.0 / g.dlon; // full-circle span in cells
    let max_i = (g.nx as f64) - 1.0;

    let wraps_lon = ((g.nx as f64) * g.dlon) >= 360.0 - 1e-9;
    let mut di = (lon - g.lon1) / g.dlon;
    if di < -0.5 {
        di += nlon; // wrap east of the seam
    } else if di >= nlon {
        di -= nlon; // wrap west of the seam
    }
    if di < -0.5 {
        return None;
    }
    if !wraps_lon && di > max_i + 0.5 {
        return None; // outside the far edge (never extrapolate)
    }

    let lat_step = if (g.scan_mode & 0x40) != 0 { g.dlat } else { -g.dlat };
    let dj = (lat - g.lat1) / lat_step;
    if dj < -0.5 || dj > (g.ny as f64) - 1.0 + 0.5 {
        return None;
    }
    return Some((di, dj));
}

/// Prepare precomputed LCC constants. Milestone 2 work: implement the forward
/// transform once validated against wgrib2 `-ll2ij` (see tests/fixtures/expected.json).
pub fn lambert_prepare(_g: &GridLambert) {}

impl GridRegular {
    pub fn describe(self: &GridRegular) -> String {
        return format!(
            "regular {}x{} lon1={} lat1={} dlon={} dlat={} scan={} wg={}",
            self.nx, self.ny, self.lon1, self.lat1, self.dlon, self.dlat,
            self.scan_mode, self.winds_grid,
        );
    }
}

impl GridLambert {
    pub fn describe(self: &GridLambert) -> String {
        return format!(
            "lambert {}x{} la1={} lo1={} lov={} lad={} latin1={} latin2={} dx={} dy={} proj={} scan={} wg={} rot={}",
            self.nx, self.ny, self.la1, self.lo1, self.lov, self.lad,
            self.latin1, self.latin2, self.dx_m, self.dy_m, self.proj_flag,
            self.scan_mode, self.winds_grid, self.rot_deg,
        );
    }
}

/// Forward transform for LCC grids. Not implemented yet (milestone 2).
pub fn lambert_ij(_g: &GridLambert, _lat: f64, _lon: f64) -> Option<(f64, f64)> {
    panic!("lambert_ij not implemented yet (milestone 2): validate against tests/fixtures/expected.json -ll2ij records");
}
