//! Section-7 codec backends for the compressed GRIB2 data representation
//! templates required by spec \u00a74.2 ("at minimum": 5.0, 5.3, 5.40 JPEG2000,
//! 5.41 PNG):
//!
//! * **DRT 5.40 JPEG2000** - `hayro-jpeg2000` 0.4.1 (pure Rust; built with
//!   `default-features = false`, features `std` + `simd`, so the `image`/`moxcms`
//!   integration is not compiled in). The section-7 payload IS the JPEG2000
//!   stream: wgrib2 writes it with NO PSV byte (`jpeg_pk.c`: `sec7[5+k] =
//!   outjpc[k]`) and reads it from `sec[7]+5`; NCEP streams are raw codestreams
//!   starting `FF 4F FF 51`, which hayro parses directly (JP2 containers are
//!   also accepted). Codestream header boxes are handled internally: hayro parses
//!   the `COM` box (Jasper writes `Creator: JasPer Version 1.900.1` there), skips
//!   `CRG` and the parameterless marker range `0xFF30..0xFF3F`, and bails with
//!   `MarkerError::Unsupported` on any other marker code — which we surface as
//!   `CodecFailure` rather than guessing.
//! * **DRT 5.41 PNG** - own PNG chunk parser (IHDR / IDAT* / IEND; each chunk's CRC32
//!   covers the four TYPE bytes plus the data, RFC PNG style, verified with
//!   `zlib-rs` chaining), inflate via `zlib-rs` 0.6.8, PNG unfilter (None/Sub/Up/
//!   Average/Paeth) implemented here EXACTLY as the libpng 1.2.59 that wgrib2 links
//!   (`pngrutil.c::png_read_filter_row`, including its Paeth predictor form and the
//!   first-`bpp`-bytes-of-row special case), then the SAME integer pipeline as simple
//!   packing. wgrib2 cannot ENCODE PNG (its encoder lives in g2clib), so fixtures
//!   come from `scripts/make_png_twin.py`, whose filter implementations mirror this
//!   decoder's inverse branch-for-branch.
//! * **DRT 5.42 CCSDS AEC** - `rust-aec` 0.1.1 (pure-Rust libaec port, explicitly
//!   targeting GRIB2 template 5.0=42).
//!
//! All three produce packed integers, and the final value is
//! `(ref + packed * 2^E) * 10^-S` evaluated in f64 and narrowed to f32, matching
//! wgrib2's `unpk_0.c` / `unpk.c` (`((packed*bin_scale)+reference)*dec_scale`)
//! bit-for-bit.
//!
//! ## Field layouts (verified against `tools/wgrib2-full-src`)
//!
//! Offsets are into the section-5 PAYLOAD as this repo's parser provides it,
//! i.e. wgrib2's `sec[5]+N` minus the 5-byte section header (`uint4 len | u8
//! number`).
//!
//! | template | sec5 payload | source of truth |
//! |---|---|---|
//! | 5.40 | `0..4` defined points, `4..6` DRT, `6..10` ref (f32 BE), `10..12` E, `12..14` S, `14` bits-per-value, `16` lossless flag, `17` undefined byte | `jpeg_pk.c`, `unpk.c` |
//! | 5.41 | identical through `14` (nothing extra is written) | `make_png_twin.py`, `unpk.c` |
//! | 5.42 | identical through `14`, `16` ccsdsFlags, `17` block size, `18..20` RSI (uint2) | `aec_pk.c`, `unpk.c` (`sec[5][21]`, `sec[5][22]`, `uint2(sec[5]+23)`) |
//!
//! Geometry rules mirror wgrib2 exactly: when a bitmap is present the image /
//! PNG row / AEC stream holds ONLY the DEFINED values (wgrib2 `mk_bms`); without
//! one it holds `nx*ny` values (`jpeg_pk.c`: `ix=nx, iy=ny` if nothing is missing,
//! otherwise `ix=n_defined, iy=1`). Samples are consumed in scan order and
//! re-interleaved with the section-6 bitmap.
//!
//! ## Deliberate limits (typed errors, never guesses)
//!
//! Anything that cannot be validated against wgrib2 truth in CI is rejected
//! instead of approximated: JPEG2000 bit depth > 24 (hayro hands back f32 samples,
//! exact only up to 2^24) or multi-component images; PNG color types other than
//! grayscale, bit depths other than 8/16, interlaced or palette PNGs; AEC
//! parameter sets rejected by `rust-aec`. See docs/DECISIONS.md.

