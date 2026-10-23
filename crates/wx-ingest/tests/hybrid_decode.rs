// V1.3 phase E4c: HYBRID level type (GRIB2 Code Table 4.5 = 105) end to end.
//
// Fixture tests/fixtures/hybrid_small.grib2 (scripts/gen_grib2.py): TMP on
// HYBRID coordinates 1/50/255 at fh 0 AND 6 plus TMP on ISOBARIC 850/500 hPa
// at fh 0, one tiny regular lat/lon grid (nx=41 ny=21, lon1 350 dlon 0.5,
// lat1 10 dlat -0.5, N->S scan), run 2026-10-19 12Z. Plane values are
// ANALYTIC and linear in the hybrid coordinate:
//     v(L, fh, j, i) = 250 + 8*L + 10*fh + 32*j + i
// isobaric planes live in a DISJOINT band:
//     v(P_hPa, j, i) = 250 + 100000 + P + 32*j + i
// so exact hits, bracket interpolation, out-of-range rejection and stack
// isolation all have EXACTLY-known answers (ground truth recorded in
// expected.json via wgrib2: node (5.0N, 355.0E) -> 588/648/980/1040/2620/
// 2680/101430/101080). Level VALUES are stored RAW and dimensionless — the
// contrast with isobaric's deliberate Pa->hPa divide is itself pinned here.

use wx_core::interp::{TimeInterpMode};
use wx_ingest::config::{parse_level_spec, validate_text};
use wx_ingest::discovery::{path_to_str};
use wx_ingest::grib2::{decode_file, ParamFilter};
use wx_ingest::index::{build_context, evaluate_point, SelectError};
use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::join_path;
use memmap2::Mmap;
use std::fs::{create_dir_all, File, read_dir, remove_dir, remove_file};
use std::io::Write;

#[allow(clippy::needless_return)]
fn rm_tree(dir: &str) {
    match read_dir(&dir) {
        Err(_) => {}
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        let p = path_to_str(&de.path());
                        match read_dir(&p) {
                            Err(_) => {
                                let _r = remove_file(&p);
                            }
                            Ok(_) => {
                                rm_tree(&p);
                            }
                        }
                    }
                }
            }
        }
    }
    let _d = remove_dir(&dir);
}

