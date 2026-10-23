// V1.3 phase E4a DEGENERATE SECTION 7 TOLERANCE. A real HRRR APCP message
// delivered on Oct 22 2026 (212 bytes TOTAL, preserved by the orchestrator at
// /tmp/opencode/hrrr_apcp_tight.grib2) carries section 7 as exactly FIVE
// octets — length + section number, ZERO data values — with DRT 5.0, a short
// 21-octet section 5, and NO bitmap (indicator 255). wgrib2-full decodes it as
// a constant-zero field (`mean=0:min=0:max=0 undef=0`); our pre-E4a framing
// floor (`ln < 6`) rejected it with GribErr::Truncated. This test builds both
// degenerate shapes from gfs_small.grib2's own sections at runtime (no new
// fixture bytes, no network) and pins:
//
//   A. empty s7 + NO bitmap   -> decodes, plane is ngrid zeros (wgrib2 parity,
//                                the reference bar set by spec §9).
//   B. empty s7 + full-zero bitmap (all points undefined) -> EVERY value NaN;
//                                an end-to-end query answers
//                                PointError::MissingData (spec §4.2: missing
//                                points are NaN, never a blended number).
//   C/D. genuinely truncated streams STILL fail loudly with Truncated:
//        a message length past EOF, and a section 7 whose declared length
//        overruns the message body.
//
// Output size ALWAYS comes from the grid definition (41*21 = 861 here), never
// from section 5's npoints field.

use std::fs;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::remove_dir;
use std::fs::remove_file;
use std::fs::File;
use std::io::Write;

use wx_core::interp::{PointError, TimeInterpMode};
use wx_core::tables::lookup_level_type;
use wx_ingest::discovery::path_to_str;
use wx_ingest::grib2::{decode_stream, GribErr, ParamFilter};
use wx_ingest::index::{build_context, evaluate_point};
use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::join_path;

fn fixture(name: &str) -> String {
    format!("../../tests/fixtures/{name}")
}

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

fn be_u32(b: &[u8], o: usize) -> u64 {
    ((b[o] as u64) << 24) | ((b[o + 1] as u64) << 16)
         | ((b[o + 2] as u64) << 8) | (b[o + 3] as u64)
}

fn be_u64v(b: &[u8], o: usize) -> u64 {
    let mut v = 0u64;
    let mut k = 0;
    while k < 8 {
        v = (v << 8) | (b[o + k] as u64);
        k += 1;
    }
    v
}

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.push(((v >> 24) & 0xFF) as u8);
    out.push(((v >> 16) & 0xFF) as u8);
    out.push(((v >> 8) & 0xFF) as u8);
    out.push((v & 0xFF) as u8);
}

fn push_u64(out: &mut Vec<u8>, v: u64) {
    let mut k = 8;
    while k > 0 {
        k -= 1;
        out.push(((v >> (8 * k)) & 0xFF) as u8);
    }
}

/// Section headers of the message at `pos`: (number, start, length).
fn walk_sections(b: &[u8], pos: usize) -> Vec<(u8, usize, usize)> {
    let msglen = be_u64v(b, pos + 8) as usize;
    let body_end = pos + msglen;
    let mut out: Vec<(u8, usize, usize)> = Vec::new();
    let mut q = pos + 16;
    while q < body_end {
        if b[q] == 0x37 && b[q + 1] == 0x37 && b[q + 2] == 0x37 && b[q + 3] == 0x37 {
            break;
        }
        let ln = be_u32(b, q) as usize;
        let num = b[q + 4];
        out.push((num, q, ln));
        if ln == 0 {
            break;
        }
        q += ln;
    }
    out
}

/// Copy sections 1/3/4 verbatim out of gfs_small's FIRST message. The grid
/// stays a legal regular 41 x 21 template 3.0 and the product definition a
/// legal TMP-on-isobaric-levels template 4.0, so ONLY sections 5-7 differ from
/// every other fixture message — exactly the shape of the live APCP sample.
fn donor_sections() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let r = fs::read(fixture("gfs_small.grib2"));
    let b: Vec<u8> = match r {
        Err(_) => { println!("no gfs_small fixture"); panic!(); }
        Ok(v) => { v }
    };
    let secs = walk_sections(&b, 0);
    let mut s1: Vec<u8> = Vec::new();
    let mut s3: Vec<u8> = Vec::new();
    let mut s4: Vec<u8> = Vec::new();
    for s in &secs {
        let slice = &b[s.1 .. s.1 + s.2];
        if s.0 == 1 { s1.extend_from_slice(slice); }
        if s.0 == 3 { s3.extend_from_slice(slice); }
        if s.0 == 4 { s4.extend_from_slice(slice); }
    }
    assert!(s1.len() >= 5 && s3.len() >= 5 && s4.len() >= 5);
    (s1, s3, s4)
}

