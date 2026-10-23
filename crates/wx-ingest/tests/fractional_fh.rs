// V1.1 phase 3 (§4.2/§4.5/§6.3): FRACTIONAL FORECAST HOURS end to end.
//
// Fixture `frac_small.grib2` (scripts/gen_grib2.py) carries Code Table 4.4
// unit = MINUTE messages: run 2026-10-19T12Z at fc 0/30/60 minutes, a SECOND
// RUN 12:30Z in the SAME file (the run-directory collision regression), and
// one PDT 4.8 accumulation whose statistical range spans 30 minutes. Ground
// truth comes from scripts/regen_expected.sh probing tools/wgrib2-full:
//     value = BASE + j*21 + i + fc_minute   (BASE 250 run 12Z, 500 run 12:30Z,
//                                           5 for APCP)
// verified equal to wgrib2's own readback of the minute-unit forecast times
// (295/325/355/545/575/80 at lon 351.5 lat 9.0).

use wx_ingest::pipeline::{Pipeline, fh_file_name, parse_fh_name, run_dir_name, parse_run_dir, level_dir_name};
use wx_ingest::index::{build_context, evaluate_point, SelectError};
use wx_ingest::grib2::{decode_file, ParamFilter, StatKind, RunTime, epoch_secs};
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

// Reference time of the fixture's first run: 2026-10-19T12:00:00Z.
const EP_A: i64 = 1792411200;

fn fixture(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
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

fn copy_file(src: String, dst: String) {
    let f = File::open(&src);
    match f {
        Err(_) => { panic!();}
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { panic!();}
                Ok(region) => {
                    let d = File::create(&dst);
                    match d {
                        Err(_) => { panic!();}
                        Ok(mut w) => {
                            let n = w.write_all(&region[0 .. region.len()]);
                            if n.is_err() { panic!(); }
                        }
                    }
                }
            }
        }
    }
}

fn write_text(path: &str, text: String) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!();}
        Ok(mut w) => {
            let n = w.write_all(text.as_bytes());
            if n.is_err() { panic!(); }
        }
    }
}

