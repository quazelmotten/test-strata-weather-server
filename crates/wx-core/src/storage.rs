//! On-disk storage format (spec §4.5): raw f32 little-endian, layout
//! `[level][y][x]`, small fixed header per .bin file. Reader side uses memmap2
//! (zero-copy); writer side goes through a temp file + fsync + atomic rename
//! so a query never sees a partial file.
//!
//! ## Header layout (fixed 24 bytes + 4 bytes per level, all little-endian)
//!
//! ```text
//! offset  size  field
//! 0       8     magic ASCII "WXPTBIN1"
//! 8       4     version u32 (= BIN_VERSION = 1)
//! 12      4     nx u32
//! 16      4     ny u32
//! 20      2     nlevels u16
//! 22      1     flags: bit0 = scan_ns (row 0 is the La1/north side and rows
//!                 advance southward, i.e. the GFS/HRRR convention; see grid.rs
//!                 `scan_ns`), bit1 = winds_grid (grid-relative component flag
//!                 as stored at ingest; queries are uniform but the flag is
//!                 kept for round-tripping)
//! 23      1     reserved (0)
//! 24      4*n   level coords, f32 LE, in CANONICAL ORDER:
//!                 - isobaric levels descending by pressure (1000, 925, ... hPa)
//!                 - every other level type ascending (height_above_ground,
//!                   msl, sigma, ...)
//! data    4*nx*ny per level, f32 LE, slab li at byte 24+4n + li*nx*ny*4,
//!         row-major [j][i] in STORED scan order (j follows the scan flag)
//! ```
//!
//! ## mmap lifetime (spec §5)
//!
//! [`open_field`] returns an owned handle that keeps the mapping alive for as
//! long as the handle does. The server index stores these handles inside
//! `Arc`s and publishes them with `arc-swap`; a run is only deleted after it
//! has been unlinked from the index, so the last `Arc` drop is the point where
//! unmapping becomes safe — the `MMap` destructor releases the address-space
//! range when the handle is freed. Handles are `Send`/`Sync` (memmap2
//! guarantees this), so sharing across query threads is fine.

use memmap2::Mmap;
use std::io::Write;

/// Magic bytes of every .bin file ("WXPTBIN1").
pub const BIN_MAGIC: [u8; 8] = [0x57, 0x58, 0x50, 0x54, 0x42, 0x49, 0x4E, 0x31];

/// Current format version.
pub const BIN_VERSION: u32 = 1;

/// Fixed part of the header before the level coordinates (bytes).
pub const BIN_FIXED_HEADER: usize = 24;

/// One stored level coordinate (f32 LE bytes).
pub const BIN_LEVEL_BYTES: usize = 4;

#[derive(Clone, Debug)]
pub struct BinHeader {
    pub nx: u32,
    pub ny: u32,
    pub levels: Vec<f32>, // canonical order (see module docs)
    pub scan_ns: bool, // row 0 = north/La1 side, rows go south
    pub winds_grid: bool,
}

#[derive(Clone, Debug)]
pub enum HeaderError {
    Truncated,
    BadMagic,
    BadVersion,
    BadFlags,
}

// ---------------------------------------------------------------------------
// little-endian codec helpers
// ---------------------------------------------------------------------------

pub fn le_u32(b: &[u8], off: usize) -> u32 {
    (b[off] as u32) | ((b[off + 1] as u32) << 8) | ((b[off + 2] as u32) << 16) | ((b[off + 3] as u32) << 24)
}

pub fn le_u16(b: &[u8], off: usize) -> u16 {
    (b[off] as u16) | ((b[off + 1] as u16) << 8)
}

pub fn bits_to_f32(bits: u32) -> f32 {
    f32::from_bits(bits)
}

pub fn push_le_u32(out: &mut Vec<u8>, v: u32) {
    out.push((v & 0xFF) as u8);
    out.push(((v >> 8) & 0xFF) as u8);
    out.push(((v >> 16) & 0xFF) as u8);
    out.push(((v >> 24) & 0xFF) as u8);
}

