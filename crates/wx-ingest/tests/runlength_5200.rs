// DRT 5.200 run length packing — coverage close (V1.2 phase D, audit P2).
//
// WHY the fixture is hand-written: wgrib2 DECODES run-length packing ("Supported
// decoding: simple, complex, rle, ieee, png, jpeg2000, CCSDS AEC") but has NO
// ENCODER for it — measured on tools/wgrib2-full v3.1.3: `-set_grib_type rle`
// prints `*** FATAL ERROR: set_grib_type: bad type rle ***` (rc=8). The encoder
// lives in g2clib, which this build deliberately disables (docs/DECISIONS.md V1.1
// phase 1). So no reference ENCODER exists anywhere in the wgrib2 ecosystem, and
// the truth chain used here is the strongest available:
//
//   1. `scripts/gen_grib2.py` writes ONE tiny 5.200 message following Code Table
//      5.0's documented layout, cross-checked field-by-field against wgrib2's OWN
//      rle DECODER offsets (`unpk_run_length.c`: n_bits=sec[5][11], mv=uint2(+12),
//      mvl=uint2(+14), decimal_scale=[16], levels[i]=int2(+15+i*2)) and its run
//      arithmetic (n = 1 + Σ factor·(code−mv−1), factor *= range, range =
//      2^nbits−1−mv).
//   2. `scripts/gen_fixtures.sh` asserts wgrib2 reports template 200 for every
//      record AND that `-checksum data` of the 5.200 file equals that of the SAME
//      field written with simple packing (`runlength_plain.grib2`) — i.e. two
//      INDEPENDENT decoders agree on all 231 values.
//   3. THIS FILE pins our decoder against the wgrib2 checksum constant, against
//      the simple-packed twin bit-for-bit, and positionally per grid node.
//
// The typed-rejection invariant stays intact: a corrupt run-length stream is a
// LOUD typed error, never garbage (test 3), exactly like the DRT 5.1 dictionary
// guard in packing_twins.rs.

use wx_ingest::grib2::decode_file;
use wx_ingest::grib2::decode_stream;
use wx_ingest::grib2::ParamFilter;
use wx_ingest::grib2::GribErr;
use wx_ingest::grib2::be_u32;
use wx_ingest::codecs::data_cksum_grid;
use std::fs::File;
use memmap2::Mmap;

// Fixture geometry (scripts/gen_grib2.py): regular lat/lon 21 x 11 = 231 points,
// laid out as 33 runs of 7 identical values.
const NPTS: usize = 231; // 33 runs x 7

// tools/wgrib2-full tests/fixtures/runlength_small.grib2 -checksum data
// (identical to runlength_plain.grib2's — asserted in gen_fixtures.sh)
const RL_CKSUM: u32 = 3596891385;

fn fixture_path(name: &str) -> String {
    return format!("../../tests/fixtures/{name}");
}

fn read_all_bytes(path: String) -> Vec<u8> {
    let f = File::open(&path);
    match f {
        Err(_) => { return Vec::<u8>::new(); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return Vec::<u8>::new(); }
                Ok(region) => {
                    let mut copy: Vec<u8> = Vec::<u8>::new();
                    for k in 0 .. region.len() {
                        copy.push(region[k]);
                    }
                    return copy;
                }
            }
        }
    }
}

