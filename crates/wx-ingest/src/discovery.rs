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
// for determinism). V1.3 phase E3b SYMLINK SAFETY: the pre-E3b walk classified
// directories by "read_dir succeeded", so a symlink pointing at an ANCESTOR
// directory re-entered itself forever and hung the ingest thread (and the
// startup wind preflight) INFINITELY. Directories are now classified with
// `metadata()` (which follows symlinks) and recorded in a VISITED set keyed by
// their CANONICAL path: a link to an ancestor resolves to an already-walked
// identity and is skipped with a note instead of looping. A hard cap on
// emitted paths bounds even pathological trees per round.
// ---------------------------------------------------------------------------

use std::collections::HashSet;
use std::fs::canonicalize;
use std::fs::metadata;

/// Hard cap on paths emitted by ONE walk (V1.3 phase E3b). Production ingest
/// dirs hold thousands of files; a million is orders of magnitude beyond that,
/// so the cap only fires on genuinely pathological trees — and then loudly,
/// once, through the caller's notes channel.
pub const WALK_MAX_PATHS: usize = 1_000_000;

pub fn walk_files(dir: &str) -> Vec<String> {
    let mut notes: Vec<String> = Vec::<String>::new();
    return walk_files_bounded(dir, WALK_MAX_PATHS, &mut notes);
}

/// The symlink-safe, capped walk. Anomalies (loop skips, unresolvable
/// directories, cap hits) are APPENDED to `notes` for the caller to surface —
/// `walk_files` keeps the old signature and simply drops them (callers that
/// cannot log still get correct results, just silently).
pub fn walk_files_bounded(dir: &str, cap: usize, notes: &mut Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::<String>::new();
    let mut stack: Vec<String> = Vec::<String>::new();
    let mut visited: HashSet<String> = HashSet::<String>::new();
    stack.push(dir.to_string());
    while !stack.is_empty() {
        let cur_o = stack.pop();
        let cur = match cur_o {
            None => { break; }
            Some(c) => { c }
        };
        if out.len() >= cap {
            // ONE loud note, then stop: the next scan round resumes where this
            // one stopped adding (the break makes a repeat-note flag pointless).
            notes.push(format!("walk of {}: reached the {}-path cap; remaining entries are IGNORED this round (the next scan round sees them)", dir, cap));
            break;
        }
        // Classify FIRST (metadata FOLLOWS symlinks, so a symlink-to-directory
        // is walked as a directory — under its canonical identity).
        let md = metadata(&cur);
        let is_dir = match md {
            Err(_) => { false } // vanished or unreadable target: leaf candidate (old behavior)
            Ok(m) => { m.is_dir() }
        };
        if !is_dir {
            out.push(cur.to_string());
            continue;
        }
        let cp = canonicalize(&cur);
        let key = match cp {
            Err(_) => {
                // Directory exists but cannot be resolved (permissions on some
                // path component): skipping the SUBTREE is the safe choice —
                // descending an unresolvable path re-opens exactly the loop
                // hole this fix closes. Noted, never silent.
                notes.push(format!("walk of {}: cannot canonicalize {} (permissions?): skipping that subtree", dir, cur));
                continue;
            }
            Ok(pc) => { path_to_str(&pc) }
        };
        if visited.contains(&key) {
            notes.push(format!("walk of {}: {} resolves to the ALREADY-WALKED directory {} (symlink loop?) — skipped", dir, cur, key));
            continue;
        }
        visited.insert(key.to_string());
        match read_dir(&cur) {
            Err(_) => {} // vanished between metadata and read_dir: fine
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
    // V1.2 phase C: the vector doubles as an LRU — an entry is MOVED TO THE
    // BACK whenever its path is observed, so the FRONT holds the paths that
    // have gone quiet longest (vanished files, names that stopped matching the
    // glob). [`prune_vanished`] drops entries whose file is gone; [`observe`]
    // enforces [`STABILITY_OBS_CAP`] by evicting from the front.
    pub obs: Vec<(String, u64, i64, i64)>,
}

/// Generous upper bound on remembered stability observations (V1.2 phase C
/// state hygiene). One entry per candidate FILE, so 10 000 covers any realistic
/// single-model ingest dir many times over; eviction only costs a re-observation
/// round for an evicted path (its window restarts), NEVER a wrong answer.
pub const STABILITY_OBS_CAP: usize = 10_000;

impl StabilityTracker {
    /// Window strategy: eligible once (size,mtime) has been continuously
    /// observed unchanged for `window_ns` of MONOTONIC time. Re-observed changed
    /// stat resets the observation stamp. `now_ns` MUST come from
    /// [`MonoClock::elapsed_ns`] (wall-clock stamps are identity-only).
    /// Every observation also bumps the path's RECENCY (see the LRU note on
    /// [`StabilityTracker`]) and enforces [`STABILITY_OBS_CAP`].
    pub fn observe(self: &mut Self, path: String, size: u64, mtime_ns: i64,
                   now_ns: i64, window_ns: i64) -> bool {
        let mut hit: i64 = -1;
        for k in 0 .. self.obs.len() {
            if self.obs[k].0 == path {
                hit = k as i64;
                break;
            }
        }
        if hit < 0 {
            self.obs.push((path, size, mtime_ns, now_ns));
            self.trim_obs();
            return false;
        }
        let ki = hit as usize;
        let o = self.obs[ki].clone();
        if o.1 != size || o.2 != mtime_ns {
            // IDENTITY CHANGED (a re-delivery): reset the stamp, stay INELIGIBLE.
            self.obs[ki] = (path, size, mtime_ns, now_ns);
            self.bump_recency(ki);
            return false;
        }
        let eligible = now_ns - o.3 >= window_ns;
        self.bump_recency(ki);
        return eligible;
    }

    /// Move one entry to the back of the LRU (recency bump).
    fn bump_recency(self: &mut Self, ki: usize) {
        if ki + 1 == self.obs.len() {
            return; // already the most recent
        }
        let e = self.obs.remove(ki);
        self.obs.push(e);
    }

    /// Enforce [`STABILITY_OBS_CAP`] by evicting the LEAST recently observed
    /// paths (front of the vector).
    pub fn trim_obs(self: &mut Self) {
        while self.obs.len() > STABILITY_OBS_CAP {
            let _e = self.obs.remove(0);
        }
    }

    /// V1.2 phase C STATE HYGIENE: drop observations whose FILE no longer exists
    /// (deleted upstream, or renamed away mid-delivery). BOUNDED: ONE stat() per
    /// remembered path, called ONCE per scan round by `pipeline::scan_round`;
    /// returns the number dropped so the caller can log it. A still-present file
    /// keeps its window state untouched, so this can never shorten a window.
    pub fn prune_vanished(self: &mut Self) -> usize {
        use std::fs::metadata;
        let mut kept: Vec<(String, u64, i64, i64)> = Vec::<(String, u64, i64, i64)>::new();
        let mut n = 0usize;
        for o in &self.obs {
            match metadata(&o.0) {
                Err(_) => { n += 1; }
                Ok(_) => { kept.push(o.clone()); }
            }
        }
        if n > 0 {
            self.obs = kept;
        }
        return n;
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
