// V1.4 phase C gate #4: the OPTIONAL grid.bin integrity trailer.
//
// grid.bin carries the projection constants every query's coordinates come
// from: ONE silently flipped byte shifts an entire LCC run's points by tens of
// kilometres and NOTHING else in the tree notices (the field .bin files are all
// individually consistent with each other). Pre-phase-C the warm boot trusted
// the blob verbatim. Now `encode_grid` seals it with a CRC-32 (same reflected
// IEEE polynomial as the GRIB2/§7 digest the codecs already verify, same
// helper, wx_core::storage::crc32_ieee) and `decode_grid` refuses a sealed
// blob whose digest disagrees — routing into the pipeline's EXISTING loud
// grid-recovery (note + error counter + delete run dir + re-decode from
// source).
//
// Warm-boot compatibility (the phase-C hard requirement): stores written by
// pre-phase-C binaries carry grid.bin WITHOUT the trailer. Presence is decided
// by EXACT length discrimination (base vs base+4), so old blobs keep decoding
// as valid-with-warning (legacy counter), new blobs decode verified, and a
// pre-phase-C READER pointed at a NEW blob tolerates the trailing 4 bytes
// because its truncation guards were always `<` checks — bidirectionally
// compatible without touching BIN/GRID_VERSION.

// Matching pedantic-but-cosmetic lints are allowed per-file, narrowly listed
// (same convention as the src modules and the phase B bounds gates).
#![allow(clippy::needless_return)]

use wx_core::grid::{GridDef, GridGaussian, GridLambert, GridRegular};
use wx_core::storage::{crc32_ieee, crc_counters_reset, crc_counters_snapshot};
use wx_ingest::store::{GRID_CRC_BYTES, decode_grid, encode_grid, le_u32_at};

// The whole binary shares the process-wide CRC counters, and these tests make
// EXACT delta assertions against them, so they run one at a time.
static CRC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn reg() -> GridDef {
    return GridDef::Regular(GridRegular {
        nx: 1440, ny: 721, lon1: 0.0, lat1: 90.0, dlon: 0.25, dlat: 0.25,
        scan_mode: 0x00, winds_grid: false,
    });
}

/// Real HRRR-conus 3 km LCC parameters (as decoded from section 3), scanning
/// NORTHWARD from the south-west corner La1 (template 3.30 scan bit 0x40).
fn lam() -> GridDef {
    return GridDef::Lambert(GridLambert::new(
        1799, 1059, 21.138, 233.442, 265.0, 38.5, 38.5, 38.5, 3000.0, 3000.0,
        1, 0x40, 6, true));
}

fn gau() -> GridDef {
    return GridDef::Gaussian(GridGaussian::new(
        384, 192, 0.0, 90.0, -90.0, 360.0 / 384.0, 0, 6, false));
}

/// Field-by-field equality per kind (no PartialEq derive exists on the grid
/// types, and derived Debug would only compare formatted floats).
fn same_grid(a: &GridDef, b: &GridDef) {
    match (a, b) {
        (GridDef::Regular(x), GridDef::Regular(y)) => {
            assert!(x.nx == y.nx && x.ny == y.ny);
            assert!(x.lon1 == y.lon1 && x.lat1 == y.lat1);
            assert!(x.dlon == y.dlon && x.dlat == y.dlat);
            assert!(x.scan_mode == y.scan_mode && x.winds_grid == y.winds_grid);
        }
        (GridDef::Lambert(x), GridDef::Lambert(y)) => {
            assert!(x.nx == y.nx && x.ny == y.ny);
            assert!(x.la1 == y.la1 && x.lo1 == y.lo1 && x.lov == y.lov);
            assert!(x.lad == y.lad && x.latin1 == y.latin1 && x.latin2 == y.latin2);
            assert!(x.dx_m == y.dx_m && x.dy_m == y.dy_m);
            assert!(x.proj_flag == y.proj_flag && x.scan_mode == y.scan_mode);
            assert!(x.earth_shape == y.earth_shape && x.winds_grid == y.winds_grid);
            assert!(x.inline_axes == y.inline_axes);
            assert!(x.ua_major_m == y.ua_major_m && x.ua_minor_m == y.ua_minor_m);
            // The whole point of sealing: the rebuilt projection answers
            // identically at a probe point.
            let pa = x.project(39.0, -98.0);
            let pb = y.project(39.0, -98.0);
            assert!(pa.is_some() && pb.is_some());
            assert!(pa.unwrap().0 == pb.unwrap().0 && pa.unwrap().1 == pb.unwrap().1);
        }
        (GridDef::Gaussian(x), GridDef::Gaussian(y)) => {
            assert!(x.nx == y.nx && x.ny == y.ny);
            assert!(x.lon1 == y.lon1 && x.la1 == y.la1 && x.la2 == y.la2);
            assert!(x.dlon == y.dlon && x.scan_mode == y.scan_mode);
            assert!(x.earth_shape == y.earth_shape && x.winds_grid == y.winds_grid);
            assert!(x.row_lats() == y.row_lats()); // recomputed bit-identical
        }
        _ => { panic!("grid kind changed across round trip"); }
    }
}

