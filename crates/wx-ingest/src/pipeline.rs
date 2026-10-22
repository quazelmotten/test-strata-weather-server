//! The ingest pipeline driver (spec §4.1 discovery+stability, §4.5 atomic
//! per-fh writes, §4.6 retention order, §5 arc-swap publication, §6 threading
//! counters). Scan rounds drain notify events first (latency) but ALWAYS
//! reconcile with a full recursive scan (authoritative on network mounts).

use wx_core::grid::{GridDef};
use crate::grib2::{decode_files_parallel, rotate_winds_to_earth, check_wind_pair, DecodedMessage, ParamFilter, StatKind, RunTime, grid_winds, epoch_secs, peek_ref_times};
use crate::config::{ResolvedModel, ResolvedParam, validate_config, load_config, DEFAULT_INGEST_THREADS, DEFAULT_STABLE_WINDOW_SECS, DEFAULT_DECODE_BATCH_FILES};
use serde_json::from_str;
use crate::state::{StateStore, FileRec, content_hash_file, stat_size_mtime, head_is_grib, REC_OK, REC_IGNORED, REC_FAILED, join_path};
use crate::discovery::{glob_match, walk_files, StabilityTracker, WatchQueue, start_watchers, drain_queue, base_name, path_to_str, MonoClock};
use crate::store::{write_grid_bin, merge_write_field_batch, decode_grid, manifest_field_json, write_bytes_atomic_pub, iso_utc, le_u32_at, GRID_VERSION};
use crate::index::{SharedIndex, IndexSnapshot, IdxModel, IdxRun, IdxFieldEntry, IdxField, IdxFh};
use wx_core::storage::{open_field, MmapField, TouchedDirs, counters_snapshot, StatMeta,
                       BIN_VERSION, bin_version_of_path, stat_kind_code, stat_kind_name};
use memmap2::Mmap;
use notify::RecommendedWatcher;
use wx_core::tables::{canonical_level_name, lookup_level_type, param_units, digit_at};
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::cmp::Ordering as CmpOrdering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use serde::Deserialize;


// ---------------------------------------------------------------------------
// mutable registry behind the snapshots
// ---------------------------------------------------------------------------

/// One forecast-hour slot inside one field entry. The key is an OFFSET IN
/// SECONDS from the run time (`fh_secs`), so fractional forecast hours
/// (30-minute HRRR-like products, 15-minute marine models) are exact keys
/// rather than lossy decimal hours. PDT 4.8 statistical time ranges are
/// carried PER forecast hour (two accumulations of the same field have
/// different ranges), stored as offsets in seconds from the run time.
#[derive(Clone)]
pub struct RegFh {
    pub fh_secs: i64,
    pub field: Arc<MmapField>,
    pub stat_has_range: bool,
    pub stat_begin_secs: i64,
    pub stat_end_secs: i64,
    pub stat_step_secs: i64,
}

#[derive(Clone)]
pub struct RegField {
    pub param_key: String,
    pub level_code: u8,
    pub units: String,
    pub stat_processed: bool,
    pub stat_kind: String,
    pub fhs: Vec<RegFh>, // sorted asc
}

#[derive(Clone)]
pub struct RegRun {
    pub epoch_secs: i64,
    pub dir_name: String,
    pub grid: GridDef,
    pub fields: Vec<RegField>, // sorted by (param_key, level_code)
}

#[derive(Clone)]
pub struct RegModel {
    pub name: String,
    pub runs: Vec<RegRun>, // sorted asc epoch
}

#[derive(Clone, Debug)]
pub struct FieldKey {
    pub param_key: String,
    pub level_code: u8,
}

pub fn find_run_idx(rs: &Vec<RegRun>, epoch: i64) -> Option<usize> {
    for k in 0 .. rs.len() {
        if rs[k].epoch_secs == epoch {
            return Some(k);
        }
    }
    return None;
}

pub fn find_field_idx(fs: &Vec<RegField>, fk: &FieldKey) -> Option<usize> {
    for k in 0 .. fs.len() {
        if fs[k].param_key == fk.param_key && fs[k].level_code == fk.level_code {
            return Some(k);
        }
    }
    return None;
}

pub fn find_fh_idx(fs: &Vec<RegFh>, fh: i64) -> Option<usize> {
    for k in 0 .. fs.len() {
        if fs[k].fh_secs == fh {
            return Some(k);
        }
    }
    return None;
}