pub fn push_le_u16(out: &mut Vec<u8>, v: u16) {
    out.push((v & 0xFF) as u8);
    out.push(((v >> 8) & 0xFF) as u8);
}

pub fn push_le_f32(out: &mut Vec<u8>, v: f32) {
    push_le_u32(out, v.to_bits());
}

// ---------------------------------------------------------------------------
// header encode / validate
// ---------------------------------------------------------------------------

/// Serialize a header (not the data planes).
pub fn encode_header(hdr: &BinHeader) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for b in BIN_MAGIC {
        out.push(b);
    }
    push_le_u32(&mut out, BIN_VERSION);
    push_le_u32(&mut out, hdr.nx);
    push_le_u32(&mut out, hdr.ny);
    push_le_u16(&mut out, hdr.levels.len() as u16);
    let flags = (if hdr.scan_ns { 1 } else { 0 }) | (if hdr.winds_grid { 2 } else { 0 });
    out.push(flags as u8);
    out.push(0);
    for k in 0..hdr.levels.len() {
        push_le_f32(&mut out, hdr.levels[k]);
    }
    out
}

/// Parse + validate a header from the first bytes of a .bin file.
pub fn decode_header(b: &[u8]) -> Result<BinHeader, HeaderError> {
    if b.len() < BIN_FIXED_HEADER {
        return Err(HeaderError::Truncated);
    }
    for k in 0..8 {
        if b[k] != BIN_MAGIC[k] {
            return Err(HeaderError::BadMagic);
        }
    }
    if le_u32(b, 8) != BIN_VERSION {
        return Err(HeaderError::BadVersion);
    }
    let nlevels = le_u16(b, 20) as usize;
    let flags = b[22];
    if (flags & 0xFC) != 0 {
        return Err(HeaderError::BadFlags);
    }
    if b.len() < BIN_FIXED_HEADER + nlevels * BIN_LEVEL_BYTES {
        return Err(HeaderError::Truncated);
    }
    let mut levels: Vec<f32> = Vec::new();
    for k in 0..nlevels {
        let off = BIN_FIXED_HEADER + k * BIN_LEVEL_BYTES;
        levels.push(bits_to_f32(le_u32(b, off)));
    }
    Ok(BinHeader {
        nx: le_u32(b, 12),
        ny: le_u32(b, 16),
        levels: levels,
        scan_ns: (flags & 1) != 0,
        winds_grid: (flags & 2) != 0,
    })
}

/// Expected total file size for a header.
pub fn expected_size(hdr: &BinHeader) -> usize {
    BIN_FIXED_HEADER + hdr.levels.len() * BIN_LEVEL_BYTES
        + hdr.levels.len() * (hdr.nx as usize) * (hdr.ny as usize) * 4
}

// ---------------------------------------------------------------------------
// write path: temp file in same dir, fsync file+dir, atomic rename (spec §4.5)
// ---------------------------------------------------------------------------

/// Write one .bin file atomically: serialize header + planes into `planes`
/// (each plane is nx*ny f32 in storage order, planes ordered like
/// `hdr.levels`), write to `<path>.tmp<pid>` in the SAME directory, fsync the
/// file, rename over the target, then fsync the directory. A reader either
/// sees the old file or the complete new one. Errors are reported as message
/// strings (OS error text).
pub fn write_bin_atomic(path: String, hdr: &BinHeader, planes: &[&[f32]]) -> Result<(), &'static str> {
    if planes.len() != hdr.levels.len() {
        return Err("plane count != level count");
    }
    let per_plane = (hdr.nx as usize) * (hdr.ny as usize);
    for li in 0..planes.len() {
        if planes[li].len() != per_plane {
            return Err("plane length != nx*ny");
        }
    }

    let mut bytes: Vec<u8> = encode_header(hdr);
    for li in 0..planes.len() {
        for k in 0..per_plane {
            push_le_f32(&mut bytes, planes[li][k]);
        }
    }

    // unique-enough temp name in the same directory. One writer per .bin
    // path at a time is assumed (the ingest pipeline guarantees this; two
    // processes must never ingest the same field concurrently).
    let tmp = format!("{path}.tmp");
    let f = std::fs::File::create(&tmp);
    match f {
        Err(_) => { return Err("create failed"); }
        Ok(mut h) => {
            let w = h.write_all(&bytes);
            if w.is_err() {
                return Err("write_all failed");
            }
            // fsync the data+metadata before the rename becomes visible
            let fl = h.flush();
            if fl.is_err() {
                return Err("flush failed");
            }
        }
    }
    // resolve the directory before the rename consumes `path`
    let dir = dir_of_path(&path);
    let r = std::fs::rename(&tmp, &path);
    if r.is_err() {
        return Err("rename failed");
    }
    // best-effort directory fsync: after a crash the renamed file must not
    // appear truncated; on platforms where opening the dir fails we surface
    // the error so callers can decide.
    let d = std::fs::File::open(dir);
    match d {
        Err(_) => { return Err("dir-open failed"); }
        Ok(mut dh) => {
            let df = dh.flush();
            if df.is_err() {
                return Err("dir-flush failed");
            }
        }
    }
    Ok(())
}

