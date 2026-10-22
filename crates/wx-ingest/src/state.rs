//! Persistent ingest record (spec §4.1): one JSON file under
//! `<data_dir>/state/files.json` mapping source path -> {size, mtime_ns,
//! content hash}. A restart must not re-ingest unchanged files; the same path
//! with a different content hash MUST re-ingest. The state file itself is
//! written atomically (temp + fsync + rename), mirroring the .bin policy.
//!
//! Content hash: FNV-1a 64 over first 64 KiB ++ last 64 KiB (whole file once
//! when smaller than 128 KiB) — spec §4.1 "first/last 64 KiB is fine".
//! V1.3 phase E3a: the windows are read with plain `File` + `seek` +
//! `read_exact` (pread semantics), NOT mmap — a producer truncating a
//! CANDIDATE file in place (before the stability verdict exists) could
//! otherwise SIGBUS the ingest thread mid-page-fault, which no error handling
//! can catch. The digest bytes and the 16-hex output format are UNCHANGED
//! (`content_hash_bytes` over an equivalent whole-file buffer produces the
//! identical string; pinned by `tests/hash_compat.rs`), so records written by
//! older builds keep matching after an upgrade.
//!
//! Persistence cadence (V1.3 phase E3a): mutations set a DIRTY flag;
//! [`StateStore::save`] is a cheap no-op while clean, so the pipeline persists
//! ONCE PER DECODE CHUNK plus at end of round instead of after every single
//! file (the old per-file save made durable state O(N²) bytes written and
//! fsynced across thousands of files). A FAILED save leaves the store dirty
//! for the next opportunity; worst case a crash loses at most one chunk's
//! records, which re-ingest idempotently.

use serde_json::from_str;
use wx_core::storage::{fsync_path};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::os::unix::fs::MetadataExt;
use std::fs::metadata;
use std::fs::read_to_string;
use std::sync::atomic::{AtomicU64, Ordering};


/// Bytes of head/tail window hashed.
pub const HASH_WINDOW: usize = 65536;

// ---------------------------------------------------------------------------
// V1.3 phase E3a STATE-WRITE INSTRUMENTATION
// ---------------------------------------------------------------------------
// The wx-core storage counters deliberately EXCLUDE this writer (see
// write_bytes_atomic docs), which made the phase-D write-amplification
// headline blind to state traffic. These dedicated relaxed counters close the
// gap WITHOUT touching the field-path counters: /metrics renders them as
// wxpoint_state_writes_total / wxpoint_state_bytes_written, and the batched
// save cadence is what makes the numbers small.

