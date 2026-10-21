// Cross-codec equality primitive: `wgrib2 -checksum data` is a POSIX cksum over
// the decoded f32 array (masked points hold wgrib2's UNDEFINED sentinel). These
// numbers were captured from tools/wgrib2-full and are reproduced here by the
// decoder, which proves the cksum port AND pins the decoded bit patterns that
// every packing twin must match (see packing_twins.rs).

use wx_ingest::grib2::decode_file;
use wx_ingest::grib2::ParamFilter;
use wx_ingest::codecs::data_cksum_grid;

fn fixture_path(name: &str) -> String {
    return format!("../../tests/fixtures/{name}");
}

// tools/wgrib2-full tests/fixtures/gfs_small.grib2 -checksum data
const GFS_CKSUMS: [u32; 39] = [
    918255062, 580032261, 1422511193, 1776270737, 2042874658, 540411886,
    3690302056, 3235669801, 169351278, 425496861, 377971663, 3810218456,
    3489271960, 876348577, 3131424764, 1361716480, 1538070026, 563782657,
    393629418, 1700735356, 3213461422, 2114025872, 1738904938, 4114257944,
    1905336425, 1820400360, 3630653118, 3467152171, 3107551146, 2669578568,
    1081991513, 3300199701, 1905183886, 2428728649, 1272157974, 3454640117,
    3411845686, 1060419952, 1874398919,
];

// tools/wgrib2-full tests/fixtures/hrrr_small.grib2 -checksum data
const HRRR_CKSUMS: [u32; 6] = [
    603596272, 1929936423, 70820645, 1067639457, 2202214748, 3444488225,
];

// tools/wgrib2-full tests/fixtures/stat_small.grib2 -checksum data
const STAT_CKSUMS: [u32; 3] = [1392457546, 1392457546, 348850530];

#[test]
fn gfs_drt50_data_checksums_match_wgrib2() {
    let r = decode_file(fixture_path("gfs_small.grib2"), ParamFilter::All);
    match r {
        Err(_e) => { assert!(false); return; }
        Ok((msgs, _st)) => {
            assert!(msgs.len() == GFS_CKSUMS.len());
            for k in 0 .. msgs.len() {
                let gk = msgs[k].grid.clone();
                assert!(data_cksum_grid(msgs[k].data.clone(), &gk) == GFS_CKSUMS[k]);
            }
        }
    }
}

#[test]
fn hrrr_drt50_data_checksums_match_wgrib2() {
    let r = decode_file(fixture_path("hrrr_small.grib2"), ParamFilter::All);
    match r {
        Err(_e) => { assert!(false); return; }
        Ok((msgs, _st)) => {
            assert!(msgs.len() == HRRR_CKSUMS.len());
            for k in 0 .. msgs.len() {
                let gk = msgs[k].grid.clone();
                assert!(data_cksum_grid(msgs[k].data.clone(), &gk) == HRRR_CKSUMS[k]);
            }
        }
    }
}

#[test]
fn stat_pdt48_data_checksums_match_wgrib2() {
    let r = decode_file(fixture_path("stat_small.grib2"), ParamFilter::All);
    match r {
        Err(_e) => { assert!(false); return; }
        Ok((msgs, _st)) => {
            assert!(msgs.len() == 3);
            for k in 0 .. 3 {
                let gk = msgs[k].grid.clone();
                assert!(data_cksum_grid(msgs[k].data.clone(), &gk) == STAT_CKSUMS[k]);
            }
        }
    }
}
