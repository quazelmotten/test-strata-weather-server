// V1.4 phase C gate #3: the INDEX-BUILD cross-validation hook (wx-ingest side)
// plus the end-to-end warm-boot consequence of the typed LCC degeneracy error.
//
// Two halves, deliberately different in shape:
//
// (a) `prune_geometry_mismatches` / `SharedIndex::publish`. A field .bin whose
//     header geometry disagrees with its run's grid.bin must NEVER be indexed
//     (the silent-wrong-answer class: the bilinear kernel walks the GRID's
//     extent while `MmapField::value()` strides by the HEADER's), and a run
//     whose grid cannot back any query must be withheld whole. Removal is per
//     FORECAST-HOUR SLOT so the rest of the run keeps answering; every removal
//     emits a note naming the param/level/fh and the class, and each class
//     advances its own counter. The CLEAN case must cost nothing: the same
//     `Arc` comes back and nothing is cloned.
//
//     ACTIVATION NOTE, pinned honestly: `Pipeline::publish` swaps its ArcSwap
//     DIRECTLY rather than calling `SharedIndex::publish`, so the ingest path
//     does not route through this filter yet. These tests exercise both
//     `prune_geometry_mismatches` and `SharedIndex::publish` (the entry point
//     every publisher would get validated through); the one-line adoption in
//     `Pipeline::publish` is called out in the phase report. Until it lands, the
//     CRC + exact-length + structural validation inside
//     `wx_core::storage::open_field` carries the warm-boot guarantee (see
//     `crates/wx-core/tests/header_integrity.rs`).
//
// (b) A REAL warm boot over a crafted corrupt grid.bin. Pre-phase-C, an LCC
//     grid.bin with La1 = 90 hit grid.rs's only input-dependent `unwrap()`
//     inside `store::decode_grid`, and under the release profile's
//     `panic = "abort"` (spec §8) the daemon died at start-up: no note, no
//     error counter, nothing served. Now the grid is refused with a named
//     reason, the pipeline's EXISTING loud grid recovery drops the run and
//     forgets the model's records, and the next round re-decodes from source.

// Matching pedantic-but-cosmetic lints are allowed per-file, narrowly listed
// (same convention as the src modules and the phase B bounds gates).
#![allow(clippy::needless_return)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::too_many_arguments)]

use std::sync::Arc;
use std::path::Path;
use wx_core::grid::{GridDef, GridGaussian, GridLambert, GridRegular};
use wx_core::interp::TimeInterpMode;
use wx_core::storage::{BinHeader, StatMeta, MmapField, open_field, write_bin_atomic};
use wx_ingest::index::{IdxField, IdxFh, IdxFieldEntry, IdxModel, IdxRun, IndexSnapshot,
                       SharedIndex, build_context, evaluate_point, field_geometry_reason,
                       grid_usability_reason, index_build_counters, index_build_counters_reset,
                       prune_geometry_mismatches, SelectError};
use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::join_path;
use wx_ingest::store::encode_grid;
use std::fs::File;
use std::io::Write;

const DIR: &str = "/tmp/opencode/wxpoint-geom-xval";
const RUN_DIR: &str = "2026101912"; // gfs_small.grib2's section-1 reference time

// This binary shares ONE scratch tree and one set of process-wide counters, so
// its tests are serialized: a concurrent rm_tree could unlink a live fixture
// mid-open and a concurrent `index_build_counters_reset` could zero another
// test's measurement window. The assertions below are exact-count assertions,
// which cannot tolerate interleaving from other tests in the same binary.
static XVAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[allow(clippy::needless_return)]
fn rm_tree(dir: &Path) {
    match std::fs::read_dir(dir) {
        Err(_e) => { return; }
        Ok(it) => {
            for de in it.flatten() {
                let p = de.path();
                if p.is_dir() {
                    rm_tree(&p);
                } else {
                    let _r = std::fs::remove_file(&p);
                }
            }
        }
    }
    let _d = std::fs::remove_dir(dir);
}

#[allow(clippy::needless_return)]
fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(path);
    match d {
        Err(_e) => { panic!("create {0}", path); }
        Ok(mut w) => {
            if w.write_all(bytes).is_err() { panic!("write {0}", path); }
        }
    }
}

// ---------------------------------------------------------------------------
// synthetic index pieces (no GRIB needed: this is about header vs grid)
// ---------------------------------------------------------------------------

