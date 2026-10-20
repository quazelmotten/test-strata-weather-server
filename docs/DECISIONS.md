# Decisions log — GRIB2 decode (milestone 2)

Ambiguity points in the spec and the choices made, plus the grib-rs
evaluation outcome. Kept current per milestone.

## grib-rs evaluation (spec §4.2 "evaluate the `grib` crate first")

Evaluated `grib` v0.20.0 in a scratch crate (`/tmp/opencode/grib_eval`)
against our fixtures. Verdict: **not used; custom parser kept.** Findings:

* With default features it fails to build in this environment (the
  `openjpeg-sys`/`aec-sys` bindings need libclang, absent here). With
  `default-features = false, features = ["time-calculation"]` it compiles
  pure-Rust.
* It decodes wgrib2-NATIVE-framed files correctly (bare `7777` terminator):
  DRT 5.0, 5.3 incl. spatial differencing and bitmap all verified bit-exact
  against analytic fixture values (e.g. TMP@850 f006 nan-count 9).
* It REJECTS the bootstrap python-writer framing (`u4 len | 0x08 | ... | '7777'`
  end section): `ParseError(UnknownSectionNumber(8))` on every fixture — i.e.
  it is stricter than wgrib2 itself, which accepts both framings.
* Missing for our needs regardless: typed section-3 fields (no La1/Lo1/LoV/LaD/
  Latin1/Latin2/Dx/Dy/scan/earth-shape accessors — only raw payload + point
  count), flag-table 3.3 wind bit not exposed, PDT 4.8 statistical semantics
  not exposed, and its iterator API is reader-tied (awkward for our
  mmap-at-index-build storage plan).

Conclusion recorded in README as well: capability gap, not just packaging.

## Framing / terminators

* Section walk follows declared byte lengths (never filenames; spec §4.2).
* Message terminators accepted in BOTH forms: bare ASCII `7777` (what wgrib2
  writes) and LEN-prefixed end section (what our python writer emits). wgrib2
  itself accepts both; our fixtures keep the LEN-prefixed form and the
  wgrib2-converted `_c3` twins carry the bare form, exercising both paths.

## Data representations (section 7)

* Implemented: DRT 5.0 simple, 5.2 complex (no spatial diff), 5.3 complex +
  spatial differencing (first AND second order), 5.200 run length, 5.4 IEEE.
* **Deviation from task text:** the task listed "5.1 runlength / 5.2 dict";
  actual GRIB2/wgrib2 numbering has run length as **5.200** and there is NO
  dictionary encoder anywhere in the wgrib2 ecosystem. We implement 5.200 and
  reject anything else with typed `UnsupportedData(tmpl)`.
* 5.40 JPEG2000 / 5.41 PNG / 5.42 AEC: rejected with typed error (our nolib
  wgrib2 lacks jpeg/aec too, so fixtures only guard the header path; real
  GFS/HRRR pgrb2 files use 5.0/5.3 exclusively). Fallback decision: if NOAA
  ever ships 5.40 products, link ecCodes or shell out to wgrib2 — deferred,
  documented in README limitations.
* Complex packing quirk found by hexdump of the wgrib2 `-set_grib_type c3`
  output: the group-LENGTH phase reserves `ceil(ngroups * lbits / 8)` bytes
  even though only ngroups-1 lengths are stored (complex_pk.c pads one extra
  slot); the value bitstream starts after that padding. First implementation
  without the padding decoded rows past the first group wrong (constant
  offset); fixed and covered by the c3 twin tests.
* Bitmap (sec 6): indicator 255 = none; 0/254 = present, bit=1 means DEFINED
  (wgrib2 missing.c). Masked points -> NaN in `DecodedMessage.data`. Run-length
  decoder mirrors wgrib2's inverted bitmap convention on that path
  (documented quirk, matches `unpk_run_length.c`).
* Sign-magnitude everywhere for int2/int4 and the complex vmn (int_n), per
  wgrib2 int8.c.

## PDT 4.8 layout (stat_small.grib2)

Validated EMPIRICALLY against wgrib2 readback (`gen_fixtures.sh` greps the
displayed "acc"/"ave" ranges): prefix identical to PDT 4.0 through the fixed
surfaces, then stat valid-time (7 B), range count, four opaque bytes, and
12-byte blocks `[CT4.6 period][CT4.11 interval][range unit][u4 len][step unit]
[u4 len]`. Classification uses CT 4.6 (0 ave / 1 acc / 2 max / 3 min); the raw
CT 4.11 code is kept in `StatInfo.ct4_11`. CT 4.4 units map minute/hour/day/
3h/6h/12h/second; month/year units rejected (ambiguous).

## Levels

* Isobaric levels arrive in Pa (Code Table 4.5 scaling) and are normalized to
  hPa at ingest so they match config/API strings (`isobaric:850`). Documented
  choice; other level types keep native GRIB units.

## Wind rotation (spec §4.4)

* Grid-relative detection: flag table 3.3 bit 0x08 (verified vs NOTES item 7).
* Formula: with β = bearing of grid +i from true north (wx-core
  `rot_angle_deg`, inverse-projection based):
  `u_e = u_g·sinβ + v_g·cosβ`, `v_e = u_g·cosβ − v_g·sinβ`. For N→S-scanning
  LCC grids (+j ~ south) this is the symmetric orthogonal matrix
  [[sinβ, cosβ], [cosβ, −sinβ]] — an involution, magnitude-preserving.
* Validated three ways (wgrib2 nolib lacks `-new_grid_winds`):
  1. `scripts/validate_rotation.py` compares the rotated axes against pyproj
     3.8 `+proj=lcc` forward transforms at 8 nodes (axes agree < 5e-4);
  2. the Rust test checks β against Snyder's closed form β = 90 + n·(λ−LoV)
     (agreement ≤ 0.05°, pyproj-vs-Snyder spread 0.006° due to sphere vs
     ellipsoid radius);
  3. invariants: magnitude preserved (rel 1e-4), double application identity.
* Regular lat/lon grids: components treated as already earth-relative
  (identity), matching wgrib2 behavior for those grids.
* Config rule enforced via `check_wind_pair`: grid-relative model configuring
  only one of UGRD/VGRD -> validation error (test covers it).

## Earth shape

Code Table 3.2 byte honored via wx-core `earth_axes_m` (sphere 6371226.6 m;
spheroid major/minor constants). Fixtures use flag 6 (sphere).

## Parallelism

`decode_files_parallel` wraps file-level decode in rayon (`par_iter`),
documented as the hook milestone 3 will bind to a dedicated ingest pool
(spec §4.2). Per-message decode stays sequential inside one file.

## Test counts (this milestone)

* `gfs_decode.rs`: 5 tests — inventory order (39 msgs), analytic values,
  wgrib2 point checks + masked-NaN count 9, c3 (DRT 5.3) twin equality,
  skip-unpack counters (decoded 15 / skipped 24 for TMP-only filter).
* `hrrr_decode.rs`: 6 tests — flags/values, nearest-node vs -ll2ij, c3 twin,
  rotation invariants + pyproj-style analytic check, regular-grid identity,
  wind-pair config rule.
* `stat_pdt.rs`: 3 tests — PDT 4.8 kinds/ranges, point checks, J2K typed
  rejection.
* `parallel_decode.rs`: 1 test — rayon == sequential.
* Workspace totals: cargo test --workspace green (wx-core 22 + wx-ingest 15);
  clippy -D warnings green (style lints silenced module-wide where the local
  parser rejects canonical shorthands — same policy as wx-core).
