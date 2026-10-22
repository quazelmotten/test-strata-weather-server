//! On-disk storage format (spec §4.5): raw f32 little-endian, layout
//! `[level][y][x]`, small fixed header per .bin file. Reader side uses memmap2
//! (zero-copy); writer side goes through a temp file + REAL fsync + atomic rename
//! plus a directory fsync, so a query never sees a partial file and a crash
//! cannot resurrect a renamed-but-unlinked file.
//!
//! Durability note (corrected in V1.2 phase D): `flush()` alone is only a
//! userspace buffer flush (strace confirms it emits NO `fsync` syscall), which
//! is why every writer here follows it with [`fsync_path`]. That helper used to
//! bind libc `open`/`fsync`/`close` because an earlier phase concluded this
//! toolchain had "no fsync surface at all" — that claim was WRONG. A compile+
//! strace probe shows `std::fs::File` DOES have `sync_all()` here and it emits
//! a real `fsync(fd)`, and it works on a DIRECTORY handle too (`File::open(dir)`
//! → `fsync(4) = 0`). The libc bindings were therefore REPLACED by opening the
//! path and calling `sync_all()`: same syscalls (`openat` O_RDONLY → `fsync` →
//! `close`), same signature, same counter placement, one fewer FFI surface.
//!
//! ## V1.2 phase A: streaming writer + batched directory fsync
//!
//! [`write_bin_atomic_stream`] serializes header + planes THROUGH a fixed
//! [`WRITE_CHUNK_BYTES`] buffer straight into the temp file, so peak writer
//! memory is ONE chunk instead of the whole file (a GFS field file used to be
//! materialized a second time as a `Vec<u8>` before `write_all`). Planes arrive
//! as BORROWED slices through [`PlaneSource`], which lets a merge of L existing
//! levels plus K new ones cost O(L+K) passes and produce ONE rename instead of
//! K. Output bytes are BIT-IDENTICAL to the pre-change writer (pinned by
//! `tests/fixtures/writer_golden_v1.bin`).
//!
//! The field-bin writer no longer fsyncs the parent DIRECTORY (that was one
//! `fsync(2)` per rename). Callers record the directories they touched in a
//! [`TouchedDirs`] and `flush()` them ONCE — same crash rule (temp -> fsync file
//! -> rename, spec Sec 4.5), fewer syscalls. `write_bytes_atomic_pub` in wx-ingest
//! still fsyncs its own directory: it writes SINGLE small files (grid.bin,
//! manifest.json), where the extra fsync is not on the amplification path.
//!
//! Every durability-relevant action bumps a module counter
//! ([`counters_snapshot`]: renames, bytes written, fsyncs). Relaxed atomics:
//! cheap, and OUTSIDE the query hot path.
//!
//! ## Header layout V1.3 E0 (fixed 56 bytes + 4 bytes per level, all little-endian)
//!
//! ```text
//! offset  size  field
//! 0       8     magic ASCII "WXPTBIN1"
//! 8       4     version u32 (= BIN_VERSION = 2)
//! 12      4     nx u32
//! 16      4     ny u32
//! 20      2     nlevels u16
//! 22      1     flags: bit0 = scan_ns (row 0 is the La1/north side and rows
//!                 advance southward, i.e. the GFS/HRRR convention; see grid.rs
//!                 `scan_ns`), bit1 = winds_grid (grid-relative component flag
//!                 as stored at ingest; queries are uniform but the flag is
//!                 kept for round-tripping)
//! 23      1     stat_kind: STAT_INSTANTANEOUS(0) / STAT_ACCUMULATION(1) /
//!                 STAT_AVERAGE(2) / STAT_MAXIMUM(3) / STAT_MINIMUM(4) /
//!                 STAT_OTHER(5) — mirrors grib2::StatKind (V1.3 E0 addition)
//! 24      1     stat_processed u8 (0/1): PDT 4.8 statistical processing seen
//!                 at decode. THE ACCUMULATION GUARD READS THIS BYTE, not
//!                 manifest.json (V1.3 E0: metadata that guards an answer must
//!                 live in the file that carries the answer)
//! 25      3     reserved (must be 0)
//! 28      8     stat_begin_secs i64 LE, statistical window BEGIN relative to
//!                 the run time (0 for instantaneous fields)
//! 36      8     stat_end_secs i64 LE, window END relative to the run time
//! 44      8     stat_step_secs i64 LE, CT 4.11 step length in seconds
//! 52      4     reserved (must be 0)
//! 56      4*n   level coords, f32 LE, in CANONICAL ORDER:
//!                 - isobaric levels descending by pressure (1000, 925, ... hPa)
//!                 - every other level type ascending (height_above_ground,
//!                   msl, sigma, ...)
//! data    4*nx*ny per level, f32 LE, slab li at byte 56+4n + li*nx*ny*4,
//!         row-major [j][i] in STORED scan order (j follows the scan flag)
//! ```
//!
//! ### Version history
//!
//! * **v1** (through V1.2): fixed header was 24 bytes — magic, version, nx, ny,
//!   nlevels, flags, ONE reserved byte — then the level coords. Statistical
//!   process metadata lived ONLY in `manifest.json`, so a missing or corrupt
//!   manifest made a warm start treat accumulations as instantaneous and
//!   TIME-INTERPOLATE them (spec §6.3 violation, silent wrong answer). Fixed by
//!   bumping to v2 and moving the metadata here.
//! * **v2** (V1.3 phase E0): 56-byte fixed header as above. A v1 field file is
//!   REJECTED structurally (`HeaderError::BadVersion` → `open_field` reports
//!   `"bad bin version"`), and the warm-start rebuild turns that into the same
//!   loud STORAGE-FORMAT RECOVERY the grid.bin upgrade trap uses (delete the run
//!   dir, forget the model's ingest records, re-decode from source), so an old
//!   store can NEVER answer from lost metadata. See docs/DECISIONS.md V1.3-E0.
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
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Magic bytes of every .bin file ("WXPTBIN1").
pub const BIN_MAGIC: [u8; 8] = [0x57, 0x58, 0x50, 0x54, 0x42, 0x49, 0x4E, 0x31];

