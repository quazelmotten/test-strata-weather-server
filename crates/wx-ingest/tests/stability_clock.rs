// V1.2 phase B0: the file-stability WINDOW is measured on a MONOTONIC clock
// (`discovery::MonoClock`, backed by `std::time::Instant`), NEVER on wall-clock
// file mtimes. The acceptance gate flakiness came from exactly that coupling:
// `scan_round` used to take `now_ns_marker()` (the MTIME of a marker FILE), so a
// backward or non-advancing timestamp made `now_ns - first_observed` NEGATIVE and
// a file whose stat had been stable for the whole window stayed INELIGIBLE
// FOREVER (a scan round then silently ingested NOTHING). mtime stays IDENTITY-ONLY
// here: a changed (size,mtime) means a re-delivery and RESETS the window.

use wx_ingest::discovery::{MonoClock, StabilityTracker};
use std::thread;
use std::time::Duration;

pub fn sleep_ms(ms: i64) {
    let d = Duration::from_millis(ms as u64);
    thread::sleep(d);
}

#[test]
fn mono_clock_never_decreases_and_tracks_real_time() {
    let c = MonoClock::new_clock();
    let a = c.elapsed_ns();
    // monotone over many rapid reads
    let mut prev = a;
    let mut k = 0usize;
    while k < 200 {
        let now = c.elapsed_ns();
        assert!(now >= prev);
        prev = now;
        k += 1;
    }
    sleep_ms(60);
    let b = c.elapsed_ns();
    assert!(b - a >= 50000000); // >= 50 ms really elapsed
    // a fresh clock starts near zero and is independent of wall-clock epoch
    let c2 = MonoClock::new_clock();
    assert!(c2.elapsed_ns() < 1000000000);
}

#[test]
fn window_eligibility_is_measured_monotonically() {
    let c = MonoClock::new_clock();
    let mut tr = StabilityTracker { obs: Vec::<(String, u64, i64, i64)>::new() };
    let win = 300000000i64; // 300 ms window

    // first observation: never eligible
    let t0 = c.elapsed_ns();
    assert!(!tr.observe("f.grib2".to_string(), 100u64, 4242i64, t0, win));
    // still inside the window: not eligible
    let t1 = c.elapsed_ns();
    assert!(!tr.observe("f.grib2".to_string(), 100u64, 4242i64, t1, win));
    // after the window has really elapsed: eligible
    sleep_ms(360);
    let t2 = c.elapsed_ns();
    assert!(tr.observe("f.grib2".to_string(), 100u64, 4242i64, t2, win));
}

#[test]
fn backward_wall_clock_cannot_freeze_the_window_anymore() {
    // Simulate the pre-fix stamp source: a clock that JUMPS BACKWARD (NTP step-
    // back, container restore, or `now_ns_marker` returning 0 when its marker
    // file cannot be created). Under the OLD rule the window could never elapse;
    // under the monotonic rule the elapsed measure comes from real time only.
    let c = MonoClock::new_clock();
    let mut tr = StabilityTracker { obs: Vec::<(String, u64, i64, i64)>::new() };
    let win = 1000000000i64; // 1 s

    let first = c.elapsed_ns();
    assert!(!tr.observe("g.grib2".to_string(), 7u64, 99i64, first, win));

    // A backward jump of the STAMP SOURCE would have made the old predicate
    // `(backward_stamp - first) >= win` permanently false:
    let backward_stamp = first - 5000000000i64; // 5 s "in the past"
    assert!((backward_stamp - first) < win); // the old rule: NOT eligible, forever

    // The tracker itself only ever receives MONOTONIC stamps from Pipeline, so
    // real elapsed time decides eligibility regardless of any wall-clock weirdness.
    sleep_ms(1100);
    let later = c.elapsed_ns();
    assert!(later - first >= win);
    assert!(tr.observe("g.grib2".to_string(), 7u64, 99i64, later, win));
}

#[test]
fn identity_change_resets_the_monotonic_window() {
    let c = MonoClock::new_clock();
    let mut tr = StabilityTracker { obs: Vec::<(String, u64, i64, i64)>::new() };
    let win = 200000000i64; // 200 ms

    let t0 = c.elapsed_ns();
    assert!(!tr.observe("h.grib2".to_string(), 10u64, 1i64, t0, win));
    sleep_ms(260);
    // SAME identity -> eligible... but a re-delivery arrives FIRST: size changes,
    // which must RESET the stamp so the new identity needs the FULL window again.
    let t1 = c.elapsed_ns();
    assert!(!tr.observe("h.grib2".to_string(), 11u64, 1i64, t1, win)); // size changed
    let t2 = c.elapsed_ns();
    assert!(!tr.observe("h.grib2".to_string(), 11u64, 1i64, t2, win)); // inside window
    sleep_ms(260);
    let t3 = c.elapsed_ns();
    assert!(tr.observe("h.grib2".to_string(), 11u64, 1i64, t3, win));

    // mtime-only change also resets (identity is path+size+mtime)
    let t4 = c.elapsed_ns();
    assert!(!tr.observe("h.grib2".to_string(), 11u64, 2i64, t4, win));
    sleep_ms(260);
    let t5 = c.elapsed_ns();
    assert!(tr.observe("h.grib2".to_string(), 11u64, 2i64, t5, win));
}
