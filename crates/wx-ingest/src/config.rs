//! Configuration schema + startup validation (spec §3).
//!
//! TOML keys beyond the spec example (all optional, defaults documented):
//! * `[ingest].stable_window_secs` (default 10): size+mtime must be unchanged
//!   for this long before a file is eligible under the window strategy.
//! * `[ingest].ingest_threads` (default 4): rayon pool size for decode work.
//! * `[ingest].decode_batch_files` (default 4, V1.2 phase B): how many candidate
//!   files are DECODED as one chunk before ANY of them is written. Cross-file
//!   parallelism is preserved inside a chunk; peak decode RAM is ~K files; the
//!   FIRST queryable output lands after the first file of the first chunk instead
//!   of after the slowest file of the WHOLE directory. Must be >= 1.
//! * `[[models]].sidecar_suffix` (absent by default): when present the model
//!   uses the sidecar-marker stability strategy instead of the window — a
//!   candidate `<file>` is eligible only once `<file><suffix>` exists.
//! * Raw parameter override: a `[[models.params]]` entry may omit `name` and
//!   give `discipline`, `category`, `number` integers directly (spec §3:
//!   "Allow the config to specify raw discipline/category/number"). Units are
//!   then reported as "" (unknown) unless the triple happens to match a table
//!   entry, in which case the canonical name/units are used. V1.3 phase E2:
//!   such an out-of-table entry MAY carry `units = "..."` (the ONLY place the
//!   key is legal — setting it on a table-known param is a hard startup error,
//!   and a raw param WITHOUT units boots with a loud note per affected entry,
//!   because spec §7 requires every response to report units).
//! * Level specs: `"type:value"`, bare `"type"` (value 0, e.g. `surface`),
//!   `"type:*"` (every value of that type present) or `"*"` (any level).

use std::fs::read_to_string;
use std::fs::canonicalize;
use std::env::current_dir;
use toml::from_str;
use wx_core::tables::lookup_level_type;
use wx_core::tables::param_code;
use wx_core::tables::param_units;
use wx_core::tables::param_name_for;
use wx_core::tables::parse_f64;

/// Default stable-window seconds when the key is absent (spec §4.1).
pub const DEFAULT_STABLE_WINDOW_SECS: u32 = 10;
/// Default ingest decode pool thread count when the key is absent.
pub const DEFAULT_INGEST_THREADS: u32 = 4;
/// Default number of files decoded per chunk before consumption starts (V1.2
/// phase B). K bounds peak decode memory at ~K files while keeping cross-file
/// parallelism; K == 1 makes ingest strictly sequential per file (lowest latency
/// to first output, lowest RAM).
pub const DEFAULT_DECODE_BATCH_FILES: u32 = 4;

#[derive(serde::Deserialize)]
pub struct ServerCfg {
    pub bind: String,
    pub data_dir: String,
}

#[derive(serde::Deserialize)]
pub struct IngestCfg {
    pub scan_interval_secs: u32,
    pub retention_runs: u32,
    pub stable_window_secs: Option<u32>,
    pub ingest_threads: Option<u32>,
    // V1.2 phase B: chunk size of the DECODE stage (see module docs).
    pub decode_batch_files: Option<u32>,
}