/// Open one synthetic field file, optionally mutating the header AFTER the
/// sealed write to simulate post-ingest rot that still passes self-consistency
/// (a transposed extent preserves the byte count, and a scan-bit flip preserves
/// everything: only the grid comparison sees those).
#[allow(clippy::needless_return)]
fn stub(name: &str, bias: f32, nx: u32, ny: u32, scan_ns: bool, levels: &[f32],
        rot_nx: Option<u32>, rot_scan: Option<bool>) -> Arc<MmapField> {
    let h = BinHeader {
        nx: nx, ny: ny, levels: levels.to_vec(), scan_ns: scan_ns,
        winds_grid: false, stat: StatMeta::instantaneous(),
    };
    let mut owned: Vec<Vec<f32>> = Vec::new();
    for li in 0 .. levels.len() {
        let mut p: Vec<f32> = Vec::new();
        for j in 0 .. (ny as usize) {
            for i in 0 .. (nx as usize) {
                p.push(bias + (1000 * li + 10 * j + i) as f32);
            }
        }
        owned.push(p);
    }
    let mut refs: Vec<&[f32]> = Vec::new();
    for plane in owned.iter() {
        refs.push(&plane[..]);
    }
    let path = format!("{0}/{1}.bin", DIR, name);
    if let Err(e) = write_bin_atomic(path.to_string(), &h, &refs[..]) {
        println!("stub write failed: {0}", e);
        panic!("stub field could not be written");
    }
    let mut mf: MmapField = open_field(path.to_string()).unwrap_or_else(|e| {
        println!("stub open failed: {0}", e);
        panic!("stub field could not be reopened");
    });
    if let Some(v) = rot_nx {
        mf.header.nx = v; // pretend the header rotted on disk
    }
    if let Some(v) = rot_scan {
        mf.header.scan_ns = v;
    }
    return Arc::new(mf);
}

fn fh(name: &str, bias: f32, fh_secs: i64, nx: u32, ny: u32, scan_ns: bool,
      levels: &[f32], rot_nx: Option<u32>, rot_scan: Option<bool>) -> IdxFh {
    return IdxFh {
        fh_secs: fh_secs,
        field: stub(name, bias, nx, ny, scan_ns, levels, rot_nx, rot_scan),
        stat_processed: false,
        stat_kind_code: wx_core::storage::STAT_INSTANTANEOUS,
        stat_has_range: false,
        stat_begin_secs: 0,
        stat_end_secs: 0,
        stat_step_secs: 0,
    };
}

fn entry(param_key: &str, level_code: u8, fhs: Vec<IdxFh>) -> IdxFieldEntry {
    return IdxFieldEntry {
        param_key: param_key.to_string(),
        level_code: level_code,
        fd: IdxField {
            units: "K".to_string(),
            stat_processed: false,
            stat_kind: "instantaneous".to_string(),
            fhs: fhs,
        },
    };
}

fn regular(nx: u32, ny: u32) -> Arc<GridDef> {
    return Arc::new(GridDef::Regular(GridRegular {
        nx: nx, ny: ny, lon1: 0.0, lat1: 45.0, dlon: 0.5, dlat: 0.5,
        scan_mode: 0x00, winds_grid: false,
    }));
}

/// Healthy run: 8 x 6 regular grid, rows south; TMP on [1000, 850] hPa at two
/// forecast hours, UGRD on [1000] hPa at one. Value at node (j, i) of level li
/// is `bias + 1000*li + 10*j + i`, so a query pins WHICH slot answered.
fn healthy_run(dir_name: &str) -> IdxRun {
    return IdxRun {
        epoch_secs: 1_760_000_000,
        dir_name: dir_name.to_string(),
        grid: regular(8, 6),
        fields: vec![
            entry("TMP", 100, vec![
                fh("tmp-f0", 100.0, 0, 8, 6, true, &[1000.0, 850.0], None, None),
                fh("tmp-f6", 200.0, 21600, 8, 6, true, &[1000.0, 850.0], None, None),
            ]),
            entry("UGRD", 100, vec![
                fh("ugrd-f0", 300.0, 0, 8, 6, true, &[1000.0], None, None),
            ]),
        ],
    };
}

fn snapshot(runs: Vec<IdxRun>) -> Arc<IndexSnapshot> {
    return Arc::new(IndexSnapshot {
        models: vec![IdxModel { name: "gfs".to_string(), runs: runs }],
    });
}

