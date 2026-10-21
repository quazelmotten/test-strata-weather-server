// V1.2 phase A gate #1: the STREAMING atomic writer must be BIT-IDENTICAL to
// the pre-change whole-buffer writer. The expected bytes were captured FROM the
// V1.1 implementation BEFORE the change (`tests/fixtures/writer_golden_v1.bin`,
// also reproduced independently by `scripts/gen_writer_golden.py`) and are ALSO
// embedded here as hex so the test cannot silently pass against a mutated
// fixture. The retained reference implementation below is the OLD algorithm
// verbatim in spirit (serialize the WHOLE file into one Vec<u8>, then write_all),
// kept so the equivalence proof covers the algorithm AND the artifact.

use wx_core::storage::{BinHeader, write_bin_atomic, write_bin_atomic_stream, encode_header,
                       PlaneSource};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;

// ---------------------------------------------------------------------------
// golden input + expected bytes
// ---------------------------------------------------------------------------

// Fixed input: nx=7 ny=5, levels [1000, 837.5, 500] hPa (canonical DESCENDING),
// scan_ns=true, winds_grid=false, plane(li,j,i) = li*1000 + j*10 + i.
fn golden_hdr() -> BinHeader {
    return BinHeader {
        nx: 7,
        ny: 5,
        levels: [1000.0, 837.5, 500.0].to_vec(),
        scan_ns: true,
        winds_grid: false,
    };
}

/// Level coordinates of the golden input, canonical (descending) order.
const LEVEL_VALS: [f64; 3] = [1000.0, 837.5, 500.0];

/// One plane of the golden input: value(li,j,i) = li*1000 + j*10 + i.
fn plane_vals(li: usize) -> Vec<f32> {
    let mut pl: Vec<f32> = Vec::<f32>::new();
    for j in 0 .. 5 {
        for i in 0 .. 7 {
            pl.push(((li * 1000) + (j * 10) + i) as f32);
        }
    }
    return pl;
}

// 912 hex chars = the 456-byte golden file (24 fixed header + 3*4 level coords
// + 3*35 f32 values). Captured from the V1.1 writer on 2026-10-21.
fn golden_hex() -> String {
    let mut hx = "".to_string();
    hx = hx + "5758505442494e310100000007000000050000000300010000007a44006051440000fa43000000000000803f00000040";
    hx = hx + "00004040000080400000a0400000c040000020410000304100004041000050410000604100007041000080410000a041";
    hx = hx + "0000a8410000b0410000b8410000c0410000c8410000d0410000f0410000f84100000042000004420000084200000c42";
    hx = hx + "0000104200002042000024420000284200002c4200003042000034420000384200007a4400407a4400807a4400c07a44";
    hx = hx + "00007b4400407b4400807b4400807c4400c07c4400007d4400407d4400807d4400c07d4400007e4400007f4400407f44";
    hx = hx + "00807f4400c07f4400008044002080440040804400c0804400e080440000814400208144004081440060814400808144";
    hx = hx + "000082440020824400408244006082440080824400a0824400c082440000fa440020fa440040fa440060fa440080fa44";
    hx = hx + "00a0fa4400c0fa440040fb440060fb440080fb4400a0fb4400c0fb4400e0fb440000fc440080fc4400a0fc4400c0fc44";
    hx = hx + "00e0fc440000fd440020fd440040fd4400c0fd4400e0fd440000fe440020fe440040fe440060fe440080fe440000ff44";
    hx = hx + "0020ff440040ff440060ff440080ff4400a0ff4400c0ff44";
    return hx;
}

fn unhex(hx: &str) -> Vec<u8> {
    let b = hx.as_bytes().to_vec();
    let mut out: Vec<u8> = Vec::<u8>::new();
    for k in 0 .. b.len() / 2 {
        out.push((nib(b[2 * k]) << 4) | nib(b[2 * k + 1]));
    }
    return out;
}

fn nib(c: u8) -> u8 {
    if c >= 97 {
        return c - 97 + 10;
    }
    return c - 48;
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Raw bytes of a file on disk (mmap copy).
fn read_bytes(path: &str) -> Vec<u8> {
    let f = File::open(path);
    match f {
        Err(_) => { return [].to_vec(); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return [].to_vec(); }
                Ok(region) => {
                    let mut out: Vec<u8> = Vec::<u8>::new();
                    for k in 0 .. region.len() {
                        out.push(region[k]);
                    }
                    return out;
                }
            }
        }
    }
}

