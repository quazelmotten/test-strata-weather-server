// Spec §3 startup validation: unknown names fail loudly; raw discipline/
// category/number triples are accepted (canonical name when known, dDcCnN key
// + "" units otherwise); wind-pair rule message for grid-relative models.

use wx_ingest::config::validate_text;
use wx_ingest::grib2::{check_wind_pair};

fn bytes_contains(hay: &[u8], pat: &[u8]) -> bool {
    if pat.len() > hay.len() {
        return false;
    }
    for s in 0 .. (hay.len() - pat.len() + 1) {
        let mut ok = true;
        for k in 0 .. pat.len() {
            if hay[s + k] != pat[k] {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }
    false
}

fn head(data: &str, ing: &str) -> String {
    let t = "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"";
    let t = format!("{0}{1}", t, data);
    let t = t + "\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 4\n\n[[models]]\nname = \"m\"\ningest_dir = \"";
    let t = format!("{0}{1}", t, ing);
    
    t + "\"\nfile_glob = \"x*\"\n"
}

#[test]
fn unknown_param_fails_loudly() {
    let t = format!("{0}{1}", head("d", "i"), "\n  [[models.params]]\n  name = \"BOGUS\"\n  levels = [\"isobaric:5\"]\n");
    match validate_text(&t) {
        Ok(_) => { panic!(); }
        Err(e) => {
            assert!(bytes_contains(e.as_bytes(), "unknown parameter 'BOGUS'".as_bytes()));
        }
    }
}

#[test]
fn raw_triple_accepted() {
    let t = format!("{0}{1}", head("d", "i"), "\n  [[models.params]]\n  discipline = 99\n  category = 98\n  number = 97\n  levels = [\"*\"]\n");
    match validate_text(&t) {
        Err(_) => { panic!(); }
        Ok(ms) => {
            assert!(ms.len() == 1);
            assert!(ms[0].params.len() == 1);
            assert!(ms[0].params[0].key == "d99c98n97");
            assert!(ms[0].params[0].units.is_empty());
            assert!(ms[0].params[0].disc == 99);
        }
    }
    // A raw triple that matches the table gets the canonical name + units.
    let t2 = format!("{0}{1}", head("d", "i"), "\n  [[models.params]]\n  discipline = 0\n  category = 0\n  number = 0\n  levels = [\"isobaric:1000\"]\n");
    match validate_text(&t2) {
        Err(_) => { panic!(); }
        Ok(ms) => {
            assert!(ms[0].params[0].key == "TMP");
            assert!(ms[0].params[0].units == "K");
        }
    }
}

#[test]
fn bad_level_retention_dupes_fail() {
    let t = format!("{0}{1}", head("d", "i"), "\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"bogus:5\"]\n");
    match validate_text(&t) {
        Ok(_) => { panic!(); }
        Err(e) => { assert!(bytes_contains(e.as_bytes(), "bad level spec".as_bytes())); }
    }
    let t2 = "[server]\nbind = \"b\"\ndata_dir = \"d\"\n\n[ingest]\nscan_interval_secs = 30\nretention_runs = 0\n\n[[models]]\nname = \"m\"\ningest_dir = \"i\"\nfile_glob = \"x*\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:1000\"]\n";
    match validate_text(t2) {
        Ok(_) => { panic!(); }
        Err(e) => { assert!(bytes_contains(e.as_bytes(), "retention_runs".as_bytes())); }
    }
    let t3 = format!("{0}{1}", head("d", "i") + "\n\n  [[models.params]]\n  name = \"HGT\"\n  levels = [\"isobaric:1000\"]\n\n[[models]]\nname = \"m\"\ningest_dir = \"j\"\nfile_glob = \"y*\"\n\n  [[models.params]]\n  name = \"TMP\"\n  levels = [\"isobaric:1000\"]\n", "");
    match validate_text(&t3) {
        Ok(_) => { panic!(); }
        Err(e) => { assert!(bytes_contains(e.as_bytes(), "duplicate model name".as_bytes())); }
    }
    let t4 = format!("{0}{1}", head("d", "i"), "\n  [[models.params]]\n  name = \"TMP\"\n  levels = []\n");
    match validate_text(&t4) {
        Ok(_) => { panic!(); }
        Err(e) => { assert!(bytes_contains(e.as_bytes(), "lists no levels".as_bytes())); }
    }
    let t5 = format!("{0}{1}", head("d", "i"), "\n  [[models.params]]\n  name = \"TMP\"\n  discipline = 0\n  category = 0\n  number = 0\n  levels = [\"isobaric:1000\"]\n");
    match validate_text(&t5) {
        Ok(_) => { panic!(); }
        Err(e) => { assert!(bytes_contains(e.as_bytes(), "both name and raw".as_bytes())); }
    }
}

#[test]
fn wind_pair_rule_message_clear() {
    let ps1: Vec<String> = vec!["TMP".to_string(), "UGRD".to_string()];
    match check_wind_pair(ps1.clone(), true) {
        None => { panic!(); }
        Some(msg) => {
            assert!(bytes_contains(msg.as_bytes(), "VGRD".as_bytes()));
            assert!(bytes_contains(msg.as_bytes(), "rotation requires both components".as_bytes()));
        }
    }
    let ps2: Vec<String> = vec!["TMP".to_string(), "UGRD".to_string(), "VGRD".to_string()];
    if let Some(_mg) = check_wind_pair(ps2.clone(), true) { panic!(); }
}
