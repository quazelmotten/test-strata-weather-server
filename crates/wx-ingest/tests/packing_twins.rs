// Packing-twin coverage (spec §9.1 / §4.2 / §6.1): every section-7 representation
// we claim to support must decode to bit-identical values -- including NaN
// positions and message metadata -- for the SAME logical message set. Twins are
// produced by scripts/gen_fixtures.sh and pre-validated there against tools/wgrib2-full
// `-checksum data`; here they are validated against the simple-packed original,
// whose own bit patterns are pinned to wgrib2 by checksum_parity.rs.
//
//   _c2    DRT 5.2  complex packing WITHOUT spatial differencing (written by
//          wgrib2 with `-set_grib_type complex1`; measured on this build, `complex1`
//          -> 5.2 while `complex2`/`complex3` -> 5.3, so the twin is named after the
//          DATA REPRESENTATION TEMPLATE, not the CLI token)
//   _c3    DRT 5.3  complex packing + spatial differencing (written by wgrib2)
//   _jpeg  DRT 5.40 JPEG2000           (hayro-jpeg2000 0.4.1)
//   _png   DRT 5.41 PNG, filter None   (scripts/make_png_twin.py)
//   _png_f DRT 5.41 PNG, all five PNG filters cycled across messages
//   _aec   DRT 5.42 CCSDS AEC          (rust-aec 0.1.1)
//   _ieee  DRT 5.4  raw IEEE float32
//
// Also covered here: masked points stay NaN for every codec, wgrib2 point probes
// reproduce on every twin (§9.1), skip-unpack counters still hold when the
// parameter filter rejects a compressed message, PNG predictor coverage, and
// typed rejection of a template nothing in the ecosystem can read.

use wx_ingest::grib2::decode_file;
use wx_ingest::grib2::ParamFilter;
use wx_ingest::grib2::GribErr;
use wx_ingest::grib2::be_u16;
use wx_ingest::grib2::be_u32;
use wx_ingest::grib2::DecodedMessage;
use wx_ingest::codecs::data_cksum_grid;
use wx_ingest::codecs::png_filter_types;
use wx_core::tables::lookup_param_index;
use wx_core::tables::PARAM_TABLE;
use std::fs::File;
use memmap2::Mmap;

const NX: usize = 41;
const HNX: usize = 61;
const HNY: usize = 41;

fn fixture_path(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

fn f_is_nan(x: f32) -> bool {
    x.is_nan()
}

/// Twin suffixes exercised on both model fixtures.
fn tags() -> Vec<String> {
    vec!["c2".to_string(), "c3".to_string(), "jpeg".to_string(),
         "png".to_string(), "png_f".to_string(), "aec".to_string(),
         "ieee".to_string()]
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn msgs_or_fail(name: &str) -> Vec<DecodedMessage> {
    let r = decode_file(fixture_path(name), ParamFilter::All);
    match r {
        Err(e) => {
            println!("decode failed for {0}: {1:?}", name, e);
            panic!();
        }
        Ok((msgs, _st)) => {
            msgs
        }
    }
}

/// Element-wise equality of a twin against the simple-packed original: same
/// record count, same metadata, identical f32 bits, NaN where NaN. Plus the
/// grid-normalized data checksum, which pins the scan-order normalization path
/// for that codec too.
fn compare_vs_base(base: &[DecodedMessage], twin: &[DecodedMessage]) {
    assert!(base.len() == twin.len());
    for x in 0 .. base.len() {
        let ma = &base[x];
        let mb = &twin[x];
        assert!(mb.discipline == ma.discipline);
        assert!(mb.category == ma.category);
        assert!(mb.number == ma.number);
        assert!(mb.fc_secs == ma.fc_secs);
        assert!(mb.valid_epoch_secs == ma.valid_epoch_secs);
        assert!(mb.levels.len() == ma.levels.len());
        let mut lv = 0;
        while lv < ma.levels.len() {
            assert!(mb.levels[lv] == ma.levels[lv]);
            lv += 1;
        }
        let da = &ma.data;
        let db = &mb.data;
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
        let ga = ma.grid.clone();
        let gb = mb.grid.clone();
        assert!(data_cksum_grid(ma.data.clone(), &ga) ==
                data_cksum_grid(mb.data.clone(), &gb));
    }
}

/// Slurp a fixture into memory (mmap + copy, the pattern used across this suite).
fn read_all(path: &str) -> Vec<u8> {
    let f = File::open(path);
    match f {
        Err(_e) => { Vec::<u8>::new()}
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_e) => { Vec::<u8>::new()}
                Ok(region) => {
                    let mut copy: Vec<u8> = Vec::<u8>::new();
                    for k in 0 .. region.len() {
                        copy.push(region[k]);
                    }
                    copy
                }
            }
        }
    }
}

