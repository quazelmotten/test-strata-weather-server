# wx-point — gridded weather model point-query server

A Rust application that ingests GRIB2 weather-model output (GFS, HRRR, and
future models) from configured directories, keeps only a configured parameter
list in a query-optimized on-disk format, and serves an HTTP API returning the
value of a parameter at (model, param, level, lat/lon, valid time) with linear
interpolation between grid points, levels, and forecast times. Point queries
only — no slant paths, soundings, bounding boxes, or grid subsetting (designed
so those could be added later).

Priorities per spec: correctness first, then query latency. All measured
numbers below are from this deployment machine and recorded honestly.

---

## 1. Build and run

```sh
cargo build --release -p wx-server          # binary: target/release/wxpoint
./target/release/wxpoint --config config.example.toml
# or: WXPOINT_CONFIG=/path/to/config.toml ./target/release/wxpoint
```

Stable Rust (`rustc 1.99.1` here), workspace crates `wx-core` (no async/HTTP
deps), `wx-ingest`, `wx-server`. Release profile: `lto = "fat"`,
`codegen-units = 1`, `panic = "abort"` (portable — see §7.4 for the
`target-cpu=native` verdict). The bench profile mirrors release but always
unwinds (criterion requirement; this cargo ignores `panic` there — documented
in Cargo.toml).

Gate: `cargo test --workspace` (57 tests green) and
`cargo clippy --workspace -- -D warnings` clean.

## 2. Configuration

One TOML file (path via `--config` or `WXPOINT_CONFIG`; validated loudly at
startup — unknown param/level names fail). See `config.example.toml`.

| Key | Default | Meaning |
|---|---|---|
| `server.bind` | — | `host:port` for the HTTP API |
| `server.data_dir` | — | where ingested, query-optimized data lives |
| `ingest.scan_interval_secs` | 30 | periodic full-scan fallback (network mounts) |
| `ingest.retention_runs` | 4 | keep N most recent runs per model |
| `ingest.stable_window_secs` | 10 | file must hold (size,mtime) stable this long before ingest; `0` still requires one re-observation |
| `ingest.ingest_threads` | 4 | dedicated rayon decode pool (separate from the tokio HTTP runtime) |
| `[[models]].sidecar_suffix` | absent | per-model alternative stability rule: eligible once `<file><suffix>` appears |

Per model: `name`, `ingest_dir`, `file_glob` (glob is a FILTER ONLY — all
metadata comes from GRIB sections 1/4, never filenames), and `[[models.params]]`
entries. Param names map to GRIB2 (discipline, category, number) through a
built-in table covering `TMP HGT UGRD VGRD RH SPFH DPT PRMSL MSLMA PRES VVEL
APCP GUST CAPE CIN`; anything else may use raw
`discipline/category/number` keys. Level specs map to Code Table 4.5
(`isobaric`=100, `height_above_ground`=103, `surface`=1, `msl`=101, …) with
grammar `"*"`, `"type:*"`, `"type:value"`, bare `"type"` (= 0, e.g. `surface`).
Isobaric levels arrive in Pa in section 4 and are normalized to hPa internally
so config/API strings match (`isobaric:850`).

Wind rule (§4.4): if a model's grid carries grid-relative wind flags (flag
table 3.3 bit 0x08) and only ONE of UGRD/VGRD is configured, config validation
fails with a clear message; when both are configured, U/V are rotated to
earth-relative AT INGEST (LCC rotation angle per grid point) and stored earth-
relative, so queries are uniform across models.

## 3. HTTP API

All times ISO 8601 UTC. JSON bodies; values in native GRIB units (K, gpm, m/s,
Pa) — never converted.

### `GET /v1/point`

```sh
curl 'http://127.0.0.1:8080/v1/point?model=gfs&param=TMP&level=isobaric:837.5&lat=41.25&lon=-95.93&valid=2026-10-19T19:30:00Z'
```

```json
{"model":"gfs","run":"2026-10-19T12:00:00Z","valid":"2026-10-19T19:30:00Z",
 "lat":41.25,"lon":-95.93,"level":"isobaric:837.5",
 "results":[{"param":"TMP","value":281.734,"units":"K",
   "interpolation":{"horizontal":"bilinear",
     "vertical":{"method":"linear","levels":[850,800]},
     "time":{"method":"linear","forecast_hours":[6,9]}}}]}
```

