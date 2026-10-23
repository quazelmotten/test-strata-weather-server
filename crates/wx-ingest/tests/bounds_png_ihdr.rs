// V1.4 phase B1 (5/6): DRT 5.41 PNG IHDR ALLOCATION CAPS + checked sizing.
//
// The PNG packed into section 7 declares its geometry in an IHDR chunk whose
// width and height are UNBOUNDED u32s, and the decoder allocated the whole
// inflated row buffer UP FRONT: need = (rowlen + 1) * h. Two failure classes:
//
//   * OOM-capable: a forged 65535 x 65535 asked for ~8.6 GB (16-bit) or
//     ~4.3 GB (8-bit) before a single IDAT byte was consulted; 40000 x
//     40000 asks for ~13 GB. On a memory-tight box that is an allocation
//     abort ON THE INGEST POOL -> whole process dies next to live HTTP.
//   * wrap-to-small: every multiply in that expression ran WITHOUT overflow
//     checks (release profile), so dimensions large enough to wrap usize
//     produced a small `need`, a short inflate, and a stream that could pass
//     the total_out check while discarding most of the field. The fix makes
//     ALL of it checked arithmetic, so no wrap can smuggle anything through
//     on any target (the exposure is worst on 32-bit builds where these
//     products wrap daily).
//
// Documented caps now enforced: each axis <= 65536 px, and w*h <= the SAME
// global grid ceiling (MAX_GRID_POINTS) the section-3 parser applies — one
// PNG-packed message holds exactly one field, so an image bigger than the
// grid ceiling cannot describe anything this server would serve.
//
// RED-FIRST: pre-fix these tests failed on the ASSERTION itself — the forged
// streams were either accepted as garbage or refused later with an unrelated
// CodecFailure(png-inflate*) after the giant upfront allocation attempt —
// never with the documented typed cap reject. Post-fix: BadField on the cap.
//
// IHDR chunks are CRC-checked here (as in the decoder), so every forgery
// recomputes the chunk CRC with the same zlib-rs chaining the decoder uses.

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
use zlib_rs::crc32::crc32;

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

struct Sec {
    num: u8,
    payload: Vec<u8>,
}

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

fn rebuild_msg(head8: &[u8], secs: &[Sec], tail: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = head8.to_vec();
    put_be_u32(&mut out, 0);
    put_be_u32(&mut out, 0);
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

/// Patch IHDR width/height inside the FIRST chunk after the PNG signature at
/// s7[png], then repair the chunk CRC exactly like a producer would (RFC
/// PNG: CRC covers the four TYPE bytes plus the data, zlib-style chaining).
fn patch_ihdr(s7: &mut [u8], w: u32, h: u32) {
    assert!(s7.len() > 8 + 4 + 4 + 13 + 4);
    // first chunk header: len(4) | "IHDR" | data(13) | crc(4)
    let clen = be_u32(s7, 8) as usize;
    assert!(clen >= 13);
    assert!(s7[12] == 0x49 && s7[13] == 0x48 && s7[14] == 0x44 &&
            s7[15] == 0x52);
    let data = 16; // payload start of chunk 0
    s7[data] = ((w >> 24) & 0xFF) as u8;
    s7[data + 1] = ((w >> 16) & 0xFF) as u8;
    s7[data + 2] = ((w >> 8) & 0xFF) as u8;
    s7[data + 3] = (w & 0xFF) as u8;
    s7[data + 4] = ((h >> 24) & 0xFF) as u8;
    s7[data + 5] = ((h >> 16) & 0xFF) as u8;
    s7[data + 6] = ((h >> 8) & 0xFF) as u8;
    s7[data + 7] = (h & 0xFF) as u8;
    let body = &s7[data .. data + clen];
    let seed = crc32(0, b"IHDR");
    let c = crc32(seed, body);
    let co = data + clen;
    s7[co] = ((c >> 24) & 0xFF) as u8;
    s7[co + 1] = ((c >> 16) & 0xFF) as u8;
    s7[co + 2] = ((c >> 8) & 0xFF) as u8;
    s7[co + 3] = (c & 0xFF) as u8;
}

fn forge_dims(bytes: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (h8, mut secs, t) = split_first_msg(bytes);
    let k7 = sec_idx(&secs, 7);
    patch_ihdr(secs[k7].payload.as_mut_slice(), w, h);
    return rebuild_msg(&h8[..], &secs[..], &t[..]);
}

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
fn ihdr_axis_beyond_the_documented_cap_is_refused_before_allocating() {
    // Both axes over the per-axis ceiling (100000 > 65536): the reject must
    // name the AXIS cap and must happen before any buffer sizing at all.
    let msg = forge_dims(&fixture("gfs_small_png.grib2"), 100_000, 100_000);
    match first_plane(&msg) {
        Err(GribErr::BadField(w)) => {
            assert!(w.contains("png dimension"));
        }
        other => {
            println!("100000x100000: expected BadField(png dimension cap), got {0:?}",
                     other);
            panic!();
        }
    }
}

#[test]
fn mission_example_fortynine_squared_is_caught_by_the_product_ceiling() {
    // The mission's own forged shape, 65535 x 65535: each axis sits JUST
    // under the per-axis ceiling, so the TOTAL-pixels cap (MAX_GRID_POINTS,
    // same global ceiling as section 3) is what refuses it — ~4.29e9 pixels
    // would demand ~8.6 GB upfront on a 16-bit stream. Pre-fix this got as
    // far as the allocation attempt and only died later on
    // CodecFailure(png-inflate-length) (see red capture); now it is a named
    // typed reject BEFORE allocating.
    let msg = forge_dims(&fixture("gfs_small_png.grib2"), 65535, 65535);
    match first_plane(&msg) {
        Err(GribErr::BadField(w)) => {
            assert!(w.contains("png pixel count"));
        }
        other => {
            println!("65535x65535: expected BadField(png pixel cap), got {0:?}",
                     other);
            panic!();
        }
    }
}

#[test]
fn ihdr_pixel_product_beyond_the_grid_ceiling_is_refused_too() {
    // Both axes UNDER the axis cap individually (40000 <= 65536) but the
    // product 1.6e9 blows through MAX_GRID_POINTS: this is the case where
    // an axis-only cap still lets ~13 GB of upfront allocation through.
    let msg = forge_dims(&fixture("gfs_small_png.grib2"), 40000, 40000);
    match first_plane(&msg) {
        Err(GribErr::BadField(w)) => {
            assert!(w.contains("png pixel count"));
        }
        other => {
            println!("40000x40000: expected BadField(png pixel cap), got {0:?}",
                     other);
            panic!();
        }
    }
}

#[test]
fn honest_ihdr_still_decodes_bit_identically() {
    // Re-frame the message WITHOUT touching any byte (CRC repair included is
    // a no-op on valid input): the caps must not disturb real geometry, and
    // the decoded plane stays bit-identical to the raw fixture — the PNG
    // golden tripwire in packing_twins covers values; this pins the forgery
    // harness itself.
    let b = fixture("gfs_small_png.grib2");
    let base = first_plane(&b);
    let (h8, secs, t) = split_first_msg(&b);
    let msg = rebuild_msg(&h8[..], &secs[..], &t[..]);
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
            println!("framing-only PNG rebuild failed to decode");
            panic!();
        }
    }
}
