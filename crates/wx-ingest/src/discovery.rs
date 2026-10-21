//! File discovery (spec §4.1): per-model ingest_dir watched with the notify
//! crate (event latency) PLUS an authoritative periodic full scan — events
//! are only an optimization; the scan ALWAYS reconciles (network mounts drop
//! events). Stability: window strategy (size+mtime unchanged for
//! stable_window_secs) or sidecar-marker strategy per model config.

use notify::watcher;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use notify::DebouncedEvent;
use std::thread;
use std::sync::mpsc::channel;
use std::sync::Mutex;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::fs::read_dir;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// basename glob: '*' any run, '?' one char, everything literal
// ---------------------------------------------------------------------------

pub fn glob_match(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let nm: Vec<char> = name.chars().collect();
    let mut pi = 0;
    let mut ni = 0;
    let mut star: i64 = -1;
    let mut mark = 0;
    while ni < nm.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == nm[ni]) {
            pi += 1;
            ni += 1;
            continue;
        }
        if pi < pat.len() && pat[pi] == '*' {
            star = pi as i64;
            mark = ni;
            pi += 1;
            continue;
        }
        if star >= 0 {
            pi = star as usize + 1;
            mark += 1;
            ni = mark;
            continue;
        }
        return false;
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    return pi == pat.len();
}

// ---------------------------------------------------------------------------
// recursive walk: every regular-file candidate under dir (depth-first, sorted
// for determinism). Directories are detected by trying to list them, which
// avoids depending on a file-type accessor in this dialect.
// ---------------------------------------------------------------------------

pub fn walk_files(dir: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::<String>::new();
    let mut stack: Vec<String> = Vec::<String>::new();
    stack.push(dir.to_string());
    while !stack.is_empty() {
        let cur_o = stack.pop();
        match cur_o {
            None => { break; }
            Some(cur) => {
        match read_dir(&cur) {
            Err(_) => {
                // not a directory: a file candidate (or vanished entry)
                out.push(cur.to_string());
            }
            Ok(it) => {
                for e in it {
                    match e {
                        Err(_) => {}
                        Ok(de) => {
                            let p = path_to_str(&de.path());
                            stack.push(p.to_string());
                        }
                    }
                }
            }
        }
            }
        }
    }
    out.sort();
    return out;
}

/// Last path component (basename) of a path string.
pub fn base_name(path: String) -> String {
    let mut best = 0;
    for k in 0 .. path.len() {
        if path[k .. k + 1] == *"/" {
            best = k + 1;
        }
    }
    return path[best ..].to_string();
}

pub fn path_to_str(p: &Path) -> String {
    match p.to_str() {
        None => { return "".to_string(); }
        Some(s) => { return s.to_string(); }
    }
}

// ---------------------------------------------------------------------------
// stability tracking (window strategy state lives across scan rounds)
// ---------------------------------------------------------------------------

/// Monotonic nanosecond clock (V1.2 phase B0). Built on `std::time::Instant`,
/// which this toolchain exposes as `Instant::now()` / subtraction /
/// `Duration::as_nanos()`. EVERY in-memory timing decision (the stability
/// window) is measured against this, NEVER against wall-clock file mtimes:
/// a backward wall-clock jump (NTP step-back, container restore, VM pause)
/// used to make `now_ns - first_observed` NEGATIVE, so a file whose stat had
/// been stable for the whole window stayed INELIGIBLE forever and a scan round
/// silently ingested NOTHING (this is the mechanism behind the flaky
/// `r2 == 4` acceptance gate). mtime remains part of the file IDENTITY (a
/// changed mtime means a re-delivery and resets the window) but plays NO part
/// in measuring how long the identity has been observed stable.
#[derive(Clone, Debug)]
pub struct MonoClock {
    pub t0: Instant, // arbitrary process-local origin
}

impl MonoClock {
    /// Nanoseconds elapsed since this clock's origin. Never decreases.
    pub fn elapsed_ns(self: &Self) -> i64 {
        let d = Instant::now() - self.t0;
        return d.as_nanos() as i64;
    }

    pub fn new_clock() -> MonoClock {
        return MonoClock { t0: Instant::now() };
    }
}

#[derive(Clone, Debug)]
pub struct StabilityTracker {
    // (path, size, mtime_ns, first_observed_MONOTONIC_ns)
    pub obs: Vec<(String, u64, i64, i64)>,
}

