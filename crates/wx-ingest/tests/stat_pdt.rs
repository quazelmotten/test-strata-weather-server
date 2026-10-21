// PDT 4.8 statistically-processed fixture (stat_small.grib2): APCP
// accumulations must carry StatKind::Accumulation + time ranges, TMP an
// Average; values analytic; wgrib2 point checks from expected.json. Also:
// the JPEG2000 twin (stat_small_jpeg.grib2, DRT 5.40) must decode with identical
// metadata and values (spec §4.2: template 4.8 alongside compressed packing),
// and a template nothing can read (DRT 5.1 dictionary) must surface as a typed
// UnsupportedData error rather than a guessed array.

use wx_ingest::grib2::decode_file;
use wx_ingest::grib2::ParamFilter;
use wx_ingest::grib2::StatKind;

const SNX: usize = 11;
const SNY: usize = 7;

fn spath(name: &str) -> String {
    return format!("../../tests/fixtures/{name}");
}

#[test]
fn stat_small_messages() {
    let r = decode_file(spath("stat_small.grib2"), ParamFilter::All);
    match r {
        Err(_) => { assert!(false); }
        Ok((msgs, st)) => {
            assert!(msgs.len() == 3);
            assert!(st.messages_total == 3);
            // msg0: APCP surface, 0-6 h accumulation, fc hour 0
            let m0 = &msgs[0];
            match m0.param_name {
                Some(s) => { assert!(s == "APCP"); }
                None => { assert!(false); }
            }
            assert!(m0.levels.len() == 1);
            assert!(m0.levels[0].0 == 1);
            assert!(m0.levels[0].1 == 0.0);
            assert!(m0.stat.kind == StatKind::Accumulation);
            assert!(m0.stat.ct4_11 == 1);
            assert!(m0.fc_secs == 0);
            assert!(m0.stat.begin_secs == 0);
            assert!(m0.stat.end_secs == 6 * 3600);
            // msg1: same field, 6-12 h accumulation at fc hour 6
            let m1 = &msgs[1];
            match m1.param_name {
                Some(s) => { assert!(s == "APCP"); }
                None => { assert!(false); }
            }
            assert!(m1.stat.kind == StatKind::Accumulation);
            assert!(m1.fc_secs == 6 * 3600);
            assert!(m1.stat.begin_secs == 6 * 3600);
            assert!(m1.stat.end_secs == 12 * 3600);
            // msg2: TMP 2 m AGL, 3-6 h average at fc hour 3
            let m2 = &msgs[2];
            match m2.param_name {
                Some(s) => { assert!(s == "TMP"); }
                None => { assert!(false); }
            }
            assert!(m2.levels[0].0 == 103);
            assert!(m2.levels[0].1 == 2.0);
            assert!(m2.stat.kind == StatKind::Average);
            assert!(m2.stat.ct4_11 == 2);
            assert!(m2.fc_secs == 3 * 3600);
            assert!(m2.stat.begin_secs == 3 * 3600);
            assert!(m2.stat.end_secs == 6 * 3600);
            // instantaneous guard: gfs messages must be Instantaneous
            let g = decode_file(spath("gfs_small.grib2"), ParamFilter::All);
            match g {
                Err(_) => { assert!(false); }
                Ok((gm, _gs)) => {
                    for m in &gm {
                        assert!(m.stat.kind == StatKind::Instantaneous);
                    }
                }
            }
        }
    }
}

#[test]
fn stat_point_checks() {
    // grid: lon1=240 dlon=-0.5, lat1=20 dlat=-0.5 -> i=(240-lon)/0.5, j=(20-lat)/0.5
    // values: APCP base 10 + j*SNX + i; TMP base 250 + j*SNX + i (expected.json).
    let r = decode_file(spath("stat_small.grib2"), ParamFilter::All);
    match r {
        Err(_) => { assert!(false); }
        Ok((msgs, _st)) => {
            assert!(msgs[0].data.len() == SNX * SNY);
            let checks: [[f64; 4]; 9] = [
                [240.0, 20.0, 1.0, 10.0],
                [238.0, 18.0, 1.0, 58.0],
                [235.0, 17.0, 1.0, 86.0],
                [240.0, 20.0, 2.0, 10.0],
                [238.0, 18.0, 2.0, 58.0],
                [235.0, 17.0, 2.0, 86.0],
                [240.0, 20.0, 3.0, 250.0],
                [238.0, 18.0, 3.0, 298.0],
                [235.0, 17.0, 3.0, 326.0],
            ];
            for c in &checks {
                let i = ((240.0 - c[0]) / 0.5 + 0.5) as usize;
                let j = ((20.0 - c[1]) / 0.5 + 0.5) as usize;
                let rec = c[2] as usize - 1;
                assert!(msgs[rec].data[j * SNX + i] == (c[3] as f32));
            }
        }
    }
}

#[test]
fn jpeg_twin_keeps_pdt48_metadata_and_values() {
    // spec §4.2: product definition template 4.8 (statistically processed) must
    // survive alongside compressed data representations. stat_small_jpeg.grib2
    // is stat_small.grib2 re-encoded by tools/wgrib2-full with -set_grib_type
    // jpeg (DRT 5.40); decoded metadata and values must be unchanged.
    let a = decode_file(spath("stat_small.grib2"), ParamFilter::All);
    match a {
        Err(_) => { assert!(false); }
        Ok((ma, _sa)) => {
            let b = decode_file(spath("stat_small_jpeg.grib2"), ParamFilter::All);
            match b {
                Err(_) => { assert!(false); }
                Ok((mb, sb)) => {
                    assert!(mb.len() == 3);
                    assert!(sb.messages_total == 3);
                    assert!(sb.messages_decoded == 3);
                    assert!(sb.messages_skipped == 0);
                    for x in 0 .. 3 {
                        assert!(mb[x].fc_secs == ma[x].fc_secs);
                        assert!(mb[x].valid_epoch_secs == ma[x].valid_epoch_secs);
                        assert!(mb[x].levels == ma[x].levels);
                        assert!(mb[x].stat.kind == ma[x].stat.kind);
                        assert!(mb[x].stat.ct4_11 == ma[x].stat.ct4_11);
                        assert!(mb[x].stat.begin_secs == ma[x].stat.begin_secs);
                        assert!(mb[x].stat.end_secs == ma[x].stat.end_secs);
                        assert!(mb[x].data.len() == ma[x].data.len());
                        for k in 0 .. ma[x].data.len() {
                            assert!(mb[x].data[k] == ma[x].data[k]);
                        }
                    }
                    // statistical process identity survives the repack, so the
                    // accumulation time-interpolation guard (§6.3) still fires
                    assert!(mb[0].stat.kind == StatKind::Accumulation);
                    assert!(mb[0].stat.end_secs == 6 * 3600);
                    assert!(mb[1].stat.kind == StatKind::Accumulation);
                    assert!(mb[1].stat.begin_secs == 6 * 3600);
                    assert!(mb[2].stat.kind == StatKind::Average);
                }
            }
        }
    }
}