/// Current format version. Bumped 1 -> 2 in V1.3 phase E0 because the header
/// gained the PDT 4.8 statistical-process metadata (see module docs); a v1
/// field file cannot represent it, so refusing beats silently losing the
/// accumulation guard.
pub const BIN_VERSION: u32 = 2;

/// Fixed part of the header before the level coordinates (bytes).
pub const BIN_FIXED_HEADER: usize = 56;

/// One stored level coordinate (f32 LE bytes).
pub const BIN_LEVEL_BYTES: usize = 4;

// ---------------------------------------------------------------------------
// statistical-process metadata (V1.3 phase E0)
// ---------------------------------------------------------------------------

// `stat_kind` codes stored in the header. They MIRROR `grib2::StatKind` (which
// wx-core cannot depend on: grib2 lives in wx-ingest), and the name table below
// is the same one `pipeline::stat_kind_name` renders into manifest.json and
// `/v1/inventory`, so code <-> name round-trips exactly across all three.
pub const STAT_INSTANTANEOUS: u8 = 0;
pub const STAT_ACCUMULATION: u8 = 1;
pub const STAT_AVERAGE: u8 = 2;
pub const STAT_MAXIMUM: u8 = 3;
pub const STAT_MINIMUM: u8 = 4;
pub const STAT_OTHER: u8 = 5;

/// Canonical lower-case name of a stat-kind code ("instantaneous", ...).
pub fn stat_kind_name(code: u8) -> &'static str {
    match code {
        STAT_INSTANTANEOUS => { return "instantaneous"; }
        STAT_ACCUMULATION => { return "accumulation"; }
        STAT_AVERAGE => { return "average"; }
        STAT_MAXIMUM => { return "maximum"; }
        STAT_MINIMUM => { return "minimum"; }
        _ => { return "other"; }
    }
}

