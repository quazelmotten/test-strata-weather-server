// V1.3 phase E4b: GRIB2 section-3 template 3.40 decoding pinned against the
// EXACT 67-byte payload of message 2 of tests/realdata/gfs_sflux_f000.grib2
// (live GFS sflux, run 2026-10-21 12Z, fetched by scripts/fetch_real_sample.sh
// and dumped byte-for-byte during E4b recon). The expected values on the
// right-hand side come from `tools/wgrib2-full <file> -grid`, which prints:
//   Gaussian grid: (3072 x 1536) units 1e-06 input WE:NS output WE:SN
//     number of latitudes between pole-equator=768 #points=4718592
//     lat 89.910324 to -89.910324
//     lon 0.000000 to 359.882813 by 0.117188
// Embedding the bytes makes this gate network-free; the realdata_gate covers
// the same file end to end when the fixtures are present.

use wx_core::grid::GridDef;
use wx_ingest::grib2::{parse_sec3, GribErr};

fn live_payload() -> Vec<u8> {
    // verbatim from the recon dump (program-printed, one space between bytes;
    // the space-stripping below makes transcription by compressed nibbles
    // impossible to get wrong):
    let hex = "00 00 48 00 00 00 00 00 28 06 00 00 00 00 00 00 \
               00 00 00 00 00 00 00 00 00 00 00 0c 00 00 00 06 \
               00 00 00 00 00 00 00 00 00 05 5b ec 34 00 00 00 \
               00 30 85 5b ec 34 15 73 60 3d 00 01 c9 c4 00 00 \
               03 00 00";
    let clean: String = hex.chars().filter(|c| *c != ' ' && *c != '\n').collect();
    let mut out: Vec<u8> = Vec::new();
    let bs: Vec<char> = clean.chars().collect();
    let mut k = 0usize;
    while k + 1 < bs.len() {
        let hi = radix16(bs[k]);
        let lo = radix16(bs[k + 1]);
        out.push(hi * 16 + lo);
        k += 2;
    }
    out
}

fn radix16(c: char) -> u8 {
    if c.is_ascii_digit() { return (c as u8) - b'0'; }
    if ('a'..='f').contains(&c) { return 10 + (c as u8) - b'a'; }
    if ('A'..='F').contains(&c) { return 10 + (c as u8) - b'A'; }
    panic!();
}

fn abs_diff(a: f64, b: f64) -> f64 {
    if a > b { a - b } else { b - a }
}

#[test]
fn sec3_live_gaussian_payload_parses_to_wgrib2_values() {
    let p = live_payload();
    if p.len() != 67 {
        println!("embedded payload length {0} (expected 67)", p.len());
        panic!();
    }
    let g = match parse_sec3(&p) {
        Err(e) => { println!("parse failed: {0:?}", e); panic!(); }
        Ok(gd) => { gd }
    };
    match g {
        GridDef::Gaussian(ref gg) => {
            assert!(gg.nx == 3072);
            assert!(gg.ny == 1536);
            assert!(abs_diff(gg.lon1, 0.0) < 1e-9);
            assert!(abs_diff(gg.la1, 89.910324) < 1e-5);
            assert!(abs_diff(gg.la2, -89.910324) < 1e-5);
            // stored Di is 117188 (rounded); we reconstruct the exact global
            // increment 360/nx (see DECISIONS §16 wrap-reconstruction rule)
            assert!(gg.dlon == 360.0 / 3072.0);
            assert!(gg.scan_mode == 0x00); // NS storage (La1 north)
            assert!(gg.earth_shape == 6);  // CT 3.2 sphere 6371.229 km
            assert!(!gg.winds_grid);       // flag byte 0x30: bit 0x08 clear
            // row table agrees with the printed extremes within 1e-4 deg
            let ts = gg.row_lats();
            assert!(ts.len() == 1536);
            assert!(abs_diff(ts[0], 89.910324) < 1e-4);
            assert!(abs_diff(ts[1535], -89.910324) < 1e-4);
            // recorded probe node (E4a): lon 264.9609375 col 2261, row near
            // 40.006506 must project EXACTLY onto integer grid coordinates
            let mut hit_row = false;
            let mut k = 0usize;
            while k < ts.len() {
                if abs_diff(ts[k], 40.006506) < 5e-7 { hit_row = true; break; }
                k += 1;
            }
            if !hit_row {
                println!("no row within 5e-7 of 40.006506");
                panic!();
            }
            let pr = match gg.project(ts[k], -95.0390625) {
                Some(v) => v,
                None => { panic!(); }
            };
            if pr.0 != 2261.0 || pr.1 != k as f64 {
                println!("probe node projected to {0:?}, want (2261,{1})", pr, k);
                panic!();
            }
            // far-edge column: exact-node projection at 359.8828125 (the
            // unrounded last longitude) proves the dlon reconstruction kept
            // node exactness across all 3072 columns
            let pf = match gg.project(ts[0], 3071.0 * (360.0 / 3072.0)) {
                Some(v) => v,
                None => { panic!(); }
            };
            if pf.0 != 3071.0 || pf.1 != 0.0 {
                println!("far edge projected to {0:?}", pf);
                panic!();
            }
        }
        other => {
            println!("not a gaussian grid: {0:?}", other);
            panic!();
        }
    }
}

