// V1.1 phase 3 regression: manifest.json carries PDT 4.8 statistical time
// ranges as absolute ISO-8601 UTC instants (spec §4.2), so `store.iso_utc`
// must be exact. The closed-form Hinnant inverse that shipped earlier had a
// wrong era divisor (z/400 instead of z/146097) and produced garbage years
// ("555635-0-6607885-0-26T12:30:00Z"). These cases pin leap days, the non-leap
// century year 2100, and both ends of the supported range; every expectation
// was cross-checked against Python's datetime.

use wx_ingest::store::{iso_utc, parse_iso_utc};

#[test]
fn iso_utc_matches_known_instants() {
    // (epoch seconds, canonical UTC string)
    let tab_ep: [i64; 8] = [0, 1792413000, 951321600, 951825600,
                            4107542400, 1709251199, 946684799, 1798761599];
    let tab_txt: [String; 8] = ["1970-01-01T00:00:00Z".to_string(),
                                "2026-10-19T12:30:00Z".to_string(),
                                "2000-02-23T16:00:00Z".to_string(),
                                "2000-02-29T12:00:00Z".to_string(),
                                "2100-03-01T00:00:00Z".to_string(),
                                "2024-02-29T23:59:59Z".to_string(),
                                "1999-12-31T23:59:59Z".to_string(),
                                "2026-12-31T23:59:59Z".to_string()];
    let mut k: i64 = 0;
    while k < 8 {
        let got = iso_utc(tab_ep[k as usize]);
        if got != tab_txt[k as usize] {
            println!("iso_utc({0}) = {1}, expected {2}", tab_ep[k as usize], got, tab_txt[k as usize]);
            assert!(false);
        }
        k += 1;
    }
}

#[test]
fn iso_utc_parse_round_trips_across_two_centuries() {
    // every day from 1970-01-01 to 2104-12-31 (leap-century boundary inclusive)
    // must render and parse back to the same instant at noon.
    let mut day: i64 = 0;
    let last_day: i64 = 49309; // 2104-12-31
    let mut bad = 0;
    while day <= last_day {
        let ep = day * 86400 + 43200;
        let s = iso_utc(ep);
        match parse_iso_utc(&s) {
            None => {
                println!("unparsable {0} -> {1}", ep, s);
                bad += 1;
            }
            Some(back) => {
                if back != ep {
                    println!("drift {0} -> {1} -> {2}", ep, s, back);
                    bad += 1;
                }
            }
        }
        day += 1;
    }
    assert!(bad == 0);
    // negative epochs are refused, not rendered
    assert!(iso_utc(-1) == "".to_string());
    // strict parser rejects deviations from the canonical form
    assert!(parse_iso_utc("2026-10-19 12:30:00Z").is_none());
    assert!(parse_iso_utc("2026-10-19T12:30:00").is_none());
    assert!(parse_iso_utc("2026-10-19T12:30:00Z ").is_none());
    assert!(parse_iso_utc("2026-13-19T12:30:00Z").is_none()); // month 13 rejected
}
