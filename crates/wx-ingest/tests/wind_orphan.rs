// V1.2 phase B gate #6: WIND PAIRING IS SCOPED TO ONE FILE.
//
// Three LCC models share one pipeline (config order matters because the pair-rule
// violation is STICKY for the whole Pipeline, exactly as in V1.1):
//   * `hrrr`    gets ONLY `tests/fixtures/hrrr_orphan.grib2` (TMP x2 + UGRD x2,
//     NO VGRD): every pending component left over at END OF FILE is an ORPHAN ->
//     noted, counted as a SKIP, REMOVED, NEVER stored half-rotated.
//   * `hrrrb`   gets the FULL `hrrr_small.grib2`: pairing must still work (the
//     orphan map from the previous file must NOT leak), and the stored components
//     carry winds_grid=false (earth-relative, spec §4.4).
//   * `hrrrc`   configures UGRD WITHOUT VGRD: the decode-time pair rule fires
//     ONCE (ONE error), and every grid-relative message afterwards is a SKIP with
//     at most ONE note PER FILE (no log spam on long deliveries).
//
// Single test in this binary: counter assertions are process-wide.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path, stat_size_mtime};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

fn fixture(name: &str) -> String {
    return format!("../../tests/fixtures/{name}");
}

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

fn write_bytes(path: &str, bytes: &[u8]) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(bytes);
            match n {
                Err(_) => { assert!(false); }
                Ok(_) => {}
            }
        }
    }
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

fn count_notes(p: &Pipeline, marker: &str) -> usize {
    let mut hits = 0;
    for l in &p.log {
        if l.contains(marker) {
            hits += 1;
        }
    }
    return hits;
}

/// TMP + BOTH wind components configured: legal for a grid-relative grid.
fn pair_ok_block(name: &str, ing: &str, glob: &str) -> String {
    let t = "\n[[models]]\nname = \"";
    let t = format!("{0}{1}", t, name);
    let t = t + "\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"";
    let t = format!("{0}{1}", t, glob);
    let t = t + "\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"height_above_ground:10\"]\n";
    return t;
}

/// TMP + UGRD only: ILLEGAL for a grid-relative grid (pair rule).
fn pair_bad_block(name: &str, ing: &str, glob: &str) -> String {
    let t = "\n[[models]]\nname = \"";
    let t = format!("{0}{1}", t, name);
    let t = t + "\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"";
    let t = format!("{0}{1}", t, glob);
    let t = t + "\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n";
    return t;
}

