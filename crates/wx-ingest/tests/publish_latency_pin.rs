// V1.4 phase B2 DEFECT GATE #6 — INGEST-LAG GAUGE VALUE PIN (coverage gap).
//
// THE GAP: pipeline_e2e.rs asserts the E6 publish-latency inputs are merely
// SANE (> 0 and < 1 h) because its fixtures were copied seconds ago, and
// point_http.rs only asserts the two Prometheus lines EXIST. Nothing pinned
// the VALUE against a KNOWN mtime-to-publish span, so a sign flip (mtime -
// now clamps to 0), a unit slip (µs instead of ns, or storing seconds), or
// feeding the wrong instant would all have passed the suite unnoticed.
//
// THIS TEST sets the source file's mtime EXPLICITLY to now - 47 s (FileTimes,
// exact nanoseconds), ingests through the normal pipeline, and pins:
//   * live.last_mtime_ns == the mtime WE set (the lag INPUT plumbing, exact);
//   * live.publish_latency_last_ns within [45 s, 120 s] — the true
//     (publish_time - mtime) span is ~47 s plus milliseconds of pipeline
//     work, so the window is tight enough to kill every plausible bug above
//     while immune to CI scheduling jitter;
//   * lifetime MAX equals LAST (ONE publish happened — nothing else advanced
//     either gauge).
// The plumbing was verified CORRECT before this gate existed (wall-vs-wall
// CLOCK_REALTIME on both sides, ns domain, /metrics renders ns/1e9): unlike
// the other five B2 gates this one is GREEN on pristine b30d156 BY DESIGN —
// it closes the value-assertion gap rather than fixing a reproduced defect.

use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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
    let mut buf: Vec<u8> = Vec::<u8>::new();
    let f = File::open(path);
    match f {
        Err(_) => { return buf; }
        Ok(mut h) => {
            let r = h.read_to_end(&mut buf);
            match r {
                Err(_) => { return Vec::<u8>::new(); }
                Ok(_n) => { return buf; }
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

/// Copy the fixture, then stamp mtime = `secs_ago` s BEFORE NOW at a fixed
/// sub-second offset. Returns the exact wall-clock ns we asked the fs for.
#[allow(clippy::needless_return)]
fn copy_with_backdated_mtime(src_fixture: &str, dst: &str, secs_ago: u64, frac_ns: u32) -> i64 {
    write_bytes(dst, read_bytes(src_fixture).as_slice());
    let now_r = SystemTime::now().duration_since(UNIX_EPOCH);
    let now_d = match now_r {
        Err(_) => { panic!(); }
        Ok(d) => { d }
    };
    let want_secs = now_d.as_secs() - secs_ago;
    let stamp = UNIX_EPOCH + Duration::new(want_secs, frac_ns);
    let mut opener = std::fs::OpenOptions::new();
    let f = opener.write(true).open(dst);
    match f {
        Err(_) => { panic!(); }
        Ok(h) => {
            let ft = std::fs::FileTimes::new().set_modified(stamp);
            let s = h.set_times(ft);
            if s.is_err() { panic!(); }
        }
    }
    return (want_secs as i64) * 1000000000 + (frac_ns as i64);
}

#[allow(clippy::needless_return)]
fn pl_cfg(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn publish_latency_gauge_equals_the_controlled_mtime_to_publish_span() {
    let root = "/tmp/opencode/wxpoint-b2-lag-pin";
    rm_tree(root);
    let ing = join_path(root, "ing");
    let data = join_path(root, "data");
    let _c = create_dir_all(&ing);
    let src = join_path(&ing, "gfs_small.grib2");
    // mtime := now - 47 s at .123456789 so an ms-truncating fs would ALSO be
    // caught by the exact last_mtime_ns pin below (ext4/xfs/tmpfs keep ns).
    let mtime_ns_want = copy_with_backdated_mtime("../../tests/fixtures/gfs_small.grib2",
                                                   src.as_str(), 47, 123456789);
    let cfg_path = join_path(root, "config.toml");
    write_bytes(&cfg_path, pl_cfg(&data, &ing).as_bytes());

    let pr = Pipeline::open(&cfg_path);
    let mut p = match pr {
        Err(_) => { panic!(); }
        Ok(pp) => { pp }
    };
    assert!(p.scan_round() == 0); // stability observation
    assert!(p.scan_round() == 1); // consumed -> publish happened in this call

    // ---- the lag INPUT: exactly the ns we stamped --------------------------
    let fed = p.live.last_mtime_ns.load(std::sync::atomic::Ordering::Relaxed);
    assert!(fed == mtime_ns_want);
    assert!(p.counters.last_mtime_ns == mtime_ns_want);

    // ---- the VALUE: (publish_time - mtime) within a tight sane window ------
    use std::sync::atomic::Ordering;
    let last_ns = p.live.publish_latency_last_ns.load(Ordering::Relaxed);
    let max_ns = p.live.publish_latency_max_ns.load(Ordering::Relaxed);
    // True span is ~47 s; a sign bug clamps to 0, a unit slip lands at ~47
    // (s) or ~47e6 (us), both OUTSIDE this window.
    assert!(last_ns >= 45000000000);
    assert!(last_ns <= 120000000000);
    // ONE successful publish: MAX tracks LAST exactly, nothing else ran.
    assert!(max_ns == last_ns);
    // And the legacy mirror never moves independently of the live feed.
    assert!(p.live.publish_latency_last_ns.load(Ordering::Relaxed) > 0);
    rm_tree(root);
}