static STATE_WRITES_TOTAL: AtomicU64 = AtomicU64::new(0);
static STATE_BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Process-wide (successful state-file saves, bytes serialized) since boot.
pub fn state_counters_snapshot() -> (u64, u64) {
    return (STATE_WRITES_TOTAL.load(Ordering::Relaxed),
            STATE_BYTES_WRITTEN.load(Ordering::Relaxed));
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct FileRec {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: String, // 16 hex chars
    /// Record KIND (V1.2 phase B poison quarantine):
    ///   0 = OK      — ingested successfully; skipped until content changes.
    ///   1 = IGNORED — magic-byte precheck failed (NOT a GRIB2 stream, e.g. a
    ///                 `.idx` sidecar matching the model glob): never decoded,
    ///                 logged ONCE, never retried until content changes.
    ///   2 = FAILED  — decode error; quarantined WITH its content hash so a
    ///                 corrupt file is not re-decoded every scan round; retried
    ///                 only when the content hash changes. V1.3 phase E3a: a
    ///                 TRANSIENT I/O failure reaches this kind only after
    ///                 IO_FAIL_ESCALATION_ROUNDS consecutive failed rounds
    ///                 ("repeated I/O failure"); such records may carry an
    ///                 EMPTY hash (the content was never readable), which makes
    ///                 them self-clear once reading works again.
    /// `#[serde(default)]` keeps pre-V1.2 `files.json` (no `kind` key) loadable:
    /// absent means 0 = OK, exactly what those records always were.
    #[serde(default)]
    pub kind: u8,
}

/// Record-kind constants (kept as named values: the pipeline and tests both
/// speak in them instead of bare integers).
pub const REC_OK: u8 = 0;
pub const REC_IGNORED: u8 = 1;
pub const REC_FAILED: u8 = 2;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct StateDoc {
    pub version: u32,
    pub recs: Vec<FileRec>,
}

#[derive(Clone, Debug)]
pub struct StateStore {
    pub dir_path: String, // <data_dir>/state
    pub file_path: String, // <data_dir>/state/files.json
    pub recs: Vec<FileRec>,
    /// V1.2 phase C STATE HYGIENE: path -> position in `recs`, so [`StateStore.find`]
    /// is O(1) instead of a linear scan over every file ever ingested (a busy
    /// ingest dir can hold tens of thousands of records, and `scan_round` calls
    /// `find` once per candidate). `recs` stays the SERIALIZED ORDER and the
    /// on-disk format is UNCHANGED (`save()` still walks `recs`, `kind` included);
    /// `idx` is derived state, rebuilt by `reindex()` after every structural
    /// change (load / upsert / drop_under / prune_missing).
    pub idx: HashMap<String, usize>,
    /// V1.3 phase E3a BATCHED PERSISTENCE: true whenever `recs` diverged from
    /// the last successfully persisted image. Every mutation sets it; [`StateStore::save`]
    /// clears it ONLY on success, so a failed save keeps the store dirty and the
    /// next save opportunity retries. While clean, `save()` returns Ok without
    /// touching the disk at all (no clone, no serialize, no fsync) — that is what
    /// makes the per-chunk + per-round save cadence cheap.
    pub dirty: bool,
}

impl StateStore {
    /// Load from disk (empty when the file does not exist yet).
    pub fn load(state_dir: String) -> Result<StateStore, String> {
        let file = join_path(&state_dir, "files.json");
        let mut st = StateStore {
            dir_path: state_dir,
            file_path: file.to_string(),
            recs: Vec::<FileRec>::new(),
            idx: HashMap::<String, usize>::new(),
            dirty: false,
        };
        match read_to_string(&file) {
            Err(_) => { st.reindex(); return Ok(st); }
            Ok(text) => {
                match from_str::<StateDoc>(&text) {
                    Err(e) => { return Err(format!("bad state file {}: {}", file, e)); }
                    Ok(doc) => {
                        for r in &doc.recs {
                            st.recs.push(r.clone());
                        }
                        st.reindex();
                        return Ok(st);
                    }
                }
            }
        }
    }

    /// Rebuild the path index from `recs` (last write wins for duplicate paths,
    /// which `upsert` never creates but a hand-edited state file might).
    pub fn reindex(self: &mut Self) {
        self.idx = HashMap::<String, usize>::new();
        for k in 0 .. self.recs.len() {
            self.idx.insert(self.recs[k].path.to_string(), k);
        }
    }

    /// Look up the record for one source path. O(1) since V1.2 phase C (was a
    /// linear scan over every record); semantics identical.
    pub fn find(self: &Self, path: String) -> Option<FileRec> {
        let pos = self.idx.get(&path);
        match pos {
            None => { return None; }
            Some(pi) => {
                let at = *pi;
                if at >= self.recs.len() {
                    return None; // defensive: index must never outrun `recs`
                }
                return Some(self.recs[at].clone());
            }
        }
    }

    pub fn upsert(self: &mut Self, rec: FileRec) {
        self.dirty = true;
        let pos = self.idx.get(&rec.path);
        match pos {
            Some(pi) => {
                let at = *pi;
                if at < self.recs.len() {
                    self.recs[at].size = rec.size;
                    self.recs[at].mtime_ns = rec.mtime_ns;
                    self.recs[at].hash = rec.hash;
                    self.recs[at].kind = rec.kind;
                    return;
                }
            }
            None => {}
        }
        self.idx.insert(rec.path.to_string(), self.recs.len());
        self.recs.push(rec);
    }

    /// Forget every record whose path lies under `prefix` (V1.2 phase B0).
    /// Used by the storage-format recovery path in `pipeline::ingest_run_dir`:
    /// when a run directory has to be DISCARDED because its grid.bin belongs to
    /// an older GRID_VERSION, the source files that produced it must become
    /// eligible again, otherwise `scan_round` would skip them as "unchanged"
    /// forever and the model would stay EMPTY. Matching is on `<prefix>/` so a
    /// sibling directory whose name merely STARTS with the prefix is untouched.
    pub fn drop_under(self: &mut Self, prefix: &str) -> usize {
        let mut kept: Vec<FileRec> = Vec::<FileRec>::new();
        let mut n = 0usize;
        for r in &self.recs {
            if under_prefix(&r.path, prefix) {
                n += 1;
                continue;
            }
            kept.push(r.clone());
        }
        if n > 0 {
            self.recs = kept;
            self.reindex();
            self.dirty = true;
        }
        return n;
    }

    /// V1.2 phase C STATE HYGIENE: forget records whose SOURCE FILE no longer
    /// exists under `prefix` (upstream deleted a product after ingest). Without
    /// this the record list grows monotonically forever: every ingested file of
    /// every run keeps a row, and a retention-pruned run whose tree AND sources
    /// are gone still costs memory plus a `find` hit per stale candidate name.
    /// BOUNDED by construction: ONE stat() per record located under `prefix`,
    /// called ONCE per model per scan round (`pipeline::scan_round`), and only
    /// records matching that model's ingest dir are touched. Returns the number
    /// of records dropped; the caller logs it. Records for files that merely
    /// FAILED/IGNORED are KEPT (their existence is what makes the quarantine
    /// stick), so this never re-arms a poisoned file.
    pub fn prune_missing(self: &mut Self, prefix: &str) -> usize {
        let mut kept: Vec<FileRec> = Vec::<FileRec>::new();
        let mut n = 0usize;
        for r in &self.recs {
            if under_prefix(&r.path, prefix) {
                match stat_size_mtime(&r.path) {
                    Err(_) => {
                        n += 1;
                        continue;
                    }
                    Ok(_) => {}
                }
            }
            kept.push(r.clone());
        }
        if n > 0 {
            self.recs = kept;
            self.reindex();
            self.dirty = true;
        }
        return n;
    }

    /// Persist atomically. V1.3 phase E3a: a cheap NO-OP while the store is
    /// clean (no clone, no serialize, no fsync), so the pipeline can offer a
    /// save at every opportunity (per chunk, per round, before prune) and pay
    /// only when something actually changed. On SUCCESS the dirty flag clears;
    /// on FAILURE it STAYS SET, so the next opportunity retries the same
    /// records. Returns Err verbatim for the caller to log + count.
    pub fn save(self: &mut Self) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        let mut copied: Vec<FileRec> = Vec::<FileRec>::new();
        for r in &self.recs {
            copied.push(r.clone());
        }
        let doc = StateDoc { version: 1, recs: copied };
        let text = serialize_text(&doc);
        let res = write_bytes_atomic(&self.file_path, &text);
        match res {
            Err(e) => { return Err(e); }
            Ok(_) => {}
        }
        // V1.3 phase E3a instrumentation: state traffic was deliberately OUT of
        // the wx-core storage counters (see write_bytes_atomic docs), which made
        // the 1.0000x write-amplification headline blind to it. These dedicated
        // relaxed counters report it separately (wxpoint_state_writes_total /
        // wxpoint_state_bytes_written on /metrics).
        let _sw = STATE_WRITES_TOTAL.fetch_add(1, Ordering::Relaxed);
        let _sb = STATE_BYTES_WRITTEN.fetch_add(text.len() as u64, Ordering::Relaxed);
        self.dirty = false;
        return Ok(());
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

pub fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        return format!("/{}", name);
    }
    return format!("{}/{}", dir, name);
}

