// V1.3 phase E1 DEFECT GATE — PER-FORECAST-HOUR statistical guards.
//
// THE DEFECT (AUDIT-confirmed (2)): the §6.3 time-interpolation guard was
// decided from ONE FIELD-LEVEL flag (`FieldDef.stat_processed`), which came
// from whichever forecast hour REGISTERED FIRST. A stack mixing an
// instantaneous fh with an accumulated one therefore answered DIFFERENTLY
// depending on message order:
//   * instantaneous registered first -> field flag FALSE -> mid-time queries
//     SILENTLY LINEAR-INTERPOLATED across the accumulation (a plausible,
//     WRONG number);
//   * accumulation registered first -> field flag TRUE -> even brackets
//     between two CLEAN instantaneous hours were refused.
// Post-E1 the guard travels PER FORECAST HOUR (each .bin header carries its
// own state; IdxFh/RegFh carry `stat_processed` + `stat_kind_code`) and the
// decision uses ONLY the slots the answer would blend:
//   * EXACT hit -> that slot's own state (always answers);
//   * INTERPOLATION -> BOTH bracket endpoints must be instantaneous;
//   * no bracket at all while ANY slot guards -> fail closed, name the first
//     guarded slot (preserves the uniform-stack behavior incl. single-slot
//     accumulations, see frac_small APCP in fractional_fh.rs).
//
// FIXTURES (scripts/gen_grib2.py): mixstat_small.grib2 = TMP@2m AGL fh
// 0 INSTANTANEOUS, fh 6 ACCUMULATION with the 0-6 h window that ENDS at
// that forecast hour (wgrib2 "0-6 hour acc(6 hour fcst)"; AUDIT_1.3 P2
// corrected the decoder, which used to store the length-shifted [6,12] h),
// fh 12 + fh 18
// INSTANTANEOUS, values analytic and linear in the forecast hour:
//     value = 250 + j*nx + i + fc_hour
// so the node (20 N, 240 E) reads 250 / 256 / 262 / 268 and mid fh 15
// between the CLEAN bracket [12, 18] is exactly 265. mixstat_rev_small.grib2
// carries the SAME four messages in REVERSE order. Pre-fix the two orders
// disagreed on mid-time answers; post-fix they MUST agree (order
// independence is the discriminator in both directions: forward order proves
// the guard fires where pre-fix blending happened, reverse order proves
// clean brackets still answer where pre-fix over-blocking happened).

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point, SelectError};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_ingest::state::join_path;
use wx_ingest::discovery::{path_to_str};
use wx_core::tables::{lookup_level_type};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

// ---------------------------------------------------------------------------
// helpers (same shapes as stat_durability.rs)
// ---------------------------------------------------------------------------

#[allow(clippy::needless_return)]
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

