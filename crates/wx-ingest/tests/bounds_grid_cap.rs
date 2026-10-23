// V1.4 phase B1 (1/6): GLOBAL GRID-POINT CEILING at grid-definition parse.
//
// nx and ny are content scalars read straight out of section 3. Before the
// MAX_GRID_POINTS cap existed, a forged template 3.0 declaring nx = ny = 1e6
// parsed cleanly, and EVERY downstream plane allocation sized itself from
// nx*ny: 1e12 points per field. The first defined point then walked the tiny
// section 7 with an unchecked get_bits -> slice-index panic -> whole-process
// abort under the release profile's panic="abort" (and had section 7 been
// large enough, the vec push loop would simply have OOMed the ingest pool:
// the mission's 4 TB-plane class). RED-FIRST: pre-fix this file's three tests
// panicked inside decode_stream (captured during the red run); post-fix each
// returns GribErr::BadField("grid point count exceeds MAX_GRID_POINTS").
//
// The cap lives where grid definitions are PARSED (section 3), so the full
// decoder, the wind-pair probe and the retention peek all reject absurd
// shapes identically. Config plumbing of the knob is deferred (see the code
// comment on the constant); until then these tests pin the compile-time
// ceiling: 2^26 = 67.1M points, ~64x the largest real NCEP grid.

// Clippy policy for integration tests mirrors the src modules (see lib.rs):
// this suite keeps the dense explicit-return / explicit-field style, so the
// matching pedantic-but-cosmetic lints are allowed per-file, narrowly listed.
#![allow(clippy::needless_return)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::needless_arbitrary_self_type)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::manual_div_ceil)]
#![allow(clippy::single_match)]
#![allow(clippy::len_zero)]

use std::fs;

use wx_ingest::grib2::be_u32;
use wx_ingest::grib2::decode_stream;
use wx_ingest::grib2::GribErr;
use wx_ingest::grib2::ParamFilter;

fn fixture(name: &str) -> Vec<u8> {
    let p = format!("../../tests/fixtures/{0}", name);
    match fs::read(&p) {
        Err(_) => { println!("missing fixture {0}", p); panic!(); }
        Ok(v) => { return v; }
    }
}

fn be_u64_at(b: &[u8], o: usize) -> u64 {
    let mut v: u64 = 0;
    let mut k = 0;
    while k < 8 {
        v = (v << 8) | (b[o + k] as u64);
        k += 1;
    }
    return v;
}

fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o] = ((v >> 24) & 0xFF) as u8;
    b[o + 1] = ((v >> 16) & 0xFF) as u8;
    b[o + 2] = ((v >> 8) & 0xFF) as u8;
    b[o + 3] = (v & 0xFF) as u8;
}

/// Section headers of the message at `pos`: (number, start, declared length).
/// Same framing convention the decoder itself walks (u4 len-including-itself,
/// sect#, payload; bare `7777` terminator ends the walk).
fn walk_sections(b: &[u8], pos: usize) -> Vec<(u8, usize, usize)> {
    let msglen = be_u64_at(b, pos + 8) as usize;
    let body_end = pos + msglen;
    let mut out: Vec<(u8, usize, usize)> = Vec::new();
    let mut q = pos + 16;
    while q + 5 <= body_end {
        if b[q] == 0x37 && b[q + 1] == 0x37 && b[q + 2] == 0x37 &&
           b[q + 3] == 0x37 {
            break;
        }
        let ln = be_u32(b, q) as usize;
        let num = b[q + 4];
        out.push((num, q, ln));
        if ln < 5 || q + ln > body_end {
            break;
        }
        q += ln;
    }
    return out;
}

/// The FIRST message only — every forgery here lives in one message, and a
/// standalone message decodes without touching its neighbours.
fn first_msg(bytes: &[u8]) -> Vec<u8> {
    let mlen = be_u64_at(bytes, 8) as usize;
    return bytes[0 .. mlen].to_vec();
}

/// Absolute payload offset of section `num`'s payload start in the message at
/// position 0 (payload = after the 5-octet len+number header).
fn sec_payload_off(m: &[u8], num: u8) -> usize {
    for s in &walk_sections(m, 0) {
        if s.0 == num {
            return s.1 + 5;
        }
    }
    println!("no section {0}", num);
    panic!();
}

fn expect_grid_cap(bytes: &[u8], what: &str) {
    let r = decode_stream(bytes, ParamFilter::All);
    match r {
        Err(GribErr::BadField(w)) => {
            assert!(w.contains("MAX_GRID_POINTS"));
        }
        other => {
            println!("{0}: expected BadField(MAX_GRID_POINTS), got {1:?}",
                     what, other.is_ok());
            panic!();
        }
    }
}

#[test]
fn absurd_regular_grid_is_a_typed_reject_not_an_abort() {
    // Template 3.0: nx at payload 25, ny at payload 29 (parse_grid_regular).
    // Forge BOTH to 1e6: 1e12 grid points, 15x over the 2^26 ceiling.
    let mut m = first_msg(&fixture("gfs_small.grib2"));
    let p3 = sec_payload_off(&m, 3);
    put_u32(&mut m, p3 + 25, 1_000_000);
    put_u32(&mut m, p3 + 29, 1_000_000);
    expect_grid_cap(&m, "regular template 3.0");
}

#[test]
fn absurd_lambert_grid_is_a_typed_reject_not_an_abort() {
    // Template 3.30 carries nx/ny at the SAME payload offsets 25/29
    // (parse_grid_lambert). HRRR's real 1799 x 1059 stays two orders of
    // magnitude under the cap; this forgery must not reach plane allocation.
    let mut m = first_msg(&fixture("hrrr_small.grib2"));
    let p3 = sec_payload_off(&m, 3);
    put_u32(&mut m, p3 + 25, 1_000_000);
    put_u32(&mut m, p3 + 29, 1_000_000);
    expect_grid_cap(&m, "lambert template 3.30");
}

#[test]
fn absurd_gaussian_grid_is_a_typed_reject_not_an_abort() {
    // Template 3.40 (NCEP gaussian): nx@25 ny@29 nlat@62. Keep the
    // ny == 2*nlat shape rule SATISFIED so the reject comes from the size
    // ceiling, not from the thinned/regional dimension gate.
    let mut m = first_msg(&fixture("hybrid_small.grib2"));
    let p3 = sec_payload_off(&m, 3);
    put_u32(&mut m, p3 + 25, 1_000_000);
    put_u32(&mut m, p3 + 29, 2_000_000);
    put_u32(&mut m, p3 + 62, 1_000_000);
    expect_grid_cap(&m, "gaussian template 3.40");
}

#[test]
fn honest_grids_still_parse_after_the_cap_exists() {
    // The cap must reject ABSURD shapes, not real ones: gfs_small (41 x 21),
    // hrrr_small and hybrid_small all decode exactly as before (the wgrib2
    // checksum goldens pin their VALUES elsewhere; here we only pin that the
    // parse path itself stayed open).
    for name in ["gfs_small.grib2", "hrrr_small.grib2", "hybrid_small.grib2"] {
        let r = decode_stream(&fixture(name), ParamFilter::All);
        match r {
            Ok((msgs, _st)) => { assert!(!msgs.is_empty()); }
            Err(e) => {
                println!("{0} regressed under the cap: {1:?}", name, e);
                panic!();
            }
        }
    }
}