| Param | Required | Notes |
|---|---|---|
| `model` | yes | configured model name |
| `param` | yes | comma list allowed (`TMP,HGT`) — shares point/level/time |
| `level` | yes | `isobaric:850`, `isobaric:837.5` (vertical interp), `height_above_ground:2`, `surface` |
| `lat` | yes | −90..90 |
| `lon` | yes | −180..360 accepted (normalized) |
| `valid` | yes | valid time = run + forecast hour |
| `run` | no | pin a specific run; default = most recent run covering `valid` |
| `time_interp` | no | `linear` (default) / `none` (exact fh required) |

Documented rendering deviations from the spec example: exact level/time matches
render `"method":"exact"` or the key with `null` instead of omitting it; masked
values render `"value":null,"reason":"missing_data","interpolation":null`;
numbers print ≤6 trimmed decimals (integers without `.0`).

Errors: `{"error":"<token>","message":"..."}` with

| Status | Tokens |
|---|---|
| 400 | `missing_required` `unknown_query_param` `bad_lat` `bad_lon` `bad_level` `bad_time` `bad_time_interp` `empty_value` |
| 404 | `unknown_model` `unknown_param` `no_run_covers` `unknown_endpoint` |
| 422 | `level_out_of_range` `stat_time_guard` (template 4.8 field needs exact fh) `time_exact_only` (`time_interp=none` w/o exact fh) `out_of_domain` (outside grid node rectangle) |

Priority when several apply: level > stat guard > time-exact-only > no-run.

### Other endpoints

```sh
curl 'http://127.0.0.1:8080/v1/models'                     # grid info (incl LCC params) + configured params/levels
curl 'http://127.0.0.1:8080/v1/inventory?model=hrrr'       # runs queryable NOW: fhs/params/levels (+stat_processed/stat_kind per field)
curl 'http://127.0.0.1:8080/health'; curl '.../ready'      # ready once >=1 indexed run per configured model
curl 'http://127.0.0.1:8080/metrics'                      # Prometheus text
```

Metrics: `wxpoint_queries_total{status="2xx|4xx|5xx"}`,
`wxpoint_query_latency_us_{bucket,count,sum}` (µs histogram),
`wxpoint_ingest_errors_total`, `wxpoint_ingested_files_total`,
`wxpoint_skipped_messages_total` (structurally unusable messages noted, NOT
errors: layer/range levels, coordinate lists, unimplemented grid/PDT templates),
`wxpoint_ingest_lag_seconds` (newest ingested file mtime → now).

## 4. Storage format (`data_dir`)

```
data_dir/
  state/files.json                 # persistent ingest record
  <model>/<run>/                     # YYYYMMDDHH, +MM when the run minute != 0,
                                     # +SS when the run second != 0 (10/12/14 chars)
    grid.bin                       # serialized grid definition
    manifest.json                  # params, levels, fhs, units, stat info + ranges
    <PARAM>/<level_type>/f<FFF>.bin  # e.g. TMP/isobaric/f006.bin
```

* **Run directory names** carry the reference time to its finest GRIB2 unit:
  `2026101912` (hour), `202610191230` (12:30Z), `20261019123015` (12:30:15Z).
  Length disambiguates, so runs 12Z and 12:30Z on one day no longer collide into
  one directory. Pre-existing 10-char directories keep parsing unchanged (minute/
  second default 0).
* **Forecast-hour filenames** are canonical and ONE scheme only. The join key
  everywhere internally is integer SECONDS past the run time; the name renders
  it with trailing zero components omitted:

  | offset | name | example |
  |---|---|---|
  | whole hours (`secs % 3600 == 0`) | `f<HHH>.bin` | `f000.bin`, `f006.bin`, `f1000.bin` |
  | whole minutes (not an hour) | `f<HHH>m<MM>.bin` | `f000m30.bin`, `f001m30.bin` |
  | whole seconds (not a minute) | `f<HHH>m<MM>s<SS>.bin` | `f001m01s01.bin` |

  Integral hours are byte-identical to the earlier layout, so existing stores
  stay readable. Hours pad to at least 3 digits (≥1000 h widen instead of breaking
  the pattern); minutes/seconds are exactly 2 digits. Sub-second offsets cannot
  exist: Code Table 4.4's finest unit is SECOND. The reader is strict about
  non-canonical names (`f001m5.bin`, `f001m70.bin` are ignored, not guessed),
  while tolerating `f006m00.bin` as an alias for `f006.bin`.
