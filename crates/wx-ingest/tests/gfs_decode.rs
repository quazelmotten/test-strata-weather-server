// GRIB2 decode tests for tests/fixtures/gfs_small.grib2 (+ c3 twin):
// message inventory order, analytic values, wgrib2 -lon point checks,
// bitmap -> NaN masking, skip-unpack counters (spec §9.1 / §4.2).
// Ground truth: tools/wgrib2 probes recorded in expected.json.

use wx_ingest::grib2::decode_file;
use wx_ingest::grib2::ParamFilter;
use wx_core::tables::lookup_param_index;
use wx_core::tables::PARAM_TABLE;

fn f_is_nan(x: f32) -> bool {
    x.is_nan()
}

const NX: usize = 41;
const NY: usize = 21;

fn fixture_path(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

// Analytic field formula from scripts/gen_grib2.py:
//   value = 250 + base_i + lv_i + fh*10 + j*4 + i, scan order k = j*NX+i.
fn expect_val(p: usize, level: f64, fh_hours: i64, j: usize, i: usize) -> f32 {
    let base_i = if p == 0 { 0 } else if p == 1 { 1000 } else { 2000 };
    let lv_i = if level == 1000.0 { 0 }
        else if level == 850.0 { 100 }
        else if level == 700.0 { 200 }
        else if level == 2.0 { 300 }
        else { 400 };
    (250.0 + (base_i + lv_i + fh_hours * 10 + (j * 4) as i64 + (i as i64)) as f64) as f32
}

#[test]
fn gfs_inventory_order() {
    let r = decode_file(fixture_path("gfs_small.grib2"), ParamFilter::All);
    match r {
        Err(_) => { panic!(); }
        Ok((msgs, st)) => {
            assert!(msgs.len() == 39);
            assert!(st.messages_total == 39);
            assert!(st.messages_decoded == 39);
            assert!(st.messages_skipped == 0);
            // order: TMP x5 levels x3 fh, UGRD x4 x3, VGRD x4 x3
            let want_levels_tmp: [f64; 5] = [1000.0, 850.0, 700.0, 2.0, 10.0];
            let want_levels_wind: [f64; 4] = [1000.0, 850.0, 700.0, 10.0];
            let mut k = 0;
            for p in 0 .. 3 {
                for l in 0 .. (if p == 0 { 5 } else { 4 }) {
                    for f in 0 .. 3 {
                        let m = &msgs[k];
                        match m.param_name {
                            None => { panic!(); }
                            Some(s) => {
                                if p == 0 {
                                    assert!(s == "TMP");
                                } else if p == 1 {
                                    assert!(s == "UGRD");
                                } else {
                                    assert!(s == "VGRD");
                                }
                            }
                        }
                        assert!(m.levels.len() == 1);
                        if p == 0 {
                            assert!(m.levels[0].1 == want_levels_tmp[l]);
                        } else {
                            assert!(m.levels[0].1 == want_levels_wind[l]);
                        }
                        assert!(m.fc_secs == (f as i64) * 21600);
                        assert!(m.valid_epoch_secs == m.run.epoch_secs + m.fc_secs);
                        k += 1;
                    }
                }
            }
            assert!(k == 39);
        }
    }
}

#[test]
fn gfs_analytic_values() {
    let r = decode_file(fixture_path("gfs_small.grib2"), ParamFilter::All);
    match r {
        Err(_) => { panic!(); }
        Ok((msgs, _st)) => {
            let mut idx = 0;
            for p in 0 .. 3 {
                let nlv = if p == 0 { 5 } else { 4 };
                for _l in 0 .. nlv {
                    for f in 0 .. 3 {
                        let m = &msgs[idx];
                        assert!(m.data.len() == NX * NY);
                        let level = m.levels[0].1;
                        let fh = (f as i64) * 6;
                        for j in 0 .. NY {
                            for i in 0 .. NX {
                                let k = j * NX + i;
                                let masked = k.is_multiple_of(97) && p == 0 && level == 850.0 && fh == 6;
                                if masked {
                                    assert!(f_is_nan(m.data[k]));
                                } else {
                                    assert!(m.data[k] == expect_val(p, level, fh, j, i));
                                }
                            }
                        }
                        idx += 1;
                    }
                }
            }
            assert!(idx == 39);
        }
    }
}

#[test]
fn gfs_wgrib2_point_checks() {
    // (lon, lat, record_index(1-based), expected value or NaN when masked)
    // straight from expected.json point_checks for gfs_small.grib2.
    let r = decode_file(fixture_path("gfs_small.grib2"), ParamFilter::All);
    match r {
        Err(_) => { panic!(); }
        Ok((msgs, _st)) => {
            // lon/lat -> grid node: i = ((lon-350+360)%360)/0.5, j=(10-lat)/0.5
            let checks: [[f64; 4]; 12] = [
                [350.0, 10.0, 1.0, 250.0],
                [350.0, 10.0, 2.0, 310.0],
                [10.0, 10.0, 1.0, 290.0],
                [10.0, 10.0, 5.0, 450.0],
                [350.0, 0.0, 1.0, 330.0],
                [10.0, 0.0, 9.0, 690.0],
                [0.0, 5.0, 1.0, 310.0],
                [359.9, 5.0, 2.0, 370.0],
                [-0.1, 5.0, 10.0, 610.0],
                [0.0, 0.0, 9.0, 670.0],
                [350.0, 10.0, 10.0, 550.0],
                [10.0, 0.0, 10.0, 670.0],
            ];
            for c in &checks {
                let lon = c[0];
                let lat = c[1];
                let rec = c[2] as usize - 1;
                let want = c[3];
                // wgrib2 -lon snaps to the nearest node; these all snap exactly
                let di = ((lon - 350.0 + 360.0) % 360.0) / 0.5;
                let dj = (10.0 - lat) / 0.5;
                let i = (di + 0.5) as usize;
                let j = (dj + 0.5) as usize;
                let v = msgs[rec].data[j * NX + i];
                assert!((v as f64) == want);
            }
            // the masked node: TMP@850 f006 at (350,10) -> wgrib2 val=9.999e20
            let mv = msgs[4].data[0];
            assert!(f_is_nan(mv));
            let mut nans = 0;
            for k in 0 .. (NX * NY) {
                if f_is_nan(msgs[4].data[k]) {
                    nans += 1;
                }
            }
            assert!(nans == 9);
        }
    }
}

#[test]
fn gfs_c3_twin_matches_simple() {
    // gfs_small_c3.grib2 is the SAME file re-encoded by wgrib2 with complex
    // packing + spatial differencing (DRT 5.3): decoded values must be
    // bit-identical to the DRT 5.0 version, including NaN positions.
    let a = decode_file(fixture_path("gfs_small.grib2"), ParamFilter::All);
    let b = decode_file(fixture_path("gfs_small_c3.grib2"), ParamFilter::All);
    match a {
        Err(_) => { panic!(); }
        Ok((ma, _sa)) => {
            match b {
                Err(_) => { panic!(); }
                Ok((mb, _sb)) => {
                    assert!(ma.len() == mb.len());
                    for x in 0 .. ma.len() {
                        let da = &ma[x].data;
                        let db = &mb[x].data;
                        assert!(da.len() == db.len());
                        for k in 0 .. da.len() {
                            let va = da[k];
                            let vb = db[k];
                            if f_is_nan(va) {
                                assert!(f_is_nan(vb));
                            } else {
                                assert!(va == vb);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn skip_unpack_counters() {
    // spec §4.2: messages outside the configured parameter list are skipped
    // WITHOUT section-7 decoding. Filter = TMP only.
    let idx = lookup_param_index("TMP");
    match idx {
        None => { panic!(); }
        Some(ti) => {
            let e = &PARAM_TABLE[ti];
            let one: Vec<(u8, u8, u8)> = vec![(e.discipline, e.category, e.number)];
            let r = decode_file(fixture_path("gfs_small.grib2"), ParamFilter::Only(one));
            match r {
                Err(_) => { panic!(); }
                Ok((_msgs, st)) => {
                    assert!(st.messages_total == 39);
                    assert!(st.messages_decoded == 15);
                    assert!(st.messages_skipped == 24);
                    assert!(st.sec7_bytes_skipped > 0);
                }
            }
        }
    }
}
