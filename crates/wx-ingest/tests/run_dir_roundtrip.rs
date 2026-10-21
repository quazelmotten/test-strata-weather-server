// V1.2 phase C gate #9: RUN-DIRECTORY NAME ROUND TRIP (spec §4.5 storage keys).
//
// `run_dir_name` used to append `mm` ONLY when the MINUTE was non-zero, so a run
// at 12:00:30 was named `2026101912` and its SECONDS silently became 12:00:00 on
// the next warm boot — shifting EVERY forecast-hour offset of that run by 30 s.
// The name is now a TOTAL, INVERTIBLE function of the run time: base `YYYYMMDDHH`
// (10), `+mm` whenever minute OR second is non-zero (12), `+ss` whenever the second
// is non-zero (14), and `parse_run_dir` recovers the exact instant from the name.

use wx_ingest::pipeline::{run_dir_name, parse_run_dir};
use wx_ingest::grib2::{RunTime, epoch_secs};

#[allow(clippy::needless_return)]
fn rt(y: i64, mo: i64, d: i64, h: i64, mi: i64, se: i64) -> RunTime {
    return RunTime {
        year: y as u16,
        month: mo as u8,
        day: d as u8,
        hour: h as u8,
        minute: mi as u8,
        second: se as u8,
        epoch_secs: epoch_secs(y as u16, mo as u8, d as u8, h as u8, mi as u8, se as u8),
    };
}

#[test]
fn every_minute_and_second_instant_round_trips_through_its_directory_name() {
    // (year, month, day) incl. a leap day, a month boundary and a year boundary
    let days: Vec<(i64, i64, i64)> = [
        (2026, 10, 19),
        (2024, 2, 29),   // leap day
        (2000, 2, 29),   // century leap
        (2026, 12, 31),  // year end
        (2027, 1, 1),    // year start
        (1970, 1, 1),    // epoch
    ].to_vec();
    let hours: Vec<i64> = [0, 1, 12, 23].to_vec();
    let minutes: Vec<i64> = [0, 1, 30, 59].to_vec();
    let seconds: Vec<i64> = [0, 1, 15, 30, 59].to_vec();

    let mut names: Vec<String> = Vec::<String>::new();
    for dd in &days {
        for hh in &hours {
            for mm in &minutes {
                for ss in &seconds {
                    let t = rt(dd.0, dd.1, dd.2, *hh, *mm, *ss);
                    let want_ep = t.epoch_secs;
                    let n = run_dir_name(t);
                    // canonical lengths only: 10 / 12 / 14
                    assert!(n.len() == 10 || n.len() == 12 || n.len() == 14);
                    match parse_run_dir(n.as_str()) {
                        None => {
                            assert!(false);
                            return;
                        }
                        Some(ep) => {
                            // EXACT INSTANT RECOVERED, no silent truncation
                            assert!(ep == want_ep);
                        }
                    }
                    names.push(n.to_string());
                }
            }
        }
    }
    // NO COLLISIONS: two runs differing only in minutes or seconds can never
    // share one directory (which is what made the old rule lossy).
    let total = days.len() * hours.len() * minutes.len() * seconds.len();
    assert!(names.len() == total);
    let mut sorted: Vec<String> = Vec::<String>::new();
    for n in &names {
        sorted.push(n.to_string());
    }
    sorted.sort();
    let mut k = 1;
    while k < sorted.len() {
        assert!(sorted[k] != sorted[k - 1]);
        k += 1;
    }
}

#[test]
fn seconds_survive_a_boot_and_the_hour_only_name_stays_back_compatible() {
    // pre-fix this was "2026101912": the 30 s silently vanished. Now 14 chars.
    assert!(run_dir_name(rt(2026, 10, 19, 12, 0, 30)) == "20261019120030".to_string());
    assert!(run_dir_name(rt(2026, 10, 19, 12, 30, 15)) == "20261019123015".to_string());
    assert!(run_dir_name(rt(2026, 10, 19, 12, 30, 0)) == "202610191230".to_string());
    // UNCHANGED for whole hours: every pre-V1.2 fixture keeps its name
    assert!(run_dir_name(rt(2026, 10, 19, 12, 0, 0)) == "2026101912".to_string());

    // the two are DISTINCT directories...
    let na = run_dir_name(rt(2026, 10, 19, 12, 0, 0));
    let nb = run_dir_name(rt(2026, 10, 19, 12, 0, 30));
    assert!(na != nb);
    let base_ep = epoch_secs(2026, 10, 19, 12, 0, 0);
    match parse_run_dir(nb.as_str()) {
        None => { assert!(false); return; }
        Some(ep) => { assert!(ep == base_ep + 30); }
    }
    // a legacy 10-char directory name still means minute=second=0
    match parse_run_dir(na.as_str()) {
        None => { assert!(false); return; }
        Some(ep2) => { assert!(ep2 == base_ep); }
    }
    // non-names are rejected, not guessed
    assert!(not_a_run_dir("202610191".to_string()));
    assert!(not_a_run_dir("20261019123".to_string()));
    assert!(not_a_run_dir("gfs.t12z".to_string()));
    assert!(not_a_run_dir("2026131912".to_string())); // month 13
}

#[allow(clippy::needless_return)]
fn not_a_run_dir(name: String) -> bool {
    match parse_run_dir(name.as_str()) {
        None => { return true; }
        Some(_) => { return false; }
    }
}
