// V1.3 phase E4c: /v1/models configured-level rendering for HYBRID specs.
// The models endpoint prints every parsed level spec through
// render_level_spec, so pinning that function pins the wire shape: wildcard
// "hybrid:*", decimals "hybrid:837.5", integers "hybrid:1". Additive-only:
// the pre-existing spellings for every other level type must not move.
// Unit-level harness per the E4b render_grid_json precedent (no HTTP needed
// for the string itself; point_http keeps the envelope honest).

use wx_server::httpapi::render_level_spec;

#[allow(clippy::needless_return)]
fn has(hay: &str, needle: &str) -> bool {
    return hay.find(needle).is_some();
}

#[test]
fn models_levels_block_renders_hybrid_specs() {
    // wildcard: every hybrid level present (spec §3 "isobaric:*" analogue)
    assert!(render_level_spec(105, None) == "hybrid:*");
    // concrete coordinates print RAW: dimensionless coordinate, no unit math
    assert!(render_level_spec(105, Some(1.0)) == "hybrid:1");
    assert!(render_level_spec(105, Some(837.5)) == "hybrid:837.5");
    assert!(render_level_spec(105, Some(255.0)) == "hybrid:255");
    // bare-name special case stays EXCLUSIVE to surface (code 1): hybrid:0
    // must print WITH its value so config round-trips unambiguously
    assert!(render_level_spec(1, Some(0.0)) == "surface");
    assert!(render_level_spec(105, Some(0.0)) == "hybrid:0");
    // wildcard-on-surface is NOT collapsed to the bare spelling: the bare form
    // belongs to the SPECIFIC value-0 spec, "*" stays explicit (pre-E4c rule)
    assert!(render_level_spec(1, None) == "surface:*");
}

#[test]
fn models_levels_block_other_kinds_unchanged() {
    // additive-only guard: the strings every existing spec renders to
    assert!(render_level_spec(255, None) == "*");
    assert!(render_level_spec(100, None) == "isobaric:*");
    assert!(render_level_spec(100, Some(850.0)) == "isobaric:850");
    assert!(render_level_spec(103, Some(2.0)) == "height_above_ground:2");
    // and the hybrid addition must not have leaked into any of them
    assert!(!has(&render_level_spec(100, Some(850.0)), "hybrid"));
    assert!(!has(&render_level_spec(103, Some(2.0)), "hybrid"));
}