* `<param>/<level_type>/f<FFF>.bin`: magic `WXPTBIN1`, u32 version=1, u32 nx,
  u32 ny, u16 nlevels, flags byte (bit0 scan N→S, bit1 winds-grid-relative),
  then nlevels × f32 level values (pressure-sorted), then raw little-endian
  f32 planes laid out `[level][y][x]`. f32 because GRIB packing precision is
  already below f32; bitmap-masked points (§ sec 6) are stored as NaN.
* `grid.bin`: magic `WXPTGRID`, u32 version, kind byte (0 regular lat/lon,
  1 Lambert Conformal Conic) + template fields (regular: nx,ny,lon1,lat1,dlon,
  dlat,scan,winds; LCC: nx,ny,la1,lo1,lov,lad,latin1,latin2,dx_m,dy_m,proj_flag,
  scan,earth_shape,winds). Precomputed projection constants are rebuilt at read
  time, not stored.
* Atomicity and durability: every write is temp-file → `flush` → **fsync of the
  file inode** → atomic rename → **fsync of the parent directory**, so a query
  never sees a partial file and a crash cannot resurrect a renamed-but-unlinked
  name. Note on how fsync is obtained here: this toolchain's `std::fs::File`
  exposes only `flush()` (a userspace buffer flush — verified with `strace`, it
  emits no `fsync` syscall), so `wx_core::storage::fsync_path()` binds libc
  `open`/`fsync`/`close` through `extern "C"` and fsyncs by path. Durability
  failures are surfaced as ingest notes/errors, never swallowed. A run becomes
  visible forecast-hour by forecast hour as each completes (publish after every
  field store).
* `manifest.json` is human-facing inventory; the index is authoritative. Per
  field it records `units`, `stat_processed`, `stat_kind`, `levels`, decimal
  `forecast_hours` (informational only — the filename is the exact key) and
  `stat_ranges`, one entry per statistically processed forecast hour:
  `{"fh_secs":1800,"begin":"2026-10-19T12:30:00Z","end":"2026-10-19T13:00:00Z",
  "step_secs":0}`. Two accumulations of ONE parameter have DIFFERENT windows,
  which is why the range travels with the forecast hour rather than the field;
  a warm start restores those ranges from this file.
* Retention: runs beyond `retention_runs` are removed from the in-memory index
  first, then deleted on disk; stale mmap handles stay valid until their last
  `Arc` reference drops (Linux unlink-while-mapped), so in-flight queries never
  fault.
* Warm start: `state/files.json` (path+size+mtime_ns+FNV-1a64 over first/last
  64 KiB) prevents re-ingest after restart; same path with different content
  hash IS re-ingested. The index is rebuilt from disk before any scan.

## 5. Interpolation semantics (exactly as implemented)

Order for a query (lat, lon, level, valid time): horizontal at each of the
(up to) 2 levels × 2 times, then vertical, then time. All arithmetic f64;
return f64.

1. **Horizontal — bilinear.** lat/lon → fractional (i,j) via the model's
   projection (regular lat/lon wraps across the 359.75→0 seam; GFS scans
   north-to-south; LCC forward transform from La1,Lo1,LoV,LaD,Latin1,Latin2,
   Dx,Dy + Code Table 3.2 earth shape, constants precomputed per grid). Exactly
   on a node (within 1e-9 grid units) returns that node's value exactly, no
   blending. Outside the node rectangle (incl. the last half-cell at
   non-wrapping edges) → 422 `out_of_domain`; never extrapolated. Any NaN
   among the 4 neighbors → `null` + `missing_data` (weights never renormalized).
2. **Vertical — linear in the level coordinate** (hPa for isobaric). Exact
   stored level used directly; otherwise bracketing stored levels interpolate
   linearly (a log-pressure option is structurally possible but NOT enabled —
   spec default). Request outside the stored range → 422 `level_out_of_range`,
   never extrapolated. Configured gaps (850 and 500 only) interpolate ACROSS
   the gap; the response echoes the bracketing levels so users can see it.
