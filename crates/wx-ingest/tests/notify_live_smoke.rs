// V1.4 phase N: LIVE-NOTIFY DISCOVERY SMOKE (spec 4.1, the REAL subscription).
//
// Every other discovery test injects queue events by hand (see the "simulate
// what a notify watcher would deliver" comment in queue_drain_fairness.rs);
// the actual inotify subscription started by Pipeline::start_watchers and the
// DebouncedEvent -> candidate-queue mapping in discovery::spawn_feeder were
// NEVER exercised end to end. If watcher creation or event mapping silently
// failed, discovery latency would degrade to the scan interval and NO test
// would notice, because every scan_round also walks the tree. This test pins
// the live machinery itself:
//
//   * watchers are started through the production entry point and their
//     SUCCESS is asserted (discovery::start_watchers swallows a watcher()
//     failure by returning FEWER handles -- zero handles means the
//     subscription never started, which is exactly the silent degradation).
//   * between the file drop and the FIRST scan_round call, NOTHING else runs
//     in this process that could enqueue a path: the periodic full-scan
//     fallback lives ONLY in the wx-server driver loop (main.rs sleeps
//     scan_interval_secs between scan_round calls; pipeline::scan_round itself
//     never reads scan_interval_secs). So any path found in p.queue during
//     that window was produced SOLELY by the real filesystem-event
//     subscription -- the scan fallback provably cannot have been responsible.
//   * the config pins scan_interval_secs = 3600, so even a hypothetically
//     running fallback could not have fired inside the bounded 15 s window
//     many times over; the measured drop-to-queryable latency lands far below
//     both the deadline and the configured fallback cadence.
//
// Stability semantics pinned honestly (repo docs + discovery::observe): under
// the WINDOW strategy, stable_window_secs = 0 still requires ONE RE-OBSERVATION
// -- the first observation opens the window and returns false unconditionally,
// no matter how small the window is. So round A observes, round B ingests. The
// SIDECAR strategy (second test) needs no re-observation: sidecar_ready flips
// eligible the moment the marker exists, so exactly ONE round after the marker
// lands the product is queryable ("answers immediately once marker lands").

use wx_ingest::pipeline::{Pipeline};
use wx_ingest::state::join_path;
use wx_ingest::discovery::{path_to_str, WatchQueue};
use wx_ingest::index::{IndexSnapshot};
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::fs::metadata;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;

/// Generous wall-clock deadlines. notify debounces at most 200 ms per watch
/// (discovery::start_watchers), so honest behavior lands sub-second; 15 s only
/// catches genuinely dead machinery.
const EVENT_DEADLINE: Duration = Duration::from_millis(15_000);
const POLL: Duration = Duration::from_millis(25);

#[allow(clippy::needless_return)]
fn fixture() -> Vec<u8> {
    // Relative to crates/wx-ingest (the cwd integration tests run from).
    let p = "../../tests/fixtures/gfs_small.grib2";
    let rd = std::fs::read(p);
    match rd {
        Err(e) => { panic!("fixture {} unreadable: {}", p, e); }
        Ok(v) => { return v; }
    }
}

#[allow(clippy::needless_return)]
fn write_bytes(path: &str, bytes: &[u8]) {
    let h = match File::create(path) {
        Err(e) => { panic!("create {}: {}", path, e); }
        Ok(g) => { g }
    };
    let mut w = h;
    let r = w.write_all(bytes);
    if let Err(e) = r { panic!("write {}: {}", path, e); }
    let f = w.flush();
    if let Err(e) = f { panic!("flush {}: {}", path, e); }
    let s = w.sync_all();
    if let Err(e) = s { panic!("sync {}: {}", path, e); }
}

#[allow(clippy::needless_return)]
fn rm_tree(dir: &str) {
    match read_dir(dir) {
        Err(_) => { return; }
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let p = path_to_str(&de.path());
                        match read_dir(&p) {
                            Err(_) => { let _r = remove_file(&p); }
                            Ok(_) => { rm_tree(&p); }
                        }
                    }
                }
            }
        }
    }
    let _d = remove_dir(dir);
}

/// NON-DESTRUCTIVE peek: clone the queue WITHOUT clearing it, so the test can
// " see events arrive while the production drain site stays untouched. Only the
/// test thread drains here; the feeder thread only ever pushes.
#[allow(clippy::needless_return)]
fn peek_queue(q: &WatchQueue) -> Vec<String> {
    let lk = q.lock();
    let g = match lk {
        Err(_) => { panic!("watch queue poisoned"); }
        Ok(m) => { m }
    };
    return g.clone();
}

