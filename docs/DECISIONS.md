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

---

# Milestone 3 decisions (ingest pipeline + index)

## Config schema additions (all optional; defaults documented)

* `[ingest].stable_window_secs` (default 10): window strategy for file
  stability.
* `[ingest].ingest_threads` (default 4): dedicated rayon pool for decode work
  (spec §4.2 "separate thread pool").
* `[[models]].sidecar_suffix` (absent by default): per-model sidecar-marker
  strategy instead of the window (spec §4.1 "or once a sidecar marker
  appears... make the strategy configurable").
* Raw param override: a `[[models.params]]` entry may omit `name` and give
  `discipline`/`category`/`number`; key becomes `d{D}c{C}n{N}` and units ""
  unless the triple matches the built-in table (then canonical name+units).
* Level spec grammar: `"*"`, `"type:*"`, `"type:value"`, bare `"type"` (= 0,
  e.g. `surface`).

## Stability semantics

* Window rule: first sighting records (size,mtime,first_seen); eligible only
  when a LATER observation sees identical (size,mtime) AND marker-clock
  elapsed >= window. With window=0 this still needs a second scan round —
  deliberate: "stable for the window" always implies at least two samples.
* Wall clock: this toolchain lacks an epoch-time API, so `now_ns_marker(dir)`
  stats the mtime of a freshly created `<state_dir>/.wxpoint-clock`. The
  persisted record stores mtime_ns via `MetadataExt` (mtime()*1e9+nsec).
* Half-written files are skipped BEFORE decoding (stability gates the decode
  call; test asserts errors counter stays 0 on a truncated GRIB).

## Persistent record + state file

* `data_dir/state/files.json`: path + size + mtime_ns + FNV-1a64 over first/
  last 64 KiB (whole file if < 128 KiB), written atomically (tmp+rename).
* Fixed: hash multiply now `wrapping_mul` (FNV requires modular arithmetic;
  checked multiply trapped in debug on the first real run).
* Same path with different content hash re-ingests (test redelivers the DRT
  5.3 twin under the original filename).

## Storage / manifest

* Layout exactly per spec §4.5: `<model>/<run YYYYMMDDHH>/grid.bin manifest.json
  <param>/<level_type>/f<FFF>.bin`; temp+fsync+rename everywhere; multi-file
  merge into the same (run,fh) file merges level planes (new values win).
* manifest.json is now WRITTEN after every field store (was read-only before —
  gap found while wiring tests). Fields list param/level_type/units/stat info
  + levels (of the newest fh file; informational) + forecast_hours. The index
  stays authoritative; a manifest write failure logs + counts but does not
  block publication. Restart restores the PDT 4.8 guard from the manifest
  (tested).
* Per-fh visibility: `store_plane` publishes (arc-swap swap) after EVERY
  successful field write; a run becomes queryable forecast-hour by forecast-
  hour (spec §4.5).

## Retention order (spec §4.6)

Registry removal -> publish() -> delete_tree. A stale snapshot held across
the eviction still answers through unlinked mmaps (Arc keeps them mapped;
Linux unlink-while-mapped). Tested with three byte-patched run twins
(sec1 day byte patched; 39 pattern hits verified).

## Index / server contract (milestone 4)

* SharedIndex.get_index() -> Arc<IndexSnapshot>; PointQueryContext.build(
  snapshot, model, pinned_epoch?, param_key, level_code, want_level, want_fh,
  mode) -> Result<ctx, SelectError>; evaluate_point(ctx, lat, lon) -> QueryResult.
* SelectError maps to HTTP later: UnknownModel/NoRunCovers -> 404,
  LevelOutOfRange/TimeExactOnly/StatTimeGuard -> 422.
* Hot path: no allocation beyond Arc refcount bumps + response body; mmaps
  opened at snapshot-build time (`Arc<MmapField>` shared across snapshots).

## Threading & counters

* Decode runs on the dedicated rayon pool: `pool.install(|| decode_files_parallel(...))`
  sized by ingest_threads.
* IngestCounters { ingested_files, errors, last_mtime_ns } feed /metrics later
  (query latency histogram lands with milestone 4).

## Deviations / notes

* Layer-range messages (levels.len() != 1) and non-integral fc_secs are logged,
  counted as ingest errors, and skipped (fixtures never hit either path).
* notify watchers (notify v3 pinned by Cargo.lock) push Create/Write/Rename
  paths onto the scan queue for LATENCY only; every scan_round drains the
  queue AND does the full recursive scan (authoritative; spec §4.1 fallback).
* Wind pairing at ingest: pending U/V keyed (epoch,fh,levelcode,val) within a
  batch; rotation applied when both arrive; earth-relative stored with
  winds_grid=false. Pair-rule violation flags the model, logs, counts one
  error, and skips wind params (config validation cannot see the grid flag
  until decode).
* Perf data point (debug build, this machine): full GFS hour (1440x720 TMP@1000
  f000, 1.3 MB GRIB) ingested + indexed in ~230 ms wall (marker clock); release
  numbers recorded in README at milestone 5.

## Test counts (this milestone)

New wx-ingest test binaries (all green, workspace totals now cargo test green,
clippy -D warnings clean):

* `pipeline_e2e.rs`: 1 — fixtures -> storage tree -> index -> queries (bilinear/
  vertical/time exactness incl. masked NaN MissingData, APCP guard, HRRR LCC
  node probe, rotated-wind magnitude invariant, manifest assertions).
* `restart.rs`: 1 — restart without re-ingest, rebuild_from_disk index restore,
  manifest stat-guard round-trip, content-change redelivery re-ingests.
* `retention.rs`: 1 — evict-after-publish ordering, stale-snapshot reads post-
  unlink, pinned-run selection on an evicted run fails NoRunCovers.
* `stability.rs`: 2 — window strategy half-written skip-then-complete; sidecar
  eligibility.
* `concurrency.rs`: 1 — 4 reader threads x >=500 queries vs 300+ arc-swap
  republishes; zero failures, stable values.
* `config_validate.rs`: 4 — unknown param, raw triples (unknown + known), bad
  level/retention/dupes/empty-levels/name+raw errors, wind-pair message text.
* `perf_fixture.rs`: 1 — full GFS geometry ingest timing + edge/seam/row value
  checks (skips when the gitignored perf fixture is absent).

# Milestone 4 decisions (HTTP server + query API)

## Error tokens and status classes (spec §7)

`GET /v1/point` parses args first; malformed input is `400` with one of
`missing_required`, `unknown_query_param`, `bad_lat`, `bad_lon`, `bad_level`,
`bad_time`, `bad_time_interp`, `empty_value`. A well-formed request that cannot
be answered maps through `FailTok` (queryapi) to a stable token + status:

| token | status | meaning |
|---|---|---|
| `unknown_model` | 404 | model not configured/indexed |
| `unknown_param` | 404 | no indexed (param, level-type) field in any run |
| `no_run_covers` | 404 | valid time outside every run's stored fh range |
| `level_out_of_range` | 422 | requested level outside stored range |
| `stat_time_guard` | 422 | template 4.8 field, exact fh required (§6.3) |
| `time_exact_only` | 422 | `time_interp=none` and no exact fh |
| `out_of_domain` | 422 | point outside grid node rectangle (§6.1) |

Error priority when several apply: LevelOutOfRange > StatTimeGuard >
TimeExactOnly > NoRunCovers (the most specific actionable cause wins).
Unknown routes get `404 unknown_endpoint`. Body shape:
`{"error":"<token>","message":"..."}`.

## Run selection

The server iterates the snapshot's runs newest→oldest and recomputes
`fh = (valid − run_epoch)/3600` per run, calling the milestone-3
`PointQueryContext.build()` with that pinned epoch; the first run that can
answer wins ("most recent run covering valid"). An explicit `run=` pins
exactly (epoch match required, otherwise `no_run_covers`).

## JSON deviations from the spec example (documented, deliberate)

* Exact vertical/time matches render explicit `"method":"exact"` (or the key
  is present with nulls) instead of omitting the object; masked values render
  `"value":null,"reason":"missing_data","interpolation":null` (spec left the
  masked shape open).
* Numbers are rendered by `fmt_num` (≤6 trimmed decimal digits): integers print
  without a trailing `.0` (`"value":341`, not `341.0`).
* Multi-param requests (`param=TMP,HGT`) share one point/level/time and add one
  entry per param to `results`; each entry carries its own `units`.
* `/v1/inventory` adds `stat_processed`/`stat_kind` per field so clients can
  see which entries reject time interpolation before querying them.

## Epoch fix (bug found this milestone)

`grib2.rs epoch_secs` double-counted the era-of-years terms and used the wrong
constant; fixed to Howard Hinnant's civil-days formula returning true Unix
seconds (`se = era*146097 + doe − 719468`). `format_iso_z` was rewritten as the
exact inverse (`z = se + 719468`, provable yoe correction loop, clamp at 399)
with a boundary round-trip test. All consumers are internal/self-consistent,
so the change is safe everywhere (workspace tests re-verified green).

## Hot path allocation proof (spec §5)

`hotpath_alloc.rs` installs a counting `#[global_allocator]` wrapping libc
malloc/free (header word stashes the raw pointer; counts via relaxed atomics)
in the test binary, then brackets the hot path on the full-GFS perf fixture:

* `evaluate_point` alone: **ZERO allocations** over 20 000 queries (asserted
  from the allocator counters), debug ≈482 ns/q.
* `build()` + evaluate: 3 allocs/q (the tiny inherited contract: units String
  + two slot Vecs), debug ≈1173 ns/q. The HTTP layer reuses one prebuilt
  context per (model,param,level,time) group, so per-param cost is the
  zero-alloc evaluate.
* No locking beyond the arc-swap load; no mmap open or file I/O per request
  (all mmaps opened at index build); the only heap work per request is the
  response-body string builder.

## Latency ballpark (recorded here; README numbers come in milestone 5)

* Debug build, direct-index loop: ≈1.2 µs/query (build+eval), ≈0.5 µs eval-only.
* Release build (`lto=fat`, `codegen-units=1`, `panic=abort`; no
  `target-cpu=native` — portable default, documented): `/metrics` sum/count
  average ≈22 µs/query including handler overhead, comfortably inside the
  p50 < 50 µs target. Measured by `scripts/smoke.sh` (400 sequential curls).

## Concurrency outcome (spec §9.9)

`concurrency_http.rs`: retention_runs=1 gfs config, byte-patched day twins
20→23 churned by an ingest thread (scan_round + forced `p.publish()` republish
every ~30 ms), 8 reader threads × 1500 queries through the full axum stack at
valid 2026-10-23T15:00:00Z. Every response was 200-with-value-341 or 404
(run not yet covering); zero 5xx, zero torn/partial bodies (`"]}"`
well-formedness checked on every 200); all 5 files ingested with 0 errors;
final deterministic query answers 341.

## Smoke script

`scripts/smoke.sh`: release build → boot on 127.0.0.1:8091 against
`tests/perfdata/gfs_perf_f000.grib2`, poll `/ready`, pull the run time from
`/v1/inventory` (never from filenames), curl truth queries (seam 609.0/609.5,
negative lon), error tokens (404/422), meta endpoints, then compute the
release average latency from `/metrics`. Prints SMOKE PASS/FAIL.

---

# Milestone 5 decisions (benchmarks, load test, profiling, README)

## Benchmark profile quirk (this toolchain)

Criterion needs unwinding, so `[profile.bench]` mirrors release minus panic
handling. This cargo (a) silently IGNORES `panic` in the bench profile (bench
always unwinds — which is exactly what criterion wants; shipped release keeps
`panic = "abort"`), and (b) rejects both `inherit` on built-in profiles and a
bare `optimize`/`optimization-level` key. The explicit quoted-key form
`"opt-level" = 3` is used and documented in Cargo.toml.

## Load-test tool: oha 1.16.0

Downloaded once into gitignored `tools/` (same policy as the local wgrib2
build tree). oha quirks recorded: `-t` is TIMEOUT (threads are
`--worker-threads`), and multiple URLs require `--urls-from-file`. 8 worker
threads per spec §8. ab/wrk would also work; oha chosen for its server-side
friendly keep-alive mix + stable RPS reporting.

## §8 targets met on first measurement -> NO bounded optimizations attempted

Decision recorded deliberately: portable build already hits p50<=20us / p99<=50us
server-side and ~287k req/s (>=50k target x5.7), so no bit-risky micro-
optimizations were attempted at all; nothing needs a bit-exactness test beyond
the pre-existing suite. The interning/chunked-layout experiment was NOT done
because there was no measured gap to close.

## target-cpu=native verdict

NATIVE=1 path in loadtest.sh rebuilds with `-C target-cpu=native` into
target-native/: 275,293 req/s vs 286,868 req/s portable (native is WORSE,
EPYC scheduling/NUMA noise likely). Shipped build stays portable; native is
documented only.

## Profiling pass: hardware profilers unavailable on this machine

perf_event_paranoid=4 AND yama ptrace_scope=1 (no passwordless sudo): perf
stat/record fail outright, gdb attach refused (attach-only-child rule), strace
ATTACH impossible. Fallback implemented: scripts/profshim.c LD_PRELOADs a
per-thread ITIMER_PROF SIGPROF sampler (pthread_create interposition enables
the itimer on tokio/rayon threads; ucontext RIP leaf samples; periodic atomic
dump survives any kill), symbolized offline against .symtab via nm plus
/proc/pid/maps for libc regions. strace LAUNCH mode (not attach) still works
for syscall accounting. Top cost centers under the load mix (12 s, 8 threads):
libc writev 27.1% (response socket writes dominate), recv 4.8%, our point_handler
closure 4.7%, free 3.2%, syscall 3.1%; ingest-side frames (rayon decode bridge,
store::write_field) appear because the window includes the initial dataset
ingest. Syscall counts (strace -c, 3 s window): futex 48,407 / epoll_wait
13,160 / writev 45,528 — one writev per response dominates.

## Metrics rendering bug fixed

Bucket lines emitted `}count` without whitespace between the closing brace and
the value (invalid Prometheus); fixed to `} <count>` before the load test read
percentiles off /metrics.

## Two real bugs found while validating milestone-5 deliverables (fixed + tested)

1. **Warm-boot LCC drop**: decode_grid's Lambert length guard demanded 97
   bytes while encode_grid correctly writes 89 (8 f64 params, not 9) -> EVERY
   LCC run was silently dropped from a warm-start index (HRRR never became
   ready after restart; cold boot masked it because GridDef lived in memory).
   Fixed guard to 89; regression test: restart.rs extended with an hrrr model
   + exact-node probe through the rebuilt index.
2. **Invalid JSON on /v1/models and /v1/inventory**: object items lacked '{'
   and loops double-pushed separator commas (`[,"model" ... },,"param"`).
   Fixed braces/separators; regression test meta_json.rs parses both endpoints
   end-to-end through the real router with serde_json.

## Perf-dataset generation decisions

* gen_perfdata.sh extends scripts/gen_grib2.py (numpy fast packing) to full
  GFS geometry (1440x720, 22 msgs/file ~33.9 MB) and HRRR-like LCC
  (1799x1059, 13 msgs/file ~37.2 MB); validate_perf.py re-probes every record
  with wgrib2 (DRT 5.0 everywhere; node values match formulas within 0.51).
* LCC scan byte 0x40 (rows go NORTH from La1 = real-HRRR convention),
  empirically validated against wgrib2 -ll2ij (SW probe ~= (6.5, 4.4)).
* Analytic fields kept EXACT in f32: GFS TMP@1000 = 250+j+fh*10 (back-compat
  with tests/perf_fixture.rs), other GFS = base+((j*1440+i)%4096)+fh*10, HRRR
  affine = base+i+j+fh*10 (affine so OFF-node bilinear probes reproduce within
  ~1e-3; probes.tsv near-mode tolerance 0.01).
* probes.tsv (17 probes, gitignored, regenerable) replayed by smoke.sh in
  grep/near/status modes against BOTH models.

## smoke.sh extension

Full perf dataset served end-to-end (gfs+hrrr, 12 param groups); inventory
polled until BOTH forecast hours of BOTH models are queryable before probing
(per-fh publication race), then probes.tsv replayed; SMOKE PASS requires all
probes green. Release avg latency from /metrics: ~24 us/query over 422
sequential curls (includes HTTP client overhead per request).

## Measured numbers (this machine: AMD EPYC 7402, rustc 1.99.1, kernel 7.0.0-31)

See README tables: criterion medians (regular_ll project 20.6 ns, lamcc
project 84.7 ns, bilinear heap 30.7 ns, mmap h_at 32.5 ns, full vertical+time
query 364.5 ns, handler parse+lookup+render 879.2 ns); load test portable
286,868 req/s, server p50<=20us p99<=50us mean 12.9us; ingest->queryable
/ready 3.9 s, gfs f000 4.1 s / f006 4.2 s, hrrr f000 4.3 s / f003 5.1 s; peak
RSS 372 MB; workspace cargo test 57 green; clippy -D warnings clean.

---

# V1.1 phase 1 decisions (full wgrib2 toolchain)

## What built, and how

Full source tree extracted fresh into `tools/wgrib2-full-src/grib2` (classic v3.1.3
**makefile**, not CMake - the task's `-DUSE_JASPER=ON` guess does not apply to this
release). Binary installed as `tools/wgrib2-full`; the existing nolib `tools/wgrib2`
is untouched and still works (both report v3.1.3 10/2023).

```
cd tools/wgrib2-full-src/grib2
CC=gcc CMAKE_POLICY_VERSION_MINIMUM=3.5 \
make -f makefile -j8 USE_NETCDF3=0 USE_NETCDF4=0 USE_HDF5=0 \
  USE_IPOLATES=0 USE_SPECTRAL=0 MAKE_FTN_API=0 USE_PROJ4=0 USE_G2CLIB=0 \
  MAKE_SHARED_LIB=0 USE_REGEX=1 USE_TIGGE=1 USE_OPENMP=1 \
  USE_PNG=1 USE_JASPER=1 USE_OPENJPEG=0 USE_AEC=1 \
  'BUILD_COMMENTS=full build gcc jasper png aec' wgrib2/wgrib2
```

* JASPER and OPENJPEG are mutually exclusive (makefile errors if both are 1); chose
  Jasper 1.900.1 (bundled debian tgz), which compiles clean under gcc 13.3 (gcc 14
  would break it: implicit int / implicit function declaration become hard errors).
* cmake here is 4.4.2, which refuses `cmake_minimum_required(VERSION < 3.5)`; libaec-1.0.6
  needs `CMAKE_POLICY_VERSION_MINIMUM=3.5` in the environment.
* netcdf/proj4/g2clib/ftn-api/shared-lib disabled: not needed for validation work,
  and the netcdf4/hdf5 tarballs are absent from the tgz anyway (the `all` target
  errors out on them). gctpc (pure C) is always linked (-lgeo).
* `-config` of the new binary confirms: `Jasper 1.900.1 is installed`,
  `libaec-1.0.6.tar.gz is installed`, `Supported decoding: simple, complex, rle,
  ieee, png, jpeg2000, CCSDS AEC`, `Supported encoding: simple, complex, ieee,
  jpeg2000, CCSDS AEC`.

## Capability matrix (measured, not assumed)

| capability | status |
|---|---|
| write DRT 5.0 / 5.3 (c3) / 5.40 (jpeg) / 5.42 (aec) | OK, verified by `-code_table_5.0` |
| read 5.0 / 5.3 / 5.40 / 5.42 | OK |
| **write PNG 5.41** | **IMPOSSIBLE in wgrib2**: `-set_grib_type png` -> `FATAL ERROR: set_grib_type: bad type png`. Accepted names are only s/simple, c1..c3 (+b), ieee, jpeg/j, aec/a, same. PNG support is decode-only (`dec_png_clone.c`); the encoder lives in g2clib |
| `-new_grid` / `-new_grid_winds earth` | unavailable: prints `IPOLATES package is not installed`, rc=8 |
| `-level 0` | option does not exist in v3.1.3 (use `-code_table_5.0` for DRT, `-ll2ij` for projection probes) |

PNG twins therefore need an external writer: `scripts/make_png_twin.py` (added this
phase) re-packs a simple-packed file into 5.41 itself. Fixtures and `gen_fixtures.sh`
were left untouched - Phase 2 owns wiring them in.

## Cross-DRT value consistency (the Phase 2 validation backbone)

`tools/wgrib2-full FILE -lon LON LAT` output is byte-identical across the original
5.0 file and its jpeg / aec / c3 / s / png twins for every message at 6 probe points
x 39 messages (gfs_small) and 2 x 12 (hrrr_small, including grid-relative wind values
passing through unchanged), including seam probes (lon 350.0 / -0.25 / 0.0) and masked
nodes (UNDEFINED 9.999e+20 preserved by all four codecs). Stronger global proof on
the full-size perf file: `-checksum data` CRC32 over the decoded float array matches
for ALL 22 records of `gfs_perf_f000.grib2` vs its jpeg twin (33.9 MB -> 1.0 MB) =>
the jasper round trip is LOSSLESS for these products.

## IPLIB verdict: cannot be built here

`iplib.v3.0.0` = 33 `.f90/.F90` module files, `ip2lib_d` likewise, `iplib_hwrf` = 55
`.f` files: pure Fortran, no C fallback (the "maybe gctpc-derived C" hope is false;
gctpc is separate and already linked). Evidence chain:

* `make USE_IPOLATES=1` aborts at makefile line 324 with `FC=? compile directions: ...`
  (make's built-in default `FC=f77` trips the guard).
* Compiling a source directly: `gcc -c ijkgds0.f90` -> `gcc: fatal error: cannot execute
  'f951': execvp: No such file or directory` (no Fortran frontend in gcc 13.3).
* No gfortran/g95/ifort/ifx/flang/f2c anywhere on PATH; `sudo -n` requires a password
  so apt is out; libgfortran runtime exists but a runtime without a compiler is useless.

Consequence: golden `wgrib2 -new_grid_winds earth` rotation truth is NOT obtainable
on this machine. Keep the existing three-way rotation validation (pyproj forward
transform + Snyder closed form + magnitude/involution invariants); real ipolates truth
would require a machine with a Fortran toolchain.

## Layout facts learned while building the PNG twin (Phase 2 must know)

* The data representation template number lives in **section 5 octets 10-11**
  (wgrib2 `code_table_5_0(sec) == uint2(sec[5]+9)`); section 7 has NO template byte.
  Our Rust `decode_sec7` already reads `be_u16(s5, 4)` from the section-5 payload, i.e.
  the same place, so adding 5.40/5.41/5.42 decoders is just extending that match.
* Section framing is identical in our fixtures and in wgrib2-written files:
  `[u32 length-including-itself][section number][payload...]`; the end section's declared
  length excludes the ASCII `7777` terminator, which is still counted inside the u64
  message length.
* PNG path specifics: bit depth comes from Sec5 bits-per-value (octet 20 = `sec[5][19]`)
  and must equal the PNG IHDR depth, else `dec_png_clone` emits a DELAYED ERROR
  ("Sec 5 octet 20 is X, png lib value (Y) is used") and silently uses Y; the PNG stream
  starts at `sec[7]+5` (no PSV field); grayscale covers depths 8/16 only (24/32 require
  RGB/RGBA color types); `width*height` must equal nvals (plus the missing-point count
  when a bitmap is present); libpng is compiled with `-DPNG_USER_WIDTH_MAX=200000000L`,
  so width = nvals (~1M) is legal.
* Fixture quirk found on the way: our simple-packed section 7 bitstreams are truncated
  to `floor(nvals*nbits/8)` bytes (861 x 7 = 6027 bits stored in 753 bytes), so re-packers
  must zero-pad before reading the final value.
* jpeg twins carry per-message metadata `Creator: JasPer Version 1.900.1` -> compare
  decoded VALUES (or `-checksum data`), never bytes.
