// V1.3 phase E4a SKIP-REASON OBSERVABILITY. The decoder already distinguished
// three STRUCTURAL skip kinds (coordinate lists / unknown grid templates /
// unknown product-definition templates) but only ever reported ONE aggregate
// count, so an operator staring at "2 messages skipped as structurally
// unsupported" could not tell WHICH construct was missing. This test pins the
// per-kind machinery end-to-end WITHOUT new fixture files: the unknown-grid
// and unknown-PDT messages are produced by PATCHING two template-number bytes
// inside a copy of gfs_small.grib2 at runtime (byte-exact otherwise), and the
// coordinate-list message comes from the existing robust_small.grib2.
//
// Pinned expectations:
//   * DecodeStats tallies per kind: skip_coordlists (u32) plus (template ->
//     count) lists skip_grid / skip_pdt.
//   * skip_tally_text renders "N skipped: X coordinate-list, Y grid template
//     3.<t>, Z PDT 4.<t>" — the exact string the ingest note appends.
//   * Pipeline: counters.skip_coordlists/_grid/_pdt advance per kind, the
//     LIVE mirror advances with them, and the note text carries the breakdown.
//   * Policy skips (the 850-800 LAYER message from robust_small) count ONLY in
//     skipped_messages — the three kind-counters partition the STRUCTURAL
//     subset, never the policy subset (docs/DECISIONS.md §15).

use std::fs;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::remove_dir;
use std::fs::remove_file;
use std::fs::File;
use std::io::Write;
use std::sync::atomic::Ordering;

use wx_ingest::discovery::path_to_str;
use wx_ingest::grib2::{decode_stream, skip_tally_text, ParamFilter};
use wx_ingest::pipeline::Pipeline;
use wx_ingest::state::join_path;

fn fixture(name: &str) -> String {
    return format!("../../tests/fixtures/{name}");
}

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

fn write_bytes(path: &str, data: &[u8]) {
    let d = File::create(&path);
    match d {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let n = w.write_all(data);
            match n {
                Err(_) => { assert!(false); }
                Ok(_) => {}
            }
        }
    }
}

fn be_u32(b: &[u8], o: usize) -> u64 {
    return ((b[o] as u64) << 24) | ((b[o + 1] as u64) << 16)
         | ((b[o + 2] as u64) << 8) | (b[o + 3] as u64);
}

fn be_u64v(b: &[u8], o: usize) -> u64 {
    let mut v = 0u64;
    let mut k = 0;
    while k < 8 {
        v = (v << 8) | (b[o + k] as u64);
        k += 1;
    }
    return v;
}

/// Section headers of the message starting at `pos`: (number, start, length).
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
    return out;
}

/// Message start offsets of a whole GRIB stream.
fn message_starts(b: &[u8]) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut pos = 0usize;
    while pos + 16 <= b.len() {
        out.push(pos);
        pos += be_u64v(b, pos + 8) as usize;
    }
    return out;
}

/// Find the section-start offset of section `num` inside message `mi`.
fn section_of(b: &[u8], starts: &Vec<usize>, mi: usize, num: u8) -> usize {
    let secs = walk_sections(b, starts[mi]);
    for s in &secs {
        if s.0 == num {
            return s.1;
        }
    }
    assert!(false);
    return 0;
}

// ---------------------------------------------------------------------------
// decoder level: per-kind tallies + the exact note string
// ---------------------------------------------------------------------------