#[derive(serde::Deserialize)]
pub struct ParamCfg {
    pub name: Option<String>,
    pub discipline: Option<i64>,
    pub category: Option<i64>,
    pub number: Option<i64>,
    pub levels: Vec<String>,
    /// V1.3 phase E2 UNITS OVERRIDE, legal ONLY on raw discipline/category/number
    /// entries that the built-in table does not know (their units would
    /// otherwise be ""). Setting it anywhere else fails validation naming the
    /// param — see docs/DECISIONS.md V1.3 E2.
    pub units: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct ModelCfg {
    pub name: String,
    pub ingest_dir: String,
    pub file_glob: String,
    pub sidecar_suffix: Option<String>,
    pub params: Vec<ParamCfg>,
}

#[derive(serde::Deserialize)]
pub struct PointConfig {
    pub server: ServerCfg,
    pub ingest: IngestCfg,
    pub models: Vec<ModelCfg>,
}

/// One resolved parameter: storage/index key, GRIB triple filter code,
/// native units ("" = unknown raw override) and accepted level specs.
#[derive(Clone, Debug)]
pub struct ResolvedParam {
    pub key: String,
    pub disc: u8,
    pub cat: u8,
    pub num: u8,
    pub units: String,
    /// V1.3 phase E2: true iff this entry came from a raw discipline/category/
    /// number triple that the built-in table does NOT know (units therefore
    /// come from the optional `units` config key or are ""). Drives the loud
    /// startup note for unit-less raw params; never used on the query path.
    pub raw: bool,
    // (CT 4.5 code, wanted value or None=wildcard); code 255 = any type.
    pub level_specs: Vec<(u8, Option<f64>)>,
}

#[derive(Clone, Debug)]
pub struct ResolvedModel {
    pub name: String,
    pub ingest_dir: String,
    pub file_glob: String,
    pub sidecar_suffix: Option<String>,
    pub params: Vec<ResolvedParam>,
}

/// Parse one level spec string (see module docs). None on malformed input.
pub fn parse_level_spec(s: &str) -> Option<(u8, Option<f64>)> {
    if s == "*" {
        return Some((255, None));
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 2 {
        return None;
    }
    let code = lookup_level_type(parts[0]);
    match code {
        None => { return None; }
        Some(c) => {
            if parts.len() == 1 {
                return Some((c, Some(0.0)));
            }
            if parts[1] == "*" {
                return Some((c, None));
            }
            let v = parse_f64(parts[1]);
            match v {
                None => { return None; }
                Some(x) => { return Some((c, Some(x))); }
            }
        }
    }
}

/// Validate the whole config (spec §3: fail loudly at startup) and produce
/// resolved models. Errors are human-readable strings for main().
/// Resolve one configured parameter entry to (discipline, category, number,
/// canonical-or-raw key, units, out-of-table-raw-flag). Errors carry the
/// human message (spec §3). V1.3 phase E2: the optional `units` key is
/// accepted ONLY for raw triples the built-in table does not know; setting
/// it on anything the table already resolves (by name OR by an in-table
/// triple) is a HARD error naming the param — silently overriding known
/// units invites wrong-unit answers (decision: hard error over warn+
/// override, docs/DECISIONS.md V1.3 E2).
fn resolve_entry(p: &ParamCfg, mname: &str) -> Result<(u8, u8, u8, String, String, bool), String> {
    let has_name = p.name.is_some();
    let has_raw = p.discipline.is_some() && p.category.is_some() && p.number.is_some();
    if has_name && has_raw {
        return Err(format!("param entry in model '{0}' sets both name and raw discipline/category/number", mname));
    }
    if !has_name && !has_raw {
        return Err(format!("param entry in model '{0}': needs name or full discipline/category/number triple", mname));
    }
    if has_name {
        let nm = p.name.clone().unwrap();
        match param_code(&nm) {
            None => {
                return Err(format!("unknown parameter '{0}' in model '{1}': not in the built-in table (or supply raw discipline/category/number)", nm, mname));
            }
            Some(trip) => {
                match &p.units {
                    None => {}
                    Some(_) => {
                        return Err(format!("units override is only valid for raw discipline/category/number params outside the built-in table: parameter '{0}' in model '{1}' already carries units '{2}'", nm, mname, param_units(&nm).unwrap()));
                    }
                }
                let un = param_units(&nm).unwrap().to_string();
                return Ok((trip.0, trip.1, trip.2, nm.to_string(), un, false));
            }
        }
    }
    let d64 = p.discipline.unwrap();
    let c64 = p.category.unwrap();
    let n64 = p.number.unwrap();
    if d64 < 0 || d64 > 255 || c64 < 0 || c64 > 255 || n64 < 0 || n64 > 255 {
        return Err(format!("raw param triple out of byte range in model '{0}'", mname));
    }
    let du = d64 as u8;
    let cu = c64 as u8;
    let nu = n64 as u8;
    let known = param_name_for(du, cu, nu);
    match known {
        None => {
            let un = match &p.units {
                None => { "".to_string() }
                Some(u) => { u.clone() }
            };
            return Ok((du, cu, nu, format!("d{0}c{1}n{2}", du, cu, nu), un, true));
        }
        Some(kn) => {
            match &p.units {
                None => {}
                Some(_) => {
                    return Err(format!("units override is only valid for raw discipline/category/number params outside the built-in table: discipline {0}/category {1}/number {2} in model '{3}' IS parameter '{4}' with units '{5}'", du, cu, nu, mname, kn, param_units(kn).unwrap()));
                }
            }
            return Ok((du, cu, nu, kn.to_string(), param_units(&kn).unwrap().to_string(), false));
        }
    }
}

pub fn validate_config(cfg: &PointConfig) -> Result<Vec<ResolvedModel>, String> {
    if cfg.models.is_empty() {
        return Err("config has no [[models]] entries".to_string());
    }
    if cfg.ingest.retention_runs < 1 {
        return Err("ingest.retention_runs must be >= 1".to_string());
    }
    if cfg.ingest.scan_interval_secs < 1 {
        return Err("ingest.scan_interval_secs must be >= 1".to_string());
    }
    match cfg.ingest.ingest_threads {
        None => {}
        Some(t) => {
            if t < 1 {
                return Err("ingest.ingest_threads must be >= 1".to_string());
            }
        }
    }
    match cfg.ingest.decode_batch_files {
        None => {}
        Some(k) => {
            if k < 1 {
                return Err("ingest.decode_batch_files must be >= 1".to_string());
            }
        }
    }
    // ---- V1.2 phase C: data_dir / ingest_dir OVERLAP GUARD -----------------
    // Ingest WRITES into `<data_dir>/<model>/...` and READS from
    // `<ingest_dir>`. If one directory contains the OTHER, the two sides fight:
    // retention (§4.6) deletes run trees that may BE the source products, and
    // atomic renames can land inside the watched ingest tree where the glob then
    // matches them again (self-ingestion). Rejected at startup with BOTH paths
    // named, exactly like every other validation failure.
    let nd = norm_dir_path(&cfg.server.data_dir);
    let nparts = path_parts(&nd);
    for model in &cfg.models {
        let ni = norm_dir_path(&model.ingest_dir);
        let rel = dir_relation(&nparts, &path_parts(&ni));
        if rel != 0 {
            return Err(format!("config error: [server].data_dir \"{0}\" and model '{1}' ingest_dir \"{2}\" OVERLAP ({3}); normalized: \"{4}\" vs \"{5}\". The query-optimized store must NOT contain, or be contained in, a model's ingest directory.",
                cfg.server.data_dir, model.name, model.ingest_dir, relation_text(rel), nd, ni));
        }
    }
    let mut out: Vec<ResolvedModel> = Vec::<ResolvedModel>::new();
    for model in &cfg.models {
        if model.name.is_empty() {
            return Err("a model name is empty".to_string());
        }
        for prev in &out {
            if prev.name == model.name {
                return Err(format!("duplicate model name '{}'", model.name));
            }
        }
        if model.ingest_dir.is_empty() {
            return Err(format!("model '{}' has empty ingest_dir", model.name));
        }
        if model.file_glob.is_empty() {
            return Err(format!("model '{}' has empty file_glob", model.name));
        }
        let mut rm = ResolvedModel {
            name: model.name.to_string(),
            ingest_dir: model.ingest_dir.to_string(),
            file_glob: model.file_glob.to_string(),
            sidecar_suffix: model.sidecar_suffix.clone(),
            params: [].to_vec(),
        };
        for p in &model.params {
            let t5 = resolve_entry(p, model.name.as_str());
            match t5 {
                Err(e) => { return Err(e); }
                Ok(t) => {
                    if p.levels.is_empty() {
                        return Err(format!("parameter '{0}' in model '{1}' lists no levels", t.3, model.name));
                    }
                    for prev in &rm.params {
                        if prev.key == t.3 {
                            return Err(format!("duplicate parameter '{0}' in model '{1}'", t.3, model.name));
                        }
                    }
                    let mut rp = ResolvedParam {
                        key: t.3,
                        disc: t.0,
                        cat: t.1,
                        num: t.2,
                        units: t.4,
                        raw: t.5,
                        level_specs: [].to_vec(),
                    };
                    for lv in &p.levels {
                        match parse_level_spec(&lv) {
                            None => {
                                return Err(format!("bad level spec '{0}' for parameter '{1}' in model '{2}'", lv, rp.key.as_str(), model.name));
                            }
                            Some(spec) => { rp.level_specs.push(spec); }
                        }
                    }
                    rm.params.push(rp);
                }
            }
        }
        if rm.params.is_empty() {
            return Err(format!("model '{}' configures no parameters", model.name));
        }
        out.push(rm);
    }
    return Ok(out);
}

// ---------------------------------------------------------------------------
// data_dir / ingest_dir overlap guard helpers (V1.2 phase C)
// ---------------------------------------------------------------------------

/// Normalize a configured DIRECTORY path for comparison. When the path EXISTS
/// it is canonicalized (which ALSO resolves symlinks); when it does not exist
/// yet — routinely true for `data_dir`, which the pipeline creates on first
/// boot — it is absolutized against the process CWD and `.`/`..` components are
/// resolved LEXICALLY. Consequence documented deliberately: a symlink pointing
/// at an EXISTING directory is resolved, while a symlink whose target does not
/// exist yet is compared by its lexical form; the guard exists to catch
/// configuration mistakes, not hostile filesystems.
pub fn norm_dir_path(p: &str) -> String {
    let c = canonicalize(p);
    match c {
        Ok(v) => { return v.to_string_lossy().to_string(); }
        Err(_) => {}
    }
    let mut base = p.to_string();
    if base.len() == 0 || base[0 .. 1] != *"/" {
        let cwd = current_dir();
        match cwd {
            Err(_) => { return collapse_lexical(&base); }
            Ok(d) => { base = format!("{0}/{1}", d.to_string_lossy(), base); }
        }
    }
    return collapse_lexical(&base);
}

/// Split a path into its components (`""` = repeated separators, `.` dropped,
/// `..` pops the previous component). Leading `/` yields an empty first marker
/// so absolute and relative forms never compare equal by accident.
pub fn path_parts(p: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::<String>::new();
    if p.len() > 0 && p[0 .. 1] == *"/" {
        out.push("".to_string());
    }
    for seg in p.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if out.len() > 1 {
                let _x = out.pop();
            }
            continue;
        }
        out.push(seg.to_string());
    }
    return out;
}

