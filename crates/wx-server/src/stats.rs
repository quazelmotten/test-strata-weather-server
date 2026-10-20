//! /metrics counters (spec §7): query latency histogram (manual µs buckets +
//! atomics, no heavy dependency), per-status-class query counts, ingest lag
//! and ingest error count. Latency is measured from request entry to response
//! start using CLOCK_MONOTONIC via libc (this toolchain has no std clock API).

use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use libc::timespec;
use libc::clock_gettime;
use libc::CLOCK_MONOTONIC;
use libc::CLOCK_REALTIME;
use std::sync::Arc;

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
    pub ingested_files: AtomicU64,
    pub last_mtime_ns: AtomicI64, // max mtime (epoch ns) over ingested files
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

    /// Mirror ingest-side counters (single writer: the ingest thread).
    pub fn set_ingest(self: &Self, errors: u64, files: u64, mtime_ns: i64) {
        self.ingest_errors.store(errors, Ordering::Relaxed);
        self.ingested_files.store(files, Ordering::Relaxed);
        self.last_mtime_ns.store(mtime_ns, Ordering::Relaxed);
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
        out = format!("{0}{1}", out, "\nwxpoint_ingest_errors_total ");
        out = format!("{0}{1}", out, self.ingest_errors.load(Ordering::Relaxed));
        out = format!("{0}{1}", out, "\nwxpoint_ingested_files_total ");
        out = format!("{0}{1}", out, self.ingested_files.load(Ordering::Relaxed));
        // ingest lag: file mtime -> now, seconds (mtime is epoch ns).
        let mtime = self.last_mtime_ns.load(Ordering::Relaxed);
        let mut lag = 0.0;
        if mtime > 0 {
            let now = realtime_ns();
            lag = ((now - mtime) as f64) / 1000000000.0;
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
/// ingest thread's counter mirror).
pub fn new_stats() -> Arc<QueryStats> {
    let mut buckets: Vec<AtomicU64> = Vec::<AtomicU64>::new();
    for _ in 0 .. BUCKET_US.len() {
        buckets.push(AtomicU64::new(0));
    }
    let qs = QueryStats {
        buckets: buckets,
        overflow: AtomicU64::new(0),
        qcount: AtomicU64::new(0),
        qsum_us: AtomicU64::new(0),
        c2xx: AtomicU64::new(0),
        c4xx: AtomicU64::new(0),
        c5xx: AtomicU64::new(0),
        ingest_errors: AtomicU64::new(0),
        ingested_files: AtomicU64::new(0),
        last_mtime_ns: AtomicI64::new(0),
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