fn cfg_text(data: &str, ing: &str) -> String {
    let head = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let mid = "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let tail = "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return format!("{0}{1}{2}{3}{4}", head, data, mid, ing, tail);
}

#[allow(clippy::needless_return)]
fn settle(p: &mut Pipeline, want: usize) -> usize {
    let mut got = 0usize;
    let mut rounds = 0usize;
    while got < want && rounds < 8 {
        got += p.scan_round();
        rounds += 1;
    }
    return got;
}

#[allow(clippy::needless_return)]
fn count_notes(p: &Pipeline, pat: &str) -> usize {
    let mut n = 0usize;
    for line in &p.log {
        if line.contains(pat) {
            n += 1;
        }
    }
    return n;
}

// ---------------------------------------------------------------------------
// (a) the hook itself
// ---------------------------------------------------------------------------

#[test]
fn a_clean_snapshot_comes_back_as_the_same_arc_and_costs_nothing() {
    let _x = XVAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _c = std::fs::create_dir_all(DIR);
    index_build_counters_reset();
    let snap = snapshot(vec![healthy_run(RUN_DIR)]);
    let before = Arc::as_ptr(&snap) as usize;
    let pair = prune_geometry_mismatches(snap.clone());
    let after = Arc::as_ptr(&pair.0) as usize;
    assert!(pair.1.is_empty());
    assert!(before == after); // zero rebuild, zero clone on the happy path
    let c = index_build_counters();
    assert!(c.checks == 3); // exactly one comparison per indexed slot
    assert!(c.geom_rejects == 0 && c.grid_rejects == 0);
    // Publishing through SharedIndex::publish keeps every slot queryable.
    let sh = SharedIndex { store: arc_swap::ArcSwap::from_pointee(
        IndexSnapshot { models: Vec::<IdxModel>::new() }) };
    sh.publish(snap.clone());
    let got = sh.get_index();
    assert!(got.models.len() == 1 && got.models[0].runs.len() == 1);
    assert!(got.models[0].runs[0].fields.len() == 2);
}

#[test]
fn a_header_whose_extent_disagrees_is_skipped_while_the_run_keeps_serving() {
    let _x = XVAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _c = std::fs::create_dir_all(DIR);
    index_build_counters_reset();
    // TMP f0 was WRITTEN with nx = 9 while this run's grid has 8 columns: the
    // file is internally consistent (header, byte count and digest all agree),
    // so neither `open_field` nor the CRC can see it — only the comparison
    // against the grid definition can. A transposed extent would behave the
    // same way while additionally preserving the byte count.
    let mut r = healthy_run(RUN_DIR);
    r.fields[0].fd.fhs[0].field = stub("tmp-f0-rot", 100.0, 9, 6, true,
                                       &[1000.0, 850.0], None, None);
    let snap = snapshot(vec![r]);
    let pair = prune_geometry_mismatches(snap);
    assert!(pair.1.len() == 1);
    assert!(pair.1[0].contains("TMP") && pair.1[0].contains("nx"));
    let c = index_build_counters();
    assert!(c.geom_rejects == 1 && c.grid_rejects == 0);
    let pruned = pair.0;
    let run = &pruned.models[0].runs[0];
    assert!(run.fields.len() == 2); // TMP entry survives on its clean fh only
    let tmp = &run.fields[0];
    assert!(tmp.fd.fhs.len() == 1 && tmp.fd.fhs[0].fh_secs == 21600);
    assert!(run.fields[1].fd.fhs.len() == 1); // UGRD untouched
    // And the surviving slot really does answer: node (0,0) of level 850 hPa at
    // f+21600 s carries bias 200 + 1000*li(=1) = 1200 exactly.
    let ctxr = build_context(&pruned, "gfs", None, "TMP", 100, 850.0, 21600.0,
                             TimeInterpMode::None);
    let ctx = match ctxr {
        Err(e) => { println!("surviving slot not selectable: {0:?}", e); panic!(); }
        Ok(v) => { v }
    };
    let q = evaluate_point(&ctx, 45.0, 0.0);
    match q {
        Err(e) => { println!("surviving slot refused: {0:?}", e); panic!(); }
        Ok(pv) => {
            let d = pv.value - 1200.0;
            let ad = if d < 0.0 { -d } else { d };
            if ad > 1e-6 {
                panic!("wrong value {0}", pv.value);
            }
        }
    }
    // The skipped hour is now a loud miss rather than a wrong number.
    let gone = build_context(&pruned, "gfs", None, "TMP", 100, 850.0, 0.0,
                             TimeInterpMode::None);
    match gone {
        Ok(_c2) => { panic!("skipped forecast hour still answered"); }
        Err(e) => {
            assert!(e == SelectError::TimeExactOnly || e == SelectError::NoRunCovers);
        }
    }
}