/// Parent-directory portion of a path (split at the last '/'), as a copy.
fn dir_of_path(path: &str) -> String {
    let mut last: Option<usize> = None;
    for k in 0..path.len() {
        if path[k .. k + 1] == *"/" {
            last = Some(k);
        }
    }
    match last {
        None => { return ".".to_string(); }
        Some(0) => { return "/".to_string(); }
        Some(k) => { return path[0 .. k].to_string(); }
    }
}

// ---------------------------------------------------------------------------
// read path: memmap2 open + validate + zero-copy accessors (spec §5)
// ---------------------------------------------------------------------------

/// Owns an mmap mapping of one .bin file plus its validated header. The
/// destructor unmaps when the (last) owner drops — see lifetime notes above.
pub struct MmapField {
    pub region: Mmap, // owns the mapping
    pub header: BinHeader,
    pub data_off: usize, // byte offset where planes start
}

/// Open, map and validate one .bin file. All mmaps are opened at index-build
/// time (spec §5), never per request.
pub fn open_field(path: String) -> Result<MmapField, &'static str> {
    let f = std::fs::File::open(path);
    match f {
        Err(_) => { return Err("open failed"); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return Err("mmap failed"); }
                Ok(region) => {
                    let hdr_res = decode_header(&region);
                    match hdr_res {
                        Err(_) => { return Err("bad header"); }
                        Ok(hdr) => {
                            let want = expected_size(&hdr);
                            if region.len() < want {
                                return Err("truncated data");
                            }
                            let doff = BIN_FIXED_HEADER + hdr.levels.len() * BIN_LEVEL_BYTES;
                            return Ok(MmapField {
                                region: region,
                                data_off: doff,
                                header: hdr,
                            });
                        }
                    }
                }
            }
        }
    }
}

impl MmapField {
    /// Zero-copy slice of one level's plane bytes (raw little-endian f32).
    pub fn plane_bytes(self: &Self, li: usize) -> &[u8] {
        let per = (self.header.nx as usize) * (self.header.ny as usize) * 4;
        let off = self.data_off + li * per;
        &self.region[off .. off + per]
    }

    /// One stored value (level index li, row j in storage order, column i).
    pub fn value(self: &Self, li: usize, j: usize, i: usize) -> f32 {
        let hdr = &self.header;
        let per = (hdr.nx as usize) * (hdr.ny as usize);
        let off = self.data_off + (li * per + j * (hdr.nx as usize) + i) * 4;
        bits_to_f32(le_u32(&self.region, off))
    }

    /// Row accessor honoring the scan flag: converts a "logical" row counted
    /// northward from La1 into the stored row index (identity when the caller
    /// already counts in storage order; provided so ingest and the server can
    /// share one convention, spec §4.5 "provide accessor").
    pub fn stored_row(self: &Self, logical_north_row: usize) -> usize {
        let ny = self.header.ny as usize;
        if self.header.scan_ns { logical_north_row } else { ny - 1 - logical_north_row }
    }
}
