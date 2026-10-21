// V1.2 phase C gate #10: STRICT CALENDAR DATES IN THE MANIFEST PARSER TOO.
//
// The audit found the date-range check in `store.parse_iso_utc` (used when a
// statistically processed field's TIME RANGE is read back out of manifest.json)
// accepted ANY day 1..31, so "2026-02-31" silently NORMALIZED into early MARCH
// through the Hinnant day formula. A hand-edited or upstream-corrupt manifest
// therefore shifted a statistical range by DAYS instead of being REJECTED. The
// same table-driven rule as the HTTP parser now applies (shared helpers in
// wx-core::tables), including the leap-year rule.

use wx_ingest::store::{parse_iso_utc};

#[test]
fn february_29_is_accepted_only_in_a_real_leap_year() {
    match parse_iso_utc("2024-02-29T00:00:00Z") {
        None => { assert!(false); return; }
        Some(ep) => { assert!(ep > 0); }
    }
    match parse_iso_utc("2000-02-29T00:00:00Z") {
        None => { assert!(false); return; }
        Some(ep) => { assert!(ep > 0); } // century leap year
    }
    // 2026 is NOT a leap year: Feb 29 must be REJECTED, not rolled over
    assert!(parse_iso_utc("2026-02-29T00:00:00Z") == None);
    // 2100 is divisible by 4 but NOT a leap year (century rule)
    assert!(parse_iso_utc("2100-02-29T00:00:00Z") == None);
}

#[test]
fn impossible_days_are_rejected_instead_of_rolling_over_into_the_next_month() {
    assert!(parse_iso_utc("2026-02-30T00:00:00Z") == None);
    assert!(parse_iso_utc("2026-02-31T00:00:00Z") == None);
    assert!(parse_iso_utc("2026-04-31T00:00:00Z") == None); // April has 30 days
    assert!(parse_iso_utc("2026-06-31T00:00:00Z") == None);
    assert!(parse_iso_utc("2026-09-31T00:00:00Z") == None);
    assert!(parse_iso_utc("2026-11-31T00:00:00Z") == None);
    assert!(parse_iso_utc("2026-01-00T00:00:00Z") == None); // day 0
    assert!(parse_iso_utc("2026-00-10T00:00:00Z") == None); // month 0
    assert!(parse_iso_utc("2026-13-01T00:00:00Z") == None); // month 13
    // the LAST valid day of every 31-day month still parses
    match parse_iso_utc("2026-12-31T23:59:59Z") {
        None => { assert!(false); return; }
        Some(ep) => { assert!(ep > 0); }
    }
    match parse_iso_utc("2026-03-31T00:00:00Z") {
        None => { assert!(false); return; }
        Some(ep2) => { assert!(ep2 > 0); }
    }
    // and the ROLLOVER DISTANCE is gone: Feb 28 -> Mar 1 is exactly 86400 s apart,
    // while Feb 29/30/31 simply do not exist.
    let feb28 = match parse_iso_utc("2026-02-28T00:00:00Z") {
        None => { assert!(false); return; }
        Some(e) => { e }
    };
    let mar1 = match parse_iso_utc("2026-03-01T00:00:00Z") {
        None => { assert!(false); return; }
        Some(e) => { e }
    };
    assert!(mar1 - feb28 == 86400);
}
