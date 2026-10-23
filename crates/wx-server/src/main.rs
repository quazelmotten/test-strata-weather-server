//! wx-point server binary (milestone 4). CLI: `wxpoint --config PATH` or the
//! WXPOINT_CONFIG env var. Startup is graceful: config validation happens
//! first (fail loudly, spec §3), then the ingest side starts on its OWN
//! plain thread (notify watchers + periodic scan reconciliation + the
//! dedicated rayon decode pool per config, spec §4.2 "separate from the
//! HTTP runtime"), while the tokio HTTP runtime serves immediately; /ready
//! reflects indexing progress (spec §7).
//!
//! ## Exit codes (V1.2 phase C — startup failures are MACHINE-OBSERVABLE)
//!
//! Before this change every startup failure PRINTED its message and still exited
//! WITH STATUS 0, so a supervisor (systemd, a k8s entrypoint, a CI smoke script)
//! could not tell a healthy boot from a dead one. Now:
//!
//! | code | meaning |
//! |---|---|
//! | 0  | served normally (or stopped cleanly) |
//! | 1  | config missing/unparsable/invalid (spec §3 loud failure) |
//! | 2  | usage error: no `--config` and no `WXPOINT_CONFIG` |
//! | 3  | pipeline init failed (state/grid/data-dir problems) |
//! | 4  | startup wind-pair preflight rejected the configuration (spec §4.4) |
//! | 5  | tokio runtime init failed |
//! | 6  | the configured `bind` address could not be bound |
//! | 7  | PROVISIONAL wind-pair enforcement fired (V1.3 phase E3b): a model
//! |    | configuring only ONE of UGRD/VGRD decoded a GRID-relative wind field
//! |    | after boot (its dir was empty/unstable at startup, so preflight code
//! |    | 4 could not decide); the "wind pair rule violated" note precedes this
//! |    | exit; set `[ingest].wind_pair_enforce = false` to opt out |
//! | 8  | the HTTP server itself returned an error |
//!
//! Distinct codes are deliberate (the task allowed "at least 1"): an operator can
//! fix a bad config without reading logs, and `scripts/smoke.sh` style harnesses
//! can gate on it. `crates/wx-server/tests/exit_codes.rs` SPAWNS the built binary
//! with deliberately broken startup conditions and asserts the EXACT code for
//! every class forceable black-box: 2 (usage), 1 (bad config ×3), 4 (boot-time
//! stable half-pair grid-relative wind config), 6 (address cannot be bound), 7
//! (provisional post-boot enforcement, both halves incl. the rc-124 opt-out
//! control). Codes 3, 5 and 8 stay WIRED BUT UNTESTED — forcing them black-box
//! needs privileged-environment surgery that would make the suite machine-
//! dependent; the honest ledger lives in README §9.

use std::env::args;
use std::env::var;
use std::process::exit;
use std::thread::spawn;
use libc::usleep;
use libc::clock_gettime;
use libc::timespec;
use libc::CLOCK_REALTIME;
use tokio::runtime::Runtime;
use tokio::net::TcpListener;
use axum::serve;
use wx_ingest::config::{load_config, validate_config, startup_wind_preflight, DEFAULT_STABLE_WINDOW_SECS};
use wx_ingest::pipeline::Pipeline;
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
        // USAGE ERROR (exit 2): nothing was started, nothing was served.
        exit(2);
    }
    // validate loudly before anything else (spec §3)
    let cfg_r = load_config(&cfg_path);
    match cfg_r {
        Err(e) => {
            println!("config error: {0}", e);
            exit(1);
        }
        Ok(cfg) => {
            // V1.3 phase E3b: the spec §4.4 wind-pair PREFLIGHT runs BEFORE
            // Pipeline::open, because open performs DESTRUCTIVE storage-format
            // recovery (deleting upgrade-trap run dirs + forgetting records) —
            // a fatal wind configuration must fail before any data is touched.
            // The stability probe compares file mtimes (wall-clock epoch ns)
            // against libc CLOCK_REALTIME directly, replacing the old
            // marker-file proxy; durations everywhere else stay on MonoClock.
            // A wrong "unstable" verdict can only DEFER now: enforcement is
            // provisional and re-decides with DECODED evidence after every scan
            // round (exit 7 below), so clock weirdness never causes a false
            // hard-fail NOR a permanent miss.
            let rms = validate_config(&cfg);
            match rms {
                Err(_e) => {} // load_config already validated; belt-and-braces
                Ok(models) => {
                    let win_secs = match cfg.ingest.stable_window_secs {
                        None => { DEFAULT_STABLE_WINDOW_SECS }
                        Some(w) => { w }
                    };
                    let now_ns = realtime_epoch_ns();
                    let pf = startup_wind_preflight(models, win_secs, now_ns);
                    match pf {
                        Err(msg) => {
                            println!("{0}", msg);
                            exit(4);
                        }
                        Ok(notes) => {
                            for n in &notes {
                                println!("{0}", n);
                            }
                        }
                    }
                }
            }
            let bind_str = cfg.server.bind;
            let interval_secs = cfg.ingest.scan_interval_secs;
            let rc = start_pipeline(&cfg_path, &bind_str, interval_secs);
            exit(rc as i32);
        }
    }
}

