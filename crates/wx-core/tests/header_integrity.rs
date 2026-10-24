// V1.4 phase C gate #2: field-header INTEGRITY and structural sanity, all of it
// executed at the once-per-file open (`open_field`), never on the query hot
// path (spec §5 keeps the hot path lock-free/alloc-free; risk register #9 makes
// the header the thing to defend).
//
// What this pins, and why each half matters:
//   * the CRC-32 written by `encode_header` catches rot ANYWHERE in the header
//     except its own four bytes — including the level coordinates (a wrong
//     level silently interpolates between the wrong pressures) and the PDT 4.8
//     stat window (a flipped byte there would start time-interpolating an
//     accumulation, spec §6.3 violation);
//   * a ZERO integrity slot means "written before phase C" and must stay valid,
//     counted as legacy: stores written by the previous HEAD warm-boot into THIS
//     build with no re-ingest, which is why BIN_VERSION stayed 2 (the pipeline's
//     version trap deletes foreign-version run dirs);
//   * `nx >= 1`, `ny >= 1`, finite strictly-monotonic levels, overflow-safe
//     sizing and EXACT length agreement turn the remaining corrupt-header
//     shapes into loud skips instead of mis-sliced planes or a panic inside
//     `MmapField::value()` under `panic = "abort"`.

// Matching pedantic-but-cosmetic lints are allowed per-file, narrowly listed
// (same convention as the src modules and the phase B bounds gates).
#![allow(clippy::needless_return)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::single_match)]

use wx_core::storage::{BinHeader, HeaderError, StatMeta, BIN_CRC_OFFSET, BIN_FIXED_HEADER,
                       crc32_ieee, crc_counters_reset, crc_counters_snapshot, crc_slot_value,
                       decode_header, encode_header, expected_size, expected_size_checked,
                       header_crc, open_field, write_bin_atomic};
use std::fs::File;
use std::io::Write;

const DIR: &str = "/tmp/opencode/wxpt-header-integrity";

fn hdr(nx: u32, ny: u32, levels: &[f32]) -> BinHeader {
    return BinHeader {
        nx: nx,
        ny: ny,
        levels: levels.to_vec(),
        scan_ns: true,
        winds_grid: false,
        stat: StatMeta {
            processed: true,
            kind: wx_core::storage::STAT_ACCUMULATION,
            begin_secs: 0,
            end_secs: 21600,
            step_secs: 0,
        },
    };
}

/// 3x2 planes of distinct bit patterns so a mis-sliced read is visible.
fn planes(nx: usize, ny: usize, nlevels: usize) -> Vec<Vec<f32>> {
    let mut out: Vec<Vec<f32>> = Vec::new();
    for li in 0 .. nlevels {
        let mut p: Vec<f32> = Vec::new();
        for j in 0 .. ny {
            for i in 0 .. nx {
                p.push(((li * 100_000) + (j * 1000) + i) as f32);
            }
        }
        out.push(p);
    }
    return out;
}

#[allow(clippy::needless_return)]
fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!("create {0}", path); }
        Ok(mut w) => {
            if w.write_all(bytes).is_err() { panic!("write {0}", path); }
        }
    }
}

#[allow(clippy::needless_return)]
fn read_bytes(path: &str) -> Vec<u8> {
    match std::fs::read(path) {
        Err(_e) => { panic!("read {0}", path); }
        Ok(v) => { return v; }
    }
}

/// Write a well-formed 3x2, 2-level file and return (path, bytes).
fn write_good_file(name: &str) -> (String, Vec<u8>) {
    let _c = std::fs::create_dir_all(DIR);
    let h = hdr(3, 2, &[1000.0, 850.0]);
    let pl = planes(3, 2, 2);
    let refs: Vec<&[f32]> = [&pl[0][..], &pl[1][..]].to_vec();
    let path = format!("{0}/{1}", DIR, name);
    let w = write_bin_atomic(path.to_string(), &h, &refs);
    match w {
        Err(e) => { println!("write failed: {0}", e); panic!(); }
        Ok(_) => {}
    }
    let bytes = read_bytes(&path);
    return (path, bytes);
}

