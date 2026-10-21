// V1.1 phase 3 (§4.2 robustness): messages this server can never answer as a
// point value must be SKIPPED AND NOTED, never fatal and never counted as
// ingest errors. Fixture `robust_small.grib2` (scripts/gen_grib2.py) holds
// FOUR messages in this order:
//   1. TMP @isobaric 850      -> stored
//   2. TMP @isobaric 925 with COORDINATE VALUES after the PDT template
//                          -> decoder SKIP-AND-NOTE (messages_unsupported)
//   3. TMP @isobaric LAYER 850-800 (two fixed surfaces)
//                          -> decoded, then the pipeline skips it
//   4. TMP @height_above_ground 2 -> stored
// The two good messages decode byte-perfectly, which proves the message walk
// resumed at the right boundary (`pos = body_end` from the section-0 declared
// length) instead of desyncing or aborting the file.
//
// Analytic field: value = 300 + j*21 + i (wgrib2-verified by scripts/regen_expected.sh).

use wx_ingest::pipeline::{Pipeline, level_dir_name};
use wx_ingest::index::{PointQueryContext, evaluate_point, SelectError};
use wx_ingest::grib2::{decode_file, ParamFilter, SkipReason, skip_reason_text};
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_core::tables::{lookup_level_type};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::remove_file;
use std::fs::remove_dir;

fn fixture(name: &str) -> String {
    return format!("../../tests/fixtures/{name}");
}

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

