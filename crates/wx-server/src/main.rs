//! wx-point server binary (milestone 4). CLI: `wxpoint --config PATH` or the
//! WXPOINT_CONFIG env var. Startup is graceful: config validation happens
//! first (fail loudly, spec §3), then the ingest side starts on its OWN
//! plain thread (notify watchers + periodic scan reconciliation + the
//! dedicated rayon decode pool per config, spec §4.2 "separate from the
//! HTTP runtime"), while the tokio HTTP runtime serves immediately; /ready
//! reflects indexing progress (spec §7).

use std::env::args;
use std::env::var;
use std::thread::spawn;
use libc::usleep;
use tokio::runtime::Runtime;
use tokio::net::TcpListener;
use axum::serve;
use wx_ingest::config::{load_config, startup_wind_preflight};
use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::now_ns_marker;
use wx_server::httpapi::{ServerState, build_router};
use wx_server::stats::{new_stats_with, mono_ns};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Resolve the config path: --config flag wins, then WXPOINT_CONFIG.
#[allow(clippy::needless_return)]
fn resolve_cfg_path() -> String {
    let mut path = "".to_string();
    let mut want_next = false;
    let mut first = true;
    let ai = args();
    for a in ai {
        if first {
            first = false;
            continue; // program name
        }
        if want_next {
            path = a.to_string();
            want_next = false;
        } else if a == *"--config" {
            want_next = true;
        }
    }
    if path.is_empty() {
        let v = var("WXPOINT_CONFIG");
        match v {
            Err(_) => {}
            Ok(s) => { path = s.to_string(); }
        }
    }
    return path;
}

#[allow(clippy::needless_return)]
pub fn main() {
    let cfg_path = resolve_cfg_path();
    if cfg_path.is_empty() {
        println!("usage: wxpoint --config <path> (or set WXPOINT_CONFIG)");
        return;
    }
    // validate loudly before anything else (spec §3)
    let cfg_r = load_config(&cfg_path);
    match cfg_r {
        Err(e) => {
            println!("config error: {0}", e);
            return;
        }
        Ok(cfg) => {
            let bind_str = cfg.server.bind;
            let interval_secs = cfg.ingest.scan_interval_secs;
            start_pipeline(&cfg_path, &bind_str, interval_secs);
        }
    }
}

#[allow(clippy::needless_borrow)]
fn start_pipeline(cfg_path: &str, bind_str: &str, interval_secs: u32) {
    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(e) => {
            println!("pipeline init failed: {0}", e);
            return;
        }
        Ok(pp) => { pp }
    };
    println!("loaded models: {0}; bind {1}; data_dir {2}", model_names(&p), bind_str, p.data_dir);

    // spec §4.4 wind-pair rule, checked at STARTUP against the first stable
    // file of each model (read-only probe of sections 1/3 only). A grid-relative
    // model that configures only one of UGRD/VGRD fails here, before any data
    // is ingested or any port is bound; the decode-time check in the pipeline
    // stays as the second line of defense for empty-at-boot directories.
    let win_secs = (p.stable_window_ns / 1000000000) as u32;
    let now_ns = now_ns_marker(&p.state.dir_path);
    let pf = startup_wind_preflight(p.models.clone(), win_secs, now_ns);
    match pf {
        Err(msg) => {
            println!("{0}", msg);
            return;
        }
        Ok(notes) => {
            for n in &notes {
                println!("{0}", n);
            }
        }
    }

    // Stats share the Pipeline's LIVE counter mirror from birth (V1.2 phase B),
    // so /metrics reflects ingest progress DURING a scan round, not only after it.
    let stats = new_stats_with(p.live.clone());
    let cfg_models = Arc::new(p.models.clone());
    let st = ServerState {
        index: p.shared.clone(), // same ArcSwap the ingest thread publishes through
        stats: stats.clone(),
        models_cfg: cfg_models,
    };
    let app = build_router(st);

    // --- LIVE progress printer (V1.2 phase B): prints the pipeline's newest
    // per-file line ("ingested <file>: G groups, W bytes, X ms", or an
    // ignore/failure note) whenever `progress_seq` advances. It runs on its OWN
    // thread because the ingest thread is blocked inside `scan_round` while
    // those lines are produced, which is exactly when operators want them.
    let live_p = p.live.clone();
    let _printer = spawn(move || {
        let mut seen: u64 = 0;
        loop {
            let seq = live_p.progress_seq.load(Ordering::Relaxed);
            if seq != seen {
                seen = seq;
                let m = live_p.last_msg.lock();
                match m {
                    Err(_) => {}
                    Ok(g) => { println!("{0}", g.clone()); }
                }
            }
            unsafe { usleep(100000); } // 0.1 s polling granularity
        }
    });

    // --- ingest side: dedicated plain thread, OUTSIDE the tokio runtime
    let interval_ns = (interval_secs as i64) * 1000000000;
    let th = spawn(move || {
        p.start_watchers();
        let mut printed_logs = 0;
        loop {
            let n = p.scan_round();
            if n > 0 {
                println!("scan_round ingested {0} file(s)", n);
            }
            // Back-compat absolute store (also monotone-merged into the live
            // mirror; see QueryStats.set_ingest). The per-FILE live mirror is
            // what makes /metrics move mid-round.
            stats.set_ingest(p.counters.errors, p.counters.ingested_files,
                        p.counters.skipped_messages, p.counters.last_mtime_ns);
            while printed_logs < p.log.len() {
                println!("{0}", p.log[printed_logs]);
                printed_logs += 1;
            }
            let target = mono_ns() + interval_ns;
            while mono_ns() < target {
                unsafe { usleep(200000); } // 0.2 s granularity
            }
        }
    });
    let _th_unused = th;

    // --- HTTP side: tokio multi-thread runtime (spec §7 axum/tokio)
    let rt_r = Runtime::new();
    let rt = match rt_r {
        Err(_e) => {
            println!("tokio runtime init failed");
            return;
        }
        Ok(rtx) => { rtx }
    };
    rt.block_on(async {
        let lr = TcpListener::bind(&bind_str);
        match lr.await {
            Err(e) => {
                println!("failed to bind {0}: {1}", bind_str, e);
            }
            Ok(listener) => {
                println!("listening on {0}", bind_str);
                let srv = serve(listener, app);
                let res = srv.await;
                match res {
                    Err(e) => { println!("server error: {0}", e); }
                    Ok(_) => { println!("server stopped"); }
                }
            }
        }
    });
}

#[allow(clippy::needless_return)]
fn model_names(p: &Pipeline) -> String {
    let mut joined = "".to_string();
    for m in &p.models {
        if !joined.is_empty() {
            joined = format!("{0}{1}", joined, ",");
        }
        joined = format!("{0}{1}", joined, m.name);
    }
    return joined;
}
