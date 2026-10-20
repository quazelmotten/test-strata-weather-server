//! File discovery, GRIB2 decode, wind rotation, storage writing.
//! Milestones 2-3 fill in the pipeline; config validation is live now so
//! startup fails loudly on unknown names (spec §3).

use std::fs::read_to_string;
use toml::from_str;
use wx_core::params::lookup_level_type;
use wx_core::params::lookup_param;

#[derive(serde::Deserialize)]
pub struct ServerCfg {
    pub bind: String,
    pub data_dir: String,
}

#[derive(serde::Deserialize)]
pub struct IngestCfg {
    pub scan_interval_secs: u32,
    pub retention_runs: u32,
}

#[derive(serde::Deserialize)]
pub struct ParamCfg {
    pub name: String,
    pub levels: Vec<String>,
}

#[derive(serde::Deserialize)]
pub struct ModelCfg {
    pub name: String,
    pub ingest_dir: String,
    pub file_glob: String,
    pub params: Vec<ParamCfg>,
}

#[derive(serde::Deserialize)]
pub struct PointConfig {
    pub server: ServerCfg,
    pub ingest: IngestCfg,
    pub models: Vec<ModelCfg>,
}

pub fn validate_config(cfg: &PointConfig) -> Result<(), String> {
    if cfg.models.is_empty() {
        return Err("config has no [[models]] entries".to_string());
    }
    for model in &cfg.models {
        if model.name.is_empty() {
            return Err("a model name is empty".to_string());
        }
        for p in &model.params {
            if lookup_param(&p.name).is_none() {
                return Err(format!(
                    "unknown parameter '{}' in model '{}': not in the built-in table",
                    p.name,
                    model.name,
                ));
            }
            for lv in &p.levels {
                if lv == "*" {
                    continue;
                }
                let parts: Vec<&str> = lv.split(':').collect();
                if parts.is_empty() {
                    return Err(format!("empty level spec in model '{}'", model.name));
                }
                if lookup_level_type(parts[0]).is_none() {
                    return Err(format!(
                        "unknown level type '{}' in model '{}'",
                        parts[0],
                        model.name,
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn load_config(path: &str) -> Result<PointConfig, String> {
    let text = read_to_string(path).map_err(|e| e.to_string())?;
    let cfg = from_str::<PointConfig>(&text).map_err(|e| e.to_string())?;
    validate_config(&cfg)?;
    Ok(cfg)
}
