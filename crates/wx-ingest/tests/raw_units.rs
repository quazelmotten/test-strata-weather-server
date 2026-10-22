// V1.3 phase E2 DEFECT GATE — UNITS OVERRIDE for raw GRIB2 params.
//
// THE DEFECT (AUDIT-confirmed): a [[models.params]] entry given as a raw
// discipline/category/number triple OUTSIDE the built-in table resolved to
// units "" and the config offered NO override, so spec §7 ("return values in
// native GRIB units ... AND report the units") could not be satisfied for
// such params — responses carried an empty units string with no way to fix
// it from config.
//
// THE FIX (orchestrator decision, docs/DECISIONS.md V1.3 E2): an OPTIONAL
// `units = "..."` key on [[models.params]], legal ONLY on out-of-table raw
// triples. Setting it on anything the built-in table already resolves (by
// name OR by an in-table triple) is a HARD startup error naming the param —
// chosen over warn+override because silently replacing known canonical units
// invites wrong-unit answers. A raw param WITHOUT units still boots (the
// operator may genuinely not know them) but emits ONE loud note per affected
// entry listing the exact fix. Units flow into FieldDef / manifest /
// /v1/models / point responses exactly like table-derived units.
//
// FIXTURE raw_small.grib2: ONE message discipline 0 / category 19 / number 5
// (outside the table -> key d0c19n5), surface level, fh 0, node (10 N, 350 E)
// value 42. Pre-E2 there was NO `units` config key at all — the serde struct
// rejected nothing but ignored nothing either: the validation assertions below
// did not exist, and the with-units answer assertion failed because units
// were always "".

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode};
use wx_ingest::state::join_path;
use wx_ingest::discovery::{path_to_str};
use wx_ingest::config::{validate_text};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

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
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(bytes);
            match n {
                Err(_) => { assert!(false); return; }
                Ok(_) => {}
            }
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

#[allow(clippy::needless_return)]
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

#[allow(clippy::needless_return)]
fn count_notes(p: &Pipeline, pat: &str) -> usize {
    let mut n = 0usize;
    for line in &p.log {
        if bytes_contains(line.as_bytes(), pat.as_bytes()) {
            n += 1;
        }
    }
    return n;
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

// One model over raw_small.grib2 with the out-of-table triple; `units_line`
// carries the optional override line ("units = \"kg/m^2\"\n") or "".
#[allow(clippy::needless_return)]
fn raw_cfg(model: &str, data: &str, ing: &str, units_line: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"";
    let t = format!("{0}{1}", t, model);
    let t = t + "\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"raw_small.grib2\"\n\n  [[models.params]]\n  discipline = 0\n  category = 19\n  number = 5\n  levels = [\"surface\"]\n  ";
    let t = t + units_line;
    return t;
}

// ---------------------------------------------------------------------------
// (a) WITH the override: units flow into index/manifest/answers; no note
// ---------------------------------------------------------------------------

#[test]
fn raw_param_with_units_override_answers_with_them() {
    let root = "/tmp/opencode/wxpoint-raw-units".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "raw_small.grib2"),
                &read_bytes("../../tests/fixtures/raw_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, raw_cfg("rawu", &data, &ing, "units = \"kg/m^2\"\n").as_bytes());

    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(e) => { println!("open failed: {e}"); assert!(false); return; }
        Ok(pp) => { pp }
    };
    // LOUD NOTE SUPPRESSED when units are supplied.
    assert!(count_notes(&p1, "NO units override") == 0);
    assert!(settle(&mut p1, 1usize) == 1usize);
    assert!(p1.counters.errors == 0);

    let snap = p1.shared.get_index();
    let sref = &*snap;
    let m = sref.find_model("rawu");
    match m {
        None => { assert!(false); return; }
        Some(mm) => {
            let mut found = false;
            for fe in &mm.runs[0].fields {
                if fe.param_key != "d0c19n5" {
                    continue;
                }
                found = true;
                // THE FIX: units reach FieldDef (-> /v1/models, inventory and
                // every point response) instead of the pre-E2 empty string.
                assert!(fe.fd.units == "kg/m^2".to_string());
            }
            assert!(found);
        }
    }

    // answer at node (10 N, 350 E) = 42, carrying the configured units.
    let c = build_context(sref, "rawu", None, "d0c19n5", 1, 0.0, 0.0, TimeInterpMode::Linear);
    match c {
        Err(_) => { assert!(false); return; }
        Ok(ctx) => {
            let v = evaluate_point(&ctx, 10.0, 350.0);
            match v {
                Err(_) => { assert!(false); return; }
                Ok(qr) => { assert!(qr.value == 42.0); }
            }
        }
    }

    // manifest inventory carries them too.
    let man = join_path(&join_path(&join_path(&data, "rawu"), "2026101912"), "manifest.json");
    assert!(exists_any(&man));
    assert!(bytes_contains(&read_bytes(&man), b"kg/m^2"));
}