/// Wall-clock epoch nanoseconds via libc CLOCK_REALTIME (same domain as file
/// mtimes). Returns 0 on failure, which makes every candidate look "just
/// written" — the SAFE direction: the preflight defers, and provisional
/// post-round enforcement (exit 7) re-decides with decoded bytes later.
#[allow(clippy::needless_return)]
fn realtime_epoch_ns() -> i64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    let r = unsafe { clock_gettime(CLOCK_REALTIME, &mut ts) };
    if r != 0 {
        return 0;
    }
    return ts.tv_sec * 1000000000 + ts.tv_nsec;
}

#[allow(clippy::needless_borrow)]
fn start_pipeline(cfg_path: &str, bind_str: &str, interval_secs: u32) -> i64 {
    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(e) => {
            println!("pipeline init failed: {0}", e);
            return 3;
        }
        Ok(pp) => { pp }
    };
    println!("loaded models: {0}; bind {1}; data_dir {2}", model_names(&p), bind_str, p.data_dir);

    // V1.3 phase E3b: the spec §4.4 wind-pair preflight now runs in main(),
    // BEFORE this function, because Pipeline::open below performs DESTRUCTIVE
    // storage-format recovery and a fatal wind config must not delete run dirs
    // on its way out. The decode-time check inside the pipeline stays as the
    // second line of defense for directories that were empty/unstable at boot,
    // and since E3b that second line is ENFORCED: see the exit(7) check in the
    // ingest thread below.

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

    // --- LIVE progress printer (V1.2 phase B, de-duplicated in phase D): drains
    // the pipeline's per-FILE progress FIFO whenever `progress_seq` advances,
    // printing EVERY line ONCE (oldest first). The previous version printed only
    // `last_msg`, which lost lines when files finished faster than the 0.1 s poll
    // cadence AND let the round streamer below replay them a second time. It runs
    // on its OWN thread because the ingest thread is blocked inside `scan_round`
    // while those lines are produced, which is exactly when operators want them.
    let live_p = p.live.clone();
    let _printer = spawn(move || {
        let mut seen: u64 = 0;
        loop {
            let seq = live_p.progress_seq.load(Ordering::Relaxed);
            if seq != seen {
                seen = seq;
                let lines = live_p.drain_progress();
                for l in &lines {
                    println!("{0}", l);
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
            // V1.3 phase E3b PROVISIONAL WIND-PAIR ENFORCEMENT (exit 7): the
            // boot preflight can only probe stable files that EXIST; a model
            // whose ingest dir was empty at boot with only ONE of UGRD/VGRD
            // configured used to start fine and then skip every wind message +
            // inflate errors FOREVER (audit defect 5). Now the first scan
            // round whose DECODED evidence proves the violation (grid-relative
            // wind grid met by a half-configured model — sticky flag set inside
            // ingest_file) ends the process NON-ZERO after the note above has
            // reached stdout, so a supervisor restarts with a fixed config.
            // `[ingest].wind_pair_enforce = false` opts back into the old
            // sticky-note-and-skip behavior (documented deviation).
            if p.wind_violation && p.wind_pair_enforce {
                println!("FATAL (spec 4.4 provisional enforcement, exit 7): a model configuring only ONE of UGRD/VGRD decoded GRID-relative wind components (see the 'wind pair rule violated' note above); fix [[models.params]] or set [ingest].wind_pair_enforce = false");
                exit(7);
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
            return 5;
        }
        Ok(rtx) => { rtx }
    };
    let mut srv_rc: i64 = 0;
    rt.block_on(async {
        let lr = TcpListener::bind(&bind_str);
        match lr.await {
            Err(e) => {
                println!("failed to bind {0}: {1}", bind_str, e);
                srv_rc = 6;
            }
            Ok(listener) => {
                println!("listening on {0}", bind_str);
                let srv = serve(listener, app);
                let res = srv.await;
                match res {
                    Err(e) => { println!("server error: {0}", e); srv_rc = 8; }
                    Ok(_) => { println!("server stopped"); }
                }
            }
        }
    });
    srv_rc
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
