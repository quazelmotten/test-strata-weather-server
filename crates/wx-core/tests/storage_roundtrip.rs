// Storage format round-trip tests (spec §4.5): atomic write (temp+fsync+
// rename), header validation, mmap read, scan-flag accessor, rejection of
// corrupt/truncated files.

use wx_core::storage::write_bin_atomic;
use wx_core::storage::open_field;
use wx_core::storage::encode_header;
use wx_core::storage::decode_header;
use wx_core::storage::expected_size;
use wx_core::storage::{BinHeader, StatMeta, STAT_ACCUMULATION, HeaderError, BIN_VERSION};
use wx_core::storage::BIN_FIXED_HEADER;
use std::io::Write;

fn v1(a: f32) -> Vec<f32> {
    vec![a]
}

fn v2(a: f32, b: f32) -> Vec<f32> {
    vec![a, b]
}

fn v3(a: f32, b: f32, c: f32) -> Vec<f32> {
    vec![a, b, c]
}

#[test]
fn roundtrip_write_read() {
    let hdr = BinHeader {
        nx: 4, ny: 3, levels: v2(1000.0, 925.0),
        scan_ns: true, winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
    let mut p0: Vec<f32> = Vec::<f32>::new();
    let mut p1: Vec<f32> = Vec::<f32>::new();
    for k in 0 .. 12 {
        p0.push(100.0 + k as f32);
        p1.push(200.0 + k as f32);
    }
    let path = "/tmp/opencode/wxpt_rt.bin".to_string();
    let path2 = path.to_string();
    let w = write_bin_atomic(path, &hdr, &[&p0[0 .. 12], &p1[0 .. 12]]);
    if w.is_err() { panic!(); }
    let mf = open_field(path2);
    match mf {
        Err(_) => { panic!(); }
        Ok(mfield) => {
            assert!(mfield.header.nx == 4);
            assert!(mfield.header.ny == 3);
            assert!(mfield.header.levels.len() == 2);
            assert!(mfield.header.levels[0] == 1000.0);
            assert!(mfield.header.levels[1] == 925.0);
            assert!(mfield.header.scan_ns);
            assert!(!mfield.header.winds_grid);
            assert!(mfield.data_off == BIN_FIXED_HEADER + 2 * 4);
            assert!(mfield.value(0, 0, 0) == 100.0);
            assert!(mfield.value(0, 2, 3) == 111.0);
            assert!(mfield.value(1, 0, 0) == 200.0);
            assert!(mfield.value(1, 2, 3) == 211.0);
            assert!(mfield.stored_row(2) == 2);
        }
    }
}

#[test]
fn header_roundtrip_codec() {
    let hdr = BinHeader {
        nx: 1440, ny: 721, levels: v3(1000.0, 925.0, 850.0),
        scan_ns: true, winds_grid: true,
        stat: StatMeta::instantaneous(),
    };
    let bytes = encode_header(&hdr);
    assert!(bytes.len() == BIN_FIXED_HEADER + 3 * 4);
    let back = decode_header(&bytes);
    match back {
        Err(_) => { panic!(); }
        Ok(h) => {
            assert!(h.nx == 1440 && h.ny == 721);
            assert!(h.levels.len() == 3 && h.levels[2] == 850.0);
            assert!(h.scan_ns && h.winds_grid);
        }
    }
    assert!(expected_size(&hdr) == BIN_FIXED_HEADER + 3 * 4 + 3 * 1440 * 721 * 4);
}

#[test]
fn reject_bad_magic() {
    let hdr = BinHeader {
        nx: 2, ny: 2, levels: v1(1000.0),
        scan_ns: true, winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
    let bytes = encode_header(&hdr);
    let path = "/tmp/opencode/wxpt_badmagic.bin".to_string();
    let f = std::fs::File::create(&path);
    match f {
        Err(_) => { panic!(); }
        Ok(mut h) => {
            let mut bad: Vec<u8> = bytes.to_vec();
            bad[0] = 88; // 'X' != 'W': magic mismatch
            let w = h.write_all(&bad);
            if w.is_err() {
                panic!();
            }
        }
    }
    let mf = open_field(path);
    if mf.is_ok() { panic!(); }
}

#[test]
fn reject_truncated_data() {
    let hdr = BinHeader {
        nx: 4, ny: 4, levels: v2(1000.0, 500.0),
        scan_ns: true, winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
    let head = encode_header(&hdr);
    let path = "/tmp/opencode/wxpt_trunc.bin".to_string();
    let f = std::fs::File::create(&path);
    match f {
        Err(_) => { panic!(); }
        Ok(mut h) => {
            // header + only ONE plane of data (two declared) -> truncated
            let w = h.write_all(&head);
            if w.is_err() {
                panic!();
            }
            let mut zeros: Vec<u8> = Vec::<u8>::new();
            for _k in 0 .. 64 {
                zeros.push(0);
            }
            let w2 = h.write_all(&zeros);
            if w2.is_err() {
                panic!();
            }
        }
    }
    let mf = open_field(path);
    if mf.is_ok() { panic!(); }
}

#[test]
fn scan_flag_accessor() {
    let hdr = BinHeader {
        nx: 2, ny: 5, levels: v1(1000.0),
        scan_ns: false, winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
    assert!(!hdr.scan_ns);
    let mut p0: Vec<f32> = Vec::<f32>::new();
    for j in 0 .. 5 {
        for _i in 0 .. 2 {
            p0.push(j as f32);
        }
    }
    let path = "/tmp/opencode/wxpt_scan.bin".to_string();
    let path2 = path.to_string();
    let w = write_bin_atomic(path, &hdr, &[&p0[0 .. 10]]);
    if w.is_err() { panic!(); }
    let mf = open_field(path2);
    match mf {
        Err(_) => { panic!(); }
        Ok(mfield) => {
            // stored row 0 is the SOUTH row (scan_ns=false): value 0;
            // logical north row 0 -> stored row ny-1 = 4 -> value 4
            assert!(mfield.value(0, 0, 0) == 0.0);
            assert!(mfield.stored_row(0) == 4);
            assert!(mfield.value(0, mfield.stored_row(0), 0) == 4.0);
        }
    }
}

// ---------------------------------------------------------------------------
// V1.3 phase E0: statistical-process metadata lives IN THE HEADER
// ---------------------------------------------------------------------------

#[test]
fn header_stat_metadata_round_trips_and_guards() {
    // A real accumulation window (offsets relative to the run time) plus a
    // NEGATIVE begin to pin two's-complement little-endian i64 encoding.
    let hdr = BinHeader {
        nx: 2, ny: 2, levels: v1(1000.0),
        scan_ns: true, winds_grid: false,
        stat: StatMeta {
            processed: true,
            kind: STAT_ACCUMULATION,
            begin_secs: -3600,
            end_secs: 43200,
            step_secs: 3600,
        },
    };
    let mut p0: Vec<f32> = Vec::<f32>::new();
    for k in 0 .. 4 {
        p0.push(k as f32);
    }
    let path = "/tmp/opencode/wxpt_stat.bin".to_string();
    let w = write_bin_atomic(path.to_string(), &hdr, &[&p0[0 .. 4]]);
    if w.is_err() { panic!(); }
    let mf = open_field(path.to_string());
    match mf {
        Err(_) => { panic!(); }
        Ok(mfield) => {
            assert!(mfield.header.stat.processed);
            assert!(mfield.header.stat.kind == STAT_ACCUMULATION);
            assert!(mfield.header.stat.begin_secs == -3600);
            assert!(mfield.header.stat.end_secs == 43200);
            assert!(mfield.header.stat.step_secs == 3600);
            // the guard decision the query path consumes (spec §6.3)
            assert!(mfield.header.stat.guards_time());
            // data planes still address exactly as before the header growth
            // (plane = [0,1,2,3] row-major over nx=2: row 1 = [2,3])
            assert!(mfield.value(0, 0, 1) == 1.0);
            assert!(mfield.value(0, 1, 1) == 3.0);
        }
    }

    // An instantaneous field must NOT guard time interpolation.
    let hdr2 = BinHeader {
        nx: 2, ny: 2, levels: v1(1000.0),
        scan_ns: true, winds_grid: false,
        stat: StatMeta::instantaneous(),
    };
    let head2 = encode_header(&hdr2);
    match decode_header(&head2) {
        Err(_) => { panic!(); }
        Ok(h) => {
            assert!(!h.stat.processed);
            assert!(h.stat.kind == 0);
            assert!(h.stat.begin_secs == 0 && h.stat.end_secs == 0 && h.stat.step_secs == 0);
            assert!(!h.stat.guards_time());
        }
    }

    // Fail-CLOSED consistency: a header that claims a stat KIND but forgot the
    // processed flag (or the reverse) still guards, because a rejected query
    // beats a wrong number.
    let half = StatMeta { processed: false, kind: STAT_ACCUMULATION,
                          begin_secs: 0, end_secs: 21600, step_secs: 0 };
    assert!(half.guards_time());
    let half2 = StatMeta { processed: true, kind: 0,
                           begin_secs: 0, end_secs: 0, step_secs: 0 };
    assert!(half2.guards_time());
}

#[test]
fn corrupt_stat_bytes_and_foreign_versions_are_refused() {
    let hdr = BinHeader {
        nx: 2, ny: 2, levels: v1(1000.0),
        scan_ns: true, winds_grid: false,
        stat: StatMeta {
            processed: true,
            kind: STAT_ACCUMULATION,
            begin_secs: 0,
            end_secs: 21600,
            step_secs: 0,
        },
    };
    let good = encode_header(&hdr);
    assert!(good.len() == BIN_FIXED_HEADER + 4);
    // baseline parses
    if decode_header(&good).is_err() { panic!(); }

    // (a) version bumped by hand -> BadVersion (the warm-start recovery trigger)
    let mut b = good.to_vec();
    b[8] = (BIN_VERSION + 1) as u8;
    match decode_header(&b) {
        Err(e) => {
            match e {
                HeaderError::BadVersion => {}
                _ => { panic!(); }
            }
        }
        Ok(_) => { panic!(); }
    }

    // (b) stat_kind outside the table -> BadStat (never guess what it meant)
    let mut b2 = good.to_vec();
    b2[23] = 9;
    match decode_header(&b2) {
        Err(e) => {
            match e {
                HeaderError::BadStat => {}
                _ => { panic!(); }
            }
        }
        Ok(_) => { panic!(); }
    }

    // (c) stat_processed byte not 0/1 -> BadStat
    let mut b3 = good.to_vec();
    b3[24] = 2;
    match decode_header(&b3) {
        Err(e) => {
            match e {
                HeaderError::BadStat => {}
                _ => { panic!(); }
            }
        }
        Ok(_) => { panic!(); }
    }

    // (d) reserved bytes must be zero: mid-block byte 26 ...
    let mut b4 = good.to_vec();
    b4[26] = 1;
    match decode_header(&b4) {
        Err(e) => {
            match e {
                HeaderError::BadStat => {}
                _ => { panic!(); }
            }
        }
        Ok(_) => { panic!(); }
    }
    // ... and the trailing reserved u32 at 52..56
    let mut b5 = good.to_vec();
    b5[55] = 7;
    match decode_header(&b5) {
        Err(e) => {
            match e {
                HeaderError::BadStat => {}
                _ => { panic!(); }
            }
        }
        Ok(_) => { panic!(); }
    }
}