/// Assemble one degenerate message. `bitmap_bytes`: None = section 6 with
/// indicator 255 (NO bitmap, the live APCP shape); Some(n) = indicator 0 plus
/// n zero bitmap octets (all points undefined). `s7_len_override` rewrites the
/// declared section-7 length without adding bytes (truncation probe).
fn build_msg(bitmap_bytes: Option<usize>, s7_len_override: Option<u32>,
             msglen_override: Option<u64>) -> Vec<u8> {
    let (s1, s3, s4) = donor_sections();
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&s1);
    body.extend_from_slice(&s3);
    body.extend_from_slice(&s4);
    // section 5: 21 octets total, mirroring the live file (payload: npoints
    // then zeros; DRT reads as 5.0 at the parser's offset p[4:6]).
    push_u32(&mut body, 21);
    body.push(5);
    push_u32(&mut body, 861); // npoints field (ignored by the degenerate path)
    let mut k = 0;
    while k < 12 { body.push(0); k += 1; }
    // section 6
    match bitmap_bytes {
        None => {
            push_u32(&mut body, 6);
            body.push(6);
            body.push(0xFF);
        }
        Some(nb) => {
            push_u32(&mut body, (6 + nb) as u32);
            body.push(6);
            body.push(0x00);
            let mut m = 0;
            while m < nb { body.push(0); m += 1; }
        }
    }
    // section 7: FIVE octets, zero data values (or an over-declared length).
    let s7_len: u32 = s7_len_override.unwrap_or(5);
    push_u32(&mut body, s7_len);
    body.push(7);
    body.push(0x37); body.push(0x37); body.push(0x37); body.push(0x37);

    let real_len = 16u64 + body.len() as u64;
    let mut out: Vec<u8> = vec![0x47, 0x52, 0x49, 0x42]; // GRIB
    out.push(0);           // reserved (octet 5)
    out.push(0);           // reserved (octet 6)
    out.push(0);           // discipline 0 (at parser offset +6, verified against
                           // live NOAA bytes: GFS ACOND msg carries 0x02 here)
    out.push(2);           // edition 2 (parser offset +7)
    push_u64(&mut out, match msglen_override { Some(v) => { v } None => { real_len } });
    out.extend_from_slice(&body);
    out
}

// ---------------------------------------------------------------------------
// A. empty section 7, no bitmap -> all-zero plane (wgrib2 parity)
// ---------------------------------------------------------------------------

#[test]
fn empty_sec7_without_bitmap_decodes_as_zeros() {
    let bytes = build_msg(None, None, None);
    let r = decode_stream(&bytes, ParamFilter::All);
    match r {
        Err(e) => {
            println!("degenerate s7 (no bitmap) failed: {0:?}", e);
            panic!();
        }
        Ok((msgs, st)) => {
            assert!(st.messages_total == 1);
            assert!(st.messages_unsupported == 0);
            assert!(msgs.len() == 1);
            // size governed by the GRID DEFINITION, never by section 5
            assert!(msgs[0].data.len() == 861);
            let mut bad = 0usize;
            for v in &msgs[0].data {
                if *v != 0.0 { bad += 1; }
            }
            assert!(bad == 0);
        }
    }
}

// ---------------------------------------------------------------------------
// B. empty section 7 + all-undefined bitmap -> every value NaN
// ---------------------------------------------------------------------------

#[test]
fn empty_sec7_with_full_bitmap_decodes_as_all_nan() {
    // 861 points -> ceil(861/8) = 108 bitmap octets, all zero bits = missing.
    let bytes = build_msg(Some(108), None, None);
    let r = decode_stream(&bytes, ParamFilter::All);
    match r {
        Err(e) => {
            println!("degenerate s7 (bitmap) failed: {0:?}", e);
            panic!();
        }
        Ok((msgs, st)) => {
            assert!(st.messages_unsupported == 0);
            assert!(msgs.len() == 1);
            assert!(msgs[0].data.len() == 861);
            let mut defined = 0usize;
            for v in &msgs[0].data {
                if !v.is_nan() { defined += 1; }
            }
            // zero value octets exist for ANY point -> whole plane missing
            assert!(defined == 0);
        }
    }
}

