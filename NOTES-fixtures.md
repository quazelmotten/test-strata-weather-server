# Fixture generation notes (wgrib2 v3.1.3, built locally)

Reference binary: `/home/frankzappa/dylantest2/tools/wgrib2` (system PATH wgrib2 is broken — always use this absolute path).

## Surprises found while reverse-engineering GRIB2 byte layouts

1. **Section 0 magic is `GRIB`, not `GRID`.** `rd_seq_grib.c` compares G,R,I,B (71,82,73,66) and requires octet 8 == 2 (edition). A file with `GRID` magic is silently skipped by this reader.
2. **Signed integers in sections 3 and 5 are SIGN-MAGNITUDE, not two's complement.** wgrib2 `int8.c::int4()`/`int2()`: bit 31 (bit 15 for int2) is the sign, remaining bits are the magnitude. Writing standard two's-complement negatives (e.g. any negative latitude) makes wgrib2 read them as `INT_MIN + |value|` → "BAD grid definition lat". This bit it is critical for our Rust GRIB decoder too: when *reading* real files, section-3 signed fields must be decoded sign-magnitude.
3. **Isobaric levels are stored in Pascals** in section 4 (1000 mb → 100000); display divides by 100. Levels are Code Table 4.5 *fixed surfaces* at sec4 octets 22–28, not coordinate-list entries.
4. **Section 6 is mandatory for this build**: the CHECK path calls `code_table_6_0` unconditionally and segfaults if sec6 is absent. Always emit sec6 (indicator 255 = no bitmap). Real NCEP files always include it.
5. **Exact ±90° latitudes are rejected** by a float range check in `geo.c` (`90000000 * 1e-6 > 90.0`). Perf grids use lat1 = 89.875 (ny = 720 at 0.25°).
6. **No `-dir` and no `-ll` options in v3.1.3.** The lat/lon → (i,j) option is **`-ll2ij LON LAT`** (returns 1-based i,j; j counts from the south edge for our Lambert grid). Negative coordinates on the CLI parse fine only when the *file argument comes first*: `wgrib2 FILE -lon LON LAT`.
7. **Wind grid-relative flag** = bit 0x08 of flag table 3.3 (`res & 8` in FlagTable.c → `winds(grid)` in `-vector_dir`). Verified on hrrr_small.grib2.
8. **ct 4.3 gen-process values:** 0 = anl (analysis; shown for f000), 1 = init, 2 = fcst. ct 4.4 unit 1 = hour.
9. **Packing templates:** fixtures now cover BOTH simple packing 5.0 (gfs_small, hrrr_small) and complex packing + spatial differencing 5.3 via wgrib2 `-set_grib_type c3` twins (gfs_small_c3, hrrr_small_c3 — verified by `-code_table_5.0` showing `code table 5.0=3` for every record). JPEG2000 5.40/PNG 5.41 remain unexercised (nolib wgrib2 cannot encode them); `unsupported_j2k.grib2` guards only the header-rejection path.
10. PDT 4.8 is now generated (`stat_small.grib2`) and validated against wgrib2 readback; see docs/DECISIONS.md for the byte layout.

## Fixture inventory

### tests/fixtures/gfs_small.grib2 (regular lat/lon, template 3.0)
- Grid 41 × 21, lon 350 → 10 (0.5°, wraps across 0), lat 10 → 0 (0.5°), scan N→S.
- Messages: TMP @ {1000, 850, 700} mb + TMP @ 2 m AGL + UGRD/VGRD @ 10 m AGL, forecast hours 0/6/12 (39 records).
- Bitmap masking applied to TMP@850mb f006 (masked points decode to `val=9.999e+20` = UNDEFINED).
- Values analytic: `base + incr * 2^E`, exact in f32.

### tests/fixtures/gfs_small_c3.grib2 / hrrr_small_c3.grib2
- Byte-identical CONTENT twins of gfs_small / hrrr_small re-encoded by wgrib2
  with DRT 5.3 complex packing + spatial differencing (`-set_grib_type c3`),
  bare `7777` terminators. Decoded values must equal the 5.0 versions exactly.

### tests/fixtures/hrrr_small.grib2 (Lambert conformal, template 3.30)
- Grid 61 × 41, La1 = 21.11389, Lo1 = 233.44, LoV = 229.53, LaD = 26.5, Latin1 = 26.5, Latin2 = 35.0, Dx = Dy = 3000 m, projflag = 1 (normal), scan +i +j.
- Messages: TMP @ 2 m AGL, UGRD/VGRD @ 10 m AGL (UGRD/VGRD carry flag 3.3 bit 0x08 = grid-relative winds), f000/f003 (6 records).

### tests/fixtures/stat_small.grib2 (PDT 4.8, regular 11 × 7)
- APCP surface accumulations 0-6 h (fcst hour 0) and 6-12 h (fcst hour 6); TMP 2 m AGL 3-6 h average (fcst hour 3). Values analytic (base 10/250 + j*11+i). wgrib2 displays the acc/ave ranges on readback (checked in gen_fixtures.sh).

### tests/fixtures/unsupported_j2k.grib2
- Single message declaring DRT 5.40 (JPEG2000); decoder must return typed `UnsupportedData(40)`. Never decoded by wgrib2 nolib (no jpeg support).

### tests/perfdata/* (gitignored; regenerate: scripts/gen_perfdata.sh + scripts/validate_perf.py)
- `gfs_perf_f000.grib2` / `gfs_perf_f006.grib2`: full GFS geometry 1440 × 720
  (0.25°, lat 89.875 → −89.875 N→S, lon 0 → 359.75), 22 messages each
  (~33.9 MB): TMP@{2m AGL, 1000..200 mb}, HGT@{8 isobarics}, UGRD/VGRD@10m,
  RH@2m, PRMSL@surface, GUST@10m; DRT 5.0. Values analytic & exact-in-f32:
  TMP@1000 = 250 + j + fh·10 (back-compat with the old single-message file and
  tests/perf_fixture.rs), other GFS params = base + ((j·1440+i) mod 4096) + fh·10.
- `hrrr_perf_f000.grib2` / `hrrr_perf_f003.grib2`: HRRR-like LCC 1799 × 1059
  (La1 21.11389, Lo1 233.44, LoV 226.25, LaD 26.5, Latin1/2 26.5/35.0, Dx=Dy
  3000 m, scan byte 0x40 = rows go NORTH from La1 — real-HRRR convention,
  validated vs wgrib2 `-ll2ij`), 13 msgs each (~37.2 MB), grid-relative winds
  rotated at ingest. Affine fields base + i + j + fh·10 so OFF-node bilinear
  probes reproduce within ~1e-3.
- `probes.tsv`: 17 ground-truth probes (url/mode/expected; grep|near|status)
  replayed by scripts/smoke.sh against both models.

## Regeneration
- `scripts/gen_fixtures.sh` regenerates all fixtures idempotently (wraps `scripts/gen_grib2.py`).
- `scripts/regen_expected.sh` re-probes wgrib2 and rewrites `tests/fixtures/expected.json`.
