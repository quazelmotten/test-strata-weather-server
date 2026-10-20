//! Configuration schema + startup validation (spec §3).
//!
//! TOML keys beyond the spec example (all optional, defaults documented):
//! * `[ingest].stable_window_secs` (default 10): size+mtime must be unchanged
//!   for this long before a file is eligible under the window strategy.
//! * `[ingest].ingest_threads` (default 4): rayon pool size for decode work.
//! * `[[models]].sidecar_suffix` (absent by default): when present the model
//!   uses the sidecar-marker stability strategy instead of the window — a
//!   candidate `<file>` is eligible only once `<file><suffix>` exists.
//! * Raw parameter override: a `[[models.params]]` entry may omit `name` and
//!   give `discipline`, `category`, `number` integers directly (spec §3:
//!   "Allow the config to specify raw discipline/category/number"). Units are
//!   then reported as "" (unknown) unless the triple happens to match a table
//!   entry, in which case the canonical name/units are used.
//! * Level specs: `"type:value"`, bare `"type"` (value 0, e.g. `surface`),
//!   `"type:*"` (every value of that type present) or `"*"` (any level).

use std::fs::read_to_string;
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
}

#[derive(serde::Deserialize)]
pub struct ParamCfg {
    pub name: Option<String>,
    pub discipline: Option<i64>,
    pub category: Option<i64>,
    pub number: Option<i64>,
    pub levels: Vec<String>,
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
/// canonical-or-raw key, units). Errors carry the human message (spec §3).
fn resolve_entry(p: &ParamCfg, mname: &str) -> Result<(u8, u8, u8, String, String), String> {
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
                let un = param_units(&nm).unwrap().to_string();
                return Ok((trip.0, trip.1, trip.2, nm.to_string(), un));
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
            return Ok((du, cu, nu, format!("d{0}c{1}n{2}", du, cu, nu), "".to_string()));
        }
        Some(kn) => {
            return Ok((du, cu, nu, kn.to_string(), param_units(&kn).unwrap().to_string()));
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

/// Validate a raw TOML text (used by tests + load_config).
pub fn validate_text(text: &str) -> Result<Vec<ResolvedModel>, String> {
    let cfg = from_str::<PointConfig>(&text);
    match cfg {
        Err(e) => { return Err(e.to_string()); }
        Ok(c) => { return validate_config(&c); }
    }
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