impl StabilityTracker {
    /// Window strategy: eligible once (size,mtime) has been continuously
    /// observed unchanged for `window_ns` of MONOTONIC time. Re-observed changed
    /// stat resets the observation stamp. `now_ns` MUST come from
    /// [`MonoClock::elapsed_ns`] (wall-clock stamps are identity-only).
    pub fn observe(self: &mut Self, path: String, size: u64, mtime_ns: i64,
                   now_ns: i64, window_ns: i64) -> bool {
        for o in &mut self.obs {
            if o.0 == path {
                if o.1 != size || o.2 != mtime_ns {
                    o.1 = size;
                    o.2 = mtime_ns;
                    o.3 = now_ns;
                    return false;
                }
                if now_ns - o.3 >= window_ns {
                    return true;
                }
                return false;
            }
        }
        self.obs.push((path, size, mtime_ns, now_ns));
        return false;
    }

    /// Sidecar strategy: `<path><suffix>` exists => eligible immediately.
    pub fn sidecar_ready(self: &Self, path: String, suffix: String) -> bool {
        let marker = format!("{}{}", path, suffix);
        use std::fs::metadata;
        match metadata(&marker) {
            Err(_) => { return false; }
            Ok(_) => { return true; }
        }
    }
}

// ---------------------------------------------------------------------------
// notify watchers: create/rename/change events push paths onto a shared queue
// that scan rounds drain first (latency optimization over the full scan).
// ---------------------------------------------------------------------------

pub type WatchQueue = Arc<Mutex<Vec<String>>>;

/// Start recursive watchers for each model dir. Returns the watcher handles
/// (caller MUST keep them alive) and pushes event paths into `queue`.
pub fn start_watchers(dirs: Vec<String>, queue: WatchQueue) -> Vec<RecommendedWatcher> {
    let mut watchers: Vec<RecommendedWatcher> = Vec::<RecommendedWatcher>::new();
    for d in dirs {
        let (tx, rx) = channel();
        match watcher(tx, Duration::from_millis(200)) {
            Err(_) => { continue; }
            Ok(w0) => {
                let mut w = w0;
                match w.watch(d, RecursiveMode::Recursive) {
                    Err(_) => {}
                    Ok(_) => {}
                }
                watchers.push(w);
                spawn_feeder(rx, queue.clone());
            }
        }
    }
    return watchers;
}

/// Feeder thread: block on recv(), forward interesting paths to the queue
/// until the channel closes (watcher dropped).
fn spawn_feeder(rx: std::sync::mpsc::Receiver<DebouncedEvent>, q: WatchQueue) {
        thread::spawn(move || {
        loop {
            match rx.recv() {
                Err(_) => { break; }
                Ok(ev) => {
                    match ev {
                        DebouncedEvent::Create(p) => { push_path(q.clone(), p); }
                        DebouncedEvent::Write(p) => { push_path(q.clone(), p); }
                        DebouncedEvent::Chmod(p) => { push_path(q.clone(), p); }
                        DebouncedEvent::Rename(_from, to) => { push_path(q.clone(), to); }
                        DebouncedEvent::NoticeWrite(p) => { push_path(q.clone(), p); }
                        DebouncedEvent::Remove(p) => { push_path(q.clone(), p); }
                        DebouncedEvent::Rescan => {}
                        DebouncedEvent::Error(_, _) => {}
                        DebouncedEvent::NoticeRemove(_) => {}
                    }
                }
            }
        }
    });
}

fn push_path(q: WatchQueue, p: PathBuf) {
    let s = path_to_str(&p);
    let m = q.lock();
    match m {
        Err(_) => {}
        Ok(mg) => {
            let mut v = mg;
            v.push(s.to_string());
        }
    }
}

/// Drain pending event paths (deduped) — called at the head of a scan round.
pub fn drain_queue(q: WatchQueue) -> Vec<String> {
    let m = q.lock();
    match m {
        Err(_) => { return [].to_vec(); }
        Ok(mg0) => {
            let mut mm = mg0;
            let mut items: Vec<String> = Vec::<String>::new();
            for k in 0 .. mm.len() {
                items.push(mm[k].to_string());
            }
            mm.clear();
            items.sort();
            let mut res: Vec<String> = Vec::<String>::new();
            for s in &items {
                if !res.is_empty() && res[res.len() - 1] == s.to_string() {
                    continue;
                }
                res.push(s.to_string());
            }
            return res;
        }
    }
}
