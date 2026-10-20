// GRIB table + level-string tests (spec §3): every built-in name resolves,
// unknown names fail loudly (None), CT 4.5 codes match, level strings
// round-trip through parse/format.

use wx_core::tables::lookup_param_index;
use wx_core::tables::param_code;
use wx_core::tables::param_units;
use wx_core::tables::param_name_for;
use wx_core::tables::lookup_level_type;
use wx_core::tables::canonical_level_name;
use wx_core::tables::parse_level_str;
use wx_core::tables::format_level_str;

#[test]
fn param_table_codes() {
    assert!(param_code("TMP") == Some((0, 0, 0)));
    assert!(param_code("HGT") == Some((0, 3, 5)));
    assert!(param_code("UGRD") == Some((0, 2, 2)));
    assert!(param_code("VGRD") == Some((0, 2, 3)));
    assert!(param_code("RH") == Some((0, 1, 1)));
    assert!(param_code("SPFH") == Some((0, 1, 0)));
    assert!(param_code("DPT") == Some((0, 0, 6)));
    assert!(param_code("PRMSL") == Some((0, 3, 1)));
    assert!(param_code("MSLMA") == Some((0, 3, 198)));
    assert!(param_code("PRES") == Some((0, 3, 0)));
    assert!(param_code("VVEL") == Some((0, 2, 8)));
    assert!(param_code("APCP") == Some((0, 1, 8)));
    assert!(param_code("GUST") == Some((0, 2, 22)));
    assert!(param_code("CAPE") == Some((7, 6, 0)));
    assert!(param_code("CIN") == Some((7, 7, 0)));
    // unknown names -> None (config validation dies on these)
    assert!(param_code("BOGUS").is_none());
    assert!(lookup_param_index("").is_none());
}

#[test]
fn units_and_reverse_lookup() {
    assert!(param_units("TMP") == Some("K"));
    assert!(param_units("HGT") == Some("gpm"));
    assert!(param_units("APCP") == Some("kg/m^2"));
    assert!(param_units("VVEL") == Some("Pa/s"));
    assert!(param_name_for(0, 0, 0) == Some("TMP"));
    assert!(param_name_for(0, 3, 198) == Some("MSLMA"));
    assert!(param_name_for(0, 2, 3) == Some("VGRD"));
    assert!(param_name_for(9, 9, 9).is_none());
}

#[test]
fn level_type_ct45() {
    assert!(lookup_level_type("surface") == Some(1));
    assert!(lookup_level_type("isobaric_sigma") == Some(9));
    assert!(lookup_level_type("isobaric") == Some(100));
    assert!(lookup_level_type("above_mean_sea_level") == Some(101));
    assert!(lookup_level_type("msl") == Some(101));
    assert!(lookup_level_type("height_above_ground") == Some(103));
    assert!(lookup_level_type("isothermal") == Some(114));
    assert!(lookup_level_type("freezing_height") == Some(200));
    assert!(lookup_level_type("isobars").is_none());
    assert!(canonical_level_name(101) == Some("msl"));
    assert!(canonical_level_name(42).is_none());
}

#[test]
fn level_string_parse_format() {
    assert!(parse_level_str("isobaric:850") == Some((100, 850.0)));
    assert!(parse_level_str("isobaric:837.5") == Some((100, 837.5)));
    assert!(parse_level_str("height_above_ground:2") == Some((103, 2.0)));
    assert!(parse_level_str("surface") == Some((1, 0.0)));
    assert!(parse_level_str("msl") == Some((101, 0.0)));
    assert!(parse_level_str("isobaric:1013.25") == Some((100, 1013.25)));
    // malformed -> None
    assert!(parse_level_str("bogus:10").is_none());
    assert!(parse_level_str("isobaric:").is_none());
    assert!(parse_level_str(":5").is_none());
    assert!(parse_level_str("isobaric:1.2.3").is_none());
    assert!(parse_level_str("isobaric:-x").is_none());
    // round-trip through the canonical formatter
    let cases: [[f64; 1]; 4] = [[850.0], [837.5], [2.0], [1013.25]];
    for c in cases {
        let s1 = format_level_str(100, c[0]);
        let back = parse_level_str(&s1);
        match back {
            None => { assert!(false); }
            Some((code, v)) => {
                assert!(code == 100);
                let mut d = v - c[0];
                if d < 0.0 {
                    d = -d;
                }
                assert!(d < 1e-6);
            }
        }
    }
    assert!(format_level_str(100, 850.0) == "isobaric:850");
    assert!(format_level_str(103, 2.0) == "height_above_ground:2");
    assert!(format_level_str(1, 0.0) == "surface");
    assert!(format_level_str(100, 837.5) == "isobaric:837.5");
    assert!(format_level_str(101, 0.0) == "msl:0");
    assert!(format_level_str(100, -12.5) == "isobaric:-12.5");
}
