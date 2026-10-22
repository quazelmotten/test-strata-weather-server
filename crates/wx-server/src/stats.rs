//! /metrics counters (spec §7): query latency histogram (manual µs buckets +
//! atomics, no heavy dependency), per-status-class query counts, ingest lag
//! and ingest error count. Latency is measured from request entry to response
//! start using CLOCK_MONOTONIC via libc (this toolchain's `Instant` has no
//! absolute-time form, so epoch-ns mtimes need CLOCK_REALTIME). Since V1.2
//! phase B the exposition also carries `wxpoint_ignored_files_total`,
//! `wxpoint_quarantined_files_total` and `wxpoint_ingest_groups_written_total`,
//! and every ingest-side gauge reads max(legacy store, LIVE pipeline mirror) so
//! a scan round in flight is visible immediately. Since V1.2 phase D it ALSO
//! exposes the wx-core storage counters as `wxpoint_storage_renames_total`,
//! `wxpoint_storage_bytes_written_total` and `wxpoint_storage_fsyncs_total` —
//! the write-amplification evidence an ingest benchmark needs (README §7.3).
//! Since V1.3 phase E3a it additionally exposes the STATE writer, which those
//! storage counters deliberately exclude: `wxpoint_state_writes_total` and
//! `wxpoint_state_bytes_written` (batched per-chunk persistence makes these
//! small; the 1.0000x field-path amplification headline stays scoped to the
//! field/grid/manifest writers).

use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use libc::timespec;
use libc::clock_gettime;
use libc::CLOCK_MONOTONIC;
use libc::CLOCK_REALTIME;
use wx_core::storage::{counters_snapshot};
use wx_ingest::state::{state_counters_snapshot};
use std::sync::Arc;
use std::sync::Mutex;
use wx_ingest::pipeline::{LiveCounters};

/// Upper bounds of the latency buckets in microseconds. Chosen around the
/// spec §8 targets (p50 < 50 us, p99 < 250 us) with decade-ish granularity.
pub const BUCKET_US: [u64; 13] = [1, 2, 5, 10, 20, 50, 100, 250, 500, 1000, 2500, 5000, 10000];

pub struct QueryStats {
    pub buckets: Vec<AtomicU64>, // one counter per BUCKET_US bound
    pub overflow: AtomicU64, // queries slower than the last bucket
    pub qcount: AtomicU64, // latency samples (= /v1/point queries served)
    pub qsum_us: AtomicU64, // sum of samples in microseconds
    pub c2xx: AtomicU64, // responses by status class, all endpoints
    pub c4xx: AtomicU64,
    pub c5xx: AtomicU64,
    pub ingest_errors: AtomicU64, // mirrored from Pipeline.counters by the ingest thread
    /// Structurally-unusable messages deliberately skipped (layer/range levels,
    /// coordinate lists, unknown grid/PDT templates): spec §4.2 wants them noted
    /// rather than failed, so they get their own counter and never inflate
    /// wxpoint_ingest_errors_total.
    pub skipped_messages: AtomicU64,
    pub ingested_files: AtomicU64,
    pub last_mtime_ns: AtomicI64, // max mtime (epoch ns) over ingested files
    /// Files whose magic bytes are not `GRIB`: never decoded, quarantined
    /// kind=IGNORED (V1.2 phase B poison quarantine).
    pub ignored_files: AtomicU64,
    /// Decode failures recorded WITH their content hash so a corrupt file is not
    /// re-decoded every scan round (kind=FAILED).
    pub quarantined_files: AtomicU64,
    /// Field files written = the write-once GROUPING unit. Compare with a message
    /// count to see the amplification phase B removed (e.g. 8 groups vs 22
    /// messages per GFS hour).
    pub groups_written: AtomicU64,
    /// V1.2 phase C: candidate FILES vetoed BEFORE decode because their SECTION-1
    /// reference time is strictly older than the oldest kept run while the registry
    /// is already full (see `Pipeline.retention_skip_reason`). Counted once per
    /// (file,content hash), never an error.
    pub retention_skipped: AtomicU64,
    /// V1.3 phase E1: statistically-distinct groups rejected because another
    /// group already claimed their field file within one source file
    /// (wxpoint_stat_collisions_total; fed by the LIVE mirror only — the
    /// legacy setters predate this counter).
    pub stat_collisions: AtomicU64,
    /// V1.3 phase E4a: structural message skips broken down BY KIND —
    /// coordinate lists, unimplemented grid templates, unimplemented product
    /// definition templates (wxpoint_skip_coordlists_total /
    /// wxpoint_skip_grid_total / wxpoint_skip_pdt_total). FLAT counters on
    /// purpose: this metrics surface has no label machinery. Fed by the LIVE
    /// mirror only, like `stat_collisions`. The three partition the
    /// STRUCTURAL subset of wxpoint_skipped_messages_total (policy skips —
    /// wind-pair drops, orphaned components — count in the total only).
    pub skip_coordlists: AtomicU64,
    pub skip_grid: AtomicU64,
    pub skip_pdt: AtomicU64,
    /// Slot holding the ingest pipeline's LIVE counter mirror (V1.2 phase B).
    /// Detached (fresh zeros) until [`QueryStats.attach_live`] points it at the
    /// Pipeline's own `Arc<LiveCounters>`; from then on `/metrics` reflects
    /// ingest progress DURING a scan round instead of only after it. A
    /// `Mutex<Arc<...>>` because the slot must be swappable through the shared
    /// `Arc<QueryStats>` the HTTP handlers hold.
    pub live_slot: Mutex<Arc<LiveCounters>>,
}

