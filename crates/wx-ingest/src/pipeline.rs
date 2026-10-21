//! The ingest pipeline driver (spec §4.1 discovery+stability, §4.5 atomic
//! per-fh writes, §4.6 retention order, §5 arc-swap publication, §6 threading
//! counters). Scan rounds drain notify events first (latency) but ALWAYS
//! reconcile with a full recursive scan (authoritative on network mounts).

use wx_core::grid::{GridDef};
use crate::grib2::{DecodeStats, decode_files_parallel, rotate_winds_to_earth, check_wind_pair, DecodedMessage, ParamFilter, StatKind, RunTime, GribErr, grid_winds, epoch_secs};
use crate::config::{ResolvedModel, ResolvedParam, validate_config, load_config, DEFAULT_INGEST_THREADS, DEFAULT_STABLE_WINDOW_SECS};
use serde_json::from_str;
use crate::state::{StateStore, FileRec, content_hash_file, stat_size_mtime, now_ns_marker, join_path};
use crate::discovery::{glob_match, walk_files, StabilityTracker, WatchQueue, start_watchers, drain_queue, base_name, path_to_str};
use crate::store::{write_grid_bin, merge_field, write_field, decode_grid, manifest_field_json, write_bytes_atomic_pub, iso_utc, parse_iso_utc};
use crate::index::{SharedIndex, IndexSnapshot, IdxModel, IdxRun, IdxFieldEntry, IdxField, IdxFh};
use wx_core::storage::{open_field, MmapField};
use memmap2::Mmap;
use notify::RecommendedWatcher;
use wx_core::tables::{canonical_level_name, lookup_level_type, param_units, digit_at};
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::sync::Arc;
use std::sync::Mutex;
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
    /// (layer/range levels, coordinate lists, unknown grid/PDT templates).
    /// Spec §4.2 wants these noted, not failed: they are counted separately
    /// and NEVER land in `errors` (mirrored to /metrics as
    /// wxpoint_skipped_messages_total).
    pub skipped_messages: u64,
    pub last_mtime_ns: i64, // max mtime over ingested files (lag input)
}

pub struct Pipeline {
    pub models: Vec<ResolvedModel>,
    pub data_dir: String,
    pub retention_runs: u32,
    pub scan_interval_secs: u32,
    pub stable_window_ns: i64,
    pub state: StateStore,
    pub stability: StabilityTracker,
    pub registry: Vec<RegModel>,
    // Arc-wrapped so milestone 4's HTTP handlers can hold a clone of the SAME
    // ArcSwap while the Pipeline itself lives on the ingest thread.
    pub shared: Arc<SharedIndex>,
    pub counters: IngestCounters,
    pub wind_violation: bool,
    pub queue: WatchQueue,
    pub watchers: Vec<RecommendedWatcher>,
    pub pool: ThreadPool,
    pub log: Vec<String>,
}

impl Pipeline {
    /// Record an ingest diagnostic message (bounded).
    pub fn note(self: &mut Self, msg: String) {
        if self.log.len() < 512 {
            self.log.push(msg);
        }
    }
}

// ---------------------------------------------------------------------------
// naming helpers
// ---------------------------------------------------------------------------

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

