// V1.3 phase E3b DEFECT GATE #1 — THE WALK NO LONGER FOLLOWS ITSELF INTO A
// SYMLINK LOOP.
//
// THE DEFECT (AUDIT-confirmed): walk_files classified directories by "read_dir
// succeeded", so a symlink pointing at an ANCESTOR directory re-entered itself
// forever — the ingest thread (and the startup wind preflight, which walks too)
// hung INFINITELY with no note and no counter.
//
// THE FIX: directories are recorded in a VISITED set keyed by their CANONICAL
// path (metadata().is_dir() dispatch, canonicalize per directory); a repeat is
// skipped with a note. A hard cap (WALK_MAX_PATHS) bounds even pathological
// trees per round, loudly. This binary proves termination with real symlinks,
// that files behind links are STILL found, that deep loop-free trees are
// untouched, and that the cap fires with a note.

use wx_ingest::discovery::{walk_files, walk_files_bounded, WALK_MAX_PATHS};
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir_all;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::symlink;

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

fn count_containing(hay: &[String], pat: &str) -> usize {
    let mut n = 0usize;
    for h in hay {
        if h.contains(pat) {
            n += 1;
        }
    }
    n
}

#[test]
fn symlink_to_ancestor_terminates_and_still_finds_every_file() {
    let root = "/tmp/opencode/wxpoint-symwalk";
    let _r = remove_dir_all(root);
    let _c = create_dir_all(format!("{0}/a/b", root));
    write_bytes(&format!("{0}/top.grib2", root), b"x");
    write_bytes(&format!("{0}/a/b/deep.grib2", root), b"y");
    // the killer shape: a link INSIDE the tree pointing at an ANCESTOR...
    let l1 = symlink(format!("{0}/a", root), format!("{0}/a/b/up", root));
    if l1.is_err() { panic!(); }
    // ...and a link pointing at the tree ROOT (loop through two hops).
    let l2 = symlink(root, format!("{0}/a/selfroot", root));
    if l2.is_err() { panic!(); }
    // a symlink straight to a FILE must remain a candidate (old behavior kept).
    let l3 = symlink(format!("{0}/top.grib2", root), format!("{0}/alias.grib2", root));
    if l3.is_err() { panic!(); }

    // Pre-E3b this call NEVER RETURNED. Post-E3b it terminates and finds the
    // three REAL files plus the file-alias (dir-links resolve to visited).
    let out = walk_files(root);
    let joined = out.join("\n");
    assert!(joined.contains("top.grib2"));
    assert!(joined.contains("deep.grib2"));
    assert!(joined.contains("alias.grib2"));

    // The bounded variant reports WHY it skipped the loops.
    let mut notes: Vec<String> = Vec::<String>::new();
    let out2 = walk_files_bounded(root, WALK_MAX_PATHS, &mut notes);
    assert!(out2.len() == out.len());
    assert!(count_containing(&notes, "ALREADY-WALKED") >= 2);
    let _d = remove_dir_all(root);
}

#[test]
fn deep_loop_free_tree_is_unaffected_and_notes_stay_empty() {
    let root = "/tmp/opencode/wxpoint-symdeep";
    let _r = remove_dir_all(root);
    // 40 nested directories, one file each: far beyond any production depth,
    // ZERO symlinks — the visited set must not change ANYTHING here.
    let mut dir = root.to_string();
    for k in 0 .. 40 {
        dir = format!("{0}/d{1}", dir, k);
        let _c = create_dir_all(&dir);
        write_bytes(&format!("{0}/f{1}.grib2", dir, k), b"z");
    }
    let mut notes: Vec<String> = Vec::<String>::new();
    let out = walk_files_bounded(root, WALK_MAX_PATHS, &mut notes);
    assert!(out.len() == 40);
    assert!(notes.is_empty());
    let _d = remove_dir_all(root);
}

#[test]
fn path_cap_stops_the_walk_with_one_loud_note() {
    let root = "/tmp/opencode/wxpoint-symcap";
    let _r = remove_dir_all(root);
    let _c = create_dir_all(root);
    for k in 0 .. 10 {
        write_bytes(&format!("{0}/f{1}.grib2", root, k), b"q");
    }
    let mut notes: Vec<String> = Vec::<String>::new();
    let out = walk_files_bounded(root, 3, &mut notes);
    // The cap counts EMITTED paths: the pop that trips the check is dropped
    // unprocessed, so exactly 3 come back and ONE note explains the cut.
    assert!(out.len() == 3);
    assert!(count_containing(&notes, "path cap") == 1);
    // A cap at/above the real count behaves like the uncapped walk: no note.
    let mut notes2: Vec<String> = Vec::<String>::new();
    let full = walk_files_bounded(root, WALK_MAX_PATHS, &mut notes2);
    assert!(full.len() == 10);
    assert!(notes2.is_empty());
    let _r2 = remove_file(format!("{0}/f0.grib2", root));
    let _d = remove_dir_all(root);
}