/// Lexically absolutize+collapse a path WITHOUT touching the filesystem.
fn collapse_lexical(p: &str) -> String {
    let parts = path_parts(p);
    let mut abs = false;
    if parts.len() > 0 && parts[0].is_empty() {
        abs = true;
    }
    let mut joined = "".to_string();
    for k in 0 .. parts.len() {
        if parts[k].is_empty() {
            continue;
        }
        if joined.is_empty() {
            joined = parts[k].to_string();
        } else {
            joined = format!("{0}/{1}", joined, parts[k]);
        }
    }
    if !abs {
        if joined.is_empty() {
            return ".".to_string();
        }
        return joined;
    }
    return format!("/{0}", joined);
}

/// Relationship between two already-normalized directory paths:
///   0 = DISJOINT, 1 = IDENTICAL, 2 = A lies UNDER B, 3 = B lies UNDER A.
/// Component-wise prefix test (never string-prefix, so `/data-x` is NOT "under"
/// `/data`).
pub fn dir_relation(a: &Vec<String>, b: &Vec<String>) -> i32 {
    if a.len() == b.len() {
        let mut same = true;
        for k in 0 .. a.len() {
            if a[k] != b[k] {
                same = false;
                break;
            }
        }
        if same {
            return 1;
        }
        return 0;
    }
    let mut short = a;
    let mut long = b;
    if a.len() > b.len() {
        short = b;
        long = a;
    }
    for k in 0 .. short.len() {
        if short[k] != long[k] {
            return 0;
        }
    }
    // `short` is a proper prefix of `long`: which one is the ancestor?
    if a.len() < b.len() {
        return 3; // B contains A
    }
    return 2; // A contains B
}

