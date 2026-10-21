//! Parameter name -> GRIB2 (discipline, category, number) table and level
//! type names -> Code Table 4.5 values. Source of codes/units: wgrib2 NCEP
//! gribtable.dat (see NOTES-fixtures.md). Unknown names must fail loudly at
//! config validation time (spec §3): every lookup returns None for unknown
//! input and the caller decides how to die.

/// One configured parameter: its GRIB2 message identifier (sections 0/1
/// discipline, category, number) and native units (spec §7: report, never
/// convert).
#[derive(Clone, Debug)]
pub struct ParamEntry {
    pub name: &'static str,
    pub discipline: u8,
    pub category: u8,
    pub number: u8,
    pub units: &'static str,
}

/// Built-in parameter table (spec §3 minimum set). Codes verified against
/// tools/wgrib2-src/grib2/wgrib2/gribtables/ncep/gribtable.dat.
pub const PARAM_TABLE: [ParamEntry; 15] = [
    ParamEntry { name: "TMP",   discipline: 0, category: 0, number: 0,   units: "K" },
    ParamEntry { name: "HGT",   discipline: 0, category: 3, number: 5,   units: "gpm" },
    ParamEntry { name: "UGRD",  discipline: 0, category: 2, number: 2,   units: "m/s" },
    ParamEntry { name: "VGRD",  discipline: 0, category: 2, number: 3,   units: "m/s" },
    ParamEntry { name: "RH",    discipline: 0, category: 1, number: 1,   units: "%" },
    ParamEntry { name: "SPFH",  discipline: 0, category: 1, number: 0,   units: "kg/kg" },
    ParamEntry { name: "DPT",   discipline: 0, category: 0, number: 6,   units: "K" },
    ParamEntry { name: "PRMSL", discipline: 0, category: 3, number: 1,   units: "Pa" },
    ParamEntry { name: "MSLMA", discipline: 0, category: 3, number: 198, units: "Pa" },
    ParamEntry { name: "PRES",  discipline: 0, category: 3, number: 0,   units: "Pa" },
    ParamEntry { name: "VVEL",  discipline: 0, category: 2, number: 8,   units: "Pa/s" },
    ParamEntry { name: "APCP",  discipline: 0, category: 1, number: 8,   units: "kg/m^2" },
    ParamEntry { name: "GUST",  discipline: 0, category: 2, number: 22,  units: "m/s" },
    ParamEntry { name: "CAPE",  discipline: 7, category: 6, number: 0,   units: "J/kg" },
    ParamEntry { name: "CIN",   discipline: 7, category: 7, number: 0,   units: "J/kg" },
];

/// Level type name -> GRIB2 Code Table 4.5 value. Includes the `msl` alias
/// used in config/API strings (spec §3 example `msl`).
pub const LEVEL_TYPE_CODES: [(&str, u8); 8] = [
    ("surface", 1),
    ("isobaric_sigma", 9),
    ("isobaric", 100),
    ("above_mean_sea_level", 101),
    ("msl", 101),
    ("height_above_ground", 103),
    ("isothermal", 114),
    ("freezing_height", 200),
];

/// Canonical spelling per code, used when echoing a level back in API
/// responses (round-trips through parse_level_str).
pub const LEVEL_CANONICAL: [(&str, u8); 7] = [
    ("surface", 1),
    ("isobaric_sigma", 9),
    ("isobaric", 100),
    ("msl", 101),
    ("height_above_ground", 103),
    ("isothermal", 114),
    ("freezing_height", 200),
];

impl ParamEntry {
    /// "TMP d=0/c=0/n=0 K" — diagnostics for config errors.
    pub fn describe(self: &Self) -> String {
        let n = self.name;
        let d = self.discipline;
        let c = self.category;
        let nu = self.number;
        let u = self.units;
        return format!("{n} d={d}/c={c}/n={nu} {u}");
    }
}

/// Index of a configured parameter by name (None => unknown name).
pub fn lookup_param_index(name: &str) -> Option<usize> {
    for k in 0 .. PARAM_TABLE.len() {
        if PARAM_TABLE[k].name == name {
            return Some(k);
        }
    }
    return None;
}

