// V1.4 phase A GRACEFUL-SHUTDOWN GATE (black-box, unix-only): the binary used
// to run a BARE axum serve(), so SIGTERM/SIGINT took their DEFAULT
// DISPOSITION — the process died mid-response with no drain (reproduced
// pre-A: WIFSIGNALED exit, no drain log). Now main drives
// serve(...).with_graceful_shutdown(shutdown_signal()): SIGTERM (unix
// SignalKind) and SIGINT (tokio ctrl_c) stop the accept loop, IN-FLIGHT
// requests finish, serve returns Ok, and the process exits 0 between two
// pinned log lines. The STARTUP exit codes (usage 2, config 1, pipeline 3,
// preflight 4, runtime 5, bind 6, provisional wind 7, serve-error 8) are
// untouched by this change — tests/exit_codes.rs keeps pinning them, and the
// clean-stop row 0 ("served normally (or stopped cleanly)") is what these
// two probes now also prove from the OTHER side (a signaled stop is a CLEAN
// stop).
//
// SYNC STRATEGY: the child binds 127.0.0.1:0 (ephemeral port: no collision
// with any other suite member, and we never need to CONNECT — the stdout
// line "listening on ..." is the registration barrier). A short settle
// delay covers the microsecond window between printing that line and tokio
// registering the signal handlers on first poll; without it a hyperactive
// parent could send SIGTERM before the handler exists (default disposition
// would then win — flaky-red, not product-wrong).

#![cfg(unix)]

use std::env::var;
use std::fs::File;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::remove_dir;
use std::fs::read_dir;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read as ReadTrait;
use std::io::Write as WriteTrait;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;
use wx_ingest::state::{join_path};
use wx_ingest::discovery::{path_to_str};

extern "C" {
    // libc::kill: std's Child::kill() only ever sends SIGKILL, which would
    // bypass with_graceful_shutdown entirely (same extern-"C" style as
    // exit_codes.rs declares system()).
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGINT_C: i32 = 2;
const SIGTERM_C: i32 = 15;

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

#[allow(clippy::needless_return)]
fn file_exists(p: &str) -> bool {
    let m = std::fs::metadata(p);
    return m.is_ok();
}

#[allow(clippy::needless_return)]
fn binary_path() -> String {
    // cargo exports CARGO_BIN_EXE_<name> to integration tests; fall back to
    // the debug output directory relative to this package's root (identical
    // pattern to exit_codes.rs).
    let v = var("CARGO_BIN_EXE_wxpoint");
    match v {
        Err(_) => {
            let fb = "../../target/debug/wxpoint".to_string();
            assert!(file_exists(&fb));
            return fb;
        }
        Ok(s) => {
            let p = s.to_string();
            assert!(file_exists(&p));
            return p;
        }
    }
}

// Empty ingest dir: boot must succeed VACUOUSLY (no files for the wind-pair
// preflight to judge, /ready irrelevant — we never query).
#[allow(clippy::needless_return)]
fn drain_cfg(data: &str, ing: &str) -> String {
    return format!("[server]\nbind = \"127.0.0.1:0\"\ndata_dir = \"{0}\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"gfs\"\ningest_dir = \"{1}\"\nfile_glob = \"gfs_*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n", data, ing);
}

// Spawn -> wait for "listening on" on stdout -> settle -> signal -> reap.
// Returns the FULL stdout log collected by the time the child exits plus its
// ExitStatus. Panic (with everything seen so far) on any handshake failure.
#[allow(clippy::needless_return)]
fn run_signaled_server(root: String, sig: i32) -> (String, std::process::ExitStatus) {
    rm_tree(&root);
    let ing = join_path(&root, "ingest");
    let data = join_path(&root, "data");
    let _c = create_dir_all(&ing);
    let cfgp = join_path(&root, "config.toml");
    write_text(&cfgp, drain_cfg(&data, &ing));

    let bin = binary_path();
    let mut child_c = Command::new(&bin);
    child_c.arg("--config");
    child_c.arg(&cfgp);
    child_c.stdout(Stdio::piped());
    child_c.stderr(Stdio::null());
    let spawn_r = child_c.spawn();
    let mut child: Child = match spawn_r {
        Err(_e) => { panic!("failed to spawn {0}", bin); }
        Ok(c) => { c }
    };
    let take_r = child.stdout.take();
    let pipe = match take_r {
        None => { panic!("no stdout pipe"); }
        Some(s) => { s }
    };
    let mut rd = BufReader::new(pipe);

    // barrier: "listening on" means the accept loop is live and the graceful
    // shutdown future has been created alongside it.
    let mut seen = "".to_string();
    let mut listening = false;
    let mut guard = 0usize;
    while guard < 50 && !listening {
        guard += 1;
        let mut line = String::new();
        let nr = rd.read_line(&mut line);
        match nr {
            Err(_e) => { panic!("read error on child stdout (saw: {0})", seen); }
            Ok(0) => { panic!("child exited before binding (saw: {0})", seen); }
            Ok(_) => {
                seen.push_str(line.as_str());
                if line.find("listening on").is_some() {
                    listening = true;
                }
            }
        }
    }
    if !listening {
        panic!("never saw 'listening on' (saw: {0})", seen);
    }
    // settle: handlers register on the signal future's FIRST poll, a hair
    // after the println above; belt-and-milliseconds against that race.
    sleep(Duration::from_millis(300));

    let pid = child.id() as i32;
    let kr = unsafe { kill(pid, sig) };
    if kr != 0 {
        panic!("kill({0}, {1}) failed", pid, sig);
    }

    // bounded reap: a hung drain must fail the test, not hang the suite.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status;
    loop {
        let tw = child.try_wait();
        match tw {
            Err(_e) => { panic!("try_wait error"); }
            Ok(Some(st)) => { status = st; break; }
            Ok(None) => {
                if Instant::now() > deadline {
                    panic!("child did not exit within 10 s of the signal (saw: {0})", seen);
                }
                sleep(Duration::from_millis(25));
            }
        }
    }
    // child is reaped; the stdout pipe now runs to EOF — collect the rest.
    let mut tail = String::new();
    let rr = rd.read_to_string(&mut tail);
    if rr.is_err() {
        tail = "".to_string();
    }
    seen.push_str(tail.as_str());
    // best-effort cleanup while root is still owned here (tests pass it in
    // by value; a leftover dir only costs disk, never determinism).
    rm_tree(&root);
    return (seen, status);
}

// SIGTERM: handler installed, accept stops, in-flight connections drain,
// serve returns Ok, process exits 0 with BOTH drain and stop lines logged.
// Pre-fix red: default disposition KILLED the child -> non-success exit
// status (signal 15) and neither log line existed.
#[test]
fn sigterm_drains_connections_and_exits_zero() {
    let root = "/tmp/opencode/wxpoint-drain-term".to_string();
    let pair = run_signaled_server(root, SIGTERM_C);
    assert!(pair.1.success(), "exit status was {0:?} (a signal-killed process reports success()==false)", pair.1);
    assert!(pair.0.find("SIGTERM received; draining in-flight connections").is_some());
    assert!(pair.0.find("server stopped").is_some());
}

// SIGINT (via tokio's ctrl_c listener — same drain path, different arm).
#[test]
fn sigint_drains_connections_and_exits_zero() {
    let root = "/tmp/opencode/wxpoint-drain-int".to_string();
    let pair = run_signaled_server(root, SIGINT_C);
    assert!(pair.1.success(), "exit status was {0:?}", pair.1);
    assert!(pair.0.find("SIGINT received; draining in-flight connections").is_some());
    assert!(pair.0.find("server stopped").is_some());
}