#[test]
fn patched_templates_tally_by_kind() {
    let r = fs::read(fixture("gfs_small.grib2"));
    let mut b: Vec<u8> = match r {
        Err(_) => { println!("no gfs_small fixture"); assert!(false); return; }
        Ok(v) => { v }
    };
    let starts = message_starts(&b);
    assert!(starts.len() >= 2);

    // Sanity: UNPATCHED, every message decodes (zero structural skips).
    let pre = decode_stream(&b, ParamFilter::All);
    match pre {
        Err(e) => { println!("pre-patch decode failed {:?}", e); assert!(false); return; }
        Ok((_, st)) => {
            assert!(st.messages_unsupported == 0);
            assert!(skip_tally_text(&st) == "".to_string());
        }
    }

    // Patch message 0's grid-definition template number to 40 (the REAL value
    // found on live GFS sflux bytes in phase E4a) and message 1's product-
    // definition template number to 77 (never assigned). Payload byte offsets:
    // sec3 template at payload p[7:9], sec4 template at p[2:4] — the same
    // offsets the parser reads (validated against wgrib2 -Sec3/-Sec4).
    let q3 = section_of(&b, &starts, 0, 3);
    b[q3 + 5 + 7] = 0x00;
    b[q3 + 5 + 8] = 40;
    let q4 = section_of(&b, &starts, 1, 4);
    b[q4 + 5 + 2] = 0x00;
    b[q4 + 5 + 3] = 77;

    let rr = decode_stream(&b, ParamFilter::All);
    match rr {
        Err(e) => { println!("patched decode failed {:?}", e); assert!(false); return; }
        Ok((msgs, st)) => {
            assert!(st.messages_total as usize == starts.len());
            assert!(st.messages_unsupported == 2);
            assert!(msgs.len() + 2 == starts.len());
            assert!(st.skip_coordlists == 0);
            assert!(st.skip_grid.len() == 1 && st.skip_grid[0].0 == 40 && st.skip_grid[0].1 == 1);
            assert!(st.skip_pdt.len() == 1 && st.skip_pdt[0].0 == 77 && st.skip_pdt[0].1 == 1);
            let t = skip_tally_text(&st);
            assert!(t == "2 skipped: 1 grid template 3.40, 1 PDT 4.77".to_string(),
                    "tally text was: {0}", t);
        }
    }

    // Concatenate with robust_small.grib2 (one coordinate-list message): GRIB
    // streams are self-framed, so file A ++ file B is one legal stream. The
    // combined tally must show all THREE kinds in the documented order.
    let rb = fs::read(fixture("robust_small.grib2"));
    let both: Vec<u8> = match rb {
        Err(_) => { println!("no robust_small fixture"); assert!(false); return; }
        Ok(v) => {
            let mut w: Vec<u8> = Vec::new();
            w.extend_from_slice(&b);
            w.extend_from_slice(&v);
            w
        }
    };
    let rc = decode_stream(&both, ParamFilter::All);
    match rc {
        Err(e) => { println!("combined decode failed {:?}", e); assert!(false); return; }
        Ok((_, st)) => {
            assert!(st.skip_coordlists == 1);
            assert!(st.skip_grid.len() == 1 && st.skip_grid[0].0 == 40 && st.skip_grid[0].1 == 1);
            assert!(st.skip_pdt.len() == 1 && st.skip_pdt[0].0 == 77 && st.skip_pdt[0].1 == 1);
            assert!(st.messages_unsupported == 3);
            let t = skip_tally_text(&st);
            assert!(t == "3 skipped: 1 coordinate-list, 1 grid template 3.40, 1 PDT 4.77".to_string(),
                    "combined tally was: {0}", t);
        }
    }
}

// ---------------------------------------------------------------------------
// pipeline level: note text + flat per-kind counters (structural vs policy)
// ---------------------------------------------------------------------------

fn mix_cfg_text(root: &str) -> String {
    let ing = join_path(&root, "ingest");
    let data = join_path(&root, "data");
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\nstable_window_secs = 0\n\n[[models]]\nname = \"skipmix\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    let t = t + "\"\nfile_glob = \"skipmix*.grib2\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:*\", \"height_above_ground:2\"]\n";
    return t;
}

