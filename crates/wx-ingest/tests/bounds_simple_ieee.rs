// V1.4 phase B1 (2/6): DRT 5.0 / 5.4 section-7 EXTENT PRE-CHECKS plus the
// BITMAP LENGTH GUARD.
//
// Two unchecked-content classes are closed here:
//
//   * simple-packing walked ngrid * nbits bits through get_bits with no proof
//     that section 7 holds them (grib2.rs bit-walk loop over ngrid), and the
//     IEEE-FP walk read 4 unchecked bytes per defined point. A truncated or
//     short section 7 -> slice-index panic -> whole-process abort under the
//     release profile's panic="abort". RED-FIRST: tests 1 and 2 panicked
//     inside decode_stream pre-fix; post-fix both are GribErr::Truncated.
//
//   * bitmap handling never cross-checked ceil(ngrid/8) against the bitmap
//     octets actually present, so bm_defined indexed off the end mid-
//     interleave. The BadField("bitmap too short") outcome was declared but
//     NEVER constructed anywhere in the decoder. RED-FIRST: tests 3 and 5
//     panicked pre-fix; post-fix both construct exactly that typed error.
//
// Test 4 pins the OTHER direction — the guard must not reject a WELL-FORMED
// full-set bitmap: decoding stays byte-identical to the no-bitmap twin. The
// checksum goldens (checksum_parity/packing_twins) cover the rest of the
// clean-stream surface.

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

fn put_be_u32(b: &mut Vec<u8>, v: u32) {
    b.push(((v >> 24) & 0xFF) as u8);
    b.push(((v >> 16) & 0xFF) as u8);
    b.push(((v >> 8) & 0xFF) as u8);
    b.push((v & 0xFF) as u8);
}

#[derive(Clone)]
struct Sec {
    num: u8,
    payload: Vec<u8>,
}

/// Split the FIRST message of a stream into its sections plus the trailing
/// terminator tail (the four ASCII '7' bytes the writer counts INSIDE the
/// message length). Sections are returned payload-first so any one of them
/// can be replaced or inserted and the whole message re-framed consistently.
fn split_first_msg(bytes: &[u8]) -> (Vec<u8>, Vec<Sec>, Vec<u8>) {
    let mlen = be_u64_at(bytes, 8) as usize;
    let m = &bytes[0 .. mlen];
    let mut secs: Vec<Sec> = Vec::new();
    let mut q = 16;
    while q + 5 <= mlen {
        if m[q] == 0x37 && m[q + 1] == 0x37 && m[q + 2] == 0x37 &&
           m[q + 3] == 0x37 {
            break;
        }
        let ln = be_u32(m, q) as usize;
        let num = m[q + 4];
        secs.push(Sec { num: num, payload: m[q + 5 .. q + ln].to_vec() });
        q += ln;
    }
    let tail = m[q .. mlen].to_vec();
    return (m[0 .. 8].to_vec(), secs, tail);
}

/// Re-frame a single-message stream from (octet0..8, sections, tail): every
/// section gets a fresh u4 len-including-itself header and the section-0
/// uint64 length is patched last, so forged PAYLOAD SIZES cannot desync the
/// framing layer (that would test the wrong thing — these tests target the
/// content scalars and payload extents INSIDE already-valid framing).
fn rebuild_msg(head8: &[u8], secs: &[Sec], tail: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = head8.to_vec();
    put_be_u32(&mut out, 0);
    put_be_u32(&mut out, 0); // placeholder uint64 message length
    for s in secs {
        put_be_u32(&mut out, (5 + s.payload.len()) as u32);
        out.push(s.num);
        out.extend_from_slice(&s.payload[..]);
    }
    out.extend_from_slice(tail);
    let n = out.len() as u64;
    for k in 0 .. 8 {
        out[8 + k] = ((n >> (8 * (7 - k))) & 0xFF) as u8;
    }
    return out;
}

fn sec_idx(secs: &[Sec], num: u8) -> usize {
    for k in 0 .. secs.len() {
        if secs[k].num == num {
            return k;
        }
    }
    println!("no section {0}", num);
    panic!();
}

/// Replace section `num`'s payload, or INSERT it after section 5 when absent
/// (section order 1,3,4,5,6,7,8 is what both producers emit; the decoder
/// walks by declared lengths and tolerates either shape).
fn forge_section(bytes: &[u8], num: u8, new_payload: &[u8]) -> Vec<u8> {
    let (h, mut secs, t) = split_first_msg(bytes);
    let mut found = false;
    for k in 0 .. secs.len() {
        if secs[k].num == num {
            secs[k].payload = new_payload.to_vec();
            found = true;
        }
    }
    if !found {
        let after5 = sec_idx(&secs, 5);
        let ins = after5 + 1;
        secs.insert(ins, Sec { num: num, payload: new_payload.to_vec() });
    }
    return rebuild_msg(&h[..], &secs[..], &t[..]);
}

/// Decode and hand back the FIRST message's plane (forgeries are single-
/// message slices; the whole-fixture baseline has 39, so "first" serves
/// both roles).
fn first_plane(msg: &[u8]) -> Result<Vec<f32>, GribErr> {
    let r = decode_stream(msg, ParamFilter::All);
    match r {
        Err(e) => { return Err(e); }
        Ok((msgs, _st)) => {
            assert!(!msgs.is_empty());
            return Ok(msgs[0].data.clone());
        }
    }
}