use wx_core::grid::GridDef;

use crate::grib2::GribErr;
use crate::grib2::be_u16;
use crate::grib2::be_u32;
use crate::grib2::ieee_f4;
use crate::grib2::sm_i2;
use crate::grib2::bm_is_missing;
use crate::grib2::pow10;
use crate::grib2::pow2;
use crate::grib2::nan_f32;

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Section-5 scalars shared by every "packed integers + linear scaling"
/// template: reference value (big-endian f32), binary scale exponent E, decimal
/// scale S, and bits per packed value.
pub struct PackedScalars {
    pub ref0: f64,
    pub e_exp: i32,
    pub s_dec: i32,
    pub nbits: u8,
}

/// Read the shared scalars. Requires a 15-byte payload, the same minimum as the
/// simple-packing path.
pub fn scalars_of(s5: &[u8]) -> Result<PackedScalars, GribErr> {
    if s5.len() < 15 {
        return Err(GribErr::Truncated);
    }
    let sc = PackedScalars {
        ref0: ieee_f4(s5, 6) as f64,
        e_exp: sm_i2(s5, 10) as i32,
        s_dec: sm_i2(s5, 12) as i32,
        nbits: s5[14],
    };
    return Ok(sc);
}

/// Number of grid points whose value is stored in the codec stream: all points
/// when there is no bitmap, otherwise the count of set bitmap bits (bit=1 means
/// DEFINED, matching wgrib2 `missing.c`).
pub fn defined_count(bm: Option<&[u8]>, ngrid: usize) -> usize {
    match bm {
        None => { return ngrid; }
        Some(_b) => {
            let mut n = 0;
            for k in 0 .. ngrid {
                if !bm_is_missing(bm, k) {
                    n += 1;
                }
            }
            return n;
        }
    }
}

/// Scale packed integers into grid values: NaN at masked points, otherwise
/// `(ref + p * 2^E) * 10^-S` in f64 narrowed to f32 (wgrib2 unpk_0.c v1.2.8
/// formula). `ud.len()` must equal the number of defined points.
pub fn finish_values(sc: &PackedScalars, bm: Option<&[u8]>, ud: Vec<i64>,
                     ngrid: usize) -> Vec<f32> {
    let f2 = pow2(sc.e_exp);
    let f10 = pow10(-sc.s_dec);
    let mut out = Vec::<f32>::new();
    let mut j = 0;
    for k in 0 .. ngrid {
        if bm_is_missing(bm, k) {
            out.push(nan_f32());
            continue;
        }
        let v = ud[j];
        j += 1;
        out.push(((sc.ref0 + (v as f64) * f2) * f10) as f32);
    }
    return out;
}

/// `nbits == 0` means every defined point equals the reference value and the
/// codec stream is empty (wgrib2 short-circuits before touching section 7).
pub fn zero_samples(n: usize) -> Result<Vec<i64>, GribErr> {
    let mut v = Vec::<i64>::new();
    for _k in 0 .. n {
        v.push(0);
    }
    return Ok(v);
}

/// Convert big-endian byte groups into packed integers.
fn bytes_to_ints(buf: &[u8], nbytes: usize, nsamples: usize) -> Vec<i64> {
    let mut ud = Vec::<i64>::new();
    for smp in 0 .. nsamples {
        let off = smp * nbytes;
        let mut v: i64 = 0;
        for b in 0 .. nbytes {
            v = (v << 8) | (buf[off + b] as i64);
        }
        ud.push(v);
    }
    return ud;
}

// ---------------------------------------------------------------------------
// DRT 5.40 JPEG2000
// ---------------------------------------------------------------------------