/// True when `path` lies UNDER `dir` (matched on `<dir>/`, so a sibling whose
/// name merely STARTS with the prefix does not match — the same rule phase B0
/// used inside `drop_under`).
fn under_prefix(path: &str, dir: &str) -> bool {
    let needle = format!("{0}/", dir);
    if path.len() < needle.len() {
        return false;
    }
    return path[0 .. needle.len()] == *needle.as_str();
}

/// FNV-1a 64 parameters (split into feed/finish so the pread window reader in
/// `content_hash_file` can chain the SAME arithmetic without materializing a
/// whole-file buffer — identical digest, identical output format).
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

#[allow(clippy::needless_return)]
fn fnv_feed(mut h: u64, bytes: &[u8]) -> u64 {
    for k in 0 .. bytes.len() {
        h = (h ^ (bytes[k] as u64)).wrapping_mul(FNV_PRIME);
    }
    return h;
}

#[allow(clippy::needless_return)]
fn fnv_finish(h: u64) -> String {
    let mut v = h;
    v = v ^ (v  >> 33);
    v = v.wrapping_mul(0xff51afd7ed558ccd);
    v = v ^ (v  >> 33);
    return format!("{:016x}", v);
}

/// FNV-1a 64 over the head/tail windows of a whole-file buffer.
pub fn content_hash_bytes(bytes: &[u8]) -> String {
    let n = bytes.len();
    let mut h = FNV_OFFSET;
    if n <= HASH_WINDOW * 2 {
        h = fnv_feed(h, bytes);
    } else {
        h = fnv_feed(h, &bytes[0 .. HASH_WINDOW]);
        h = fnv_feed(h, &bytes[n - HASH_WINDOW .. n]);
    }
    return fnv_finish(h);
}

