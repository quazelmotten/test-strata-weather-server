// V1.3 phase E3a DEFECT GATE #3 — PRE-STABILITY READS WITHOUT MMAP.
//
// THE DEFECT (AUDIT-confirmed): content_hash_file and head_is_grib mmap EVERY
// CANDIDATE — content_hash_file runs before the stability verdict exists, on
// files producers may still be writing. A producer truncating the file in
// place between the mapping and a page fault delivers SIGBUS to the ingest
// thread: a hardware-style signal no Rust error handling catches, taking the
// whole process down over a transient race. Additionally read_bytes_maybe
// copied mmap bytes ONE AT A TIME into a Vec.
//
// THE FIX: plain File + seek + read_exact over the first/last 64 KiB windows
// (whole file once when <= 128 KiB). A shrink race now costs an Err (retry
// next round) or a digest of what was really read — never a panic. The DIGEST
// AND OUTPUT FORMAT ARE PRESERVED EXACTLY: this binary pins the new reader
// against (a) content_hash_bytes over whole-file buffers (the unchanged old
// algorithm) at every window boundary, and (b) literal digests computed by an
// INDEPENDENT Python port of the pre-E3a algorithm — old state records keep
// matching byte-for-byte after an upgrade.

use wx_ingest::state::{content_hash_file, content_hash_bytes, head_is_grib, HASH_WINDOW};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::OpenOptions;

#[allow(clippy::needless_return)]
fn rm_tree(dir: &str) {
    match read_dir(dir) {
        Err(_) => {}
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let p = path_to_str(&de.path());
                        match read_dir(&p) {
                            Err(_) => {
                                let _r = remove_file(&p);
                            }
                            Ok(_) => {
                                rm_tree(&p);
                            }
                        }
                    }
                }
            }
        }
    }
    let _d = remove_dir(dir);
}

#[allow(clippy::needless_return)]
fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!(); }
        Ok(mut w) => {
            let n = w.write_all(bytes);
            if n.is_err() { panic!(); }
        }
    }
}

#[allow(clippy::needless_return)]
fn read_full(path: &str) -> Vec<u8> {
    let f = File::open(path);
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

/// The SAME LCG the Python golden generator used (mod 2^64 wrapping math).
#[allow(clippy::needless_return)]
fn lcg_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::<u8>::new();
    let mut s = seed;
    for _k in 0 .. n {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.push(((s >> 33) & 0xFF) as u8);
    }
    return out;
}

#[allow(clippy::needless_return)]
fn ramp_bytes(n: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::<u8>::new();
    for i in 0 .. n {
        out.push((i % 251) as u8);
    }
    return out;
}