fn relation_text(rel: i32) -> String {
    if rel == 1 {
        return "same directory".to_string();
    }
    if rel == 2 {
        return "data_dir contains the ingest_dir".to_string();
    }
    if rel == 3 {
        return "ingest_dir contains data_dir".to_string();
    }
    return "disjoint".to_string();
}

/// Validate a raw TOML text (used by tests + load_config).
pub fn validate_text(text: &str) -> Result<Vec<ResolvedModel>, String> {
    let cfg = from_str::<PointConfig>(&text);
    match cfg {
        Err(e) => { return Err(e.to_string()); }
        Ok(c) => { return validate_config(&c); }
    }
}

/// Config-time half of the spec §4.4 wind rule: which single wind component
/// (if any) a model's resolved parameters list. Recognizes BOTH the canonical
/// table names and raw GRIB2 triples (category 2 / number 2 = UGRD, number 3
/// = VGRD), because a config may name parameters by code. Returns Some((present,
/// missing)) when EXACTLY ONE of the pair is configured, None otherwise.
pub fn wind_pair_gap(params: &[ResolvedParam]) -> Option<(String, String)> {
    let mut has_u = false;
    let mut has_v = false;
    for p in params {
        let is_u = p.key == "UGRD" || (p.cat == 2 && p.num == 2);
        let is_v = p.key == "VGRD" || (p.cat == 2 && p.num == 3);
        if is_u { has_u = true; }
        if is_v { has_v = true; }
    }
    if has_u && !has_v {
        return Some(("UGRD".to_string(), "VGRD".to_string()));
    }
    if has_v && !has_u {
        return Some(("VGRD".to_string(), "UGRD".to_string()));
    }
    return None;
}

