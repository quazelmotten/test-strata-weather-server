//! wx-point server binary. Milestone 4 wires the axum app and /v1/point
//! endpoints; this scaffold validates config at startup (spec §3: fail loudly)
//! and reports readiness so the binary is runnable end-to-end.

use wx_ingest::load_config;

pub fn main() {
    // CLI/env wiring for the config path is milestone-4 work; scaffold uses the
    // example config directly so startup validation + routing are exercised.
    match load_config("config.example.toml") {
        Ok(cfg) => {
            let mut names: Vec<&str> = [].to_vec();
            for m in &cfg.models {
                names.push(&m.name);
            }
            println!("loaded models: {}", names.join(", "));
        },
        Err(e) => panic!("config error: {}", e),
    }
}