/// Code of a canonical stat-kind name; anything unrecognized maps to
/// `STAT_OTHER`, which still GUARDS time interpolation (fail closed).
pub fn stat_kind_code(name: &str) -> u8 {
    if name == "instantaneous" {
        return STAT_INSTANTANEOUS;
    }
    if name == "accumulation" {
        return STAT_ACCUMULATION;
    }
    if name == "average" {
        return STAT_AVERAGE;
    }
    if name == "maximum" {
        return STAT_MAXIMUM;
    }
    if name == "minimum" {
        return STAT_MINIMUM;
    }
    return STAT_OTHER;
}

/// Per-field-file statistical-process metadata (spec §4.2 "store the process
/// type AND the time range"). One .bin file IS one (param, level type, forecast
/// hour) group, so ONE record per file is exactly right. Begin/end/step are
/// SECONDS RELATIVE TO THE RUN TIME, matching what the index and manifest use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StatMeta {
    pub processed: bool,
    pub kind: u8, // one of the STAT_* codes above
    pub begin_secs: i64,
    pub end_secs: i64,
    pub step_secs: i64,
}

impl StatMeta {
    /// The metadata of a plain PDT 4.0 field (no guard, no range).
    pub fn instantaneous() -> StatMeta {
        return StatMeta {
            processed: false,
            kind: STAT_INSTANTANEOUS,
            begin_secs: 0,
            end_secs: 0,
            step_secs: 0,
        };
    }

    /// TRUE when a query must NOT interpolate this field in time (spec §6.3).
    /// Fail CLOSED: either signal alone is enough — a file that claims a stat
    /// KIND but forgot the flag (or vice versa) still gets guarded, because a
    /// rejected query beats a wrong number.
    pub fn guards_time(self: &Self) -> bool {
        return self.processed || self.kind != STAT_INSTANTANEOUS;
    }
}

#[derive(Clone, Debug)]
pub struct BinHeader {
    pub nx: u32,
    pub ny: u32,
    pub levels: Vec<f32>, // canonical order (see module docs)
    pub scan_ns: bool, // row 0 = north/La1 side, rows go south
    pub winds_grid: bool,
    /// PDT 4.8 statistical-process metadata (V1.3 E0): the accumulation guard
    /// reads THIS, from the durable file, never manifest.json.
    pub stat: StatMeta,
}

#[derive(Clone, Debug)]
pub enum HeaderError {
    Truncated,
    BadMagic,
    BadVersion,
    BadFlags,
    /// stat_kind / stat_processed byte out of range, or a reserved byte that
    /// is not zero (V1.3 E0).
    BadStat,
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

/// Append a little-endian i64 (two's complement; stored as its u64 bits).
pub fn push_le_i64(out: &mut Vec<u8>, v: i64) {
    let b = v as u64;
    for k in 0 .. 8 {
        out.push(((b >> (k * 8)) & 0xFF) as u8);
    }
}

/// Read a little-endian i64 at `off` (caller guarantees 8 readable bytes).
pub fn le_i64(b: &[u8], off: usize) -> i64 {
    let mut v: u64 = 0;
    for k in 0 .. 8 {
        v |= (b[off + k] as u64) << (k * 8);
    }
    return v as i64;
}

// ---------------------------------------------------------------------------
// header encode / validate
// ---------------------------------------------------------------------------

/// Serialize a header (not the data planes).
pub fn encode_header(hdr: &BinHeader) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::<u8>::new();
    for b in BIN_MAGIC {
        out.push(b);
    }
    push_le_u32(&mut out, BIN_VERSION);
    push_le_u32(&mut out, hdr.nx);
    push_le_u32(&mut out, hdr.ny);
    push_le_u16(&mut out, hdr.levels.len() as u16);
    let flags = (if hdr.scan_ns { 1 } else { 0 }) | (if hdr.winds_grid { 2 } else { 0 });
    out.push(flags as u8);
    // --- V1.3 E0 statistical-process metadata ------------------------------
    out.push(hdr.stat.kind);
    let sp: u8 = if hdr.stat.processed { 1 } else { 0 };
    out.push(sp);
    out.push(0); // reserved x3 (validated ZERO on read)
    out.push(0);
    out.push(0);
    push_le_i64(&mut out, hdr.stat.begin_secs);
    push_le_i64(&mut out, hdr.stat.end_secs);
    push_le_i64(&mut out, hdr.stat.step_secs);
    out.push(0); // reserved u32 (validated ZERO on read)
    out.push(0);
    out.push(0);
    out.push(0);
    for k in 0 .. hdr.levels.len() {
        push_le_f32(&mut out, hdr.levels[k]);
    }
    return out;
}