#[test]
fn sec3_gaussian_rejections_skip_as_grid_40() {
    // thinned/reduced shape: nlat no longer half of ny -> skip bucket 3.40
    let mut p = live_payload();
    p[62] = 0;
    p[63] = 0;
    p[64] = 2;
    p[65] = 0xbc; // nlat = 700, ny stays 1536
    match parse_sec3(&p) {
        Err(GribErr::UnsupportedGrid(40)) => {}
        other => { println!("nlat mismatch accepted: {0:?}", other); panic!(); }
    }
    // row ends disagreeing with the P_ny table beyond tolerance -> skip
    let mut q = live_payload();
    q[41] = 0x0a; // La1 magnitude 10.6e6-ish nonsense (was 0x05)
    match parse_sec3(&q) {
        Err(GribErr::UnsupportedGrid(40)) => {}
        other => { println!("row-end mismatch accepted: {0:?}", other); panic!(); }
    }
    // truncated section
    let r = live_payload();
    match parse_sec3(&r[..60]) {
        Err(GribErr::Truncated) => {}
        other => { println!("truncated accepted: {0:?}", other); panic!(); }
    }
    // zero nlat
    let mut z = live_payload();
    z[62] = 0; z[63] = 0; z[64] = 0; z[65] = 0;
    match parse_sec3(&z) {
        Err(GribErr::UnsupportedGrid(40)) => {}
        other => { println!("zero nlat accepted: {0:?}", other); panic!(); }
    }
}

#[test]
fn sec3_gaussian_south_north_orientation_accepted() {
    // SN storage: swap the two end latitudes (GRIB sign-magnitude encoding:
    // high bit set == negative). Row validation uses the northern end from
    // whichever slot holds it; scan_mode must come out 0x40.
    let mut p = live_payload();
    // La1 slot <- -89910324 (sign-magnitude): 85 5b ec 34
    p[41] = 0x85; p[42] = 0x5b; p[43] = 0xec; p[44] = 0x34;
    // La2 slot <- +89910324: 05 5b ec 34 (it already starts with 85 there)
    p[50] = 0x05; p[51] = 0x5b; p[52] = 0xec; p[53] = 0x34;
    let g = match parse_sec3(&p) {
        Err(e) => { println!("SN variant rejected: {0:?}", e); panic!(); }
        Ok(gd) => { gd }
    };
    match g {
        GridDef::Gaussian(ref gg) => {
            assert!(gg.scan_mode == 0x40);
            let ts = gg.row_lats();
            // row 0 is the La1 side = southernmost here
            assert!(ts[0] < 0.0 && abs_diff(ts[0], -89.910324) < 1e-4);
            assert!(ts[1535] > 0.0 && abs_diff(ts[1535], 89.910324) < 1e-4);
            // projection still lands on integer nodes at the north end —
            // using the TABLE row value (the request at the file's rounded
            // 89.910324 instead blends with weight ~3e-6/0.117, matching the
            // documented no-snap-beyond-eps behavior)
            let pr = match gg.project(ts[1535], 0.0) {
                Some(v) => v,
                None => { panic!(); }
            };
            if pr.1 != 1535.0 || pr.0 != 0.0 {
                println!("SN dj {0}, want 1535", pr.1);
                panic!();
            }
            // and the rounded La1 from section 3 answers just south of it
            let pr2 = match gg.project(89.910324, 0.0) {
                Some(v) => v,
                None => { panic!(); }
            };
            if !(pr2.1 > 1534.9 && pr2.1 < 1535.0) {
                println!("SN rounded-end dj {0}", pr2.1);
                panic!();
            }
        }
        _ => { panic!(); }
    }
}
