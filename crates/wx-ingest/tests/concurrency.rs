// Spec §5/§9.9 concurrency: readers hammer queries through the arc-swap-published
// index while ingest publishes new snapshots atomically. No request may error,
// see a torn snapshot, or lose mmap data mid-publish; every reader must observe
// the same stable values.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::pipeline::{build_snapshot};
use wx_ingest::index::{SharedIndex, build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode};
use wx_core::tables::{lookup_level_type};
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

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

fn cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn concurrent_publishers_and_readers_never_tear() {
    let root = "/tmp/opencode/wxpoint-conc".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "gfs_small.grib2"), &read_bytes(&fixture("gfs_small.grib2")));
    let cfg_path = join_path(&root, "config.toml");
    write_bytes(&cfg_path, cfg(&data, &ing).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(e) => { println!("{0}", e); assert!(false); return; }
        Ok(pp) => { pp }
    };
    let _ = p.scan_round();
    let n2 = p.scan_round();
    assert!(n2 == 1);

    // A shared index hammered by both sides (same API the server uses).
    let sh = Arc::new(SharedIndex { store: ArcSwap::from_pointee(build_snapshot(&p.registry)) });

    // counters: [ok, fail, done_flag, finished_readers]
    let cnt = Arc::new(Mutex::new([0, 0, 0, 0].to_vec()));

    let nreaders = 4;
    for _r in 0 .. nreaders {
        let sh_c = sh.clone();
        let cnt_c = cnt.clone();
        thread::spawn(move || {
            let mut it = 0;
            loop {
                let snap = sh_c.get_index();
                let sref = &*snap;
                let gm = sref.find_model("gfs");
                match gm {
                    None => {
                        let m = cnt_c.lock();
                        match m {
                            Err(_) => {}
                            Ok(v) => {
                                let mut w = v;
                                w[1] = w[1] + 1;
                            }
                        }
                    }
                    Some(_mm) => {
                        match lookup_level_type("isobaric") {
                            None => {}
                            Some(iso) => {
                                let c_r = build_context(sref, "gfs", None, "TMP", iso,
                                                      925.0, 10800.0, TimeInterpMode::Linear);
                                match c_r {
                                    Err(_) => {
                                        let m = cnt_c.lock();
                                        match m {
                                            Err(_) => {}
                                            Ok(v) => {
                                                let mut w = v;
                                                w[1] = w[1] + 1;
                                            }
                                        }
                                    }
                                    Ok(c) => {
                                        let v = evaluate_point(&c, 9.0, 351.5);
                                        match v {
                                            Err(_) => {
                                                let m = cnt_c.lock();
                                                match m {
                                                    Err(_) => {}
                                                    Ok(vv) => {
                                                        let mut w = vv;
                                                        w[1] = w[1] + 1;
                                                    }
                                                }
                                            }
                                            Ok(pvv) => {
                                                // vertical 925: fh0 311, fh6 371; time fh 3 -> 341.
                                                if pvv.value == 341.0 {
                                                    let m = cnt_c.lock();
                                                    match m {
                                                        Err(_) => {}
                                                        Ok(vb) => {
                                                            let mut w = vb;
                                                            w[0] = w[0] + 1;
                                                        }
                                                    }
                                                } else {
                                                    let m = cnt_c.lock();
                                                    match m {
                                                        Err(_) => {}
                                                        Ok(vt) => {
                                                            let mut w = vt;
                                                            w[1] = w[1] + 1;
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
                it += 1;
                // stop when main signals done after >= 500 queries
                let m2 = cnt_c.lock();
                match m2 {
                    Err(_) => {}
                    Ok(vw) => {
                        if vw[2] == 1 && it >= 500 {
                            let mut w = vw;
                            w[3] = w[3] + 1;
                            break;
                        }
                    }
                }
            }
        });
    }

    // Publisher side: swap fresh snapshots while readers hammer.
    for _k in 0 .. 300 {
        let snap = build_snapshot(&p.registry);
        sh.publish(Arc::new(snap));
    }
    let md = cnt.lock();
    match md {
        Err(_) => { assert!(false); return; }
        Ok(v) => {
            let mut w = v;
            w[2] = 1;
        }
    }

    // Wait for all readers to finish.
    loop {
        let mw = cnt.lock();
        match mw {
            Err(_) => { break; }
            Ok(v) => {
                if v[3] >= nreaders {
                    break;
                }
            }
        }
        let snap2 = build_snapshot(&p.registry);
        sh.publish(Arc::new(snap2));
    }

    let mf = cnt.lock();
    match mf {
        Err(_) => { assert!(false); return; }
        Ok(v) => {
            assert!(v[1] == 0);          // no query ever failed
            assert!(v[0] >= 4 * 500);    // every reader did its share of queries
        }
    }
}