/// Walk section framing and return one (section-5 payload, section-7 payload)
/// pair per message. Used to inspect packed streams without going through the
/// value pipeline.
fn sec_pairs(bytes: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::<(Vec<u8>, Vec<u8>)>::new();
    let mut p = 0;
    while p + 16 <= bytes.len() {
        let hi = be_u32(bytes, p + 8) as u64;
        let lo = be_u32(bytes, p + 12) as u64;
        let msglen = ((hi << 32) | lo) as usize;
        if msglen < 20 || p + msglen > bytes.len() {
            break;
        }
        let mut s5: Vec<u8> = Vec::<u8>::new();
        let mut s7: Vec<u8> = Vec::<u8>::new();
        let mut q = p + 16;
        while q + 5 <= p + msglen {
            if bytes[q] == 0x37 && bytes[q + 1] == 0x37 &&
               bytes[q + 2] == 0x37 && bytes[q + 3] == 0x37 {
                break;
            }
            let ln = be_u32(bytes, q) as usize;
            if ln < 5 || q + ln > p + msglen {
                break;
            }
            if bytes[q + 4] == 5 {
                s5 = bytes[q + 5 .. q + ln].to_vec();
            }
            if bytes[q + 4] == 7 {
                s7 = bytes[q + 5 .. q + ln].to_vec();
            }
            q += ln;
        }
        if s5.len() >= 6 && !s7.is_empty() {
            out.push((s5, s7));
        }
        p += msglen;
    }
    out
}

// ---------------------------------------------------------------------------
// §9.1: identical decoded arrays on every packing twin
// ---------------------------------------------------------------------------

#[test]
fn gfs_twins_match_simple_packing() {
    let base = msgs_or_fail("gfs_small.grib2");
    assert!(base.len() == 39);
    for t in &tags() {
        let twin = msgs_or_fail(&format!("gfs_small_{0}.grib2", t));
        compare_vs_base(&base[..], &twin[..]);
    }
}

#[test]
fn hrrr_twins_match_simple_packing() {
    let base = msgs_or_fail("hrrr_small.grib2");
    assert!(base.len() == 6);
    for t in &tags() {
        let twin = msgs_or_fail(&format!("hrrr_small_{0}.grib2", t));
        compare_vs_base(&base[..], &twin[..]);
    }
}

#[test]
fn masked_points_are_nan_for_every_codec() {
    // TMP@850 fc hour 6 (record index 4) carries a bitmap with 9 missing points
    // in gfs_small; every codec must produce NaN at exactly those indices.
    let base = msgs_or_fail("gfs_small.grib2");
    let want_idx: Vec<usize> = missing_idx(&base[4].data);
    assert!(want_idx.len() == 9);
    for t in &tags() {
        let twin = msgs_or_fail(&format!("gfs_small_{0}.grib2", t));
        let got_idx = missing_idx(&twin[4].data);
        assert!(got_idx.len() == want_idx.len());
        let mut k = 0;
        while k < want_idx.len() {
            assert!(got_idx[k] == want_idx[k]);
            k += 1;
        }
        // the masked node itself (lon 350, lat 10) must read NaN, never a blend
        assert!(f_is_nan(twin[4].data[0]));
    }
}

fn missing_idx(d: &[f32]) -> Vec<usize> {
    let mut v: Vec<usize> = Vec::<usize>::new();
    for (k, dv) in d.iter().enumerate() {
        if f_is_nan(*dv) {
            v.push(k);
        }
    }
    v
}

