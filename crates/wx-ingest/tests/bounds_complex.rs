// V1.4 phase B1 (3/6): DRT 5.2/5.3 COMPLEX-PACKING pre-checks — the biggest
// abort surface in the decoder, because THREE forged section-5 scalars feed
// unchecked loops and allocations at once:
//
//   * ngroups is an UNCAPPED u32 (payload octet 27..30). Pre-fix the group-
//     reference walk pushed get_bits(s7, pos, nbits) ngroups times: 0xFFFF-
//     FFFF walked the bit cursor off the payload end -> index panic (and had
//     the widths been wide, the three Vec growths would have OOMed first).
//     Post-fix two typed rejects bracket it: ngroups <= np (every group
//     covers at least one stored value, and np is capped by the grid hence
//     MAX_GRID_POINTS) plus an exact byte/bit proof of the refs/widths/
//     reserved-lengths phases against the section-7 extent.
//   * np must equal the bitmap-defined count (the SAME cross-check every
//     codec wrapper already runs). Pre-fix a stream storing FEWER values
//     than the grid declared decoded until the final interleave indexed
//     ud[j] past the end -> slice panic. RED-FIRST: test (b) panicked at
//     exactly that line pre-fix.
//   * the continuous value bitstream now has a proven extent too: one pass
//     over lens*widths per message proves every get_bits in the group walk.
//
// Tests (c)/(d) shrink section 7 instead — framing stays valid, content
// extents do not, and BOTH reject classes stay typed (Truncated or BadField
// depending on which scalar goes out of range first; never a panic). The
// total!=np gate additionally became SATURATING rather than wrapping: under
// overflow-checks-off a forged lens array could wrap its sum right back onto
// np and the value loop iterated the true (astronomical) sum — OOM-class.

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

/// Forge DRT 5.3 section-5 CONTENT scalars (same payload length, framing
/// untouched) and/or truncate section 7's payload. `p32` are (payload
/// offset, u32 BE) multi-octet patches, `p8` single-byte patches — kept
/// separate because the extra-octets byte sits at the LAST payload slot, so
/// a 4-byte write there would run off the payload end inside the test.
fn forge(bytes: &[u8], p32: &[(usize, u32)], p8: &[(usize, u8)],
         s7_keep: Option<usize>) -> Vec<u8> {
    let (h, mut secs, t) = split_first_msg(bytes);
    let k5 = sec_idx(&secs, 5);
    for p in p32 {
        assert!(p.0 + 4 <= secs[k5].payload.len());
        let pl = secs[k5].payload.as_mut_slice();
        for j in 0 .. 4 {
            pl[p.0 + j] = ((p.1 >> (8 * (3 - j))) & 0xFF) as u8;
        }
    }
    for p in p8 {
        assert!(p.0 < secs[k5].payload.len());
        secs[k5].payload[p.0] = p.1;
    }
    match s7_keep {
        None => {}
        Some(n) => {
            let k7 = sec_idx(&secs, 7);
            let keep = secs[k7].payload.clone();
            assert!(n < keep.len());
            secs[k7].payload = keep[0 .. n].to_vec();
        }
    }
    return rebuild_msg(&h[..], &secs[..], &t[..]);
}

fn first_result(msg: &[u8]) -> Result<(), GribErr> {
    let r = decode_stream(msg, ParamFilter::All);
    match r {
        Err(e) => { return Err(e); }
        Ok((_msgs, _st)) => { return Ok(()); }
    }
}

#[test]
fn forged_huge_ngroups_is_a_typed_reject_not_an_abort() {
    // ngroups = 0xFFFFFFFF at section-5 payload octet 26. Pre-fix the refs
    // walk pushed get_bits results until the bit cursor left the payload
    // (slice panic) — hundreds of millions of iterations before dying.
    let msg = forge(&fixture("gfs_small_c3.grib2"),
                    &[(26, 0xFFFF_FFFF)], &[], None);
    match first_result(&msg) {
        Err(GribErr::BadField(w)) => {
            assert!(w.contains("group count"));
        }
        other => {
            println!("huge ngroups: expected BadField(group count), got {0:?}",
                     other);
            panic!();
        }
    }
}