// ---------------------------------------------------------------------------
// (b) WITHOUT the override: boots, answers, but says so LOUDLY once
// ---------------------------------------------------------------------------

#[test]
fn raw_param_without_units_boots_with_loud_note() {
    let root = "/tmp/opencode/wxpoint-raw-nounits".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "raw_small.grib2"),
                &read_bytes("../../tests/fixtures/raw_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, raw_cfg("rawn", &data, &ing, "").as_bytes());

    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    // exactly ONE startup note, naming the param AND the fix...
    assert!(count_notes(&p1, "NO units override") == 1);
    assert!(count_notes(&p1, "d0c19n5") == 1);
    // ...emitted at BOOT (before any ingest round ran).
    assert!(p1.counters.ingested_files == 0);
    assert!(settle(&mut p1, 1usize) == 1usize);
    assert!(p1.counters.errors == 0);
    // not re-noted per scan round:
    assert!(count_notes(&p1, "NO units override") == 1);

    let snap = p1.shared.get_index();
    let sref = &*snap;
    let m = sref.find_model("rawn");
    match m {
        None => { assert!(false); return; }
        Some(mm) => {
            let mut found = false;
            for fe in &mm.runs[0].fields {
                if fe.param_key != "d0c19n5" {
                    continue;
                }
                found = true;
                assert!(fe.fd.units == "".to_string());
            }
            assert!(found);
        }
    }
    let c = build_context(sref, "rawn", None, "d0c19n5", 1, 0.0, 0.0, TimeInterpMode::Linear);
    match c {
        Err(_) => { assert!(false); return; }
        Ok(ctx) => {
            let v = evaluate_point(&ctx, 10.0, 350.0);
            match v {
                Err(_) => { assert!(false); return; }
                Ok(qr) => { assert!(qr.value == 42.0); }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (c) VALIDATION: the key is legal ONLY on out-of-table raw triples
// ---------------------------------------------------------------------------

#[allow(clippy::needless_return)]
fn param_text(body: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"/tmp/opencode/wxpoint-raw-units-val-data\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\n\n[[models]]\nname = \"m\"\ningest_dir = \"/tmp/opencode/wxpoint-raw-units-val-ing\"\nfile_glob = \"x.grib2\"\n\n  [[models.params]]\n  ";
    let t = format!("{0}{1}", t, body);
    let t = t + "  levels = [\"surface\"]\n";
    return t;
}

#[test]
fn units_override_validation_matrix() {
    // named table param + units -> HARD error naming the param
    let t1 = param_text("name = \"TMP\"\n  units = \"K\"\n");
    match validate_text(&t1) {
        Ok(_) => { assert!(false); return; }
        Err(e) => {
            assert!(bytes_contains(e.as_bytes(), b"units override is only valid"));
            assert!(bytes_contains(e.as_bytes(), b"'TMP'"));
        }
    }
    // raw triple that IS in the table (0/0/0 = TMP) + units -> HARD error
    // naming the resolved canonical param too
    let t2 = param_text("discipline = 0\n  category = 0\n  number = 0\n  units = \"K\"\n");
    match validate_text(&t2) {
        Ok(_) => { assert!(false); return; }
        Err(e) => {
            assert!(bytes_contains(e.as_bytes(), b"units override is only valid"));
            assert!(bytes_contains(e.as_bytes(), b"'TMP'"));
        }
    }
    // out-of-table raw triple + units -> ACCEPTED with those units
    let t3 = param_text("discipline = 0\n  category = 19\n  number = 5\n  units = \"kg/m^2\"\n");
    match validate_text(&t3) {
        Err(e) => { println!("unexpected reject: {e}"); assert!(false); return; }
        Ok(ms) => {
            assert!(ms[0].params[0].key == "d0c19n5".to_string());
            assert!(ms[0].params[0].units == "kg/m^2".to_string());
            assert!(ms[0].params[0].raw);
        }
    }
    // out-of-table raw triple WITHOUT units -> accepted, units "", raw flag set
    let t4 = param_text("discipline = 0\n  category = 19\n  number = 5\n");
    match validate_text(&t4) {
        Err(_) => { assert!(false); return; }
        Ok(ms) => {
            assert!(ms[0].params[0].units == "".to_string());
            assert!(ms[0].params[0].raw);
        }
    }
    // named param without units -> unchanged behavior, raw flag FALSE
    let t5 = param_text("name = \"TMP\"\n");
    match validate_text(&t5) {
        Err(_) => { assert!(false); return; }
        Ok(ms) => {
            assert!(!ms[0].params[0].raw);
            assert!(ms[0].params[0].units == "K".to_string());
        }
    }
}
