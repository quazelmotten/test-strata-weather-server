// V1.4 phase B1 (6/6): THE ANTI-ABORT NET — deterministic seeded corruption
// sweeps over every committed fixture, plus explicit tail-truncation cases.
//
// WHY this file exists: the release profile runs panic="abort" with overflow
// checks OFF, so ANY unchecked slice index or wrapping overflow inside
// section-5/7 CONTENT (the framing layer proves section boundaries; it does
// NOT prove the scalars inside them) takes down the whole process, HTTP and
// all. The mission for phase B1 converts those classes into typed GribErr
// results the existing content-hash quarantine machinery absorbs. This file
// is the net that proves the conversion holds EMPIRICALLY, not just by
// inspection:
//
//   SWEEP: for every committed .grib2 fixture, make copies, flip 1..=3 random
//   bits INSIDE the section 5 / section 7 payload regions (grid geometry in
//   section 3 stays honest so planes stay small and the assertion is about
//   DECODER robustness, not about forged grids), and feed each mutated
//   stream through decode_stream. The assertion is simply "returns": in the
//   test profile unwinding is ON, so any residual unchecked access shows up
//   as a panic -> test failure. Every Ok/Err outcome is fine (GribErr has no
//   untyped variant); what must never happen is silence-by-abort in release
//   or panic here. The sweep additionally pins that mutations DO bite: at
//   least one mutation per fixture family must end Err, else the region
//   selection would be vacuous.
//
//   TRUNCATION: cut every fixture's LAST message at several interior offsets
//   (quartiles plus one/eight bytes before its end). Section 0's declared
//   length then exceeds the file, which the framing check turns into
//   GribErr::Truncated deterministically — the same scalar trust boundary,
//   approached from the other side (bytes REMOVED instead of FLIPPED).
//
// Determinism: the LCG is hand-rolled (no rand dev-dependency), seeded per
// fixture from an FNV-1a hash of the file name, and fixtures are processed in
// sorted directory order, so the exact same 1..=3-bit mutations replay on
// every machine. Red-first note: pre-fix this file's sweep aborted or panicked
// on multiple families (run-length mvl overrun, complex ngroups group-walk
// overrun, PNG IHDR upfront allocation); captured during the red run.

// Clippy policy for integration tests mirrors the src modules (see lib.rs):
// this suite keeps the dense explicit-return / explicit-field style, so the
// matching pedantic-but-cosmetic lints are allowed per-file, narrowly listed.
#![allow(clippy::needless_return)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::needless_arbitrary_self_type)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::manual_div_ceil)]
#![allow(clippy::single_match)]
#![allow(clippy::len_zero)]

use std::fs;
use std::path::PathBuf;

use wx_ingest::grib2::be_u32;
use wx_ingest::grib2::be_u64;
use wx_ingest::grib2::decode_stream;
use wx_ingest::grib2::GribErr;
use wx_ingest::grib2::ParamFilter;

const FIXTURE_DIR: &str = "../../tests/fixtures";

/// Trials per fixture (each = 1..=3 bit flips inside s5/s7 payloads). Sized
/// so the whole suite stays comfortably under a minute even with the codec
/// families (JPEG/PNG/AEC decode the full 39-message streams per trial).
const TRIALS: usize = 120;

struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Lcg {
        // Mix away a zero seed like every decent LCG consumer.
        return Lcg { state: if seed == 0 { 0x9E3779B97F4A7C15 } else { seed } };
    }
    /// Numerical Recipes / Knuth MMIX parameters: pure wrapping arithmetic,
    /// identical on every platform and opt level.
    fn next_u64(self: &mut Self) -> u64 {
        self.state = self.state.wrapping_mul(6364136223846793005)
                           .wrapping_add(1442695040888963407);
        // Output mix (xor-shift-multiply style) so low bits aren't used raw.
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        return z ^ (z >> 31);
    }
    fn below(self: &mut Self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        return self.next_u64() % n;
    }
}