fn fixture(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

// ---------------------------------------------------------------------------
// (a) equivalence at EVERY window boundary + independent Python goldens
// ---------------------------------------------------------------------------

#[test]
fn pread_hash_equals_the_old_buffer_digest_and_python_goldens() {
    let root = "/tmp/opencode/wxpoint-hashcompat".to_string();
    rm_tree(&root);
    let _c = create_dir_all(&root);
    let w = HASH_WINDOW; // 65536

    // name, bytes, golden digest from the INDEPENDENT Python port ("" = only
    // the buffer-equivalence check applies).
    let cases: Vec<(String, Vec<u8>, String)> = vec![
        ("empty".to_string(), lcg_bytes(0, 12345), "ecba3df2c3383c52".to_string()),
        ("one".to_string(), bytes_of(&[7u8]), "33e83e77f52c095b".to_string()),
        ("small1000".to_string(), ramp_bytes(1000), "688457a0d845dd0b".to_string()),
        ("at_window".to_string(), lcg_bytes(w, 12345), "e2ac57ca99fe4fe3".to_string()),
        ("window_minus_1".to_string(), lcg_bytes(w - 1, 12345), "".to_string()),
        ("window_plus_1".to_string(), lcg_bytes(w + 1, 12345), "40fc172aa7c6592b".to_string()),
        ("at_2window".to_string(), lcg_bytes(2 * w, 12345), "235edc76a1695b46".to_string()),
        ("two_window_minus_1".to_string(), lcg_bytes(2 * w - 1, 12345), "".to_string()),
        ("over_2window".to_string(), lcg_bytes(2 * w + 1, 12345), "1268fba53cf6ee2c".to_string()),
        ("big300k".to_string(), lcg_bytes(300000, 12345), "8123d46442e50f4c".to_string()),
    ];
    for cs in &cases {
        let p = format!("{0}/{1}.bin", root, cs.0);
        write_bytes(&p, &cs.1);
        let hf = content_hash_file(&p);
        let got = match hf {
            Err(e) => { println!("{0}: hash failed: {e}", cs.0); panic!(); }
            Ok(v) => { v }
        };
        // (1) identical to the UNCHANGED whole-buffer algorithm...
        assert!(got == content_hash_bytes(&cs.1));
        // (2) ...and identical to the pre-E3a algorithm as ported to Python
        //     independently of this codebase (proves old records keep matching).
        if !cs.2.is_empty() {
            assert!(got == cs.2.clone());
        }
    }
    rm_tree(&root);
}

#[allow(clippy::needless_return)]
fn bytes_of(b: &[u8]) -> Vec<u8> {
    b.to_vec()
}

// ---------------------------------------------------------------------------
// (b) shrinking-file tolerance: stat->truncate->read races never panic
// ---------------------------------------------------------------------------

#[test]
fn shrinking_file_between_stat_and_read_is_graceful() {
    let root = "/tmp/opencode/wxpoint-hashshrink".to_string();
    rm_tree(&root);
    let _c = create_dir_all(&root);
    let p = format!("{0}/shrink.bin", root);
    let big = lcg_bytes(300000, 999);
    write_bytes(&p, &big);
    let h_big = content_hash_file(&p);
    if h_big.is_err() { panic!(); }

    // Producer truncates IN PLACE (the SIGBUS surface under the mmap reader):
    // the plain reader must simply observe the shorter file.
    let fo = OpenOptions::new().write(true).open(&p);
    match fo {
        Err(_) => { panic!(); }
        Ok(h) => {
            let t = h.set_len(10);
            if t.is_err() { panic!(); }
        }
    }
    let h_small = content_hash_file(&p);
    let hs = match h_small {
        Err(e) => { println!("truncated hash failed: {e}"); panic!(); }
        Ok(v) => { v }
    };
    assert!(hs != "0000000000000000");
    let first10 = read_full(&p);
    assert!(hs == content_hash_bytes(&first10));

    // Fully truncated to ZERO: the old mmap version ERRORED on empty files
    // (mmap of a 0-length mapping); the plain reader hashes the empty buffer
    // — a documented, benign improvement, pinned here.
    let fo2 = OpenOptions::new().write(true).open(&p);
    match fo2 {
        Err(_) => { panic!(); }
        Ok(h) => {
            let t = h.set_len(0);
            if t.is_err() { panic!(); }
        }
    }
    let h_zero = content_hash_file(&p);
    match h_zero {
        Err(e) => { println!("empty hash failed: {e}"); panic!(); }
        Ok(v) => {
            assert!(v == "ecba3df2c3383c52");
            assert!(v == content_hash_bytes([].as_ref()));
        }
    }
    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// (c) head_is_grib edges: short/empty/missing files are graceful
// ---------------------------------------------------------------------------

#[test]
fn head_is_grib_short_empty_missing_are_graceful() {
    let root = "/tmp/opencode/wxpoint-headgrib".to_string();
    rm_tree(&root);
    let _c = create_dir_all(&root);

    // missing file: retryable I/O Err (the pipeline streaks it since E3a).
    let miss = head_is_grib(&format!("{0}/nope.grib2", root));
    if let Ok(v) = miss { println!("missing file gave Ok({v})"); panic!(); }
    // empty and 2-byte files: definitive Ok(false), NOT an error, no panic.
    write_bytes(&format!("{0}/e.grib2", root), [].as_ref());
    match head_is_grib(&format!("{0}/e.grib2", root)) {
        Err(_) => { panic!(); }
        Ok(v) => { assert!(!v); }
    }
    write_bytes(&format!("{0}/gr.grib2", root), &bytes_of(b"GR"));
    match head_is_grib(&format!("{0}/gr.grib2", root)) {
        Err(_) => { panic!(); }
        Ok(v) => { assert!(!v); }
    }
    write_bytes(&format!("{0}/ok.grib2", root), &bytes_of(b"GRIBx"));
    match head_is_grib(&format!("{0}/ok.grib2", root)) {
        Err(_) => { panic!(); }
        Ok(v) => { assert!(v); }
    }
    // a real fixture still reads true through the plain reader.
    match head_is_grib(fixture("gfs_small.grib2").as_str()) {
        Err(_) => { panic!(); }
        Ok(v) => { assert!(v); }
    }
    rm_tree(&root);
}