#[test]
fn moderate_ngroups_beyond_the_point_count_is_refused_too() {
    // The ngroups <= np cap must fire on modest forgeries as well, not just
    // sentinel values: pick a value comfortably above np (~861 points on
    // this fixture's 41 x 21 grid) but below any header-extent threshold so
    // THIS check is what rejects.
    let msg = forge(&fixture("gfs_small_c3.grib2"), &[(26, 900)], &[], None);
    match first_result(&msg) {
        Err(GribErr::BadField(w)) => {
            assert!(w.contains("group count"));
        }
        other => {
            println!("ngroups=900: expected BadField(group count), got {0:?}",
                     other);
            panic!();
        }
    }
}

#[test]
fn stored_points_below_defined_count_is_a_typed_reject() {
    // np -= 7 AND len_last -= 7 keep the group-length sum EQUAL to np, so the
    // pre-fix total!=np gate passed and the FINAL INTERLEAVE indexed ud[j]
    // past the end when the grid declared more points than were stored —
    // exactly the slice panic this pair closes. (len_last lives at payload
    // octet 37..41 per complex_pk.c; the fixture's real value is far above
    // 7, verified by the control decode below.)
    let b = fixture("gfs_small_c3.grib2");
    let (_h, secs, _t) = split_first_msg(&b);
    let k5 = sec_idx(&secs, 5);
    let np = be_u32(&secs[k5].payload, 0);
    let ll = be_u32(&secs[k5].payload, 37);
    assert!(np >= 8 && ll >= 8);
    let msg = forge(&b, &[(0, np - 7), (37, ll - 7)], &[], None);
    match first_result(&msg) {
        Err(GribErr::BadField(w)) => {
            assert!(w.contains("defined-point count mismatch"));
        }
        other => {
            println!("np<ndef: expected BadField(defined-point count), got {0:?}",
                     other);
            panic!();
        }
    }
}

#[test]
fn shrunken_section7_stays_a_typed_error() {
    // Halving the value stream must land in one of the two typed reject
    // families depending on which phase runs out: Truncated (the extent or
    // value-bitstream proofs) or BadField (the total != np gate once the
    // length phase itself survives). NEVER a panic, NEVER a silent short
    // plane. Control: framing-only rebuild still decodes.
    let b = fixture("gfs_small_c3.grib2");
    let (h, secs, t) = split_first_msg(&b);
    let k7 = sec_idx(&secs, 7);
    let half = secs[k7].payload.len() / 2;
    let msg = forge(&b, &[], &[], Some(half));
    match first_result(&msg) {
        Err(GribErr::Truncated) => {}
        Err(GribErr::BadField(_w)) => {}
        other => {
            println!("halved s7: expected typed Err, got {0:?}", other);
            panic!();
        }
    }
    let ctrl = rebuild_msg(&h[..], &secs[..], &t[..]);
    match first_result(&ctrl) {
        Ok(()) => {}
        Err(e) => {
            println!("control c3 message regressed under framing rebuild: {0:?}",
                     e);
            panic!();
        }
    }
}

#[test]
fn extra_octets_beyond_the_stream_is_a_typed_reject() {
    // DRT 5.3 payload octet 44 carries the extra-octets count (seeds + vmn).
    // Forged to 255 it demands 255 * (nseed+1) bytes up front plus shifts
    // every later phase right — either the header proof or the value-phase
    // proof fires, both are Truncated. Pre-fix uint_n read the seeds with no
    // bound at all and the walks followed.
    let msg = forge(&fixture("gfs_small_c3.grib2"), &[], &[(43, 255)], None);
    match first_result(&msg) {
        Err(GribErr::Truncated) => {}
        other => {
            println!("eo=255: expected Truncated, got {0:?}", other);
            panic!();
        }
    }
}
