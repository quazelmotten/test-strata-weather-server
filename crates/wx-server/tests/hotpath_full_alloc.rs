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

static n_alloc: AtomicU64 = AtomicU64::new(0);
static n_free: AtomicU64 = AtomicU64::new(0);

struct CountingAlloc {
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(self: &Self, req: Layout) -> *mut u8 {
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
        let _a = n_alloc.fetch_add(1, Ordering::Relaxed);
        return aligned as *mut u8;
    }

    unsafe fn dealloc(self: &Self, ptr: *mut u8, _req: Layout) {
        let header = (ptr as usize - 8) as *mut usize;
        let raw = *(header);
        let _f = n_free.fetch_add(1, Ordering::Relaxed);
        free(raw as *mut c_void);
    }
}

#[global_allocator]
static CA: CountingAlloc = CountingAlloc { };

#[allow(clippy::needless_return)]
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
fn copy_file(src: String, dst: String) {
    let f = File::open(&src);
    match f {
        Err(_) => { assert!(false); return; }
        Ok(h) => {
            let m = unsafe { Mmap::map(&h) };
            match m {
                Err(_) => { assert!(false); return; }
                Ok(region) => {
                    let d = File::create(&dst);
                    match d {
                        Err(_) => { assert!(false); }
                        Ok(mut w) => {
                            let n = w.write_all(&region[0 .. region.len()]);
                            match n {
                                Err(_) => { assert!(false); }
                                Ok(_) => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::needless_return)]
fn write_text(path: &str, text: String) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(text.as_bytes());
            match n {
                Err(_) => { assert!(false); }
                Ok(_) => {}
            }
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
    match pr0 {
        Err(_) => { return; }
        Ok(_) => {}
    }
    let pr6 = File::open(&perf6);
    match pr6 {
        Err(_) => { return; }
        Ok(_) => {}
    }
    let ph0 = File::open(&hrr0);
    match ph0 {
        Err(_) => { return; }
        Ok(_) => {}
    }
    let ph3 = File::open(&hrr3);
    match ph3 {
        Err(_) => { return; }
        Ok(_) => {}
    }
    let psml = File::open(&small);
    match psml {
        Err(_) => { return; }
        Ok(_) => {}
    }
    let root = "/tmp/opencode/wxpoint-fullalloc";
    rm_tree(&root);
    let ing_g = join_path(&root, "ing_g");
    let ing_h = join_path(&root, "ing_h");
    let data = join_path(&root, "data");
    let _c1 = create_dir_all(&ing_g);
    let _c2 = create_dir_all(&ing_h);
    copy_file(perf0, join_path(&ing_g, "gfs_perf_f000.grib2"));
    copy_file(perf6.to_string(), join_path(&ing_g, "gfs_perf_f006.grib2"));
    copy_file(hrr0.to_string(), join_path(&ing_h, "hrrr_perf_f000.grib2"));
    copy_file(hrr3.to_string(), join_path(&ing_h, "hrrr_perf_f003.grib2"));

    let cfgp = join_path(&root, "config.toml");
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
        Err(_e) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    let _r1 = p.scan_round();
    let r2 = p.scan_round();
    assert!(r2 == 4);

    let snap = p.shared.get_index();
    let sref = &*snap;

    // ---- success-class mix mirroring tests/perfdata/probes.tsv ------------
    let mut qs_list: Vec<String> = Vec::<String>::new();
    let _a = qs_list.push("model=gfs&param=TMP&level=isobaric:1000&lat=39.875&lon=25.0&valid=2026-10-19T12:00:00Z".to_string());
    let _b = qs_list.push("model=gfs&param=TMP&level=isobaric:1000&lat=0.125&lon=359.9&valid=2026-10-19T12:00:00Z".to_string());
    let _c = qs_list.push("model=gfs&param=TMP&level=isobaric:775&lat=39.875&lon=25.0&valid=2026-10-19T12:00:00Z".to_string());
    let _d = qs_list.push("model=gfs&param=TMP&level=isobaric:1000&lat=39.875&lon=25.0&valid=2026-10-19T15:00:00Z".to_string());
    let _e = qs_list.push("model=gfs&param=TMP,HGT&level=isobaric:850&lat=39.875&lon=25.0&valid=2026-10-19T12:00:00Z".to_string());
    let _f = qs_list.push("model=hrrr&param=TMP&level=isobaric:850&lat=35.009136&lon=-96.979227&valid=2026-10-19T12:00:00Z".to_string());
    let _g = qs_list.push("model=hrrr&param=TMP&level=isobaric:775&lat=35.009136&lon=-96.979227&valid=2026-10-19T13:30:00Z".to_string());
    let _h = qs_list.push("model=gfs&param=TMP&level=isobaric:1000&lat=39.875&lon=25.0&valid=2026-10-19T14:30:00Z&run=2026-10-19T12:00:00Z".to_string());
    let _i = qs_list.push("model=gfs&param=UGRD,VGRD&level=height_above_ground:10&lat=39.875&lon=25.0&valid=2026-10-19T12:00:00Z".to_string());

    // sanity: expected VALUES from probes.tsv (exact-node 450, vertical 1642.5,
    // time-interp 480, multi-param TMP 1640) — proves the refactor changed no
    // output bit on these paths.
    let chk = |qi: usize, want: f64| -> bool {
        let pa_r = parse_query(qs_list[qi].as_str());
        match pa_r {
            Err(pf) => { println!("parse fail {0}", pf.detail); return false; }
            Ok(pa) => {
                let mut eb = new_entries();
                let lr = lookup_entries(sref, &pa, &mut eb);
                match lr {
                    Err(fi) => { println!("lookup fail {0}", fi.param); return false; }
                    Ok(_) => {
                        let v = entries_at(&eb, 0).value;
                        return v == want;
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
    let a0 = n_alloc.load(Ordering::Relaxed);
    let f0 = n_free.load(Ordering::Relaxed);
    let mut sink = 0.0;
    let mut ne = 0usize;
    for k in 0 .. nq {
        let qs = qs_list[k % qs_list.len()].as_str();
        let pa_r = parse_query(qs);
        match pa_r {
            Err(_pf) => { assert!(false); }
            Ok(pa) => {
                let mut eb = new_entries();
                let lr = lookup_entries(sref, &pa, &mut eb);
                match lr {
                    Err(_fi) => { assert!(false); }
                    Ok(_) => {
                        let mut j = 0;
                        while j < eb.n {
                            let e = entries_at(&eb, j);
                            sink = sink + e.value + (e.units.len() as f64);
                            if e.masked { sink = sink + 1.0; }
                            j += 1;
                        }
                        ne = ne + eb.n;
                    }
                }
            }
        }
    }
    let a1 = n_alloc.load(Ordering::Relaxed);
    let f1 = n_free.load(Ordering::Relaxed);
    assert!(a1 == a0);
    assert!(f1 == f0);
    assert!(ne >= nq as usize);
    if sink == -12345.0 && ne == 0usize {
        // observer; never taken
    }

    // ---- masked path against the small fixture: null entry, still zero allocs
    let root2 = "/tmp/opencode/wxpoint-fullalloc-small";
    rm_tree(&root2);
    let ing2 = join_path(&root2, "ing");
    let data2 = join_path(&root2, "data");
    let _c3 = create_dir_all(&ing2);
    copy_file(small.to_string(), join_path(&ing2, "gfs_small.grib2"));
    let cfgp2 = join_path(&root2, "config.toml");
    let t2 = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t2 = format!("{0}{1}", t2, data2);
    let t2 = t2 + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ningest_threads = 2\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t2 = format!("{0}{1}", t2, ing2);
    let t2 = t2 + "\"\nfile_glob = \"gfs_small.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n";
    write_text(&cfgp2, t2);
    let pr2 = Pipeline::open(&cfgp2);
    let mut p2 = match pr2 {
        Err(_e) => { assert!(false); return; }
        Ok(pp) => { pp }
    };
    let _r1x = p2.scan_round();
    let r2x = p2.scan_round();
    assert!(r2x == 1);
    let snap2 = p2.shared.get_index();
    let sref2 = &*snap2;
    let mq = "model=gfs&param=TMP&level=isobaric:850&lat=10&lon=350&valid=2026-10-19T18:00:00Z".to_string();
    let a2 = n_alloc.load(Ordering::Relaxed);
    let f2 = n_free.load(Ordering::Relaxed);
    let mut masked_seen = 0usize;
    for _k in 0 .. 2000 {
        let pa_r = parse_query(mq.as_str());
        match pa_r {
            Err(_pf) => { assert!(false); }
            Ok(pa) => {
                let mut eb = new_entries();
                let lr = lookup_entries(sref2, &pa, &mut eb);
                match lr {
                    Err(_fi) => { assert!(false); }
                    Ok(_) => {
                        let e = entries_at(&eb, 0);
                        if e.masked { masked_seen = masked_seen + 1usize; }
                    }
                }
            }
        }
    }
    let a3 = n_alloc.load(Ordering::Relaxed);
    let f3 = n_free.load(Ordering::Relaxed);
    assert!(masked_seen == 2000usize);
    assert!(a3 == a2);
    assert!(f3 == f2);

    println!("full-handler lookup path: {0} mixed queries + 2000 masked queries, 0 allocations", nq);
    rm_tree(&root);
    rm_tree(&root2);
}