/// RETAINED reference implementation of the PRE-change writer: build the ENTIRE
/// file as one Vec<u8> (header + every plane, little-endian f32) and write it.
/// This is the V1.1 algorithm (`write_bin_atomic` before phase A), kept here so
/// the golden test compares the NEW streaming path against the OLD algorithm
/// directly, not only against a stored artifact.
fn ref_write_pre_change(path: &str, hdr: &BinHeader, planes: &[&[f32]]) {
    let mut bytes: Vec<u8> = encode_header(hdr);
    for li in 0 .. planes.len() {
        for k in 0 .. planes[li].len() {
            let bits = planes[li][k].to_bits();
            bytes.push((bits & 0xFF) as u8);
            bytes.push(((bits >> 8) & 0xFF) as u8);
            bytes.push(((bits >> 16) & 0xFF) as u8);
            bytes.push(((bits >> 24) & 0xFF) as u8);
        }
    }
    let f = File::create(path);
    match f {
        Err(_) => { return; }
        Ok(mut h) => {
            let _w = h.write_all(&bytes);
        }
    }
}

/// Borrowed plane source over (level, plane) pairs — the shape wx-ingest merges
/// into, used here to exercise the generic streaming entry point directly.
struct PairPlanes<'a> {
    pub items: &'a Vec<(f64, Vec<f32>)>,
    pub idx: usize
}

impl<'a> PlaneSource for PairPlanes<'a> {
    fn next_plane(self: &mut Self) -> Option<&[f32]> {
        if self.idx >= self.items.len() {
            return None;
        }
        let r = &self.items[self.idx].1[..];
        self.idx += 1;
        return Some(r);
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn streaming_writer_output_is_byte_identical_to_golden() {
    let dir = "/tmp/opencode/wxpt-golden-a".to_string();
    let _c = std::fs::create_dir_all(&dir);
    let hdr = golden_hdr();
    // pairs FIRST, then borrow the planes out of them for the slice API
    let mut pairs: Vec<(f64, Vec<f32>)> = Vec::<(f64, Vec<f32>)>::new();
    for li in 0 .. 3 {
        pairs.push(((LEVEL_VALS[li]) as f64, plane_vals(li)));
    }
    let refs: Vec<&[f32]> = [&pairs[0].1[..], &pairs[1].1[..], &pairs[2].1[..]].to_vec();

    // (1) the historical wrapper (unchanged signature) ...
    let p_wrap = format!("{0}/wrap.bin", dir);
    let w1 = write_bin_atomic(p_wrap.to_string(), &hdr, &refs);
    assert!(w1.is_ok());
    // (2) ... and the new streaming entry point over a borrowed source.
    let p_stream = format!("{0}/stream.bin", dir);
    let w2 = write_bin_atomic_stream(p_stream.to_string(), &hdr, pairs.len(),
                                     PairPlanes { items: &pairs, idx: 0 });
    assert!(w2.is_ok());
    // (3) the retained reference implementation of the OLD writer.
    let p_ref = format!("{0}/ref.bin", dir);
    ref_write_pre_change(&p_ref, &hdr, &refs);

    let want = unhex(&golden_hex());
    assert!(want.len() == 456);
    let got_wrap = read_bytes(&p_wrap);
    let got_stream = read_bytes(&p_stream);
    let got_ref = read_bytes(&p_ref);
    assert!(got_wrap == want);
    assert!(got_stream == want);
    assert!(got_ref == want);

    // (4) the committed fixture agrees with the embedded hex (guards against a
    // silently modified fixture).
    let fx = "../../tests/fixtures/writer_golden_v1.bin".to_string();
    let got_fx = read_bytes(&fx);
    assert!(got_fx == want);
}

#[test]
fn streaming_writer_chunks_across_the_buffer_boundary_identically() {
    // A plane LARGER than WRITE_CHUNK_BYTES forces MANY internal flushes: nx*ny*4
    // = 700*600*4 = 1.68 MB > 256 KiB. The old writer materialized this whole
    // file at once; the new one must still produce the SAME BYTES.
    let dir = "/tmp/opencode/wxpt-golden-b".to_string();
    let _c = std::fs::create_dir_all(&dir);
    let hdr = BinHeader {
        nx: 700,
        ny: 600,
        levels: [1000.0, 925.0].to_vec(),
        scan_ns: true,
        winds_grid: false,
    };
    let mut pa: Vec<f32> = Vec::<f32>::new();
    let mut pb: Vec<f32> = Vec::<f32>::new();
    for j in 0 .. 600 {
        for i in 0 .. 700 {
            // values chosen so mantissa rounding cannot hide a byte-order bug
            pa.push(((j * 700 + i) % 100003) as f32);
            pb.push(-((j * 7919 + i) % 99991) as f32);
        }
    }
    let refs: Vec<&[f32]> = [&pa[..], &pb[..]].to_vec();
    let p_new = format!("{0}/big.bin", dir);
    let w = write_bin_atomic(p_new.to_string(), &hdr, &refs);
    assert!(w.is_ok());
    let p_old = format!("{0}/big_ref.bin", dir);
    ref_write_pre_change(&p_old, &hdr, &refs);
    let a = read_bytes(&p_new);
    let b = read_bytes(&p_old);
    assert!(a.len() == b.len());
    let mut same = true;
    for k in 0 .. a.len() {
        if a[k] != b[k] {
            same = false;
            break;
        }
    }
    assert!(same);
    assert!(a.len() == 24 + 2 * 4 + 2 * 700 * 600 * 4);
}