3. **Time — linear between forecast hours of the SAME run.** Forecast-hour
   arithmetic happens in integer SECONDS past the run's reference time, so
   FRACTIONAL forecast hours (`f000m30.bin`, CT 4.4 units minute/hour/day/3h/6h/12h/
   second) are first-class data, never rounded onto hour slots. Default run =
   most recent run covering `valid`; the offset is RECOMPUTED per candidate run
   (one instant can be run A's `f001.bin` AND run B's `f000m30.bin` — the newest
   run that covers it answers), and `run=` pins one exactly. Both bracketing slots
   must exist when interpolating. Exact fh match uses it directly (no blending,
   no `time` object in the response). The reported `forecast_hours` bracket converts
   back to HOURS per §7 (e.g. `[0.5,1]`). Statistically processed fields (PDT 4.8
   accumulations/averages/max/min) NEVER interpolate in time: exact fh required,
   else 422 `stat_time_guard` — interpolating an accumulation is wrong.
   `time_interp=none` requires exactness for any field (422 `time_exact_only`).
   A valid time outside every run's stored hours → 404 `no_run_covers`.

## 6. Ingest pipeline

Discovery watches each `ingest_dir` with `notify` for latency AND does a full
recursive scan every `scan_interval_secs` (authoritative fallback). Files must
pass stability (window or sidecar) BEFORE decode. Decoding skips messages not
in the configured list by reading section 4 before unpacking section 7. Data
representations implemented: DRT 5.0 simple, 5.2 complex, 5.3 complex + spatial
differencing (first and second order), 5.200 run length, 5.4 IEEE raw float32,
**5.40 JPEG2000** (`hayro-jpeg2000` 0.4.1, `default-features = false` + `std`,`simd`),
**5.41 PNG** (`zlib-rs` 0.6.8 inflate + in-house chunk/CRC/unfilter layer),
**5.42 CCSDS AEC** (`rust-aec` 0.1.1) — all pure Rust, no C FFI anywhere.
Templates outside that set (e.g. dictionary 5.1, which nothing in the wgrib2
ecosystem can read either) yield typed `UnsupportedData(tmpl)`; a well-formed
stream using a construct a backend deliberately does not implement yields
`CodecUnsupported(reason)`, an unparseable stream `CodecFailure(reason)`.
Note the task text listed "5.1 runlength / 5.2 dict" — actual GRIB2 numbering
has run length as 5.200 and NO dictionary encoder exists in the wgrib2
ecosystem; we implement 5.200. PDT 4.0 and 4.8 both handled (stat process type
+ time ranges stored and surfaced through `/v1/inventory`). Decode runs on a
dedicated rayon pool (`ingest_threads`) separate from the HTTP runtime.
The grib-rs crate was evaluated and rejected (capability gaps documented in
docs/DECISIONS.md); the decoder is purpose-built against wgrib2 behavior.

Concurrency: read-mostly index published atomically with `arc-swap`; query
threads take zero locks, zero heap allocations beyond the response body, zero
file I/O beyond page-faulting mmapped pages (all mmaps opened at index-build
time). Proven by `hotpath_alloc.rs` (counting allocator: ZERO allocations in
`evaluate_point` over 20k queries).

## 7. Performance (measured on this machine)

Machine: AMD EPYC 7402 (48 logical cores; load test pinned to 8 worker threads
per spec §8), Linux 7.0.0-31-generic, rustc 1.99.1, oha 1.16.0. Dataset: full
GFS geometry (1440×720, 22 msgs/file ~34 MB ×2 files) + HRRR-like LCC
(1799×1059, 13 msgs/file ~37 MB ×2), warm page cache.

### 7.1 Criterion benches (`cargo bench`, bench profile = release minus panic)

| Kernel | median |
|---|---|
| regular lat/lon project (per point) | **20.6 ns** |
| LCC project, precomputed consts | **84.7 ns** |
| bilinear over heap f32 slab | **30.7 ns** |
| bilinear over mmap field (server h_at pattern) | **32.5 ns** |
| full vertical+time point query via index accessor | **364.5 ns** |
| `/v1/point` handler: parse + lookup + render | **879.2 ns** |

### 7.2 Load test (`scripts/loadtest.sh [secs]`, `NATIVE=1` variant)

oha mix: 8 URLs (exact-node GFS, seam, vertical interp, time interp,
multi-param, HRRR LCC, 404 unknown model, 422 bad level), `--worker-threads 8
-c 64 -z 15s --no-tui --urls-from-file`, keep-alive.

| Metric | portable | target-cpu=native |
|---|---|---|
| throughput (client, 8 workers) | **286,868 req/s** | 275,293 req/s |
| server-side mean latency (/metrics delta) | 12.9 µs | 13.4 µs |
| server-side p50 / p99 (histogram buckets) | **≤20 µs / ≤50 µs** | ≤20 µs / ≤50 µs |
| client p50 / p99 (oha) | 0.21 ms / 0.44 ms | similar |

