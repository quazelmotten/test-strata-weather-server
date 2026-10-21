// V1.2 phase C gate #7: STABILITY TRACKER HYGIENE (spec §4.1).
//
// `StabilityTracker.obs` remembered ONE entry per candidate path FOREVER: an
// ingest dir that churns (files renamed away mid-delivery, products deleted
// upstream, names that stopped matching the glob) grew the vector without bound,
// and every scan round paid a linear scan over it per candidate. Now:
//   * [`prune_vanished`] (called ONCE per scan round by `scan_round`) forgets
//     entries whose file no longer exists — ONE stat per remembered path;
//   * the vector doubles as an LRU and `observe` enforces [`STABILITY_OBS_CAP`]
//     by evicting the LEAST recently observed path. Eviction only restarts a
//     stability WINDOW, so it can never change an answer.

use wx_ingest::discovery::{StabilityTracker, STABILITY_OBS_CAP, path_to_str};
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

#[allow(clippy::needless_return)]
fn touch(path: String) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut h) => {
            let w = h.write_all("x".as_bytes());
            match w {
                Err(_) => { assert!(false); }
                Ok(_) => {}
            }
        }
    }
}

#[test]
fn vanished_paths_are_forgotten_while_surviving_windows_keep_their_state() {
    let root = "/tmp/opencode/wxpoint-stab-vanish".to_string();
    rm_tree(&root);
    let _c = create_dir_all(&root);
    let live = join_s(root.to_string(), "live.grib2".to_string());
    let ghost = join_s(root.to_string(), "ghost.grib2".to_string());
    touch(live.to_string());
    touch(ghost.to_string());

    let mut st = StabilityTracker { obs: Vec::<(String, u64, i64, i64)>::new() };
    // window 0 ns: first sighting INELIGIBLE, second sighting ELIGIBLE
    assert!(!st.observe(live.to_string(), 1, 100, 1000, 0));
    assert!(st.observe(live.to_string(), 1, 100, 2000, 0));
    assert!(!st.observe(ghost.to_string(), 1, 200, 3000, 0));
    assert!(st.obs.len() == 2);

    // upstream deletes the ghost product
    let _rf = remove_file(&ghost);
    assert!(st.prune_vanished() == 1);
    assert!(st.obs.len() == 1);
    // pruning must NOT shorten or reset the LIVE file's window: it stays eligible
    assert!(st.observe(live.to_string(), 1, 100, 4000, 0));
    // and a second prune is a no-op (idempotent, bounded work)
    assert!(st.prune_vanished() == 0);
    rm_tree(&root);
}

#[test]
fn remembered_observations_are_bounded_and_evict_least_recently_seen_first() {
    let mut st = StabilityTracker { obs: Vec::<(String, u64, i64, i64)>::new() };
    // seed the tracker at the cap plus three extra paths
    for k in 0 .. (STABILITY_OBS_CAP + 3) {
        let name = format!("{0}{1}", "/tmp/opencode/wxpoint-stab-cap/p", k);
        st.obs.push((name.to_string(), 1, 1, 0));
    }
    assert!(st.obs.len() == STABILITY_OBS_CAP + 3);
    st.trim_obs();
    assert!(st.obs.len() == STABILITY_OBS_CAP);
    // eviction happens at the FRONT = the least recently observed entries
    let first = format!("{0}{1}", "/tmp/opencode/wxpoint-stab-cap/p", 0);
    let last = format!("{0}{1}", "/tmp/opencode/wxpoint-stab-cap/p", STABILITY_OBS_CAP + 2);
    let mut have_first = false;
    let mut have_last = false;
    for e in &st.obs {
        if e.0 == first {
            have_first = true;
        }
        if e.0 == last {
            have_last = true;
        }
    }
    assert!(!have_first);
    assert!(have_last);

    // RECENCY BUMP: re-observing a path moves it to the back of the LRU, so a
    // busy file can never be the one evicted while idle neighbours are kept.
    let mut st2 = StabilityTracker { obs: Vec::<(String, u64, i64, i64)>::new() };
    st2.obs.push(("a".to_string(), 1, 1, 0));
    st2.obs.push(("b".to_string(), 1, 1, 0));
    st2.obs.push(("c".to_string(), 1, 1, 0));
    // long window keeps b INELIGIBLE but still counts as an observation
    let _r = st2.observe("b".to_string(), 1, 1, 500, 1000000);
    assert!(st2.obs[2].0 == "b");
    assert!(st2.obs[0].0 == "a");
    // a re-delivery (identity change) also bumps recency
    let _r2 = st2.observe("a".to_string(), 9, 9, 600, 1000000);
    assert!(st2.obs[2].0 == "a");
    assert!(st2.obs.len() == 3);
}

#[allow(clippy::needless_return)]
fn join_s(dir: String, name: String) -> String {
    return format!("{0}/{1}", dir, name);
}