#[test]
fn sealed_grid_blobs_round_trip_and_seal_the_last_four_bytes() {
    let _l = CRC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crc_counters_reset();
    if GRID_CRC_BYTES != 4 {
        panic!("trailer width contract moved: GRID_CRC_BYTES = {0}", GRID_CRC_BYTES);
    }
    let kinds: Vec<GridDef> = vec![reg(), lam(), gau()];
    for g in &kinds {
        let blob = encode_grid(g);
        let back = decode_grid(&blob);
        match back {
            Err(e) => { panic!("sealed blob refused: {0}", e); }
            Ok(d) => { same_grid(g, &d); }
        }
        // The trailer is EXACTLY the last four bytes, little-endian, over every
        // preceding byte — pinned independently of any base-length constant so
        // a layout change that MOVES the digest while keeping the total length
        // cannot slip through.
        let n = blob.len();
        let want = le_u32_at(&blob, n - GRID_CRC_BYTES);
        assert!(want == crc32_ieee(&blob[..n - GRID_CRC_BYTES]),
                "trailer must be crc32_ieee over the payload");
    }
    // Every read above was a SEALED hit: three verified, zero legacy.
    let c = crc_counters_snapshot();
    assert!(c.verified == 3 && c.legacy == 0);
}

#[test]
fn stripping_the_trailer_decodes_as_legacy_which_is_the_warm_boot_contract() {
    let _l = CRC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crc_counters_reset();
    let blob = encode_grid(&reg());
    // A grid.bin exactly as a PRE-phase-C binary wrote it: the same bytes minus
    // the digest. Length discrimination alone identifies it; it MUST still
    // decode (restart gates stay green unmodified) and must be COUNTED so ops
    // can see how much of the store predates sealing.
    let legacy = &blob[..blob.len() - GRID_CRC_BYTES];
    let back = decode_grid(legacy);
    match back {
        Err(e) => { panic!("pre-phase-C shaped grid.bin rejected: {0}", e); }
        Ok(d) => { same_grid(&reg(), &d); }
    }
    let c = crc_counters_snapshot();
    assert!(c.legacy == 1 && c.verified == 0);
    // And the blind spot this leaves is honest: without the digest even a
    // flipped bit still decodes (see the sealed twin below). That is why the
    // writer seals and the reader counts.
    let mut rot = legacy.to_vec();
    rot[21] ^= 0x08; // lon1 low mantissa bit
    let back2 = decode_grid(&rot);
    match back2 {
        Err(e) => { panic!("legacy blob wrongly refused: {0}", e); }
        Ok(_d) => {}
    }
    assert!(crc_counters_snapshot().legacy == 2);
}
#[test]
fn a_single_flipped_byte_in_a_sealed_blob_is_refused_as_integrity() {
    let _l = CRC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crc_counters_reset();
    // One bit anywhere in the payload (here: the lat1 mantissa of a regular
    // grid) MUST become a named integrity error, NOT a subtly different grid.
    // This is the exact failure mode pre-phase-C could not see at all.
    let seeds: Vec<(GridDef, usize)> = vec![
        (reg(), 29),  // lat1 low byte
        (lam(), 69),  // dx_m low byte: a 3 km -> 3.0000000001 km shift nobody notices
        (gau(), 45),  // dlon low byte
    ];
    for item in &seeds {
        let mut blob = encode_grid(&item.0);
        blob[item.1] ^= 0x01;
        let back = decode_grid(&blob);
        match back {
            Ok(_d) => { panic!("flipped byte at offset {0} sailed through", item.1); }
            Err(e) => {
                assert!(e.contains("integrity"), "message must name integrity: {0}", e);
                assert!(e.contains("header crc mismatch"),
                        "message must carry the shared crc-mismatch tag: {0}", e);
            }
        }
    }
    // Corruption of the DIGEST ITSELF is caught too (the payload is untouched,
    // so a naive reader might trust the bytes and ignore the word).
    let mut blob = encode_grid(&lam());
    let n = blob.len();
    blob[n - 1] ^= 0x80;
    match decode_grid(&blob) {
        Ok(_d) => { panic!("corrupt trailer accepted"); }
        Err(e) => { assert!(e.contains("integrity")); }
    }
    // None of those refusals may count as verified or legacy reads.
    let c = crc_counters_snapshot();
    assert!(c.verified == 0 && c.legacy == 0);
}