/// Hash a file WITHOUT mmap (V1.3 phase E3a): plain `File` + `seek` +
/// `read_exact` over the first/last 64 KiB windows (whole file read once when
/// it is at most 128 KiB). This runs on EVERY CANDIDATE, including files still
/// being written whose size may change under us before the stability verdict
/// exists — an mmap of such a file can SIGBUS the reading thread when the
/// producer truncates in place (a signal no error handling catches), while a
/// plain read just returns fewer bytes or UnexpectedEof. The produced string
/// is BYTE-IDENTICAL to the old mmap version for unchanged content (pinned by
/// `tests/hash_compat.rs`), so records written by older builds keep matching.
/// A shrink race yields Err (retry next round) or a digest of what was really
/// read — never a panic, never a torn page mix.
pub fn content_hash_file(path: &str) -> Result<String, String> {
    let f = File::open(&path);
    match f {
        Err(_) => { return Err("open failed".to_string()); }
        Ok(mut h) => {
            let md = h.metadata();
            match md {
                Err(_) => { return Err("metadata failed".to_string()); }
                Ok(info) => {
                    let n = info.len();
                    if n <= (HASH_WINDOW as u64) * 2 {
                        // Whole file fits the small path: read it and reuse the
                        // buffer digest EXACTLY as before.
                        let mut buf: Vec<u8> = Vec::<u8>::new();
                        let r = h.read_to_end(&mut buf);
                        match r {
                            Err(_) => { return Err("read failed".to_string()); }
                            Ok(_c) => {}
                        }
                        return Ok(content_hash_bytes(&buf));
                    }
                    let mut head = vec![0u8; HASH_WINDOW];
                    let rh = h.read_exact(&mut head);
                    if rh.is_err() {
                        return Err("read head window failed".to_string());
                    }
                    let seek_r = h.seek(SeekFrom::Start(n - (HASH_WINDOW as u64)));
                    match seek_r {
                        Err(_) => { return Err("seek failed".to_string()); }
                        Ok(_) => {}
                    }
                    let mut tail = vec![0u8; HASH_WINDOW];
                    let rt = h.read_exact(&mut tail);
                    if rt.is_err() {
                        return Err("read tail window failed".to_string());
                    }
                    let mut hh = FNV_OFFSET;
                    hh = fnv_feed(hh, &head);
                    hh = fnv_feed(hh, &tail);
                    return Ok(fnv_finish(hh));
                }
            }
        }
    }
}

/// Magic-byte precheck (V1.2 phase B): true when the file starts with ASCII
/// `GRIB`. Cheap because it inspects FOUR BYTES, and it runs BEFORE any decode
/// so a non-GRIB candidate that matches a model's glob (a `.idx`/`.xml`
/// sidecar delivered next to the product is the common case) can NEVER reach
/// the decoder. V1.3 phase E3a reads those four bytes with a plain `read`
/// instead of an mmap (same SIGBUS rationale as `content_hash_file`). Returns
/// Err ONLY when the file cannot be opened or the read itself fails (both are
/// retryable I/O conditions the pipeline handles as such); a file shorter than
/// 4 bytes is a definitive Ok(false) — not GRIB, record it.
pub fn head_is_grib(path: &str) -> Result<bool, String> {
    let f = File::open(&path);
    match f {
        Err(_) => { return Err("open failed".to_string()); }
        Ok(mut h) => {
            let mut b = [0u8; 4];
            let mut got = 0usize;
            while got < 4 {
                let r = h.read(&mut b[got .. 4]);
                match r {
                    Err(_) => { return Err("read failed".to_string()); }
                    Ok(0) => { break; }
                    Ok(k) => { got += k; }
                }
            }
            if got < 4 {
                return Ok(false);
            }
            return Ok(b[0] == 0x47 && b[1] == 0x52 &&
                       b[2] == 0x49 && b[3] == 0x42);
        }
    }
}

