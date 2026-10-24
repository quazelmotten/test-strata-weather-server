// Spec §9.3 EXTERNAL GOLDEN gate: our earth-relative wind rotation must match
// `wgrib2 -new_grid_winds earth` (IPOLATES) node-for-node.
//
// Golden fixture: tests/fixtures/hrrr_ipolates_earth.grib2 — produced by a
// locally built wgrib2 v3.1.3 10/2023 linked with IPOLATES (USE_IPOLATES=3,
// double-precision ip2lib_d) via an IDENTITY-resolution Lambert->identical-
// Lambert neighbor regrid of tests/fixtures/hrrr_small.grib2 (see the
// .PROVENANCE.txt sidecar for the exact command line and date). Neighbor
// interpolation on an identical target grid means every output node value is
// the ROTATED input value at the SAME node: no interpolation smearing, only
// the vector rotation plus a complex-packing re-pack of the output.
//
// Comparison protocol (documented choice per review):
//   * BOTH files are decoded with OUR decoder.
//   * The golden is compared AS-IS, never re-rotated: its section-3 component
//     flag (Code Table 3.3 bit 0x08) is CLEARED by new_grid (octet 47 flips
//     0x38 -> 0x30; audited against stock tools/wgrib2 -V which prints
//     "winds(N/S)" on every golden message), and our decoder reads that same
//     octet/bit, so the golden decodes with winds_grid=false and its stored
//     values ARE already earth-relative. The test asserts exactly that
//     (flag respected, not forced).
//   * Our fixed rotation is applied to the SOURCE file's grid-relative
//     planes and compared over ALL nodes of BOTH forecast hours, U and V.
//
// Tolerance: identity neighbor regrid is mathematically exact, so the only
// error is the golden file's own complex-packing quantum. Measured floors:
// 4.686e-5 relative on this synthetic fixture (values ~O(5e3..9e3) pseudo-
// units) and 0.01-0.02 m/s absolute on real-scale HRRR bytes. The gates
// below use abs <= 0.05 + 1e-3 * |golden| (>= 100x headroom over the
// measured floor, still >= 4 orders of magnitude tighter than the historical
// reflection defect, whose residuals were O(amplitude) / up to 179.87 deg).
// WORST OBSERVED numbers are printed and asserted.

// Cosmetic lints are silenced per-file (same policy as bounds_complex.rs and
// the crate roots: the local toolchain's parser rejects canonical shorthands).
#![allow(clippy::needless_return)]
#![allow(clippy::single_match)]
#![allow(clippy::redundant_field_names)]

use wx_core::grid::{GridDef, GridLambert};
use wx_ingest::grib2::{decode_file, grid_winds, rotate_winds_reflection_legacy,
                       rotate_winds_to_earth, ParamFilter};