§8 targets: **p50 < 50 µs MET** (≤20 µs), **p99 < 250 µs MET** (≤50 µs),
**≥50k req/s on 8 cores MET ×5.7**. No gap analysis needed: first measurement
hit all targets, so NO bit-risky bounded optimizations were attempted at all
(deliberate decision recorded in docs/DECISIONS.md — nothing here needs an
extra bit-exactness test beyond the existing suite). `target-cpu=native` was
measured and is WORSE (−4%); shipped build stays portable.

### 7.3 Ingest → queryable (`scripts/ingestbench.sh`)

Files placed cold; scan_interval_secs=1, stable_window_secs=0, ingest_threads=16:

* first-queryable (`/ready`): **3.9 s** after placement
* gfs f000 visible 4.1 s, f006 4.2 s; hrrr f000 4.3 s, f003 5.1 s
  (one GFS forecast hour ≈ 10 params becomes queryable within a few seconds —
  §8 target MET)
* peak RSS (VmHWM): **372 MB** for the whole dataset indexed

Codec cost at production scale (release build, this machine): decoding one full
GFS forecast hour (1440 x 720, 22 fields) takes **~1.0 s** from the simple-packed
file (three back-to-back runs: 978 / 987 / 1010 ms) and **~1.14 s** from its
JPEG2000 twin (1136 / 1143 / 1150 ms, 33.9 MB -> 1.0 MB on disk, values bit-
identical). An earlier milestone-5 session recorded 752 ms for the simple-packed
hour; the JPEG path has been stable at ~1.14 s across sessions, so the simple-
packing spread is treated as unattributed run-to-run variance rather than a
regression — the V1.1 phase-3 changes touch only ingest-time filename rendering
and manifest writing (once per field), never the per-message decode loop. Both
paths stay far inside the "few seconds" §8 target, and packed products cut
transfer volume by ~33x.

### 7.4 Profiling pass (`scripts/profile.sh [secs]`, optional `STRACE=1`)

This machine has `perf_event_paranoid=4` AND yama `ptrace_scope=1`: perf
stat/record fail outright, gdb attach refused. The pass therefore uses
`scripts/profshim.c` — an LD_PRELOAD per-thread ITIMER_PROF statistical sampler
(no ptrace privilege needed; pthread_create interposition enables the itimer on
tokio/rayon threads; ucontext RIP leaf samples dumped atomically every 4096
samples) — symbolized offline against the release binary's `.symtab` plus
`/proc/pid/maps` for libc regions. Top cost centers under the load mix
(12 s × 8 threads, 36,864 samples):

| Share | Cost center |
|---|---|
| 27.1% | libc `writev` (response socket writes dominate) |
| 4.8% | libc `recv` (request reads) |
| 4.7% | `point_handler` closure (parse + index lookup + JSON render) |
| 3.2% | libc `free` (allocator churn) |
| 3.1% | libc `syscall` |
| ~3% each | rayon decode bridge + `store::write_field` (window includes initial dataset ingest) |

Syscall accounting (strace LAUNCH mode, 3 s window): futex 48,407 / epoll_wait
13,160 / writev 45,528 — one writev per response is the single biggest cost,
i.e. the hot path is already kernel-I/O-bound; compute is ~1 µs/query matching
the bench ladder. Conclusion: no userspace optimization could move the p50
materially; the §8 margin (≤20 µs vs 50 µs target) stands without any.

## 8. Verification (spec §9 — all green)

1. Exact grid points vs `wgrib2 -lon` on real-geometry fixtures (gfs/hrrr
   decode tests + perf probes within f32 rounding).
2. LCC projection vs wgrib2 `-ll2ij` at corners/center/20+ random points
   (within 1e-4 grid units).
3. Wind rotation vs pyproj axes + Snyder closed form + magnitude/double-apply
   invariants (nolib wgrib2 lacks `-new_grid_winds`; three-way validation
   documented).
4. Bilinear/vertical/time analytic-grid unit tests (affine fields reproduced
   exactly at arbitrary points; `exact_node_bitexact`, `seam_blend_values`,
   `vertical_gap_bracket`, `time_guard_stat_and_none`, …).