#[test]
fn a_rotted_scan_flag_is_named_and_dropped_per_slot() {
    let _x = XVAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _c = std::fs::create_dir_all(DIR);
    index_build_counters_reset();
    let mut r = healthy_run(RUN_DIR);
    r.fields[1].fd.fhs[0] = fh("ugrd-f0-flip", 300.0, 0, 8, 6, false,
                               &[1000.0], None, Some(false));
    // (written scan_ns=false, mutated to stay false: the grid scans south, so
    // trusting it would mirror every row north-south — a plausible wrong map.)
    let pair = prune_geometry_mismatches(snapshot(vec![r]));
    assert!(pair.1.len() == 1);
    assert!(pair.1[0].contains("scan"));
    assert!(index_build_counters().geom_rejects == 1);
    // UGRD had EXACTLY ONE forecast hour and it was the rotten one, so the
    // whole entry leaves the snapshot (an entry with no answerable hour must
    // not linger as a queryable-looking miss). TMP's two slots are untouched.
    let run = &pair.0.models[0].runs[0];
    assert!(run.fields.len() == 1 && run.fields[0].param_key == "TMP");
    assert!(run.fields[0].fd.fhs.len() == 2);
}

#[test]
fn an_unusable_grid_withholds_its_run_and_nothing_else() {
    let _x = XVAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _c = std::fs::create_dir_all(DIR);
    index_build_counters_reset();
    let mut bad = healthy_run("1999123100");
    // A grid.bin whose La1 rotted onto the pole: `new()` poisoned it, so there
    // is no anchor. Its fields are individually consistent with the header
    // extents, which is exactly why the run-level check has to exist.
    bad.grid = Arc::new(GridDef::Lambert(GridLambert::new(
        8, 6, 90.0, 233.44, 229.53, 26.5, 26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, false)));
    let pair = prune_geometry_mismatches(snapshot(vec![healthy_run(RUN_DIR), bad]));
    assert!(pair.1.len() == 1);
    assert!(pair.1[0].contains("1999123100") && pair.1[0].contains("degenerate"));
    assert!(index_build_counters().grid_rejects == 1);
    let kept = &pair.0.models[0].runs;
    assert!(kept.len() == 1 && kept[0].dir_name == RUN_DIR);
    assert!(kept[0].fields.len() == 2);
}

#[test]
fn every_unusable_grid_shape_is_named_by_the_predicate() {
    let _x = XVAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let good = GridDef::Regular(GridRegular {
        nx: 8, ny: 6, lon1: 0.0, lat1: 45.0, dlon: 0.5, dlat: 0.5,
        scan_mode: 0, winds_grid: false });
    assert!(grid_usability_reason(&good).is_none());
    let zerox = GridDef::Regular(GridRegular {
        nx: 0, ny: 6, lon1: 0.0, lat1: 45.0, dlon: 0.5, dlat: 0.5,
        scan_mode: 0, winds_grid: false });
    match grid_usability_reason(&zerox) {
        None => { panic!("nx=0 regular grid accepted"); }
        Some(t) => { assert!(t.contains("zero")); }
    }
    let zerodx = GridDef::Regular(GridRegular {
        nx: 8, ny: 6, lon1: 0.0, lat1: 45.0, dlon: 0.0, dlat: 0.5,
        scan_mode: 0, winds_grid: false });
    assert!(grid_usability_reason(&zerodx).is_some());
    let nanlat = GridDef::Regular(GridRegular {
        nx: 8, ny: 6, lon1: 0.0, lat1: f64::NAN, dlon: 0.5, dlat: 0.5,
        scan_mode: 0, winds_grid: false });
    assert!(grid_usability_reason(&nanlat).is_some());
    // gaussian with an odd row count: the derived table cannot cover ny rows
    let oddg = GridDef::Gaussian(GridGaussian::new(4, 3, 0.0, 60.0, -60.0, 90.0, 0, 6, false));
    match grid_usability_reason(&oddg) {
        None => { panic!("gaussian grid with an uncovered row table accepted"); }
        Some(t) => { assert!(t.contains("row table")); }
    }
    let okgauss = GridDef::Gaussian(GridGaussian::new(8, 4, 0.0, 60.0, -60.0, 45.0, 0, 6, false));
    assert!(grid_usability_reason(&okgauss).is_none());
    // lambert: healthy vs poisoned anchor
    let okl = GridDef::Lambert(GridLambert::new(61, 41, 21.11389, 233.44, 229.53, 26.5,
                                                26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true));
    assert!(grid_usability_reason(&okl).is_none());
    let badl = GridDef::Lambert(GridLambert::new(61, 41, 90.0, 233.44, 229.53, 26.5,
                                                 26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true));
    match grid_usability_reason(&badl) {
        None => { panic!("anchorless lambert grid accepted"); }
        Some(t) => { assert!(t.contains("anchor")); }
    }
    // The per-field predicate agrees with the store-side single source of truth.
    let mf = stub("probe-tmp-f6", 200.0, 8, 6, true, &[1000.0], None, None);
    assert!(field_geometry_reason(&okl, &mf.header).is_some()); // 8x6 vs 61x41
    assert!(field_geometry_reason(&regular(8, 6), &mf.header).is_none());
}