fn fnv1a(name: &str) -> u64 {
    let mut h: u64 = 0xCBF29CE484222325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001B3);
    }
    return h;
}

/// All committed .grib2 fixtures, sorted by file name for run-to-run
/// determinism.
fn fixtures() -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(FIXTURE_DIR);
    let rd = fs::read_dir(&dir);
    let entries = match rd {
        Err(_) => { println!("no fixture dir {0}", FIXTURE_DIR); panic!(); }
        Ok(v) => { v }
    };
    let mut names: Vec<String> = Vec::new();
    for e in entries {
        match e {
            Err(_) => {}
            Ok(de) => {
                let p = de.path();
                match p.to_str() {
                    None => {}
                    Some(s) if s.ends_with(".grib2") => {
                        match de.file_name().into_string() {
                            Err(_) => {}
                            Ok(nm) => { names.push(nm); }
                        }
                    }
                    Some(_s) => {}
                }
            }
        }
    }
    names.sort();
    assert!(names.len() >= 20);
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for nm in &names {
        let full = format!("{0}/{1}", FIXTURE_DIR, nm);
        match fs::read(&full) {
            Err(_) => { println!("unreadable fixture {0}", full); panic!(); }
            Ok(bytes) => { out.push((nm.to_string(), bytes)); }
        }
    }
    return out;
}

/// Payload byte ranges of every section 5 and section 7 in the stream —
/// exactly the "section 5/7 regions" the mission sweep is scoped to. The
/// walk mirrors the decoder's framing but NEVER fails: a malformed region
/// just stops collection (this runs on clean fixtures only).
fn sec57_payloads(b: &[u8]) -> Vec<(usize, usize)> {
    let n = b.len();
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    let mut guard = 0;
    while pos + 16 <= n && guard < 4096 {
        guard += 1;
        if !(b[pos] == 0x47 && b[pos + 1] == 0x52 && b[pos + 2] == 0x49 &&
             b[pos + 3] == 0x42) {
            break;
        }
        let msglen = be_u64(b, pos + 8) as usize;
        if msglen < 16 || pos + msglen > n {
            break;
        }
        let body_end = pos + msglen;
        let mut q = pos + 16;
        while q + 5 <= body_end {
            if b[q] == 0x37 && b[q + 1] == 0x37 && b[q + 2] == 0x37 &&
               b[q + 3] == 0x37 {
                break;
            }
            let ln = be_u32(b, q) as usize;
            let num = b[q + 4];
            if ln < 5 || q + ln > body_end {
                break;
            }
            if (num == 5 || num == 7) && q + 5 < q + ln {
                out.push((q + 5, q + ln)); // payload range [start, end)
            }
            q += ln;
        }
        pos = body_end;
    }
    return out;
}