#[test]
fn pipeline_note_and_counters_break_down_skips_by_kind() {
    let root = "/tmp/opencode/wxpoint-skipreasons".to_string();
    rm_tree(&root);
    let ing = join_path(&root, "ingest");
    let _c = create_dir_all(&ing);

    // Build the same combined stream as the decoder test above.
    let r = fs::read(fixture("gfs_small.grib2"));
    let mut b: Vec<u8> = match r {
        Err(_) => { assert!(false); return; }
        Ok(v) => { v }
    };
    let starts = message_starts(&b);
    let q3 = section_of(&b, &starts, 0, 3);
    b[q3 + 5 + 7] = 0x00;
    b[q3 + 5 + 8] = 40;
    let q4 = section_of(&b, &starts, 1, 4);
    b[q4 + 5 + 2] = 0x00;
    b[q4 + 5 + 3] = 77;
    let rb = fs::read(fixture("robust_small.grib2"));
    match rb {
        Err(_) => { assert!(false); return; }
        Ok(v) => { b.extend_from_slice(&v); }
    }
    write_bytes(&join_path(&ing, "skipmix.grib2"), &b);

    let cfgp = join_path(&root, "config.toml");
    match File::create(&cfgp) {
        Err(_) => { assert!(false); return; }
        Ok(mut w) => {
            let _n = w.write_all(mix_cfg_text(&root).as_bytes());
        }
    }

    let po = Pipeline::open(&cfgp);
    let mut p = match po {
        Err(e) => {
            println!("pipeline open failed: {0}", e);
            assert!(false);
            return;
        }
        Ok(pp) => { pp }
    };
    let n1 = p.scan_round();
    let n2 = p.scan_round();
    assert!(n1 == 0);
    assert!(n2 == 1);
    assert!(p.counters.errors == 0);

    // THREE structural skips (grid 40, PDT 77, coordinate list) PLUS the
    // 850-800 LAYER message rejected by the pipeline's level filter: four
    // total in skipped_messages, but only three in the kind counters — the
    // documented partition between decoder gaps and policy decisions.
    assert!(p.counters.skip_coordlists == 1);
    assert!(p.counters.skip_grid == 1);
    assert!(p.counters.skip_pdt == 1);
    assert!(p.counters.skipped_messages == 4);

    // The live mirror advanced with the counters (metrics freshness path).
    assert!(p.live.skip_coordlists.load(Ordering::Relaxed) == 1);
    assert!(p.live.skip_grid.load(Ordering::Relaxed) == 1);
    assert!(p.live.skip_pdt.load(Ordering::Relaxed) == 1);

    // The note names every missing construct by template number.
    let mut saw_note = false;
    for line in &p.log {
        if line.find("skipped as structurally unsupported").is_some() {
            if !(line.find("grid template 3.40").is_some()
                 && line.find("PDT 4.77").is_some()
                 && line.find("coordinate-list").is_some()) {
                println!("note lacks breakdown: {0}", line);
                assert!(false);
            }
            saw_note = true;
        }
    }
    assert!(saw_note);

    rm_tree(&root);
}

// ---------------------------------------------------------------------------
// real-data diagnosis gate lives in tests/realdata_gate.rs (manual fixtures)
// ---------------------------------------------------------------------------

#[test]
fn decode_file_on_real_gfs_sflux_skips_everything_as_grid_40() {
    // The preserved live-fire artifacts (orchestrator, Oct 22 2026) sit under
    // /tmp/opencode/realtest; when present this test pins the E4a diagnosis:
    // ALL SIX messages across the three "GFS" files ride grid template 3.40,
    // so the ONLY skip kind is Grid(40) and NOTHING decodes. When absent
    // (any other machine / CI) it prints why and passes vacuously.
    let dir = "/tmp/opencode/realtest/in_gfs";
    let names = ["gfs.t211200z.pgrb2.0p25.f000",
                 "gfs.t211200z.pgrb2.0p25.f006",
                 "gfs.t211200z.pgrb2.0p25.f012"];
    let mut seen = 0;
    for nm in names.iter() {
        let path = format!("{0}/{1}", dir, nm);
        let rd = fs::read(&path);
        let b: Vec<u8> = match rd {
            Err(_) => { continue; }
            Ok(v) => { v }
        };
        seen += 1;
        let r = decode_stream(&b, ParamFilter::All);
        match r {
            Err(e) => {
                println!("real GFS {0} failed to walk: {1:?}", nm, e);
                assert!(false);
                return;
            }
            Ok((msgs, st)) => {
                if !(st.messages_total >= 1 && msgs.is_empty()
                     && st.messages_decoded == 0
                     && st.skip_coordlists == 0
                     && st.skip_pdt.is_empty()
                     && st.skip_grid.len() == 1
                     && st.skip_grid[0].0 == 40
                     && st.skip_grid[0].1 == st.messages_total
                     && st.messages_unsupported == st.messages_total) {
                    println!("unexpected stats for {0}: total={1} decoded={2} kinds={3:?} pdt={4:?}",
                             nm, st.messages_total, st.messages_decoded, st.skip_grid, st.skip_pdt);
                    assert!(false);
                    return;
                }
                let t = skip_tally_text(&st);
                if !(t.find("grid template 3.40").is_some()) {
                    println!("tally text missing 3.40: {0}", t);
                    assert!(false);
                    return;
                }
            }
        }
    }
    if seen == 0 {
        println!("SKIP decode_file_on_real_gfs: {0} absent on this machine", dir);
    } else {
        println!("real-GFS grid-40-only skips pinned for {0} file(s)", seen);
    }
}