/// Parse + validate a header from the first bytes of a .bin file.
pub fn decode_header(b: &[u8]) -> Result<BinHeader, HeaderError> {
    if b.len() < BIN_FIXED_HEADER {
        return Err(HeaderError::Truncated);
    }
    for k in 0 .. 8 {
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
    // V1.3 E0: the stat bytes are part of the VALIDATED surface. A kind code
    // outside the table, a processed byte that is not 0/1, or a non-zero
    // reserved byte means the file was not written by this writer, so we refuse
    // it rather than guess at what the guard should have been.
    let kind = b[23];
    if kind > STAT_OTHER {
        return Err(HeaderError::BadStat);
    }
    let spb = b[24];
    if spb > 1 {
        return Err(HeaderError::BadStat);
    }
    if b[25] != 0 || b[26] != 0 || b[27] != 0 {
        return Err(HeaderError::BadStat);
    }
    if b[52] != 0 || b[53] != 0 || b[54] != 0 || b[55] != 0 {
        return Err(HeaderError::BadStat);
    }
    if b.len() < BIN_FIXED_HEADER + nlevels * BIN_LEVEL_BYTES {
        return Err(HeaderError::Truncated);
    }
    let mut levels: Vec<f32> = Vec::<f32>::new();
    for k in 0 .. nlevels {
        let off = BIN_FIXED_HEADER + k * BIN_LEVEL_BYTES;
        levels.push(bits_to_f32(le_u32(b, off)));
    }
    Ok(BinHeader {
        nx: le_u32(b, 12),
        ny: le_u32(b, 16),
        levels: levels,
        scan_ns: (flags & 1) != 0,
        winds_grid: (flags & 2) != 0,
        stat: StatMeta {
            processed: spb != 0,
            kind: kind,
            begin_secs: le_i64(b, 28),
            end_secs: le_i64(b, 36),
            step_secs: le_i64(b, 44),
        },
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

// Durability via std (V1.2 phase D correction). An earlier phase believed this
// toolchain exposed NO fsync surface and bound libc `open`/`fsync`/`close`
// through `extern "C"`. That was FALSE: `std::fs::File` has `sync_all()`, and
// verified with strace it emits exactly the same syscall sequence as the libc
// version did — `openat(AT_FDCWD, path, O_RDONLY|O_CLOEXEC)` then `fsync(fd)`,
// including for a DIRECTORY opened read-only (Linux accepts fsync on a directory
// fd). So the FFI bindings are GONE; behaviour (and the counter placement below)
// is unchanged. `File::flush()` alone would NOT be enough — it only flushes the
// userspace buffer and emits no syscall.

// ---------------------------------------------------------------------------
// instrumentation counters (regression gates now, /metrics later)
// ---------------------------------------------------------------------------

// Module-level relaxed atomics. They count DURABILITY ACTIONS performed by THIS
// library: successful atomic renames, bytes handed to the writer, and fsync
// syscalls attempted through [`fsync_path`] (file AND directory). wx-ingest's
// `write_bytes_atomic_pub` routes its rename/bytes/fsync accounting through the
// same accessors; `state.rs` keeps its own writer and is NOT counted (documented
// limitation of phase A). Tests read DELTAS around their own operations; the
// accessors below also serve /metrics later.
static RENAMES_TOTAL: AtomicU64 = AtomicU64::new(0);
static BYTES_WRITTEN_TOTAL: AtomicU64 = AtomicU64::new(0);
static FSYNCS_TOTAL: AtomicU64 = AtomicU64::new(0);
// per-process sequence appended to temp names for thread-safe uniqueness.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct StorageCounters {
    pub renames: u64,
    pub bytes_written: u64,
    pub fsyncs: u64
}

/// Read the three counters as a consistent-enough snapshot (relaxed reads: fine
/// for metrics and for test deltas taken around single-threaded operations).
pub fn counters_snapshot() -> StorageCounters {
    return StorageCounters {
        renames: RENAMES_TOTAL.load(Ordering::Relaxed),
        bytes_written: BYTES_WRITTEN_TOTAL.load(Ordering::Relaxed),
        fsyncs: FSYNCS_TOTAL.load(Ordering::Relaxed),
    };
}

/// Zero the counters (test helper; production code never calls it).
pub fn counters_reset() {
    RENAMES_TOTAL.store(0, Ordering::Relaxed);
    BYTES_WRITTEN_TOTAL.store(0, Ordering::Relaxed);
    FSYNCS_TOTAL.store(0, Ordering::Relaxed);
}

/// Count an atomic rename performed OUTSIDE this module (wx-ingest's byte
/// writer for grid.bin / manifest.json), so the metrics and the regression
/// gates stay honest about total write traffic.
pub fn count_rename() {
    let _r = RENAMES_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Count bytes handed to a writer outside this module.
pub fn count_bytes_written(n: u64) {
    let _b = BYTES_WRITTEN_TOTAL.fetch_add(n, Ordering::Relaxed);
}

/// fsync the inode behind `path` (a regular file OR a directory). Opening READ-
/// ONLY is enough for fsync. Returns Some(message) on failure (missing path,
/// EACCES, filesystems that reject directory fsync) so callers can decide
/// whether the write is good enough. Counter placement is UNCHANGED from the
/// libc version: a failed OPEN costs no fsync, an attempted fsync counts ONCE.
pub fn fsync_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty path".to_string());
    }
    let h = std::fs::File::open(path);
    match h {
        Err(_) => {
            return Err("open-for-fsync failed".to_string());
        }
        Ok(f) => {
            // Count the fsync we are about to attempt (the open already happened).
            let _fc = FSYNCS_TOTAL.fetch_add(1, Ordering::Relaxed);
            let r = f.sync_all();
            match r {
                Ok(()) => {
                    return Ok(());
                }
                Err(_) => {
                    return Err("sync_all failed".to_string());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// streaming atomic writer (V1.2 phase A): header + planes go THROUGH a fixed
// chunk buffer straight into the temp file; NO whole-file Vec<u8> is ever built.
// ---------------------------------------------------------------------------

/// Bytes buffered per `write_all` by the streaming writer. 256 KiB keeps the
/// writer's peak memory FLAT regardless of grid size (one GFS plane alone is
/// 4.1 MB) while staying far above the per-syscall break-even point.
pub const WRITE_CHUNK_BYTES: usize = 256 * 1024;

/// Borrowed source of the planes for [`write_bin_atomic_stream`]: yields the
/// planes in canonical level order (parallel to `hdr.levels`), each exactly
/// `nx*ny` f32 values, WITHOUT copying anything. This is what lets a merge of L
/// existing levels plus K new ones stream out in O(L+K) passes; wx-ingest
/// implements it over its merged `(level, plane)` vector, and
/// [`write_bin_atomic`] uses the built-in adapter over borrowed slices.
pub trait PlaneSource {
    /// Next plane, or None when exhausted. The writer stops after `count`
    /// planes and REJECTS a source that yields more or fewer.
    fn next_plane(self: &mut Self) -> Option<&[f32]>;
}

struct RefPlanes<'a> {
    pub planes: &'a [&'a [f32]],
    pub idx: usize
}

impl<'a> PlaneSource for RefPlanes<'a> {
    fn next_plane(self: &mut Self) -> Option<&[f32]> {
        if self.idx >= self.planes.len() {
            return None;
        }
        let r = self.planes[self.idx];
        self.idx += 1;
        return Some(r);
    }
}

/// Unique temp name `<path>.tmp.<pid>.<seq>`: pid separates PROCESSES, the
/// process-local sequence separates THREADS/retries, so concurrent writers of
/// the same field can never collide on the temp path (the old plain `.tmp`
/// could, and its doc even promised a pid it never appended). PUBLIC because
/// wx-ingest's byte writer (grid.bin / manifest.json) shares the scheme.
pub fn unique_tmp_path(path: &str) -> String {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    return format!("{0}.tmp.{1}.{2}", path, std::process::id(), n);
}

/// Write one .bin file atomically from a streaming plane source: header +
/// planes written through a [`WRITE_CHUNK_BYTES`] buffer into
/// `<path>.tmp.<pid>.<seq>` in the SAME directory, flush, REAL fsync of the
/// temp inode, then rename over the target. A reader either sees the OLD file
/// or the COMPLETE new one (spec Sec 4.5). Output bytes are BIT-IDENTICAL to
/// the pre-change whole-buffer writer (golden fixture test).
///
/// ANY failure path unlinks the temp file, so no `.tmp` litter survives a
/// failed write (create error, short/extra plane, write error, flush error,
/// fsync error, rename error).
///
/// NO parent-directory fsync here any more: callers batch those with
/// [`TouchedDirs`] (one fsync per touched directory instead of one per rename).
pub fn write_bin_atomic_stream<S: PlaneSource>(path: String, hdr: &BinHeader,
                                               count: usize, src: S) -> Result<(), String> {
    if count != hdr.levels.len() {
        return Err("plane count != level count".to_string());
    }
    let per_plane = (hdr.nx as usize) * (hdr.ny as usize);
    let tmp = unique_tmp_path(&path);
    // empty `err` means success; every failure branch sets it and the tail
    // below does the unlink-on-failure + error return in ONE place.
    let mut err: String = "".to_string();
    let head = encode_header(hdr);
    let f = std::fs::File::create(&tmp);
    match f {
        Err(_) => { err = format!("create failed: {0}", tmp); }
        Ok(mut h) => {
            let mut ok = true;
            let mut written: u64 = 0;
            let w1 = h.write_all(&head);
            if w1.is_err() {
                err = "write_all failed".to_string();
                ok = false;
            } else {
                written += head.len() as u64;
            }
            let mut buf: Vec<u8> = Vec::<u8>::new();
            let mut q = src;
            let mut nplanes: usize = 0;
            while ok && nplanes < count {
                let p = q.next_plane();
                match p {
                    None => {
                        err = "source yielded fewer planes than declared".to_string();
                        ok = false;
                        break;
                    }
                    Some(plane) => {
                        if plane.len() != per_plane {
                            err = "plane length != nx*ny".to_string();
                            ok = false;
                            break;
                        }
                        for k in 0 .. per_plane {
                            let bits = plane[k].to_bits();
                            buf.push((bits & 0xFF) as u8);
                            buf.push(((bits >> 8) & 0xFF) as u8);
                            buf.push(((bits >> 16) & 0xFF) as u8);
                            buf.push(((bits >> 24) & 0xFF) as u8);
                            if buf.len() >= WRITE_CHUNK_BYTES {
                                let wf = h.write_all(&buf);
                                if wf.is_err() {
                                    err = "write_all failed".to_string();
                                    ok = false;
                                    break;
                                }
                                written += buf.len() as u64;
                                buf.clear();
                            }
                        }
                        nplanes += 1;
                    }
                }
            }
            // a source that keeps going past `count` is a contract violation:
            // refuse rather than append stray planes.
            if ok {
                let extra = q.next_plane();
                if extra.is_some() {
                    err = "source yielded more planes than declared".to_string();
                    ok = false;
                }
            }
            if ok && !buf.is_empty() {
                let wf = h.write_all(&buf);
                if wf.is_err() {
                    err = "write_all failed".to_string();
                } else {
                    written += buf.len() as u64;
                    buf.clear();
                }
            }
            if err.is_empty() {
                // flush the userspace buffer, then fsync the INODE itself BEFORE
                // the rename makes the name visible (spec Sec 4.5).
                let fl = h.flush();
                if fl.is_err() {
                    err = "flush failed".to_string();
                }
            }
            if err.is_empty() {
                if let Err(e) = fsync_path(&tmp) {
                    err = format!("file fsync: {0}", e);
                }
            }
            if err.is_empty() {
                let _bw = BYTES_WRITTEN_TOTAL.fetch_add(written, Ordering::Relaxed);
            }
        }
    }
    if err.is_empty() {
        let r = std::fs::rename(&tmp, &path);
        if r.is_ok() {
            let _rn = RENAMES_TOTAL.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        err = "rename failed".to_string();
    }
    // unlink-on-any-failure (best effort: nothing to remove if create failed)
    let _rm = std::fs::remove_file(&tmp);
    return Err(err);
}

/// Thin wrapper over [`write_bin_atomic_stream`] keeping the historical
/// signature (`planes: &[&[f32]]`) so existing callers compile UNCHANGED.
/// NOTE behaviour change vs V1.1: no parent-directory fsync (callers batch it)
/// and the temp name now carries pid + sequence.
pub fn write_bin_atomic(path: String, hdr: &BinHeader, planes: &[&[f32]]) -> Result<(), String> {
    return write_bin_atomic_stream(path, hdr, planes.len(), RefPlanes { planes: planes, idx: 0 });
}

// ---------------------------------------------------------------------------
// directory fsync batching (V1.2 phase A)
// ---------------------------------------------------------------------------

/// Collects the DIRECTORIES touched by atomic writes so the caller fsyncs each
/// ONE exactly once instead of once per rename. `note` records the parent
/// directory of a written path (duplicates collapse); `flush` fsyncs every
/// distinct directory ONCE, clears the list, and reports the FIRST error.
/// Durability rule unchanged: file data is fsynced before its rename by the
/// writer; the directory fsync only has to happen before the process crashes.
pub struct TouchedDirs {
    pub dirs: Vec<String>
}

impl TouchedDirs {
    /// Record the parent directory of `file_path` (the file just written).
    pub fn note(self: &mut Self, file_path: String) {
        self.push_dir(dir_of_path(&file_path));
    }

    /// Record a directory directly (caller already knows it).
    pub fn note_dir(self: &mut Self, dir: String) {
        self.push_dir(dir);
    }

    /// How many DISTINCT directories are pending.
    pub fn pending_count(self: &Self) -> usize {
        return self.dirs.len();
    }

    /// fsync every pending directory ONCE (relaxed counter increments inside
    /// [`fsync_path`]), then clear the list. Returns the first error if any
    /// directory refused (e.g. a filesystem that rejects directory fsync).
    pub fn flush(self: &mut Self) -> Result<(), String> {
        let mut first: Option<String> = None;
        for k in 0 .. self.dirs.len() {
            let d = &self.dirs[k];
            if let Err(e) = fsync_path(d) {
                if first.is_none() {
                    first = Some(format!("dir fsync {0}: {1}", d, e));
                }
            }
        }
        self.dirs.clear();
        if let Some(e) = first {
            return Err(e);
        }
        return Ok(());
    }

    fn push_dir(self: &mut Self, dir: String) {
        for k in 0 .. self.dirs.len() {
            if self.dirs[k] == dir {
                return;
            }
        }
        self.dirs.push(dir);
    }
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
/// time (spec §5), never per request. The error strings are PART OF THE WARM-
/// START CONTRACT, not just diagnostics: `"bad bin version"` is what tells
/// `pipeline::ingest_run_dir` to run the storage-format recovery (delete the
/// run dir, forget the model's records, re-decode from source) instead of
/// indexing a store whose statistical metadata this build cannot read.
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
                        Err(HeaderError::BadVersion) => { return Err("bad bin version"); }
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

/// Read ONLY the format-version field of a .bin file (magic + version = the
/// first 12 bytes) without mapping it. Used by the warm-start upgrade check so
/// the recovery log can name the offending file AND the version it found; a
/// multi-megabyte plane never gets faulted in just to ask that question.
/// None = unreadable / too short / wrong magic (the caller then falls through
/// to `open_field`, which reports the structural failure).
pub fn bin_version_of_path(path: &str) -> Option<u32> {
    use std::io::Read;
    let f = std::fs::File::open(path);
    match f {
        Err(_) => { return None; }
        Ok(mut h) => {
            let mut b: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            let r = h.read_exact(&mut b);
            if r.is_err() {
                return None;
            }
            for k in 0 .. 8 {
                if b[k] != BIN_MAGIC[k] {
                    return None;
                }
            }
            return Some(le_u32(&b, 8));
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