#[test]
fn sealed_headers_round_trip_and_are_counted_as_verified() {
    crc_counters_reset();
    let (path, bytes) = write_good_file("sealed.bin");
    let c0 = crc_counters_snapshot();
    let o = open_field(path.to_string());
    match o {
        Err(e) => { println!("sealed field refused: {0}", e); panic!(); }
        Ok(mf) => {
            assert!(mf.header.nx == 3 && mf.header.ny == 2);
            assert!(mf.header.levels.len() == 2 && mf.header.levels[1] == 850.0);
            assert!(mf.header.stat.processed && mf.header.stat.end_secs == 21600);
            assert!(mf.value(1, 1, 2) == 101002.0); // plane/level addressing intact
        }
    }
    let c1 = crc_counters_snapshot();
    assert!(c1.verified > c0.verified);
    // The digest that was stored is the documented region, and it is non-zero
    // (zero would read as "unsealed").
    let head_end = BIN_FIXED_HEADER + 2 * 4;
    let want = crc_slot_value(header_crc(&bytes[0 .. head_end]));
    let got = (bytes[BIN_CRC_OFFSET] as u32) | ((bytes[BIN_CRC_OFFSET + 1] as u32) << 8)
            | ((bytes[BIN_CRC_OFFSET + 2] as u32) << 16) | ((bytes[BIN_CRC_OFFSET + 3] as u32) << 24);
    assert!(got == want && got != 0);
}

#[test]
fn rot_anywhere_in_the_header_is_a_loud_field_skip() {
    let (_p0, good) = write_good_file("rot-base.bin");
    // (offset, what lives there): every one of these is a SILENT wrong answer
    // if trusted, and none of them changes the file length except the nx/ny
    // pair, which the CRC still names first.
    let sites: [(usize, &str); 6] = [
        (12, "nx low byte"),
        (17, "ny high byte"),
        (22, "scan flag bit"),
        (23, "stat_kind byte"),
        (40, "stat window begin_secs mid byte"),
        (58, "first level coordinate high byte"),
    ];
    for k in 0 .. sites.len() {
        let mut b = good.to_vec();
        let off = sites[k].0;
        b[off] ^= 0xFF;
        let path = format!("{0}/rot-{1}.bin", DIR, k);
        write_bytes(&path, &b[..]);
        let o = open_field(path.to_string());
        match o {
            Ok(mf) => {
                println!("{0}: rotted header ACCEPTED (value {1})",
                         sites[k].1, mf.value(0, 0, 0));
                panic!();
            }
            Err(e) => {
                // Distinct, class-naming strings (the pipeline notes them and
                // advances the ingest-error counter, then serves the rest).
                let ok = e == "header crc mismatch" || e == "bad header"
                      || e == "bad header geometry" || e == "bad header level coords"
                      || e == "truncated data" || e == "field length disagrees with header";
                if !ok {
                    println!("{0}: unexpected refusal reason {1}", sites[k].1, e);
                    panic!();
                }
                if k == 4 {
                    // The stat-window byte has NO structural consequence (the
                    // value stays in range), so ONLY the digest can see it.
                    assert!(e == "header crc mismatch");
                } else if k == 5 {
                    // A rotted level coordinate may trip the finiteness /
                    // monotonicity guard before the digest does; either refusal
                    // is correct, both are loud, neither answers wrong.
                    assert!(e == "header crc mismatch" || e == "bad header level coords");
                }
            }
        }
    }
}

#[test]
fn legacy_unsealed_header_still_warms_boot_and_is_counted() {
    let (_p0, good) = write_good_file("legacy-base.bin");
    let mut b = good.to_vec();
    b[BIN_CRC_OFFSET] = 0; b[BIN_CRC_OFFSET + 1] = 0;
    b[BIN_CRC_OFFSET + 2] = 0; b[BIN_CRC_OFFSET + 3] = 0;
    let path = format!("{0}/legacy.bin", DIR);
    write_bytes(&path, &b[..]);

    crc_counters_reset();
    let o = open_field(path.to_string());
    match o {
        Err(e) => { println!("pre-phase-C store refused at warm boot: {0}", e); panic!(); }
        Ok(mf) => {
            // Every value still answers: compatibility means the DATA works,
            // not merely that the file opens.
            assert!(mf.value(1, 1, 2) == 101002.0);
            assert!(mf.header.stat.guards_time()); // accumulation guard survives
        }
    }
    let c = crc_counters_snapshot();
    assert!(c.legacy >= 1 && c.verified == 0);
}