/// Decode a JPEG2000-packed section 7 into packed integer samples.
pub fn jpeg_samples(s7: &[u8], expect_n: usize) -> Result<Vec<i64>, GribErr> {
    let settings = hayro_jpeg2000::DecodeSettings::default();
    let img = hayro_jpeg2000::Image::new(s7, &settings);
    match img {
        Err(_e) => { return Err(codec_fail("jpeg-stream")); }
        Ok(im) => {
            let mut ctx = hayro_jpeg2000::DecoderContext::default();
            let dec = im.decode(&mut ctx);
            match dec {
                Err(_e) => { return Err(codec_fail("jpeg-decode")); }
                Ok(d) => {
                    let comps = d.components();
                    if comps.len() != 1 {
                        return Err(codec_unsupported("jpeg-multichannel"));
                    }
                    let c = &comps[0];
                    if c.bit_depth() > 24 {
                        // hayro hands back f32 samples: exact only up to 2^24.
                        return Err(codec_unsupported("jpeg-bit-depth-gt-24"));
                    }
                    let smp = c.samples();
                    if smp.len() != expect_n {
                        return Err(GribErr::BadField("jpeg sample count mismatch"));
                    }
                    let mut ud = Vec::<i64>::new();
                    for x in smp {
                        let v = *x as f64;
                        if v < 0.0 || v > 4294967295.0 {
                            return Err(codec_fail("jpeg-sample-range"));
                        }
                        ud.push((v + 0.5).floor() as i64);
                    }
                    return Ok(ud);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DRT 5.41 PNG
// ---------------------------------------------------------------------------

const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// True when `s7[p+4..p+8]` equals the ASCII chunk name `nm`.
fn chunk_is(s7: &[u8], p: usize, n0: u8, n1: u8, n2: u8, n3: u8) -> bool {
    return s7[p + 4] == n0 && s7[p + 5] == n1 && s7[p + 6] == n2 && s7[p + 7] == n3;
}

/// Parse, inflate and unfilter a PNG-packed section-7 stream. Returns the
/// reconstructed big-endian sample bytes, the per-row filter types (fixture
/// diagnostic), the bytes per sample and the pixel count. The cross-check against
/// the defined-point count lives in `png_samples` so this helper can also be
/// used on streams without one.
fn png_unfiltered(s7: &[u8]) -> Result<(Vec<u8>, Vec<u8>, usize, usize), GribErr> {
    if s7.len() < 8 {
        return Err(GribErr::Truncated);
    }
    let mut sig_ok = true;
    for k in 0 .. 8 {
        if s7[k] != PNG_SIG[k] {
            sig_ok = false;
        }
    }
    if !sig_ok {
        return Err(codec_fail("png-signature"));
    }

    // ---- chunk walk: u32 length | 4-byte type | data | u32 crc32 ----
    let mut ihdr: Vec<u8> = Vec::<u8>::new();
    let mut idat: Vec<u8> = Vec::<u8>::new();
    let mut seen_ihdr = false;
    let mut seen_iend = false;
    let mut p = 8;
    while p + 12 <= s7.len() {
        let clen = be_u32(s7, p) as usize;
        if p + 12 + clen > s7.len() {
            return Err(GribErr::Truncated);
        }
        let body = &s7[p + 8 .. p + 8 + clen];
        let crc = be_u32(s7, p + 8 + clen);
        // RFC PNG: the CRC covers the four chunk TYPE bytes plus the data, so
        // seed the running checksum with the name bytes (zlib-style chaining).
        let nm: [u8; 4] = [s7[p + 4], s7[p + 5], s7[p + 6], s7[p + 7]];
        let seed = zlib_rs::crc32::crc32(0, &nm[..]);
        if crc != zlib_rs::crc32::crc32(seed, body) {
            return Err(codec_fail("png-crc"));
        }
        if chunk_is(s7, p, 0x49, 0x48, 0x44, 0x52) {
            // "IHDR"
            if seen_ihdr {
                return Err(codec_fail("png-duplicate-ihdr"));
            }
            seen_ihdr = true;
            ihdr = body.to_vec();
        } else if chunk_is(s7, p, 0x49, 0x44, 0x41, 0x54) {
            // "IDAT"
            idat.extend(body.iter().copied());
        } else if chunk_is(s7, p, 0x49, 0x45, 0x4e, 0x44) {
            // "IEND"
            seen_iend = true;
            break;
        }
        p += 12 + clen;
    }
    if !seen_ihdr || !seen_iend {
        return Err(codec_fail("png-chunks"));
    }
    if ihdr.len() < 13 {
        return Err(codec_fail("png-ihdr-short"));
    }

    let w = be_u32(&ihdr[..], 0) as usize;
    let h = be_u32(&ihdr[..], 4) as usize;
    let depth = ihdr[8];
    let ctype = ihdr[9];
    let comp = ihdr[10];
    let fmethod = ihdr[11];
    let interlace = ihdr[12];
    if ctype != 0 {
        // grayscale only: GRIB PNG producers emit gray images, and the 24/32-bit
        // RGB paths cannot be validated against wgrib2 truth here.
        return Err(codec_unsupported("png-color-type"));
    }
    if interlace != 0 {
        return Err(codec_unsupported("png-interlace"));
    }
    if comp != 0 || fmethod != 0 {
        return Err(codec_unsupported("png-methods"));
    }
    if depth != 8 && depth != 16 {
        // WMO also lists 1/2/4/24/32; wgrib2 itself mis-reads sub-8 depths (its
        // bitstream branch only fires when w*depth % 8 != 0) and no GRIB producer
        // emits them, so they stay rejected.
        return Err(codec_unsupported("png-bit-depth"));
    }
    if w == 0 || h == 0 {
        return Err(GribErr::BadField("png zero dimension"));
    }

    let nbytes = (depth as usize) / 8;
    let rowlen = w * nbytes;
    let need = (rowlen + 1) * h;

    // ---- inflate the concatenated IDAT stream (zlib wrapper, RFC PNG) ----
    // inflate writes exactly `need` bytes; the buffer starts zeroed so a short
    // stream cannot masquerade as valid output (the StreamEnd check below).
    let mut rows_z: Vec<u8> = vec![0; need];
    let mut inf = zlib_rs::Inflate::new(true, 15);
    let res = inf.decompress(&idat[..], &mut rows_z[..], zlib_rs::InflateFlush::Finish);
    match res {
        Err(_e) => { return Err(codec_fail("png-inflate")); }
        Ok(st) => {
            if st != zlib_rs::Status::StreamEnd || inf.total_out() != (need as u64) {
                return Err(codec_fail("png-inflate-length"));
            }
        }
    }

    // ---- unfilter scanlines into a plain sample buffer ----
    let mut rows: Vec<u8> = Vec::<u8>::new();
    let mut filters: Vec<u8> = Vec::<u8>::new();
    for j in 0 .. h {
        let stride = (rowlen + 1) as usize;
        let ftype = rows_z[j * stride];
        if ftype > 4 {
            return Err(codec_fail("png-filter-type"));
        }
        filters.push(ftype);
        for k in 0 .. rowlen {
            let cur = rows_z[j * stride + 1 + k] as i32;
            let left: i32 = if k >= nbytes { rows[j * rowlen + k - nbytes] as i32 } else { 0 };
            let up: i32 = if j > 0 { rows[(j - 1) * rowlen + k] as i32 } else { 0 };
            let ul: i32 = if j > 0 && k >= nbytes {
                rows[(j - 1) * rowlen + k - nbytes] as i32
            } else {
                0
            };
            let pred: i32 = match ftype {
                0 => { 0 }
                1 => { left }
                2 => { up }
                3 => { (left + up) >> 1 }
                4 => {
                    // libpng-1.2.59 pngrutil.c PAETH inverse: p = b-c, pc = a-c,
                    // pa = |p|, pb = |pc|, pc' = |p+pc|; pick a, else b, else c.
                    let d = up - ul;
                    let e = left - ul;
                    let ad = if d < 0 { -d } else { d };
                    let ae = if e < 0 { -e } else { e };
                    let s = d + e;
                    let asb = if s < 0 { -s } else { s };
                    if ad <= ae && ad <= asb {
                        left
                    } else if ae <= asb {
                        up
                    } else {
                        ul
                    }
                }
                _ => { 0 }
            };
            rows.push(((cur + pred) & 0xFF) as u8);
        }
    }

    return Ok((rows, filters, nbytes, w * h));
}

/// Decode a PNG-packed section 7 into packed integer samples. The PNG holds one
/// grayscale pixel per stored value, big-endian across `depth/8` bytes (equivalent
/// to wgrib2's `unpk_0(..., ((nbits+7)/8)*8, ...)` byte-strided bitstream read).
pub fn png_samples(s7: &[u8], expect_n: usize) -> Result<Vec<i64>, GribErr> {
    let pr = png_unfiltered(s7);
    match pr {
        Err(e) => { return Err(e); }
        Ok((rows, _filters, nbytes, npix)) => {
            if npix != expect_n {
                return Err(GribErr::BadField("png pixel count mismatch"));
            }
            return Ok(bytes_to_ints(&rows[..], nbytes, npix));
        }
    }
}

/// Per-row PNG filter types of a section-7 stream. Fixture diagnostics: proves
/// which predictor branches a packing twin actually exercises.
pub fn png_filter_types(s7: &[u8]) -> Result<Vec<u8>, GribErr> {
    let pr = png_unfiltered(s7);
    match pr {
        Err(e) => { return Err(e); }
        Ok((_rows, filters, _nbytes, _npix)) => { return Ok(filters); }
    }
}

// ---------------------------------------------------------------------------
// DRT 5.42 CCSDS AEC
// ---------------------------------------------------------------------------

/// Decode an AEC-packed section 7 into packed integer samples. `flags`,
/// `block_size` and `rsi` come from sec5 payload offsets 16, 17 and 18..20
/// (`aec_pk.c` writes `sec5[21]=ccsdsFlags`, `sec5[22]=block size`,
/// `uint2(sec5+23)=rsi`; `unpk.c` reads exactly those three).
pub fn aec_samples(s5: &[u8], s7: &[u8], expect_n: usize) -> Result<Vec<i64>, GribErr> {
    if s5.len() < 20 {
        return Err(GribErr::Truncated);
    }
    let flags = s5[16];
    let block = s5[17];
    let rsi = be_u16(s5, 18);
    if block == 0 || rsi == 0 {
        return Err(GribErr::BadField("aec block size / rsi is zero"));
    }
    let params = rust_aec::AecParams::new(s5[14], block as u32, rsi as u32,
                                          rust_aec::flags_from_grib2_ccsds_flags(flags));
    let dec = rust_aec::decode(s7, params, expect_n);
    match dec {
        Err(_e) => { return Err(codec_fail("aec-decode")); }
        Ok(buf) => {
            let nbytes = ((s5[14] as usize) + 7) / 8;
            if buf.len() != nbytes * expect_n {
                return Err(codec_fail("aec-length"));
            }
            return Ok(bytes_to_ints(&buf[..], nbytes, expect_n));
        }
    }
}

// ---------------------------------------------------------------------------
// wgrib2-compatible data checksum (validation primitive)
// ---------------------------------------------------------------------------

/// Build the POSIX cksum table (generating polynomial 0x04C11DB7), identical to
/// `tools/wgrib2-full-src/grib2/wgrib2/crc32.c::init_crctab`.
// The 0xFFFFFFFF masks are no-ops on purpose: they document the C source's
// 32-bit wrap-around semantics that this port mirrors byte-for-byte.
#[allow(clippy::identity_op)]
fn cksum_table() -> Vec<u32> {
    const GEN: u32 = 0x04C11DB7;
    let mut r: Vec<u32> = Vec::<u32>::new();
    r.push(GEN);
    for i in 1 .. 8 {
        let prev = r[i - 1];
        let next: u32 = ((prev << 1) & 0xFFFFFFFF) ^ (if (prev & 0x80000000) != 0 { GEN } else { 0 });
        r.push(next);
    }
    let mut tab: Vec<u32> = Vec::<u32>::new();
    for m in 0 .. 256 {
        let rem: u32 = {
            let mut acc: u32 = 0;
            for i in 0 .. 8 {
                if ((m >> i) & 1) != 0 {
                    acc ^= r[i];
                }
            }
            acc & 0xFFFFFFFF
        };
        tab.push(rem);
    }
    return tab;
}

/// POSIX `cksum` CRC32 (MSB-first table driven, length folded in, complemented)
/// — the exact algorithm `wgrib2 -checksum` uses. Verified against the system
/// `cksum` utility and against `wgrib2 -checksum message` in tests.
#[allow(clippy::identity_op)]
pub fn posix_cksum(buf: &[u8]) -> u32 {
    let tab = cksum_table();
    let mut crc: u32 = 0;
    for b in buf {
        let idx: usize = (((crc >> 24) ^ (*b as u32)) & 0xFF) as usize;
        crc = ((crc << 8) & 0xFFFFFFFF) ^ tab[idx];
    }
    let mut n: usize = buf.len();
    while n != 0 {
        let idx2: usize = ((((crc >> 24) ^ ((n as u32) & 0xFF)) & 0xFF)) as usize;
        crc = ((crc << 8) & 0xFFFFFFFF) ^ tab[idx2];
        n >>= 8;
    }
    return (!crc) & 0xFFFFFFFF;
}

/// Grid width/height helpers (same accessors the decoder uses).
pub fn grid_nxi(g: &GridDef) -> usize {
    match g {
        GridDef::Regular(r) => { return r.nx as usize; }
        GridDef::Lambert(r) => { return r.nx as usize; }
    }
}

pub fn grid_nyi(g: &GridDef) -> usize {
    match g {
        GridDef::Regular(r) => { return r.ny as usize; }
        GridDef::Lambert(r) => { return r.ny as usize; }
    }
}

/// True when wgrib2 stores the array with rows reversed relative to the file
/// (file scans north-to-south: scanning-mode bit 0x40 clear).
pub fn rows_flipped(g: &GridDef) -> bool {
    match g {
        GridDef::Regular(r) => { return (r.scan_mode & 0x40) == 0; }
        GridDef::Lambert(r) => { return (r.scan_mode & 0x40) == 0; }
    }
}

/// True when wgrib2 stores the array with columns reversed relative to the
/// file (file scans east-to-west: scanning-mode bit 0x80 set).
pub fn cols_flipped(g: &GridDef) -> bool {
    match g {
        GridDef::Regular(r) => { return (r.scan_mode & 0x80) != 0; }
        GridDef::Lambert(r) => { return (r.scan_mode & 0x80) != 0; }
    }
}

/// CRC32 of a decoded grid exactly as `wgrib2 -checksum data` computes it.
///
/// wgrib2 hashes its NORMALIZED internal array, not the file's scan order:
/// `-text` dumps confirm it stores south-to-north rows and west-to-east columns
/// (it flips during read, and flips back when writing a section-7 stream, which
/// is why our decoder - and the JPEG/PNG/AEC streams themselves - stay in file
/// scan order). The normalization, expressed through the template 3.0 / 3.30
/// scanning-mode byte this repo's parser keeps:
///
/// * rows are reversed unless the file already stores south-to-north
///   (scanning-mode bit 0x40 set, i.e. latitude increases with j);
/// * columns are reversed when the file stores east-to-west
///   (scanning-mode bit 0x80 set, i.e. longitude decreases with i).
///
/// Masked points hold wgrib2's `UNDEFINED` sentinel (9.999e20f) rather than NaN,
/// matching `cksum((unsigned char *) data, sizeof(float) * ndata)`.
pub fn data_cksum_grid(values: Vec<f32>, g: &GridDef) -> u32 {
    let nxi: usize = grid_nxi(g);
    let nyi: usize = grid_nyi(g);
    let row_flip: bool = rows_flipped(g);
    let col_flip: bool = cols_flipped(g);
    if values.len() != nxi * nyi {
        return posix_cksum(&Vec::<u8>::new()[..]);
    }
    let undef: f32 = 9.999e20;
    let mut buf: Vec<u8> = Vec::<u8>::new();
    for jj in 0 .. nyi {
        let j: usize = if row_flip { nyi - 1 - jj } else { jj };
        for ii in 0 .. nxi {
            let i: usize = if col_flip { nxi - 1 - ii } else { ii };
            let v: f32 = values[j * nxi + i];
            let stored: f32 = if v.is_nan() { undef } else { v };
            let bits: u32 = f32_bits(stored);
            buf.push((bits & 0xFF) as u8);
            buf.push(((bits >> 8) & 0xFF) as u8);
            buf.push(((bits >> 16) & 0xFF) as u8);
            buf.push(((bits >> 24) & 0xFF) as u8);
        }
    }
    return posix_cksum(&buf[..]);
}

/// Unnormalized variant: hash the decoder's own scan-order array. Used for
/// twin-vs-twin equality where wgrib2's orientation convention is irrelevant.
pub fn data_cksum(values: Vec<f32>) -> u32 {
    return data_cksum_flat(&values[..]);
}

fn data_cksum_flat(values: &[f32]) -> u32 {
    let undef: f32 = 9.999e20;
    let mut buf: Vec<u8> = Vec::<u8>::new();
    for v in values {
        let stored: f32 = if v.is_nan() { undef } else { *v };
        let bits: u32 = f32_bits(stored);
        buf.push((bits & 0xFF) as u8);
        buf.push(((bits >> 8) & 0xFF) as u8);
        buf.push(((bits >> 16) & 0xFF) as u8);
        buf.push(((bits >> 24) & 0xFF) as u8);
    }
    return posix_cksum(&buf[..]);
}

/// IEEE f32 bit pattern, little-endian byte order matching x86 memory layout.
pub fn f32_bits(v: f32) -> u32 {
    return v.to_bits();
}

// ---------------------------------------------------------------------------
// error constructors
// ---------------------------------------------------------------------------

/// The codec backend could not parse the stream at all (corrupt, truncated or
/// rejected by the third-party decoder): distinct from "not supported".
fn codec_fail(reason: &'static str) -> GribErr {
    return GribErr::CodecFailure(reason);
}

/// The stream parsed but uses a construct this decoder deliberately does not
/// implement (see module docs and docs/DECISIONS.md).
fn codec_unsupported(reason: &'static str) -> GribErr {
    return GribErr::CodecUnsupported(reason);
}
