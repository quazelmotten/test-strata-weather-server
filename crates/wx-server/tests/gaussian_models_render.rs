// V1.3 phase E4b: /v1/models grid-block rendering for the new
// "gaussian_latlon" kind (spec §7 model info). render_grid_json is exercised
// directly so the JSON shape is pinned without needing a synthesized
// template-3.40 GRIB fixture; the HTTP-level tests cover the surrounding
// envelope and keep the regular/lcc pins honest.

use wx_core::grid::{GridDef, GridGaussian};
use wx_server::httpapi::render_grid_json;

fn has(hay: &str, needle: &str) -> bool {
    return hay.find(needle).is_some();
}

#[test]
fn models_grid_block_renders_gaussian_kind() {
    let g = GridDef::Gaussian(GridGaussian::new(
        3072, 1536, 0.0, 89.910324, -89.910324, 360.0 / 3072.0, 0x00, 6, false));
    let j = render_grid_json(&g);
    assert!(has(&j, "\"kind\":\"gaussian_latlon\""));
    // every field the task lists must be present with sane values
    assert!(has(&j, "\"nx\":3072"));
    assert!(has(&j, "\"ny\":1536"));
    assert!(has(&j, "\"lon1\":0"));
    assert!(has(&j, "\"la1\":89.910324"));
    assert!(has(&j, "\"la2\":-89.910324"));
    assert!(has(&j, "\"dlon\":0.1171875"));
    assert!(has(&j, "\"scan_mode\":0"));
    assert!(has(&j, "\"earth_shape\":6"));
    assert!(has(&j, "\"source_winds_grid\":false"));
    // balanced braces sanity (same guard style as the meta_json regression)
    let mut opens = 0usize;
    let mut closes = 0usize;
    for c in j.chars() {
        if c == '{' { opens += 1; }
        if c == '}' { closes += 1; }
    }
    assert!(opens == 1 && closes == 1);
}

#[test]
fn models_grid_block_other_kinds_unchanged() {
    // additive-only change: the regular and lcc blocks must keep their exact
    // kind strings (pinned here at unit level too, mirroring point_http)
    use wx_core::grid::GridRegular;
    let r = GridDef::Regular(GridRegular {
        nx: 41, ny: 41, lon1: 230.0, lat1: 25.0, dlon: 0.05, dlat: 0.05,
        scan_mode: 0, winds_grid: false,
    });
    let jr = render_grid_json(&r);
    assert!(has(&jr, "\"kind\":\"regular\""));
    assert!(!has(&jr, "gaussian"));

    use wx_core::grid::GridLambert;
    let l = GridDef::Lambert(GridLambert::new(
        1799, 1059, 21.11389, 233.44, 265.0, 21.11389, 25.0, 25.0,
        3000.0, 3000.0, 1, 0, 2, true));
    let jl = render_grid_json(&l);
    assert!(has(&jl, "\"kind\":\"lcc\""));
    assert!(has(&jl, "\"latin1\":25"));
    assert!(!has(&jl, "gaussian"));
}