// ---------------------------------------------------------------------------
// C/D. genuinely truncated streams still fail LOUDLY
// ---------------------------------------------------------------------------

#[test]
fn truly_truncated_streams_still_fail_loudly() {
    // C: message length declares far more bytes than the file holds.
    let c = build_msg(None, None, Some(1u64 << 40)); // wildly past EOF
    match decode_stream(&c, ParamFilter::All) {
        Err(GribErr::Truncated) => {}
        Err(other) => {
            println!("wrong error for bogus msglen: {0:?}", other);
            panic!();
        }
        Ok(_) => {
            println!("bogus msglen decoded silently");
            panic!();
        }
    }
    // D: section 7 claims 10 octets but only 5 exist before the 7777 tail,
    // so the declared length overruns the message body by one octet.
    let d = build_msg(None, Some(10), None);
    match decode_stream(&d, ParamFilter::All) {
        Err(GribErr::Truncated) => {}
        Err(other) => {
            println!("wrong error for overlong s7: {0:?}", other);
            panic!();
        }
        Ok(_) => {
            println!("overlong s7 decoded silently");
            panic!();
        }
    }
}

// ---------------------------------------------------------------------------
// E. end-to-end: the masked degenerate field answers null missing_data
// ---------------------------------------------------------------------------

fn degen_cfg_text(root: &str) -> String {
    let ing = join_path(root, "ingest");
    let data = join_path(root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"degen\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"degen*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\"]\n"
}

#[test]
fn masked_degenerate_field_queries_return_missing_data() {
    // The level value carried by the donor sec4 (read from the decoder itself,
    // so this test cannot drift from the fixture).
    let probe = build_msg(Some(108), None, None);
    let rp = decode_stream(&probe, ParamFilter::All);
    let want_level = match rp {
        Err(_) => { panic!(); }
        Ok((msgs, _)) => {
            assert!(msgs[0].levels.len() == 1 && msgs[0].levels[0].0 == 100);
            msgs[0].levels[0].1
        }
    };

    let root = "/tmp/opencode/wxpoint-degensec7".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ingest");
    let _c = create_dir_all(&ing);
    match File::create(join_path(&ing, "degenmask.grib2")) {
        Err(_) => { panic!(); }
        Ok(mut w) => {
            let n = w.write_all(&probe);
            if n.is_err() { panic!(); }
        }
    }
    let cfgp = join_path(&root, "config.toml");
    match File::create(&cfgp) {
        Err(_) => { panic!(); }
        Ok(mut w) => {
            let _n = w.write_all(degen_cfg_text(&root).as_bytes());
        }
    }

    let po = Pipeline::open(&cfgp);
    let mut p = match po {
        Err(e) => { println!("open failed: {0}", e); panic!(); }
        Ok(pp) => { pp }
    };
    let _n1 = p.scan_round();
    let n2 = p.scan_round();
    // the degenerate message is a SUCCESS at every layer: no errors, no skips
    assert!(n2 == 1);
    assert!(p.counters.errors == 0);
    assert!(p.counters.skipped_messages == 0);

    let snap = p.shared.get_index();
    let sref = &*snap;
    match sref.find_model("degen") {
        None => { panic!(); }
        Some(m) => {
            assert!(m.runs.len() == 1);
            match lookup_level_type("isobaric") {
                None => { panic!(); }
                Some(iso_code) => {
                    let cb = build_context(sref, "degen", None, "TMP", iso_code,
                                           want_level, 0.0, TimeInterpMode::Linear);
                    match cb {
                        Err(e) => {
                            println!("build_context failed: {0:?}", e);
                            panic!();
                        }
                        Ok(c) => {
                            // node j=2,i=3 of the 41x21 grid (same probe point
                            // the other gfs_small-derived tests use)
                            let v = evaluate_point(&c, 9.0, 351.5);
                            match v {
                                Err(PointError::MissingData) => {}
                                Err(other) => {
                                    println!("wrong point error: {0:?}", other);
                                    panic!();
                                }
                                Ok(pv) => {
                                    println!("masked plane returned a value: {0}", pv.value);
                                    panic!();
                                }
                            }
                            // and a second in-domain point behaves identically
                            let v2 = evaluate_point(&c, 8.5, 350.5);
                            match v2 {
                                Err(PointError::MissingData) => {}
                                _ => { panic!(); }
                            }
                        }
                    }
                }
            }
        }
    }
    rm_tree(&root);
}