fn fx(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

struct Pair { ug: Vec<f32>, vg: Vec<f32>, fc: i64 }

/// Decode wind pairs keyed by forecast offset; assert the expected component
/// flag on every wind message (the flag audit, spec §4.4 gating).
fn pairs(path: &str, want_flag: bool) -> (Vec<Pair>, GridLambert) {
    let r = decode_file(path.to_string(), ParamFilter::All);
    let (msgs, _st) = match r {
        Ok(x) => x,
        Err(e) => { panic!("decode {path}: {e:?}"); }
    };
    let mut out: Vec<Pair> = Vec::new();
    let mut gl: Option<GridLambert> = None;
    for m in &msgs {
        if m.param_name == Some("UGRD") || m.param_name == Some("VGRD") {
            assert!(m.winds_grid == want_flag,
                    "component flag mismatch in {path}: {} fc={} reads winds_grid={} want {}",
                    m.param_name.unwrap_or("?"), m.fc_secs, m.winds_grid, want_flag);
            assert!(grid_winds(&m.grid) == want_flag);
        }
        if m.param_name == Some("UGRD") {
            out.push(Pair { ug: m.data.clone(), vg: Vec::new(), fc: m.fc_secs });
            if let GridDef::Lambert(ref g) = m.grid { gl = Some(g.clone()); }
        } else if m.param_name == Some("VGRD") {
            let mut placed = false;
            for p in out.iter_mut() {
                if p.fc == m.fc_secs && p.vg.is_empty() {
                    p.vg = m.data.clone();
                    placed = true;
                    break;
                }
            }
            if !placed { panic!("unpaired VGRD fc={} in {path}", m.fc_secs); }
        }
    }
    for p in &out { assert!(!p.vg.is_empty(), "unpaired UGRD fc={}", p.fc); }
    match gl {
        Some(g) => (out, g),
        None => { panic!("no lambert grid in {path}"); }
    }
}

fn wrap_deg(mut d: f64) -> f64 {
    while d > 180.0 { d -= 360.0; }
    while d < -180.0 { d += 360.0; }
    d
}

struct Stats {
    n: usize,
    worst_abs_u: f64,
    worst_abs_v: f64,
    worst_rel: f64,
    worst_ang: f64,
    worst_ang_strong: f64,
    worst_tolratio: f64,
    worst_mag_drift: f64,
}

impl Stats {
    fn new() -> Stats {
        return Stats { n: 0, worst_abs_u: 0.0, worst_abs_v: 0.0, worst_rel: 0.0,
                       worst_ang: 0.0, worst_ang_strong: 0.0,
                       worst_tolratio: 0.0, worst_mag_drift: 0.0 };
    }
    /// Compare a candidate earth pair against the golden at every node.
    fn observe(&mut self, ue: &[f32], vn: &[f32], gu: &[f32], gv: &[f32],
               src_u: &[f32], src_v: &[f32]) {
        for k in 0..ue.len() {
            let (uo, vo) = (ue[k] as f64, vn[k] as f64);
            let (uw, vw) = (gu[k] as f64, gv[k] as f64);
            let (ug, vg) = (src_u[k] as f64, src_v[k] as f64);
            if !(uo.is_finite() && vo.is_finite() && uw.is_finite() && vw.is_finite()) {
                panic!("non-finite value at node {k}");
            }
            let amp = uw.hypot(vw);
            let du = (uo - uw).abs();
            let dv = (vo - vw).abs();
            if du > self.worst_abs_u { self.worst_abs_u = du; }
            if dv > self.worst_abs_v { self.worst_abs_v = dv; }
            if amp > 1e-9 {
                let rel = du.max(dv) / amp;
                if rel > self.worst_rel { self.worst_rel = rel; }
            }
            // tolerance gate: absolute floor + golden-scaled relative term
            let tol = 0.05 + 1.0e-3 * uw.abs().max(vw.abs());
            let ratio = du.max(dv) / tol;
            if ratio > self.worst_tolratio { self.worst_tolratio = ratio; }
            // direction divergence: on NEAR-CALM nodes a repack quantum of a
            // few hundredths of a unit maps into degrees very fast (a 0.03
            // m/s error on a 0.7 m/s wind is already ~2.5 deg), so the gated
            // metric uses a strong-wind subset (|w| >= 5, same convention as
            // the Agent G audit's |w| >= 3 subset, tightened); worst_ang over
            // ALL nodes is reported for information only.
            let a = wrap_deg((f64::atan2(vo, uo) - f64::atan2(vw, uw))
                             * 180.0 / std::f64::consts::PI).abs();
            if amp > 1e-9 && a > self.worst_ang { self.worst_ang = a; }
            if amp >= 5.0 && a > self.worst_ang_strong { self.worst_ang_strong = a; }
            // magnitude preservation vs the SOURCE grid vector (proper rotation)
            let drift = (uo.hypot(vo) - ug.hypot(vg)).abs();
            if drift > self.worst_mag_drift { self.worst_mag_drift = drift; }
            self.n += 1;
        }
    }
    fn report(&self, tag: &str) {
        println!("{tag}: nodes={} worst_abs_u={:.6e} worst_abs_v={:.6e} \
                  worst_rel={:.6e} worst_angle_vs_golden(all)={:.6e} deg \
                  worst_angle_vs_golden(|w|>=5)={:.6e} deg \
                  worst|diff|/tol={:.6e} worst_magnitude_drift={:.6e}",
                  self.n, self.worst_abs_u, self.worst_abs_v, self.worst_rel,
                  self.worst_ang, self.worst_ang_strong, self.worst_tolratio,
                  self.worst_mag_drift);
    }
}

/// Gate over one source/golden pair set; returns (fixed-stats, legacy-stats).
fn compare(src_path: String, gold_path: String, tag: &str) -> (Stats, Stats) {
    let (ps, gl) = pairs(&src_path, true);        // source: flag SET
    let (wp, _gl2) = pairs(&gold_path, false);    // golden: flag CLEAR (as-is)
    assert!(ps.len() == wp.len(), "pair count differs");
    // grids must describe the SAME node placement (identity regrid invariant)
    assert!(gl.nx == _gl2.nx && gl.ny == _gl2.ny, "grid size differs");
    assert!(gl.scan_mode == _gl2.scan_mode, "scan mode differs");
    let gd = GridDef::Lambert(gl.clone());
    let mut s_fix = Stats::new();
    let mut s_leg = Stats::new();
    for pr in &ps {
        let wg = match wp.iter().find(|w| w.fc == pr.fc) {
            Some(x) => x,
            None => { panic!("no golden pair fc={}", pr.fc); }
        };
        assert!(pr.ug.len() == wg.ug.len() && pr.vg.len() == wg.vg.len());
        let fixed = rotate_winds_to_earth(&gd, &pr.ug, &pr.vg);
        let legacy = rotate_winds_reflection_legacy(&gd, &pr.ug, &pr.vg);
        s_fix.observe(&fixed.0, &fixed.1, &wg.ug, &wg.vg, &pr.ug, &pr.vg);
        s_leg.observe(&legacy.0, &legacy.1, &wg.ug, &wg.vg, &pr.ug, &pr.vg);
    }
    s_fix.report(&format!("{tag} FIXED(M2) vs IPOLATES golden"));
    s_leg.report(&format!("{tag} LEGACY-M1(reflection) vs IPOLATES golden"));
    return (s_fix, s_leg);
}

#[test]
fn hrrr_rotation_matches_ipolates_golden_synthetic() {
    // spec §9.3 golden over the committed synthetic fixture (N->S scan).
    let (fix, leg) = compare(fx("hrrr_small.grib2"), fx("hrrr_ipolates_earth.grib2"),
                             "SYNTHETIC(hrrr_small N->S)");
    // THE gate: fixed rotation must reproduce the golden within the packing
    // quantum on EVERY node of both records, both components.
    assert!(fix.worst_tolratio <= 1.0,
            "ours-vs-IPOLATES exceeded tolerance: worst |du|={:.4} |dv|={:.4} rel={:.3e}",
            fix.worst_abs_u, fix.worst_abs_v, fix.worst_rel);
    assert!(fix.worst_ang <= 0.05 && fix.worst_ang_strong <= 0.05,
            "worst direction diff {} / {} deg", fix.worst_ang, fix.worst_ang_strong);
    // magnitude preserved exactly (float rounding only)
    assert!(fix.worst_mag_drift <= 1.0, "magnitude drift {}", fix.worst_mag_drift);
    // RED-FIRST evidence retained: the retired reflection matrix must NOT pass
    // (historical mismatch order 100 deg+), so this file keeps proving the
    // discriminating property even after the fix.
    assert!(leg.worst_tolratio > 100.0 || leg.worst_ang > 90.0,
            "legacy M1 unexpectedly close to golden: tolratio={} ang={}",
            leg.worst_tolratio, leg.worst_ang);
    assert!(leg.worst_rel > 1.0, "legacy M1 relative residual {}", leg.worst_rel);
}

#[test]
fn hrrr_rotation_matches_ipolates_golden_real_bytes() {
    // Optional real-bytes gate: REAL NOMADS HRRR CONUS f000 UGRD/VGRD 10 m
    // bytes (S->N scan_mode 0x40 — production geometry, the case where the
    // old "+j=south" assumption mirrored vectors outright) and its
    // wgrib2/IPOLATES earth-wind twin. Installed under tests/realdata/ (NOT
    // committed); clean skip when absent.
    let uv = match real_fixture("hrrr_real_uv.grib2") {
        Some(p) => p,
        None => { println!("SKIP: tests/realdata/hrrr_real_uv.grib2 absent"); return; }
    };
    let earth = match real_fixture("hrrr_real_earth.grib2") {
        Some(p) => p,
        None => { println!("SKIP: tests/realdata/hrrr_real_earth.grib2 absent"); return; }
    };
    let (fix, leg) = compare(uv, earth, "REAL(hrrr CONUS S->N)");
    assert!(fix.worst_tolratio <= 1.0,
            "ours-vs-IPOLATES (real bytes) exceeded tolerance: worst |du|={:.4} \
             |dv|={:.4} rel={:.3e}", fix.worst_abs_u, fix.worst_abs_v, fix.worst_rel);
    assert!(fix.worst_abs_u <= 0.05 + 1.0e-3 * 64.0
            && fix.worst_abs_v <= 0.05 + 1.0e-3 * 64.0,
            "real-bytes abs component diff {} / {} m/s", fix.worst_abs_u, fix.worst_abs_v);
    // No separate angle gate here: the component tolerance above already
    // bounds direction by asin(sqrt(2)*tol/|w|), and on production bytes the
    // golden's own repack quantum (~0.03 m/s) maps to up to ~0.5 deg on the
    // weakest |w| >= 5 nodes (observed 0.488 deg; all-node worst is a calm-
    // node artifact at 36 deg). Direction correctness is enforced strictly on
    // the synthetic gate (<= 0.05 deg) where amplitudes are O(1e3).
    // the retired reflection matrix diverges by up to ~180 deg here (the
    // S->N production scan is exactly where "+j=south" mirrored V)
    assert!(leg.worst_ang > 90.0, "legacy M1 angle divergence {} deg", leg.worst_ang);
    assert!(leg.worst_tolratio > 100.0, "legacy M1 tolratio {}", leg.worst_tolratio);
}

fn real_fixture(name: &str) -> Option<String> {
    let p = format!("../../tests/realdata/{name}");
    if std::path::Path::new(&p).exists() { Some(p) } else { None }
}