#[test]
fn point_probes_reproduce_on_every_gfs_twin() {
    // expected.json point_checks for gfs_small.grib2 (wgrib2 -lon nearest node).
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
    for t in &tags() {
        let msgs = msgs_or_fail(&format!("gfs_small_{0}.grib2", t));
        for c in &checks {
            let lon = c[0];
            let lat = c[1];
            let rec = c[2] as usize - 1;
            let want = c[3];
            let di = ((lon - 350.0 + 360.0) % 360.0) / 0.5;
            let dj = (10.0 - lat) / 0.5;
            let i = (di + 0.5) as usize;
            let j = (dj + 0.5) as usize;
            let v = msgs[rec].data[j * NX + i];
            assert!((v as f64) == want);
        }
    }
}

#[test]
fn point_probes_reproduce_on_every_hrrr_twin() {
    // expected.json ij_checks (wgrib2 -ll2ij nearest node), storage convention
    // i_store = i_wg - 1, j_store = HNY - j_wg.
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
    for t in &tags() {
        let msgs = msgs_or_fail(&format!("hrrr_small_{0}.grib2", t));
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

fn abs_diff(a: f64, b: f64) -> f64 {
    let d = a - b;
    if d < 0.0 {
        return -d;
    }
    d
}

// ---------------------------------------------------------------------------
// §4.2: filtered messages are skipped without touching section 7, whatever
// the representation is (JPEG streams are the expensive case).
// ---------------------------------------------------------------------------

#[test]
fn skip_unpack_counters_hold_for_compressed_twins() {
    let idx = lookup_param_index("TMP");
    match idx {
        None => { panic!(); }
        Some(ti) => {
            let e = &PARAM_TABLE[ti];
            let mut one: Vec<(u8, u8, u8)> = Vec::<(u8, u8, u8)>::new();
            one.push((e.discipline, e.category, e.number));
            for t in &tags() {
                let r = decode_file(fixture_path(&format!("gfs_small_{0}.grib2", t)),
                                     ParamFilter::Only(one.clone()));
                match r {
                    Err(_er) => {
                        println!("filtered decode failed for twin {0}", t);
                        panic!();
                    }
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
}

// ---------------------------------------------------------------------------
// PNG predictor coverage: the cycle twin must actually use all five filters,
// the plain twin none of them (so both the fast path and every unfilter branch
// are proven to run against wgrib2-decodable input).
// ---------------------------------------------------------------------------

#[test]
fn png_twin_filter_coverage() {
    let plain = read_all(&fixture_path("gfs_small_png.grib2"));
    assert!(!plain.is_empty());
    let pairs = sec_pairs(&plain[..]);
    assert!(pairs.len() == 39);
    for pr in &pairs {
        assert!(be_u16(&pr.0[..], 4) == 41);
        let fr = png_filter_types(&pr.1[..]);
        match fr {
            Err(_e) => { panic!(); }
            Ok(fs) => {
                assert!(fs.len() >= 2);
                for f in &fs {
                    assert!(*f == 0);
                }
            }
        }
    }

    let cyc = read_all(&fixture_path("gfs_small_png_f.grib2"));
    assert!(!cyc.is_empty());
    let pairs2 = sec_pairs(&cyc[..]);
    assert!(pairs2.len() == 39);
    let mut seen: [bool; 5] = [false, false, false, false, false];
    for pr in &pairs2 {
        assert!(be_u16(&pr.0[..], 4) == 41);
        let fr = png_filter_types(&pr.1[..]);
        match fr {
            Err(_e) => { panic!(); }
            Ok(fs) => {
                assert!(fs.len() >= 2);
                for f in &fs {
                    if *f < 5 {
                        seen[*f as usize] = true;
                    } else {
                        panic!();
                    }
                }
            }
        }
    }
    for s in seen.iter() {
        assert!(*s);
    }
}

// ---------------------------------------------------------------------------
// Typed rejection: DRT 5.1 (dictionary packing) is declared by nothing we can
// read, so it must surface as UnsupportedData(1), never a guessed array.
// ---------------------------------------------------------------------------

#[test]
fn dictionary_packing_rejected_with_typed_error() {
    let r = decode_file(fixture_path("unsupported_drt.grib2"), ParamFilter::All);
    match r {
        Ok((_m, _s)) => { panic!(); }
        Err(e) => {
            match e {
                GribErr::UnsupportedData(tmpl) => { assert!(tmpl == 1); }
                _ => {
                    println!("unexpected error kind for dictionary packing");
                    panic!();
                }
            }
        }
    }
}
