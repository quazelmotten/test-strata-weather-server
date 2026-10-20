// rayon-backed multi-file decode (spec §4.2: ingest decoding parallel across
// messages/files). Checks per-file results equal the sequential results.

use wx_ingest::grib2::decode_files_parallel;
use wx_ingest::grib2::decode_file;
use wx_ingest::grib2::ParamFilter;

fn f_is_nan(x: f32) -> bool {
    return x != x;
}

fn spath(name: &str) -> String {
    return format!("../../tests/fixtures/{name}");
}

#[test]
fn parallel_matches_sequential() {
    let mut paths: Vec<String> = Vec::<String>::new();
    paths.push(spath("gfs_small.grib2"));
    paths.push(spath("hrrr_small.grib2"));
    paths.push(spath("stat_small.grib2"));
    let results = decode_files_parallel(paths.clone(), ParamFilter::All);
    assert!(results.len() == 3);
    let want_counts: [usize; 3] = [39, 6, 3];
    for x in 0 .. 3 {
        match &results[x] {
            Err(_) => { assert!(false); }
            Ok((msgs, _st)) => {
                assert!(msgs.len() == want_counts[x]);
                // cross-check against sequential decode of the same file
                let seq = decode_file(paths[x].to_string(), ParamFilter::All);
                match seq {
                    Err(_) => { assert!(false); }
                    Ok((sm, _ss)) => {
                        assert!(sm.len() == msgs.len());
                        for y in 0 .. sm.len() {
                            let a = &sm[y].data;
                            let b = &msgs[y].data;
                            assert!(a.len() == b.len());
                            for k in 0 .. a.len() {
                                if f_is_nan(a[k]) {
                                    assert!(f_is_nan(b[k]));
                                } else {
                                    assert!(a[k] == b[k]);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
