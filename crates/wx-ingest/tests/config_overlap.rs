// V1.2 phase C gate #8: data_dir / ingest_dir OVERLAP VALIDATION (spec §3 "fail
// loudly at startup" + §4.6 retention safety).
//
// Ingest WRITES into `<data_dir>/<model>/...` and READS from `<ingest_dir>`. If
// one directory CONTAINS the other (or they are the SAME directory spelled two
// ways), the two sides fight: retention deletes run trees that may BE the source
// products, and atomic renames land INSIDE the watched ingest tree where the glob
// matches them again (self-ingestion). Rejected at startup naming BOTH paths.
// Comparison is COMPONENT-WISE after normalization, so `/data-x` is NOT "under"
// `/data`, and relative vs absolute spellings of the SAME directory DO collide.

use wx_ingest::config::{PointConfig, ServerCfg, IngestCfg, ModelCfg, ParamCfg, validate_config};
use wx_ingest::state::{join_path};
use std::env::current_dir;

#[allow(clippy::needless_return)]
fn one_model_cfg(data: String, ing: String) -> PointConfig {
    let mut lv = Vec::<String>::new();
    lv.push("isobaric:*".to_string());
    let mut ps = Vec::<ParamCfg>::new();
    ps.push(ParamCfg {
        name: Some("TMP".to_string()),
        discipline: None,
        category: None,
        number: None,
        levels: lv,
    });
    let mut ms = Vec::<ModelCfg>::new();
    ms.push(ModelCfg {
        name: "gfs".to_string(),
        ingest_dir: ing.to_string(),
        file_glob: "gfs_*.grib2".to_string(),
        sidecar_suffix: None,
        params: ps,
    });
    return PointConfig {
        server: ServerCfg { bind: "127.0.0.1:8080".to_string(), data_dir: data.to_string() },
        ingest: IngestCfg {
            scan_interval_secs: 30,
            retention_runs: 4,
            stable_window_secs: Some(0),
            ingest_threads: Some(2),
            decode_batch_files: Some(1),
        },
        models: ms,
    };
}

#[test]
fn identical_data_dir_and_ingest_dir_is_rejected_naming_both_paths() {
    let ing = "/tmp/opencode/wxpoint-cfg-over/same".to_string();
    let cfg = one_model_cfg(ing.to_string(), ing.to_string());
    match validate_config(&cfg) {
        Err(e) => {
            assert!(e.contains("OVERLAP"));
            assert!(e.contains("same"));
            assert!(e.contains("/tmp/opencode/wxpoint-cfg-over/same"));
            assert!(e.contains("gfs"));
        }
        Ok(_) => { assert!(false); return; }
    }
}

#[test]
fn data_dir_nested_inside_an_ingest_dir_is_rejected() {
    // the classic mistake: pointing the STORE inside the delivery directory
    let ing = "/tmp/opencode/wxpoint-cfg-over/in".to_string();
    let data = join_path(&ing, "store").to_string();
    let cfg = one_model_cfg(data.to_string(), ing.to_string());
    match validate_config(&cfg) {
        Err(e) => {
            assert!(e.contains("OVERLAP"));
            assert!(e.contains("contains"));
            assert!(e.contains(&data));
            assert!(e.contains(&ing));
        }
        Ok(_) => { assert!(false); return; }
    }
}

#[test]
fn ingest_dir_nested_inside_the_data_dir_is_rejected() {
    let data = "/tmp/opencode/wxpoint-cfg-over/store".to_string();
    let ing = join_path(&data, "incoming").to_string();
    let cfg = one_model_cfg(data.to_string(), ing.to_string());
    match validate_config(&cfg) {
        Err(e) => {
            assert!(e.contains("OVERLAP"));
            assert!(e.contains("contains"));
        }
        Ok(_) => { assert!(false); return; }
    }
}

#[test]
fn relative_and_absolute_spellings_of_one_directory_still_collide() {
    // Neither path exists yet, so normalization is LEXICAL against the process
    // CWD: `./x/data` and `<cwd>/x/data` are the SAME directory and must be caught.
    let cwd_r = current_dir();
    match cwd_r {
        Err(_) => { assert!(false); return; }
        Ok(cwd) => {
            let abs = format!("{0}/wxpoint-cfg-rel/data", cwd.to_string_lossy());
            let cfg = one_model_cfg("./wxpoint-cfg-rel/data".to_string(), abs.to_string());
            match validate_config(&cfg) {
                Err(e) => { assert!(e.contains("OVERLAP")); }
                Ok(_) => { assert!(false); return; }
            }
            // and a `..` component that walks back into the other tree collides too
            let up = format!("{0}/wxpoint-cfg-rel/x/../data", cwd.to_string_lossy());
            let cfg2 = one_model_cfg("./wxpoint-cfg-rel/data".to_string(), up.to_string());
            match validate_config(&cfg2) {
                Err(e) => { assert!(e.contains("OVERLAP")); }
                Ok(_) => { assert!(false); return; }
            }
        }
    }
}

#[test]
fn sibling_directories_that_merely_share_a_name_prefix_are_accepted() {
    // The string-prefix trap: `/…/data` vs `/…/data-extra` are DISJOINT trees.
    let data = "/tmp/opencode/wxpoint-cfg-ok/data".to_string();
    let ing = "/tmp/opencode/wxpoint-cfg-ok/data-extra/incoming".to_string();
    let cfg = one_model_cfg(data.to_string(), ing.to_string());
    match validate_config(&cfg) {
        Err(e) => {
            assert!(!e.contains("OVERLAP"));
            assert!(false);
            return;
        }
        Ok(ms) => {
            assert!(ms.len() == 1);
            assert!(ms[0].params.len() == 1);
        }
    }
}