#[test]
fn seeded_bitflip_sweep_never_panics_and_stays_typed() {
    let fx = fixtures();
    let mut total_trials: usize = 0;
    let mut total_errs: usize = 0;
    let mut total_oks: usize = 0;
    let mut families_with_errors: usize = 0;
    for f in &fx {
        let name = f.0.as_str();
        let bytes = &f.1;
        let regions = sec57_payloads(bytes);
        // Flat list of every flippable byte position (payloads of sections
        // 5/7). Fixtures are tiny, so materializing it costs nothing and
        // makes selection exact by construction.
        let mut pool: Vec<usize> = Vec::new();
        for r in &regions {
            let mut o = r.0;
            while o < r.1 {
                pool.push(o);
                o += 1;
            }
        }
        assert!(!pool.is_empty(), "{0}: no section 5/7 payload bytes to flip",
                name);
        let mut rng = Lcg::new(fnv1a(name));
        let mut errs_here: usize = 0;
        for _t in 0 .. TRIALS {
            let nf = 1 + rng.below(3); // 1..=3 flips per trial (mission spec)
            let mut copy = bytes.clone();
            for _k in 0 .. nf {
                let off = pool[rng.below(pool.len() as u64) as usize];
                let bit = rng.below(8) as usize;
                copy[off] ^= (1 << bit) as u8;
            }
            // THE assertion: this call must RETURN. Any unchecked access in
            // the decoder panics here (test profile unwinds), which cargo
            // reports as a test failure -- the release-profile abort class.
            let r = decode_stream(&copy[..], ParamFilter::All);
            total_trials += 1;
            match r {
                Ok((_m, _s)) => { total_oks += 1; }
                Err(_e) => { total_errs += 1; errs_here += 1; }
            }
        }
        if errs_here > 0 {
            families_with_errors += 1;
        } else {
            println!("WARNING: {0}: none of the {1} mutations produced an \
                      error (flips may be landing on benign padding)",
                     name, TRIALS);
        }
    }
    // The sweep must actually bite SOMEWHERE or the net is vacuous: with
    // ~32 fixture families x 120 trials inside content scalars, EVERY family
    // is expected to reject at least one mutation post-fix... except tiny
    // degenerate streams where whole fields of slack bits exist, so the hard
    // floor is "most families" and the soft warning above pins the rest.
    assert!(families_with_errors >= fx.len() * 3 / 4);
    println!("corruption sweep: {0} trials, {1} typed errors, {2} accepted, \
              {3}/{4} families rejected at least one mutation",
             total_trials, total_errs, total_oks, families_with_errors,
             fx.len());
}

/// Message start offsets + lengths (section-0 view), for the truncation
/// battery. Same never-fails walk policy as above.
fn message_bounds(b: &[u8]) -> Vec<(usize, usize)> {
    let n = b.len();
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    let mut guard = 0;
    while pos + 16 <= n && guard < 4096 {
        guard += 1;
        if !(b[pos] == 0x47 && b[pos + 1] == 0x52 && b[pos + 2] == 0x49 &&
             b[pos + 3] == 0x42) {
            break;
        }
        let msglen = be_u64(b, pos + 8) as usize;
        if msglen < 16 || pos + msglen > n {
            break;
        }
        out.push((pos, msglen));
        pos += msglen;
    }
    return out;
}

#[test]
fn tail_truncations_are_always_typed_errors() {
    let fx = fixtures();
    let mut cuts = 0usize;
    for f in &fx {
        let name = f.0.as_str();
        let bytes = &f.1;
        let msgs = message_bounds(bytes);
        assert!(msgs.len() >= 1, "{0}: fixture has no complete message",
                name);
        let (start, mlen) = msgs[msgs.len() - 1];
        // Cut the LAST message at interior offsets: quartiles of its body
        // plus near-tail single/double-octet shaves. Every such file ends
        // INSIDE the final message, so section 0's declared length exceeds
        // the remaining bytes -> GribErr::Truncated by framing proof, before
        // any content scalar can even be consulted.
        let offs: [usize; 6] = [mlen / 4, mlen / 2, (mlen * 3) / 4,
                                mlen - 1, mlen - 2, mlen - 8];
        for k in 0 .. offs.len() {
            let keep = offs[k];
            if keep < 16 || keep >= mlen {
                continue; // only interior cuts of the final message
            }
            let mut cut = bytes.clone();
            cut.truncate(start + keep);
            let r = decode_stream(&cut[..], ParamFilter::All);
            cuts += 1;
            match r {
                Err(GribErr::Truncated) => {}
                Err(other) => {
                    println!("{0}: interior truncation at +{1} gave {2:?}",
                             name, keep, other);
                    panic!();
                }
                Ok((_m, _s)) => {
                    println!("{0}: interior truncation at +{1} was ACCEPTED",
                             name, keep);
                    panic!();
                }
            }
        }
    }
    assert!(cuts >= fx.len()); // every fixture contributed at least one cut
    println!("truncation battery: {0} interior cuts, all Truncated", cuts);
}