/// GRIB2 (discipline, category, number) for a parameter name.
pub fn param_code(name: &str) -> Option<(u8, u8, u8)> {
    let idx = lookup_param_index(name);
    match idx {
        None => { return None; }
        Some(k) => {
            let e = &PARAM_TABLE[k];
            return Some((e.discipline, e.category, e.number));
        }
    }
}

/// Native units string for a parameter name.
pub fn param_units(name: &str) -> Option<&'static str> {
    let idx = lookup_param_index(name);
    match idx {
        None => { return None; }
        Some(k) => { return Some(PARAM_TABLE[k].units); }
    }
}

/// Reverse lookup: (discipline, category, number) -> canonical name. Used by
/// ingest to decide whether a GRIB message is in the configured list before
/// unpacking section 7 (spec §4.2).
pub fn param_name_for(disc: u8, cat: u8, num: u8) -> Option<&'static str> {
    for k in 0 .. PARAM_TABLE.len() {
        let e = &PARAM_TABLE[k];
        if e.discipline == disc && e.category == cat && e.number == num {
            return Some(e.name);
        }
    }
    return None;
}

/// Level type name -> CT 4.5 code (accepts aliases such as `msl`).
pub fn lookup_level_type(name: &str) -> Option<u8> {
    for k in 0 .. LEVEL_TYPE_CODES.len() {
        if LEVEL_TYPE_CODES[k].0 == name {
            return Some(LEVEL_TYPE_CODES[k].1);
        }
    }
    return None;
}

/// Canonical display name for a CT 4.5 code.
pub fn canonical_level_name(code: u8) -> Option<&'static str> {
    for k in 0 .. LEVEL_CANONICAL.len() {
        if LEVEL_CANONICAL[k].1 == code {
            return Some(LEVEL_CANONICAL[k].0);
        }
    }
    return None;
}

/// Parse an API/config level string: "isobaric:850", "height_above_ground:2",
/// "isobaric:837.5", "surface", "msl". Returns (CT 4.5 code, numeric level
/// value); bare names get value 0. Decimal values only (no exponent form —
/// levels never need one). None on any malformed input.
pub fn parse_level_str(s: &str) -> Option<(u8, f64)> {
    let n = s.len();
    let mut colon: Option<usize> = None;
    for k in 0 .. n {
        if s[k .. k + 1] == *":" {
            colon = Some(k);
        }
    }
    match colon {
        None => {
            let code = lookup_level_type(s);
            match code {
                None => { return None; }
                Some(c) => { return Some((c, 0.0)); }
            }
        }
        Some(k) => {
            let tname = &s[0 .. k];
            let vtxt = &s[k + 1 .. n];
            let code = lookup_level_type(tname);
            match code {
                None => { return None; }
                Some(c) => {
                    let v = parse_f64(vtxt);
                    match v {
                        None => { return None; }
                        Some(x) => { return Some((c, x)); }
                    }
                }
            }
        }
    }
}

/// Format a parsed level back to its canonical string (echoed in API
/// responses). surface prints bare.
pub fn format_level_str(code: u8, value: f64) -> String {
    let name = canonical_level_name(code);
    match name {
        None => { return "unknown".to_string(); }
        Some(nm) => {
            if code == 1 && value == 0.0 {
                return nm.to_string();
            }
            let numtxt = fmt_num(value);
            return format!("{nm}:{numtxt}");
        }
    }
}

/// Minimal exact decimal parser: optional sign, digits, optional '.',
/// digits. No exponent, no thousands separators, no empty sides of '.'.
pub fn parse_f64(s: &str) -> Option<f64> {
    let n = s.len();
    if n == 0 {
        return None;
    }
    let mut k = 0;
    let neg = s[0 .. 1] == *"-";
    if neg || s[0 .. 1] == *"+" {
        k = 1;
    }
    let mut acc = 0.0;
    let mut ndigits = 0;
    let mut frac_seen = false;
    let mut scale = 1.0;
    while k < n {
        if s[k .. k + 1] == *"." {
            if frac_seen || ndigits == 0 {
                return None; // ".5" and "5..1" rejected; "5." also rejected
            }
            frac_seen = true;
        } else {
            let d = digit_at(s, k);
            match d {
                None => { return None; }
                Some(dv0) => {
                    let dv = dv0 as f64;
                    if !frac_seen {
                        acc = acc * 10.0 + dv;
                    } else {
                        scale *= 10.0;
                        acc += dv / scale;
                    }
                    ndigits += 1;
                }
            }
        }
        k += 1;
    }
    if ndigits == 0 {
        return None;
    }
    if neg {
        return Some(-acc);
    }
    return Some(acc);
}