fn read_bytes(path: String) -> Vec<u8> {
    let f = File::open(&path);
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

fn file_exists(path: String) -> bool {
    let f = File::open(&path);
    match f {
        Err(_) => { false}
        Ok(_) => { true}
    }
}

// ---------------------------------------------------------------------------
// naming scheme: canonical filename <-> seconds, both directions
// ---------------------------------------------------------------------------

#[test]
fn fh_filename_scheme_roundtrip_and_backcompat() {
    // integral hours keep the milestone-3 names EXACTLY (backward compatible)
    assert!(fh_file_name(0) == "f000.bin");
    assert!(fh_file_name(6 * 3600) == "f006.bin");
    assert!(fh_file_name(125 * 3600) == "f125.bin");
    // >= 1000 hours widen the hour field rather than break the pattern
    assert!(fh_file_name(1000 * 3600) == "f1000.bin");
    // fractional whole minutes get m<MM>; sub-minute whole seconds add s<SS>
    assert!(fh_file_name(1800) == "f000m30.bin");
    assert!(fh_file_name(5400) == "f001m30.bin");
    assert!(fh_file_name(3661) == "f001m01s01.bin");
    // round trip every form
    let vals: [i64; 9] = [0, 1800, 3600, 5400, 21600, 3661, 360000, 7325, 1000 * 3600];
    for v in &vals {
        let name = fh_file_name(*v);
        match parse_fh_name(name.as_str()) {
            None => { println!("round-trip lost {0}", name); panic!(); }
            Some(back) => { assert!(back == *v); }
        }
    }
    // legacy names still parse exactly as before
    match parse_fh_name("f006.bin") {
        None => { panic!(); }
        Some(v) => { assert!(v == 21600); }
    }
    // strictness: non-canonical forms are IGNORED, never reinterpreted
    assert!(parse_fh_name("f001m5.bin").is_none()); // one-digit minute
    assert!(parse_fh_name("f001m300.bin").is_none()); // three-digit minute
    assert!(parse_fh_name("f001m70.bin").is_none()); // minute out of range
    assert!(parse_fh_name("f001m00s777.bin").is_none()); // three-digit second
    // `f006m00.bin` is never WRITTEN (zero components are omitted), but the
    // reader tolerates it as an alias for the same offset instead of dropping
    // data a hand-assembled store might contain.
    match parse_fh_name("f006m00.bin") {
        None => { panic!(); }
        Some(v) => { assert!(v == 21600); }
    }
    assert!(parse_fh_name("grid.bin").is_none());
    assert!(parse_fh_name("manifest.json").is_none());
}

#[test]
fn run_dir_names_carry_minutes_without_collisions() {
    let a = RunTime { year: 2026, month: 10, day: 19, hour: 12, minute: 0, second: 0, epoch_secs: 0 };
    let b = RunTime { year: 2026, month: 10, day: 19, hour: 12, minute: 30, second: 0, epoch_secs: 0 };
    let c = RunTime { year: 2026, month: 10, day: 19, hour: 12, minute: 30, second: 15, epoch_secs: 0 };
    let na = run_dir_name(a);
    let nb = run_dir_name(b);
    let nc = run_dir_name(c);
    assert!(na == "2026101912");
    assert!(nb == "202610191230");
    assert!(nc == "20261019123015");
    // the two same-day runs no longer collide, and every name round-trips to
    // the exact epoch seconds
    let e0 = epoch_secs(2026, 10, 19, 12, 0, 0);
    let e30 = epoch_secs(2026, 10, 19, 12, 30, 0);
    let e3015 = epoch_secs(2026, 10, 19, 12, 30, 15);
    match parse_run_dir(na.as_str()) {
        None => { panic!(); }
        Some(v) => { assert!(v == e0); }
    }
    match parse_run_dir(nb.as_str()) {
        None => { panic!(); }
        Some(v) => { assert!(v == e30); }
    }
    match parse_run_dir(nc.as_str()) {
        None => { panic!(); }
        Some(v) => { assert!(v == e3015); }
    }
    // pre-existing 10-char directories keep parsing (minute/second default 0)
    match parse_run_dir("2026101912") {
        None => { panic!(); }
        Some(v) => { assert!(v == e0); }
    }
    // garbage is still rejected
    assert!(parse_run_dir("202610191").is_none());
    assert!(parse_run_dir("202610191260").is_none()); // minute 60 invalid
    assert!(parse_run_dir("notarun").is_none());
}

// ---------------------------------------------------------------------------
// decoder: CT 4.4 minute unit -> whole-second offsets, stat range preserved
// ---------------------------------------------------------------------------

#[test]
fn frac_fixture_decodes_minute_units_as_seconds() {
    let r = decode_file(fixture("frac_small.grib2"), ParamFilter::All);
    match r {
        Err(e) => {
            println!("decode failed {:?}", e);
            panic!();
        }
        Ok((msgs, st)) => {
            assert!(msgs.len() == 6);
            assert!(st.messages_total == 6);
            // minute-unit forecast times are VALID data: nothing skipped
            assert!(st.messages_unsupported == 0);
            let ep_a = EP_A;
            let ep_b = ep_a + 1800; // 12:30Z
            // run A: fc 0 / 30 / 60 minutes
            assert!(msgs[0].fc_secs == 0);
            assert!(msgs[1].fc_secs == 1800);
            assert!(msgs[2].fc_secs == 3600);
            assert!(msgs[0].run.minute == 0 && msgs[0].run.epoch_secs == ep_a);
            assert!(msgs[3].run.minute == 30 && msgs[3].run.epoch_secs == ep_b);
            assert!(msgs[3].fc_secs == 0 && msgs[4].fc_secs == 1800);
            // valid times are per-second exact
            assert!(msgs[1].valid_epoch_secs == ep_a + 1800);
            // APCP accumulation over a 30-MINUTE window at fc 30 min
            let m5 = &msgs[5];
            match m5.param_name {
                Some(s) => { assert!(s == "APCP"); }
                None => { panic!(); }
            }
            assert!(m5.stat.kind == StatKind::Accumulation);
            assert!(m5.stat.begin_secs == 1800);
            assert!(m5.stat.end_secs == 3600);
            assert!(m5.fc_secs == 1800);
            // wgrib2-probed node (lat 9.0, lon 351.5 -> j=2, i=3, k=45):
            // APCP = 5 + 45 + 30 = 80; run A fc 30min = 325; run B fc 30min = 575
            assert!(m5.data[2 * 21 + 3] == 80.0);
            assert!(msgs[1].data[2 * 21 + 3] == 325.0);
            assert!(msgs[4].data[2 * 21 + 3] == 575.0);
        }
    }
}

// ---------------------------------------------------------------------------
// full pipeline: storage names, manifest, index, queries
// ---------------------------------------------------------------------------

fn frac_cfg_text(root: &str) -> String {
    let ing = join_path(root, "ingest");
    let data = join_path(root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"frac\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"frac_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n\n  [[models.params]]\n  name = \"APCP\"\n  levels = [\"*\"]\n"
}

// Fresh tree + config + fixture file. stable_window_secs = 0 means eligibility
// needs one re-observation on the NEXT scan round, which is exactly what the
// two scan_rounds below do (round 1 observes, round 2 ingests).
fn prep_frac_tree(root: String) {
    rm_tree(&root);
    let ing = join_path(&root, "ingest");
    let _c = create_dir_all(&ing);
    copy_file(fixture("frac_small.grib2"), join_path(&ing, "frac_small.grib2"));
    write_text(&join_path(&root, "config.toml"), frac_cfg_text(&root));
}

#[test]
fn fractional_hours_storage_and_manifest() {
    let root = "/tmp/opencode/wxpoint-frac".to_string();
    prep_frac_tree(root.clone());
    let po = Pipeline::open(&join_path(&root, "config.toml"));
    let mut p = match po {
        Err(e) => {
            println!("pipeline open failed: {0}", e);
            panic!();
        }
        Ok(pp) => { pp }
    };
    let n1 = p.scan_round();
    assert!(n1 == 0);
    let n2 = p.scan_round();
    assert!(n2 == 1);
    // fractional forecast hours are VALID data: no ingest errors, and nothing
    // was structurally skipped either.
    assert!(p.counters.errors == 0);
    assert!(p.counters.skipped_messages == 0);
    let data = join_path(&root, "data");

    // ---- storage layout: canonical fractional filenames ------------------
    let run_a = join_path(&join_path(&data, "frac"), "2026101912");
    let run_b = join_path(&join_path(&data, "frac"), "202610191230");
    let iso = level_dir_name(100);
    let surf = level_dir_name(1);
    assert!(file_exists(join_path(&run_a, "grid.bin")));
    assert!(file_exists(join_path(&join_path(&join_path(&run_a, "TMP"), iso.as_str()), "f000.bin")));
    assert!(file_exists(join_path(&join_path(&join_path(&run_a, "TMP"), iso.as_str()), "f000m30.bin")));
    assert!(file_exists(join_path(&join_path(&join_path(&run_a, "TMP"), iso.as_str()), "f001.bin")));
    assert!(file_exists(join_path(&join_path(&join_path(&run_b, "TMP"), iso.as_str()), "f000.bin")));
    assert!(file_exists(join_path(&join_path(&join_path(&run_b, "TMP"), iso.as_str()), "f000m30.bin")));
    assert!(file_exists(join_path(&join_path(&join_path(&run_a, "APCP"), surf.as_str()), "f000m30.bin")));
    // the 30-minute product must NOT have been rounded onto an hour slot, and
    // an INTEGRAL hour is never written with a redundant zero component.
    assert!(!file_exists(join_path(&join_path(&join_path(&run_a, "TMP"), iso.as_str()), "f001m00.bin")));
    assert!(!file_exists(join_path(&join_path(&join_path(&run_b, "TMP"), iso.as_str()), "f001.bin")));

    // ---- manifest: decimal hours + per-fh statistical range --------------
    let man = read_bytes(join_path(&run_a, "manifest.json"));
    assert!(bytes_contains(&man, "\"param\":\"TMP\"".as_bytes()));
    assert!(bytes_contains(&man, "\"forecast_hours\":[0,0.5,1]".as_bytes()));
    assert!(bytes_contains(&man, "\"stat_processed\":true".as_bytes()));
    assert!(bytes_contains(&man, "\"stat_ranges\":[{\"fh_secs\":1800,\"begin\":\"2026-10-19T12:30:00Z\",\"end\":\"2026-10-19T13:00:00Z\",\"step_secs\":0}]".as_bytes()));
    // unprocessed fields carry an empty range array, not a bogus one
    assert!(bytes_contains(&man, "\"stat_ranges\":[]".as_bytes()));

    // ---- index: two runs coexist with distinct epochs, fractional slots --
    let snap = p.shared.get_index();
    let sref = &*snap;
    match sref.find_model("frac") {
        None => { panic!(); }
        Some(m) => {
            assert!(m.runs.len() == 2);
            assert!(m.runs[0].epoch_secs + 1800 == m.runs[1].epoch_secs);
            assert!(m.runs[0].dir_name == "2026101912");
            assert!(m.runs[1].dir_name == "202610191230");
            let mut tmp_slots = 0;
            let mut apcp_range = (false, 0, 0);
            for fe in &m.runs[0].fields {
                if fe.param_key == "TMP" {
                    assert!(fe.fd.fhs.len() == 3);
                    assert!(fe.fd.fhs[0].fh_secs == 0);
                    assert!(fe.fd.fhs[1].fh_secs == 1800);
                    assert!(fe.fd.fhs[2].fh_secs == 3600);
                    tmp_slots = 3;
                }
                if fe.param_key == "APCP" {
                    assert!(fe.fd.stat_processed);
                    assert!(fe.fd.fhs.len() == 1);
                    let sl = &fe.fd.fhs[0];
                    assert!(sl.stat_has_range);
                    apcp_range = (sl.stat_has_range, sl.stat_begin_secs, sl.stat_end_secs);
                    assert!(sl.fh_secs == 1800);
                }
            }
            assert!(tmp_slots == 3);
            assert!(apcp_range.1 == 1800 && apcp_range.2 == 3600);
        }
    }
    rm_tree(&root);
}

#[test]
fn fractional_hours_queries_and_run_selection() {
    let root = "/tmp/opencode/wxpoint-frac-q".to_string();
    prep_frac_tree(root.clone());
    let po = Pipeline::open(&join_path(&root, "config.toml"));
    let mut p = match po {
        Err(e) => {
            println!("pipeline open failed: {0}", e);
            panic!();
        }
        Ok(pp) => { pp }
    };
    let n1 = p.scan_round();
    assert!(n1 == 0);
    let n2 = p.scan_round();
    assert!(n2 == 1);
    assert!(p.counters.errors == 0);

    let ep_a: i64 = EP_A;
    let ep_b = ep_a + 1800;
    let snap = p.shared.get_index();
    let sref = &*snap;

    match sref.find_model("frac") {
        None => { panic!(); }
        Some(m) => {
            match lookup_level_type("isobaric") {
                None => { panic!(); }
                Some(iso_code) => {
                    // exact fractional forecast hour: run A + 30 min at node
                    // (lat 9.0, lon 351.5) -> 250 + 45 + 30 = 325 exactly, no
                    // blending of any kind.
                    let cx = build_context(sref, "frac",
                        Some(m.runs[0].epoch_secs), "TMP", iso_code, 850.0, 1800.0,
                        TimeInterpMode::Linear);
                    match cx {
                        Err(se) => {
                            println!("build failed {:?}", se);
                            panic!();
                        }
                        Ok(c) => {
                            let v = evaluate_point(&c, 9.0, 351.5);
                            match v {
                                Err(pe) => {
                                    println!("eval failed {:?}", pe);
                                    panic!();
                                }
                                Ok(pv) => {
                                    assert!(pv.value == 325.0);
                                    assert!(pv.info.horizontal == HorizMethod::Exact);
                                    // no time blend expected:
                                    if pv.info.time.is_some() { panic!(); }
                                }
                            }
                        }
                    }
                    // mid-bracket between f000m30 and f001m00 at fc 45 min:
                    // linear in the minute coordinate -> 340, bracket reported
                    // back in HOURS per spec §7.
                    let cb = build_context(sref, "frac",
                        Some(m.runs[0].epoch_secs), "TMP", iso_code, 850.0, 2700.0,
                        TimeInterpMode::Linear);
                    match cb {
                        Err(_) => { panic!(); }
                        Ok(c) => {
                            let v = evaluate_point(&c, 9.0, 351.5);
                            match v {
                                Ok(pv) => {
                                    assert!(pv.value == 340.0);
                                    match pv.info.time {
                                        None => { panic!(); }
                                        Some(tm) => {
                                            assert!(tm.forecast_hours.0 == 0.5);
                                            assert!(tm.forecast_hours.1 == 1.0);
                                        }
                                    }
                                }
                                Err(pe) => {
                                    println!("eval failed {:?}", pe);
                                    panic!();
                                }
                            }
                        }
                    }
                    // time_interp=none at a NON-stored fractional fh is rejected
                    let cn = build_context(sref, "frac",
                        Some(m.runs[0].epoch_secs), "TMP", iso_code, 850.0, 2700.0,
                        TimeInterpMode::None);
                    match cn {
                        Err(ne) => {
                            match ne {
                                SelectError::TimeExactOnly => {}
                                _ => {
                                    println!("unexpected select error {:?}", ne);
                                    panic!();
                                }
                            }
                        }
                        Ok(_) => { panic!(); }
                    }
                    // accumulation guard still fires for a FRACTIONAL range: APCP
                    // at fc 45 min must be refused, at fc 30 min answered 80.
                    let cg = build_context(sref, "frac",
                        Some(m.runs[0].epoch_secs), "APCP", 1, 0.0, 2700.0,
                        TimeInterpMode::Linear);
                    match cg {
                        Ok(_) => { panic!(); }
                        Err(g) => {
                            match g {
                                SelectError::StatTimeGuard(..) => {}
                                _ => {
                                    println!("unexpected select error {:?}", g);
                                    panic!();
                                }
                            }
                        }
                    }
                    let ce = build_context(sref, "frac",
                        Some(m.runs[0].epoch_secs), "APCP", 1, 0.0, 1800.0,
                        TimeInterpMode::Linear);
                    match ce {
                        Err(_) => { panic!(); }
                        Ok(c) => {
                            let v = evaluate_point(&c, 9.0, 351.5);
                            match v {
                                Ok(pv) => { assert!(pv.value == 80.0); }
                                Err(pe) => {
                                    println!("eval failed {:?}", pe);
                                    panic!();
                                }
                            }
                        }
                    }
                    // ---- forecast-hour offsets are PER-RUN ----------------
                    // valid 13:00Z is 60 min past run A but only 30 min past
                    // run B, so the SAME wall-clock instant resolves to run B's
                    // `f000m30.bin` slot (575) or run A's `f001.bin` slot (355)
                    // depending on the run. The server recomputes the offset for
                    // every candidate run and prefers the NEWEST one that covers
                    // the valid time (spec §6.3); that walk lives in
                    // queryapi.answer_param and is exercised in the wx-server
                    // fractional-hour test.
                    let cb_now = build_context(sref, "frac",
                        Some(m.runs[1].epoch_secs), "TMP", iso_code, 850.0, 1800.0,
                        TimeInterpMode::Linear);
                    match cb_now {
                        Err(_) => { panic!(); }
                        Ok(c) => {
                            assert!(c.run_epoch_secs == ep_b);
                            let v = evaluate_point(&c, 9.0, 351.5);
                            match v {
                                Ok(pv) => { assert!(pv.value == 575.0); }
                                Err(_) => { panic!(); }
                            }
                        }
                    }
                    // pinning run A answers from run A's f001m00 slot instead
                    let cp = build_context(sref, "frac",
                        Some(ep_a), "TMP", iso_code, 850.0, 3600.0, TimeInterpMode::Linear);
                    match cp {
                        Err(_) => { panic!(); }
                        Ok(c) => {
                            assert!(c.run_epoch_secs == ep_a);
                            let v = evaluate_point(&c, 9.0, 351.5);
                            match v {
                                Ok(pv) => { assert!(pv.value == 355.0); }
                                Err(_) => { panic!(); }
                            }
                        }
                    }
                    // 13:15Z is beyond BOTH runs' last stored slot (run A tops
                    // out at f001m00, run B at f000m30): no run covers it.
                    let cno = build_context(sref, "frac",
                        None, "TMP", iso_code, 850.0, 4500.0, TimeInterpMode::Linear);
                    match cno {
                        Err(ne) => {
                            match ne {
                                SelectError::NoRunCovers => {}
                                _ => {
                                    println!("unexpected select error {:?}", ne);
                                    panic!();
                                }
                            }
                        }
                        Ok(_) => { panic!(); }
                    }
                }
            }
        }
    }
    rm_tree(&root);
}
