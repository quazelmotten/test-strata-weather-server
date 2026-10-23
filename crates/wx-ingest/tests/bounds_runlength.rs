// V1.4 phase B1 (4/6): DRT 5.200 RUN-LENGTH level-table extent proof.
//
// The level table lives INSIDE section 5: sm_i2(s5, 10 + i*2) for
// i = 1..mvl, and mvl itself is a forged u16 read from that same section.
// Pre-fix there was NO proof that s5 extended past the table before the
// walk ran: a 12-octet payload declaring mvl = 3000 asked for octet ~6012
// of a 12-octet slice -> be_u16 index panic -> whole-process abort under
// release panic="abort". RED-FIRST: both forgery tests panicked pre-fix at
// the first slot past the payload; post-fix each is GribErr::Truncated,
// computed as 12 + 2*mvl > s5.len() BEFORE the walk (which additionally
// bounds the `levels` allocation by the payload length).
//
// The honest-table test pins that the formula agrees with what real
// producers write (scripts/gen_grib2.py's 5.200 message, wgrib2-decoded in
// runlength_5200.rs): the check must reject ONLY tables that genuinely run
// past the declared section.

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

/// Rewrite the u16 missing-value-level-count (section-5 payload octets 10..11,
/// wgrib2 unpk_run_length.c mvl=uint2(sec5+14)) and re-frame the message.
fn forge_mvl(bytes: &[u8], mvl: u16) -> Vec<u8> {
    let (h, mut secs, t) = split_first_msg(bytes);
    let k5 = sec_idx(&secs, 5);
    assert!(secs[k5].payload.len() >= 12);
    secs[k5].payload[10] = ((mvl >> 8) & 0xFF) as u8;
    secs[k5].payload[11] = (mvl & 0xFF) as u8;
    return rebuild_msg(&h[..], &secs[..], &t[..]);
}

#[test]
fn forged_level_count_is_a_truncated_error_not_an_abort() {
    // The real fixture table is tiny (three levels); declaring 3000 demands
    // a ~6 KB section 5 that the declared length does not provide.
    let msg = forge_mvl(&fixture("runlength_small.grib2"), 3000);
    match decode_stream(&msg, ParamFilter::All) {
        Err(GribErr::Truncated) => {}
        other => {
            println!("mvl=3000: expected Truncated, got ok={0}", other.is_ok());
            panic!();
        }
    }
}

#[test]
fn max_declared_level_count_is_a_truncated_error_too() {
    // mvl's worst case: the full u16 range against whatever the section
    // actually holds. Same typed reject, no walk, no abort.
    let msg = forge_mvl(&fixture("runlength_small.grib2"), 65535);
    match decode_stream(&msg, ParamFilter::All) {
        Err(GribErr::Truncated) => {}
        other => {
            println!("mvl=65535: expected Truncated, got ok={0}",
                     other.is_ok());
            panic!();
        }
    }
}

#[test]
fn honest_level_table_still_decodes() {
    // Control: framing-only rebuild of the untouched message must decode
    // exactly like the raw fixture (231-point plane), proving the extent
    // formula matches what gen_grib2.py + wgrib2 agree the table occupies.
    let b = fixture("runlength_small.grib2");
    let (h, secs, t) = split_first_msg(&b);
    let msg = rebuild_msg(&h[..], &secs[..], &t[..]);
    let r = decode_stream(&msg, ParamFilter::All);
    match r {
        Ok((msgs, _st)) => {
            assert!(!msgs.is_empty() && msgs[1..].is_empty());
            assert!(msgs[0].data.len() == 231);
        }
        Err(e) => {
            println!("honest run-length table regressed: {0:?}", e);
            panic!();
        }
    }
}