fn copy_file(src: String, dst: String) {
    let f = File::open(&src);
    match f {
        Err(_) => { assert!(false); return; }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { assert!(false); return; }
                Ok(region) => {
                    let d = File::create(&dst);
                    match d {
                        Err(_) => { assert!(false); return; }
                        Ok(mut w) => {
                            let n = w.write_all(&region[0 .. region.len()]);
                            match n {
                                Err(_) => { assert!(false); }
                                Ok(_) => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

fn write_text(path: &str, text: String) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(text.as_bytes());
            match n {
                Err(_) => { assert!(false); }
                Ok(_) => {}
            }
        }
    }
}

fn file_exists(path: String) -> bool {
    let f = File::open(&path);
    match f {
        Err(_) => { return false; }
        Ok(_) => { return true; }
    }
}

fn robust_cfg_text(root: &str) -> String {
    let ing = join_path(&root, "ingest");
    let data = join_path(&root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"robust\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"robust_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

// ---------------------------------------------------------------------------
// decoder level: coordinate-list messages are skipped, the walk stays aligned
// ---------------------------------------------------------------------------

#[test]
fn coord_list_message_is_skipped_and_the_walk_resumes() {
    let r = decode_file(fixture("robust_small.grib2"), ParamFilter::All);
    match r {
        Err(e) => {
            println!("decode failed {:?}", e);
            assert!(false);
            return;
        }
        Ok((msgs, st)) => {
            // FOUR messages in the file; ONE is structurally unusable, so THREE
            // come back and the file as a whole decoded WITHOUT an error.
            assert!(st.messages_total == 4);
            assert!(st.messages_unsupported == 1);
            assert!(msgs.len() == 3);
            // order preserved: [850mb][layer 850-800][2m AGL]
            assert!(msgs[0].levels.len() == 1 && msgs[0].levels[0].0 == 100);
            assert!(msgs[1].levels.len() == 2); // layer/range survives decoding
            assert!(msgs[2].levels.len() == 1 && msgs[2].levels[0].0 == 103);
            // The skip reason is typed and human-readable (surfaced in notes).
            let sr = SkipReason::CoordLists;
            assert!(skip_reason_text(&sr) == "coordinate list after the product-definition template".to_string());
            // Byte-perfect resume: BOTH good messages carry the analytic field
            // 300 + j*21 + i at every node, including the probe node j=2,i=3.
            let k = 2 * 21 + 3;
            assert!(msgs[0].data[k] == 345.0);
            assert!(msgs[2].data[k] == 345.0);
            // and the neighbours match too, so nothing shifted by one message
            assert!(msgs[0].data[0] == 300.0);
            assert!(msgs[2].data[2 * 21 + 4] == 346.0);
        }
    }
}

// ---------------------------------------------------------------------------
// pipeline level: skips are counted separately from errors, storage is clean
// ---------------------------------------------------------------------------

#[test]
fn pipeline_skips_are_noted_not_errors() {
    let root = "/tmp/opencode/wxpoint-robust".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ingest");
    let _c = create_dir_all(&ing);
    copy_file(fixture("robust_small.grib2"), join_path(&ing, "robust_small.grib2"));
    write_text(&join_path(&root, "config.toml"), robust_cfg_text(&root));
    let po = Pipeline::open(&join_path(&root, "config.toml"));
    let mut p = match po {
        Err(e) => {
            println!("pipeline open failed: {0}", e);
            assert!(false);
            return;
        }
        Ok(pp) => { pp }
    };
    let n1 = p.scan_round();
    assert!(n1 == 0);
    let n2 = p.scan_round();
    // the FILE is a success: one ingested file, ZERO errors...
    assert!(n2 == 1);
    assert!(p.counters.errors == 0);
    // ...and TWO noted skips: the coordinate-list message (from the decoder)
    // plus the 850-800 layer message (rejected by the pipeline).
    assert!(p.counters.skipped_messages == 2);
    // a note exists explaining the layer skip
    let mut saw_layer_note = false;
    for line in &p.log {
        if line.find("levels skipped").is_some() {
            saw_layer_note = true;
        }
    }
    assert!(saw_layer_note);

    let data = join_path(&root, "data");
    let run = join_path(&join_path(&data, "robust"), "2026101912");
    let iso = level_dir_name(100);
    let hag = level_dir_name(103);
    // exactly the two point-queryable fields exist on disk...
    assert!(file_exists(join_path(&join_path(&join_path(&run, "TMP"), iso.as_str()), "f000.bin")));
    assert!(file_exists(join_path(&join_path(&join_path(&run, "TMP"), hag.as_str()), "f000.bin")));
    // ...and NO layer/range artefact leaked into the store: the only entries
    // under TMP are exactly the two canonical level directories.
    let mut dirs_seen = 0;
    let mut saw_layer_dir = false;
    match read_dir(&join_path(&run, "TMP")) {
        Err(_) => { assert!(false); }
        Ok(it) => {
            for e in it {
                match e {
                    Err(_) => {}
                    Ok(de) => {
                        dirs_seen += 1;
                        let pth = path_to_str(&de.path());
                        let expect_a = join_path(&join_path(&run, "TMP"), iso.as_str());
                        let expect_b = join_path(&join_path(&run, "TMP"), hag.as_str());
                        if pth != expect_a.clone() && pth != expect_b.clone() {
                            // anything other than the two canonical level dirs
                            saw_layer_dir = true;
                        }
                    }
                }
            }
        }
    }
    assert!(dirs_seen == 2);
    assert!(!saw_layer_dir);

    // Both surviving fields answer the wgrib2-probed node exactly.
    let snap = p.shared.get_index();
    let sref = &*snap;
    match sref.find_model("robust") {
        None => { assert!(false); return; }
        Some(m) => {
            assert!(m.runs.len() == 1);
            assert!(m.runs[0].fields.len() == 2);
            let seed = PointQueryContext {
                grid: m.runs[0].grid.clone(),
                units: "".to_string(),
                run_epoch_secs: 0,
                stat_processed: false,
                want_level: 0.0,
                want_fh_secs: 0.0,
                fh_secs: (0.0, 0.0),
                slots: [None, None],
            };
            match lookup_level_type("isobaric") {
                None => { assert!(false); return; }
                Some(iso_code) => {
                    let c8 = seed.build(sref, "robust", None, "TMP", iso_code,
                                        850.0, 0.0, TimeInterpMode::Linear);
                    match c8 {
                        Err(_) => { assert!(false); }
                        Ok(c) => {
                            let v = evaluate_point(&c, 9.0, 351.5);
                            match v {
                                Ok(pv) => {
                                    assert!(pv.value == 345.0);
                                    assert!(pv.info.horizontal == HorizMethod::Exact);
                                }
                                Err(_) => { assert!(false); }
                            }
                        }
                    }
                    // the LAYER level type was never indexed: asking for 825mb
                    // has no field to bracket between (only 850 exists).
                    let cl = seed.build(sref, "robust", None, "TMP", iso_code,
                                        825.0, 0.0, TimeInterpMode::Linear);
                    match cl {
                        Err(ne) => {
                            match ne {
                                SelectError::LevelOutOfRange => {}
                                _ => {
                                    println!("unexpected select error {:?}", ne);
                                    assert!(false);
                                }
                            }
                        }
                        Ok(_) => { assert!(false); }
                    }
                }
            }
            match lookup_level_type("height_above_ground") {
                None => { assert!(false); return; }
                Some(hag_code) => {
                    let c2 = seed.build(sref, "robust", None, "TMP", hag_code,
                                        2.0, 0.0, TimeInterpMode::Linear);
                    match c2 {
                        Err(_) => { assert!(false); }
                        Ok(c) => {
                            let v = evaluate_point(&c, 9.0, 351.5);
                            match v {
                                Ok(pv) => { assert!(pv.value == 345.0); }
                                Err(_) => { assert!(false); }
                            }
                        }
                    }
                }
            }
        }
    }
    rm_tree(&root);
}