/// Wait until the LIVE watcher has enqueued some path ending with `suffix`.
/// Returns (elapsed since t0, matched path). Panics past `deadline`.
#[allow(clippy::needless_return)]
fn await_queued(p: &Pipeline, suffix: &str, t0: Instant, deadline: Duration, what: &str) -> (f64, String) {
    loop {
        let items = peek_queue(&p.queue);
        for s in &items {
            if s.ends_with(suffix) {
                return ((t0.elapsed().as_millis() as f64) / 1000.0, s.to_string());
            }
        }
        let el = t0.elapsed();
        if el > deadline {
            panic!("{}: no live-watcher event for {:?} within {:.1} s (queue held {:?}) -- the real notify subscription/mapping is not delivering", what, suffix, deadline.as_secs_f64(), items);
        }
        sleep(POLL);
    }
}

#[allow(clippy::needless_return)]
fn grid_written(data: &str, model: &str, run_dir: &str) -> bool {
    let probe = join_path(&join_path(&join_path(data, model), run_dir), "grid.bin");
    match metadata(&probe) {
        Err(_) => { return false; }
        Ok(md) => { return md.len() > 0; }
    }
}

/// Queryable == published into the arc-swap index (spec 5): model present,
/// its run carries a field slot for `param`.
#[allow(clippy::needless_return)]
fn index_has(snap: &Arc<IndexSnapshot>, model: &str, param: &str) -> bool {
    let m = match snap.find_model(model) {
        None => { return false; }
        Some(mm) => { mm }
    };
    for r in &m.runs {
        for fe in &r.fields {
            if fe.param_key == param {
                return true;
            }
        }
    }
    return false;
}

#[allow(clippy::needless_return)]
fn cfg_toml(data: &str, ing: &str, extra_model: &str) -> String {
    return format!("[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"{0}\"\n\n[ingest]\nscan_interval_secs = 3600\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"{1}\"\nfile_glob = \"gfs_*.grib2\"{2}\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n", data, ing, extra_model);
}

#[allow(clippy::needless_return)]
fn open_pipeline(root: &str, extra_model: &str) -> (Pipeline, String, String) {
    let ing = join_path(root, "ing");
    let data = join_path(root, "data");
    let _c = create_dir_all(&ing);
    let cfg_path = join_path(root, "config.toml");
    write_bytes(&cfg_path, cfg_toml(&data, &ing, extra_model).as_bytes());
    let pr = Pipeline::open(cfg_path.as_str());
    let p = match pr {
        Err(e) => { panic!("Pipeline::open failed: {}", e); }
        Ok(pp) => { pp }
    };
    // The periodic fallback cadence this deployment would use. scan_round never
    // reads it; ONLY the wx-server driver loop sleeps on it -- and that loop is
    // not running in this process. Pinned here so the exclusion argument below
    // stays anchored to an asserted value instead of a config string.
    assert!(p.scan_interval_secs == 3600, "config must pin the fallback interval at 3600 s");
    return (p, ing, data);
}