/// Run-directory name. Base is the classic `YYYYMMDDHH`; when the run MINUTE
/// is non-zero `mm` is appended, and when the run SECOND is also non-zero
/// `ss` after that (so 10/12/14-char names, disambiguated by length). This
/// removes the collision where two runs 12Z and 12:30Z on one day both mapped
/// to `2026101912` (V1.1 phase 3 bug (a)). Pre-existing 10-char directories
/// keep parsing exactly as before: minute/second default to 0.
pub fn run_dir_name(rt: RunTime) -> String {
    let a = pad_num(rt.year as i64, 4);
    let b = pad_num(rt.month as i64, 2);
    let c = pad_num(rt.day as i64, 2);
    let d = pad_num(rt.hour as i64, 2);
    let ab = format!("{0}{1}", a, b);
    let cd = format!("{0}{1}", c, d);
    let base = format!("{0}{1}", ab, cd);
    if rt.minute != 0 {
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
        let q = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut p = Pipeline {
            models: models,
            data_dir: data_dir,
            retention_runs: cfg.ingest.retention_runs,
            scan_interval_secs: cfg.ingest.scan_interval_secs,
            stable_window_ns: (window as i64) * 1000000000,
            state: st.unwrap(),
            stability: StabilityTracker { obs: Vec::<(String, u64, i64, i64)>::new() },
            registry: Vec::<RegModel>::new(),
            shared: Arc::new(SharedIndex { store: ArcSwap::from_pointee(IndexSnapshot { models: Vec::<IdxModel>::new() }) }),
            counters: IngestCounters { ingested_files: 0, errors: 0, skipped_messages: 0, last_mtime_ns: 0 },
            wind_violation: false,
            queue: q,
            watchers: Vec::<RecommendedWatcher>::new(),
            pool: pool.unwrap(),
            log: Vec::<String>::new(),
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
                Err(_) => { return; }
                Ok(grid) => {
                    let mp = format!("{}/manifest.json", run_dir);
                    let mt = read_text_maybe(&mp);
                    let mut doc = ManifestDoc { model: mdl.name.clone(), fields: Vec::<ManifestField>::new() };
                    match mt {
                        None => {}
                        Some(txt) => {
                            match from_str::<ManifestDoc>(&txt) {
                                Err(_) => {}
                                Ok(d) => { doc = d; }
                            }
                        }
                    }
                    match read_dir(&run_dir) {
                        Err(_) => {}
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
                                                            let code = level_code_of_dir(&lvl_dir);
                                                            match code {
                                                                None => { continue; }
                                                                Some(code2) => {
                                                                    match read_dir(&ldir) {
                                                                        Err(_) => { continue; }
                                                                        Ok(fit) => {
                                                                            for fe in fit {
                                                                                match fe {
                                                                                    Err(_) => { continue; }
                                                                                    Ok(fde) => {
                                                                                        let fp = path_to_str(&fde.path());
                                                                                        let bfp = base_name(fp.to_string());
                        let fh = parse_fh_name(&bfp);
                                                                                        match fh {
                                                                                            None => { continue; }
                                                                                            Some(fhv) => {
                                                                                                let mf = open_field(fp.to_string());
                                                                                                match mf {
                                                                                                    Err(_) => { continue; }
                                                                                                    Ok(mfv) => {
                                                                                                        let un = units_for(p, mdl, &param_key, code2);
let spc = stat_for(&doc, &param_key, &lvl_dir);
// restore this forecast hour's 4.8 time range from manifest.json
// so a restart keeps BOTH the guard and the range metadata.
let rng = stat_range_for(&doc, &param_key, &lvl_dir, fhv, epoch);
p.registry_put(mdl, epoch, dir_name.clone(), grid.clone(),
    FieldKey { param_key: param_key.clone(), level_code: code2 },
    fhv, Arc::new(mfv), un, spc.0, spc.1,
    rng.0, rng.1, rng.2, rng.3);
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
                        }
                    }
                }
            }
        }
    }
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

/// Restore one forecast hour's PDT 4.8 statistical time range from
/// manifest.json: returns (has_range, begin_secs, end_secs, step_secs) where
/// begin/end are offsets from the run time (`run_epoch` supplied by the caller)
/// so they match what the writer stored. Absent or unparsable -> (false,0,0,0)
/// and the field still gets its `stat_processed` guard from `stat_for`.
fn stat_range_for(doc: &ManifestDoc, param_key: &str, lvl_dir: &str,
                  fh_secs: i64, run_epoch: i64) -> (bool, i64, i64, i64) {
    for f in &doc.fields {
        if f.param != param_key || f.level_type != lvl_dir {
            continue;
        }
        for r in &f.stat_ranges {
            if r.fh_secs != fh_secs {
                continue;
            }
            let b = parse_iso_utc(&r.begin);
            match b {
                None => { return (true, 0, 0, r.step_secs); }
                Some(bv) => {
                    let en = parse_iso_utc(&r.end);
                    match en {
                        None => { return (true, bv - run_epoch, 0, r.step_secs); }
                        Some(ev) => {
                            return (true, bv - run_epoch, ev - run_epoch, r.step_secs);
                        }
                    }
                }
            }
        }
    }
    return (false, 0, 0, 0);
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

fn stat_for(doc: &ManifestDoc, param_key: &str, lvl_dir: &str) -> (bool, String) {
    for f in &doc.fields {
        if f.param == param_key && f.level_type == lvl_dir {
            return (f.stat_processed, f.stat_kind.clone());
        }
    }
    return (false, "instantaneous".to_string());
}

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
    pub fn scan_round(self: &mut Self) -> usize {
        let now_ns = now_ns_marker(&self.state.dir_path);
        let mut ingested = 0;
        for mi in 0 .. self.models.len() {
            let mdl = self.models[mi].clone();
            let mut cands: Vec<String> = Vec::<String>::new();
            let evs = drain_queue(self.queue.clone());
            for e in &evs {
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
            let mut triples: Vec<(u8, u8, u8)> = Vec::<(u8, u8, u8)>::new();
            for rp in &mdl.params {
                triples.push((rp.disc, rp.cat, rp.num));
            }
            let mut names: Vec<String> = Vec::<String>::new();
            for rdy in &ready {
                names.push(rdy.0.clone());
            }
            let results = self.pool.install(|| decode_files_parallel(names.clone(), ParamFilter::Only(triples.clone())));
            let mut rlist: Vec<Result<(Vec<DecodedMessage>, DecodeStats), GribErr>> = Vec::<Result<(Vec<DecodedMessage>, DecodeStats), GribErr>>::new();
            for rr in results {
                rlist.push(rr);
            }
            let mut fpos = 0;
            while !rlist.is_empty() {
                let one = rlist.remove(0);
                match one {
                    Err(e) => {
                        self.note(format!("grib decode {}: {:?}", names[fpos], e));
                        self.counters.errors += 1;
                    }
                    Ok(dr) => {
                        // structural SKIP-AND-NOTE bookkeeping (spec §4.2):
                        // coordinate-list / unknown-template messages counted
                        // separately from errors and logged for operators.
                        if dr.1.messages_unsupported > 0 {
                            self.note(format!("{} message(s) skipped as structurally unsupported in {}",
                                dr.1.messages_unsupported, names[fpos]));
                            self.counters.skipped_messages += dr.1.messages_unsupported as u64;
                        }
                        self.ingest_messages(&mdl, dr.0);
                        self.state.upsert(FileRec {
                            path: names[fpos].clone(),
                            size: ready[fpos].1,
                            mtime_ns: ready[fpos].2,
                            hash: ready[fpos].3.clone(),
                        });
                        if ready[fpos].2 > self.counters.last_mtime_ns {
                            self.counters.last_mtime_ns = ready[fpos].2;
                        }
                        self.counters.ingested_files += 1;
                        ingested += 1;
                    }
                }
                fpos += 1;
            }
        }
        let _ = self.state.save();
        retention_prune(self);
        return ingested;
    }

    /// Publish the current registry as a new immutable snapshot atomically.
    pub fn publish(self: &Self) {
        let snap = build_snapshot(&self.registry);
        self.shared.store.swap(Arc::new(snap));
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
}

fn find_wind(pend: &Vec<WindPending>, key: (i64, i64, u8, f64)) -> Option<usize> {
    for k in 0 .. pend.len() {
        if pend[k].key == key {
            return Some(k);
        }
    }
    return None;
}

impl Pipeline {
    pub fn ingest_messages(self: &mut Self, mdl: &ResolvedModel, msgs: Vec<DecodedMessage>) {
        let mut pend: Vec<WindPending> = Vec::<WindPending>::new();
        for msg in &msgs {
            let rp = resolve_param(mdl, msg);
            match rp {
                None => { continue; }
                Some(_) => {}
            }
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
                    }
                }
            }
            if wg && self.wind_violation {
                continue;
            }
            let mut is_wind = false;
            match msg.param_name {
                None => {}
                Some(pn) => {
                    if pn == "UGRD" || pn == "VGRD" {
                        is_wind = wg;
                    }
                }
            }
            // Layer / range messages (two fixed surfaces, or a coordinate list
            // that survived decoding) cannot be stored as a single point value:
            // SKIP-AND-NOTE, counted separately from errors (spec §4.2 spirit:
            // skipping is not failing). Point queries are out of scope for
            // layers per spec §13, but the policy must not poison the error
            // counter nor abort the file.
            if msg.levels.len() != 1 {
                self.note(format!("message with {} levels skipped (layer/range levels unsupported for point queries)", msg.levels.len()));
                self.counters.skipped_messages += 1;
                continue;
            }
            // Fractional forecast hours are FIRST-CLASS data (V1.1 phase 3):
            // CT 4.4 units minute/hour/day/3h/6h/12h/second all normalize to
            // whole SECONDS, so `fc_secs` is the storage key directly. Only a
            // NEGATIVE offset (unrepresentable in section 4's unsigned field,
            // i.e. corrupt input) is rejected, and as a skip, not an error.
            if msg.fc_secs < 0 {
                self.note(format!("negative forecast offset fc_secs={} skipped", msg.fc_secs));
                self.counters.skipped_messages += 1;
                continue;
            }
            let lv = msg.levels[0];
            let fh = msg.fc_secs;
            if is_wind {
                let key = (msg.run.epoch_secs, fh, lv.0, lv.1);
                let is_u = param_is_ugrd(&msg);
                let ki = find_wind(&pend, key);
                match ki {
                    None => {
                        let mut wp = WindPending { key: key, u: None, v: None };
                        if is_u { wp.u = Some(msg.data.clone()); } else { wp.v = Some(msg.data.clone()); }
                        pend.push(wp);
                    }
                    Some(ki2) => {
                        if is_u { pend[ki2].u = Some(msg.data.clone()); } else { pend[ki2].v = Some(msg.data.clone()); }
                    }
                }
                let ki3 = find_wind(&pend, key);
                match ki3 {
                    Some(ki4) => {
                        if pend[ki4].u.is_some() && pend[ki4].v.is_some() {
                            let u4 = pend[ki4].u.clone().unwrap();
                            let v4 = pend[ki4].v.clone().unwrap();
                            let rot = rotate_winds_to_earth(&msg.grid, &u4, &v4);
                            let uu = rot.0;
                            let vv = rot.1;
                            let rp_u = resolve_param_by_key(mdl, "UGRD");
                            let rp_v = resolve_param_by_key(mdl, "VGRD");
                            match rp_u {
                                None => {}
                                Some(ru) => {
                                    self.store_plane(mdl, msg, ru, uu, lv.0, lv.1, fh);
                                }
                            }
                            match rp_v {
                                None => {}
                                Some(rv) => {
                                    self.store_plane(mdl, msg, rv, vv, lv.0, lv.1, fh);
                                }
                            }
                            pend.remove(ki4);
                        }
                    }
                    None => {}
                }
                continue;
            }
            self.store_plane(mdl, msg, rp.unwrap(), msg.data.clone(), lv.0, lv.1, fh);
        }
    }
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
// atomically after every field write; the index remains authoritative, so a
// manifest failure is logged but does not block publication. Levels listed
// are those of the newest forecast-hour file for the field (informational).
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

impl Pipeline {
    /// Store one plane under (param, level type, forecast-hour offset in
    /// SECONDS), merging any existing levels in that file, then PUBLISH
    /// (forecast-hour granularity, spec §4.5 incremental visibility).
    pub fn store_plane(self: &mut Self, mdl: &ResolvedModel, msg: &DecodedMessage,
                       rp: ResolvedParam, plane: Vec<f32>, level_code: u8,
                       level_val: f64, fh_secs: i64) {
        if !level_spec_match(&rp, level_code, level_val) {
            return; // configured param but level not requested: skip silently
        }
        let epoch = msg.run.epoch_secs;
        let dirn = run_dir_name(msg.run.clone());
        let mdir = join_path(&self.data_dir, mdl.name.as_str());
        let run_dir = join_path(&mdir, dirn.as_str());
        let mk = create_dir_all(&run_dir);
        if mk.is_err() {
            self.note(format!("mkdir run dir failed: {}", run_dir));
            self.counters.errors += 1;
            return;
        }
        let gpath = format!("{}/grid.bin", run_dir);
        match stat_size_mtime(&gpath) {
            Ok(_) => {}
            Err(_) => {
                let gw = write_grid_bin(&run_dir, &msg.grid);
                if gw.is_err() {
                    self.note(format!("grid.bin write failed: {}", run_dir));
                    self.counters.errors += 1;
                    return;
                }
            }
        }
        let lvl_dir = level_dir_name(level_code);
        let kdir = join_path(&run_dir, rp.key.as_str());
        let fparent = join_path(&kdir, lvl_dir.as_str());
        let pd = create_dir_all(&fparent);
        if pd.is_err() {
            self.note(format!("mkdir field dir failed: {}", fparent));
            self.counters.errors += 1;
            return;
        }
        let fpath = join_path(&fparent, &fh_file_name(fh_secs));
        let newv: Vec<(f64, Vec<f32>)> = [(level_val, plane)].to_vec();
        let merged = merge_field(fpath.to_string(), level_code, newv);
        match merged {
            Err(e) => {
                self.note(format!("merge failed {}: {}", fpath, e));
                self.counters.errors += 1;
                return;
            }
            Ok(mg) => {
                let scan_ns = grid_scan_ns(&msg.grid);
                let wf = write_field(fpath.to_string(), level_code, msg.grid.nx(), msg.grid.ny(),
                                     scan_ns, false, mg);
                if wf.is_err() {
                    self.note(format!("field write failed: {}", fpath));
                    self.counters.errors += 1;
                    return;
                }
            }
        }
        let mf = open_field(fpath.to_string());
        match mf {
            Err(e) => {
                self.note(format!("open_field failed: {}", e));
                self.counters.errors += 1;
                return;
            }
            Ok(mfv) => {
                let stp = msg.stat.kind != StatKind::Instantaneous;
                // PDT 4.8 messages carry their statistical time range (offsets
                // from the run time in seconds); PDT 4.0 has none.
                let has_rng = stp;
                self.registry_put(mdl, epoch, dirn.clone(), msg.grid.clone(),
                    FieldKey { param_key: rp.key, level_code: level_code },
                    fh_secs, Arc::new(mfv), rp.units, stp, stat_kind_name(msg.stat.kind.clone()),
                    has_rng, msg.stat.begin_secs, msg.stat.end_secs, msg.stat.step_secs);
                let mok = update_run_manifest(self, mdl.name.clone(), epoch);
                if !mok {
                    self.note(format!("manifest update failed for run {0}", dirn));
                    self.counters.errors += 1;
                }
                self.publish();
            }
        }
    }
}

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

fn stat_kind_name(k: StatKind) -> String {
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
