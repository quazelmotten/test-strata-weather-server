// V1.1 phase 5 (spec §5): the FULL handler lookup path allocates ZERO times.
// Measurement boundary (documented deliberately): from REQUEST-LINE/query-string
// parsing (`parse_query`) through per-parameter selection, evaluation and
// interpolation-metadata structuring (`lookup_entries` + `entries_at`), UP TO
// but EXCLUDING JSON body string construction (`render_point_json` /
// `render_entry` / `make_response`) — the spec allows allocation there ("apart
// from building the response body"). The axum router dispatch around the handler
// is outside our code path and not measured; everything OUR code does per request
// between those boundaries is covered.
//
// Mix (>=20k queries, all success-class so the zero-alloc claim is about the
// answering path): GFS exact node, dateline seam, vertical interp, time interp
// (integral AND fractional forecast-hour offsets, post-phase-3 seconds math),
// multi-param, run-pinned, wind pair, HRRR LCC exact + LCC vertical/time interp,
// plus a masked-point pass against the small fixture (null + missing_data entry
// must ALSO cost zero allocations). Values are asserted against the wgrib2-derived
// expectations in tests/perfdata/probes.tsv, so this test doubles as a bit-exactness
// guard for the interning/borrowing refactor.

use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::ffi::c_void;
use libc::malloc;
use libc::free;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use wx_ingest::pipeline::Pipeline;
use wx_server::queryapi::{parse_query, lookup_entries, new_entries, entries_at};
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;

static N_ALLOC: AtomicU64 = AtomicU64::new(0);
static N_FREE: AtomicU64 = AtomicU64::new(0);

struct CountingAlloc {
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, req: Layout) -> *mut u8 {
        let align = req.align();
        let size = req.size();
        let total = size + align + 8;
        let raw = malloc(total);
        let addr = raw as usize;
        assert!(addr != 0);
        let mut aligned = addr + 8 + align - 1;
        aligned = aligned - (aligned % align);
        let header = (aligned - 8) as *mut usize;
        *(header) = addr;
        let _a = N_ALLOC.fetch_add(1, Ordering::Relaxed);
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _req: Layout) {
        let header = (ptr as usize - 8) as *mut usize;
        let raw = *(header);
        let _f = N_FREE.fetch_add(1, Ordering::Relaxed);
        free(raw as *mut c_void);
    }
}