#[test]
fn zero_digest_cannot_masquerade_as_the_absent_sentinel_forever() {
    // `crc_slot_value` maps a raw digest of 0 onto 1 so a sealed header NEVER
    // stores the sentinel. Verification applies the same mapping, so both a
    // true-0 and a true-1 digest verify against slot value 1 — the residual
    // gap is at most one missed DETECTION for one header in 2^32, never a wrong
    // ANSWER. Pin the mapping and the decode consequence.
    assert!(crc_slot_value(0) == 1);
    assert!(crc_slot_value(1) == 1);
    assert!(crc_slot_value(0xDEAD_BEEF) == 0xDEAD_BEEF);
    let h = hdr(3, 2, &[1000.0, 850.0]);
    let head = encode_header(&h);
    assert!(head[BIN_CRC_OFFSET] != 0 || head[BIN_CRC_OFFSET + 1] != 0
            || head[BIN_CRC_OFFSET + 2] != 0 || head[BIN_CRC_OFFSET + 3] != 0);
    // And the crate's own digest agrees with the standard zlib construction over
    // the same concatenated region (wx-ingest pins it against `zlib_rs` too).
    let region_len = BIN_FIXED_HEADER + 2 * 4;
    let mut region: Vec<u8> = Vec::new();
    region.extend_from_slice(&head[0 .. BIN_CRC_OFFSET]);
    region.extend_from_slice(&head[BIN_FIXED_HEADER .. region_len]);
    assert!(header_crc(&head[0 .. region_len]) == crc32_ieee(&region[..]));
}

#[test]
fn structurally_impossible_headers_are_refused_before_any_plane_is_read() {
    let h = hdr(3, 2, &[1000.0, 850.0]);
    let base = encode_header(&h);

    // nx = 0 / ny = 0: every plane offset is a product of these.
    let mut b = base.to_vec();
    b[12] = 0;
    match decode_header(&b) {
        Err(HeaderError::BadGeometry) => {}
        Err(e) => { println!("nx=0 reported {0:?}", e); panic!(); }
        Ok(_) => { panic!("nx=0 accepted"); }
    }
    let mut b2 = base.to_vec();
    b2[16] = 0; b2[17] = 0; b2[18] = 0; b2[19] = 0;
    match decode_header(&b2) {
        Err(HeaderError::BadGeometry) => {}
        Err(e) => { println!("ny=0 reported {0:?}", e); panic!(); }
        Ok(_) => { panic!("ny=0 accepted"); }
    }
    // nlevels = 0: nothing to answer, and data_off would land on the fixed
    // header itself.
    let mut b3 = base.to_vec();
    b3[20] = 0; b3[21] = 0;
    match decode_header(&b3) {
        Err(HeaderError::BadLevels) => {}
        Err(e) => { println!("nlevels=0 reported {0:?}", e); panic!(); }
        Ok(_) => { panic!("nlevels=0 accepted"); }
    }
    // Level block: duplicate, non-monotonic and NaN entries all refuse. With
    // THREE coordinates the monotonic direction becomes observable (a two-level
    // list is trivially monotonic in one direction or the other, and BOTH
    // directions are legitimate: descending for isobaric, ascending for e.g.
    // height_above_ground — the level TYPE lives in the directory name, not the
    // header, so a globally reversed pair must stay accepted).
    let h3 = hdr(3, 2, &[1000.0, 850.0, 500.0]);
    let base3 = encode_header(&h3);
    let mk = |lv: [f32; 3]| -> Vec<u8> {
        let mut v = base3.to_vec();
        let mut slot = 0usize;
        while slot < 3 {
            let bits = lv[slot].to_bits();
            let off = BIN_FIXED_HEADER + slot * 4;
            v[off] = (bits & 0xFF) as u8;
            v[off + 1] = ((bits >> 8) & 0xFF) as u8;
            v[off + 2] = ((bits >> 16) & 0xFF) as u8;
            v[off + 3] = ((bits >> 24) & 0xFF) as u8;
            slot += 1;
        }
        return v;
    };
    let probes: [(f32, f32, f32); 4] = [
        (850.0, 850.0, 500.0),      // duplicate coordinate
        (1000.0, 500.0, 850.0),     // not monotonic: bracket search would lie
        (f32::NAN, 850.0, 500.0),   // NaN coordinate
        (1000.0, 850.0, f32::INFINITY),
    ];
    for k in 0 .. probes.len() {
        let bb = mk([probes[k].0, probes[k].1, probes[k].2]);
        match decode_header(&bb) {
            Err(HeaderError::BadLevels) => {}
            Err(e) => { println!("levels {0}/{1}/{2} reported {3:?}",
                                 probes[k].0, probes[k].1, probes[k].2, e); panic!(); }
            Ok(_) => { println!("levels {0}/{1}/{2} accepted",
                               probes[k].0, probes[k].1, probes[k].2); panic!(); }
        }
    }
}