// ---------------------------------------------------------------------------
// (b) real warm boot over a crafted corrupt grid.bin
// ---------------------------------------------------------------------------

#[test]
fn warm_boot_over_a_pole_lcc_gridbin_recovers_loudly_instead_of_aborting() {
    let _x = XVAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = DIR.to_string();
    rm_tree(Path::new(&root));
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = std::fs::create_dir_all(&ing);
    let fx = std::fs::read("../../tests/fixtures/gfs_small.grib2");
    let fixture = match fx {
        Err(_e) => { println!("missing tests/fixtures/gfs_small.grib2"); panic!(); }
        Ok(b) => { b }
    };
    write_bytes(&join_path(&ing, "gfs_small.grib2"), &fixture[..]);
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg_text(&data, &ing).as_bytes());

    // ---- life 1: normal ingest writes a real grid.bin ----------------------
    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(e) => { println!("pipeline open failed: {0}", e); panic!(); }
        Ok(v) => { v }
    };
    assert!(settle(&mut p1, 1) >= 1);
    let run_dir = join_path(&join_path(&data, "gfs"), RUN_DIR);
    let gp = join_path(&run_dir, "grid.bin");
    assert!(std::fs::metadata(&gp).is_ok());

    // ---- plant the corrupt grid.bin: LCC, La1 = +90 (the audited degeneracy)
    let pole = GridDef::Lambert(GridLambert::new(61, 41, 90.0, 233.44, 229.53, 26.5,
                                                 26.5, 35.0, 3000.0, 3000.0, 1, 0, 6, true));
    write_bytes(&gp, &encode_grid(&pole)[..]);

    // ---- life 2: restart over it. Pre-phase-C THIS LINE ABORTED the process.
    let p2r = Pipeline::open(&cfg_path);
    let mut p2 = match p2r {
        Err(e) => { println!("warm boot refused to start: {0}", e); panic!(); }
        Ok(v) => { v }
    };
    assert!(count_notes(&p2, "lambert grid rejected") == 1);
    // The recovery is LOUD TWICE under its established prefix: once naming the
    // typed rejection, once confirming the forgotten ingest records (both
    // predate phase C — the pipeline's grid-recovery contract is pinned
    // exactly as the restart gates know it).
    assert!(count_notes(&p2, "grid.bin in ") == 1);
    assert!(count_notes(&p2, "STORAGE-FORMAT RECOVERY: forgot") == 1);
    assert!(p2.counters.errors == 1);
    // the unusable run dir is gone and the model's records were forgotten...
    assert!(std::fs::metadata(&gp).is_err());
    // ... so NOTHING is queryable yet, which is the honest answer
    let snap = p2.shared.get_index();
    match snap.find_model("gfs") {
        None => {}
        Some(m) => { assert!(m.runs.is_empty()); }
    }
    // ---- and one round later the run is back from SOURCE, intact -----------
    assert!(settle(&mut p2, 1) >= 1);
    let snap2 = p2.shared.get_index();
    let m = match snap2.find_model("gfs") {
        None => { panic!("model never came back"); }
        Some(v) => { v }
    };
    assert!(m.runs.len() == 1 && !m.runs[0].fields.is_empty());
}
