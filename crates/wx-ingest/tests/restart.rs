// Spec §4.1 persistent record: a restart must NOT re-ingest unchanged files,
// but the same path delivered with DIFFERENT content (here: the DRT 5.3 twin
// written over the 5.0 file) MUST be re-ingested. Also proves rebuild_from_disk
// restores the index (mmaps opened at build time) and that manifest.json round-
// trips the PDT 4.8 stat guard across a restart.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_core::tables::{lookup_level_type};
use wx_core::grid::{GridDef};
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

fn fixture(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

fn read_bytes(path: &str) -> Vec<u8> {
    let f = File::open(path);
    match f {
        Err(_) => { Vec::<u8>::new()}
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { Vec::<u8>::new()}
                Ok(region) => {
                    let mut copy: Vec<u8> = Vec::<u8>::new();
                    for k in 0 .. region.len() {
                        copy.push(region[k]);
                    }
                    copy
                }
            }
        }
    }
}

fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!();}
        Ok(mut w) => {
            let n = w.write_all(bytes);
            if n.is_err() { panic!(); }
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
    false
}

fn rm_tree(dir: &str) {
    match read_dir(dir) {
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
    let _d = remove_dir(dir);
}

fn e2e_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"isobaric:*\", \"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"isobaric:*\", \"height_above_ground:10\"]\n\n[[models]]\nname = \"stat\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"stat_*.grib2\"\n\n  [[models.params]]\n  name = \"APCP\"\n  levels = [\"*\"]\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n\n[[models]]\nname = \"hrrr\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"hrrr_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"height_above_ground:10\"]\n"
}