impl QueryStats {
    /// Record one point-query latency sample in microseconds.
    pub fn record_latency(self: &Self, us: u64) {
        let _ = self.qcount.fetch_add(1, Ordering::Relaxed);
        let _ = self.qsum_us.fetch_add(us, Ordering::Relaxed);
        for k in 0 .. self.buckets.len() {
            if us <= BUCKET_US[k] {
                let _ = self.buckets[k].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        let _ = self.overflow.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a response by status class.
    pub fn record_status(self: &Self, status: u16) {
        if status >= 500 {
            let _ = self.c5xx.fetch_add(1, Ordering::Relaxed);
        } else if status >= 400 {
            let _ = self.c4xx.fetch_add(1, Ordering::Relaxed);
        } else {
            let _ = self.c2xx.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Mirror ingest-side counters (single writer: the ingest thread). Kept for
    /// back-compat (V1.0/V1.1 embedders and tests call it with ABSOLUTE values).
    /// Since V1.2 phase B it ALSO merges those values into the live mirror
    /// monotonically, so an attached pipeline's mid-round progress is never rolled
    /// backwards by a post-round store.
    pub fn set_ingest(self: &Self, errors: u64, files: u64, skipped: u64, mtime_ns: i64) {
        self.ingest_errors.store(errors, Ordering::Relaxed);
        self.skipped_messages.store(skipped, Ordering::Relaxed);
        self.ingested_files.store(files, Ordering::Relaxed);
        self.last_mtime_ns.store(mtime_ns, Ordering::Relaxed);
        let lv = self.live();
        lv.merge_abs(errors, files, skipped, 0, 0, 0, mtime_ns);
    }

    /// V1.2 phase B DELTA API: for embedders that do NOT hold the pipeline's
    /// `Arc<LiveCounters>` (tests, other binaries): adds increments into the
    /// legacy atomics and takes the max mtime. The live mirror is deliberately
    /// untouched here — when a pipeline IS attached it owns that mirror and would
    /// otherwise be double-counted.
    pub fn add_ingest(self: &Self, errors_delta: u64, files_delta: u64, skipped_delta: u64,
                      ignored_delta: u64, quarantined_delta: u64, groups_delta: u64,
                      mtime_ns: i64) {
        let _a = self.ingest_errors.fetch_add(errors_delta, Ordering::Relaxed);
        let _b = self.ingested_files.fetch_add(files_delta, Ordering::Relaxed);
        let _c = self.skipped_messages.fetch_add(skipped_delta, Ordering::Relaxed);
        let _d = self.ignored_files.fetch_add(ignored_delta, Ordering::Relaxed);
        let _e = self.quarantined_files.fetch_add(quarantined_delta, Ordering::Relaxed);
        let _f = self.groups_written.fetch_add(groups_delta, Ordering::Relaxed);
        if mtime_ns > self.last_mtime_ns.load(Ordering::Relaxed) {
            self.last_mtime_ns.store(mtime_ns, Ordering::Relaxed);
        }
    }

    /// Point the live slot at a pipeline's counter mirror (called ONCE at startup
    /// by `main`, right after the Pipeline is opened).
    pub fn attach_live(self: &Self, arc: Arc<LiveCounters>) {
        let m = self.live_slot.lock();
        match m {
            Err(_) => {}
            Ok(mut g) => { *g = arc; }
        }
    }

    /// Current mirror (detached fresh instance when nothing was attached).
    pub fn live(self: &Self) -> Arc<LiveCounters> {
        let m = self.live_slot.lock();
        match m {
            Err(_) => { return Arc::new(LiveCounters::new_live()); }
            Ok(g) => { return g.clone(); }
        }
    }

    /// Prometheus text exposition (spec §7: version 0.0.4 content type is set
    /// by the handler; this renders the sample lines).
    pub fn render_prometheus(self: &Self) -> String {
        let mut out = "# wx-point metrics\n".to_string();
        out = format!("{0}{1}", out, "wxpoint_queries_total{status=\"2xx\"}");
        out = format!("{0}{1}", out, self.c2xx.load(Ordering::Relaxed));
        out = format!("{0}{1}", out, "\nwxpoint_queries_total{status=\"4xx\"}");
        out = format!("{0}{1}", out, self.c4xx.load(Ordering::Relaxed));
        out = format!("{0}{1}", out, "\nwxpoint_queries_total{status=\"5xx\"}");
        out = format!("{0}{1}", out, self.c5xx.load(Ordering::Relaxed));
        // Ingest-side values: max(legacy store, LIVE mirror) so a scan round in
        // flight is visible immediately (phase B) while pre-B callers that only
        // use `set_ingest`/`add_ingest` keep working unchanged.
        let lv = self.live();
        let mut v_err = self.ingest_errors.load(Ordering::Relaxed);
        let l_err = lv.errors.load(Ordering::Relaxed);
        if l_err > v_err {
            v_err = l_err;
        }
        out = format!("{0}{1}", out, "\nwxpoint_ingest_errors_total ");
        out = format!("{0}{1}", out, v_err);
        let mut v_files = self.ingested_files.load(Ordering::Relaxed);
        let l_files = lv.ingested_files.load(Ordering::Relaxed);
        if l_files > v_files {
            v_files = l_files;
        }
        out = format!("{0}{1}", out, "\nwxpoint_ingested_files_total ");
        out = format!("{0}{1}", out, v_files);
        let mut v_skip = self.skipped_messages.load(Ordering::Relaxed);
        let l_skip = lv.skipped_messages.load(Ordering::Relaxed);
        if l_skip > v_skip {
            v_skip = l_skip;
        }
        out = format!("{0}{1}", out, "\nwxpoint_skipped_messages_total ");
        out = format!("{0}{1}", out, v_skip);
        // V1.2 phase B quarantine + grouping counters.
        let mut v_ign = self.ignored_files.load(Ordering::Relaxed);
        let l_ign = lv.ignored_files.load(Ordering::Relaxed);
        if l_ign > v_ign {
            v_ign = l_ign;
        }
        out = format!("{0}{1}", out, "\nwxpoint_ignored_files_total ");
        out = format!("{0}{1}", out, v_ign);
        let mut v_quar = self.quarantined_files.load(Ordering::Relaxed);
        let l_quar = lv.quarantined_files.load(Ordering::Relaxed);
        if l_quar > v_quar {
            v_quar = l_quar;
        }
        out = format!("{0}{1}", out, "\nwxpoint_quarantined_files_total ");
        out = format!("{0}{1}", out, v_quar);
        let mut v_grp = self.groups_written.load(Ordering::Relaxed);
        let l_grp = lv.groups_written.load(Ordering::Relaxed);
        if l_grp > v_grp {
            v_grp = l_grp;
        }
        out = format!("{0}{1}", out, "\nwxpoint_ingest_groups_written_total ");
        out = format!("{0}{1}", out, v_grp);
        // V1.2 phase C: files vetoed before decode by the §4.6 retention-aware skip.
        let mut v_stale = self.retention_skipped.load(Ordering::Relaxed);
        let l_stale = lv.retention_skipped.load(Ordering::Relaxed);
        if l_stale > v_stale {
            v_stale = l_stale;
        }
        out = format!("{0}{1}", out, "\nwxpoint_retention_skipped_files_total ");
        out = format!("{0}{1}", out, v_stale);
        // V1.3 phase E1: statistically-distinct groups rejected inside one
        // source file (first-in-message-order wins; loud, never an error).
        let mut v_col = self.stat_collisions.load(Ordering::Relaxed);
        let l_col = lv.stat_collisions.load(Ordering::Relaxed);
        if l_col > v_col {
            v_col = l_col;
        }
        out = format!("{0}{1}", out, "\nwxpoint_stat_collisions_total ");
        out = format!("{0}{1}", out, v_col);
        // V1.3 phase E4a: structural skip tallies BY KIND (flat, no labels).
        // A gap between these and wxpoint_skipped_messages_total is EXPECTED
        // and meaningful: policy skips (wind pair, orphans) live only in the
        // total; decoder-coverage gaps live here with their template numbers
        // spelled out in the ingest log note.
        let mut v_kc = self.skip_coordlists.load(Ordering::Relaxed);
        let l_kc = lv.skip_coordlists.load(Ordering::Relaxed);
        if l_kc > v_kc {
            v_kc = l_kc;
        }
        out = format!("{0}{1}", out, "\nwxpoint_skip_coordlists_total ");
        out = format!("{0}{1}", out, v_kc);
        let mut v_kg = self.skip_grid.load(Ordering::Relaxed);
        let l_kg = lv.skip_grid.load(Ordering::Relaxed);
        if l_kg > v_kg {
            v_kg = l_kg;
        }
        out = format!("{0}{1}", out, "\nwxpoint_skip_grid_total ");
        out = format!("{0}{1}", out, v_kg);
        let mut v_kp = self.skip_pdt.load(Ordering::Relaxed);
        let l_kp = lv.skip_pdt.load(Ordering::Relaxed);
        if l_kp > v_kp {
            v_kp = l_kp;
        }
        out = format!("{0}{1}", out, "\nwxpoint_skip_pdt_total ");
        out = format!("{0}{1}", out, v_kp);
        // V1.2 phase D: write-amplification observability straight from the wx-core
        // storage counters (renames / bytes handed to writers / fsync syscalls).
        // These are PROCESS-WIDE totals since boot, which is exactly what an ingest
        // benchmark needs (a fresh process starts at zero): compare the bytes gauge
        // with the final on-disk store size to see the amplification factor, and the
        // renames/fsyncs gauges against the number of FIELD FILES.
        let sc = counters_snapshot();
        out = format!("{0}{1}", out, "\nwxpoint_storage_renames_total ");
        out = format!("{0}{1}", out, sc.renames);
        out = format!("{0}{1}", out, "\nwxpoint_storage_bytes_written_total ");
        out = format!("{0}{1}", out, sc.bytes_written);
        out = format!("{0}{1}", out, "\nwxpoint_storage_fsyncs_total ");
        out = format!("{0}{1}", out, sc.fsyncs);
        // V1.3 phase E3a: the STATE writer is NOT part of the storage counters
        // above (documented exclusion), so report it separately: successful
        // files.json saves + bytes serialized. Batched cadence (per decode
        // chunk + round end) is what keeps these tiny.
        let stc = state_counters_snapshot();
        out = format!("{0}{1}", out, "\nwxpoint_state_writes_total ");
        out = format!("{0}{1}", out, stc.0);
        out = format!("{0}{1}", out, "\nwxpoint_state_bytes_written ");
        out = format!("{0}{1}", out, stc.1);
        // ingest lag: file mtime -> now, seconds (mtime is epoch ns).
        let mtime = self.last_mtime_ns.load(Ordering::Relaxed);
        let mt2 = lv.last_mtime_ns.load(Ordering::Relaxed);
        let mut mtime_f = mtime;
        if mt2 > mtime_f {
            mtime_f = mt2;
        }
        let mut lag = 0.0;
        if mtime_f > 0 {
            let now = realtime_ns();
            lag = ((now - mtime_f) as f64) / 1000000000.0;
            if lag < 0.0 {
                lag = 0.0;
            }
        }
        out = format!("{0}{1}", out, "\nwxpoint_ingest_lag_seconds ");
        out = format!("{0}{1}", out, lag);
        // cumulative histogram
        let cnt = self.qcount.load(Ordering::Relaxed);
        let mut cum = 0;
        for k in 0 .. self.buckets.len() {
            cum = cum + self.buckets[k].load(Ordering::Relaxed);
            out = format!("{0}{1}", out, "wxpoint_query_latency_us_bucket{le=\"");
            out = format!("{0}{1}", out, BUCKET_US[k]);
            out = format!("{0}{1}", out, "\"} ");
            out = format!("{0}{1}", out, cum);
            out = format!("{0}{1}", out, "\n");
        }
        cum = cum + self.overflow.load(Ordering::Relaxed);
        out = format!("{0}{1}", out, "wxpoint_query_latency_us_bucket{le=\"+inf\"}");
        out = format!("{0}{1}", out, cum);
        out = format!("{0}{1}", out, "\nwxpoint_query_latency_us_count ");
        out = format!("{0}{1}", out, cnt);
        out = format!("{0}{1}", out, "\nwxpoint_query_latency_us_sum ");
        out = format!("{0}{1}", out, self.qsum_us.load(Ordering::Relaxed));
        out = format!("{0}{1}", out, "\n");
        return out;
    }
}

/// Fresh zeroed stats wrapped in an Arc (shared by HTTP handlers and the
/// ingest thread's counter mirror). The live slot starts DETACHED (a private
/// mirror); `attach_live` or [`new_stats_with`] shares the Pipeline's own.
pub fn new_stats() -> Arc<QueryStats> {
    return new_stats_with(Arc::new(LiveCounters::new_live()));
}

fn fresh_buckets() -> Vec<AtomicU64> {
    let mut buckets: Vec<AtomicU64> = Vec::<AtomicU64>::new();
    for _ in 0 .. BUCKET_US.len() {
        buckets.push(AtomicU64::new(0));
    }
    return buckets;
}

/// Fresh zeroed stats whose live slot points at a CALLER-SUPPLIED mirror (used
/// by `main` to share the Pipeline's `Arc<LiveCounters>` from birth instead of
/// swapping it later).
pub fn new_stats_with(live: Arc<LiveCounters>) -> Arc<QueryStats> {
    let qs = QueryStats {
        buckets: fresh_buckets(),
        overflow: AtomicU64::new(0),
        qcount: AtomicU64::new(0),
        qsum_us: AtomicU64::new(0),
        c2xx: AtomicU64::new(0),
        c4xx: AtomicU64::new(0),
        c5xx: AtomicU64::new(0),
        ingest_errors: AtomicU64::new(0),
        skipped_messages: AtomicU64::new(0),
        ingested_files: AtomicU64::new(0),
        last_mtime_ns: AtomicI64::new(0),
        ignored_files: AtomicU64::new(0),
        quarantined_files: AtomicU64::new(0),
        groups_written: AtomicU64::new(0),
        retention_skipped: AtomicU64::new(0),
        stat_collisions: AtomicU64::new(0),
        skip_coordlists: AtomicU64::new(0),
        skip_grid: AtomicU64::new(0),
        skip_pdt: AtomicU64::new(0),
        live_slot: Mutex::<Arc<LiveCounters>>::new(live),
    };
    return Arc::new(qs);
}

/// Monotonic nanoseconds (latency measurement; libc because std has no clock).
pub fn mono_ns() -> i64 {
    let mut ts = timespec { tv_sec: 0 as i64, tv_nsec: 0 as i64 };
    unsafe {
        clock_gettime(CLOCK_MONOTONIC, &mut ts);
    }
    return ts.tv_sec * 1000000000 + ts.tv_nsec;
}

/// Wall-clock nanoseconds since epoch (CLOCK_REALTIME matches the epoch-ns
/// mtimes stored by ingest, so lag arithmetic is direct).
pub fn realtime_ns() -> i64 {
    let mut ts = timespec { tv_sec: 0 as i64, tv_nsec: 0 as i64 };
    unsafe {
        clock_gettime(CLOCK_REALTIME, &mut ts);
    }
    return ts.tv_sec * 1000000000 + ts.tv_nsec;
}

#[cfg(test)]
mod skip_kind_render_tests {
    use super::*;

    // V1.3 phase E4a: the three FLAT structural-skip counters must surface on
    // /metrics from the LIVE mirror (max-merge pattern of stat_collisions),
    // each under its own name — no label machinery.
    #[test]
    fn skip_kind_counters_render_from_the_live_mirror() {
        let lv = Arc::new(LiveCounters::new_live());
        lv.bump_skip_kinds(2, 5, 1);
        let st = new_stats_with(lv);
        let body = st.render_prometheus();
        assert!(body.find("wxpoint_skip_coordlists_total 2").is_some());
        assert!(body.find("wxpoint_skip_grid_total 5").is_some());
        assert!(body.find("wxpoint_skip_pdt_total 1").is_some());
        // and the pre-existing total remains independent of them (partition)
        assert!(body.find("wxpoint_skipped_messages_total ").is_some());
    }
}
