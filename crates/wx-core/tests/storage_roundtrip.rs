// Storage format round-trip tests (spec §4.5): atomic write (temp+fsync+
// rename), header validation, mmap read, scan-flag accessor, rejection of
// corrupt/truncated files.

use wx_core::storage::write_bin_atomic;
use wx_core::storage::open_field;
use wx_core::storage::encode_header;
use wx_core::storage::decode_header;
use wx_core::storage::expected_size;
use wx_core::storage::BinHeader;
use wx_core::storage::BIN_FIXED_HEADER;
use std::io::Write;

fn v1(a: f32) -> Vec<f32> {
    let mut v: Vec<f32> = Vec::<f32>::new();
    v.push(a);
    return v;
}

fn v2(a: f32, b: f32) -> Vec<f32> {
    let mut v: Vec<f32> = Vec::<f32>::new();
    v.push(a);
    v.push(b);
    return v;
}

fn v3(a: f32, b: f32, c: f32) -> Vec<f32> {
    let mut v: Vec<f32> = Vec::<f32>::new();
    v.push(a);
    v.push(b);
    v.push(c);
    return v;
}

#[test]
fn roundtrip_write_read() {
    let hdr = BinHeader {
        nx: 4, ny: 3, levels: v2(1000.0, 925.0),
        scan_ns: true, winds_grid: false,
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
    match w {
        Err(_) => { assert!(false); }
        Ok(_) => {}
    }
    let mf = open_field(path2);
    match mf {
        Err(_) => { assert!(false); }
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
    };
    let bytes = encode_header(&hdr);
    assert!(bytes.len() == BIN_FIXED_HEADER + 3 * 4);
    let back = decode_header(&bytes);
    match back {
        Err(_) => { assert!(false); }
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
    };
    let bytes = encode_header(&hdr);
    let path = "/tmp/opencode/wxpt_badmagic.bin".to_string();
    let f = std::fs::File::create(&path);
    match f {
        Err(_) => { assert!(false); }
        Ok(mut h) => {
            let mut bad: Vec<u8> = Vec::<u8>::new();
            for k in 0 .. bytes.len() {
                bad.push(bytes[k]);
            }
            bad[0] = 88; // 'X' != 'W': magic mismatch
            let w = h.write_all(&bad);
            if w.is_err() {
                assert!(false);
            }
        }
    }
    let mf = open_field(path);
    match mf {
        Ok(_) => { assert!(false); }
        Err(_) => {}
    }
}

#[test]
fn reject_truncated_data() {
    let hdr = BinHeader {
        nx: 4, ny: 4, levels: v2(1000.0, 500.0),
        scan_ns: true, winds_grid: false,
    };
    let head = encode_header(&hdr);
    let path = "/tmp/opencode/wxpt_trunc.bin".to_string();
    let f = std::fs::File::create(&path);
    match f {
        Err(_) => { assert!(false); }
        Ok(mut h) => {
            // header + only ONE plane of data (two declared) -> truncated
            let w = h.write_all(&head);
            if w.is_err() {
                assert!(false);
            }
            let mut zeros: Vec<u8> = Vec::<u8>::new();
            for _k in 0 .. 64 {
                zeros.push(0);
            }
            let w2 = h.write_all(&zeros);
            if w2.is_err() {
                assert!(false);
            }
        }
    }
    let mf = open_field(path);
    match mf {
        Ok(_) => { assert!(false); }
        Err(_) => {}
    }
}

#[test]
fn scan_flag_accessor() {
    let hdr = BinHeader {
        nx: 2, ny: 5, levels: v1(1000.0),
        scan_ns: false, winds_grid: false,
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
    match w {
        Err(_) => { assert!(false); }
        Ok(_) => {}
    }
    let mf = open_field(path2);
    match mf {
        Err(_) => { assert!(false); }
        Ok(mfield) => {
            // stored row 0 is the SOUTH row (scan_ns=false): value 0;
            // logical north row 0 -> stored row ny-1 = 4 -> value 4
            assert!(mfield.value(0, 0, 0) == 0.0);
            assert!(mfield.stored_row(0) == 4);
            assert!(mfield.value(0, mfield.stored_row(0), 0) == 4.0);
        }
    }
}
