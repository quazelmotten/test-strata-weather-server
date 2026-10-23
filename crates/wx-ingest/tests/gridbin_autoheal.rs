// V1.4 phase B2 DEFECT GATE #1 — grid.bin AUTO-HEAL within <=2 SCAN ROUNDS.
//
// THE DEFECT (AUDIT_1.3 P1, reproduced live): a run dir whose grid.bin goes
// MISSING stays invisible FOREVER in a static ingest dir. The E3b note promised
// "until its source re-delivers it", but the source file keeps its kind=OK
// state record, so scan_round skips it as unchanged on every later round and
// NOTHING ever re-delivers anything: the orphan never heals, /v1/inventory
// never shows the run, and every restart repeats the same dead promise.
//
// THE FIX: when a run dir fails to load because grid.bin is missing/unreadable,
// FORGET the model's (model,path) state records under its ingest dir (same
// idempotent-re-decode machinery the storage-format recovery trap uses), and
// when grid.bin EXISTS but cannot be READ, remove that husk FIRST (the cold
// writer emits grid.bin only when stat says absent, so an unreadable husk
// would block the rewrite forever). The next scan round re-decodes from source
// and atomically REWRITES grid.bin byte-identically (encode_grid is a pure
// function of the decoded GridDef). The run tree itself is KEPT (field .bin
// headers are durable + validated at open — stat_durability.rs gates), and the
// heal is logged LOUDLY.
//
// PRE-FIX DISCRIMINATORS (all three fail on b30d156):
//   * state.find("gfs", src) is still Some(REC_OK) after boot  -> RED
//   * the run never becomes visible no matter how many rounds  -> RED
//   * no GRID AUTO-HEAL note exists                            -> RED

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode, HorizMethod};
use wx_core::tables::{lookup_level_type};
use wx_ingest::state::{join_path, REC_OK};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

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

#[allow(clippy::needless_return)]
fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for k in 0 .. a.len() {
        if a[k] != b[k] {
            return false;
        }
    }
    return true;
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

#[allow(clippy::needless_return)]
fn gh_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn missing_grid_bin_heals_within_two_rounds_and_rewrites_it_byte_identical() {
    let root = "/tmp/opencode/wxpoint-gridbin-autoheal";
    rm_tree(root);
    let ing = join_path(root, "ing");
    let data = join_path(root, "data");
    let _c = create_dir_all(&ing);
    let src = join_path(&ing, "gfs_small.grib2");
    write_bytes(&src, &read_bytes("../../tests/fixtures/gfs_small.grib2"));
    let cfg_path = join_path(root, "config.toml");
    write_bytes(&cfg_path, gh_cfg(&data, &ing).as_bytes());

    // ---- life 1: healthy ingest, keep a golden copy of grid.bin -----------
    let p0_r = Pipeline::open(&cfg_path);
    let mut p0 = match p0_r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(p0.scan_round() == 0);
    assert!(p0.scan_round() == 1);
    let gp = format!("{0}/gfs/2026101912/grid.bin", data);
    let golden = read_bytes(&gp);
    assert!(!golden.is_empty());
    drop(p0);

    // ---- the AUDIT_1.3 P1 scenario: ONLY grid.bin vanishes ----------------
    let rf = remove_file(&gp);
    if rf.is_err() { panic!(); }

    // ---- restart: loud heal note + records FORGOTTEN at open --------------
    let p1_r = Pipeline::open(&cfg_path);
    let mut p1 = match p1_r {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    // The E3b loudness contract survives (one MISSING GRID note, one error)...
    assert!(count_notes(&p1, "MISSING GRID") == 1);
    assert!(p1.counters.errors == 1);
    // ...and B2 adds the actual HEAL: a loud AUTO-HEAL note naming the forget.
    assert!(count_notes(&p1, "GRID AUTO-HEAL") >= 1);
    // THE RED DISCRIMINATOR: the REC_OK record that kept the source file
    // "unchanged" forever is GONE, so the next scan MUST re-decode it.
    if p1.state.find("gfs", src.as_str()).is_some() { panic!(); }
    // Still invisible AT OPEN (the rebuild cannot index what it cannot read).
    {
        let snap = p1.shared.get_index();
        if let Some(m) = snap.find_model("gfs") {
            assert!(m.runs.is_empty());
        }
    }

    // ---- <=2 scan rounds and the run answers queries again -----------------
    // Round 1 observes stability (fresh process, window 0 still needs the
    // second observation), round 2 re-decodes and atomically rewrites grid.bin.
    let mut healed_rounds = 0usize;
    let mut visible = false;
    while healed_rounds < 2usize && !visible {
        p1.scan_round();
        healed_rounds += 1;
        let snap = p1.shared.get_index();
        match snap.find_model("gfs") {
            None => {}
            Some(m) => {
                visible = m.runs.len() == 1 && !m.runs[0].fields.is_empty();
            }
        }
    }
    assert!(visible); // pre-fix: NEVER happens in a static ingest dir -> RED
    assert!(healed_rounds <= 2usize);

    // grid.bin is back and BYTE-IDENTICAL to the golden bytes.
    let now_gb = read_bytes(&gp);
    assert!(bytes_eq(&golden, &now_gb));

    // Queryable with the analytic answer restart.rs pins for this fixture.
    {
        let snap = p1.shared.get_index();
        let sref = &*snap;
        match lookup_level_type("isobaric") {
            None => { panic!(); }
            Some(iso) => {
                let c_r = build_context(sref, "gfs", None, "TMP", iso,
                                        1000.0, 0.0, TimeInterpMode::Linear);
                match c_r {
                    Err(_) => { panic!(); }
                    Ok(c) => {
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

    // The healed state record says ingested again, and the store settled: a
    // third round does nothing (no perpetual re-decode loop).
    match p1.state.find("gfs", src.as_str()) {
        None => { panic!(); }
        Some(rc) => {
            assert!(rc.kind == REC_OK);
        }
    }
    assert!(p1.scan_round() == 0);
    rm_tree(root);
}