#[test]
fn simple_short_section7_is_a_truncated_error() {
    // DRT 5.0 on gfs_small's first message: keep HALF of the value bitstream
    // (the walk needs ceil(ndef * nbits / 8) octets and gets half of that).
    // Pre-fix the bit-walk ran off the payload end -> get_bits index panic.
    let b = fixture("gfs_small.grib2");
    let (h, secs, t) = split_first_msg(&b);
    let k = sec_idx(&secs, 7);
    let full = secs[k].payload.clone();
    assert!(full.len() >= 16);
    let mut forged = secs.clone();
    forged[k].payload = full[0 .. full.len() / 2].to_vec();
    let msg = rebuild_msg(&h[..], &forged[..], &t[..]);
    match first_plane(&msg) {
        Err(GribErr::Truncated) => {}
        other => {
            println!("short simple s7: expected Truncated, got {0:?}", other);
            panic!();
        }
    }
    // control: the untouched message still decodes (framing rewrite alone
    // must not change behaviour)
    let ctrl = rebuild_msg(&h[..], &secs[..], &t[..]);
    match first_plane(&ctrl) {
        Ok(_p) => {}
        Err(e) => { println!("control regressed: {0:?}", e); panic!(); }
    }
}

#[test]
fn ieee_short_section7_is_a_truncated_error() {
    // DRT 5.4 on gfs_small_ieee's first message: ndef * 4 bytes are read
    // unchecked per point; keep a quarter of the stream.
    let b = fixture("gfs_small_ieee.grib2");
    let (h, secs, t) = split_first_msg(&b);
    let k = sec_idx(&secs, 7);
    let full = secs[k].payload.clone();
    assert!(full.len() >= 64);
    let mut forged = secs.clone();
    forged[k].payload = full[0 .. full.len() / 4].to_vec();
    let msg = rebuild_msg(&h[..], &forged[..], &t[..]);
    match first_plane(&msg) {
        Err(GribErr::Truncated) => {}
        other => {
            println!("short ieee s7: expected Truncated, got {0:?}", other);
            panic!();
        }
    }
}

#[test]
fn bitmap_one_octet_short_of_grid_is_a_typed_reject() {
    // gfs_small's grid is 41 x 21 = 861 points -> 108 bitmap octets required.
    // Forge an indicator-0 section 6 carrying EIGHT: every present bit says
    // DEFINED, so pre-fix the interleave walked bm_defined off the eight-byte
    // end at point 64 -> slice panic. Post-fix: the declared-but-never-built
    // BadField("bitmap too short") outcome fires before any unpacking.
    let b = fixture("gfs_small.grib2");
    let mut p6: Vec<u8> = vec![0x00u8];
    for _k in 0 .. 8 {
        p6.push(0xFF);
    }
    let msg = forge_section(&b, 6, &p6[..]);
    match first_plane(&msg) {
        Err(GribErr::BadField(w)) => {
            assert!(w.contains("bitmap too short"));
        }
        other => {
            println!("short bitmap: expected BadField(bitmap too short), got {0:?}",
                     other);
            panic!();
        }
    }
}

#[test]
fn full_set_bitmap_decodes_exactly_like_no_bitmap() {
    // The guard must reject ONLY corrupt bitmaps: a well-formed all-defined
    // bitmap (indicator 0 + exactly ceil(ngrid/8) octets set) changes nothing
    // about the values — same bits consumed, same scaling, no NaN.
    let b = fixture("gfs_small.grib2");
    let base = first_plane(&b);
    let (_h, secs, _t) = split_first_msg(&b);
    let k3 = sec_idx(&secs, 3);
    let nx = be_u32(&secs[k3].payload, 25) as usize;
    let ny = be_u32(&secs[k3].payload, 29) as usize;
    let need = (nx * ny + 7) / 8;
    let mut p6: Vec<u8> = vec![0x00u8];
    for _k in 0 .. need {
        p6.push(0xFF);
    }
    let msg = forge_section(&b, 6, &p6[..]);
    let got = first_plane(&msg);
    match (&base, &got) {
        (Ok(db), Ok(dg)) => {
            assert!(db.len() == dg.len());
            for k in 0 .. db.len() {
                if db[k].is_nan() || dg[k].is_nan() {
                    assert!(db[k].is_nan() && dg[k].is_nan());
                } else {
                    assert!(db[k].to_bits() == dg[k].to_bits());
                }
            }
        }
        _ => {
            println!("full-set bitmap failed to decode like the original");
            panic!();
        }
    }
}

#[test]
fn ieee_bitmap_too_short_is_a_typed_reject() {
    // Same guard, IEEE path: the central check lives in decode_sec7 so every
    // DRT refuses identically BEFORE its own unpacker runs.
    let b = fixture("gfs_small_ieee.grib2");
    let mut p6: Vec<u8> = vec![0x00u8];
    for _k in 0 .. 8 {
        p6.push(0xFF);
    }
    let msg = forge_section(&b, 6, &p6[..]);
    match first_plane(&msg) {
        Err(GribErr::BadField(w)) => {
            assert!(w.contains("bitmap too short"));
        }
        other => {
            println!("short ieee bitmap: expected BadField(bitmap too short), got {0:?}",
                     other);
            panic!();
        }
    }
}
