//! Persistent ingest record (spec §4.1): one JSON file under
//! `<data_dir>/state/files.json` mapping source path -> {size, mtime_ns,
//! content hash}. A restart must not re-ingest unchanged files; the same path
//! with a different content hash MUST re-ingest. The state file itself is
//! written atomically (temp + fsync + rename), mirroring the .bin policy.
//!
//! Content hash: FNV-1a 64 over first 64 KiB ++ last 64 KiB (whole file once
//! when smaller than 128 KiB) — spec §4.1 "first/last 64 KiB is fine".

use serde_json::from_str;
use memmap2::Mmap;
use wx_core::storage::{fsync_path};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::fs::metadata;
use std::fs::read_to_string;


/// Bytes of head/tail window hashed.
pub const HASH_WINDOW: usize = 65536;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct FileRec {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: String, // 16 hex chars
}

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
}

impl StateStore {
    /// Load from disk (empty when the file does not exist yet).
    pub fn load(state_dir: String) -> Result<StateStore, String> {
        let file = join_path(&state_dir, "files.json");
        let mut st = StateStore {
            dir_path: state_dir,
            file_path: file.to_string(),
            recs: Vec::<FileRec>::new(),
        };
        match read_to_string(&file) {
            Err(_) => { return Ok(st); }
            Ok(text) => {
                match from_str::<StateDoc>(&text) {
                    Err(e) => { return Err(format!("bad state file {}: {}", file, e)); }
                    Ok(doc) => {
                        for r in &doc.recs {
                            st.recs.push(r.clone());
                        }
                        return Ok(st);
                    }
                }
            }
        }
    }

    pub fn find(self: &Self, path: String) -> Option<FileRec> {
        for r in &self.recs {
            if r.path == path {
                return Some(r.clone());
            }
        }
        return None;
    }

    pub fn upsert(self: &mut Self, rec: FileRec) {
        for r in &mut self.recs {
            if r.path == rec.path {
                r.size = rec.size;
                r.mtime_ns = rec.mtime_ns;
                r.hash = rec.hash;
                return;
            }
        }
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
        let needle = format!("{0}/", prefix);
        let mut kept: Vec<FileRec> = Vec::<FileRec>::new();
        let mut n = 0usize;
        for r in &self.recs {
            if r.path.len() >= needle.len() && r.path[0 .. needle.len()] == *needle.as_str() {
                n += 1;
                continue;
            }
            kept.push(r.clone());
        }
        self.recs = kept;
        return n;
    }

    /// Persist atomically after each scan round.
    pub fn save(self: &Self) -> Result<(), String> {
        let mut copied: Vec<FileRec> = Vec::<FileRec>::new();
        for r in &self.recs {
            copied.push(r.clone());
        }
        let doc = StateDoc { version: 1, recs: copied };
        let text = serialize_text(&doc);
        return write_bytes_atomic(&self.file_path, &text);
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

/// FNV-1a 64 over the head/tail windows of a whole-file buffer.
pub fn content_hash_bytes(bytes: &[u8]) -> String {
    let n = bytes.len();
    let mut h: u64 = 0xcbf29ce484222325;
    if n <= HASH_WINDOW * 2 {
        for k in 0 .. n {
            h = (h ^ (bytes[k] as u64)).wrapping_mul(0x100000001b3);
        }
    } else {
        for k in 0 .. HASH_WINDOW {
            h = (h ^ (bytes[k] as u64)).wrapping_mul(0x100000001b3);
        }
        for k in n - HASH_WINDOW .. n {
            h = (h ^ (bytes[k] as u64)).wrapping_mul(0x100000001b3);
        }
    }
    let mut v = h;
    v = v ^ (v  >> 33);
    v = v.wrapping_mul(0xff51afd7ed558ccd);
    v = v ^ (v  >> 33);
    return format!("{:016x}", v);
}

/// Hash a file by mmapping it (page cache shared with the decode step).
pub fn content_hash_file(path: &str) -> Result<String, String> {
    let f = File::open(&path);
    match f {
        Err(_) => { return Err("open failed".to_string()); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return Err("mmap failed".to_string()); }
                Ok(region) => {
                    return Ok(content_hash_bytes(&region[0 .. region.len()]));
                }
            }
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
    return format!("{{\"path\":\"{}\",\"size\":{},\"mtime_ns\":{},\"hash\":\"{}\"}}",
        esc_json_str(&r.path), r.size, r.mtime_ns, r.hash);
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
/// `wx_core::storage::fsync_path` because this toolchain's std has no fsync
/// API (`File::flush` is only a userspace-buffer flush).
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