#[test]
fn absurd_geometry_cannot_wrap_the_size_check_into_a_panic() {
    // u32::MAX x u32::MAX x 1 level x 4 bytes overflows usize. Before phase C
    // the product WRAPPED to a small number, the length check passed, and the
    // first query panicked inside `value()`'s mmap slice range — under
    // `panic = "abort"` that is the daemon dying on eight bad bytes.
    let huge = BinHeader {
        nx: 0xFFFF_FFFF,
        ny: 0xFFFF_FFFF,
        levels: [1000.0].to_vec(),
        scan_ns: true,
        winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
    assert!(expected_size_checked(&huge).is_none());
    assert!(expected_size(&huge) == usize::MAX); // saturating, never wrapping
    // A real file shaped like that header (sealed header + a token amount of
    // data) must be refused with a NAMED reason, not accepted.
    let head = encode_header(&huge);
    let _c = std::fs::create_dir_all(DIR);
    let path = format!("{0}/absurd.bin", DIR);
    let mut bytes = head.to_vec();
    for k in 0 .. 64 {
        bytes.push(k as u8);
    }
    write_bytes(&path, &bytes[..]);
    match open_field(path.to_string()) {
        Ok(_mf) => { panic!("absurd geometry was indexed"); }
        Err(e) => { assert!(e == "header geometry out of range"); }
    }
    // Sane sizing arithmetic still agrees with the writer byte-for-byte.
    let sane = hdr(3, 2, &[1000.0, 850.0]);
    assert!(expected_size_checked(&sane) == Some(BIN_FIXED_HEADER + 8 + 2 * 6 * 4));
}

#[test]
fn trailing_slack_now_disagrees_instead_of_passing_silently() {
    // Pre-phase-C the length rule was only `len < want`, so a file holding MORE
    // than its header accounts for passed. Exact agreement is what ties the
    // declared geometry to the bytes actually written (and it is what makes the
    // swapped-extent case loud even when the CRC slot happened to be legacy).
    let (_p, good) = write_good_file("slack-base.bin");
    let mut b = good.to_vec();
    for k in 0 .. 4 {
        b.push(0xA5 + k as u8);
    }
    let path = format!("{0}/slack.bin", DIR);
    write_bytes(&path, &b[..]);
    match open_field(path.to_string()) {
        Ok(_mf) => { panic!("file longer than its header accounted for"); }
        Err(e) => { assert!(e == "field length disagrees with header"); }
    }
    // And shortening is still the classic truncated-data case.
    let short = good[0 .. good.len() - 8].to_vec();
    let path2 = format!("{0}/short.bin", DIR);
    write_bytes(&path2, &short[..]);
    match open_field(path2.to_string()) {
        Ok(_mf) => { panic!("truncated field was indexed"); }
        Err(e) => { assert!(e == "truncated data" || e == "header crc mismatch"); }
    }
}