/// Exact decimal formatter for level coordinates: integers print without a
/// fraction; non-integers print up to 6 fraction digits with trailing zeros
/// trimmed (enough for hPa/m coordinates in practice; round-trips through
/// parse_f64 for all values produced by format_num).
pub fn fmt_num(value: f64) -> String {
    let neg = value < 0.0;
    let mut v = value;
    if neg {
        v = -value;
    }
    let ipart = v.trunc();
    let ip = ipart as i64;
    let mut out = format!("{ip}");
    if neg {
        out = format!("-{ip}");
    }
    let frac = v - ipart;
    if frac == 0.0 {
        return out;
    }
    // decimal digits of the fraction, up to 6, trimmed
    let mut fr = frac;
    let mut digits: Vec<u8> = Vec::<u8>::new();
    let mut k = 0;
    while k < 6 {
        fr *= 10.0;
        let d = fr.trunc() as i64;
        digits.push(d as u8);
        fr -= d as f64;
        k += 1;
    }
    // trim trailing zero digits
    let mut m = digits.len();
    while m > 0 && digits[m - 1] == 0 {
        m -= 1;
    }
    out = format!("{out}.");
    for t in 0 .. m {
        let dv = digits[t];
        let dtxt = format!("{dv}");
        out = format!("{out}{dtxt}");
    }
    return out;
}

/// Digit value of the byte at index k of s, or None (non-digit / OOB).
pub fn digit_at(s: &str, k: usize) -> Option<i64> {
    if k >= s.len() {
        return None;
    }
    if s[k .. k + 1] == *"0" {
        return Some(0);
    }
    if s[k .. k + 1] == *"1" {
        return Some(1);
    }
    if s[k .. k + 1] == *"2" {
        return Some(2);
    }
    if s[k .. k + 1] == *"3" {
        return Some(3);
    }
    if s[k .. k + 1] == *"4" {
        return Some(4);
    }
    if s[k .. k + 1] == *"5" {
        return Some(5);
    }
    if s[k .. k + 1] == *"6" {
        return Some(6);
    }
    if s[k .. k + 1] == *"7" {
        return Some(7);
    }
    if s[k .. k + 1] == *"8" {
        return Some(8);
    }
    if s[k .. k + 1] == *"9" {
        return Some(9);
    }
    return None;
}

// ---------------------------------------------------------------------------
// Calendar validation (V1.2 phase C): ONE table-driven rule shared by every
// ISO-8601 reader in the workspace (wx-server `timeparse::parse_iso_z` for the
// HTTP query parameters, wx-ingest `store::parse_iso_utc` for manifest
// statistical ranges). Both previously accepted any day 1..31 for every month,
// so "2026-02-31" silently NORMALIZED into March instead of being rejected.
// ---------------------------------------------------------------------------

/// Days in each month (index 1..12) of a NON-leap year; index 0 unused so the
/// table reads like a calendar.
pub const DAYS_IN_MONTH: [i64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// Proleptic Gregorian leap year — the SAME calendar the Hinnant civil-days
/// math in `epoch_secs`/`iso_utc` implements, so the two agree by construction.
pub fn is_leap_year(y: i64) -> bool {
    return y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
}

/// Days in month `m` (1..12) of year `y`, leap-corrected; 0 for an out-of-range
/// month (callers compare `d <= days_in_month(...)` after checking the month).
pub fn days_in_month(y: i64, m: i64) -> i64 {
    if !(1 ..= 12).contains(&m) {
        return 0;
    }
    let base = DAYS_IN_MONTH[m as usize];
    if m == 2 && is_leap_year(y) {
        return base + 1;
    }
    return base;
}

/// True when (y, m, d) is a REAL calendar date: February 29 only in a leap year
/// (2024 yes, 2026 no), February 30/31 NEVER, April 31 NEVER, December 31 ALWAYS.
pub fn valid_civil_date(y: i64, m: i64, d: i64) -> bool {
    if !(1 ..= 12).contains(&m) {
        return false;
    }
    if d < 1 {
        return false;
    }
    return d <= days_in_month(y, m);
}