#[test]
fn orphan_wind_components_are_drained_per_file() {
    let root = "/tmp/opencode/wxpoint-windorphan".to_string();
    rm_tree(&root);
    let a_dir = join_path(&root, "ing_a");
    let b_dir = join_path(&root, "ing_b");
    let c_dir = join_path(&root, "ing_c");
    let data = join_path(&root, "data");
    let _c1 = create_dir_all(&a_dir);
    let _c2 = create_dir_all(&b_dir);
    let _c3 = create_dir_all(&c_dir);

    // hrrr_orphan.grib2 = the first FOUR messages of hrrr_small.grib2
    // (TMP f000/f003 + UGRD f000/f003): its VGRD partner never arrives.
    write_bytes(&join_path(&a_dir, "o1.grib2"),
                &read_bytes(&fixture("hrrr_orphan.grib2")));
    write_bytes(&join_path(&b_dir, "o2.grib2"),
                &read_bytes(&fixture("hrrr_small.grib2")));
    write_bytes(&join_path(&c_dir, "v1.grib2"),
                &read_bytes(&fixture("hrrr_small.grib2")));
    write_bytes(&join_path(&c_dir, "v2.grib2"),
                &read_bytes(&fixture("hrrr_small_c3.grib2")));

    let cfg_path = join_path(&root, "config.toml");
    let head = "[server]\nbind = \"b\"\ndata_dir = \"";
    let head = format!("{0}{1}", head, data);
    let head = head + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n";
    let mut cfg = head.to_string();
    cfg = cfg + pair_ok_block("hrrr", &a_dir, "o1.grib2").as_str();
    cfg = cfg + pair_ok_block("hrrrb", &b_dir, "o2.grib2").as_str();
    cfg = cfg + pair_bad_block("hrrrc", &c_dir, "v*.grib2").as_str();
    write_bytes(&cfg_path, cfg.as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    assert!(p.scan_round() == 0);   // observation round
    assert!(p.scan_round() == 4);   // all four files consumed

    // ---- ORPHANS: noted, skipped, removed ---------------------------------
    assert!(count_notes(&p, "orphaned") == 2);      // one per pending key (f000, f003)
    assert!(p.counters.errors == 1);                // pair rule fires ONCE (sticky)
    // model A: 2 orphan UGRDs. Model C: once the sticky rule is set, every
    // grid-relative message that RESOLVES to a configured parameter is a SKIP
    // (TMP x2 + UGRD x2 per file); its VGRD messages never resolve (VGRD is not
    // configured there) and stay invisible, exactly as before phase B.
    assert!(p.counters.skipped_messages == 2 + 4);
    assert!(count_notes(&p, "pair rule violated") == 1);
    assert!(count_notes(&p, "wind components skipped") == 2); // <= ONE note per file

    // ---- model A stored TMP but NOT the orphaned UGRD ---------------------
    let run = "2026101912";
    let a_run = join_path(&join_path(&data, "hrrr"), run);
    let tmp_agl = join_path(&join_path(&a_run, "TMP"), "height_above_ground");
    match stat_size_mtime(&join_path(&tmp_agl, "f000.bin")) {
        Err(_) => { assert!(false); return; }
        Ok(_) => {}
    }
    let ugd_dir = join_path(&join_path(&a_run, "UGRD"), "height_above_ground");
    match stat_size_mtime(&join_path(&ugd_dir, "f000.bin")) {
        Ok(_) => { assert!(false); return; }   // NEVER stored half-rotated
        Err(_) => {}
    }

    // ---- model B paired normally: nothing leaked from A's orphan map -----
    let b_run = join_path(&join_path(&data, "hrrrb"), run);
    let bu = join_path(&join_path(&b_run, "UGRD"), "height_above_ground");
    match stat_size_mtime(&join_path(&bu, "f000.bin")) {
        Err(_) => { assert!(false); return; }
        Ok(_) => {}
    }
    let bv = join_path(&join_path(&b_run, "VGRD"), "height_above_ground");
    match stat_size_mtime(&join_path(&bv, "f000.bin")) {
        Err(_) => { assert!(false); return; }
        Ok(_) => {}
    }
    // earth-relative storage (spec §4.4): the .bin header flag bit 1 (winds_grid)
    // must be CLEAR. Flags byte sits at offset 22 (magic 8 + version 4 + nx 4
    // + ny 4 + level-count 2), and the level count (u16 LE at offset 20) must be
    // ONE because a GROUP holds exactly one level here.
    let fb = read_bytes(&join_path(&bu, "f000.bin"));
    assert!(fb.len() > 24);
    assert!((fb[22] & 2) == 0);
    assert!(fb[20] == 1 && fb[21] == 0);

    // ---- model C: TMP (non-grid-relative messages) survives, the half-
    // configured wind pair does NOT ------------------------------------------
    let c_run = join_path(&join_path(&data, "hrrrc"), run);
    let ctmp = join_path(&join_path(&c_run, "TMP"), "height_above_ground");
    match stat_size_mtime(&join_path(&ctmp, "f000.bin")) {
        Err(_) => { assert!(false); return; }
        Ok(_) => {}
    }
    match stat_size_mtime(&join_path(&join_path(&c_run, "UGRD"), "height_above_ground")) {
        Ok(_) => { assert!(false); return; }   // NEVER stored half-rotated
        Err(_) => {}
    }

    // idempotent afterwards: another round changes NOTHING
    assert!(p.scan_round() == 0);
    assert!(p.counters.errors == 1);
    assert!(count_notes(&p, "orphaned") == 2);

    rm_tree(&root);
}
