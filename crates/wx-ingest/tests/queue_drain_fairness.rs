// V1.2 phase C gate #13: THE EVENT QUEUE IS DRAINED ONCE PER ROUND, NOT ONCE PER
// MODEL (spec §4.1: notify events are a LATENCY optimization over the periodic
// full scan).
//
// Previously `scan_round` drained the shared queue INSIDE the per-model loop, so
// the FIRST model consumed every queued path and LATER MODELS always saw an EMPTY
// queue: their events were silently stolen, leaving them dependent on the next
// periodic scan. The discriminator used here is a product file that the directory
// WALK cannot see at all (it sits OUTSIDE the model's ingest_dir), so ONLY a
// correctly delivered EVENT can make it queryable.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str, WatchQueue};
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
fn has_grid(data: &str, model: &str, run_dir: &str) -> bool {
    let md = join_path(data, model);
    let rd = join_path(&md, run_dir);
    let probe = join_path(&rd, "grid.bin");
    return !read_bytes(&probe).is_empty();
}

// TWO models: gfs FIRST, hrrr SECOND (the one that used to be starved).
#[allow(clippy::needless_return)]
fn enqueue(q: WatchQueue, paths: Vec<String>) {
    // scoped so the mutex is RELEASED before the scan round drains the queue
    let lk = q.lock();
    match lk {
        Err(_) => { panic!(); }
        Ok(mut g) => {
            for s in &paths {
                g.push(s.to_string());
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn two_model_cfg(data: &str, ing_gfs: &str, ing_hrr: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_gfs);
    let t = t + "\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n\n[[models]]\nname = \"hrrr\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_hrr);
    let t = t + "\"\nfile_glob = \"hrrr_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn queued_events_reach_every_model_in_the_same_round_not_just_the_first_one() {
    let root = "/tmp/opencode/wxpoint-queue-fair".to_string();
    rm_tree(&root);
    let ing_gfs = join_path(&root, "ing-gfs");
    let ing_hrr = join_path(&root, "ing-hrrr");
    let staged = join_path(&root, "staged");
    let _c1 = create_dir_all(&ing_gfs);
    let _c2 = create_dir_all(&ing_hrr);
    let _c3 = create_dir_all(&staged);
    let data = join_path(&root, "data");

    // gfs product sits IN its ingest dir (walk finds it)
    write_bytes(&join_path(&ing_gfs, "gfs_small.grib2"), &read_bytes(&fixture("gfs_small.grib2")));
    // hrrr product sits OUTSIDE its ingest dir: the WALK cannot see it, only an
    // EVENT can carry it in.
    let hrr_path = join_path(&staged, "hrrr_small.grib2");
    write_bytes(&hrr_path, &read_bytes(&fixture("hrrr_small.grib2")));

    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, two_model_cfg(&data, &ing_gfs, &ing_hrr).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };

    // simulate what a notify watcher would deliver for BOTH products
    let evs: Vec<String> = vec![join_path(&ing_gfs, "gfs_small.grib2").to_string(),
                                hrr_path.to_string()];
    enqueue(p.queue.clone(), evs.clone());

    // round 1: both paths observed (stability window opens), nothing ingested yet
    assert!(p.scan_round() == 0);
    // A drain CONSUMES the queue, so a re-delivery (the notify CREATE+RENAME pair,
    // or any later touch of the same path) re-queues both products for the next
    // round - exactly what a real watcher does while a product is being written.
    enqueue(p.queue.clone(), evs.clone());
    // round 2: BOTH models act on their event-carried candidate in the SAME round
    // pair. Under the per-model drain this is 1: hrrr's event was eaten by gfs and
    // its file is invisible to the directory walk, so it would NEVER be ingested.
    let n2 = p.scan_round();
    assert!(n2 == 2);
    assert!(p.counters.errors == 0);
    assert!(has_grid(&data, "gfs", "2026101912"));
    assert!(has_grid(&data, "hrrr", "2026101912"));
    rm_tree(&root);
}