impl RegModel {
    /// Insert/replace one field slot (creates run/field entries as needed).
    pub fn put(self: &mut Self, epoch: i64, dir_name: String, grid: GridDef,
               fk: FieldKey, fh_secs: i64, field: Arc<MmapField>,
               units: String, stat_processed: bool, stat_kind: String,
               stat_has_range: bool, stat_begin_secs: i64, stat_end_secs: i64,
               stat_step_secs: i64) {
        let mut ri = find_run_idx(&self.runs, epoch);
        match ri {
            None => {
                let r = RegRun {
                    epoch_secs: epoch,
                    dir_name: dir_name,
                    grid: grid,
                    fields: Vec::<RegField>::new(),
                };
                self.runs.push(r);
                for i2 in 1 .. self.runs.len() {
                    let mut j2 = i2;
                    while j2 > 0 && self.runs[j2].epoch_secs < self.runs[j2 - 1].epoch_secs {
                        let tv = self.runs[j2].clone();
                        self.runs[j2] = self.runs[j2 - 1].clone();
                        self.runs[j2 - 1] = tv;
                        j2 -= 1;
                    }
                }
                ri = find_run_idx(&self.runs, epoch);
            }
            Some(_) => {}
        }
        let ri2 = ri.unwrap();
        let mut fi = find_field_idx(&self.runs[ri2].fields, &fk);
        match fi {
            None => {
                let f = RegField {
                    param_key: fk.param_key.clone(),
                    level_code: fk.level_code,
                    units: units,
                    stat_processed: stat_processed,
                    stat_kind: stat_kind,
                    fhs: Vec::<RegFh>::new(),
                };
                self.runs[ri2].fields.push(f);
                let mut ki2 = self.runs[ri2].fields.len();
                while ki2 > 1 {
                    let ka = &self.runs[ri2].fields[ki2 - 1].param_key;
                    let kb = &self.runs[ri2].fields[ki2 - 2].param_key;
                    let ca = self.runs[ri2].fields[ki2 - 1].level_code;
                    let cb = self.runs[ri2].fields[ki2 - 2].level_code;
                    let mut sw = false;
                    if ka < kb {
                        sw = true;
                    } else if ka == kb && ca < cb {
                        sw = true;
                    }
                    if !sw {
                        break;
                    }
                    let tv = self.runs[ri2].fields[ki2 - 1].clone();
                    self.runs[ri2].fields[ki2 - 1] = self.runs[ri2].fields[ki2 - 2].clone();
                    self.runs[ri2].fields[ki2 - 2] = tv;
                    ki2 -= 1;
                }
                fi = find_field_idx(&self.runs[ri2].fields, &fk);
            }
            Some(_) => {}
        }
        let fi2 = fi.unwrap();
        let ki = find_fh_idx(&self.runs[ri2].fields[fi2].fhs, fh_secs);
        match ki {
            None => {
                self.runs[ri2].fields[fi2].fhs.push(RegFh {
                    fh_secs: fh_secs,
                    field: field,
                    stat_has_range: stat_has_range,
                    stat_begin_secs: stat_begin_secs,
                    stat_end_secs: stat_end_secs,
                    stat_step_secs: stat_step_secs,
                });
                for i2 in 1 .. self.runs[ri2].fields[fi2].fhs.len() {
                    let mut j2 = i2;
                    while j2 > 0 && self.runs[ri2].fields[fi2].fhs[j2].fh_secs < self.runs[ri2].fields[fi2].fhs[j2 - 1].fh_secs {
                        let tv = self.runs[ri2].fields[fi2].fhs[j2].clone();
                        self.runs[ri2].fields[fi2].fhs[j2] = self.runs[ri2].fields[fi2].fhs[j2 - 1].clone();
                        self.runs[ri2].fields[fi2].fhs[j2 - 1] = tv;
                        j2 -= 1;
                    }
                }
            }
            Some(ki2) => { self.runs[ri2].fields[fi2].fhs[ki2].field = field; }
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct IngestCounters {
    pub ingested_files: u64,
    pub errors: u64,
    /// Messages deliberately SKIPPED because they are structurally unusable
    /// (layer/range levels, coordinate lists, unknown grid/PDT templates) AND,
    /// since V1.2 phase B, wind components DROPPED by the pair rule or ORPHANED
    /// inside one file. Spec §4.2 wants these noted, not failed: they are counted
    /// separately and NEVER land in `errors` (mirrored to /metrics as
    /// wxpoint_skipped_messages_total).
    pub skipped_messages: u64,
    pub last_mtime_ns: i64, // max mtime over ingested files (lag input)
    /// Files whose FIRST FOUR BYTES are not `GRIB`: never decoded, recorded
    /// kind=IGNORED, logged ONCE (phase B poison quarantine).
    pub ignored_files: u64,
    /// Decode failures recorded as kind=FAILED with their content hash: retried
    /// only when the content changes, so `errors` advances ONCE per new/changed
    /// corrupt file instead of once per scan round.
    pub quarantined_files: u64,
    /// Field files written (the write-once grouping unit; compare with a
    /// message count to see the amplification removed by phase B).
    pub groups_written: u64,
    /// V1.2 phase C RETENTION-AWARE SKIP: candidate FILES whose SECTION-1
    /// reference time is STRICTLY OLDER than the oldest run the registry keeps
    /// while the registry is ALREADY FULL (`runs >= retention_runs`), so
    /// ingesting them could only produce a run retention would evict again
    /// IMMEDIATELY. Such a file is NEVER DECODED. Counted ONCE per
    /// (path,content-hash) verdict — like the quarantine counters, never once
    /// per scan round — and NEVER added to `errors`.
    pub retention_skipped_files: u64,
}

/// Live mirror of the ingest counters (V1.2 phase B). The pipeline updates this
/// after EVERY consumed FILE, so `/metrics` (ingest lag, error count, file/group
/// counts) moves DURING a long scan round instead of only after it, and the
/// server binary can print per-file progress while ingest is still running.
/// Relaxed atomics: single writer (the ingest thread), lock-free readers.
/// NOT `Clone`/`Debug` (atomics are neither): it is shared through `Arc`.
pub struct LiveCounters {
    pub errors: AtomicU64,
    pub ingested_files: AtomicU64,
    pub skipped_messages: AtomicU64,
    pub ignored_files: AtomicU64,
    pub quarantined_files: AtomicU64,
    pub groups_written: AtomicU64,
    pub retention_skipped: AtomicU64,
    pub last_mtime_ns: AtomicI64,
    /// Bumped once per consumed file; the printer side compares it against the
    /// last value it saw to decide whether a fresh progress line exists.
    pub progress_seq: AtomicU64,
    /// Newest per-file progress line ("ingested <file>: G groups, W bytes, X ms"
    /// or a failure/quarantine note), published together with `progress_seq`.
    pub last_msg: Mutex<String>,
    /// V1.2 phase D: FIFO of per-FILE progress lines NOT YET printed by the
    /// server's LIVE printer. `last_msg` alone could not do this: it keeps only
    /// the NEWEST line, so when two files finish between two 0.1 s polls the
    /// earlier line was LOST from operator output, and the round streamer then
    /// replayed the WHOLE ingest log, printing some lines a SECOND time (the
    /// double-INGEST red herring in the phase-D benchmark log). Bounded so a
    /// starved printer cannot grow memory without limit; overflow drops the OLDEST
    /// lines (they remain in `Pipeline.progress_log` for diagnostics).
    pub progress_queue: Mutex<Vec<String>>,
}

/// Cap on the un-printed progress FIFO (see `progress_queue`).
pub const PROGRESS_QUEUE_CAP: usize = 256;

impl LiveCounters {
    pub fn new_live() -> LiveCounters {
        return LiveCounters {
            errors: AtomicU64::new(0),
            ingested_files: AtomicU64::new(0),
            skipped_messages: AtomicU64::new(0),
            ignored_files: AtomicU64::new(0),
            quarantined_files: AtomicU64::new(0),
            groups_written: AtomicU64::new(0),
            retention_skipped: AtomicU64::new(0),
            last_mtime_ns: AtomicI64::new(0),
            progress_seq: AtomicU64::new(0),
            last_msg: Mutex::<String>::new("".to_string()),
            progress_queue: Mutex::<Vec<String>>::new(Vec::<String>::new()),
        };
    }

    /// Take EVERY pending progress line OUT of the FIFO (oldest first). The
    /// server's LIVE printer calls this ONCE per observed `progress_seq` advance,
    /// which is what makes each per-file line print EXACTLY ONCE: nothing is
    /// lost (FIFO, unlike `last_msg`) and nothing repeats.
    pub fn drain_progress(self: &Self) -> Vec<String> {
        let q = self.progress_queue.lock();
        match q {
            Err(_) => { return Vec::<String>::new(); }
            Ok(mut g) => {
                let out = g.clone();
                g.clear();
                return out;
            }
        }
    }

    /// Add deltas for every counter in ONE logical step (relaxed: single writer).
    pub fn add(self: &Self, errors: u64, files: u64, skipped: u64, ignored: u64,
               quarantined: u64, groups: u64) {
        let _a = self.errors.fetch_add(errors, Ordering::Relaxed);
        let _b = self.ingested_files.fetch_add(files, Ordering::Relaxed);
        let _c = self.skipped_messages.fetch_add(skipped, Ordering::Relaxed);
        let _d = self.ignored_files.fetch_add(ignored, Ordering::Relaxed);
        let _e = self.quarantined_files.fetch_add(quarantined, Ordering::Relaxed);
        let _f = self.groups_written.fetch_add(groups, Ordering::Relaxed);
    }

    /// Track the newest mtime seen (lag input for /metrics).
    pub fn note_mtime(self: &Self, ns: i64) {
        let cur = self.last_mtime_ns.load(Ordering::Relaxed);
        if ns > cur {
            self.last_mtime_ns.store(ns, Ordering::Relaxed);
        }
    }

    /// One retention-aware skip happened (V1.2 phase C). Separate from
    /// [`add`] because the pipeline counts it ONCE per (path,content-hash)
    /// verdict, not as part of a consumed-file delta.
    pub fn bump_retention_skip(self: &Self) {
        let _a = self.retention_skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// Monotone merge of ABSOLUTE values (used by `QueryStats.set_ingest`, which
    /// stores whole-counter values rather than deltas). Compare-store keeps this
    /// safe even when the pipeline has already advanced past the caller's snapshot
    /// mid-round: a mirror never moves BACKWARDS.
    pub fn merge_abs(self: &Self, errors: u64, files: u64, skipped: u64, ignored: u64,
                     quarantined: u64, groups: u64, mtime_ns: i64) {
        let mut cur = self.errors.load(Ordering::Relaxed);
        if errors > cur {
            self.errors.store(errors, Ordering::Relaxed);
        }
        cur = self.ingested_files.load(Ordering::Relaxed);
        if files > cur {
            self.ingested_files.store(files, Ordering::Relaxed);
        }
        cur = self.skipped_messages.load(Ordering::Relaxed);
        if skipped > cur {
            self.skipped_messages.store(skipped, Ordering::Relaxed);
        }
        cur = self.ignored_files.load(Ordering::Relaxed);
        if ignored > cur {
            self.ignored_files.store(ignored, Ordering::Relaxed);
        }
        cur = self.quarantined_files.load(Ordering::Relaxed);
        if quarantined > cur {
            self.quarantined_files.store(quarantined, Ordering::Relaxed);
        }
        cur = self.groups_written.load(Ordering::Relaxed);
        if groups > cur {
            self.groups_written.store(groups, Ordering::Relaxed);
        }
        self.note_mtime(mtime_ns);
    }

    /// Snapshot into the plain counter shape (used by the Prometheus mirror).
    pub fn snapshot(self: &Self) -> IngestCounters {
        return IngestCounters {
            ingested_files: self.ingested_files.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            skipped_messages: self.skipped_messages.load(Ordering::Relaxed),
            last_mtime_ns: self.last_mtime_ns.load(Ordering::Relaxed),
            ignored_files: self.ignored_files.load(Ordering::Relaxed),
            quarantined_files: self.quarantined_files.load(Ordering::Relaxed),
            groups_written: self.groups_written.load(Ordering::Relaxed),
            retention_skipped_files: self.retention_skipped.load(Ordering::Relaxed),
        };
    }
}

pub struct Pipeline {
    pub models: Vec<ResolvedModel>,
    pub data_dir: String,
    pub retention_runs: u32,
    pub scan_interval_secs: u32,
    pub stable_window_ns: i64,
    // V1.2 phase B: how many candidate files are DECODED as one chunk before
    // ANY of them is written ([ingest].decode_batch_files, default 4, >= 1).
    pub decode_batch_files: usize,
    pub state: StateStore,
    pub stability: StabilityTracker,
    // V1.2 phase B0: monotonic clock for the stability WINDOW (see
    // discovery::MonoClock). Wall-clock mtimes stay IDENTITY-ONLY.
    pub clock: MonoClock,
    pub registry: Vec<RegModel>,
    // Arc-wrapped so milestone 4's HTTP handlers can hold a clone of the SAME
    // ArcSwap while the Pipeline itself lives on the ingest thread.
    pub shared: Arc<SharedIndex>,
    pub counters: IngestCounters,
    // Live per-FILE mirror shared with the HTTP side (phase B).
    pub live: Arc<LiveCounters>,
    pub wind_violation: bool,
    // V1.2 phase C: bounded memo of retention-skip VERDICTS already noted/counted
    // ((path, content hash)). A stale file stays in the ingest dir and is re-peeked
    // every round — cheap by design — but must not spam the log or inflate the
    // counter once per round. A CONTENT CHANGE produces a new hash, re-arms exactly
    // ONE note plus ONE count, mirroring the kind=FAILED quarantine rule.
    pub stale_verdicts: Vec<(String, String)>,
    pub queue: WatchQueue,
    pub watchers: Vec<RecommendedWatcher>,
    pub pool: ThreadPool,
    pub log: Vec<String>,
    /// V1.2 phase D: per-FILE progress lines live HERE, NOT in `log`, because
    /// `main` streams `log` once per round AND the LIVE printer emits progress
    /// lines as they happen. Sharing one vector made every progress line appear
    /// TWICE in the server's stdout (and once more when the LIVE printer echoed
    /// its newest message again). Notes stay in `log`; progress lines are
    /// diagnostic-only here and reach operators through the LIVE printer.
    pub progress_log: Vec<String>,
}

impl Pipeline {
    /// Record an ingest diagnostic message (bounded).
    pub fn note(self: &mut Self, msg: String) {
        if self.log.len() < 512 {
            self.log.push(msg);
        }
    }

    /// Record ONE per-FILE progress line (V1.2 phase B, de-duplicated in phase D):
    /// appended to the bounded progress log (diagnostics only), emitted through
    /// `tracing` for operators who installed a backend, and pushed into the
    /// [`LiveCounters`] FIFO so the server binary prints it WHILE the scan round
    /// is still running. Each line reaches operator output EXACTLY ONCE: the
    /// round streamer in `main` never sees these lines (they are not in `log`).
    pub fn progress(self: &mut Self, msg: String) {
        tracing::info!("{0}", msg);
        if self.progress_log.len() < 512 {
            self.progress_log.push(msg.clone());
        }
        let m = self.live.last_msg.lock();
        match m {
            Err(_) => {}
            Ok(mut g) => { *g = msg.clone(); }
        }
        let q = self.live.progress_queue.lock();
        match q {
            Err(_) => {}
            Ok(mut g) => {
                if g.len() >= PROGRESS_QUEUE_CAP {
                    // drop the OLDEST un-printed line (still in progress_log)
                    g.remove(0);
                }
                g.push(msg.clone());
            }
        }
        let _q = self.live.progress_seq.fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// naming helpers
// ---------------------------------------------------------------------------

/// Bounded memo of retention-skip VERDICTS already noted/counted (§4.6 skip
/// path). Sized generously: one entry per (file,content hash) whose staleness
/// was judged, so ordinary churn never evicts anything.
pub const STALE_VERDICT_CAP: usize = 4096;

/// FILENAME-DERIVED RUN-TIME HINT — AN ORDERING HEURISTIC, NOTHING MORE.
/// Scans the BASENAME for digit runs of at least 10 characters and reads the
/// leading `YYYYMMDDHH` of each (covers `gfs.t2026101912z...`,
/// `hrrr.t2026101912.wrf...f006`, `20261019/12`-style names too because every
/// run of digits in the basename is tried). Returns the MAXIMUM plausible epoch
/// found, or 0 when the name carries no timestamp (hint 0 sorts LAST).
///
/// THE HINT NEVER DRIVES A SKIP OR A VALUE. The retention decision is taken
/// from the SECTION-1 REFERENCE TIME read out of the FILE (`peek_ref_times`),
/// per spec §4.2 ("never trust filenames for metadata"); this hint only decides
/// which candidate is consumed first so that the registry is as FULL and as NEW
/// as possible when the older candidates are judged. A misleading filename
/// therefore changes performance, never correctness — see the ordering-hint test.
pub fn filename_run_hint(path: String) -> i64 {
    let bn = base_name(path.to_string()).to_string();
    let mut best: i64 = 0;
    let n = bn.len();
    let mut k: usize = 0;
    while k < n {
        // START of a digit run?
        match digit_at(&bn, k) {
            None => { k += 1; continue; }
            Some(_) => {}
        }
        let start = k;
        let mut end = k;
        while end < n {
            match digit_at(&bn, end) {
                None => { break; }
                Some(_) => { end += 1; }
            }
        }
        if end - start >= 10 {
            let ep = hint_epoch_at(&bn, start);
            match ep {
                None => {}
                Some(ev) => {
                    if ev > best {
                        best = ev;
                    }
                }
            }
        }
        k = end;
    }
    return best;
}

/// One `YYYYMMDDHH` slot of a digit run as an epoch, None when the fields are
/// not a plausible UTC timestamp.
fn hint_epoch_at(bn: &str, start: usize) -> Option<i64> {
    let y = parse_num(bn, start, 4);
    let mo = parse_num(bn, start + 4, 2);
    let da = parse_num(bn, start + 6, 2);
    let hr = parse_num(bn, start + 8, 2);
    match y {
        None => { return None; }
        Some(yv) => {
            match mo {
                None => { return None; }
                Some(mov) => {
                    if mov < 1 || mov > 12 {
                        return None;
                    }
                    match da {
                        None => { return None; }
                        Some(dav) => {
                            if dav < 1 || dav > 31 {
                                return None;
                            }
                            match hr {
                                None => { return None; }
                                Some(hrv) => {
                                    if hrv > 23 {
                                        return None;
                                    }
                                    return Some(epoch_secs(yv as u16, mov as u8, dav as u8, hrv as u8, 0, 0));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Sort ONE model's ready list NEWEST-RUN-FIRST by [`filename_run_hint`]
/// (descending), STABLY: equal hints — including "no hint at all" (0) — keep the
/// sorted-path order they had, so pre-existing fixtures and tests consume files
/// in exactly the same sequence as before this change. ORDERING ONLY; see the
/// call site and [`filename_run_hint`] for why correctness is unaffected.
pub fn order_newest_first(ready: Vec<(String, u64, i64, String)>) -> Vec<(String, u64, i64, String)> {
    let mut keyed: Vec<(i64, String, u64, i64, String)> = Vec::<(i64, String, u64, i64, String)>::new();
    for r in &ready {
        keyed.push((filename_run_hint(r.0.to_string()), r.0.to_string(), r.1, r.2, r.3.to_string()));
    }
    keyed.sort_by(|a, b| {
        if a.0 > b.0 {
            return CmpOrdering::Less; // DESCENDING: bigger hint first
        }
        if a.0 < b.0 {
            return CmpOrdering::Greater;
        }
        return CmpOrdering::Equal;
    });
    let mut out: Vec<(String, u64, i64, String)> = Vec::<(String, u64, i64, String)>::new();
    for kv in &keyed {
        out.push((kv.1.to_string(), kv.2, kv.3, kv.4.to_string()));
    }
    return out;
}


pub fn level_dir_name(code: u8) -> String {
    let cn = canonical_level_name(code);
    match cn {
        None => { return format!("ct{}", code); }
        Some(s) => { return s.to_string(); }
    }
}

pub fn level_code_of_dir(dir: &str) -> Option<u8> {
    let c = lookup_level_type(&dir);
    match c {
        Some(v) => { return Some(v); }
        None => {
            if dir.len() > 2 && dir[0 .. 2] == *"ct" {
                return parse_small_u8(&dir[2 ..]);
            }
            return None;
        }
    }
}

fn parse_small_u8(s: &str) -> Option<u8> {
    let mut v: i64 = 0;
    for k in 0 .. s.len() {
        let d = digit_at(&s, k);
        match d {
            None => { return None; }
            Some(dv) => { v = v * 10 + (dv as i64); }
        }
    }
    if v < 0 || v > 255 {
        return None;
    }
    return Some(v as u8);
}

pub fn pad_num(v: i64, w: usize) -> String {
    let s = format!("{0}", v);
    let mut zeros = "".to_string();
    if s.len() < w {
        for _ in 0 .. (w - s.len()) {
            zeros = zeros + "0";
        }
    }
    return format!("{0}{1}", zeros, s);
}

/// Run-directory name — A TOTAL FUNCTION of the run time, and INVERTIBLE.
/// Base is the classic `YYYYMMDDHH`; `mm` is appended whenever the run MINUTE
/// OR the run SECOND is non-zero, and `ss` whenever the SECOND is non-zero, so
/// the three canonical lengths are 10 / 12 / 14 and [`parse_run_dir`] recovers
/// the exact instant from the name alone:
///
///   12:00:00 -> `2026101912`      (10, unchanged from V1.1: back compatible)
///   12:30:00 -> `202610191230`    (12)
///   12:00:30 -> `20261019120030`  (14)  ← was DROPPED to `2026101912` before
///   12:30:15 -> `20261019123015`  (14)
///
/// The pre-fix rule keyed on MINUTE ALONE, so a run at 12:00:30 named
/// `2026101912` and its SECONDS silently became 12:00:00 on the next warm boot
/// (a 30-second shift of every forecast-hour offset in that run). With `+mm`
/// driven by `m != 0 || s != 0`, `HHmmss` is ALWAYS present when any sub-hour
/// component is, which is what makes the 10/12/14 length rule unambiguous.
/// Pre-existing 10-char directories keep parsing exactly as before (minute and
/// second default to 0), and two runs that differ ONLY in minutes or seconds
/// can never COLLIDE into one directory.
pub fn run_dir_name(rt: RunTime) -> String {
    let a = pad_num(rt.year as i64, 4);
    let b = pad_num(rt.month as i64, 2);
    let c = pad_num(rt.day as i64, 2);
    let d = pad_num(rt.hour as i64, 2);
    let ab = format!("{0}{1}", a, b);
    let cd = format!("{0}{1}", c, d);
    let base = format!("{0}{1}", ab, cd);
    if rt.minute != 0 || rt.second != 0 {
        let mm = pad_num(rt.minute as i64, 2);
        let with_mm = format!("{0}{1}", base, mm);
        if rt.second != 0 {
            let ss = pad_num(rt.second as i64, 2);
            return format!("{0}{1}", with_mm, ss);
        }
        return with_mm;
    }
    return base;
}

/// Parse a run directory name back into epoch seconds. Accepts the three
/// canonical lengths (10 = hour only, 12 = +minute, 14 = +minute+second);
/// anything else is not a run directory.
pub fn parse_run_dir(name: &str) -> Option<i64> {
    if name.len() != 10 && name.len() != 12 && name.len() != 14 {
        return None;
    }
    let y = parse_num(&name, 0, 4);
    let mo = parse_num(&name, 4, 2);
    let da = parse_num(&name, 6, 2);
    let hr = parse_num(&name, 8, 2);
    let mi = if name.len() >= 12 { parse_num(&name, 10, 2) } else { Some(0) };
    let sc = if name.len() == 14 { parse_num(&name, 12, 2) } else { Some(0) };
    match y {
        None => { return None; }
        Some(yy) => {
            match mo {
                None => { return None; }
                Some(mo2) => {
                    match da {
                        None => { return None; }
                        Some(da2) => {
                            match hr {
                                None => { return None; }
                                Some(hr2) => {
                                    if mo2 < 1 || mo2 > 12 || da2 < 1 || da2 > 31 || hr2 > 23 {
                                        return None;
                                    }
                                    match mi {
                                        None => { return None; }
                                        Some(mi2) => {
                                            if mi2 > 59 {
                                                return None;
                                            }
                                            match sc {
                                                None => { return None; }
                                                Some(sc2) => {
                                                    if sc2 > 59 {
                                                        return None;
                                                    }
                                                    return Some(epoch_secs(yy as u16, mo2 as u8, da2 as u8, hr2 as u8, mi2 as u8, sc2 as u8));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn parse_num(s: &str, off: usize, len: usize) -> Option<i64> {
    let part = &s[off .. off + len];
    let mut v: i64 = 0;
    for k in 0 .. part.len() {
        let d = digit_at(&part, k);
        match d {
            None => { return None; }
            Some(dv) => { v = v * 10 + (dv as i64); }
        }
    }
    return Some(v);
}

/// Canonical forecast-hour filename for an offset of `fh_secs` SECONDS past
/// the run time (spec §4.5 storage keys). ONE canonical scheme, documented
/// in README/docs/DECISIONS.md:
///
///   secs % 3600 == 0              -> `f<HHH>.bin`           f000.bin / f006.bin
///   secs % 60   == 0 (not hour)   -> `f<HHH>m<MM>.bin`      f001m30.bin
///   whole seconds otherwise       -> `f<HHH>m<MM>s<SS>.bin` f001m00s05.bin
///
/// Hours are zero-padded to AT LEAST 3 digits (so every pre-existing
/// `f006.bin` keeps its exact name and stays readable by older indexes);
/// minutes and seconds are exactly 2 digits. Trailing components are OMITTED
/// when zero, never written as `f006m00`, which is what keeps integral hours
/// byte-identical to the milestone-3 layout (backward compatible).
///
/// Sub-second offsets cannot exist: GRIB2 section 4's finest Code Table 4.4
/// unit is SECOND (code 13), so `fc_secs` is always a whole number of seconds
/// by construction. A caller passing a negative or sub-second value is
/// rejected upstream with a typed note (`skipped_messages`, not `errors`).
pub fn fh_file_name(fh_secs: i64) -> String {
    let h = fh_secs / 3600;
    let rem = fh_secs - h * 3600;
    if rem == 0 {
        return format!("f{0}.bin", pad_num(h, 3));
    }
    if rem % 60 == 0 {
        return format!("f{0}m{1}.bin", pad_num(h, 3), pad_num(rem / 60, 2));
    }
    return format!("f{0}m{1}s{2}.bin", pad_num(h, 3), pad_num(rem / 60, 2), pad_num(rem % 60, 2));
}

/// Inverse of [`fh_file_name`]: filename -> offset in SECONDS. Strict about
/// the canonical form (minutes/seconds must be exactly two digits, `m` before
/// `s`, no trailing empty components) so a stray file like `f001m5.bin` is
/// ignored rather than silently reinterpreted.
pub fn parse_fh_name(name: &str) -> Option<i64> {
    if name.len() < 6 {
        return None;
    }
    if name[0 .. 1] != *"f" {
        return None;
    }
    if name[name.len() - 4 ..] != *".bin" {
        return None;
    }
    let core = &name[1 .. name.len() - 4];
    let mut hlen = 0;
    while hlen < core.len() {
        match digit_at(&core, hlen) {
            None => { break; }
            Some(_) => { hlen += 1; }
        }
    }
    if hlen == 0 {
        return None;
    }
    let hv = parse_num(&core, 0, hlen);
    match hv {
        None => { return None; }
        Some(hv2) => {
            let mut secs = hv2 * 3600;
            let mut off = hlen;
            if off >= core.len() {
                return Some(secs); // integral hours: f<HHH>.bin
            }
            if core[off .. off + 1] != *"m" {
                return None;
            }
            off += 1;
            if core.len() - off != 2 && core.len() - off != 5 {
                return None; // m<MM> (2 chars) or m<MM>s<SS> (5 chars)
            }
            let mv = parse_num(&core, off, 2);
            match mv {
                None => { return None; }
                Some(mv2) => {
                    if mv2 > 59 {
                        return None;
                    }
                    secs = secs + mv2 * 60;
                    let off2 = off + 2;
                    if off2 >= core.len() {
                        return Some(secs);
                    }
                    if core[off2 .. off2 + 1] != *"s" {
                        return None;
                    }
                    let sv = parse_num(&core, off2 + 1, 2);
                    match sv {
                        None => { return None; }
                        Some(sv2) => {
                            if sv2 > 59 {
                                return None;
                            }
                            return Some(secs + sv2);
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// open + disk rebuild
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ManifestField {
    pub param: String,
    pub level_type: String,
    pub units: String,
    pub stat_processed: bool,
    pub stat_kind: String,
    pub levels: Vec<f64>,
    // Human-facing decimal HOURS (0, 0.5, 1): informational only. The
    // authoritative keys are the on-disk filenames (see fh_file_name), so
    // rounding here can never corrupt an index entry.
    pub forecast_hours: Vec<f64>,
    // PDT 4.8 statistical time range PER forecast hour (spec §4.2: "store
    // the statistical process type AND time range"). `fh_secs` is the exact
    // join key against the filename-derived offset; begin/end are absolute
    // ISO-8601 UTC instants; step_secs is the CT 4.11 step length in seconds.
    pub stat_ranges: Vec<ManifestStatRange>,
}

#[derive(Deserialize)]
pub struct ManifestStatRange {
    pub fh_secs: i64,
    pub begin: String,
    pub end: String,
    pub step_secs: i64,
}

#[derive(Deserialize)]
pub struct ManifestDoc {
    pub model: String,
    pub fields: Vec<ManifestField>,
}

impl Pipeline {
    pub fn open(cfg_path: &str) -> Result<Pipeline, String> {
        let cfg = load_config(&cfg_path)?;
        let models = validate_config(&cfg)?;
        let data_dir = cfg.server.data_dir;
        let state_dir = join_path(&data_dir, "state");
        let _c = create_dir_all(&state_dir);
        let st = StateStore::load(state_dir);
        match st {
            Err(e) => { return Err(e); }
            Ok(_) => {}
        }
        let threads = match cfg.ingest.ingest_threads {
            None => { DEFAULT_INGEST_THREADS }
            Some(t) => { t }
        };
        let pool = ThreadPoolBuilder::new().num_threads(threads as usize).build();
        match pool {
            Err(_) => { return Err("rayon pool build failed".to_string()); }
            Ok(_) => {}
        }
        let window = match cfg.ingest.stable_window_secs {
            None => { DEFAULT_STABLE_WINDOW_SECS }
            Some(w) => { w }
        };
        let kb = match cfg.ingest.decode_batch_files {
            None => { DEFAULT_DECODE_BATCH_FILES }
            Some(k) => { k }
        };
        let q = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut p = Pipeline {
            models: models,
            data_dir: data_dir,
            retention_runs: cfg.ingest.retention_runs,
            scan_interval_secs: cfg.ingest.scan_interval_secs,
            stable_window_ns: (window as i64) * 1000000000,
            decode_batch_files: kb as usize,
            state: st.unwrap(),
            stability: StabilityTracker { obs: Vec::<(String, u64, i64, i64)>::new() },
            clock: MonoClock::new_clock(),
            registry: Vec::<RegModel>::new(),
            shared: Arc::new(SharedIndex { store: ArcSwap::from_pointee(IndexSnapshot { models: Vec::<IdxModel>::new() }) }),
            counters: IngestCounters { ingested_files: 0, errors: 0, skipped_messages: 0, last_mtime_ns: 0, ignored_files: 0, quarantined_files: 0, groups_written: 0, retention_skipped_files: 0 },
            live: Arc::new(LiveCounters::new_live()),
            wind_violation: false,
            stale_verdicts: Vec::<(String, String)>::new(),
            queue: q,
            watchers: Vec::<RecommendedWatcher>::new(),
            pool: pool.unwrap(),
            log: Vec::<String>::new(),
            progress_log: Vec::<String>::new(),
        };
        rebuild_from_disk(&mut p);
        p.publish();
        return Ok(p);
    }

    /// Start notify watchers over each ingest dir (event latency only; the
    /// full scan reconciles regardless — spec §4.1).
    pub fn start_watchers(self: &mut Self) {
        let mut dirs: Vec<String> = Vec::<String>::new();
        for m in &self.models {
            dirs.push(m.ingest_dir.clone());
        }
        let q2 = self.queue.clone();
        self.watchers = start_watchers(dirs, q2);
    }
}

use arc_swap::ArcSwap;

fn rebuild_from_disk(p: &mut Pipeline) {
    let mut jobs: Vec<(usize, String, i64, String)> = Vec::<(usize, String, i64, String)>::new();
    let mcount = p.models.len();
    for mi in 0 .. mcount {
        let mdir = join_path(&p.data_dir, p.models[mi].name.as_str());
        match read_dir(&mdir) {
            Err(_) => { continue; }
            Ok(it) => {
                for e in it {
                    match e {
                        Err(_) => { continue; }
                        Ok(de) => {
                            let rd = path_to_str(&de.path());
                            let bn = base_name(rd.to_string());
                            let ep = parse_run_dir(&bn);
                            match ep {
                                None => { continue; }
                                Some(epoch) => { jobs.push((mi, rd.to_string(), epoch, bn)); }
                            }
                        }
                    }
                }
            }
        }
    }
    for jb in &jobs {
        let mdlc = p.models[jb.0].clone();
        ingest_run_dir(p, &mdlc, jb.1.as_str(), jb.2, jb.3.clone());
    }
}

fn ingest_run_dir(p: &mut Pipeline, mdl: &ResolvedModel, run_dir: &str,
                  epoch: i64, dir_name: String) {
    let gp = format!("{}/grid.bin", run_dir);
    let gb = read_bytes_maybe(&gp);
    match gb {
        None => { return; }
        Some(b) => {
            let gd = decode_grid(&b);
            match gd {
                Err(e) => {
                    // V1.2 phase B0 UPGRADE-TRAP FIX. Pre-change this branch
                    // returned SILENTLY: a run dir written by an older storage
                    // format (grid.bin GRID_VERSION 1, before the earth-axes
                    // addition) was rejected on read, never logged, and NEVER
                    // rewritten (`store_plane` only writes grid.bin when MISSING),
                    // while the persistent state still marked the SOURCE files as
                    // ingested — so `/ready` stayed false FOREVER with no clue.
                    // Recovery chosen (documented in docs/DECISIONS.md + README
                    // limitations): LOUD log + DELETE the unusable run dir +
                    // FORGET this MODEL's ingest records so the NEXT scan round
                    // re-decodes from source with the CURRENT format. Idempotent
                    // and correctness-preserving (re-decode is deterministic);
                    // costs one re-ingest per affected model.
                    let found_ver = if b.len() >= 12 { le_u32_at(&b, 8) } else { 0 };
                    p.note(format!("STORAGE-FORMAT RECOVERY: grid.bin in {} cannot be decoded ({}); found grid version {}, current {}; deleting the run dir and forgetting model '{}'s ingest records so its source files are re-decoded on the next scan round",
                        run_dir, e, found_ver, GRID_VERSION, mdl.name));
                    p.counters.errors += 1;
                    delete_tree(&run_dir);
                    let dropped = p.state.drop_under(&mdl.ingest_dir);
                    p.note(format!("STORAGE-FORMAT RECOVERY: forgot {} ingest record(s) under {}", dropped, mdl.ingest_dir));
                    let _ = p.state.save();
                    return;
                }
                Ok(grid) => {
                    // ---- V1.3 phase E0: FIELD-FILE FORMAT PRE-CHECK ---------
                    // A .bin written by a DIFFERENT BIN_VERSION cannot carry the
                    // statistical-process metadata this build needs to guard
                    // accumulations (spec §6.3), so it must never answer a query.
                    // Same recovery as the grid.bin upgrade trap above: LOUD note
                    // + DELETE the run dir + FORGET this model's ingest records,
                    // so the NEXT scan round re-decodes from source with the
                    // current format. Done BEFORE anything is indexed, so a doomed
                    // run dir never half-publishes.
                    let files = collect_field_files(run_dir);
                    let mut bad_ver: Option<(String, u32)> = None;
                    for fe in &files {
                        match bin_version_of_path(fe.0.as_str()) {
                            Some(vv) => {
                                if vv != BIN_VERSION && bad_ver.is_none() {
                                    bad_ver = Some((fe.0.to_string(), vv));
                                }
                            }
                            // unreadable / shorter than the version field: leave
                            // it to open_field below, which reports WHY.
                            None => {}
                        }
                    }
                    match bad_ver {
                        Some(bp) => {
                            p.note(format!("STORAGE-FORMAT RECOVERY: field file {} in {} was written by a different storage format (found bin version {}, current {}); deleting the run dir and forgetting model '{}'s ingest records so its source files are re-decoded on the next scan round",
                                bp.0, run_dir, bp.1, BIN_VERSION, mdl.name));
                            p.counters.errors += 1;
                            delete_tree(&run_dir);
                            let dropped = p.state.drop_under(&mdl.ingest_dir);
                            p.note(format!("STORAGE-FORMAT RECOVERY: forgot {} ingest record(s) under {}", dropped, mdl.ingest_dir));
                            let _ = p.state.save();
                            return;
                        }
                        None => {}
                    }
                    // ---- manifest.json: HUMAN-FACING INVENTORY ONLY ----------
                    // V1.3 E0: the accumulation guard comes from each .bin's own
                    // header now, so a missing or unparsable manifest can no
                    // longer change an ANSWER. It is still a reportable anomaly
                    // (a failed manifest write means something IS wrong with the
                    // store), hence LOUD note once per run + one error count.
                    let mp = format!("{}/manifest.json", run_dir);
                    match read_text_maybe(&mp) {
                        None => {
                            p.note(format!("manifest.json MISSING in {} (model '{}'): indexing proceeds from the .bin headers, which carry the statistical metadata; the manifest is rewritten whenever this run ingests again", run_dir, mdl.name));
                            p.counters.errors += 1;
                        }
                        Some(txt) => {
                            match from_str::<ManifestDoc>(&txt) {
                                Err(e) => {
                                    p.note(format!("manifest.json in {} (model '{}') is UNPARSABLE ({:?}): indexing proceeds from the .bin headers", run_dir, mdl.name, e));
                                    p.counters.errors += 1;
                                }
                                Ok(_) => {}
                            }
                        }
                    }
                    // ---- index every readable field file --------------------
                    // Stat guard + range come from the FILE HEADER (V1.3 E0),
                    // never from the manifest: warm path and cold path agree by
                    // construction because both read the same durable bytes.
                    for fe in &files {
                        let mfr = open_field(fe.0.to_string());
                        match mfr {
                            Err(e) => {
                                // Structural failure that is NOT a version
                                // mismatch (truncated file, bad flags, corrupt
                                // stat bytes): loud and counted, slot absent.
                                p.note(format!("field file {} in run dir {} could not be opened ({}): skipped", fe.0, run_dir, e));
                                p.counters.errors += 1;
                                continue;
                            }
                            Ok(mfv) => {
                                let un = units_for(p, mdl, fe.1.as_str(), fe.3);
                                let sm = mfv.header.stat;
                                let sp = sm.guards_time();
                                let kind_name = stat_kind_name(sm.kind).to_string();
                                p.registry_put(mdl, epoch, dir_name.clone(), grid.clone(),
                                    FieldKey { param_key: fe.1.to_string(), level_code: fe.3 },
                                    fe.2, Arc::new(mfv), un, sp, kind_name,
                                    sp, sm.begin_secs, sm.end_secs, sm.step_secs);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One QUERYABLE field file found under a run directory: `(full path, param
/// key, forecast-hour offset in SECONDS, CT 4.5 level code)`. Entries that are
/// not a canonical `f<FFF>.bin` under a KNOWN level-type directory (grid.bin,
/// manifest.json, writer temp litter, hand-dropped junk) are IGNORED — exactly
/// the files the pre-V1.3-E0 inline walk skipped. Sorted by (param, level code,
/// fh) so both the recovery log and the index-build order are deterministic
/// across filesystems (V1.3 E0: the walk became a reusable collector because
/// the version pre-check has to see every field file BEFORE any of them gets
/// indexed).
fn collect_field_files(run_dir: &str) -> Vec<(String, String, i64, u8)> {
    let mut out: Vec<(String, String, i64, u8)> = Vec::<(String, String, i64, u8)>::new();
    match read_dir(run_dir) {
        Err(_) => { return out; }
        Ok(it) => {
            for pe in it {
                match pe {
                    Err(_) => { continue; }
                    Ok(pde) => {
                        let pdir = path_to_str(&pde.path());
                        let param_key = base_name(pdir.to_string());
                        if param_key == "grid.bin" || param_key == "manifest.json" {
                            continue;
                        }
                        match read_dir(&pdir) {
                            Err(_) => { continue; }
                            Ok(lit) => {
                                for le in lit {
                                    match le {
                                        Err(_) => { continue; }
                                        Ok(lde) => {
                                            let ldir = path_to_str(&lde.path());
                                            let lvl_dir = base_name(ldir.to_string());
                                            match level_code_of_dir(&lvl_dir) {
                                                None => { continue; }
                                                Some(code) => {
                                                    match read_dir(&ldir) {
                                                        Err(_) => { continue; }
                                                        Ok(fit) => {
                                                            for fe in fit {
                                                                match fe {
                                                                    Err(_) => { continue; }
                                                                    Ok(fde) => {
                                                                        let fp = path_to_str(&fde.path());
                                                                        let bn = base_name(fp.to_string());
                                                                        match parse_fh_name(&bn) {
                                                                            None => { continue; }
                                                                            Some(fh) => {
                                                                                out.push((fp.to_string(), param_key.to_string(), fh, code));
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| {
        if a.1 < b.1 {
            return CmpOrdering::Less;
        }
        if a.1 > b.1 {
            return CmpOrdering::Greater;
        }
        if a.3 < b.3 {
            return CmpOrdering::Less;
        }
        if a.3 > b.3 {
            return CmpOrdering::Greater;
        }
        if a.2 < b.2 {
            return CmpOrdering::Less;
        }
        if a.2 > b.2 {
            return CmpOrdering::Greater;
        }
        return CmpOrdering::Equal;
    });
    return out;
}

impl Pipeline {
    pub fn registry_put(self: &mut Self, mdl: &ResolvedModel, epoch: i64, dir_name: String,
                        grid: GridDef, fk: FieldKey, fh_secs: i64, field: Arc<MmapField>,
                        units: String, stat_processed: bool, stat_kind: String,
                        stat_has_range: bool, stat_begin_secs: i64,
                        stat_end_secs: i64, stat_step_secs: i64) {
        let mut ri = find_model_idx(&self.registry, &mdl.name);
        match ri {
            None => {
                let nm = RegModel { name: mdl.name.clone(), runs: Vec::<RegRun>::new() };
                self.registry.push(nm);
                for i2 in 1 .. self.registry.len() {
                    let mut j2 = i2;
                    while j2 > 0 && self.registry[j2].name < self.registry[j2 - 1].name {
                        let tv = self.registry[j2].clone();
                        self.registry[j2] = self.registry[j2 - 1].clone();
                        self.registry[j2 - 1] = tv;
                        j2 -= 1;
                    }
                }
                ri = find_model_idx(&self.registry, &mdl.name);
            }
            Some(_) => {}
        }
        self.registry[ri.unwrap()].put(epoch, dir_name, grid, fk, fh_secs, field, units,
            stat_processed, stat_kind, stat_has_range, stat_begin_secs,
            stat_end_secs, stat_step_secs);
    }
}

fn find_model_idx(rs: &Vec<RegModel>, name: &str) -> Option<usize> {
    for k in 0 .. rs.len() {
        if rs[k].name == name {
            return Some(k);
        }
    }
    return None;
}

fn units_for(_p: &Pipeline, mdl: &ResolvedModel, param_key: &str, _code: u8) -> String {
    for rp in &mdl.params {
        if rp.key == param_key {
            return rp.units.clone();
        }
    }
    let known = param_units(&param_key);
    match known {
        None => { return "".to_string(); }
        Some(u) => { return u.to_string(); }
    }
}

/// Render the per-fh statistical ranges of one field entry as a JSON array
/// (`[]` when nothing is statistically processed). begin/end are ABSOLUTE
/// instants in ISO-8601 UTC; `fh_secs` is the exact join key.
pub fn stat_ranges_json(fhs: &Vec<RegFh>, run_epoch: i64) -> String {
    let mut out = "[".to_string();
    let mut first = true;
    for e in fhs {
        if !e.stat_has_range {
            continue;
        }
        if !first {
            out = out + ",";
        }
        first = false;
        let b = iso_utc(run_epoch + e.stat_begin_secs);
        let en = iso_utc(run_epoch + e.stat_end_secs);
        out = format!("{0}{{\"fh_secs\":{1},\"begin\":\"{2}\",\"end\":\"{3}\",\"step_secs\":{4}}}",
            out, e.fh_secs, b, en, e.stat_step_secs);
    }
    return out + "]";
}

// V1.3 phase E0 REMOVED: `stat_for()` and `stat_range_for()` used to restore
// the PDT 4.8 guard and its time range from manifest.json on a warm start. That
// made a human-facing inventory file correctness-critical: delete or corrupt
// one manifest and an accumulation started answering with LINEAR TIME
// INTERPOLATION (spec §6.3 violation, silent wrong answer). The guard and the
// range now live in each field file's own header (`wx_core::storage::StatMeta`),
// which `ingest_run_dir` reads directly, so both paths answer from the same
// durable bytes.

fn read_bytes_maybe(path: &str) -> Option<Vec<u8 > > {
            let f = std::fs::File::open(&path);
    match f {
        Err(_) => { return None; }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return None; }
                Ok(region) => {
                    let mut copy: Vec<u8> = Vec::<u8>::new();
                    for k in 0 .. region.len() {
                        copy.push(region[k]);
                    }
                    return Some(copy);
                }
            }
        }
    }
}

fn read_text_maybe(path: &str) -> Option<String> {
    use std::fs::read_to_string;
    match read_to_string(&path) {
        Err(_) => { return None; }
        Ok(t) => { return Some(t); }
    }
}

// ---------------------------------------------------------------------------
// scan round
// ---------------------------------------------------------------------------

impl Pipeline {
    /// One full discovery+ingest pass. Returns files ingested.
    /// V1.2 phase B0: the stability window is measured on the MONOTONIC clock
    /// (`self.clock`), so a backward wall-clock jump can no longer make an
    /// otherwise-stable file ineligible.
    /// V1.2 phase C additions: the notify queue is drained ONCE PER ROUND into
    /// per-model buckets (draining inside the model loop let the FIRST model
    /// consume every event, so later models' event latency waited for the NEXT
    /// round); stale candidates are skipped BEFORE decode; and both the persistent
    /// record list and the stability tracker are PRUNED of vanished files.
    pub fn scan_round(self: &mut Self) -> usize {
        let now_ns = self.clock.elapsed_ns();
        let mut ingested = 0;
        // ---- EVENT QUEUE: drained ONCE for the WHOLE ROUND ------------------
        let evs_all = drain_queue(self.queue.clone());
        // ---- STABILITY TRACKER HYGIENE (bounded: one stat per remembered path)
        let vanished = self.stability.prune_vanished();
        if vanished > 0 {
            self.note(format!("stability tracker: forgot {} vanished file(s)", vanished));
        }
        for mi in 0 .. self.models.len() {
            let mdl = self.models[mi].clone();
            // ---- PERSISTENT RECORD HYGIENE (bounded by this model's record count)
            let pruned = self.state.prune_missing(&mdl.ingest_dir);
            if pruned > 0 {
                self.note(format!("state: dropped {} ingest record(s) whose source file vanished under {}", pruned, mdl.ingest_dir));
            }
            let mut cands: Vec<String> = Vec::<String>::new();
            for e in &evs_all {
                if glob_match(&mdl.file_glob, base_name(e.to_string()).as_str()) {
                    cands.push(e.to_string());
                }
            }
            for f in walk_files(&mdl.ingest_dir) {
                if glob_match(&mdl.file_glob, base_name(f.to_string()).as_str()) {
                    cands.push(f);
                }
            }
            cands.sort();
            cands.dedup();
            let mut ready: Vec<(String, u64, i64, String)> = Vec::<(String, u64, i64, String)>::new();
            for path in &cands {
                let sm = stat_size_mtime(&path);
                let sm2 = match sm {
                    Err(_) => { continue; }
                    Ok(v0) => { v0 }
                };
                let hv = content_hash_file(&path);
                let hv2 = match hv {
                    Err(_) => { continue; }
                    Ok(v1) => { v1 }
                };
                let rec = self.state.find(path.to_string());
                match rec {
                    Some(r) => {
                        if r.size == sm2.0 && r.mtime_ns == sm2.1 && r.hash == hv2 {
                            continue; // unchanged: no re-ingest (spec §4.1)
                        }
                    }
                    None => {}
                }
                let sc = mdl.sidecar_suffix.clone();
                let eligible = match sc {
                    None => { self.stability.observe(path.to_string(), sm2.0, sm2.1, now_ns, self.stable_window_ns) }
                    Some(suf) => { self.stability.sidecar_ready(path.to_string(), suf) }
                };
                if !eligible {
                    continue;
                }
                ready.push((path.to_string(), sm2.0, sm2.1, hv2));
            }
            if ready.is_empty() {
                continue;
            }
            // ---- CANDIDATE ORDERING: NEWEST RUN FIRST, HINT ONLY -------------
            // The sort key is a run time read OUT OF THE FILENAME, and it is a
            // HEURISTIC FOR ORDERING ONLY: it decides which candidate is
            // CONSUMED FIRST so a freshly delivered burst fills the registry with
            // its NEWEST runs before older candidates are judged against them
            // (maximizing how many stale files the §4.6 skip below can veto
            // WITHOUT ever decoding them). It NEVER decides whether a file is
            // skipped: that verdict comes SOLELY from the SECTION-1 peek
            // (`peek_ref_times`). A wrong, misleading or absent hint therefore
            // costs nothing but a little wasted decode; `sort_by` is STABLE, so
            // names without a parseable stamp keep their sorted-path order and
            // every pre-existing fixture behaves exactly as in phase B.
            ready = order_newest_first(ready);
            let mut triples: Vec<(u8, u8, u8)> = Vec::<(u8, u8, u8)>::new();
            for rp in &mdl.params {
                triples.push((rp.disc, rp.cat, rp.num));
            }
            // ---- V1.2 phase B: CHUNKED DECODE + PER-FILE CONSUMPTION ----------
            // K candidate files are decoded together (cross-file parallelism
            // preserved, peak RAM ~K files), then EACH file is consumed IMMEDIATELY:
            // group -> write ONCE per group -> registry update -> state upsert+save
            // -> publish -> progress line. So the FIRST queryable output lands after
            // the first file of the first chunk, not after the slowest file of the
            // whole directory.
            let kchunk = self.decode_batch_files;
            let mut ci = 0;
            while ci < ready.len() {
                let mut cend = ci + kchunk;
                if cend > ready.len() {
                    cend = ready.len();
                }
                // MAGIC-BYTE PRECHECK BEFORE ANY DECODE (poison quarantine part 1):
                // a candidate whose first four bytes are not ASCII `GRIB` (a `.idx`
                // sidecar delivered next to the product is the common case) can
                // NEVER reach the decoder. It gets a kind=IGNORED record, is logged
                // ONCE, and is never looked at again until its content changes.
                let mut dec: Vec<(String, u64, i64, String)> = Vec::<(String, u64, i64, String)>::new();
                let mut cj = ci;
                while cj < cend {
                    let rpath = ready[cj].0.to_string();
                    let rsize = ready[cj].1;
                    let rmtime = ready[cj].2;
                    let rhash = ready[cj].3.to_string();
                    // ---- RETENTION-AWARE SKIP BEFORE DECODE (spec §4.6) -----
                    // Proven entirely-obsolete candidates are vetoed HERE, i.e.
                    // BEFORE the magic-byte precheck, the rayon decode AND any
                    // write. See `retention_skip_reason` for the exact rule
                    // (registry FULL + SECTION-1 reference time STRICTLY OLDER
                    // than the oldest kept run) and why an uncertain peek always
                    // falls through to the normal path.
                    let stale = self.retention_skip_reason(&mdl, rpath.clone(), rhash.clone());
                    match stale {
                        Some(reason) => {
                            self.progress(format!("skipped {0}: {1}", rpath, reason));
                            cj += 1;
                            continue;
                        }
                        None => {}
                    }
                    let hg = head_is_grib(&rpath);
                    let isg = match hg {
                        Err(e) => {
                            // unreadable/short: NOT a verdict about the content, so
                            // NO record is written and the candidate is retried.
                            self.note(format!("magic-byte precheck could not read {} ({}): retrying next round", rpath, e));
                            false
                        }
                        Ok(v) => { v }
                    };
                    if !isg {
                        let prior = self.state.find(rpath.clone());
                        let already = match prior {
                            None => { false }
                            Some(pr) => { pr.kind == REC_IGNORED && pr.hash == rhash }
                        };
                        if !already {
                            self.note(format!("not GRIB2 (magic bytes absent), never decoded: {}", rpath));
                            self.counters.ignored_files += 1;
                            self.live.add(0, 0, 0, 1, 0, 0);
                        }
                        self.progress(format!("ignored {0}: not a GRIB2 stream", rpath));
                        self.state.upsert(FileRec {
                            path: rpath,
                            size: rsize,
                            mtime_ns: rmtime,
                            hash: rhash,
                            kind: REC_IGNORED,
                        });
                        let _ = self.state.save();
                        cj += 1;
                        continue;
                    }
                    dec.push((rpath, rsize, rmtime, rhash));
                    cj += 1;
                }
                if !dec.is_empty() {
                    let mut names: Vec<String> = Vec::<String>::new();
                    for d in &dec {
                        names.push(d.0.clone());
                    }
                    // ONE rayon pass over the chunk. The returned vector is iterated
                    // DIRECTLY: the pre-B code copied it into a second vector first.
                    let results = self.pool.install(|| decode_files_parallel(names.clone(), ParamFilter::Only(triples.clone())));
                    for one in results {
                        let rd = dec.remove(0);
                        match one {
                            Err(e) => {
                                // POISON QUARANTINE part 2: record the failure WITH
                                // its content hash, so later rounds skip this file
                                // until its CONTENT changes, and advance `errors`
                                // only when a NEW FAILED record is created or the
                                // hash changed (never once per scan round).
                                let prior = self.state.find(rd.0.clone());
                                let same = match prior {
                                    None => { false }
                                    Some(pr) => { pr.kind == REC_FAILED && pr.hash == rd.3 }
                                };
                                if !same {
                                    self.note(format!("grib decode {}: {:?}", rd.0, e));
                                    self.counters.errors += 1;
                                    self.counters.quarantined_files += 1;
                                    self.live.add(1, 0, 0, 0, 1, 0);
                                }
                                self.state.upsert(FileRec {
                                    path: rd.0.clone(),
                                    size: rd.1,
                                    mtime_ns: rd.2,
                                    hash: rd.3.clone(),
                                    kind: REC_FAILED,
                                });
                                let _ = self.state.save();
                                self.progress(format!("failed {0}: decode error quarantined ({1:?})", rd.0, e));
                            }
                            Ok(dr) => {
                                // structural SKIP-AND-NOTE bookkeeping (spec §4.2):
                                // coordinate-list / unknown-template messages counted
                                // separately from errors and logged for operators.
                                if dr.1.messages_unsupported > 0 {
                                    self.note(format!("{} message(s) skipped as structurally unsupported in {}",
                                        dr.1.messages_unsupported, rd.0));
                                    self.counters.skipped_messages += dr.1.messages_unsupported as u64;
                                    self.live.add(0, 0, dr.1.messages_unsupported as u64, 0, 0, 0);
                                }
                                let out = self.ingest_file(&mdl, dr.0, rd.0.to_string());
                                if out.failed_groups > 0 {
                                    // ANY write step failed (a GROUP or the
                                    // manifest refresh): the file is NOT marked
                                    // ingested, so the NEXT round retries it
                                    // (idempotent thanks to phase-A merge dedup +
                                    // atomic renames, and V1.3 E0 makes the
                                    // manifest rewrite part of that contract).
                                    // ONE error per file-with-failure, not one
                                    // per failed step.
                                    self.counters.errors += 1;
                                    self.live.add(1, 0, 0, 0, 0, out.groups as u64);
                                    self.progress(format!("FAILED {0}: {1} of {2} write step(s) failed (group writes + manifest refresh), retrying next round",
                                        rd.0, out.failed_groups, out.groups + out.failed_groups));
                                } else {
                                    self.state.upsert(FileRec {
                                        path: rd.0.clone(),
                                        size: rd.1,
                                        mtime_ns: rd.2,
                                        hash: rd.3.clone(),
                                        kind: REC_OK,
                                    });
                                    let _ = self.state.save();
                                    if rd.2 > self.counters.last_mtime_ns {
                                        self.counters.last_mtime_ns = rd.2;
                                    }
                                    self.counters.ingested_files += 1;
                                    self.counters.groups_written += out.groups as u64;
                                    ingested += 1;
                                    self.live.add(0, 1, 0, 0, 0, out.groups as u64);
                                    self.live.note_mtime(rd.2);
                                    self.progress(format!("ingested {0}: {1} groups, {2} bytes, {3} ms",
                                        rd.0, out.groups, out.bytes_written, out.ms));
                                }
                            }
                        }
                    }
                }
                ci = cend;
            }
        }
        // Per-FILE state saves above are authoritative; this final save keeps the
        // IGNORED/FAILED records of a chunk that produced no successful file.
        let _ = self.state.save();
        retention_prune(self);
        return ingested;
    }

    /// Publish the current registry as a new immutable snapshot atomically.
    pub fn publish(self: &Self) {
        let snap = build_snapshot(&self.registry);
        self.shared.store.swap(Arc::new(snap));
    }

    /// V1.2 phase C — RETENTION-AWARE SKIP DECISION (spec §4.6 efficiency, gated
    /// by §4.2 "never trust filenames"). Returns `Some(reason)` when this candidate
    /// may be vetoed WITHOUT being decoded, `None` when the normal path must run.
    ///
    /// THE RULE, exactly:
    ///   * the model's registry holds AT LEAST `retention_runs` DISTINCT RUNS
    ///     (a registry with room left can always absorb another run: skipping
    ///     would be wrong), AND
    ///   * the NEWEST SECTION-1 REFERENCE TIME declared anywhere in the file is
    ///     STRICTLY OLDER than the OLDEST run the registry keeps.
    ///
    /// WHY THAT IS SAFE (not just convenient): ingesting such a file could only
    /// ADD runs older than everything kept, and `retention_prune` removes runs
    /// oldest-first, so the new run(s) would be evicted again at the end of the
    /// SAME round — the observable state is IDENTICAL, minus a full decode.
    ///
    /// STRICTNESS MATTERS: equality means the file belongs to a run that is ALREADY
    /// KEPT, e.g. a partially delivered same-run product, so it is NEVER skipped.
    /// A file carrying SEVERAL runs (`frac_small.grib2`) is judged on its NEWEST
    /// reference time, which is the conservative end.
    ///
    /// UNCERTAINTY NEVER SKIPS: if the header peek fails for ANY structural
    /// reason (bad magic, bad edition byte, truncated framing, no section 1,
    /// unreadable) the candidate falls through to the normal path, where the
    /// magic-byte precheck and the poison quarantine apply exactly as before.
    ///
    /// NO STATE RECORD IS WRITTEN for a skipped file: the verdict is re-derived
    /// every round against the CURRENT registry, so if the newer runs later
    /// disappear upstream and the registry empties out, the file becomes
    /// ingestible again instead of being stuck behind a false "already ingested"
    /// claim. The note/counter are deduped per (path,content-hash) instead.
    pub fn retention_skip_reason(self: &mut Self, mdl: &ResolvedModel,
                                 path: String, hash: String) -> Option<String> {
        let mi = find_model_idx(&self.registry, &mdl.name);
        match mi {
            None => { return None; }
            Some(mi2) => {
                if self.registry[mi2].runs.len() < (self.retention_runs as usize) {
                    return None; // registry has room: nothing is obsolete yet
                }
                let oldest_kept = self.registry[mi2].runs[0].epoch_secs;
                let pk = peek_ref_times(path.clone());
                match pk {
                    Err(e) => {
                        // Uncertain: fall through, ONCE-noted per (path,hash).
                        if !self.seen_verdict(path.clone(), hash.clone()) {
                            self.remember_verdict(path.clone(), hash.clone());
                            self.note(format!("retention-skip peek failed for {} ({:?}): decoding it normally", path, e));
                        }
                        return None;
                    }
                    Ok(pv) => {
                        if pv.max_epoch >= oldest_kept {
                            return None; // same-run partial delivery or a newer run
                        }
                        let fresh = !self.seen_verdict(path.clone(), hash.clone());
                        if fresh {
                            self.remember_verdict(path.clone(), hash.clone());
                            self.counters.retention_skipped_files += 1;
                            self.live.bump_retention_skip();
                            self.note(format!(
                                "retention skip: {} declares newest reference time {} (oldest {}), strictly older than the oldest kept run {} of model '{}' ({} runs kept) — not decoded",
                                path, iso_utc(pv.max_epoch), iso_utc(pv.min_epoch), iso_utc(oldest_kept),
                                mdl.name, self.registry[mi2].runs.len()));
                        }
                        return Some(format!("reference time {0} is older than the oldest kept run {1}",
                            iso_utc(pv.max_epoch), iso_utc(oldest_kept)));
                    }
                }
            }
        }
    }

    /// Already noted/counted this (path,content-hash) verdict?
    fn seen_verdict(self: &Self, path: String, hash: String) -> bool {
        for v in &self.stale_verdicts {
            if v.0 == path && v.1 == hash {
                return true;
            }
        }
        return false;
    }

    /// Remember a verdict, evicting the oldest when the bounded memo is full
    /// (an evicted path simply gets ONE more note+count if it is still stale).
    fn remember_verdict(self: &mut Self, path: String, hash: String) {
        while self.stale_verdicts.len() >= STALE_VERDICT_CAP {
            let _x = self.stale_verdicts.remove(0);
        }
        self.stale_verdicts.push((path.to_string(), hash.to_string()));
    }
}

// ---------------------------------------------------------------------------
// messages -> storage (per-fh visibility: publish after every field write)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct WindPending {
    pub key: (i64, i64, u8, f64),
    pub u: Option<Vec<f32> >,
    pub v: Option<Vec<f32> >,
    // V1.2 phase B: the CONFIGURED param KEY each component resolved to. Looking
    // the pair up by name ("UGRD"/"VGRD") would silently DROP a model that names
    // its winds by RAW discipline/category/number triple, so the keys travel with
    // the pending entry instead.
    pub ukey: String,
    pub vkey: String,
}

fn find_wind(pend: &Vec<WindPending>, key: (i64, i64, u8, f64)) -> Option<usize> {
    for k in 0 .. pend.len() {
        if pend[k].key == key {
            return Some(k);
        }
    }
    return None;
}

/// One WRITE UNIT (V1.2 phase B): every message of ONE source file that lands in
/// the SAME field file is merged and written ONCE. The group key is
/// `(run epoch, param key, level type code, forecast-hour offset in seconds)` —
/// the WindPending-style keying extended with the run EPOCH, because ONE file can
/// carry TWO runs (`tests/fixtures/frac_small.grib2`) and `fh_secs` is relative
/// to the run. Levels inside a group keep caller order, so the documented
/// "first entry wins" rule of `merge_field` applies inside a group too.
#[derive(Clone, Debug)]
pub struct StoreGroup {
    pub epoch: i64,
    pub dir_name: String,
    pub grid: GridDef,
    pub param_key: String,
    pub level_code: u8,
    pub fh_secs: i64,
    pub units: String,
    pub stat_processed: bool,
    pub stat_kind: String,
    pub stat_has_range: bool,
    pub stat_begin_secs: i64,
    pub stat_end_secs: i64,
    pub stat_step_secs: i64,
    pub entries: Vec<(f64, Vec<f32>)>, // (level value, plane) in caller order
}

fn find_group(gs: &Vec<StoreGroup>, epoch: i64, param_key: &str, code: u8,
              fh: i64) -> Option<usize> {
    for k in 0 .. gs.len() {
        if gs[k].epoch == epoch && gs[k].param_key == param_key &&
           gs[k].level_code == code && gs[k].fh_secs == fh {
            return Some(k);
        }
    }
    return None;
}

/// Append one plane to its group, creating the group on demand. Planes are MOVED
/// into the group (no per-message clone of a multi-megabyte plane, which the
/// pre-B code paid once per message).
fn group_put(gs: &mut Vec<StoreGroup>, epoch: i64, dir_name: &str, grid: &GridDef,
             param_key: &str, units: &str, level_code: u8, fh_secs: i64,
             stat_processed: bool, stat_kind: &str, stat_has_range: bool,
             stat_begin_secs: i64, stat_end_secs: i64, stat_step_secs: i64,
             level_val: f64, plane: Vec<f32>) {
    let gi = find_group(gs, epoch, param_key, level_code, fh_secs);
    match gi {
        None => {
            gs.push(StoreGroup {
                epoch: epoch,
                dir_name: dir_name.to_string(),
                grid: grid.clone(),
                param_key: param_key.to_string(),
                level_code: level_code,
                fh_secs: fh_secs,
                units: units.to_string(),
                stat_processed: stat_processed,
                stat_kind: stat_kind.to_string(),
                stat_has_range: stat_has_range,
                stat_begin_secs: stat_begin_secs,
                stat_end_secs: stat_end_secs,
                stat_step_secs: stat_step_secs,
                entries: [(level_val, plane)].to_vec(),
            });
        }
        Some(gi2) => { gs[gi2].entries.push((level_val, plane)); }
    }
}

/// Result of consuming ONE source file.
#[derive(Clone, Debug)]
pub struct FileOutcome {
    pub groups: usize,          // field files WRITTEN successfully
    /// Groups whose write FAILED **plus** manifest-refresh failures (V1.3 E0):
    /// ANY non-zero value means the source file is NOT recorded as ingested and
    /// gets retried next round.
    pub failed_groups: usize,
    pub bytes_written: u64,     // storage BYTES_WRITTEN_TOTAL delta
    pub renames: u64,           // RENAMES_TOTAL delta (groups + grid.bin + manifests)
    pub ms: i64,                // monotonic time spent on this file
    pub runs_touched: usize,    // manifest refreshes performed
}

impl Pipeline {
    /// Consume ONE decoded file (V1.2 phase B). Order of operations:
    ///   1. pair wind components (the pending map is scoped to THIS FILE and
    ///      FULLY DRAINED at the end: an orphan is noted + counted as a SKIP,
    ///      never stored and never leaked into the next file),
    ///   2. group every plane by (run, param, level type, forecast hour),
    ///   3. merge+write each group ONCE via `merge_write_field_batch`, collecting
    ///      per-group errors WITHOUT aborting the rest of the file,
    ///   4. refresh each affected run's manifest.json ONCE,
    ///   5. fsync every DISTINCT directory touched ONCE (`TouchedDirs`),
    ///   6. publish the registry ONCE (visibility granularity = one file, which
    ///      still satisfies spec §4.5 because products arrive per forecast hour).
    pub fn ingest_file(self: &mut Self, mdl: &ResolvedModel, msgs: Vec<DecodedMessage>,
                       path: String) -> FileOutcome {
        let t0 = self.clock.elapsed_ns();
        let c0 = counters_snapshot();
        let mut gs: Vec<StoreGroup> = Vec::<StoreGroup>::new();
        let mut pend: Vec<WindPending> = Vec::<WindPending>::new();
        let mut viol_noted = false;
        let mut queue: Vec<DecodedMessage> = msgs;
        while !queue.is_empty() {
            let msg = queue.remove(0);
            let rp = resolve_param(mdl, &msg);
            match rp {
                None => { continue; }
                Some(_) => {}
            }
            // Owned handle on the resolved parameter (V1.2 phase B): the group
            // key/units are needed several times below, so unwrap ONCE instead of
            // moving `rp` repeatedly.
            let rpp = rp.unwrap();
            let wg = grid_winds(&msg.grid);
            if wg && !self.wind_violation {
                let mut names2: Vec<String> = Vec::<String>::new();
                for q2 in &mdl.params {
                    names2.push(q2.key.clone());
                }
                let viol = check_wind_pair(names2, true);
                match viol {
                    None => {}
                    Some(m) => {
                        self.note(format!("wind pair rule violated (model {}): {}", mdl.name, m));
                        self.wind_violation = true;
                        self.counters.errors += 1;
                        self.live.add(1, 0, 0, 0, 0, 0);
                    }
                }
            }
            if wg && self.wind_violation {
                // Sticky rule KEPT (a half-rotated wind field is nonsense), but
                // the note is logged at most ONCE per FILE and every dropped
                // component counts as a SKIP, not as an extra error.
                self.counters.skipped_messages += 1;
                self.live.add(0, 0, 1, 0, 0, 0);
                if !viol_noted {
                    self.note(format!("wind components skipped in {}: model {} already violated the pair rule", path, mdl.name));
                    viol_noted = true;
                }
                continue;
            }
            // Layer / range messages (two fixed surfaces, or a coordinate list
            // that survived decoding) cannot be stored as a single point value:
            // SKIP-AND-NOTE, counted separately from errors.
            if msg.levels.len() != 1 {
                self.note(format!("message with {} levels skipped (layer/range levels unsupported for point queries)", msg.levels.len()));
                self.counters.skipped_messages += 1;
                self.live.add(0, 0, 1, 0, 0, 0);
                continue;
            }
            // Fractional forecast hours are FIRST-CLASS data (V1.1 phase 3); only a
            // NEGATIVE offset (corrupt) is rejected, and as a skip, not an error.
            if msg.fc_secs < 0 {
                self.note(format!("negative forecast offset fc_secs={} skipped", msg.fc_secs));
                self.counters.skipped_messages += 1;
                self.live.add(0, 0, 1, 0, 0, 0);
                continue;
            }
            let lv = msg.levels[0];
            let fh = msg.fc_secs;
            let epoch = msg.run.epoch_secs;
            let dirn = run_dir_name(msg.run.clone());
            let stp = msg.stat.kind != StatKind::Instantaneous;
            let kind_name = stat_kind_label(msg.stat.kind.clone());
            // Configured parameter whose LEVEL is NOT requested: skip silently.
            // This used to happen inside `store_plane`; doing it here keeps an
            // unrequested level from ever creating a group (and therefore a write).
            if !level_spec_match(&rpp, lv.0, lv.1) {
                continue;
            }
            let is_wind = wg && named_wind_component(&msg);
            if is_wind {
                let key = (epoch, fh, lv.0, lv.1);
                let is_u = param_is_ugrd(&msg);
                let ki = find_wind(&pend, key);
                match ki {
                    None => {
                        let mut wp = WindPending { key: key, u: None, v: None,
                                                   ukey: "".to_string(), vkey: "".to_string() };
                        if is_u {
                            wp.u = Some(msg.data);
                            wp.ukey = rpp.key.clone();
                        } else {
                            wp.v = Some(msg.data);
                            wp.vkey = rpp.key.clone();
                        }
                        pend.push(wp);
                    }
                    Some(ki2) => {
                        if is_u {
                            pend[ki2].u = Some(msg.data);
                            pend[ki2].ukey = rpp.key.clone();
                        } else {
                            pend[ki2].v = Some(msg.data);
                            pend[ki2].vkey = rpp.key.clone();
                        }
                    }
                }
                let ki3 = find_wind(&pend, key);
                match ki3 {
                    Some(ki4) => {
                        if pend[ki4].u.is_some() && pend[ki4].v.is_some() {
                            // BOTH components present: rotate ONCE for the PAIR, then
                            // each component flows into its OWN group (two field files,
                            // ONE write each). The completed entry is REMOVED.
                            let wp = pend.remove(ki4);
                            let ugv = wp.u.unwrap();
                            let vgv = wp.v.unwrap();
                            let rot = rotate_winds_to_earth(&msg.grid, &ugv, &vgv);
                            let uu = rot.0;
                            let vv = rot.1;
                            let ru = resolve_param_by_key(mdl, wp.ukey.as_str());
                            let rv = resolve_param_by_key(mdl, wp.vkey.as_str());
                            match ru {
                                None => {}
                                Some(rux) => {
                                    if level_spec_match(&rux, lv.0, lv.1) {
                                        group_put(&mut gs, epoch, dirn.as_str(), &msg.grid,
                                            rux.key.as_str(), rux.units.as_str(), lv.0, fh,
                                            stp, kind_name.as_str(), stp,
                                            msg.stat.begin_secs, msg.stat.end_secs, msg.stat.step_secs,
                                            lv.1, uu);
                                    }
                                }
                            }
                            match rv {
                                None => {}
                                Some(rv2) => {
                                    if level_spec_match(&rv2, lv.0, lv.1) {
                                        group_put(&mut gs, epoch, dirn.as_str(), &msg.grid,
                                            rv2.key.as_str(), rv2.units.as_str(), lv.0, fh,
                                            stp, kind_name.as_str(), stp,
                                            msg.stat.begin_secs, msg.stat.end_secs, msg.stat.step_secs,
                                            lv.1, vv);
                                    }
                                }
                            }
                        }
                    }
                    None => {}
                }
                continue;
            }
            group_put(&mut gs, epoch, dirn.as_str(), &msg.grid, rpp.key.as_str(),
                      rpp.units.as_str(), lv.0, fh, stp, kind_name.as_str(), stp,
                      msg.stat.begin_secs, msg.stat.end_secs, msg.stat.step_secs,
                      lv.1, msg.data);
        }
        // ---- WIND MAP FULLY DRAINED PER FILE ---------------------------------
        // Anything left here is an ORPHAN component (its partner never arrived IN
        // THIS file). It is noted and counted as a SKIP, REMOVED, and NEVER stored
        // — so nothing leaks into the next file's pairing (the pre-B code simply
        // dropped these silently at the end of the function).
        while !pend.is_empty() {
            let wp = pend.remove(0);
            self.note(format!("wind component orphaned in {} at run/fh/level {:?}: only one of UGRD/VGRD present, message skipped", path, wp.key));
            self.counters.skipped_messages += 1;
            self.live.add(0, 0, 1, 0, 0, 0);
        }
        // ---- WRITE PHASE: ONE merge+write per GROUP --------------------------
        let mut dirs = TouchedDirs { dirs: Vec::<String>::new() };
        let mut runs: Vec<(i64, String)> = Vec::<(i64, String)>::new();
        let mut written = 0usize;
        let mut failed = 0usize;
        while !gs.is_empty() {
            let g = gs.remove(0);
            let mdir = join_path(&self.data_dir, mdl.name.as_str());
            let run_dir = join_path(&mdir, g.dir_name.as_str());
            let mk = create_dir_all(&run_dir);
            if mk.is_err() {
                self.note(format!("mkdir run dir failed: {}", run_dir));
                failed += 1;
                continue;
            }
            // grid.bin ONCE per run directory (per FILE, not per message).
            let gpath = format!("{}/grid.bin", run_dir);
            match stat_size_mtime(&gpath) {
                Ok(_) => {}
                Err(_) => {
                    let gw = write_grid_bin(&run_dir, &g.grid);
                    if gw.is_err() {
                        self.note(format!("grid.bin write failed: {}", run_dir));
                        failed += 1;
                        continue;
                    }
                    note_run(&mut runs, g.epoch, g.dir_name.clone());
                }
            }
            let lvl_dir = level_dir_name(g.level_code);
            let kdir = join_path(&run_dir, g.param_key.as_str());
            let fparent = join_path(&kdir, lvl_dir.as_str());
            let pd = create_dir_all(&fparent);
            if pd.is_err() {
                self.note(format!("mkdir field dir failed: {}", fparent));
                failed += 1;
                continue;
            }
            let fpath = join_path(&fparent, &fh_file_name(g.fh_secs));
            let scan_ns = grid_scan_ns(&g.grid);
            // V1.3 phase E0: the group's PDT 4.8 metadata goes into the FIELD
            // FILE HEADER, not only into manifest.json. That header is what a
            // warm start reads, so the accumulation guard survives a deleted,
            // corrupt or never-written manifest. The kind NAME maps to its
            // on-disk CODE through the one shared table
            // (`wx_core::storage::stat_kind_code`), which is also the table the
            // warm path uses in reverse, so the two paths cannot drift.
            let smeta = StatMeta {
                processed: g.stat_processed,
                kind: stat_kind_code(g.stat_kind.as_str()),
                begin_secs: g.stat_begin_secs,
                end_secs: g.stat_end_secs,
                step_secs: g.stat_step_secs,
            };
            // winds_grid=false: components are stored EARTH-relative (spec §4.4).
            let mw = merge_write_field_batch(fpath.to_string(), g.level_code, g.grid.nx(),
                                             g.grid.ny(), scan_ns, false, smeta, g.entries, Some(&mut dirs));
            match mw {
                Err(e) => {
                    self.note(format!("group write failed {}: {}", fpath, e));
                    failed += 1;
                    continue;
                }
                Ok(_nlv) => {
                    written += 1;
                    note_run(&mut runs, g.epoch, g.dir_name.clone());
                }
            }
            let mf = open_field(fpath.to_string());
            match mf {
                Err(e) => {
                    self.note(format!("open_field failed: {}", e));
                    failed += 1;
                    continue;
                }
                Ok(mfv) => {
                    // Cold path publishes the GROUP's metadata; the durable
                    // header just written carries exactly those bytes, so the
                    // warm path (which reads the header back) indexes the same
                    // guard and range. `mfv.header.stat` is available here too
                    // and is what the NEXT process will see.
                    self.registry_put(mdl, g.epoch, g.dir_name.clone(), g.grid.clone(),
                        FieldKey { param_key: g.param_key, level_code: g.level_code },
                        g.fh_secs, Arc::new(mfv), g.units, g.stat_processed, g.stat_kind.clone(),
                        g.stat_has_range, g.stat_begin_secs, g.stat_end_secs, g.stat_step_secs);
                }
            }
        }
        // ---- manifest.json ONCE per AFFECTED RUN -----------------------------
        // V1.3 phase E0: a FAILED manifest write now BLOCKS the REC_OK state
        // record (it increments the same `failed` counter a failed GROUP does),
        // so the source file is retried next round instead of being recorded as
        // fully ingested with no inventory. The retry is idempotent (groups are
        // already durable and rewrite through merge dedup + atomic rename).
        // Before this change the failure only logged + counted, and a crash
        // between the durable .bin writes and the manifest left exactly that
        // state permanently — the amplifier of the manifest-loss defect.
        for r in &runs {
            if !update_run_manifest(self, mdl.name.clone(), r.0) {
                self.note(format!("manifest update failed for run {0}: source file NOT marked ingested, retrying next round", r.1));
                // NO extra error count here: the caller's `failed_groups > 0`
                // branch is the single point where a file-with-any-write-
                // failure gets exactly ONE error (same rule as a failed GROUP,
                // see the "ONE error per file-with-failure" comment there).
                failed += 1;
            }
        }
        // ---- DIRECTORY FSYNCS ONCE per DISTINCT DIRECTORY TOUCHED ------------
        match dirs.flush() {
            Err(e) => {
                self.note(format!("directory fsync batch failed: {}", e));
                failed += 1;
            }
            Ok(_) => {}
        }
        // ---- PUBLISH ONCE per FILE -------------------------------------------
        self.publish();
        let c1 = counters_snapshot();
        return FileOutcome {
            groups: written,
            failed_groups: failed,
            bytes_written: c1.bytes_written - c0.bytes_written,
            renames: c1.renames - c0.renames,
            ms: (self.clock.elapsed_ns() - t0) / 1000000,
            runs_touched: runs.len(),
        };
    }
}

/// Remember a run touched by this file (deduped) so its manifest is refreshed
/// exactly ONCE afterwards.
fn note_run(runs: &mut Vec<(i64, String)>, epoch: i64, dir_name: String) {
    let mut k = 0;
    while k < runs.len() {
        if runs[k].0 == epoch {
            return;
        }
        k += 1;
    }
    runs.push((epoch, dir_name));
}

fn resolve_param(mdl: &ResolvedModel, msg: &DecodedMessage) -> Option<ResolvedParam> {
    for rp in &mdl.params {
        if rp.disc == msg.discipline && rp.cat == msg.category && rp.num == msg.number {
            return Some(rp.clone());
        }
    }
    return None;
}

/// True when the message's reverse-looked-up parameter name is UGRD.
fn param_is_ugrd(msg: &DecodedMessage) -> bool {
    match msg.param_name {
        None => { return false; }
        Some(pn) => { return pn == "UGRD"; }
    }
}

/// True when the message's reverse-looked-up parameter name is one of the two
/// wind components. A RAW-triple parameter whose (discipline,category,number)
/// maps to UGRD/VGRD through the built-in table matches too, which is exactly
/// what the earth-relative rotation must apply to.
fn named_wind_component(msg: &DecodedMessage) -> bool {
    match msg.param_name {
        None => { return false; }
        Some(pn) => { return pn == "UGRD" || pn == "VGRD"; }
    }
}

fn resolve_param_by_key(mdl: &ResolvedModel, key: &str) -> Option<ResolvedParam> {
    for rp in &mdl.params {
        if rp.key == key {
            return Some(rp.clone());
        }
    }
    return None;
}

// ---------------------------------------------------------------------------
// manifest.json maintenance (spec §4.5: what is present per run). Written
// atomically after every field write; the index remains authoritative AND, since
// V1.3 phase E0, so does the FIELD FILE HEADER for the statistical metadata that
// guards an answer (spec §6.3). A manifest failure therefore can no longer change
// an answer, but it DOES block the REC_OK state record so the file is retried and
// the inventory gets rebuilt. Levels listed are those of the newest forecast-hour
// file for the field (informational).
// ---------------------------------------------------------------------------

pub fn update_run_manifest(p: &Pipeline, model_name: String, epoch: i64) -> bool {
    let mi = find_model_idx(&p.registry, &model_name);
    match mi {
        None => { return false; }
        Some(mi2) => {
            let ri = find_run_idx(&p.registry[mi2].runs, epoch);
            match ri {
                None => { return false; }
                Some(ri2) => {
                    let dirn = p.registry[mi2].runs[ri2].dir_name.clone();
                    let mut out = format!("{{\"model\":\"{0}\",\"fields\":[", model_name);
                    let mut first = true;
                    for f in &p.registry[mi2].runs[ri2].fields {
                        if !first {
                            out = out + ",";
                        }
                        first = false;
                        let lvl_dir = level_dir_name(f.level_code);
                        let mut lv: Vec<f64> = Vec::<f64>::new();
                        if !f.fhs.is_empty() {
                            let mf = &f.fhs[f.fhs.len() - 1].field;
                            for x in &mf.header.levels {
                                lv.push(*x as f64);
                            }
                        }
                        // manifest forecast hours are written as decimal HOURS
                        // (human-facing); the exact keys live in the filenames.
                        let mut fhs: Vec<f64> = Vec::<f64>::new();
                        for e in &f.fhs {
                            fhs.push((e.fh_secs as f64) / 3600.0);
                        }
                        // per-fh PDT 4.8 ranges (spec §4.2): absolute ISO instants
                        // joined by exact fh_secs, so a restart restores them.
                        let run_ep = p.registry[mi2].runs[ri2].epoch_secs;
                        let rng_json = stat_ranges_json(&f.fhs, run_ep);
                        out = format!("{0}{1}", out, manifest_field_json(f.param_key.as_str(), lvl_dir.as_str(),
                            f.units.as_str(), f.stat_processed, f.stat_kind.clone(), lv, fhs, rng_json));
                    }
                    out = out + "]}";
                    let mdir = join_path(&p.data_dir, model_name.as_str());
                    let rdir = join_path(&mdir, dirn.as_str());
                    let mpath = format!("{0}/manifest.json", rdir);
                    let w = write_bytes_atomic_pub(&mpath, out.as_bytes());
                    match w {
                        Err(_) => { return false; }
                        Ok(_) => { return true; }
                    }
                }
            }
        }
    }
}

// (V1.2 phase B) the per-message `store_plane` path is GONE: `ingest_file`
// groups a file's messages per (run, param, level type, forecast hour) and writes
// each group ONCE, so mkdir/grid.bin/manifest/publish happen per FILE instead of
// per MESSAGE. The single-level merge+write primitives stay available in `store`.

pub fn level_spec_match(rp: &ResolvedParam, code: u8, val: f64) -> bool {
    for sp in &rp.level_specs {
        if sp.0 == 255 || sp.0 == code {
            match sp.1 {
                None => { return true; }
                Some(w) => {
                    if (w - val).abs() <= 1e-9 {
                        return true;
                    }
                }
            }
        }
    }
    return false;
}

fn grid_scan_ns(g: &GridDef) -> bool {
    match g {
        GridDef::Regular(r) => { return (r.scan_mode & 0x40) == 0; }
        GridDef::Lambert(l) => { return l.j_south; }
    }
}

/// Canonical lower-case name of a decoded stat kind. Same table as
/// `wx_core::storage::stat_kind_name` (which maps the ON-DISK u8 code), so the
/// cold path's string, the header's code and `/v1/inventory`'s label can never
/// drift apart (V1.3 phase E0 renamed this from `stat_kind_name` to keep the
/// two directions distinct and non-conflicting).
fn stat_kind_label(k: StatKind) -> String {
    match k {
        StatKind::Instantaneous => { return "instantaneous".to_string(); }
        StatKind::Accumulation => { return "accumulation".to_string(); }
        StatKind::Average => { return "average".to_string(); }
        StatKind::Maximum => { return "maximum".to_string(); }
        StatKind::Minimum => { return "minimum".to_string(); }
        StatKind::Other => { return "other".to_string(); }
    }
}

// ---------------------------------------------------------------------------
// snapshot building + retention
// ---------------------------------------------------------------------------

pub fn build_snapshot(reg: &Vec<RegModel>) -> IndexSnapshot {
    let mut snap = IndexSnapshot { models: Vec::<IdxModel>::new() };
    for rm in reg {
        let mut im = IdxModel { name: rm.name.clone(), runs: Vec::<IdxRun>::new() };
        for rr in &rm.runs {
            let mut ir = IdxRun {
                epoch_secs: rr.epoch_secs,
                dir_name: rr.dir_name.clone(),
                grid: Arc::new(rr.grid.clone()),
                fields: Vec::<IdxFieldEntry>::new(),
            };
            for rf in &rr.fields {
                let mut ifs: Vec<IdxFh> = Vec::<IdxFh>::new();
                for fhv in &rf.fhs {
                    ifs.push(IdxFh {
                        fh_secs: fhv.fh_secs,
                        field: fhv.field.clone(),
                        stat_has_range: fhv.stat_has_range,
                        stat_begin_secs: fhv.stat_begin_secs,
                        stat_end_secs: fhv.stat_end_secs,
                        stat_step_secs: fhv.stat_step_secs,
                    });
                }
                ir.fields.push(IdxFieldEntry {
                    param_key: rf.param_key.clone(),
                    level_code: rf.level_code,
                    fd: IdxField {
                        units: rf.units.clone(),
                        stat_processed: rf.stat_processed,
                        stat_kind: rf.stat_kind.clone(),
                        fhs: ifs,
                    },
                });
            }
            im.runs.push(ir);
        }
        snap.models.push(im);
    }
    return snap;
}

/// Retention (spec §4.6): remove from the registry and PUBLISH first, then
/// delete the tree; old snapshots keep mmap Arcs alive until readers drop
/// them (unlinking does not invalidate live mappings on Linux).
pub fn retention_prune(p: &mut Pipeline) {
    let rcount = p.registry.len();
    for mi in 0 .. rcount {
        while p.registry[mi].runs.len() > (p.retention_runs as usize) {
            let victim_dir = p.registry[mi].runs[0].dir_name.clone();
            p.registry[mi].runs.remove(0);
            p.publish();
            let mdir2 = join_path(&p.data_dir, p.registry[mi].name.as_str());
            let path = join_path(&mdir2, victim_dir.as_str());
            delete_tree(&path);
        }
    }
}

fn delete_tree(dir: &str) {
    match read_dir(&dir) {
        Err(_) => {}
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let pth = path_to_str(&de.path());
                        match read_dir(&pth) {
                            Ok(_) => { delete_tree(&pth); }
                            Err(_) => { let _r = remove_file(&pth); }
                        }
                    }
                }
            }
        }
    }
    let _d = remove_dir(&dir);
}