#[allow(clippy::needless_return)]
fn read_bytes(path: &str) -> Vec<u8> {
    let f = File::open(path);
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
    let d = File::create(path);
    match d {
        Err(_) => { panic!(); }
        Ok(mut w) => {
            let n = w.write_all(bytes);
            if n.is_err() { panic!(); }
        }
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

fn mix_cfg(model: &str, file: &str, data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"";
    let t = format!("{0}{1}", t, model);
    let t = t + "\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"";
    let t = format!("{0}{1}", t, file);
    
    t + "\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n"
}

// ---------------------------------------------------------------------------
// shared probe machinery
// ---------------------------------------------------------------------------

// Answer value at (20 N, 240 E) on TMP@2m AGL for a wanted fh offset in
// seconds; Err carries the FULL guard payload so the tests can pin WHICH
// bracket and WHICH forecast hour triggered it.
#[allow(clippy::needless_return)]
fn probe(sref: &wx_ingest::index::IndexSnapshot, model: &str, want_fh: f64)
    -> Result<f64, (i64, i64, i64, u8)> {
    let agl = lookup_level_type("height_above_ground");
    match agl {
        None => { panic!(); }
        Some(code) => {
            let c = build_context(sref, model, None, "TMP", code, 2.0, want_fh,
                                  TimeInterpMode::Linear);
            match c {
                Err(e) => {
                    match e {
                        SelectError::StatTimeGuard(a, b, o, k) => {
                            return Err((a, b, o, k));
                        }
                        _ => {
                            println!("unexpected select error {:?}", e);
                            panic!();
                        }
                    }
                }
                Ok(ctx) => {
                    let v = evaluate_point(&ctx, 20.0, 240.0);
                    match v {
                        Err(we) => {
                            println!("unexpected eval error {:?}", we);
                            panic!();
                        }
                        Ok(qr) => {
                            // the probe sits EXACTLY on grid node (i=0, j=0)
                            assert!(qr.info.horizontal == HorizMethod::Exact);
                            return Ok(qr.value);
                        }
                    }
                }
            }
        }
    }
}

// Pin the per-fh statistical flags as they must appear in the published
// index snapshot, IDENTICALLY for both message orders (warm==cold==any-order
// determinism was half of defect (2)).
#[allow(clippy::needless_return)]
fn check_stack_flags(sref: &wx_ingest::index::IndexSnapshot, model: &str) {
    let m = sref.find_model(model);
    match m {
        None => { panic!(); }
        Some(mm) => {
            assert!(mm.runs.len() == 1);
            let mut found = false;
            for fe in &mm.runs[0].fields {
                if fe.param_key != "TMP" {
                    continue;
                }
                found = true;
                assert!(fe.fd.fhs.len() == 4);
                for sl in &fe.fd.fhs {
                    if sl.fh_secs == 21600 {
                        // the ONE accumulated slot keeps its own truth...
                        assert!(sl.stat_processed);
                        assert!(sl.stat_kind_code == 1); // STAT_ACCUMULATION
                    } else {
                        // ...and the instantaneous slots are NOT tainted by
                        // their neighbour (pre-fix the field-level flag made
                        // this order-dependent).
                        assert!(!sl.stat_processed);
                        assert!(sl.stat_kind_code == 0);
                    }
                }
                // FIELD-LEVEL flags are inventory display only since E1, and
                // recomputed deterministically: TRUE because SOME fh guards,
                // naming the FIRST guarding fh's kind — regardless of which
                // message registered first (pre-fix forward order showed
                // FALSE here while reverse order showed TRUE: the coin flip
                // the audit called out).
                assert!(fe.fd.stat_processed);
                assert!(fe.fd.stat_kind == "accumulation");
            }
            assert!(found);
        }
    }
}

// ---------------------------------------------------------------------------
// (a) INSTANTANEOUS registered FIRST (the pre-fix silent-blend order)
// ---------------------------------------------------------------------------

#[test]
fn mixed_stack_instantaneous_first_guards_per_forecast_hour() {
    let root = "/tmp/opencode/wxpoint-mixstat-inst".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "mixstat_small.grib2"),
                &read_bytes("../../tests/fixtures/mixstat_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, mix_cfg("mix", "mixstat_small.grib2", &data, &ing).as_bytes());

    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);
    assert!(p1.counters.errors == 0);

    let snap = p1.shared.get_index();
    let sref = &*snap;
    check_stack_flags(sref, "mix");

    // EXACT hits answer from their OWN slot, guarded or not (spec §6.3 asks
    // for exactness, not absence): fh 0 -> 250, fh 6 (the accumulation) ->
    // 256. These pass pre-fix too; they pin that the refactor did not break
    // exactness.
    match probe(sref, "mix", 0.0) {
        Err(_) => { panic!(); }
        Ok(v) => { assert!(v == 250.0); }
    }
    match probe(sref, "mix", 21600.0) {
        Err(_) => { panic!(); }
        Ok(v) => { assert!(v == 256.0); }
    }

    // MID fh 3: bracket [0 h, 6 h] where fh 6 is an ACCUMULATION. Pre-fix the
    // field flag came from fh 0 (instantaneous, registered first) and this
    // answered 200 with the SILENT BLEND 253.0 — the wrong number the audit
    // flagged. Post-fix: refused, naming the offending hour + kind.
    match probe(sref, "mix", 10800.0) {
        Ok(_v) => { panic!(); }
        Err(d) => {
            assert!(d.0 == 0);       // bracket low  = fh 0 h
            assert!(d.1 == 21600);   // bracket high = fh 6 h
            assert!(d.2 == 21600);   // offender     = fh 6 h
            assert!(d.3 == 1);       // accumulation
        }
    }

    // MID fh 9: bracket [6 h, 12 h] — same accumulation on the LOW side this
    // time; still refused, offender still fh 6.
    match probe(sref, "mix", 32400.0) {
        Ok(_v) => { panic!(); }
        Err(d) => {
            assert!(d.0 == 21600);
            assert!(d.1 == 43200);
            assert!(d.2 == 21600);
            assert!(d.3 == 1);
        }
    }

    // MID fh 15: bracket [12 h, 18 h], BOTH endpoints instantaneous -> the
    // guard must NOT over-block: answers the analytic midpoint exactly.
    match probe(sref, "mix", 54000.0) {
        Err(_) => { panic!(); }
        Ok(v) => { assert!(v == 265.0); }
    }

    // OUT OF RANGE fh 22: nothing brackets it, but the stack HOLDS a guarded
    // slot -> fail closed with the guard naming the first guarded hour
    // (bracket reported as -1/-1: there is none to blend).
    match probe(sref, "mix", 79200.0) {
        Ok(_v) => { panic!();}
        Err(d) => {
            assert!(d.0 == -1);
            assert!(d.1 == -1);
            assert!(d.2 == 21600);
            assert!(d.3 == 1);
        }
    }
}

// ---------------------------------------------------------------------------
// (b) ACCUMULATION registered FIRST (the pre-fix over-block order): the
//     answers MUST be identical to (a) — order independence.
// ---------------------------------------------------------------------------

#[test]
fn mixed_stack_accumulation_first_answers_clean_brackets() {
    let root = "/tmp/opencode/wxpoint-mixstat-rev".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "mixstat_rev_small.grib2"),
                &read_bytes("../../tests/fixtures/mixstat_rev_small.grib2"));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, mix_cfg("mixr", "mixstat_rev_small.grib2", &data, &ing).as_bytes());

    let p1r = Pipeline::open(&cfg_path);
    let mut p1 = match p1r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(settle(&mut p1, 1usize) == 1usize);
    assert!(p1.counters.errors == 0);

    let snap = p1.shared.get_index();
    let sref = &*snap;
    check_stack_flags(sref, "mixr");

    // THE DISCRIMINATOR FOR THIS ORDER: mid fh 15 between two CLEAN hours.
    // Pre-fix the field flag came from the ACCUMulation registered first and
    // this was WRONGLY refused (422); post-fix it answers exactly 265.
    match probe(sref, "mixr", 54000.0) {
        Err(_) => { panic!(); }
        Ok(v) => { assert!(v == 265.0); }
    }

    // Mid fh 3 / fh 9: guarded brackets refuse with payloads IDENTICAL to
    // the forward-order test above (order independence, per-fh truth).
    match probe(sref, "mixr", 10800.0) {
        Ok(_v) => { panic!(); }
        Err(d) => {
            assert!(d.0 == 0);
            assert!(d.1 == 21600);
            assert!(d.2 == 21600);
            assert!(d.3 == 1);
        }
    }
    match probe(sref, "mixr", 32400.0) {
        Ok(_v) => { panic!(); }
        Err(d) => {
            assert!(d.0 == 21600);
            assert!(d.1 == 43200);
            assert!(d.2 == 21600);
            assert!(d.3 == 1);
        }
    }

    // Exact hits unaffected by order.
    match probe(sref, "mixr", 0.0) {
        Err(_) => { panic!(); }
        Ok(v) => { assert!(v == 250.0); }
    }
    match probe(sref, "mixr", 21600.0) {
        Err(_) => { panic!();}
        Ok(v) => { assert!(v == 256.0); }
    }
}