/// stat() surface used for stability + records: (size, mtime ns since epoch).
pub fn stat_size_mtime(path: &str) -> Result<(u64, i64), String> {
    match metadata(&path) {
        Err(_) => { return Err("stat failed".to_string()); }
        Ok(md) => {
            let ns = (md.mtime() as i64) * 1000000000 + (md.mtime_nsec() as i64);
            return Ok((md.size(), ns));
        }
    }
}

/// Wall-clock nanoseconds since epoch via the mtime of a freshly created
/// marker file (this dialect lacks a direct clock API; the marker reflects the
/// same clock the stability rule compares against).
pub fn now_ns_marker(dir: &str) -> i64 {
    let p = join_path(dir, ".wxpoint-clock");
    let f = File::create(&p);
    match f {
        Err(_) => { return 0; }
        Ok(mut h) => {
            let _w = h.write_all(&[].to_vec());
            match stat_size_mtime(&p) {
                Err(_) => { return 0; }
                Ok(t) => { return t.1; }
            }
        }
    }
}

/// Serialize a state doc to JSON text. Paths are ingest paths; quotes and
/// backslashes escaped, everything else passed through.
pub fn serialize_text(doc: &StateDoc) -> String {
    let mut out = "{\"version\":".to_string();
    out = format!("{0}{1}", out, doc.version);
    out = format!("{0}{1}", out, ",\"recs\":[");
    for k in 0 .. doc.recs.len() {
        if k > 0 {
            out = format!("{0}{1}", out, ",");
        }
        let r = &doc.recs[k];
        out = format!("{0}{1}", out, esc_rec(r));
    }
    out = format!("{0}]}}", out);
    return out;
}

fn esc_rec(r: &FileRec) -> String {
    return format!("{{\"path\":\"{}\",\"size\":{},\"mtime_ns\":{},\"hash\":\"{}\",\"kind\":{}}}",
        esc_json_str(&r.path), r.size, r.mtime_ns, r.hash, r.kind);
}

/// Parse state JSON text written by serialize_text.
pub fn deserialize_text(text: &str) -> Result<StateDoc, String> {
    match from_str::<StateDoc>(&text) {
        Err(e) => { return Err(e.to_string()); }
        Ok(d) => { return Ok(d); }
    }
}

/// Atomic byte writer: `<path>.tmp` in same dir, write, flush, REAL fsync of
/// the temp inode, rename, then REAL fsync of the parent directory. Uses
/// `wx_core::storage::fsync_path` (which is `File::open(path)` + `sync_all()`
/// since V1.2 phase D — `File::flush` alone is only a userspace-buffer flush
/// and emits no syscall). NOTE: this writer is NOT routed through the wx-core
/// storage counters (documented limitation of phase A).
pub fn write_bytes_atomic(path: &str, text: &str) -> Result<(), String> {
    let tmp = format!("{}.tmp", path);
    let f = File::create(&tmp);
    match f {
        Err(_) => { return Err("create failed".to_string()); }
        Ok(mut h) => {
            let w = h.write_all(text.as_bytes());
            if w.is_err() {
                return Err("write_all failed".to_string());
            }
            let fl = h.flush();
            if fl.is_err() {
                return Err("flush failed".to_string());
            }
        }
    }
    match fsync_path(&tmp) {
        Err(e) => { return Err(format!("file fsync: {0}", e)); }
        Ok(_) => {}
    }
    let r = std::fs::rename(&tmp, &path);
    if r.is_err() {
        return Err("rename failed".to_string());
    }
    let dir = parent_dir(&path);
    match fsync_path(&dir) {
        Err(e) => { return Err(format!("dir fsync: {0}", e)); }
        Ok(_) => {}
    }
    return Ok(());
}

fn parent_dir(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 1 {
        return ".".to_string();
    }
    let mut out = "".to_string();
    for k in 0 .. parts.len() - 1 {
        if k > 0 {
            out = format!("{0}/{1}", out, parts[k]);
        } else {
            out = parts[k].to_string();
        }
    }
    if out.is_empty() {
        return "/".to_string();
    }
    return out;
}

fn esc_json_str(s: &str) -> String {
    let mut out = "".to_string();
    for ch in s.chars() {
        if ch == '"' || ch == '\\' {
            out = format!("{0}\\{1}", out, ch);
        } else {
            out = format!("{0}{1}", out, ch);
        }
    }
    return out;
}