#[test]
fn restart_no_reingest_and_content_change_redelivers() {
    let root = "/tmp/opencode/wxpoint-restart".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);

    write_bytes(&join_path(&ing, "gfs_small.grib2"), &read_bytes(&fixture("gfs_small.grib2")));
    write_bytes(&join_path(&ing, "stat_small.grib2"), &read_bytes(&fixture("stat_small.grib2")));
    write_bytes(&join_path(&ing, "hrrr_small.grib2"), &read_bytes(&fixture("hrrr_small.grib2")));

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, e2e_cfg(&data, &ing).as_bytes());

    // ---- first life ----------------------------------------------------
    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    let _ = p1.scan_round();
    let n = p1.scan_round();
    assert!(n == 3);
    assert!(p1.counters.errors == 0);

    // ---- PDT 4.8 statistical TIME RANGES must be on disk (spec §4.2) ------
    // Two accumulations of ONE field have DIFFERENT ranges, so the range travels
    // with each forecast hour (`stat_ranges` keyed by exact fh_secs), rendered as
    // absolute ISO-8601 UTC instants. The 3-hour TMP average proves the same
    // machinery covers non-accumulation statistics.
    let stat_run = join_path(&join_path(&data, "stat"), "2026101912");
    let man = read_bytes(&join_path(&stat_run, "manifest.json"));
    assert!(bytes_contains(&man, "\"fh_secs\":0,\"begin\":\"2026-10-19T12:00:00Z\",\"end\":\"2026-10-19T18:00:00Z\",\"step_secs\":0".as_bytes()));
    assert!(bytes_contains(&man, "\"fh_secs\":21600,\"begin\":\"2026-10-19T18:00:00Z\",\"end\":\"2026-10-20T00:00:00Z\",\"step_secs\":0".as_bytes()));
    assert!(bytes_contains(&man, "\"fh_secs\":10800,\"begin\":\"2026-10-19T15:00:00Z\",\"end\":\"2026-10-19T18:00:00Z\"".as_bytes()));

    // ---- simulated restart ---------------------------------------------
    let p2r = Pipeline::open(&cfg_path);
    let mut p2 = match p2r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    // Index rebuilt from disk BEFORE any scan: gfs run present with mmapped
    // fields, units restored from config, stat guard restored from manifest.
    let snap = p2.shared.get_index();
    let sref = &*snap;
    let gm = sref.find_model("gfs");
    match gm {
        None => { panic!(); }
        Some(m) => {
            assert!(m.runs.len() == 1);
            assert!(m.runs[0].fields.len() == 6);
            match lookup_level_type("isobaric") {
                None => { panic!(); }
                Some(iso) => {
                    let c_r = build_context(sref, "gfs", None, "TMP", iso,
                                         1000.0, 0.0, TimeInterpMode::Linear);
                    match c_r {
                        Err(_) => { panic!(); }
                        Ok(c) => {
                            assert!(c.units == "K");
                            let v = evaluate_point(&c, 10.0, 350.0);
                            match v {
                                Err(_) => { panic!(); }
                                Ok(pv) => {
                                    assert!(pv.value == 250.0);
                                    assert!(pv.info.horizontal == HorizMethod::Exact);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let sm = sref.find_model("stat");
    match sm {
        None => { panic!(); }
        Some(m) => {
            let mut found = false;
            let mut apcp_r0 = (false, 0, 0);
            let mut apcp_r6 = (false, 0, 0);
            let mut tmp_r3 = (false, 0, 0);
            for fe in &m.runs[0].fields {
                if fe.param_key == "APCP" {
                    found = true;
                    // manifest.json round-trip restored the accumulation guard.
                    assert!(fe.fd.stat_processed);
                    assert!(fe.fd.stat_kind == "accumulation");
                    // ...and the PER-fh statistical ranges survived the restart
                    // (offsets from the run time, exactly as written).
                    assert!(fe.fd.fhs.len() == 2);
                    for sl in &fe.fd.fhs {
                        if sl.fh_secs == 0 {
                            apcp_r0 = (sl.stat_has_range, sl.stat_begin_secs, sl.stat_end_secs);
                        }
                        if sl.fh_secs == 21600 {
                            apcp_r6 = (sl.stat_has_range, sl.stat_begin_secs, sl.stat_end_secs);
                        }
                    }
                }
                if fe.param_key == "TMP" && fe.level_code == 103 {
                    for sl in &fe.fd.fhs {
                        if sl.fh_secs == 10800 {
                            tmp_r3 = (sl.stat_has_range, sl.stat_begin_secs, sl.stat_end_secs);
                        }
                    }
                }
            }
            assert!(found);
            assert!(apcp_r0.0 && apcp_r0.1 == 0 && apcp_r0.2 == 21600);
            assert!(apcp_r6.0 && apcp_r6.1 == 21600 && apcp_r6.2 == 43200);
            assert!(tmp_r3.0 && tmp_r3.1 == 10800 && tmp_r3.2 == 21600);
        }
    }

    // LCC warm-boot regression (bug fixed at milestone 5): decode_grid's
    // lambert length guard previously demanded 97 bytes while encode_grid
    // writes 89 -> EVERY LCC run was silently dropped from a warm-start
    // index. hrrr must now be present with its Lambert grid and answer an
    // exact-node probe at La1/Lo1.
    let hm = sref.find_model("hrrr");
    match hm {
        None => { panic!(); }
        Some(m) => {
            assert!(m.runs.len() == 1);
            assert!(m.runs[0].fields.len() == 3);
            match &*m.runs[0].grid {
                GridDef::Lambert(g) => {
                    assert!(g.nx == 61);
                    assert!(g.ny == 41);
                }
                GridDef::Regular(_) => { panic!(); }
                GridDef::Gaussian(_) => { panic!(); }
            }
            match lookup_level_type("height_above_ground") {
                None => { panic!(); }
                Some(hgt) => {
                    let c_r = build_context(sref, "hrrr", None, "TMP", hgt,
                                         2.0, 0.0, TimeInterpMode::Linear);
                    match c_r {
                        Err(_) => { panic!(); }
                        Ok(c) => {
                            let v = evaluate_point(&c, 21.11389, 233.44);
                            match v {
                                Err(_) => { panic!(); }
                                Ok(pv) => {
                                    assert!(pv.info.horizontal == HorizMethod::Exact);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Unchanged files: two more rounds must re-ingest NOTHING (record hit).
    let a = p2.scan_round();
    let b = p2.scan_round();
    assert!(a == 0);
    assert!(b == 0);
    assert!(p2.counters.ingested_files == 0);

    // Same path, different content (the c3 twin bytes): MUST re-ingest.
    write_bytes(&join_path(&ing, "gfs_small.grib2"), &read_bytes(&fixture("gfs_small_c3.grib2")));
    let c1 = p2.scan_round();
    assert!(c1 == 0); // size changed -> stability resets, then one more round
    let c2 = p2.scan_round();
    assert!(c2 == 1);
    assert!(p2.counters.ingested_files == 1);
    assert!(p2.counters.errors == 0);

    // Values identical after the re-delivery (DRT 5.3 twin decodes equal):
    // TMP@850 f006 at node j=2,i=3 (k=85, unmasked) = 250+100+60+8+3 = 421.
    let snap2 = p2.shared.get_index();
    let s2 = &*snap2;
    let gm2 = s2.find_model("gfs");
    match gm2 {
        None => { panic!(); }
        Some(_m) => {
            match lookup_level_type("isobaric") {
                None => { panic!(); }
                Some(iso) => {
                    let c_r = build_context(s2, "gfs", None, "TMP", iso,
                                         850.0, 21600.0, TimeInterpMode::Linear);
                    match c_r {
                        Err(_) => { panic!(); }
                        Ok(c) => {
                            let v = evaluate_point(&c, 9.0, 351.5);
                            match v {
                                Err(_) => { panic!(); }
                                Ok(pv) => { assert!(pv.value == 421.0); }
                            }
                        }
                    }
                }
            }
        }
    }

    // State file persisted atomically under <data>/state/files.json.
    let st = read_bytes(&join_path(&join_path(&data, "state"), "files.json"));
    assert!(st.len() > 10);

    rm_tree(&root);
}