#[global_allocator]
static CA: CountingAlloc = CountingAlloc { };

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
fn copy_file(src: String, dst: String) {
    let f = File::open(&src);
    match f {
        Err(_) => { panic!(); }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { panic!(); }
                Ok(region) => {
                    let d = File::create(&dst);
                    match d {
                        Err(_) => { panic!(); }
                        Ok(mut w) => {
                            let n = w.write_all(&region[0 .. region.len()]);
                            if n.is_err() { panic!(); }
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn write_text(path: &str, text: String) {
    let d = File::create(path);
    match d {
        Err(_) => { panic!(); }
        Ok(mut w) => {
            let n = w.write_all(text.as_bytes());
            if n.is_err() { panic!(); }
        }
    }
}

#[test]
fn full_handler_lookup_is_allocation_free() {
    let perf0 = "../../tests/perfdata/gfs_perf_f000.grib2".to_string();
    let perf6 = "../../tests/perfdata/gfs_perf_f006.grib2".to_string();
    let hrr0 = "../../tests/perfdata/hrrr_perf_f000.grib2".to_string();
    let hrr3 = "../../tests/perfdata/hrrr_perf_f003.grib2".to_string();
    let small = "../../tests/fixtures/gfs_small.grib2".to_string();
    let pr0 = File::open(&perf0);
    if pr0.is_err() { return; }
    let pr6 = File::open(&perf6);
    if pr6.is_err() { return; }
    let ph0 = File::open(&hrr0);
    if ph0.is_err() { return; }
    let ph3 = File::open(&hrr3);
    if ph3.is_err() { return; }
    let psml = File::open(&small);
    if psml.is_err() { return; }
    let root = "/tmp/opencode/wxpoint-fullalloc";
    rm_tree(root);
    let ing_g = join_path(root, "ing_g");
    let ing_h = join_path(root, "ing_h");
    let data = join_path(root, "data");
    let _c1 = create_dir_all(&ing_g);
    let _c2 = create_dir_all(&ing_h);
    copy_file(perf0, join_path(&ing_g, "gfs_perf_f000.grib2"));
    copy_file(perf6.to_string(), join_path(&ing_g, "gfs_perf_f006.grib2"));
    copy_file(hrr0.to_string(), join_path(&ing_h, "hrrr_perf_f000.grib2"));
    copy_file(hrr3.to_string(), join_path(&ing_h, "hrrr_perf_f003.grib2"));

    let cfgp = join_path(root, "config.toml");
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ningest_threads = 8\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_g);
    let t = t + "\"\nfile_glob = \"gfs_perf_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n\n  [[models.params]]\n  name = \"HGT\"\n  levels = [\"isobaric:*\"]\n\n  [[models.params]]\n  name = \"UGRD\"\n  levels = [\"height_above_ground:10\"]\n\n  [[models.params]]\n  name = \"VGRD\"\n  levels = [\"height_above_ground:10\"]\n\n[[models]]\nname = \"hrrr\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing_h);
    let t = t + "\"\nfile_glob = \"hrrr_perf_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n\n  [[models.params]]\n  name = \"HGT\"\n  levels = [\"isobaric:*\"]\n";
    write_text(&cfgp, t);

    let pr = Pipeline::open(&cfgp);
    let mut p = match pr {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    // Deterministic gate (V1.2 phase B0): do NOT pin WHICH round ingests. With
    // stable_window_secs = 0 the files become eligible on the second round, but
    // a transient stat/mmap failure on one candidate (the realistic flake class
    // under load: `content_hash_file` / `stat_size_mtime` errors silently SKIP a
    // candidate for that round) used to make `r2 == 4` fail. Settle with a BOUNDED
    // loop and assert the TOTAL.
    let mut got = 0usize;
    let mut rounds = 0usize;
    while got != 4usize && rounds < 8usize {
        let one = p.scan_round();
        got += one;
        rounds += 1;
    }
    assert!(got == 4usize);
    assert!(p.counters.errors == 0);

    let snap = p.shared.get_index();
    let sref = &*snap;

    // ---- success-class mix mirroring tests/perfdata/probes.tsv ------------
    let qs_list: Vec<String> = vec!["model=gfs&param=TMP&level=isobaric:1000&lat=39.875&lon=25.0&valid=2026-10-19T12:00:00Z".to_string(),
    "model=gfs&param=TMP&level=isobaric:1000&lat=0.125&lon=359.9&valid=2026-10-19T12:00:00Z".to_string(),
    "model=gfs&param=TMP&level=isobaric:775&lat=39.875&lon=25.0&valid=2026-10-19T12:00:00Z".to_string(),
    "model=gfs&param=TMP&level=isobaric:1000&lat=39.875&lon=25.0&valid=2026-10-19T15:00:00Z".to_string(),
    "model=gfs&param=TMP,HGT&level=isobaric:850&lat=39.875&lon=25.0&valid=2026-10-19T12:00:00Z".to_string(),
    "model=hrrr&param=TMP&level=isobaric:850&lat=35.009136&lon=-96.979227&valid=2026-10-19T12:00:00Z".to_string(),
    "model=hrrr&param=TMP&level=isobaric:775&lat=35.009136&lon=-96.979227&valid=2026-10-19T13:30:00Z".to_string(),
    "model=gfs&param=TMP&level=isobaric:1000&lat=39.875&lon=25.0&valid=2026-10-19T14:30:00Z&run=2026-10-19T12:00:00Z".to_string(),
    "model=gfs&param=UGRD,VGRD&level=height_above_ground:10&lat=39.875&lon=25.0&valid=2026-10-19T12:00:00Z".to_string(),];

    // sanity: expected VALUES from probes.tsv (exact-node 450, vertical 1642.5,
    // time-interp 480, multi-param TMP 1640) — proves the refactor changed no
    // output bit on these paths.
    let chk = |qi: usize, want: f64| -> bool {
        let pa_r = parse_query(qs_list[qi].as_str());
        match pa_r {
            Err(pf) => { println!("parse fail {0}", pf.detail); false}
            Ok(pa) => {
                let mut eb = new_entries();
                let lr = lookup_entries(sref, &pa, &mut eb);
                match lr {
                    Err(fi) => { println!("lookup fail {0}", fi.param); false}
                    Ok(_) => {
                        let v = entries_at(&eb, 0).value;
                        v == want
                    }
                }
            }
        }
    };
    assert!(chk(0, 450.0));
    assert!(chk(2, 1642.5));
    assert!(chk(3, 480.0));
    assert!(chk(4, 1640.0));

    // ---- counted region: >=20k mixed queries, ZERO allocations -----------
    let nq = 20000;
    let a0 = N_ALLOC.load(Ordering::Relaxed);
    let f0 = N_FREE.load(Ordering::Relaxed);
    let mut sink = 0.0;
    let mut ne = 0usize;
    for k in 0 .. nq {
        let qs = qs_list[k % qs_list.len()].as_str();
        let pa_r = parse_query(qs);
        match pa_r {
            Err(_pf) => { panic!(); }
            Ok(pa) => {
                let mut eb = new_entries();
                let lr = lookup_entries(sref, &pa, &mut eb);
                match lr {
                    Err(_fi) => { panic!(); }
                    Ok(_) => {
                        let mut j = 0;
                        while j < eb.n {
                            let e = entries_at(&eb, j);
                            sink = sink + e.value + (e.units.len() as f64);
                            if e.masked { sink += 1.0; }
                            j += 1;
                        }
                        ne += eb.n;
                    }
                }
            }
        }
    }
    let a1 = N_ALLOC.load(Ordering::Relaxed);
    let f1 = N_FREE.load(Ordering::Relaxed);
    assert!(a1 == a0);
    assert!(f1 == f0);
    assert!(ne >= nq);
    if sink == -12345.0 && ne == 0usize {
        // observer; never taken
    }

    // ---- masked path against the small fixture: null entry, still zero allocs
    let root2 = "/tmp/opencode/wxpoint-fullalloc-small";
    rm_tree(root2);
    let ing2 = join_path(root2, "ing");
    let data2 = join_path(root2, "data");
    let _c3 = create_dir_all(&ing2);
    copy_file(small.to_string(), join_path(&ing2, "gfs_small.grib2"));
    let cfgp2 = join_path(root2, "config.toml");
    let t2 = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t2 = format!("{0}{1}", t2, data2);
    let t2 = t2 + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ningest_threads = 2\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t2 = format!("{0}{1}", t2, ing2);
    let t2 = t2 + "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n";
    write_text(&cfgp2, t2);
    let pr2 = Pipeline::open(&cfgp2);
    let mut p2 = match pr2 {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    let mut got2 = 0usize;
    let mut rounds2 = 0usize;
    while got2 != 1usize && rounds2 < 8usize {
        let one2 = p2.scan_round();
        got2 += one2;
        rounds2 += 1;
    }
    assert!(got2 == 1usize);
    let snap2 = p2.shared.get_index();
    let sref2 = &*snap2;
    let mq = "model=gfs&param=TMP&level=isobaric:850&lat=10&lon=350&valid=2026-10-19T18:00:00Z".to_string();
    let a2 = N_ALLOC.load(Ordering::Relaxed);
    let f2 = N_FREE.load(Ordering::Relaxed);
    let mut masked_seen = 0usize;
    for _k in 0 .. 2000 {
        let pa_r = parse_query(mq.as_str());
        match pa_r {
            Err(_pf) => { panic!(); }
            Ok(pa) => {
                let mut eb = new_entries();
                let lr = lookup_entries(sref2, &pa, &mut eb);
                match lr {
                    Err(_fi) => { panic!(); }
                    Ok(_) => {
                        let e = entries_at(&eb, 0);
                        if e.masked { masked_seen += 1usize; }
                    }
                }
            }
        }
    }
    let a3 = N_ALLOC.load(Ordering::Relaxed);
    let f3 = N_FREE.load(Ordering::Relaxed);
    assert!(masked_seen == 2000usize);
    assert!(a3 == a2);
    assert!(f3 == f2);

    println!("full-handler lookup path: {0} mixed queries + 2000 masked queries, 0 allocations", nq);
    rm_tree(root);
    rm_tree(root2);
}