#[test]
fn truncated_blobs_fail_closed_with_named_truncation_errors() {
    let _l = CRC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // base+4 shapes per kind; cutting INTO the payload must never be mistaken
    // for "legacy length" or read past the end.
    let cases: Vec<(GridDef, &str)> = vec![
        (reg(), "truncated regular grid"),
        (lam(), "truncated lambert grid"),
        (gau(), "truncated gaussian grid"),
    ];
    for item in &cases {
        let blob = encode_grid(&item.0);
        let base = blob.len() - GRID_CRC_BYTES;
        // one byte short of the payload: falls BELOW the legacy length
        let cut = &blob[..base - 1];
        match decode_grid(cut) {
            Ok(_d) => { panic!("short blob decoded"); }
            Err(e) => { assert!(e.contains(item.1), "expected '{0}', got '{1}'", item.1, e); }
        }
        // payload complete but only PART of the trailer present: neither exact
        // legacy nor exact sealed length -> tolerated trailing-bytes shape? NO:
        // base+1..base+3 is SHORTER than sealed, longer than base — the guards
        // are `<`, so this decodes UNVERIFIED. Pinned here so the documented
        // three-shape rule stays exactly three shapes.
        let ragged = &blob[..base + 2];
        match decode_grid(ragged) {
            Ok(d) => { same_grid(&item.0, &d); }
            Err(e) => { panic!("ragged-trailer blob refused: {0}", e); }
        }
    }
    // Below even the magic block: the generic truncation error.
    let tiny = encode_grid(&reg());
    match decode_grid(&tiny[..8]) {
        Ok(_d) => { panic!("8-byte blob decoded"); }
        Err(e) => { assert!(e.contains("truncated grid.bin")); }
    }
}

#[test]
fn longer_than_sealed_is_tolerated_unverified_as_documented() {
    let _l = CRC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crc_counters_reset();
    // Historical reader behavior: trailing bytes beyond the payload were always
    // ignored (guards are `<` checks). A NEW writer plus an OLD reader is that
    // shape (old reader sees base+4 as "trailing bytes") — compatibility in the
    // other direction rests on exactly this tolerance, so pin it.
    let mut blob = encode_grid(&reg());
    blob.extend_from_slice(&[0xA5, 0xA5, 0xA5]);
    match decode_grid(&blob) {
        Err(e) => { panic!("extra trailing bytes refused: {0}", e); }
        Ok(d) => { same_grid(&reg(), &d); }
    }
    let c = crc_counters_snapshot();
    assert!(c.verified == 0 && c.legacy == 0, "unverified shape must not bump either counter");
}

#[test]
fn hostile_lambert_blobs_are_rejected_by_name_instead_of_panicking() {
    let _l = CRC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The audited degeneracy: La1 exactly on the cone pole. `new()` poisons,
    // `encode_grid` still serializes the INPUTS faithfully, and `decode_grid`
    // rebuilds through `try_new` and REFUSES with the typed reason.
    let pole = GridDef::Lambert(GridLambert::new(
        61, 41, 90.0, 233.44, 229.53, 26.5, 26.5, 35.0, 3000.0, 3000.0,
        1, 0, 6, false));
    let blob = encode_grid(&pole);
    match decode_grid(&blob) {
        Ok(_d) => { panic!("pole-anchored lambert blob decoded"); }
        Err(e) => {
            assert!(e.starts_with("lambert grid rejected"), "got: {0}", e);
            assert!(e.contains("cone pole"), "typed anchor reason missing: {0}", e);
            // The recovery note embeds the geometry so operators can tell
            // WHICH run dir and inputs rotted.
            assert!(e.contains("la1=90"), "inputs must be named: {0}", e);
        }
    }
    // Non-finite and zero-cell-size blobs map to the other typed variant.
    let nan = GridDef::Lambert(GridLambert::new(
        61, 41, f64::NAN, 233.44, 229.53, 26.5, 26.5, 35.0, 3000.0, 3000.0,
        1, 0, 6, false));
    match decode_grid(&encode_grid(&nan)) {
        Ok(_d) => { panic!("NaN-la1 lambert blob decoded"); }
        Err(e) => { assert!(e.starts_with("lambert grid rejected"), "got: {0}", e); }
    }
    let zerodx = GridDef::Lambert(GridLambert::new(
        61, 41, 21.0, 233.44, 229.53, 26.5, 26.5, 35.0, 0.0, 3000.0,
        1, 0, 6, false));
    match decode_grid(&encode_grid(&zerodx)) {
        Ok(_d) => { panic!("dx=0 lambert blob decoded"); }
        Err(e) => { assert!(e.starts_with("lambert grid rejected"), "got: {0}", e); }
    }
    // Inline user-defined axes that leave no finite projection are refused too
    // (set_user_axes poisons; decode re-checks anchors_ok after applying them).
    let mut axes = GridLambert::new(61, 41, 21.0, 233.44, 229.53, 26.5, 26.5,
                                    35.0, 3000.0, 3000.0, 1, 0, 6, false);
    axes.inline_axes = true;
    axes.ua_major_m = f64::INFINITY;
    axes.ua_minor_m = f64::INFINITY;
    let axes_blob = encode_grid(&GridDef::Lambert(axes));
    match decode_grid(&axes_blob) {
        Ok(_d) => { panic!("infinite inline-axes lambert blob decoded"); }
        Err(e) => {
            assert!(e.starts_with("lambert grid rejected"), "got: {0}", e);
        }
    }
}
