//! ISO-8601 UTC ("YYYY-MM-DDTHH:MM:SSZ") <-> epoch seconds. Strict grammar:
//! exactly the shape shown in spec §7 (no fractional seconds, trailing Z
//! required). Epoch math mirrors wx_ingest.grib2.epoch_secs (Hinnant
//! days_from_civil), and format_iso_z is its exact inverse (round-trip tested
//! across leap years/month boundaries in tests).

use wx_ingest::grib2::epoch_secs;
use wx_ingest::pipeline::pad_num;
use wx_core::tables::digit_at;

/// Parse "YYYY-MM-DDTHH:MM:SSZ" -> epoch seconds. None on any deviation
/// (wrong length, non-digits, bad separators, out-of-range components).
pub fn parse_iso_z(s: &str) -> Option<i64> {
    if s.len() != 20 {
        return None;
    }
    if s[4 .. 5] != *"-" || s[7 .. 8] != *"-" || s[10 .. 11] != *"T" || s[13 .. 14] != *":" || s[16 .. 17] != *":" || s[19 .. 20] != *"Z" {
        return None;
    }
    let yv = num_at(&s, 0, 4);
    let mo = num_at(&s, 5, 2);
    let da = num_at(&s, 8, 2);
    let hr = num_at(&s, 11, 2);
    let mi = num_at(&s, 14, 2);
    let se = num_at(&s, 17, 2);
    match yv {
        None => { return None; }
        Some(y) => {
            match mo {
                None => { return None; }
                Some(m) => {
                    match da {
                        None => { return None; }
                        Some(d) => {
                            match hr {
                                None => { return None; }
                                Some(h) => {
                                    match mi {
                                        None => { return None; }
                                        Some(mn) => {
                                            match se {
                                                None => { return None; }
                                                Some(sc) => {
                                                    if m < 1 || m > 12 || d < 1 || d > 31 || h > 23 || mn > 59 || sc > 59 {
                                                        return None;
                                                    }
                                                    return Some(epoch_secs(y as u16, m as u8, d as u8, h as u8, mn as u8, sc as u8));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Digits [off, off+len) as i64, None if any position is not a digit.
fn num_at(s: &str, off: usize, len: usize) -> Option<i64> {
    let mut v: i64 = 0;
    for k in 0 .. len {
        let d = digit_at(&s, off + k);
        match d {
            None => { return None; }
            Some(dv) => { v = v * 10 + dv; }
        }
    }
    return Some(v);
}

/// Inverse of epoch_secs / grib2 epoch math (Hinnant civil_from_days). All
/// epochs we handle are post-1970; negative epochs floor-divide correctly
/// through the era shift below.
pub fn format_iso_z(ep: i64) -> String {
    let secs_day: i64 = 86400;
    let mut se = ep / secs_day;
    if ep % secs_day < 0 {
        se -= 1;
    }
    let rem = ep - se * secs_day;
    let hr = rem / 3600;
    let mi = (rem % 3600) / 60;
    let sc = rem % 60;

    let z = se + 719468; // days since 1970 -> era space (inverse of epoch_secs)
    let mut era = z / 146097;
    if z % 146097 < 0 {
        era -= 1;
    }
    let doe = z - era * 146097; // [0, 146096]
    let mut yoe = doe / 365;
    loop {
        let base = 365 * yoe + yoe / 4 - yoe / 100;
        if doe < base {
            yoe -= 1;
        } else if doe > base + 365 {
            yoe += 1;
        } else {
            break;
        }
    }
    if yoe > 399 {
        yoe = 399; // last era day (Feb 29 of the century-leap year)
    }
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let yy = y + if m <= 2 { 1 } else { 0 };

    let ys = pad_num(yy, 4);
    let mos = pad_num(m, 2);
    let ds = pad_num(d, 2);
    let hrs = pad_num(hr, 2);
    let mis = pad_num(mi, 2);
    let scs = pad_num(sc, 2);
    return format!("{0}-{1}-{2}T{3}:{4}:{5}Z", ys, mos, ds, hrs, mis, scs);
}