#[test]
fn runlength_5200_matches_simple_packing_and_wgrib2_checksum() {
    let ra = decode_file(fixture_path("runlength_small.grib2"), ParamFilter::All);
    let pa = decode_file(fixture_path("runlength_plain.grib2"), ParamFilter::All);
    match ra {
        Err(e) => { println!("rle decode failed: {0:?}", e); assert!(false); }
        Ok((rl, _sr)) => {
            match pa {
                Err(e) => { println!("plain decode failed: {0:?}", e); assert!(false); }
                Ok((pl, _sp)) => {
                    assert!(rl.len() == 1);
                    assert!(pl.len() == 1);
                    let a = rl[0].data.clone();
                    let b = pl[0].data.clone();
                    assert!(a.len() == NPTS);
                    assert!(b.len() == NPTS);
                    // (a) element-wise identical f32 bits — compare bits so even
                    //     a signed-zero difference could not slip through.
                    for k in 0 .. NPTS {
                        assert!(a[k].to_bits() == b[k].to_bits());
                    }
                    // (b) both reproduce wgrib2's own checksum over the decoded
                    //     array (independent-decoder leg of the truth chain).
                    let g1 = rl[0].grid.clone();
                    let g2 = pl[0].grid.clone();
                    assert!(data_cksum_grid(a.clone(), &g1) == RL_CKSUM);
                    assert!(data_cksum_grid(b.clone(), &g2) == RL_CKSUM);
                    // (c) metadata comes from sections 0/4 only: the 5.200 section-5
                    //     layout has no reference value / binary scale at all.
                    assert!(rl[0].category == pl[0].category);
                    assert!(rl[0].number == pl[0].number);
                    assert!(rl[0].levels.len() == 1);
                    assert!(rl[0].levels[0].0 == pl[0].levels[0].0);
                    assert!(rl[0].levels[0].1 == pl[0].levels[0].1);
                    assert!(rl[0].fc_secs == pl[0].fc_secs);
                }
            }
        }
    }
}

#[test]
fn runlength_runs_land_on_the_right_grid_positions() {
    // Analytic pattern: value at scan index k is LEVELS[(k/7) % 3] with
    // LEVELS = [250.0, 256.0, 260.0], i.e. the level table read out of section 5
    // as ints 2500/2560/2600 with decimal scale 1. Every run boundary must fall
    // on a multiple of 7: a wrong digit order (base-`range` digits are LITTLE-
    // endian in that base) or an off-by-one in the repetition arithmetic shifts
    // values sideways instead of failing loudly.
    let r = decode_file(fixture_path("runlength_small.grib2"), ParamFilter::All);
    match r {
        Err(_e) => { assert!(false); }
        Ok((msgs, _st)) => {
            let d = msgs[0].data.clone();
            assert!(d.len() == NPTS);
            let lv: [f32; 3] = [250.0, 256.0, 260.0];
            for k in 0 .. NPTS {
                let expect = lv[((k / 7) % 3) as usize];
                assert!(d[k] == expect);
            }
            // explicit transition probes + last node (the final run ends exactly
            // at the grid edge: 33 runs x 7 = 231 points, no padding codewords)
            assert!(d[6] == 250.0);
            assert!(d[7] == 256.0);
            assert!(d[13] == 256.0);
            assert!(d[14] == 260.0);
            assert!(d[NPTS - 1] == lv[((NPTS - 1) / 7) % 3]);
        }
    }
}

#[test]
fn corrupt_runlength_stream_is_a_typed_error_not_garbage() {
    // Patch ONE byte of the section-7 bitstream to 0xFF: with nbits=4 and mv=3
    // the first codeword becomes 15 > mv, which is NOT a legal level index, so
    // the decoder must refuse rather than invent a value (same policy as the
    // DRT 5.1 dictionary guard: never a silent wrong number).
    let mut bytes = read_all_bytes(fixture_path("runlength_small.grib2"));
    assert!(bytes.len() > 32);
    // walk the section framing: sec0 is the 16-byte GRIB header, then per
    // section [u32 len-including-itself][sect#][payload...].
    let mut off = 16;
    let mut s7payload = -1;
    let mut guard = 0;
    while off + 5 <= bytes.len() && guard < 32 {
        guard += 1;
        let l = be_u32(&bytes, off);
        let sect = bytes[off + 4];
        if sect == 7 {
            s7payload = off as i64 + 5;
            break;
        }
        if l < 5 {
            break;
        }
        off += l as usize;
    }
    assert!(s7payload >= 0);
    bytes[s7payload as usize] = 0xFF;
    let r = decode_stream(&bytes, ParamFilter::All);
    match r {
        Ok((_m, _s)) => {
            println!("corrupt run-length stream was ACCEPTED (should be refused)");
            assert!(false);
        }
        Err(e) => {
            match e {
                GribErr::BadField(_why) => { return; }
                GribErr::Truncated => { return; }
                _ => {
                    println!("unexpected error kind: {0:?}", e);
                    assert!(false);
                }
            }
        }
    }
}
