// Milestone 3 end-to-end: fixture files -> ingest pipeline -> storage tree
// (spec §4.5 layout, atomic writes, manifest.json, multi-file merge) -> arc-swap
// index -> point queries through the snapshot (bilinear / vertical / time, NaN
// mask survival spec §9.7, APCP accumulation guard §9.8, wind rotation §4.4).
// Ground truth reuses the analytic formulas + wgrib2 probes validated in
// milestones 1-2 (tests/fixtures/expected.json).

use wx_ingest::pipeline::Pipeline;
use wx_ingest::pipeline::{level_dir_name};
use wx_ingest::store::{decode_grid};
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use wx_ingest::index::{build_context, evaluate_point, SelectError};
use wx_core::interp::{TimeInterpMode, PointError, HorizMethod};
use wx_core::tables::{lookup_level_type};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::read_dir;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::create_dir_all;

fn f_is_nan(x: f64) -> bool {
    return x != x;
}

fn abs_diff(a: f64, b: f64) -> f64 {
    let d = a - b;
    if d < 0.0 {
        return -d;
    }
    return d;
}

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
                        Err(_) => { assert!(false); }
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

fn read_bytes(path: String) -> Vec<u8> {
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

fn bytes_contains(hay: &[u8], pat: &[u8]) -> bool {
    if pat.len() > hay.len() {
        return false;
    }
    for s in 0 .. (hay.len() - pat.len() + 1) {
        let mut ok = true;
        for k in 0 .. pat.len() {
            if hay[s + k] != pat[k] {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }
    return false;
}

// The three ingest dirs each receive their fixtures; all three models share one
// data_dir. stable_window_secs=0: eligibility needs one re-observation on the
// NEXT scan round (the "size/mtime unchanged for the window" rule with a zero
// window), which is exactly what the two scan_rounds below exercise.
fn e2e_config_text(root: &str, ing_gfs: &str, ing_hrr: &str, ing_stat: &str) -> String {
    let data = join_path(&root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ningest_threads = 2\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_gfs);
    let t = t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"isobaric:*\", \"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"isobaric:*\", \"height_above_ground:10\"]\n\n[[models]]\nname = \"hrrr\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_hrr);
    let t = t + "\"\nfile_glob = \"hrrr_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"height_above_ground:10\"]\n\n[[models]]\nname = \"stat\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_stat);
    let t = t + "\"\nfile_glob = \"stat_*.grib2\"\n\n  [[models.params]]\n  name = \"APCP\"\n  levels = [\"*\"]\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn e2e_roundtrip_through_index() {
    let root = "/tmp/opencode/wxpoint-e2e".to_string();
    rm_tree(&root);
    let ing_gfs = join_path(&root, "ingest-gfs");
    let ing_hrr = join_path(&root, "ingest-hrrr");
    let ing_stat = join_path(&root, "ingest-stat");
    let data = join_path(&root, "data");
    let _c1 = create_dir_all(&ing_gfs);
    let _c2 = create_dir_all(&ing_hrr);
    let _c3 = create_dir_all(&ing_stat);
    // gfs gets BOTH twins: two files delivering the same (run,fh) must merge.
    copy_file(fixture("gfs_small.grib2"), join_path(&ing_gfs, "gfs_small.grib2"));
    copy_file(fixture("gfs_small_c3.grib2"), join_path(&ing_gfs, "gfs_small_c3.grib2"));
    copy_file(fixture("hrrr_small.grib2"), join_path(&ing_hrr, "hrrr_small.grib2"));
    copy_file(fixture("stat_small.grib2"), join_path(&ing_stat, "stat_small.grib2"));

    let cfg_path = join_path(&root, "config.toml");
    write_text(&cfg_path, e2e_config_text(&root, &ing_gfs, &ing_hrr, &ing_stat));

    let po = Pipeline::open(&cfg_path);
    let mut p = match po {
        Err(_e) => { assert!(false); return; }
        Ok(pp) => { pp }
    };

    // Round 1 only records stability observations; round 2 ingests everything.
    let n1 = p.scan_round();
    assert!(n1 == 0);
    let n2 = p.scan_round();
    assert!(n2 == 4);
    assert!(p.counters.ingested_files == 4);
    assert!(p.counters.errors == 0);
    // Round 3: persistent record says unchanged -> nothing re-ingested.
    let n3 = p.scan_round();
    assert!(n3 == 0);

    // ---- storage tree assertions -------------------------------------
    let gfs_run = join_path(&join_path(&data, "gfs"), "2026101912");
    let gb = read_bytes(join_path(&gfs_run, "grid.bin"));
    assert!(bytes_contains(&gb, "WXPTGRID".as_bytes()));
    let gd = decode_grid(&gb);
    match gd {
        Err(_) => { assert!(false); }
        Ok(g) => {
            assert!(g.nx() == 41);
            assert!(g.ny() == 21);
        }
    }
    let man = read_bytes(join_path(&gfs_run, "manifest.json"));
    assert!(bytes_contains(&man, "\"param\":\"TMP\"".as_bytes()));
    assert!(bytes_contains(&man, "\"param\":\"UGRD\"".as_bytes()));
    assert!(bytes_contains(&man, "\"forecast_hours\":[0,6,12]".as_bytes()));
    let iso_dir = level_dir_name(100);
    let hag_dir = level_dir_name(103);
    let surf_dir = level_dir_name(1);
    // TMP@isobaric f000 carries all three merged levels (41*21*4*3 + header).
    let tf = read_bytes(join_path(&join_path(&join_path(&gfs_run, "TMP"), iso_dir.as_str()), "f000.bin"));
    assert!(tf.len() > 10000);
    let th = read_bytes(join_path(&join_path(&join_path(&gfs_run, "TMP"), hag_dir.as_str()), "f006.bin"));
    assert!(th.len() > 3000);
    let vg = read_bytes(join_path(&join_path(&join_path(&gfs_run, "VGRD"), hag_dir.as_str()), "f012.bin"));
    assert!(vg.len() > 3000);
    let hrr_run = join_path(&join_path(&data, "hrrr"), "2026101912");
    let hh = read_bytes(join_path(&join_path(&join_path(&hrr_run, "TMP"), hag_dir.as_str()), "f003.bin"));
    assert!(hh.len() > 61 * 41 * 4);
    let stat_run = join_path(&join_path(&data, "stat"), "2026101912");
    let ap = read_bytes(join_path(&join_path(&join_path(&stat_run, "APCP"), surf_dir.as_str()), "f000.bin"));
    assert!(ap.len() > 11 * 7 * 4);

    // ---- index-level value checks through the arc-swap snapshot ------
    let snap = p.shared.get_index();
    let sref = &*snap;
    let gm = sref.find_model("gfs");
    match gm {
        None => { assert!(false); return; }
        Some(m) => {
            assert!(m.runs.len() == 1);
            assert!(m.runs[0].fields.len() == 6);
            match lookup_level_type("isobaric") {
                None => { assert!(false); return; }
                Some(iso_code) => {
                    // exact node: TMP @1000 f000 at (lat 10, lon 350) = 250.
                    let c_r = build_context(sref, "gfs", None, "TMP", iso_code,
                                         1000.0, 0.0, TimeInterpMode::Linear);
                    match c_r {
                        Err(_se) => { assert!(false); }
                        Ok(c) => {
                            let v = evaluate_point(&c, 10.0, 350.0);
                            match v {
                                Err(_) => { assert!(false); }
                                Ok(pv) => {
                                    assert!(pv.value == 250.0);
                                    assert!(pv.info.horizontal == HorizMethod::Exact);
                                }
                            }
                        }
                    }
                    // off-node bilinear: mid of (0,0)..(1,1) -> mean 252.5.
                    let c_b = build_context(sref, "gfs", None, "TMP", iso_code,
                                         1000.0, 0.0, TimeInterpMode::Linear);
                    match c_b {
                        Err(_) => { assert!(false); }
                        Ok(cb) => {
                            let vb = evaluate_point(&cb, 9.75, 350.25);
                            match vb {
                                Err(_) => { assert!(false); }
                                Ok(pvb) => { assert!(pvb.value == 252.5); }
                            }
                        }
                    }
                    // vertical: 925 between 1000 and 850 at node j=2,i=3.
                    let c_v = build_context(sref, "gfs", None, "TMP", iso_code,
                                         925.0, 0.0, TimeInterpMode::Linear);
                    match c_v {
                        Err(_) => { assert!(false); }
                        Ok(cv) => {
                            let vv = evaluate_point(&cv, 9.0, 351.5);
                            match vv {
                                Err(_) => { assert!(false); }
                                Ok(pvv) => {
                                    assert!(pvv.value == 311.0);
                                    match pvv.info.vertical {
                                        None => { assert!(false); }
                                        Some(vm) => {
                                            assert!(vm.levels.0 == 1000.0 && vm.levels.1 == 850.0);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // time: fh 3 between 0 and 6 at same node -> 291.
                    let c_t = build_context(sref, "gfs", None, "TMP", iso_code,
                                         1000.0, 10800.0, TimeInterpMode::Linear);
                    match c_t {
                        Err(_) => { assert!(false); }
                        Ok(ct) => {
                            let vt = evaluate_point(&ct, 9.0, 351.5);
                            match vt {
                                Err(_) => { assert!(false); }
                                Ok(pvt) => {
                                    assert!(pvt.value == 291.0);
                                    match pvt.info.time {
                                        None => { assert!(false); }
                                        Some(tm) => {
                                            assert!(tm.forecast_hours.0 == 0.0 && tm.forecast_hours.1 == 6.0);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // time_interp=none: fh 3 must be rejected outright.
                    let c_n = build_context(sref, "gfs", None, "TMP", iso_code,
                                         1000.0, 10800.0, TimeInterpMode::None);
                    match c_n {
                        Err(ne) => {
                            match ne {
                                SelectError::TimeExactOnly => {}
                                _ => { assert!(false); }
                            }
                        }
                        Ok(_) => { assert!(false); }
                    }
                    // masked node: TMP @850 f006 k=0 is NaN -> MissingData.
                    let c_m = build_context(sref, "gfs", None, "TMP", iso_code,
                                         850.0, 21600.0, TimeInterpMode::Linear);
                    match c_m {
                        Err(_) => { assert!(false); }
                        Ok(cm) => {
                            let vm = evaluate_point(&cm, 10.0, 350.0);
                            match vm {
                                Ok(pvm) => { assert!(f_is_nan(pvm.value)); }
                                Err(pe) => {
                                    match pe {
                                        PointError::MissingData => {}
                                        _ => { assert!(false); }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // HRRR LCC exact-node probe + rotated-wind magnitude invariant (§4.4).
    let hm = sref.find_model("hrrr");
    match hm {
        None => { assert!(false); }
        Some(m) => {
            assert!(m.runs.len() == 1);
            assert!(m.runs[0].fields.len() == 3);
            match lookup_level_type("height_above_ground") {
                None => { assert!(false); }
                Some(hag_code) => {
                    let c5 = build_context(sref, "hrrr", None, "TMP", hag_code,
                                        2.0, 0.0, TimeInterpMode::Linear);
                    match c5 {
                        Err(_) => { assert!(false); }
                        Ok(c) => {
                            let v5 = evaluate_point(&c, 21.11389, 233.44);
                            match v5 {
                                Err(_) => { assert!(false); }
                                Ok(pv5) => { assert!(pv5.value == 260.0); }
                            }
                        }
                    }
                    // grid winds were U=5260 V=9260 at this node (fh 0); the
                    // stored earth pair must preserve magnitude exactly (f32
                    // rotation rounding aside).
                    let cu = build_context(sref, "hrrr", None, "UGRD", hag_code,
                                        10.0, 0.0, TimeInterpMode::Linear);
                    let cv = build_context(sref, "hrrr", None, "VGRD", hag_code,
                                        10.0, 0.0, TimeInterpMode::Linear);
                    match cu {
                        Err(_) => { assert!(false); }
                        Ok(cu2) => {
                            match cv {
                                Err(_) => { assert!(false); }
                                Ok(cv2) => {
                                    let eu = evaluate_point(&cu2, 21.11389, 233.44);
                                    let ev = evaluate_point(&cv2, 21.11389, 233.44);
                                    match eu {
                                        Err(_) => { assert!(false); }
                                        Ok(eu2) => {
                                            match ev {
                                                Err(_) => { assert!(false); }
                                                Ok(ev2) => {
                                                    let lhs = eu2.value * eu2.value + ev2.value * ev2.value;
                                                    let rhs = 5260.0 * 5260.0 + 9260.0 * 9260.0;
                                                    assert!(abs_diff(lhs, rhs) <= 1e-4 * rhs);
                                                    // rotation actually happened (not identity)
                                                    assert!(eu2.value < 5260.0 && eu2.value > 4000.0);
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

    // stat model: PDT 4.8 guard survives ingest into the index.
    let sm2 = sref.find_model("stat");
    match sm2 {
        None => { assert!(false); }
        Some(m) => {
            assert!(m.runs.len() == 1);
            assert!(m.runs[0].fields.len() == 2);
            let mut found = false;
            for fe in &m.runs[0].fields {
                if fe.param_key == "APCP" {
                    found = true;
                    assert!(fe.fd.stat_processed);
                    assert!(fe.fd.stat_kind == "accumulation".to_string());
                    assert!(fe.fd.fhs.len() == 2);
                }
            }
            assert!(found);
            // APCP accumulation: fh 3 between 0 and 6 must be REJECTED (§9.8).
            let cg = build_context(sref, "stat", None, "APCP", 1,
                                0.0, 10800.0, TimeInterpMode::Linear);
            match cg {
                Ok(_) => { assert!(false); }
                Err(g) => {
                    match g {
                        SelectError::StatTimeGuard(..) => {}
                        SelectError::TimeExactOnly => {}
                        _ => { assert!(false); }
                    }
                }
            }
            // APCP exact fh 0 answers with the analytic value 10 at (240,20).
            let ce = build_context(sref, "stat", None, "APCP", 1,
                                0.0, 0.0, TimeInterpMode::Linear);
            match ce {
                Err(_) => { assert!(false); }
                Ok(ce2) => {
                    let ve = evaluate_point(&ce2, 20.0, 240.0);
                    match ve {
                        Err(_) => { assert!(false); }
                        Ok(pve) => { assert!(pve.value == 10.0); }
                    }
                }
            }
        }
    }

    rm_tree(&root);
}