#[allow(clippy::needless_return)]
fn read_bytes(path: &str) -> Vec<u8> {
    let f = File::open(&path);
    match f {
        Err(_) => { return Vec::<u8>::new(); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { return Vec::<u8>::new(); }
                Ok(region) => {
                    let mut copy: Vec<u8> = Vec::<u8>::new();
                    for k in 0 .. region.len() {
                        copy.push(region[k]);
                    }
                    return copy;
                }
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(&path);
    match d {
        Err(_) => { return; }
        Ok(mut w) => {
            let _n = w.write_all(bytes);
            return;
        }
    }
}

#[allow(clippy::needless_return)]
fn exists_any(path: &str) -> bool {
    let f = File::open(&path);
    match f {
        Err(_) => { return false; }
        Ok(_) => { return true; }
    }
}

/// Bounded settle loop: never pin WHICH round ingests (V1.2 phase B0 rule).
#[allow(clippy::needless_return)]
fn settle(p: &mut Pipeline, want: usize) -> usize {
    let mut got = 0usize;
    let mut rounds = 0usize;
    while got != want && rounds < 8usize {
        let one = p.scan_round();
        got += one;
        rounds += 1;
        if rounds >= 8usize {
            break;
        }
    }
    return got;
}

/// Config: model `model` over `glob`, single param TMP with `levels_toml`.
#[allow(clippy::needless_return)]
fn tmp_cfg(model: &str, data: &str, ing: &str, glob: &str, levels_toml: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"";
    let t = format!("{0}{1}", t, model);
    let t = t + "\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"";
    let t = format!("{0}{1}", t, glob);
    let t = t + "\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [";
    let t = format!("{0}{1}", t, levels_toml);
    let t = t + "]\n";
    return t;
}

// node (lat 5.0, lon 355.0) is EXACTLY grid node i=10, j=10 on this grid
const NODE_LAT: f64 = 5.0;
const NODE_LON: f64 = 355.0;
const PLANE_OFF: f64 = 32.0 * 10.0 + 10.0; // 32*j + i at the probe node = 330

#[allow(clippy::needless_return)]
fn hyb_want(l: f64, fh: f64) -> f64 {
    return 250.0 + 8.0 * l + 10.0 * fh + PLANE_OFF;
}

#[allow(clippy::needless_return)]
fn iso_want(p_hpa: f64) -> f64 {
    return 250.0 + 100000.0 + p_hpa + PLANE_OFF;
}

#[allow(clippy::needless_return)]
fn absdiff(a: f64, b: f64) -> f64 {
    if a > b { a - b } else { b - a }
}

// ---------------------------------------------------------------------------
// (1) decoder: type-105 messages parse, values RAW (dimensionless), while
// isobaric keeps its deliberate Pa->hPa divide — same file, both stacks.
// ---------------------------------------------------------------------------

#[test]
fn hybrid_sec4_decodes_raw_dimensionless_levels() {
    let r = decode_file("../../tests/fixtures/hybrid_small.grib2".to_string(),
                        ParamFilter::All);
    let (msgs, st) = match r {
        Err(e) => { println!("decode failed: {e:?}"); assert!(false); return; }
        Ok(pair) => { pair }
    };
    assert!(st.messages_total == 8);
    assert!(st.messages_decoded == 8);
    assert!(st.skip_coordlists == 0);
    assert!(st.skip_grid.is_empty());
    assert!(st.skip_pdt.is_empty());
    assert!(msgs.len() == 8);
    // first six: hybrid coords 1/50/255 x fh 0/6, RAW values, scale factor 0
    let want_l = [1.0f64, 1.0, 50.0, 50.0, 255.0, 255.0];
    let want_fh = [0i64, 21600, 0, 21600, 0, 21600];
    for k in 0 .. 6 {
        let m = &msgs[k];
        assert!(m.param_name == Some("TMP"));
        assert!(m.levels.len() == 1);
        assert!(m.levels[0].0 == 105);           // CT 4.5 type 105, NOT 100
        assert!(m.levels[0].1 == want_l[k]);     // raw coordinate, no divide
        assert!(m.fc_secs == want_fh[k]);
        assert!(m.data.len() == 41 * 21);
    }
    // last two: isobaric written Pa-scaled by the producer, normalized to hPa
    assert!(msgs[6].levels[0].0 == 100);
    assert!(msgs[6].levels[0].1 == 850.0);
    assert!(msgs[7].levels[0].1 == 500.0);
    // plane values at the probe node match expected.json exactly
    let n = 10usize * 41usize + 10usize; // N->S scan: row j=10 starts at 10*41
    let truth = [588.0f64, 648.0, 980.0, 1040.0, 2620.0, 2680.0, 101430.0, 101080.0];
    for k in 0 .. 8 {
        assert!(msgs[k].data[n] as f64 == truth[k]);
    }
}

// ---------------------------------------------------------------------------
// (2) pipeline: wildcard hybrid:* stores ONE ascending hybrid stack per fh;
// exact-level queries answer bit-exactly at the recorded node.
// ---------------------------------------------------------------------------

#[test]
fn hybrid_wildcard_stores_ascending_stack_and_answers_exact() {
    let root = "/tmp/opencode/wxpoint-hybrid-store".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "hybrid_small.grib2"),
                &read_bytes("../../tests/fixtures/hybrid_small.grib2"));
    let cfgp = join_path(&root, "config.toml");
    write_bytes(&cfgp, tmp_cfg("hyb", &data, &ing, "hybrid_small.grib2",
                               "\"hybrid:*\"").as_bytes());
    let p1r = Pipeline::open(&cfgp);
    let mut p1 = match p1r {
        Err(e) => { println!("open failed: {e}"); assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);
    assert!(p1.counters.errors == 0);
    assert!(p1.counters.skip_grid == 0);
    // storage dir name flows through canonical_level_name: TMP/hybrid/f*.bin
    let base = format!("{0}/hyb/2026101912/TMP/hybrid", data);
    assert!(exists_any(&format!("{0}/f000.bin", base)));
    assert!(exists_any(&format!("{0}/f006.bin", base)));
    assert!(!exists_any(&format!("{0}/hyb/2026101912/TMP/isobaric/f000.bin", data)));

    let snap = p1.shared.get_index();
    let sref = &*snap;
    let m = match sref.find_model("hyb") {
        None => { assert!(false); return; }
        Some(mm) => { mm }
    };
    assert!(m.runs.len() == 1);
    let mut hyb_field = None;
    for fe in &m.runs[0].fields {
        if fe.param_key == "TMP" && fe.level_code == 105 {
            hyb_field = Some(fe);
        }
    }
    let fe = match hyb_field {
        None => { println!("no TMP level_code 105 field in index"); assert!(false); return; }
        Some(f) => { f }
    };
    // TWO forecast hours grouped from six messages, three RAW levels each,
    // canonical ASCENDING order (level_better's non-isobaric branch)
    assert!(fe.fd.fhs.len() == 2);
    assert!(fe.fd.fhs[0].fh_secs == 0);
    assert!(fe.fd.fhs[1].fh_secs == 21600);
    let lv = &fe.fd.fhs[0].field.header.levels;
    assert!(lv.len() == 3);
    assert!(lv[0] as f64 == 1.0);
    assert!(lv[1] as f64 == 50.0);
    assert!(lv[2] as f64 == 255.0);

    // exact-match queries: every stored coordinate at both fhs, node-exact
    for kf in 0 .. 2 {
        let fhh = if kf == 0 { 0.0 } else { 21600.0 };
        for kl in 0 .. 3 {
            let l = want_levels()[kl];
            let c1r = build_context(sref, "hyb", None, "TMP", 105, l, fhh,
                                    TimeInterpMode::Linear);
            let c1 = match c1r {
                Err(e) => { println!("ctx L={l} fh={fhh}: {e:?}"); assert!(false); return; }
                Ok(c) => { c }
            };
            let q = evaluate_point(&c1, NODE_LAT, NODE_LON);
            match q {
                Err(other) => { println!("probe L={l} fh={fhh}: {other:?}"); assert!(false); }
                Ok(pv) => {
                    let w = hyb_want(l, if kf == 0 { 0.0 } else { 6.0 });
                    if !(pv.value == w) {
                        println!("L={l} fh={fhh}: got {0} want {w}", pv.value);
                        assert!(false);
                    }
                }
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn want_levels() -> [f64; 3] {
    return [1.0, 50.0, 255.0];
}

// ---------------------------------------------------------------------------
// (3) vertical interpolation LINEAR IN THE HYBRID COORDINATE: the field is
// analytic in L, and both probes below sit at binary-exact bracket weights
// (152.5 -> t = 0.5 between 50/255; 203.5 -> t = 0.75), so the answer must be
// EXACT, not approximately right. Also pins the §6.4 ORDER bonus: a request
// at fh 3 blends two exact horizontal+vertical solves in TIME with t = 0.5.
// ---------------------------------------------------------------------------

#[test]
fn hybrid_bracket_interpolation_is_analytic() {
    let root = "/tmp/opencode/wxpoint-hybrid-vert".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "hybrid_small.grib2"),
                &read_bytes("../../tests/fixtures/hybrid_small.grib2"));
    let cfgp = join_path(&root, "config.toml");
    write_bytes(&cfgp, tmp_cfg("hyv", &data, &ing, "hybrid_small.grib2",
                               "\"hybrid:*\"").as_bytes());
    let p1r = Pipeline::open(&cfgp);
    let mut p1 = match p1r {
        Err(e) => { println!("open failed: {e}"); assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);

    let snap = p1.shared.get_index();
    let sref = &*snap;

    // hybrid:152.5 @ fh 0 -> bracket (50, 255), t = 0.5 -> 1800 exactly
    let c1r = build_context(sref, "hyv", None, "TMP", 105, 152.5, 0.0,
                            TimeInterpMode::Linear);
    let c1 = match c1r {
        Err(e) => { println!("ctx 152.5: {e:?}"); assert!(false); return; }
        Ok(c) => { c }
    };
    let q1 = evaluate_point(&c1, NODE_LAT, NODE_LON);
    match q1 {
        Err(other) => { println!("probe 152.5: {other:?}"); assert!(false); }
        Ok(pv) => {
            let w = hyb_want(152.5, 0.0); // = 1800
            if absdiff(pv.value, w) > 1e-9 {
                println!("152.5: got {0} want {w}", pv.value);
                assert!(false);
            }
        }
    }
    // hybrid:203.5 @ fh 0 -> t = 0.75 -> 2208 exactly
    let c2r = build_context(sref, "hyv", None, "TMP", 105, 203.5, 0.0,
                            TimeInterpMode::Linear);
    let c2 = match c2r {
        Err(e) => { println!("ctx 203.5: {e:?}"); assert!(false); return; }
        Ok(c) => { c }
    };
    match evaluate_point(&c2, NODE_LAT, NODE_LON) {
        Err(other) => { println!("probe 203.5: {other:?}"); assert!(false); }
        Ok(pv) => {
            let w = hyb_want(203.5, 0.0); // = 2208
            if absdiff(pv.value, w) > 1e-9 {
                println!("203.5: got {0} want {w}", pv.value);
                assert!(false);
            }
        }
    }
    // hybrid:50 @ fh 3 (10800 s): vertical EXACT (stored level), time blend
    // t = 0.5 between fh 0 and fh 6 planes -> 985 exactly
    let c3r = build_context(sref, "hyv", None, "TMP", 105, 50.0, 10800.0,
                            TimeInterpMode::Linear);
    let c3 = match c3r {
        Err(e) => { println!("ctx fh3 blend: {e:?}"); assert!(false); return; }
        Ok(c) => { c }
    };
    match evaluate_point(&c3, NODE_LAT, NODE_LON) {
        Err(other) => { println!("probe fh3 blend: {other:?}"); assert!(false); }
        Ok(pv) => {
            let w = hyb_want(50.0, 3.0); // = 985
            if absdiff(pv.value, w) > 1e-9 {
                println!("fh3 blend: got {0} want {w}", pv.value);
                assert!(false);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (4) out of range NEVER extrapolates (spec §6.2): above the top stored
// coordinate, below the bottom one. Error is the existing vertical-range
// class — no new status codes anywhere.
// ---------------------------------------------------------------------------

#[test]
fn hybrid_out_of_range_never_extrapolates() {
    let root = "/tmp/opencode/wxpoint-hybrid-range".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "hybrid_small.grib2"),
                &read_bytes("../../tests/fixtures/hybrid_small.grib2"));
    let cfgp = join_path(&root, "config.toml");
    write_bytes(&cfgp, tmp_cfg("hyr", &data, &ing, "hybrid_small.grib2",
                               "\"hybrid:*\"").as_bytes());
    let p1r = Pipeline::open(&cfgp);
    let mut p1 = match p1r {
        Err(e) => { println!("open failed: {e}"); assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);

    let snap = p1.shared.get_index();
    let sref = &*snap;
    for want in [300.0f64, -5.0, 255.000001, 0.999999].iter() {
        // just outside the stored [1, 255] window on either side
        let wr = *want;
        let cr = build_context(sref, "hyr", None, "TMP", 105, wr, 0.0,
                               TimeInterpMode::Linear);
        match cr {
            Ok(_) => { println!("hybrid:{wr} unexpectedly resolvable"); assert!(false); }
            Err(e) => {
                match e {
                    SelectError::LevelOutOfRange => {}
                    other => { println!("hybrid:{wr}: wrong error {other:?}"); assert!(false); }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (5) STACK ISOLATION: same param, two level types in ONE file. Brackets
// never cross level types because every field file carries ONE type's levels
// (separate directories); a hybrid request whose VALUE falls numerically
// inside the isobaric band must still resolve against the hybrid stack, and
// an isobaric request outside its own band must fail rather than fall through.
// ---------------------------------------------------------------------------

#[test]
fn hybrid_isobaric_same_param_stacks_never_mix() {
    let root = "/tmp/opencode/wxpoint-hybrid-iso".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "hybrid_small.grib2"),
                &read_bytes("../../tests/fixtures/hybrid_small.grib2"));
    let cfgp = join_path(&root, "config.toml");
    write_bytes(&cfgp, tmp_cfg("hyi", &data, &ing, "hybrid_small.grib2",
                               "\"isobaric:*\", \"hybrid:*\"").as_bytes());
    let p1r = Pipeline::open(&cfgp);
    let mut p1 = match p1r {
        Err(e) => { println!("open failed: {e}"); assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);
    assert!(p1.counters.errors == 0);
    // BOTH stacks on disk, separate directories (the isolation mechanism)
    let base = format!("{0}/hyi/2026101912/TMP", data);
    assert!(exists_any(&format!("{0}/hybrid/f000.bin", base)));
    assert!(exists_any(&format!("{0}/isobaric/f000.bin", base)));

    let snap = p1.shared.get_index();
    let sref = &*snap;
    let m = match sref.find_model("hyi") {
        None => { assert!(false); return; }
        Some(mm) => { mm }
    };
    let mut n105 = 0usize;
    let mut n100 = 0usize;
    let mut hyb_fhs = 0usize;
    for fe in &m.runs[0].fields {
        if fe.param_key == "TMP" && fe.level_code == 105 {
            n105 += 1;
            // ONE field entry per (param, level type); forecast hours ride
            // inside it as fhs (same shape pinned in test 2)
            hyb_fhs = fe.fd.fhs.len();
        }
        if fe.param_key == "TMP" && fe.level_code == 100 { n100 += 1; }
    }
    assert!(n105 == 1); // hybrid stack entry, carrying fh 0 AND 6 ...
    assert!(hyb_fhs == 2); // ...both of them
    assert!(n100 == 1); // isobaric entry with fh 0 only

    // hybrid:128 lies NUMERICALLY INSIDE the isobaric band [500, 850]; the
    // answer proves the hybrid stack was used (band values ~1e5 would be
    // wildly different), resolved between hybrid 50 and 255.
    let c1r = build_context(sref, "hyi", None, "TMP", 105, 128.0, 0.0,
                            TimeInterpMode::Linear);
    let c1 = match c1r {
        Err(e) => { println!("ctx hyb128: {e:?}"); assert!(false); return; }
        Ok(c) => { c }
    };
    match evaluate_point(&c1, NODE_LAT, NODE_LON) {
        Err(other) => { println!("probe hyb128: {other:?}"); assert!(false); }
        Ok(pv) => {
            let w = hyb_want(128.0, 0.0); // 1604
            if absdiff(pv.value, w) > 1e-9 {
                println!("hyb128: got {0} want {w}", pv.value);
                assert!(false);
            }
            // belt AND braces: nothing near the DISJOINT isobaric band (~1e5)
            // leaked into a hybrid-stack answer (hybrid values top out ~2721)
            if pv.value > 10000.0 || pv.value < 0.0 {
                println!("hyb128 magnitude leak: {0}", pv.value);
                assert!(false);
            }
        }
    }

    // isobaric:700 interpolates linearly in hPa between 850 and 500 ONLY
    let c2r = build_context(sref, "hyi", None, "TMP", 100, 700.0, 0.0,
                            TimeInterpMode::Linear);
    let c2 = match c2r {
        Err(e) => { println!("ctx iso700: {e:?}"); assert!(false); return; }
        Ok(c) => { c }
    };
    match evaluate_point(&c2, NODE_LAT, NODE_LON) {
        Err(other) => { println!("probe iso700: {other:?}"); assert!(false); }
        Ok(pv) => {
            let w = iso_want(700.0); // 101280
            if absdiff(pv.value, w) > 1e-9 {
                println!("iso700: got {0} want {w}", pv.value);
                assert!(false);
            }
        }
    }
    // isobaric:300 is OUTSIDE the isobaric stored range [500, 850] even
    // though hybrid coordinates 1..255 bracket nothing there either: it must
    // be a vertical-range error, never a silent fall-through to the other
    // stack and never an extrapolation.
    let c3r = build_context(sref, "hyi", None, "TMP", 100, 300.0, 0.0,
                            TimeInterpMode::Linear);
    match c3r {
        Ok(_) => { println!("iso300 unexpectedly resolvable"); assert!(false); }
        Err(e) => {
            match e {
                SelectError::LevelOutOfRange => {}
                other => { println!("iso300: wrong error {other:?}"); assert!(false); }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (6) config/API parsing matrix: hybrid:NN integers AND decimals accepted,
// wildcard via parse_level_spec ("hybrid:*"), malformed spellings rejected
// with the SAME validation-error style as every other bad level name.
// ---------------------------------------------------------------------------

#[test]
fn hybrid_config_matrix_and_rejects() {
    // spec-level parses (config side)
    assert!(parse_level_spec("hybrid:*") == Some((105, None)));
    assert!(parse_level_spec("hybrid:837.5") == Some((105, Some(837.5))));
    assert!(parse_level_spec("hybrid:1") == Some((105, Some(1.0))));
    assert!(parse_level_spec("hybridx:1").is_none());
    assert!(parse_level_spec("hybrid:").is_none());
    assert!(parse_level_spec("hybrid:1e3").is_none());
    assert!(parse_level_spec("hybrid:*x").is_none());

    // full-config validation: accepted...
    let good = tmp_cfg("hym", "/d", "/i", "hybrid_small.grib2",
                       "\"hybrid:*\", \"hybrid:837.5\", \"hybrid:1\"");
    match validate_text(&good) {
        Err(e) => { println!("good hybrid config rejected: {e}"); assert!(false); }
        Ok(vm) => {
            let specs = &vm[0].params[0].level_specs;
            assert!(specs.len() == 3);
            assert!(specs[0] == (105, None));
            assert!(specs[1] == (105, Some(837.5)));
            assert!(specs[2] == (105, Some(1.0)));
        }
    }
    // ...and rejected, naming the bad spec like every other case does
    let bad = tmp_cfg("hym", "/d", "/i", "hybrid_small.grib2", "\"hybridx:1\"");
    match validate_text(&bad) {
        Ok(_) => { println!("typo hybrid level accepted?!"); assert!(false); }
        Err(e) => {
            if e.find("bad level spec").is_none() {
                println!("unexpected rejection text: {e}");
                assert!(false);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (7) UNKNOWN level types keep today's behavior EXACTLY: they DECODE fine
// (no structural skip, no error) but simply never match a configured spec,
// so they are not stored — and shrinking the stored stack shrinks the
// answerable range (hybrid:1 stops resolving once its plane carries type 200).
// Runtime byte-patch per the skip_reasons.rs precedent (no fixture churn).
// ---------------------------------------------------------------------------

/// Locate section-4 payload starts of every message; returns Vec<payload_off>
/// where payload index 0 == file offset of sec4 octet 6 (decoder convention).
#[allow(clippy::needless_return)]
fn sec4_payload_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::<usize>::new();
    let mut pos = 0usize;
    loop {
        // find next GRIB magic
        let mut i = usize::MAX;
        let mut k = pos;
        while k + 4 <= bytes.len() {
            if bytes[k] == 0x47 && bytes[k + 1] == 0x52 && bytes[k + 2] == 0x49 && bytes[k + 3] == 0x42 {
                i = k;
                break;
            }
            k += 1;
        }
        if i == usize::MAX {
            break;
        }
        let mut total = 0u64;
        for t in 8 .. 16 {
            total = (total << 8) | (bytes[i + t] as u64);
        }
        if total == 0 || (i as u64) + total > bytes.len() as u64 {
            // false magic inside a packed bitstream: keep scanning past it
            pos = i + 4;
            continue;
        }
        let mut j = i + 16;
        let stop = i + (total as usize);
        let mut found = false;
        while j + 5 <= stop {
            let mut slen = 0u32;
            for t in 0 .. 4 {
                slen = (slen << 8) | (bytes[j + t] as u32);
            }
            let sno = bytes[j + 4];
            if slen < 5 || j + (slen as usize) > stop {
                break;
            }
            if sno == 4 {
                out.push(j + 5);
                found = true;
                break;
            }
            j += slen as usize;
        }
        if !found {
            pos = i + 4;
            continue;
        }
        pos = i + (total as usize);
    }
    return out;
}

#[test]
fn unknown_level_type_stays_decode_ok_store_skip() {
    let raw = read_bytes("../../tests/fixtures/hybrid_small.grib2");
    let offs = sec4_payload_offsets(&raw);
    assert!(offs.len() == 8);
    let mut patched = Vec::<u8>::new();
    for k in 0 .. raw.len() {
        patched.push(raw[k]);
    }
    // message 0 (hybrid coord 1, fh 0): first fixed surface type 105 -> 200
    // (freezing level: a KNOWN name but unconfigured here; also proves
    // pass-through normalization leaves the value untouched)
    let t1off = offs[0] + 17;
    assert!(patched[t1off] == 105);
    patched[t1off] = 200;

    let root = "/tmp/opencode/wxpoint-hybrid-unknown".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "hybrid_small.grib2"), &patched);

    // decode side: still ALL decoded, zero skips of any kind
    let pth = join_path(&ing, "hybrid_small.grib2");
    let r = decode_file(pth, ParamFilter::All);
    let (msgs, st) = match r {
        Err(e) => { println!("patched decode failed: {e:?}"); assert!(false); return; }
        Ok(pair) => { pair }
    };
    assert!(st.messages_total == 8);
    assert!(st.messages_decoded == 8);
    assert!(st.skip_coordlists == 0);
    assert!(st.skip_grid.is_empty());
    assert!(st.skip_pdt.is_empty());
    assert!(msgs[0].levels[0].0 == 200);   // type flows through verbatim...
    assert!(msgs[0].levels[0].1 == 1.0);   // ...value unchanged (no special case)

    // store side: hybrid:* no longer matches that message -> only coords
    // 50/255 remain in the fh-0 stack, and hybrid:1 is now OUT OF RANGE
    let cfgp = join_path(&root, "config.toml");
    write_bytes(&cfgp, tmp_cfg("hyu", &data, &ing, "hybrid_small.grib2",
                               "\"hybrid:*\"").as_bytes());
    let p1r = Pipeline::open(&cfgp);
    let mut p1 = match p1r {
        Err(e) => { println!("open failed: {e}"); assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);
    assert!(p1.counters.errors == 0);
    assert!(p1.counters.skip_coordlists == 0);
    assert!(p1.counters.skip_grid == 0);

    let snap = p1.shared.get_index();
    let sref = &*snap;
    let m = match sref.find_model("hyu") {
        None => { assert!(false); return; }
        Some(mm) => { mm }
    };
    let mut lv: Vec<f64> = Vec::<f64>::new();
    for fe in &m.runs[0].fields {
        if fe.param_key == "TMP" && fe.level_code == 105 && fe.fd.fhs[0].fh_secs == 0 {
            for k in 0 .. fe.fd.fhs[0].field.header.levels.len() {
                lv.push(fe.fd.fhs[0].field.header.levels[k] as f64);
            }
        }
    }
    assert!(lv.len() == 2);
    assert!(lv[0] == 50.0);
    assert!(lv[1] == 255.0);
    let c1r = build_context(sref, "hyu", None, "TMP", 105, 1.0, 0.0,
                            TimeInterpMode::Linear);
    match c1r {
        Ok(_) => { println!("hybrid:1 resolved though its plane became type 200"); assert!(false); }
        Err(e) => {
            match e {
                SelectError::LevelOutOfRange => {}
                other => { println!("wrong error {other:?}"); assert!(false); }
            }
        }
    }
}