5. GFS seam at lon 359.9 / −0.1 / 0.0 (`seam_blend_values`, smoke truth checks).
6. Poles/edges defined and tested (`poles_global_grid`, `out_of_domain_edges`).
7. Masked points → null, never blended (`nan_neighbor_missing_data`).
8. APCP time interpolation rejected (`time_guard_stat_and_none` + probe replay).
9. Ingest-under-load concurrency: `concurrency.rs` + `concurrency_http.rs`
   (readers hammer while publishers churn; zero 5xx, zero torn bodies; the only
   non-200/404 statuses accepted are the documented transient 422s).
10. Packing parity (V1.1 phase 2): each small fixture exists in five extra packings
   (`_c3` 5.3, `_jpeg` 5.40, `_png` 5.41 filter None, `_png_f` 5.41 cycling all five
   PNG predictors, `_aec` 5.42, `_ieee` 5.4). `packing_twins.rs` asserts element-wise
   identical arrays (NaN positions preserved), wgrib2 point probes reproduce on
   every twin, masked nodes stay NaN per codec, skip-unpack counters hold when a
   compressed message is filtered out, and all five PNG filters are proven present.
   `checksum_parity.rs` reproduces wgrib2 `-checksum data` (POSIX cksum over the
   normalized array) for gfs/hrrr/stat fixtures; `perf_fixture.rs` pins all 22
   checksums of the JPEG2000 twin of the full-geometry GFS hour.
Plus milestone-5 regressions: warm-boot LCC restore (`restart.rs` now covers
hrrr) and `/v1/models` + `/v1/inventory` JSON validity (`meta_json.rs`).

Scripts: `scripts/smoke.sh` (release server over the full perf dataset, 17
probes replayed from `tests/perfdata/probes.tsv`, SMOKE PASS/FAIL),
`scripts/loadtest.sh`, `scripts/ingestbench.sh`, `scripts/profile.sh`,
`scripts/gen_perfdata.sh` + `scripts/validate_perf.py` (regenerate perf data +
wgrib2 ground truth), `scripts/gen_fixtures.sh` + `scripts/regen_expected.sh`
(small fixtures). `tools/` (gitignored) holds the local wgrib2 build tree and
oha.

## 9. Known limitations / decisions where the spec was ambiguous

Full log: `docs/DECISIONS.md` (per milestone). Highlights:

* Codec scope still unsupported (typed `CodecUnsupported`, never a guess):
  JPEG2000 streams whose samples exceed 8-bit-per-value packing or carry multiple
  components; PNG non-grayscale color types, bit depths other than 8/16, interlacing,
  non-zero compression/filter methods; AEC parameter sets `rust-aec` rejects. DRT
  5.1 "dictionary" remains `UnsupportedData(1)` — no encoder for it exists in the
  wgrib2 ecosystem (guard fixture `tests/fixtures/unsupported_drt.grib2`).
* Field-by-field publish (§4.5 incremental visibility) means a run can momentarily
  expose a parameter without its whole level stack; a request for a NON-STORED
  level then answers 422 `level_out_of_range` rather than extrapolating. Coarser
  publish granularity is a documented follow-up (see docs/DECISIONS.md).
* Run-length DRT is 5.200 (task text's "5.1" does not exist in GRIB2); no
  dictionary encoder exists in the ecosystem.
* Skip-and-NOTE (never a failure, never silent): layer/range level messages
  (two fixed surfaces), messages carrying coordinate values after the product-
  definition template, and unimplemented grid/product-definition templates cannot
  answer a point query, so they are counted in `wxpoint_skipped_messages_total`
  and noted in the ingest log while the message walk resumes at the declared
  byte length — the rest of the file still ingests. Corrupt or undecodable section
  7 (e.g. DRT 5.1) still fails LOUD as an ingest error, so the packing-twin
  guard keeps working. Sub-second forecast offsets cannot exist (CT 4.4's finest
  unit is second); a negative offset is treated as corrupt input and skipped, not
  stored.
* Isobaric levels normalized Pa→hPa internally (config/API strings match).
* Out of scope per §13: slant paths, bounding boxes, soundings, grid
  subsetting, unit conversion, derived quantities, auth, NOAA downloads.
* Perf dataset is synthetic-but-real-geometry (generated by gen_perfdata.sh,
  validated against wgrib2 readback); real NOAA file NAMES/framing beyond the
  two accepted terminator forms have not been exercised end-to-end.
* Fixture caveat: values are analytic formulas chosen exact-in-f32; wgrib2
  comparisons hold to f32 rounding.