// ===========================================================================
// TEST 1 (primary): window strategy under the REAL watcher.
// ===========================================================================
#[test]
fn live_watcher_delivers_new_file_while_scan_fallback_cannot_fire() {
    let root = "/tmp/opencode/wxpoint-nlsmoke-win".to_string();
    rm_tree(&root);
    let (mut p, ing, data) = open_pipeline(&root, "");

    // Baseline round over an EMPTY ingest dir: nothing exists yet anywhere.
    assert!(p.scan_round() == 0, "empty baseline round must ingest nothing");
    assert!(peek_queue(&p.queue).is_empty(), "no events before any file exists");

    // PRODUCTION subscription entry point. start_watchers swallows failures by
    // returning fewer handles; an empty vec means inotify never subscribed --
    // the exact silent degradation this whole test exists to catch.
    p.start_watchers();
    assert!(!p.watchers.is_empty(), "start_watchers subscribed ZERO watchers for the ingest dir: the live notify subscription is broken");

    // ---- DROP THE PRODUCT. From here until the first scan_round call below,
    // NOTHING in this process can enqueue a path except the real watcher.
    let target = join_path(&ing, "gfs_small.grib2");
    let t0 = Instant::now();
    write_bytes(&target, &fixture());

    let (t_event, got) = await_queued(&p, "/gfs_small.grib2", t0, EVENT_DEADLINE, "live watcher after product drop");
    assert!(got == target, "watcher delivered {} instead of the canonical ingest-dir path {}", got, target);

    // Round A drains the REAL event through the production drain site AND
    // observes the candidate for the FIRST time: per discovery::observe the
    // window opens now and eligibility is FALSE even with stable_window_secs=0
    // (one re-observation is promised, no matter how small the window).
    let n1 = p.scan_round();
    assert!(n1 == 0, "first observation must only OPEN the stability window (code promise), got {}", n1);
    assert!(!grid_written(&data, "gfs", "2026101912"), "product must not be queryable after the observation round");

    // Round B re-observes the unchanged identity -> eligible -> decode + atomic
    // writes + publish.
    let n2 = p.scan_round();
    let t_visible = (t0.elapsed().as_millis() as f64) / 1000.0;
    assert!(n2 == 1, "second observation under stable_window_secs=0 must ingest exactly 1 file, got {}", n2);
    assert!(p.counters.errors == 0, "clean fixture must record zero errors");
    assert!(grid_written(&data, "gfs", "2026101912"), "grid.bin must exist once the file is ingested");
    let snap = p.shared.get_index();
    assert!(index_has(&snap, "gfs", "TMP"), "published index must expose the TMP field for the run");

    // ---- THE TWO PROOFS ----
    // (1) the queued path appeared strictly between watcher-start and the first
    //     scan_round call, i.e. produced ONLY by the real subscription;
    // (2) end-to-end latency beats both the deadline and the configured
    //     fallback cadence by orders of magnitude, so the periodic full scan
    //     (the ONLY consumer of scan_interval_secs, in the wx-server main loop)
    //     could not have been responsible for discovery.
    assert!(t_event <= EVENT_DEADLINE.as_secs_f64(), "event latency {} s exceeded the deadline", t_event);
    assert!(t_visible <= EVENT_DEADLINE.as_secs_f64(), "drop-to-queryable took {} s, above the bounded window", t_visible);
    assert!(t_visible < 60.0, "measured drop-to-queryable {} s should sit FAR below the 3600 s fallback cadence", t_visible);
    eprintln!("notify_live_smoke[window]: live-watcher event after {:?} s; drop-to-queryable after {:?} s; scan_interval_secs pinned at 3600 (fallback could not fire)", t_event, t_visible);

    rm_tree(&root);
}

// ===========================================================================
// TEST 2 (cheap variant): sidecar-marker strategy under the REAL watcher.
// File present-but-marker-later: ineligible while only the product exists,
// then ONE round after the marker lands (sidecar_ready needs no
// re-observation) the product is queryable.
// ===========================================================================
#[test]
fn live_watcher_sidecar_marker_flips_eligibility_immediately() {
    let root = "/tmp/opencode/wxpoint-nlsmoke-sidecar".to_string();
    rm_tree(&root);
    let (mut p, ing, data) = open_pipeline(&root, "\nsidecar_suffix = \".ok\"");

    assert!(p.scan_round() == 0);
    assert!(peek_queue(&p.queue).is_empty());
    p.start_watchers();
    assert!(!p.watchers.is_empty(), "start_watchers subscribed ZERO watchers for the ingest dir");

    // Product WITHOUT marker: the watcher sees it, the round sees it, but the
    // sidecar strategy keeps it INELIGIBLE (marker absent).
    let target = join_path(&ing, "gfs_small.grib2");
    let t_prod = Instant::now();
    write_bytes(&target, &fixture());
    let (t_prod_event, _) = await_queued(&p, "/gfs_small.grib2", t_prod, EVENT_DEADLINE, "live watcher after marker-less product drop");
    let np = p.scan_round();
    assert!(np == 0, "a marker-less product must stay ineligible under the sidecar strategy, got {}", np);
    assert!(!grid_written(&data, "gfs", "2026101912"), "no ingestion before the marker exists");

    // Marker lands LATER -- the flow this strategy exists for. The same live
    // subscription must carry it, and sidecar_ready flips eligible on SIGHTING,
    // so exactly one round after the marker the answer is ready.
    let marker = format!("{}{}", target, ".ok");
    let t_mark = Instant::now();
    write_bytes(&marker, b"done");
    let (t_mark_event, _) = await_queued(&p, ".ok", t_mark, EVENT_DEADLINE, "live watcher after marker arrival");
    let nm = p.scan_round();
    let t_visible = (t_mark.elapsed().as_millis() as f64) / 1000.0;
    assert!(nm == 1, "one round after the marker lands the product must be ingested immediately, got {}", nm);
    assert!(p.counters.errors == 0);
    assert!(grid_written(&data, "gfs", "2026101912"));
    let snap = p.shared.get_index();
    assert!(index_has(&snap, "gfs", "TMP"));
    assert!(t_visible <= EVENT_DEADLINE.as_secs_f64(), "marker-to-queryable took {} s, above the bounded window", t_visible);
    eprintln!("notify_live_smoke[sidecar]: product event after {:?} s; marker event after {:?} s; marker-to-queryable after {:?} s with scan_interval_secs pinned at 3600", t_prod_event, t_mark_event, t_visible);

    rm_tree(&root);
}