// ---------------------------------------------------------------------------
// startup wind-pair preflight (spec §4.4: "If only one component is configured
// for a grid-relative model, fail config validation with a clear message.")
// ---------------------------------------------------------------------------

/// Fail-fast check performed at STARTUP, before any ingest, against the FIRST
/// STABLE file matching the model's glob (read-only; sections 1/3 only):
///
/// * both components configured, or neither -> nothing to check;
/// * exactly one component AND the probed grid declares grid-relative vector
///   components (flag table 3.3 bit 0x08) -> HARD ERROR naming model, file,
///   template, flag byte and the missing parameter;
/// * exactly one component but the grid is earth-relative -> accepted (note);
/// * no stable candidate file / unreadable file / unsupported grid template ->
///   DEFERRED with a note: the decode-time rule (`check_wind_pair` in the
///   pipeline, second line of defense) still fires as soon as data lands.
///
/// Stability here means "size > 0 and mtime at least `stable_window_secs` old":
/// unlike a scan round, a single startup probe cannot observe size/mtime over
/// time, so age is the practical equivalent (documented decision). Notes are
/// returned for the caller to log; Err carries the fatal message.
pub fn startup_wind_preflight(models: Vec<ResolvedModel>, stable_window_secs: u32,
                              now_ns: i64) -> Result<Vec<String>, String> {
    use crate::discovery::{glob_match, walk_files, base_name};
    use crate::state::stat_size_mtime;
    use crate::grib2::{probe_first_grid};

    let mut notes: Vec<String> = Vec::<String>::new();
    let window_ns = (stable_window_secs as i64) * 1000000000;
    for mdl in &models {
        let gap = wind_pair_gap(&mdl.params);
        match gap {
            None => { continue; }
            Some(pair) => {
                // first stable file matching the model's glob (walk_files sorts)
                let cands = walk_files(&mdl.ingest_dir);
                let mut chosen = "".to_string();
                let mut unstable = 0;
                for c in &cands {
                    if !glob_match(&mdl.file_glob, base_name(c.to_string()).as_str()) {
                        continue;
                    }
                    let st = stat_size_mtime(&c);
                    match st {
                        Err(_) => { continue; }
                        Ok(sz_mt) => {
                            if sz_mt.0 == 0 {
                                continue;
                            }
                            if sz_mt.1 > now_ns - window_ns {
                                unstable += 1;
                                continue;
                            }
                        }
                    }
                    chosen = c.to_string();
                    break;
                }
                if chosen.is_empty() {
                    let why = if unstable > 0 {
                        format!("{0} matching file(s) not stable yet", unstable).to_string()
                    } else {
                        "no matching file present".to_string()
                    };
                    notes.push(format!("wind-pair preflight: model '{0}' configures only {1} of the UGRD/VGRD pair; deferring ({2}) under '{3}'",
                               mdl.name, pair.0, why, mdl.ingest_dir));
                    continue;
                }
                let pr = probe_first_grid(chosen.to_string());
                match pr {
                    Err(e) => {
                        notes.push(format!("wind-pair preflight: model '{0}' could not probe '{1}' ({2:?}); deferring to the decode-time rule",
                                   mdl.name, chosen, e));
                        continue;
                    }
                    Ok(pp) => {
                        // Decisive answer: what the FIRST message that IS a
                        // wind component declared for ITS grid. A real file
                        // mixes scalar grids (flag bit clear) and vector grids
                        // (bit set) -- exactly what HRRR files do -- so only
                        // the wind messages settle the question.
                        if pp.wind_msg == 2 {
                            return Err(format!("config error (spec 4.4): model '{0}' configures ONLY {1} but needs {2} as well: in the first stable file '{3}' the WIND messages carry GRID-RELATIVE vector components (section 3 template 3.{4}, flag table 3.3 byte 0x{5:02X} with bit 0x08 set; {6} of {7} probed messages declare that flag). Rotating a single component to earth-relative is meaningless -- add {2} to [[models.params]] or drop {1}.",
                                       mdl.name, pair.0, pair.1, chosen, pp.tmpl, pp.wind_flag33,
                                       pp.winds_true, pp.messages));
                        }
                        if pp.wind_msg == 1 {
                            notes.push(format!("wind-pair preflight: model '{0}' configures only {1}; in '{2}' the wind messages declare EARTH-relative components (flag table 3.3 byte 0x{3:02X}): accepted",
                                       mdl.name, pair.0, chosen, pp.wind_flag33));
                            continue;
                        }
                        // no wind message in the probed file: cannot decide
                        notes.push(format!("wind-pair preflight: model '{0}' configures only {1}; '{2}' contains no wind message ({3} messages probed), cannot decide -- deferring to the decode-time rule",
                                   mdl.name, pair.0, chosen, pp.messages));
                        continue;
                    }
                }
            }
        }
    }
    return Ok(notes);
}

pub fn load_config(path: &str) -> Result<PointConfig, String> {
    let text = read_to_string(&path);
    match text {
        Err(e) => { return Err(e.to_string()); }
        Ok(t) => {
            let cfg = from_str::<PointConfig>(&t);
            match cfg {
                Err(e) => { return Err(e.to_string()); }
                Ok(c) => {
                    let v = validate_config(&c);
                    match v {
                        Err(msg) => { return Err(msg); }
                        Ok(_) => { return Ok(c); }
                    }
                }
            }
        }
    }
}
