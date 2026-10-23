// Spec §5 hot-path rules, proven empirically: a COUNTING global allocator
// wraps libc malloc/free, and we bracket the query hot path (snapshot load
// -> build_context -> evaluate_point) on the full-GFS perf fixture. V1.1
// phase 5: BOTH stages must allocate ZERO heap blocks over 20k queries
// (contexts borrow grid/units/mmap handles from the snapshot; selection
// resolves forecast hours and levels without scratch vectors). The
// companion test hotpath_full_alloc.rs extends the same harness to the FULL
// handler lookup path (query-string parse through entry structuring). This
// test also prints debug-build ns/query for the README ballpark numbers
// (release numbers come from cargo bench and scripts/loadtest.sh via /metrics).

use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::ffi::c_void;
use libc::malloc;
use libc::free;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use wx_ingest::pipeline::Pipeline;
use wx_ingest::index::{build_context, evaluate_point};
use wx_core::interp::{TimeInterpMode};
use wx_core::tables::{lookup_level_type};
use wx_server::stats::{mono_ns};
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
        // ask for one extra word to stash the raw pointer below the aligned
        // block; alignment >= 1 so (align - 1) rounding is safe.
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
fn hot_path_allocation_count_and_latency() {
    // conditional skip like perf_fixture when the big fixture is absent
    let perf = "../../tests/perfdata/gfs_perf_f000.grib2".to_string();
    let pr0 = File::open(&perf);
    if pr0.is_err() { return; }
    let root = "/tmp/opencode/wxpoint-hotpath".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ing");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    copy_file(perf, join_path(&ing, "gfs_perf_f000.grib2"));
    let cfgp = join_path(&root, "config.toml");
    let t = "[server]\nbind = \"b\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\ningest_threads = 4\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"gfs_perf_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n";
    write_text(&cfgp, t);

    let pr = Pipeline::open(&cfgp);
    let mut p = match pr {
        Err(_e) => { panic!(); }
        Ok(pp) => { pp }
    };
    let _r1 = p.scan_round();
    let r2 = p.scan_round();
    assert!(r2 == 1);

    let snap = p.shared.get_index();
    let sref = &*snap;
    let gm = sref.find_model("gfs");
    match gm {
        None => { panic!(); }
        Some(_mm) => {
            match lookup_level_type("isobaric") {
                None => { panic!(); }
                Some(iso) => {
                    // mid-cell query: bilinear + seam wrap at lon 359.9
                    let nq = 20000;

                    // ---- build+evaluate per-query allocation count (V1.1 phase 5:
                    // MUST BE ZERO; the old units-String/scratch-Vec allocations are
                    // gone, contexts borrow everything from the snapshot) -------
                    let a0 = N_ALLOC.load(Ordering::Relaxed);
                    let f0 = N_FREE.load(Ordering::Relaxed);
                    let t0 = mono_ns();
                    for _k in 0 .. nq {
                        let cr = build_context(sref, "gfs", None, "TMP", iso,
                                           1000.0, 0.0, TimeInterpMode::Linear);
                        match cr {
                            Err(_e) => { panic!(); }
                            Ok(c) => {
                                let v = evaluate_point(&c, 0.125, 359.9);
                                match v {
                                    Err(_e) => { panic!(); }
                                    Ok(pv) => {
                                        assert!(pv.value == 609.0);
                                    }
                                }
                            }
                        }
                    }
                    let t1 = mono_ns();
                    let a1 = N_ALLOC.load(Ordering::Relaxed);
                    let f1 = N_FREE.load(Ordering::Relaxed);
                    let alloc_total = a1 - a0;
                    let dealloc_total = f1 - f0;
                    // V1.1 phase 5 hard gate: selection+assembly allocate NOTHING.
                    assert!(alloc_total == 0);
                    assert!(dealloc_total == 0);
                    let ns_q = (t1 - t0) / nq as i64;

                    // ---- evaluate_point alone: ZERO allocations ---------
                    let cr2 = build_context(sref, "gfs", None, "TMP", iso,
                                         1000.0, 0.0, TimeInterpMode::Linear);
                    match cr2 {
                        Err(_e) => { panic!(); }
                        Ok(c2) => {
                            let a2 = N_ALLOC.load(Ordering::Relaxed);
                            let f2 = N_FREE.load(Ordering::Relaxed);
                            let t2 = mono_ns();
                            for _k in 0 .. nq {
                                let v2 = evaluate_point(&c2, 0.125, 359.9);
                                if let Err(_e) = v2 { panic!(); }
                            }
                            let t3 = mono_ns();
                            let a3 = N_ALLOC.load(Ordering::Relaxed);
                            let f3 = N_FREE.load(Ordering::Relaxed);
                            // the hot lookup path must not touch the allocator
                            assert!(a3 == a2);
                            assert!(f3 == f2);
                            let ns_ev = (t3 - t2) / nq as i64;
                            println!("hotpath debug: build+eval {0} ns/q (0 allocs), eval-only {1} ns/q (0 allocs)",
                                     ns_q, ns_ev);
                            let mut msg = "build+eval ".to_string();
                            msg = format!("{0}{1}", msg, ns_q);
                            msg = format!("{0}{1}", msg, " ns/q (0 allocs), eval-only ");
                            msg = format!("{0}{1}", msg, ns_ev);
                            msg = format!("{0}{1}", msg, " ns/q (0 allocs)");
                            write_text("/tmp/opencode/hotpath_debug.txt", msg);
                        }
                    }
                                    }
            }
        }
    }
    rm_tree(&root);
}
