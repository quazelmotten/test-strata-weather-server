// V1.4 phase C gate #5: the field-header integrity digest must be the SAME
// CRC-32 the rest of the stack already trusts, pinned against an independent
// implementation instead of self-certified.
//
// wx-core hand-rolls its table so it gains no dependency edge (zlib-rs lives
// one crate UP the tree: wx-ingest::codecs uses it for GRIB2 message bodies).
// The polynomial choice is load-bearing twice over:
//   * warm-boot compatibility: pre-phase-C stores wrote a ZERO reserved word;
//     readers must never mistake a foreign-but-valid digest for corruption.
//   * ops familiarity: this is the exact reflected IEEE CRC-32 of GRIB2 §7 /
//     zlib / Python `zlib.crc32` — NOT Castagnoli (CRC-32c), whose check value
//     for "123456789" differs. A future edit swapping tables would silently
//     invalidate every sealed header; the known-answer vectors below refuse it.
//
// `zlib_rs` is reachable here because package integration tests link the
// package's ordinary dependencies (wx-ingest depends on it for codecs).

use wx_core::storage::{BIN_CRC_ABSENT, BIN_CRC_OFFSET, BIN_FIXED_HEADER,
                       BIN_LEVEL_BYTES, BinHeader, HeaderError, StatMeta,
                       STAT_ACCUMULATION, crc32_fold, crc32_ieee, crc_slot_value,
                       decode_header, encode_header, header_crc, le_u32};

fn lcg_bytes(n: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::<u8>::with_capacity(n);
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut k = 0usize;
    while k < n {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.push(((s >> 33) & 0xFF) as u8);
        k += 1;
    }
    out
}

#[test]
fn ieee_helper_is_bit_identical_to_zlib_over_shaped_vectors() {
    // Sizes deliberately straddle the vectorized chunk boundaries zlib-rs
    // switches implementations at (word/SSE widths), plus zero and one-byte
    // inputs where init/final inversion is easiest to get wrong.
    let sizes: Vec<usize> = vec![0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32,
                                 33, 63, 64, 65, 127, 128, 129, 255, 256, 1000,
                                 4095, 4096, 4097, 65536, 65537];
    for s in &sizes {
        let d = lcg_bytes(*s);
        let mine = crc32_ieee(&d);
        let theirs = zlib_rs::crc32::crc32(0, &d);
        assert!(mine == theirs, "crc32_ieee != zlib crc32 at len {0}", s);
    }
    // Zero-length input MUST externalize to 0 exactly (the sentinel we reserve
    // for "unsealed" headers relies on digests being rare-zero, not always-zero).
    assert!(crc32_ieee(&[]) == 0 && zlib_rs::crc32::crc32(0, &[]) == 0);
}

#[test]
fn known_answers_pin_the_ieee_polynomial_not_castagnoli() {
    // Canonical CRC-32/ISO-HDLC ("zlib") check values.
    assert!(crc32_ieee(b"123456789") == 0xCBF4_3926);
    assert!(zlib_rs::crc32::crc32(0, b"123456789") == 0xCBF4_3926);
    assert!(crc32_ieee(b"a") == 0xE8B7_BE43);
    assert!(crc32_ieee(b"abc") == 0x3524_41C2);
    // Castagnoli (CRC-32c, iSCSI/SSE4.2) check value for the same input — the
    // near-miss a careless table swap would produce, refused BY NAME.
    assert!(crc32_ieee(b"123456789") != 0xE306_9729);
    // And GRIB2's own convention: the §7 message-body digest uses this exact
    // polynomial, which is what `codecs` verifies with zlib today. One CRC to
    // rule them all — no second polynomial anywhere in the tree.
}

#[test]
fn state_continuing_fold_matches_chained_zlib() {
    // `header_crc` skips the four sealed bytes at offset 52, so it folds TWO
    // discontinuous regions back-to-back. That only works if `crc32_fold`'s
    // INTERNAL-state contract composes exactly like zlib's EXTERNAL chaining.
    // Pin both directions of that equivalence over real splits.
    let data = lcg_bytes(4096);
    let splits: Vec<usize> = vec![0, 1, 4095, 4096, 52, 56, 2048, 3000];
    for s0 in &splits {
        if *s0 > data.len() {
            continue;
        }
        let (a, b) = data.split_at(*s0);
        // external chain via zlib semantics...
        let chained_zlib = zlib_rs::crc32::crc32(zlib_rs::crc32::crc32(0, a), b);
        // ...must equal the one-shot...
        let oneshot = crc32_ieee(&data);
        // ...and must equal MY fold continued across the seam: feed the
        // inverted-in register from stage A straight into stage B, invert once.
        let inner_a = crc32_fold(0xFFFF_FFFF, a);
        let external = crc32_fold(inner_a, b) ^ 0xFFFF_FFFF;
        assert!(chained_zlib == oneshot && external == oneshot,
                "fold composition diverges at split {0}", s0);
    }
}

