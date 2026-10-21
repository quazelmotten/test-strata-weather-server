// Full-GFS-geometry ingest timing (spec §8: "one forecast hour of GFS becomes
// queryable within seconds of stability"). Uses tests/perfdata/gfs_perf_f000.grib2
// (1440 x 720 TMP@1000 f000, value = 250 + row_index; gitignored, regenerate
// with scripts/gen_grib2.py --perf). Skips silently when absent. Prints the
// measured ingest wall time (marker clock) for the README numbers.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{PointQueryContext, evaluate_point};
use wx_ingest::grib2::decode_file;
use wx_ingest::grib2::ParamFilter;
use wx_ingest::codecs::data_cksum_grid;
use wx_core::interp::{TimeInterpMode};
use wx_core::tables::{lookup_level_type};
use wx_ingest::state::{join_path, now_ns_marker, stat_size_mtime};
use wx_ingest::discovery::{path_to_str};
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

fn perf_path() -> String {
    return "../../tests/perfdata/gfs_perf_f000.grib2".to_string();
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

// ---------------------------------------------------------------------------
// JPEG2000 packing at production scale (spec §9.1 at GFS geometry): the same
// forecast hour repacked as DRT 5.40 must decode to bit-identical arrays. The
// checksums below were captured from tools/wgrib2-full `-checksum data` on
// tests/perfdata/gfs_perf_f000_jpeg.grib2 (and are identical to the simple-packed
// file's); regenerating fixtures re-checks that equality, this test pins it in
// CI without wgrib2. Also prints the wall time of decoding one GFS-scale hour
// through the JPEG2000 path, for the README ingest-latency numbers.
// ---------------------------------------------------------------------------

const PERF_JPEG_CKSUMS: [u32; 22] = [
    313604276, 3623768571, 1605277577, 1093394411, 3302352484, 385811988,
    4150684559, 4286297571, 183562162, 2724321841, 1830331398, 3780209560,
    2536677251, 3457776197, 1335159323, 3035415466, 1830331398, 3032319591,
    701295723, 96053817, 3109806711, 416041556,
];

#[test]
fn perf_gfs_hour_jpeg_matches_simple_packing() {
    match stat_size_mtime("../../tests/perfdata/gfs_perf_f000_jpeg.grib2") {
        Err(_) => { return; }
        Ok(_) => {}
    }

    let t0 = now_ns_marker("../../tests/perfdata");
    let r = decode_file("../../tests/perfdata/gfs_perf_f000_jpeg.grib2".to_string(), ParamFilter::All);
    let t1 = now_ns_marker("../../tests/perfdata");
    match r {
        Err(_e) => {
            println!("jpeg perf twin decode failed");
            assert!(false);
        }
        Ok((msgs, st)) => {
            assert!(msgs.len() == PERF_JPEG_CKSUMS.len());
            assert!(st.messages_decoded == 22);
            let mut k = 0;
            while k < msgs.len() {
                let gk = msgs[k].grid.clone();
                assert!(data_cksum_grid(msgs[k].data.clone(), &gk) == PERF_JPEG_CKSUMS[k]);
                k += 1;
            }
            let ms = (t1 - t0) / 1000000;
            println!("{}", ms);
            // generous CI bound; the README records the real measured number
            assert!(ms < 120000);
        }
    }
}

#[test]
fn perf_gfs_hour_ingest_and_query() {
    // skip when the perf fixture was not generated on this machine
    match stat_size_mtime(&perf_path()) {
        Err(_) => { return; }
        Ok(_) => {}
    }

    let root = "/tmp/opencode/wxpoint-perf".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    write_bytes(&join_path(&ing, "gfs_perf_f000.grib2"), &read_bytes(&perf_path()));

    let cfg_path = join_path(&root, "config.toml");
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ningest_threads = 4\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_perf_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n";
    write_bytes(&cfg_path, t.as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { assert!(false); return; }
        Ok(pp) => { pp }
    };

    let t0 = now_ns_marker(&p.state.dir_path);
    let _r1 = p.scan_round();
    let r2 = p.scan_round();
    let t1 = now_ns_marker(&p.state.dir_path);
    assert!(r2 == 1);
    assert!(p.counters.errors == 0);

    let ms = (t1 - t0) / 1000000;
    println!("{}", ms);
    // generous CI bound; the README records the real measured number
    assert!(ms < 60000);

    // Correctness on the full grid: value = 250 + row j, lat 89.875 - 0.25j.
    match lookup_level_type("isobaric") {
        None => { assert!(false); return; }
        Some(iso) => {
            let snap = p.shared.get_index();
            let sref = &*snap;
            match sref.find_model("gfs") {
                None => { assert!(false); return; }
                Some(m) => {
                    let seed = PointQueryContext {
                        grid: m.runs[0].grid.clone(),
                        units: "".to_string(),
                        run_epoch_secs: 0,
                        stat_processed: false,
                        want_level: 0.0,
                        want_fh_hours: 0.0,
                        fh_hours: (0.0, 0.0),
                        slots: [None, None],
                    };
                    let c_r = seed.build(sref, "gfs", None, "TMP", iso,
                                         1000.0, 0.0, TimeInterpMode::Linear);
                    match c_r {
                        Err(_) => { assert!(false); return; }
                        Ok(c) => {
                            // north edge node
                            let v0 = evaluate_point(&c, 89.875, 0.0);
                            match v0 {
                                Err(_) => { assert!(false); return; }
                                Ok(pv) => { assert!(pv.value == 250.0); }
                            }
                            // row 1
                            let v1 = evaluate_point(&c, 89.625, 0.25);
                            match v1 {
                                Err(_) => { assert!(false); return; }
                                Ok(pv) => { assert!(pv.value == 251.0); }
                            }
                            // south edge node (row 719)
                            let v2 = evaluate_point(&c, -89.875, 359.75);
                            match v2 {
                                Err(_) => { assert!(false); return; }
                                Ok(pv) => { assert!(pv.value == 969.0); }
                            }
                            // off-node bilinear between rows 0/1 at lon 0.125
                            let v3 = evaluate_point(&c, 89.75, 0.125);
                            match v3 {
                                Err(_) => { assert!(false); return; }
                                Ok(pv) => { assert!(pv.value == 250.5); }
                            }
                        }
                    }
                }
            }
        }
    }
}
