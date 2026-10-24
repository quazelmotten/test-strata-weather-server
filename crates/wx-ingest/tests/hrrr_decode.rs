// Decode tests for tests/fixtures/hrrr_small.grib2 (+ c3 twin): LCC grid,
// analytic values in scan order, wgrib2 -ll2ij nearest-node values, wind
// component flags (flag table 3.3 bit 0x08) and rotation invariants.

use wx_ingest::grib2::decode_file;
use wx_ingest::grib2::rotate_winds_to_earth;
use wx_ingest::grib2::grid_winds;
use wx_ingest::grib2::check_wind_pair;
use wx_ingest::grib2::ParamFilter;
use wx_core::grid::GridDef;

const HNX: usize = 61;
const HNY: usize = 41;

fn fixture_path(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

// ij_checks from expected.json (wgrib2 -ll2ij): lon, lat, i_wg, j_wg,
// nearest TMP f000 value. Storage convention (see wx-core lcc test):
//   i_store = i_wg - 1,  j_store = HNY - j_wg.
const CHECKS: [[f64; 5]; 8] = [
    [233.440000, 21.113890, 1.000000, 41.000000, 260.0],
    [235.064820, 20.189420, 59.473405, 8.741953, 638.0],
    [235.011450, 20.417740, 57.173318, 17.217010, 556.0],
    [234.929580, 20.303340, 54.502304, 12.784808, 594.0],
    [235.020990, 20.551940, 57.261651, 22.266474, 506.0],
    [233.961710, 20.340520, 20.376653, 12.683610, 559.0],
    [235.021810, 20.591830, 57.217017, 23.763524, 486.0],
    [234.955610, 20.664550, 54.761382, 26.376338, 464.0],
];

#[test]
fn hrrr_inventory_flags_values() {
    let r = decode_file(fixture_path("hrrr_small.grib2"), ParamFilter::All);
    match r {
        Err(_) => { panic!(); }
        Ok((msgs, st)) => {
            assert!(msgs.len() == 6);
            assert!(st.messages_total == 6);
            // order: TMP fh0, TMP fh3, UGRD fh0, UGRD fh3, VGRD fh0, VGRD fh3
            let offs: [i64; 6] = [0, 0, 5000, 5000, 9000, 9000];
            for x in 0 .. 6 {
                let m = &msgs[x];
                match m.param_name {
                    None => { panic!(); }
                    Some(s) => {
                        if x < 2 {
                            assert!(s == "TMP");
                        } else if x < 4 {
                            assert!(s == "UGRD");
                        } else {
                            assert!(s == "VGRD");
                        }
                    }
                }
                assert!(m.levels.len() == 1);
                let want_level = if x < 2 { 2.0 } else { 10.0 };
                assert!(m.levels[0].1 == want_level);
                let fh = if x % 2 == 0 { 0 } else { 10800 };
                assert!(m.fc_secs == fh as i64);
                assert!(m.data.len() == HNX * HNY);
                // grid-relative wind flag (flag table 3.3 bit 0x08)
                assert!(m.winds_grid == (x >= 2));
                assert!(grid_winds(&m.grid) == (x >= 2));
                // analytic values in scan order
                for j in 0 .. HNY {
                    for i in 0 .. HNX {
                        let k = j * HNX + i;
                        let want = (260.0 + (offs[x] + ((fh as i64) / 3600) * 100 + (j * 10) as i64 + (i as i64)) as f64) as f32;
                        assert!(m.data[k] == want);
                    }
                }
            }
        }
    }
}

#[test]
fn hrrr_nearest_node_matches_wgrib2() {
    // decoded scan-order array must agree with wgrib2 -lon at the node nearest
    // to each -ll2ij probe point (ties decode order to projection).
    let r = decode_file(fixture_path("hrrr_small.grib2"), ParamFilter::All);
    match r {
        Err(_) => { panic!(); }
        Ok((msgs, _st)) => {
            let g = &msgs[0].grid;
            for c in &CHECKS {
                let p = g.project(c[1], c[0]);
                match p {
                    None => { panic!(); }
                    Some((di, dj)) => {
                        assert!(abs_diff(di + 1.0, c[2]) <= 1e-4);
                        assert!(abs_diff((HNY as f64) - dj, c[3]) <= 1e-4);
                        let i = (di + 0.5) as usize;
                        let j = (dj + 0.5) as usize;
                        assert!(msgs[0].data[j * HNX + i] == (c[4] as f32));
                    }
                }
            }
        }
    }
}

fn abs_diff(a: f64, b: f64) -> f64 {
    let d = a - b;
    if d < 0.0 {
        return -d;
    }
    d
}

#[test]
fn hrrr_c3_twin_matches_simple() {
    let a = decode_file(fixture_path("hrrr_small.grib2"), ParamFilter::All);
    match a {
        Err(_) => { panic!(); }
        Ok((ma, _sa)) => {
            let b = decode_file(fixture_path("hrrr_small_c3.grib2"), ParamFilter::All);
            match b {
                Err(_) => { panic!(); }
                Ok((mb, _sb)) => {
                    assert!(ma.len() == mb.len());
                    for x in 0 .. ma.len() {
                        let da = &ma[x].data;
                        let db = &mb[x].data;
                        assert!(da.len() == db.len());
                        for k in 0 .. da.len() {
                            assert!(da[k] == db[k]);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn hrrr_rotation_invariants() {
    // spec §9.3: rotated HRRR U/V must satisfy the physical invariants of the
    // LCC rotation. V1.4 phase X rewrote the step from the reflection matrix
    // [[sinB,cosB],[cosB,-sinB]] to the PROPER rotation M2 =
    // [[sinB,-cosB],[cosB,sinB]] pinned by the wgrib2/IPOLATES golden
    // (tests/fixtures/hrrr_ipolates_earth.grib2, see
    // crates/wx-ingest/tests/rotation_vs_ipolates.rs). The old involution
    // ("rotate twice == identity") was a property OF THE REFLECTION and is
    // dead for proper rotations: what holds now is magnitude preservation,
    // composition with the INVERSE angle recovers the grid components, and
    // identity exactly at LoV.
    let r = decode_file(fixture_path("hrrr_small.grib2"), ParamFilter::All);
    match r {
        Err(_) => { panic!(); }
        Ok((msgs, _st)) => {
            let gd = &msgs[2].grid;
            // V1.4 phase X: pair the REAL components (UGRD fh0 == msgs[2],
            // VGRD fh0 == msgs[4]); the old code mixed msgs[2]/msgs[3] which
            // are BOTH UGRD messages — harmless for the magnitude/involution
            // properties it checked, but it made any value-level cross-pin
            // against the earth-wind golden impossible.
            let u = &msgs[2].data;
            let v = &msgs[4].data;
            let pair = rotate_winds_to_earth(gd, u, v);
            let ue = pair.0;
            let vn = pair.1;
            assert!(ue.len() == HNX * HNY);
            assert!(vn.len() == HNX * HNY);
            for k in 0 .. ue.len() {
                let ug = u[k] as f64;
                let vg = v[k] as f64;
                let uef = ue[k] as f64;
                let vnf = vn[k] as f64;
                let lhs = uef * uef + vnf * vnf;
                let rhs = ug * ug + vg * vg;
                assert!(abs_diff(lhs, rhs) <= 1e-4 * rhs);
            }
            // Independent validation (spec §9.3): for LCC grids the bearing of
            // +i must equal Snyder's convergence relation beta = 90 + n*(lon-LoV)
            // (n from latin1/latin2). The stored V axis is the projection-plane
            // +y axis on EVERY scan mode (V1.4 phase X): this fixture scans N->S,
            // which only reverses row ORDER and never reinterprets the vector.
            let d2r: f64 = 0.017453292519943296;
            let la1c: f64 = 26.5 * d2r;
            let la2c: f64 = 35.0 * d2r;
            let tg1v: f64 = (45.0 + 26.5 / 2.0) * d2r;
            let tg2v: f64 = (45.0 + 35.0 / 2.0) * d2r;
            let tg1 = tg1v.tan();
            let tg2 = tg2v.tan();
            let nr = (la1c.cos() / la2c.cos()).ln();
            let dr = (tg2 / tg1).ln();
            let n_c = nr / dr;
            match gd {
                GridDef::Lambert(gl) => {
                    let pts: [[f64; 2]; 4] = [
                        [233.440000, 21.113890],
                        [235.064820, 20.189420],
                        [235.011450, 20.417740],
                        [233.472980, 20.156480],
                    ];
                    for c in &pts {
                        let pr = gl.project(c[1], c[0]);
                        match pr {
                            None => { panic!(); }
                            Some((di, dj)) => {
                                let gamma = n_c * (c[0] - 229.53);
                                let beta_w = gl.rot_angle_deg(di, dj);
                                // V1.1 phase 4: the angle is now derived from
                                // the projection's own cone geometry, so it
                                // reproduces Snyder's convergence relation to
                                // floating-point noise (was a 5e-2 DEG slop under
                                // the old finite-difference method).
                                assert!(abs_diff(beta_w, 90.0 + gamma) <= 1e-9);
                                // full rotation against the ANALYTIC beta at the
                                // nearest node, using the M2 matrix the external
                                // golden pins (errors of any sign/formula kind are
                                // O(1e4), tolerance 5e-2 rel).
                                let br = (90.0 + gamma) * d2r;
                                let kk = ((dj + 0.5) as usize) * HNX + ((di + 0.5) as usize);
                                let ug = u[kk] as f64;
                                let vg = v[kk] as f64;
                                let want_ue = ug * br.sin() - vg * br.cos();
                                let want_vn = ug * br.cos() + vg * br.sin();
                                let au = if ug < 0.0 { -ug } else { ug };
                                let av = if vg < 0.0 { -vg } else { vg };
                                let sc = if au > av { au } else { av };
                                assert!(abs_diff(ue[kk] as f64, want_ue) <= 5e-2 * sc + 1.0);
                                assert!(abs_diff(vn[kk] as f64, want_vn) <= 5e-2 * sc + 1.0);
                                // cross-pin vs the IPOLATES golden: node k=(0,0)
                                // fc 0 decodes from the golden as U=5580.1226
                                // V=9071.116 (repack quantum ~1 unit).
                                if kk == 0 {
                                    assert!(abs_diff(ue[0] as f64, 5580.1226) <= 1.0);
                                    assert!(abs_diff(vn[0] as f64, 9071.116) <= 1.0);
                                }
                            }
                        }
                    }
                }
                GridDef::Regular(_rg) => { panic!(); }
                GridDef::Gaussian(_gg) => { panic!(); }
            }
            // inverse composition: applying the SAME angle with the opposite
            // handedness (M2 transpose) recovers the grid components exactly --
            // the algebraic anchor that replaces the reflection-era involution
            // pin, which proper rotations do NOT satisfy (double-applying M2
            // rotates by 2*(beta - 90 deg), not 0).
            for k in 0 .. ue.len() {
                let i = (k % HNX) as f64;
                let j = (k / HNX) as f64;
                let br = match gd {
                    GridDef::Lambert(gl2) => { gl2.rot_angle_deg(i, j) * d2r }
                    _ => { panic!(); }
                };
                let s = br.sin();
                let c = br.cos();
                // (u_g, v_g) = M2^T (u_e, v_n) = [[s, c], [-c, s]] (u_e, v_n)
                let gu = ue[k] as f64 * s + vn[k] as f64 * c;
                let gv = -ue[k] as f64 * c + vn[k] as f64 * s;
                assert!(abs_diff(gu, u[k] as f64) <= 1e-2);
                assert!(abs_diff(gv, v[k] as f64) <= 1e-2);
            }
        }
    }
}

#[test]
fn regular_grid_winds_pass_through() {
    // GFS-style regular lat/lon grids are treated as already earth-relative
    // (documented decision); rotation must be the identity.
    let r = decode_file(fixture_path("gfs_small.grib2"), ParamFilter::All);
    match r {
        Err(_) => { panic!(); }
        Ok((msgs, _st)) => {
            let gd = &msgs[15].grid;
            assert!(!grid_winds(gd));
            let pair = rotate_winds_to_earth(gd, &msgs[15].data, &msgs[16].data);
            for k in 0 .. (41 * 21) {
                assert!(pair.0[k] == msgs[15].data[k]);
                assert!(pair.1[k] == msgs[16].data[k]);
            }
        }
    }
}

#[test]
fn wind_pair_config_rule() {
    // spec §4.4 config rule: grid-relative model with only one wind component
    // must fail validation with a clear message.
    let ps1: Vec<String> = vec!["TMP".to_string(), "UGRD".to_string()];
    let bad = check_wind_pair(ps1.clone(), true);
    match bad {
        None => { panic!(); }
        Some(msg) => { assert!(!msg.is_empty()); }
    }
    let ps2: Vec<String> = vec!["TMP".to_string(), "UGRD".to_string(), "VGRD".to_string()];
    if let Some(_mg) = check_wind_pair(ps2.clone(), true) { panic!(); }
    if let Some(_mg) = check_wind_pair(ps1.clone(), false) { panic!(); }
}