fn header_variants() -> Vec<BinHeader> {
    let v = vec![
        BinHeader { nx: 1440, ny: 721, levels: vec![0.0], scan_ns: true,
                    winds_grid: false, stat: StatMeta::instantaneous() },
        BinHeader { nx: 1, ny: 1, levels: vec![850.0], scan_ns: false,
                    winds_grid: true, stat: StatMeta::instantaneous() },
        BinHeader { nx: 1799, ny: 1059, scan_ns: true, winds_grid: false,
                    levels: vec![1000.0, 850.0, 700.0, 500.0, 250.0],
                    stat: StatMeta { processed: true, kind: STAT_ACCUMULATION,
                                     begin_secs: 0, end_secs: 21600,
                                     step_secs: 3600 } },
        BinHeader { nx: 1799, ny: 1059, scan_ns: true, winds_grid: true,
                    levels: vec![2.0, 10.0], // ASCENDING non-isobaric order
                    stat: StatMeta::instantaneous() },
        BinHeader { nx: 384, ny: 192, scan_ns: false, winds_grid: false,
                    levels: vec![1000.0, 925.0],
                    stat: StatMeta { processed: true, kind: STAT_ACCUMULATION,
                                     begin_secs: 3600, end_secs: 3600,
                                     step_secs: 0 } },
    ];
    v
}

#[test]
fn header_crc_matches_chained_zlib_over_every_real_header_shape() {
    for h in &header_variants() {
        let head = encode_header(h);
        assert!(head.len() == BIN_FIXED_HEADER
                + h.levels.len() * BIN_LEVEL_BYTES);
        let raw = header_crc(&head);
        // The independent computation: zlib-chained over `[0,52)` then
        // `[56,len)` — the exact region definition of `header_crc`.
        let part_a = zlib_rs::crc32::crc32(0, &head[0 .. BIN_CRC_OFFSET]);
        let want = zlib_rs::crc32::crc32(part_a, &head[BIN_FIXED_HEADER ..]);
        assert!(raw == want, "header_crc region/polynomial diverged");
        // What actually ships in the file: the slot carries the sentinel-
        // adjusted value, and the four slot bytes are NOT part of the digest
        // (that is what makes self-sealing possible).
        let stored = le_u32(&head, BIN_CRC_OFFSET);
        assert!(stored == crc_slot_value(raw));
        assert!(stored != BIN_CRC_ABSENT, "a sealed header never stores zero");
        // And the round trip through the validating reader stays green: the
        // seal verifies against the very bytes encode wrote.
        match decode_header(&head) {
            Err(e) => { panic!("sealed header rejected by its own writer: {0:?}", e); }
            Ok(back) => {
                assert!(back.nx == h.nx && back.ny == h.ny);
                assert!(back.levels == h.levels);
                assert!(back.scan_ns == h.scan_ns && back.winds_grid == h.winds_grid);
                assert!(back.stat == h.stat);
            }
        }
    }
}

#[test]
fn slot_sentinel_and_tamper_detection_behave_as_the_format_promises() {
    // Sentinel table: raw digest 0 cannot be stored (0 means "unsealed"), so
    // it maps to 1; every other digest stores as itself.
    if BIN_CRC_ABSENT != 0 {
        panic!("the unsealed sentinel must stay zero: {0}", BIN_CRC_ABSENT);
    }
    assert!(crc_slot_value(0) == 1);
    assert!(crc_slot_value(1) == 1);
    assert!(crc_slot_value(2) == 2);
    assert!(crc_slot_value(0xFFFF_FFFF) == 0xFFFF_FFFF);

    let h = BinHeader { nx: 640, ny: 321, levels: vec![925.0, 850.0],
                        scan_ns: true, winds_grid: false,
                        stat: StatMeta::instantaneous() };
    let head = encode_header(&h);

    // Zeroing the slot demotes the file to pre-phase-C LEGACY: valid, read
    // with the existing warning path, NOT a BadCrc failure. This is the exact
    // shape a warm boot meets for stores written by current HEAD-minus-C.
    let mut legacy = head.clone();
    for b in legacy[BIN_CRC_OFFSET .. BIN_FIXED_HEADER].iter_mut() {
        *b = 0;
    }
    match decode_header(&legacy) {
        Err(e) => { panic!("zero-slot header refused instead of legacy: {0:?}", e); }
        Ok(back) => { assert!(back.nx == 640 && back.levels.len() == 2); }
    }

    // Bit rot ANYWHERE in the covered region is caught — sample one offset per
    // structural block: fixed geometry (nx mid-bytes), the stat time window,
    // and each level coordinate. All must come back as BadCrc, not as a
    // plausible but wrong decoded header. (Offsets deliberately dodge the two
    // flag bytes and the three must-be-zero reserved bytes, which the reader
    // validates structurally BEFORE consulting the digest.)
    let base_len = BIN_FIXED_HEADER + 2 * BIN_LEVEL_BYTES;
    let probes: Vec<usize> = vec![13, 40, 56, base_len - 4];
    for p in &probes {
        let mut rot = head.clone();
        rot[*p] ^= 0x40;
        match decode_header(&rot) {
            Ok(back) => { panic!("covered byte {0} rotted unnoticed: {1:?}", p, back); }
            Err(e) => {
                match e {
                    HeaderError::BadCrc => {}
                    _ => { panic!("rot at {0} reported as {1:?}", p, e); }
                }
            }
        }
    }
}
