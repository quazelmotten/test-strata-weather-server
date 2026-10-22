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
* 5.40 JPEG2000 / 5.41 PNG / 5.42 AEC: **at milestone 2 these were rejected** with
  typed `UnsupportedData(tmpl)` because the local wgrib2 could not produce or read
  them, so no truth was available. Resolved in V1.1 phase 2 (see that section below):
  all three decode against `tools/wgrib2-full` ground truth.
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
  `rot_angle_deg`):
  `u_e = u_g·sinβ + v_g·cosβ`, `v_e = u_g·cosβ − v_g·sinβ`. For N→S-scanning
  LCC grids (+j ~ south) this is the symmetric orthogonal matrix
  [[sinβ, cosβ], [cosβ, −sinβ]] — an involution, magnitude-preserving.
* **Angle derivation (V1.1 phase 4 — changed).** β is now derived from the
  projection's OWN cone geometry instead of differencing two inverse transforms:
  `for_point` places a point at apex + rh1·(sin θ, cos θ) with θ = n·(λ−LoV),
  so meridians are exact radial rays of the cone and, because the map is
  CONFORMAL, true north maps to that radial direction. Hence
  β = 90° + θ, with θ recovered by `cone_polar(x, y)` — the very helper
  `inverse()` uses, so projection and rotation share one truth. The earth model
  enters only through n/rh (built from the Code Table 3.2 axes, including the
  INLINE radii of codes 1/3/7), so spherical and oblate grids are both exact.
  Measured against pyproj with the shape matched exactly: **4.8e-11 deg** worst
  delta over 42 nodes (was up to **2.4e-2 deg** with the old method — see the
  phase-4 section below for the root-cause numbers).
* Validated four ways (wgrib2 nolib lacks `-new_grid_winds`):
  1. `scripts/validate_rotation.py` measures the conformal angle independently
     with pyproj (`+proj=lcc`, earth shape taken FROM the fixture's CT 3.2 byte,
     incl. inline code-3 axes) at 21 nodes per fixture and writes
     `tests/fixtures/rotation_expected.csv`;
  2. `crates/wx-ingest/tests/rotation_vs_pyproj.rs` compares our angles with
     **≤ 1e-6 deg** tolerance (observed 4.8e-11) and asserts the identity
     β = 90 + n·(λ−LoV) at ≤ 1e-12 deg;
  3. invariants on real decoded data: magnitude preserved (rel 1e-4), double
     application identity, LoV meridian lands exactly on due east;
  4. `scripts/validate_rotation_wgrib2.sh`: the real `wgrib2 -new_grid_winds
     earth` golden, enabled automatically on any machine whose wgrib2 links
     IPOLATES (see IPLIB verdict above: not buildable here).
* Regular lat/lon grids: components treated as already earth-relative
  (identity), matching wgrib2 behavior for those grids.
* Config rule, spec §4.4 wording ("fail config validation"): enforced TWICE.
  * STARTUP preflight `config::startup_wind_preflight` (called from `main`
    before ingest/serving): for a model configuring EXACTLY ONE of UGRD/VGRD
    (by table name OR raw triple category 2 / number 2 or 3), it probes the
    FIRST STABLE file matching the model glob (read-only, sections 1/3/4 only,
    never section 7) and fails if THAT file's WIND messages declare grid-relative
    components. Message names model, file, template, flag byte and the missing
    parameter.
  * DECODE time `check_wind_pair` (pipeline) stays as the second line of defense
    for directories that are empty/unstable at boot; it also blocks storing the
    affected wind fields rather than emitting nonsense rotations.

## Earth shape

Code Table 3.2 byte honored via wx-core `earth_axes_m` (sphere 6371229 m for
code 6, International spheroid 6378160/6356775 m for code 2 — values follow
wgrib2 `Earth.c::axes_earth` so our numbers match `wgrib2 -ll2ij`). Codes
**1/3/7 declare the axes INLINE in section 3**; the reader now parses them
(`scaled_val` slots: shape+1/+2 sphere radius, +6/+7 major, +11/+12 minor;
code 3 in km, code 7 in meters) and calls `GridLambert::set_user_axes`, so a
user-defined spheroid projects with the axes the FILE declares. grid.bin
carries them (GRID_VERSION 2). Fixtures: `hrrr_small.grib2` uses code 6 (sphere
R=6371229 m); `hrrr_ellip_small.grib2` uses code 3 with WGS 84 (6378.137 /
6356.75231 km) so BOTH shapes are exercised end to end.

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

---

# V1.1 phase 2 decisions (JPEG2000 / PNG / CCSDS AEC codecs)

## Backends chosen — pure Rust, NO C FFI

| DRT | backend | version | why this one |
|---|---|---|---|
| 5.40 JPEG2000 | `hayro-jpeg2000` | 0.4.1 | pure-Rust J2C decoder (MQ arithmetic coder + bitplane/tile assembly). `default-features = false`, features `std`,`simd` only, so the image/colour machinery (`moxcms`, `pxfm`) is not compiled into an ingest binary that never displays pixels |
| 5.41 PNG | `zlib-rs` 0.6.8 (inflate + CRC32) + in-house chunk walk and unfilter | wgrib2 reads PNG through libpng; there is no maintained pure-Rust PNG reader that hands back raw scanlines without a pixel-format pipeline, and pulling one in would add exactly the conversion code we must NOT run (samples are integers, not colours) |
| 5.42 CCSDS AEC | `rust-aec` 0.1.1 | direct port of the libaec block model GRIB2 describes; takes the same three parameters Sec5 stores |

Why no C FFI (spec §4.2 offered ecCodes via FFI as the fallback): `openjpeg-sys`/
`aec-sys`/libpng bindings need **libclang**, absent on this machine (phase 1 hit the
same wall with IPLIB/Fortran), and an FFI dependency would make the shipped binary
depend on system libraries we cannot pin or verify here. All three backends come
from crates.io and are pinned in `Cargo.lock`; nothing in the workspace links C.

## Error taxonomy for codec paths (`GribErr`)

* `UnsupportedData(u16)` — unchanged meaning: a data representation template we do
  not implement at all (dictionary 5.1, …).
* `CodecFailure(&'static str)` — the stream is corrupt/truncated/unparseable
  (bad signature, chunk CRC mismatch, inflate failure, backend parse error).
* `CodecUnsupported(&'static str)` — the stream parsed but uses a construct the
  backend deliberately does not implement (see scope table below).

Rule kept from phase 1: anything that cannot be validated against wgrib2 truth in
CI stays behind a typed error. No silent guessing, no partial decode.

## Section-5 payload offsets consumed (payload = wgrib2 `sec[5]` minus its 5-byte header)

Common to 5.40/5.41/5.42: `0..4` defined-point count, `4..6` DRT, `6..10` reference
value (f32 BE), `10..12` binary scale E, `12..14` decimal scale S, `14` bits-per-value.

| template | extra fields |
|---|---|
| 5.40 | `16` lossless flag, `17` undefined byte (both recorded, unused: Jasper always writes lossless) |
| 5.41 | nothing past `14`; bits-per-value (`payload[14]` = octet 20) MUST equal the IHDR depth |
| 5.42 | `16` ccsdsFlags, `17` block size, `18..20` RSI (uint2) |

Section 7 carries **no** PSV/leading field for any of the three: the codec stream
starts at payload offset 0.

## JPEG2000 specifics

* Verified end-to-end against `tools/wgrib2-full`: a Jasper-written GRS-style
  stream (41 x 21, bit depth 7) decodes to samples 0…120 in **file scan order**.
* wgrib2/Jasper embed a text box `Creator: JasPer Version 1.900.1`. hayro parses
  `COM` (marker 0xFF64), skips `CRG` (0xFF63) and the parameterless
  `0xFF30..0xFF3F` markers, and bails with `MarkerError::Unsupported` on any other
  marker code — which we surface as `CodecFailure("jpeg-stream")`. So NCEP-style
  metadata boxes are fine; exotic boxes are rejected loudly.
* Samples come back as f32: exact only up to 2^24, so `bit_depth > 24` and
  multi-component images are `CodecUnsupported` rather than silently rounded.

## PNG specifics (field layout)

* Stream: `[u32 len][7][PNG]`; chunks are `u32 | 4-byte type | data | u32 crc32`
  and the CRC covers **type bytes + data** (RFC PNG). Implemented as zlib-style
  chaining: `crc32(crc32(0, name_bytes), body)`. Bug found while wiring this up:
  seeding the CRC over data only rejected every valid file (`png-crc`).
* Accepted: color type 0 (grayscale), depths 8/16, non-interlaced, compression
  and filter methods 0, `w * h == defined-value count`, samples big-endian over
  `depth/8` bytes per value (equivalent to wgrib2's `unpk_0(..., ((nbits+7)/8)*8, ...)`
  bitstream read). Rejected: everything else (`png-color-type`, `png-bit-depth`,
  `png-interlace`, `png-methods`).
* Unfilter implemented **exactly** as the libpng 1.2.59 that wgrib2 links
  (`pngrutil.c::png_read_filter_row`): None/Sub/Up/Average/Paeth, with the first
  `bpp` bytes of each row predicted from `up` alone. Paeth predictor as libpng has
  it: `p = b - c; pc = a - c; pa = |p|; pb = |pc|; pc' = |p + pc|; pick a, else b,
  else c` (a=left, b=up, c=upper-left). The textbook form written from the prose
  spec picks differently on real data — the first PNG twin failed on every message
  until this was corrected, in both the Rust reader and the Python writer.
* Per-row filter bytes are collected and exposed as `codecs::png_filter_types`
  so a fixture can prove which predictor branches it actually exercises.

## CCSDS AEC specifics (parameter layout)

* `aec_pk.c` writes `sec5[21]=ccsdsFlags`, `sec5[22]=block size`,
  `uint2(sec5+23)=rsi`; `unpk.c` reads exactly those, i.e. payload offsets
  `16`, `17`, `18..20` in our representation.
* Call shape: `AecParams(nbits, block, rsi, flags_from_grib2_ccsds_flags(flags))`
  then `decode(s7, params, expect_n)`.
* Verified against wgrib2 output: gfs twin `nbits=7 flags=14 block=32 rsi=128`
  yields packed integers identical to wgrib2's; hrrr twin uses `nbits=9` → 2-byte
  big-endian samples. Parameter sets `rust-aec` refuses become typed errors.

## Shared integer pipeline (why values stay bit-identical)

* With a bitmap, the codec stream holds ONLY the DEFINED points (Jasper/libaec write
  `mk_bms` output); samples are re-interleaved with the bitmap exactly like DRT 5.0,
  and Sec5's defined-point count is cross-checked against the bitmap population
  (`BadField("* defined-point count mismatch")`).
* Scaling `(ref + p * 2^E) * 10^-S` computed in f64 then narrowed to f32 — the same
  code path as simple packing (`codecs::finish_values`), so decoded bits match
  `dec_simple` and wgrib2 exactly.
* `nbits == 0` (constant field) short-circuits without touching the codec stream but
  still honours the bitmap.

## Checksum normalization rule (validation primitive)

`wgrib2 -checksum data` hashes wgrib2's **normalized** array — rows south→north,
columns west→east — not the file scan order (proved by `-text` dumps and
permutation search on three fixtures). Rule implemented in `codecs::data_cksum_grid`:

```
row_flip = (scan_mode & 0x40) == 0     # file scans north-to-south -> rows reversed
col_flip = (scan_mode & 0x80) != 0     # file scans east-to-west  -> cols reversed
```

Masked points are hashed as wgrib2's `UNDEFINED` sentinel `9.999e20f`, never NaN.
Encoders still write file scan order; only the checksum view is normalized.

## Tests added (names)

* `checksum_parity.rs`: `gfs_drt50_data_checksums_match_wgrib2`,
  `hrrr_drt50_data_checksums_match_wgrib2`, `stat_pdt48_data_checksums_match_wgrib2`
  (39 / 6 / 3 embedded wgrib2 checksums).
* `packing_twins.rs`: `gfs_twins_match_simple_packing`,
  `hrrr_twins_match_simple_packing`, `masked_points_are_nan_for_every_codec`,
  `point_probes_reproduce_on_every_gfs_twin`,
  `point_probes_reproduce_on_every_hrrr_twin`,
  `skip_unpack_counters_hold_for_compressed_twins`,
  `png_twin_filter_coverage`, `dictionary_packing_rejected_with_typed_error`.
* `stat_pdt.rs`: `jpeg_twin_keeps_pdt48_metadata_and_values` replaces the old
  `j2k_rejected_with_typed_error` guard (5.40 is now decodable, so the guard moved).
* `perf_fixture.rs`: `perf_gfs_hour_jpeg_matches_simple_packing` — 22 embedded
  wgrib2 `-checksum data` values for the full-geometry JPEG twin, skips silently
  when the gitignored perf dataset is absent.

## Fixture set (regenerated by `scripts/gen_fixtures.sh`)

`_c3` (5.3), `_jpeg` (5.40), `_aec` (5.42), `_ieee` (5.4), `_png` (5.41, filter
None), `_png_f` (5.41, all five PNG filters cycled across messages) for both
`gfs_small` and `hrrr_small`, plus `stat_small_jpeg` (PDT 4.8 x 5.40) and the
perf-scale `gfs_perf_f000_jpeg`. Every twin is asserted two ways inside the script:
per-record DRT via `-code_table_5.0`, and `-checksum data` equality with the
simple-packed original. Guard fixture moved from `unsupported_j2k.grib2` (now
decodable) to `unsupported_drt.grib2` declaring DRT 5.1 — which wgrib2 itself
refuses (`*** FATAL ERROR: packing type 1 not supported ***`, rc=8). Note the guard
must force a decode (`-checksum data`): a bare listing never touches section 7, so
the earlier check passed vacuously. `scripts/regen_expected.sh` switched to
`tools/wgrib2-full` and probes every twin, asserting offset-normalized inventories
and point/ij values are identical across packings.

## Measured numbers (this machine: AMD EPYC 7402, release build)

| workload | value |
|---|---|
| one GFS forecast hour (1440 x 720, 22 fields) decode, simple packing | 752 ms |
| same hour, JPEG2000 twin (33.9 MB -> 1.0 MB on disk) | 1143 ms |
| small-fixture twin decode (all six packings) | < 1 ms per file |
| workspace test suite incl. twins | green; clippy `-D warnings` clean |

JPEG2000 costs ~1.5x of simple packing at scale while shrinking files ~33x, so
ingest latency stays well inside the §8 "few seconds" target even if NOAA ships
packed products.

## Deviations / follow-ups

* **Transient 422 during ingest.** The pipeline publishes after every field write
  (§4.5 incremental visibility), so mid-file a run can hold `TMP@1000` without
  `TMP@850/700`; a request for a NON-STORED level (925) then has no bracketing pair
  and answers 422 `level_out_of_range` instead of extrapolating — spec §7-correct,
  but noisy. `concurrency_http.rs` previously asserted "only 200/404" and therefore
  flaked under CPU load (36 of 12000 queries in a reproduced failure); it now counts
  those separately (`transient_422`) while still requiring zero 5xx and zero torn
  bodies, plus a settled-state batch proving they disappear once ingest finishes.
  Coarsening publish granularity (per forecast-hour group) is the documented follow-up.
* PNG fixtures come from our own writer (`make_png_twin.py`) because wgrib2 has no
  PNG encoder; the round trip is validated in both directions (our reader vs
  wgrib2's libpng reader), which is stronger than a self-consistent loop.
* Still unsupported by design: JPEG2000 >24-bit depth or multi-component, PNG
  colour types / interlace / depths other than 8-16 / non-zero methods, AEC
  parameter sets `rust-aec` rejects, DRT 5.1 dictionary. All typed errors.

# V1.1 phase 3 decisions (fractional forecast hours, skip-and-note, durability)

Audit items closed: fractional forecast hours end-to-end, robustness for
structurally unusable messages, real directory fsync, PDT 4.8 statistical time
ranges in `manifest.json`.

## Canonical forecast-hour naming (spec §4.5 storage keys)

Internal join key everywhere is **integer SECONDS past the run reference time**
(`RegFh.fh_secs`, `IdxFh.fh_secs`, `PointQueryContext.want_fh_secs`). Filenames
render that offset with ONE canonical scheme, trailing zero components omitted:

| condition | name | example |
|---|---|---|
| `secs % 3600 == 0` | `f<HHH>.bin` | `f000.bin`, `f006.bin`, `f1000.bin` |
| `secs % 60 == 0` (not an hour) | `f<HHH>m<MM>.bin` | `f000m30.bin`, `f001m30.bin` |
| whole seconds otherwise | `f<HHH>m<MM>s<SS>.bin` | `f001m01s01.bin` |

* Integral hours are **byte-identical** to the milestone-3 layout, so every pre-
  existing store stays readable and `/v1/inventory` output is unchanged for them.
* Hours pad to ≥3 digits; ≥1000 h widens instead of breaking the pattern.
* Sub-second offsets cannot exist: Code Table 4.4's finest unit is SECOND (code
  13), so `fc_secs` is always whole seconds by construction.
* The reader (`parse_fh_name`) is strict about non-canonical names — `f001m5.bin`,
  `f001m70.bin`, `f001m00s777.bin` are IGNORED rather than reinterpreted — but
  tolerates `f006m00.bin` as an alias of `f006.bin`: the writer never emits it, and
  refusing it would silently DROP data from a hand-assembled store. Documented
  nuance rather than a bug either way.
* Why not decimal-hour filenames (`f0.5.bin`)? Locale/format ambiguity, and a
  float can't be a filesystem key; seconds are exact integers.

## Run-directory naming (collision fix)

`YYYYMMDDHH` gains `MM` when the run minute ≠ 0 and `SS` when the run second ≠
0 → lengths 10/12/14, disambiguated by length. Before this, runs 12Z and 12:30Z
on one day both mapped to `2026101912` and MERGED into one run directory (data
corruption, not cosmetic). Old 10-char directories keep parsing (minute/second
default 0); `parse_run_dir` rejects impossible fields (minute 60 etc.).

## Time arithmetic in seconds, reported in hours

All comparisons/interpolation use whole-second offsets (`select_run(want_fh_secs)`,
`resolve_times`, `time_lerp`): 1800 s is exactly representable as an integer while
0.5 h is only exactly representable as a fraction, so the seconds form is at least
as accurate as what it replaces. The RESPONSE converts back to HOURS
(`InterpInfo.time.forecast_hours = fh_secs / 3600`) because spec §7 shows
`forecast_hours` in hours; `manifest.forecast_hours` and `/v1/inventory` likewise
print decimal hours (informational only — the filename is the authoritative key).

Per-run offset recomputation: one instant can be run A's `f001.bin` AND run B's
`f000m30.bin`. `queryapi.answer_param` walks runs newest→oldest and recomputes
`fh_secs = valid_ep - run_ep` per candidate, so "most recent run covering `valid`"
is well-defined for non-hourly reference times (spec §6.3).

## Manifest statistical ranges are PER forecast hour

```json
{"param":"APCP","level_type":"surface","stat_processed":true,"stat_kind":"accumulation",
 "levels":[0],"forecast_hours":[0.5],
 "stat_ranges":[{"fh_secs":1800,"begin":"2026-10-19T12:30:00Z",
                 "end":"2026-10-19T13:00:00Z","step_secs":0}]}
```
Two accumulations of ONE (param, level type) have DIFFERENT windows, so the range
travels with the forecast hour (`fh_secs` is the exact join key), not the field.
`begin`/`end` are absolute ISO-8601 UTC instants; a warm start restores them via
`stat_range_for` so the accumulation guard survives restarts.

**Bug found while wiring this:** `store.iso_utc`'s closed-form Hinnant inverse used
`era = z / 400` instead of `z / 146097` (and a bogus `yoe` expression), producing
years like `555635-0-6607885-0-26T12:30:00Z`. Replaced with the correct era divisor
plus a bounded year-of-era search (`yoe ≤ 399`; at `doe = 146096` — a leap year's
Feb 29 — the naive predicate admits yoe=400 and shifts the date by one day). Runs
at ingest time only. Pinned by `tests/iso_roundtrip.rs`: 8 known instants plus a
day-by-day round trip over 1970-01-01…2104-12-31 (leap day, non-leap century 2100).

## wgrib2 renders accumulation ranges differently than we store them

wgrib2 prints an accumulation as `[fc-len, fc]` ("0-30 min acc(30 min fcst)") but
an average as `[fc, fc+len]`. Our decoder stores `begin = fc_secs`, `end = fc_secs + len`
uniformly (validated empirically against wgrib2 readback in phase 2 and asserted by
`tests/stat_pdt.rs`). The guard behaviour is unaffected — it only needs the range to
exist and be non-degenerate — but anyone diffing manifest JSON against wgrib2 text
must expect this difference for accumulations.

## Skip-and-NOTE policy (never an error, never silent)

Three structural cases SKIP the message and CONTINUE at `body_end` (the byte length
declared in section 0), so the rest of the file still ingests:
* coordinate values after the product-definition template (`SkipReason::CoordLists`),
* grid templates this decoder does not implement (`SkipReason::Grid(t)`),
* product-definition templates not implemented (`SkipReason::Pdt(t)`).

They increment `DecodeStats.messages_unsupported` → `IngestCounters.skipped_messages`
→ Prometheus `wxpoint_skipped_messages_total`, plus a bounded ingest note. Layer/range
messages (two fixed surfaces) decode fine structurally but cannot answer a point query,
so the pipeline skips them the same way (`msg.levels.len() != 1`). Fractional
forecast hours are VALID data and NEVER count as errors; only a NEGATIVE offset
(corrupt) is skipped. Corrupt or unsupported section 7 (DRT 5.1 dictionary) still
fails LOUD as an ingest error — that keeps the packing-twin guard (`packing_twins.rs`)
working: a decoder that silently returns zeros must not look like a skip.

## Durability: real fsync — the libc detour was based on a WRONG claim (corrected in V1.2 phase D)

**Original claim (phases 1-3, kept here as the audit trail):** this toolchain's
`std::fs::File` exposes only `flush()`; `strace` confirmed it emits NO `fsync`
syscall and there is no `fd()`/`FileDescriptor`/`flush_sync` accessor, so
`wx_core::storage::fsync_path()` bound libc directly:

```rust
extern "C" {
    #[link_name = "open"]  fn libc_open(path: *const i8, flags: i32, ...) -> i32;
    #[link_name = "fsync"] fn libc_fsync(fd: i32) -> i32;
    #[link_name = "close"] fn libc_close(fd: i32) -> i32;
}
```

(`extern "C"` block + explicit `#[link_name]` + `*const i8` avoided the suspicious-
runtime-symbol warning this toolchain emits for an implicit `open`.)

**Phase-D correction:** that premise was FALSE. A compile probe OUTSIDE the repo
(`/tmp/opencode/fsync_probe.rs`, `/tmp/opencode/fsync_probe2.rs`) shows
`std::fs::File::sync_all()` EXISTS in this toolchain (rustc resolves it to
`library/std/src/fs.rs:774`) and `strace` of the probe shows a REAL `fsync(3)` —
including on a DIRECTORY handle (`File::open(dir)` → `fsync(4) = 0`). The libc
bindings were therefore REPLACED by `File::open(path)` + `sync_all()`: identical
syscalls (`openat` O_RDONLY → `fsync` → `close`), identical signature, identical
counter placement and error taxonomy, one fewer FFI surface. Order per file is
unchanged: write → flush → fsync(inode) → rename → **fsync(parent dir)** (batched
per DISTINCT directory since phase B), so a crash cannot resurrect a renamed-but-
unlinked name. Errors are surfaced, not swallowed. Lesson recorded: an
"unsupported in this environment" conclusion needs a compile probe, not an
error-message inference.

`write_bin_atomic`'s error type widened from `&'static str` to `String` (callers
only match Ok/Err) so fsync messages can carry the path.

## Tests added (names)

* `crates/wx-ingest/tests/fractional_fh.rs`: `fh_filename_scheme_roundtrip_and_backcompat`,
  `run_dir_names_carry_minutes_without_collisions`, `frac_fixture_decodes_minute_units_as_seconds`,
  `fractional_hours_storage_and_manifest`, `fractional_hours_queries_and_run_selection`
* `crates/wx-ingest/tests/robust_skips.rs`: `coord_list_message_is_skipped_and_the_walk_resumes`,
  `pipeline_skips_are_noted_not_errors`
* `crates/wx-ingest/tests/iso_roundtrip.rs`: `iso_utc_matches_known_instants`,
  `iso_utc_parse_round_trips_across_two_centuries`
* `crates/wx-server/tests/frac_http.rs`: `fractional_hours_over_http` (exact fractional
  slot, mid-bracket `[0.5,1]`, newest-run-wins vs pinned run, 404 beyond both runs,
  accumulation guard on a fractional window, `time_interp=none`, inventory hours,
  `wxpoint_skipped_messages_total 2`), `robust_model_answers_despite_skipped_messages`
* extended `restart.rs`: manifest `stat_ranges` bytes on disk + warm-boot restore of
  per-fh begin/end for BOTH accumulations and the 3-hour average.

Fixtures: `tests/fixtures/frac_small.grib2` (CT 4.4 minute units, two runs 12Z and
12:30Z in ONE file, one 30-minute accumulation) and `robust_small.grib2`
([good TMP@850][coordinate-list][layer 850-800][good TMP@2m AGL]); both generated
by `scripts/gen_grib2.py`, probed into `tests/fixtures/expected.json` by
`scripts/regen_expected.sh` through `tools/wgrib2-full` (values 295/325/355/545/575/80
at lon 351.5 lat 9.0 for frac; 300 + j*21 + i for robust).

## Gate

`cargo test --workspace`: 80 tests green (was 69). `cargo clippy --workspace -- -D
warnings`: rc=0. Two pre-existing `format!`-in-`format!` lints in `store.fmt_hours`
had to be fixed to keep that gate clean. `--all-targets` still reports lints in
older wx-core test/bench files (pre-existing, untouched by this phase).


# V1.1 phase 4 decisions (rotation earth-model alignment + startup wind-pair rule)

## Root cause of the residual rotation error (audit question answered)

The audit suspected the residual was a SPHERE-vs-ELLIPSOID mismatch between our
grid (fixtures declare CT 3.2 = 6, a sphere R = 6371229 m) and the pyproj check
(which defaulted to the WGS84 ellipsoid). Measured on this machine, that was
**only a secondary effect**; the dominant error was the ANGLE DERIVATION itself.
Re-implementing the previous Rust algorithm (`rot_angle_deg` = finite difference
of two `inverse()` calls, then a planar `dλ·cos φ` metric) and comparing it with
an independent pyproj measurement whose earth shape EXACTLY matches each fixture:

| quantity | worst angular error (deg) |
|---|---|
| old method vs matched-shape pyproj, **sphere** fixture (CT 3.2 = 6) | 7.3e-3 |
| old method vs matched-shape pyproj, **oblate** grid (International spheroid) | 2.4e-2 |
| new analytic angle vs matched-shape pyproj, sphere | 1.6e-13 |
| new analytic angle vs matched-shape pyproj, oblate (WGS84 axes) | 1.0e-11 |
| Rust `rot_angle_deg` vs pyproj table, both fixtures (42 nodes) | **4.8e-11** |

So even with the shapes aligned, the old method was off by ~1e-2 deg: differencing
the inverse transform over a FULL 3 km cell and then treating latitude/longitude
as a locally planar `Δφ, Δλ·cos φ` pair is a spherical approximation that does not
reproduce the CONFORMAL angle on any earth model, let alone an oblate one. With
shapes aligned AND the analytic derivation, agreement is at float-noise level.
Conclusion: root cause = derivation, not (only) the comparison harness. The audit's
hypothesis was refuted as the primary cause but confirmed as a real secondary one:
shape mismatch DOES matter, because n (and therefore β) depends on the eccentricity
— comparing CT-2 International-spheroid axes against pyproj WGS84 injects ~3e-4 deg,
which is why the checks below pin the axes from the FILE.

## Measurement method (why no geodesy library)

The quantity wgrib2/IPOLATES rotates by is the CONFORMAL angle: the bearing of the
grid +i axis from TRUE north. In a Lambert conformal conic map meridians are
exactly straight radial lines, so the chord between two points on the meridian
through P IS the meridian tangent — zero curvature error. `scripts/validate_rotation.py`
therefore measures β purely in pyproj's projected plane (forward transforms only,
earth shape read out of the fixture's section 3), which also sidesteps a local
pyproj limitation: `Geod(a=..., b=...)` returns NaN for custom ellipsoids here.

## Angle derivation now used (one sentence)

β = 90° + θ where θ is the cone-apex polar angle returned by `GridLambert::cone_polar`
— the same θ that `for_point` computes as n·(λ−LoV) and that `inverse()` consumes —
so the projection and the wind rotation are driven by one piece of math, and the
earth model enters only through n/rh built from the Code Table 3.2 axes.

## Inline user-defined axes (Code Table 3.2 codes 1/3/7)

wgrib2 `Earth.c::axes_earth` reads them from section 3 relative to the shape byte
(+1/+2 sphere radius, +6/+7 major axis, +11/+12 minor axis; code 3 in km, code 7
in meters, GRIB "scaled value" = INT1 factor + sign-magnitude INT4, value·10^-f).
Our reader now parses the same slots and calls `GridLambert::set_user_axes`, which
recomputes every projection constant. Before this, code 3/7 silently fell back to
a nearest-constant guess — a real correctness gap for any file declaring its own
spheroid, and fatal for rotation precision. `grid.bin` gained the two f64 axes plus
an inline flag (`GRID_VERSION` 1 → 2); warm-boot rebuilds restore them.

Note on the task text: it described CT 3.2 = 2 as "6378.137 / 6356.7523 km". wgrib2
(and the GRIB2 tables) define code 2 as the International spheroid 6378.160 /
6356.775 km; the WGS 84 numbers belong to a user-defined spheroid. We follow wgrib2
and therefore emit the ellipsoid fixture with **code 3 + inline WGS 84 radii**, which
is what lets it be validated against pyproj's DEFAULT WGS84 ellipsoid exactly.

## Fixtures exercising BOTH earth shapes

* `tests/fixtures/hrrr_small.grib2` — unchanged: CT 3.2 = 6 sphere R = 6371229 m.
* `tests/fixtures/hrrr_ellip_small.grib2` — NEW: LCC 41×31 @ 12 km, La1 40 N
  scanning south, Lo1 250 E, LoV 235 E, standard parallels 30/40, CT 3.2 = 3 with
  inline WGS 84 axes; UGRD/VGRD on the grid-relative grid (first messages), TMP on
  the earth-relative grid. Corroborated by wgrib2: `-radius` reports
  `code3.2=3 radius_maj=6378137.000000 radiusmin=6356752.310000 is_spherical=0`
  (asserted in `scripts/gen_fixtures.sh`) and 16 `-ll2ij` probes land in
  `expected.json` (asserted by `oblate_projection_matches_wgrib2_ll2ij`).

## Startup wind-pair rule (spec §4.4 wording)

`config::startup_wind_preflight(models, stable_window_secs, now_ns)` runs from
`main` after config validation, BEFORE ingest and before the port is bound.
Behavior matrix (all covered by `crates/wx-ingest/tests/preflight_windpair.rs`):

| configured components | first stable matching file | outcome |
|---|---|---|
| both, or neither | any | no check, no probe |
| exactly one | file whose WIND messages carry flag bit 0x08 | **hard startup failure** naming model, file, template 3.x, flag byte, counts, missing param |
| exactly one | file whose WIND messages are earth-relative | accepted, logged note |
| exactly one | dir empty / no matching file | deferred, logged note |
| exactly one | matching files exist but inside the stability window | deferred ("not stable yet") |
| exactly one | matching file unreadable / not GRIB | deferred ("could not probe") |
| exactly one | matching file has NO wind message | deferred ("cannot decide") |

Decisions worth recording:
* The probe classifies the FIRST message that IS a wind component (section 4
  category 2, number 2 or 3) rather than "the first grid in the file": HRRR-style
  files mix an earth-relative scalar grid with a grid-relative vector grid, so the
  naive reading would have answered "earth-relative" for `hrrr_small.grib2`.
* Stability at startup means "size > 0 and mtime at least `stable_window_secs`
  old" — a single boot-time probe cannot observe size/mtime over time the way a
  scan round does. `now_ns` is injected so tests control the clock.
* Only sections 1/3/4 are interpreted; section 7 is stepped over by its declared
  length, so probing a full-size GFS file costs microseconds.
* Everything short of a definitive answer DEFERS to the existing decode-time
  `check_wind_pair` rule, which stays in place (second line of defense) and still
  refuses to store half-rotated winds.

## Tests added (names)

* `crates/wx-ingest/tests/rotation_vs_pyproj.rs`: `rotation_matches_pyproj_within_1e_6_deg`
  (42 nodes, ≤1e-6 deg), `rotation_equals_convergence_relation_of_own_projection`
  (β = 90 + n·(λ−LoV) at ≤1e-12 deg), `rotation_shape_axes_are_distinct_earths`.
* `crates/wx-ingest/tests/hrrr_ellip_decode.rs`: `ellipsoid_axes_come_from_the_file`,
  `oblate_projection_matches_wgrib2_ll2ij`, `oblate_rotation_and_inverse_roundtrip_at_nodes`.
* `crates/wx-ingest/tests/preflight_windpair.rs`: `preflight_rejects_single_component_on_grid_relative_grid`,
  `preflight_accepts_pair_and_earth_relative_single`,
  `preflight_defers_when_nothing_stable_or_readable`,
  `wind_pair_gap_recognizes_raw_code_triples`, `probe_reports_wind_message_flags`.
* Tightened in place: `hrrr_decode.rs::hrrr_rotation_invariants` now asserts the
  Snyder relation at ≤1e-9 deg (was 5e-2). No expected VALUES changed: the rotation
  application itself is behaviorally identical apart from angles shifting by ≤2.4e-2
  deg toward exactness, and every stored-value test either uses regular grids or
  compares against analytic values recomputed with the same code path.

## Remaining §9.3 deviation (verbatim-short)

> The spec's golden `wgrib2 -new_grid_winds earth` comparison is NOT run: IPOLATES
> is Fortran-only (33 .f90 modules), this machine has no Fortran compiler (no
> gfortran/f95/ifort/flang/f2c; `sudo -n` needs a password), and both built
> binaries print "IPOLATES package is not installed". In place of it: the rotation
> angle is derived from our own validated LCC transform, cross-checked against
> pyproj with the earth shape matched EXACTLY per fixture (sphere CT 3.2 = 6 and
> inline WGS 84 CT 3.2 = 3) at 42 nodes to ≤1e-6 deg (observed 4.8e-11), plus
> magnitude-preservation and double-application involution on real decoded data,
> plus wgrib2 `-ll2ij` agreement for both grids. `scripts/validate_rotation_wgrib2.sh`
> enables the true golden automatically wherever wgrib2 links IPOLATES.

## Gate

`cargo test --workspace`: green (10 new tests over phase 3's count).
`cargo clippy --workspace -- -D warnings`: rc=0.

# V1.1 phase 5 — allocation-free query path (spec §5)

Spec §5 demands the query hot path does NO heap allocation apart from
building the response body. Milestone 4 left three inherited allocations
per query (`units` String clone, per-slot level `Vec<f64>`, forecast-hour
scratch `Vec<f64>`); phase 5 removes them AND extends the proof to the
whole handler lookup path.

## Interning strategy: borrow-from-snapshot, not interned tables

Chosen: **lifetimes tied to the snapshot Arc**, not a global string table.
The handler pins `Arc<IndexSnapshot>` for the whole request, so every
display string a query needs already lives either in the snapshot
(`units`, param keys) or in the request itself (query string). Making
`PointQueryContext<'s>` hold `&'s GridDef`, `&'s str` units and `&'s
MmapField` slot handles (instead of `Arc` clones + `String` clone) costs
zero at query time and zero extra memory at index-build time. A shared
interning table would have added snapshot-build cost and a second source
of truth for units strings for no measured gain (the allocator was never
reached by those fields after this change). The old `seed.build(...)`
contract lost its seed entirely: `build_context(snapshot, ...)` is a free
function; the seed literal was pure ceremony (build overwrote every field)
and its owned `units: ""` was part of the alloc count.

## Inline caps: 8 params, 8 entries (documented fallback)

`ParsedArgs.params` and `PointEntries.inline` are fixed arrays of 8
(`MAX_INLINE_PARAMS` / `MAX_INLINE_ENTRIES`). Operational multi-param
queries stay far below 8 (spec §7 shows two). Requests beyond the cap
fall back to a heap tail (`extra_params` / `extra`) allocated ONCE per
request; such requests are explicitly OUT of the zero-alloc guarantee and
the boundary is asserted nowhere weaker than the docs claim. No silent
truncation: all params are always answered.

## Error paths still allocate (deliberate)

`ParseFail.detail`, `arg_token`, `fail_token`, `error_message` build
Strings — they exist only to render error bodies, which the spec allows.
The zero-alloc gate measures success-class queries only.

## Measurement boundary (hotpath_full_alloc.rs)

Counting allocator wraps the FULL handler lookup: `parse_query` ->
`lookup_entries` (selection + evaluation + entry structuring incl. masked
entries) -> `entries_at`. EXCLUDED by design: `render_point_json` /
`render_entry` / `make_response` (response-body construction, spec allows)
and the axum router dispatch around the handler (not our code path). The
test asserts ZERO allocations AND ZERO frees across 20k mixed success
queries (exact node, seam, vertical, integral+fractional time interp,
multi-param, run-pinned, wind pair, HRRR LCC exact/vertical/time) plus
2000 masked-point queries on the small fixture.

## Bit-exactness of the refactor (spec §8 rule)

Nothing about arithmetic moved, but two kernels were re-shaped to read
different containers, so equivalence tests pin them:
* `resolve_levels_f32` (wx-core) vs `resolve_levels(Vec<f64>)`: identical
  bracket selection AND identical widened doubles over 4000 randomized
  level sets incl. eps-boundary wants, duplicates, gaps
  (`crates/wx-core/tests/alloc_free_kernels.rs`).
* `resolve_field_times` (index) vs materialized-`Vec<f64>` `resolve_times`:
  identical `Result` incl. guard order over 3000 randomized slot lists x
  both modes x stat flag (`crates/wx-ingest/tests/alloc_free_select.rs`).
* Response bytes: unchanged renderer (same field order, same `fmt_num`);
  existing HTTP-level golden assertions (point_http/meta_json/frac_http)
  pass untouched, and `hotpath_full_alloc.rs` re-asserts probes.tsv values
  (450 / 1642.5 / 480 / 1640) through the new path.

## Measured deltas (AMD EPYC 7402, portable build)

| Path | allocs/q before | allocs/q after | ns/q before | ns/q after |
|---|---|---|---|---|
| evaluate_point | 0 | 0 | ~330 | ~135 (bench ladder 354.6 -> 135.0) |
| build_context (was seed.build) | ~3 | 0 | included above | included above |
| full handler lookup (parse..entries) | ~6-7 | 0 | bench 828.8 | bench 694.3 |
| load test server-side mean | 12.5 us | 10.0 us | p50 bucket <=20 -> <=10 us | client R/s 286.9k -> 286.9k (kernel-I/O-bound, expected) |

Snapshot-build cost delta: none measurable (borrowing removes work; no
interning tables added). Retention/index semantics untouched.

## Gate

`cargo test --workspace`: green (95 tests; +4 over phase 4's 91).
`cargo clippy --workspace -- -D warnings`: rc=0.

# V1.1 phase 6 — docs finalization + acceptance sweep

Scope: documentation plus a FULL acceptance sweep. NO product-behaviour changes;
the two fixes the sweep forced are both non-product (script + bench print label),
listed below.

## Acceptance sweep results (AMD EPYC 7402, rustc 1.99.1, oha 1.16.0, kernel 7.0.0-31)

| Step | Result |
|---|---|
| `bash scripts/gen_fixtures.sh` | rc=0; 17 twin/checksum assertions ok (c3/jpeg/aec/ieee/png/png_f per model + stat jpeg + unsupported_drt guard + perf jpeg twin); md5s of all 20 `tests/fixtures/*.grib2` IDENTICAL across two consecutive runs → idempotent; 17.2 s wall |
| `cargo test --workspace` | rc=0; **95 passed / 0 failed across 38 test binaries**; 68.6 s |
| `cargo clippy --workspace -- -D warnings` | rc=0; zero lint diagnostics (only cargo's future-incompat NOTE about vendored `bitflags` 0.4.0/0.7.0, not a lint). Scope note: the documented gate is workspace lib+bin targets. Probing wider (`--all-targets`) surfaces PRE-EXISTING lint debt in wx-core test/bench code (`assertions_on_constants` from the `assert!(false)` idiom, `needless_return`, …) — 93 diagnostics, none in product code, none introduced this phase; left alone because fixing them would touch test bodies during a docs phase. |
| `cargo bench --workspace` | rc=0; medians: regular lat/lon project **20.430 ns**, LCC project **86.187 ns**, bilinear heap slab **30.746 ns**, bilinear mmap field **32.663 ns**, index point query **136.62 ns** (+2.0% vs the phase-5 record 135.0 ns — inside the ±2% session band; criterion flagged the change as noise-level), handler parse+lookup+render **681.74 ns** (`-p wx-server` re-run same session: 699.05 ns); handler percentile pass p50=p90=p99=**1 µs**, max 60 µs, mean 1.0068 µs, ≈**993k ops/s** single-thread |
| `bash scripts/smoke.sh` | **SMOKE PASS**; 17 `probes.tsv` ground-truth probes replayed, **0 failures**; ready after 10 s; release avg point-query latency **18.6 µs** over 422 queries |
| `bash scripts/loadtest.sh 20` | portable: **287,451 req/s** then **286,342 req/s** (two runs); server-side n = 5,752,100 / 5,730,170, mean **9.4 µs**, p50 ≤**10 µs**, p99 ≤**50 µs** → §8 targets MET; `NATIVE=1`: **281,305 req/s**, mean **9.9 µs** (MET, still NOT better than portable) |
| `python3 scripts/validate_rotation.py` | rc=0; 42 expected-angle rows (21 sphere CT 3.2 = 6 + 21 oblate CT 3.2 = 3 inline WGS 84) → `tests/fixtures/rotation_expected.csv`; Rust cross-check worst delta **4.80469e-11 deg** against a 1e-6 deg tolerance |
| extra pole probe (docs evidence) | on the full-geometry GFS fixture: `lat=90` → 422 `out_of_domain`, `lat=89.9` → 422, `lat=-90` → 422, `lat=89.875` → 200 exact node (value 250 K) — pins the README §9 true-pole caveat on measured behaviour |

## Minimal fixes made during the sweep (non-product)

* `scripts/loadtest.sh`: its pre-hammering sanity probe fired ONCE per URL and
  raced the field-by-field publisher — the HRRR 200-class URL answered 422 while
  its level stack was still PARTIAL, aborting an otherwise valid run. Each URL is
  now waited out (bounded 15 s) before hammering. Product behaviour untouched; the
  publish window itself remains documented (README §9) and is what the retry exists
  for.
* `crates/wx-server/benches/handler.rs` printed single-thread throughput with the
  unit `Mops/s` while computing ops/s (it rendered "993260 Mops/s"). Label corrected
  to `ops/s`; measurement code and numbers untouched.

## Publish granularity: decision kept, follow-up named (audit item)

DECISION: keep publish-after-every-field-store. WHY: spec §4.5 requires a run to
become visible forecast-hour by forecast hour as each completes, and per-field
publish satisfies that at the finest granularity with one `arc-swap` per field
store (no batch staging, no lock, no window where a whole group is withheld).
CONSEQUENCE accepted: the transient 422 `level_out_of_range` window for NON-STORED
levels mid-file. Evidence it is bounded and self-healing: `concurrency_http.rs`
counts those separately (`transient_422`) instead of asserting them away, requires
zero 5xx and zero torn JSON bodies, and a settled-state batch proves they are
gone once ingest finishes. FOLLOW-UP if ever needed: coarsen publish to per-(param,
forecast-hour-group) — needs a staged snapshot builder that publishes a whole group
at once, delaying first visibility of a field by up to one group. NOT taken: nothing
in §4.5 or §8 asks for it and current behaviour is spec-correct.

## DRT 5.1 rejection rationale (audit item)

GRIB2 Code Table 5.0 contains NO dictionary template: 5.0 simple packing, 5.2
complex packing, 5.3 complex + spatial differencing, 5.200 run length packing,
5.4 IEEE float, 5.40 JPEG2000, 5.41 PNG, 5.42 CCSDS AEC. So the task text's
"5.1 dictionary" names something that does not exist, and its "5.1 runlength" is
mis-numbered 5.200 (which we DO implement). DECISION: return typed
`UnsupportedData(1)` for template 5.1 rather than write a decoder, because (a)
there is no reference encoder OR decoder anywhere in the wgrib2 ecosystem to
validate against — any implementation would be unverifiable fiction, and (b)
guessing a template mapping risks wrong NUMBERS, which correctness-first forbids.
Evidence the rejection is real rather than assumed: `tools/wgrib2-full` itself
refuses to emit DRT 5.1, and the guard fixture `tests/fixtures/unsupported_drt.grib2`
declares it by hand so `dictionary_packing_rejected_with_typed_error` pins the
typed error end-to-end. Corrupt or unsupported section 7 stays a LOUD ingest ERROR
(not a skip), so the guard can never be silently downgraded into the skip-and-note
path (`robust_skips.rs`).

## Docs finalization decisions (this phase)

* README §9 LIMITATIONS narrowed to exactly five categories: §13 out-of-scope list;
  the typed-rejection list (one-line rationale each); the §9.3 environment block
  (pointer here + `scripts/validate_rotation_wgrib2.sh` stub); the transient-publish
  window; the ±90 fixture caveat. Claims that JPEG2000 / PNG / AEC / fractional
  forecast hours are limitations were REMOVED — they are implemented and tested
  (phases 2–3), and leaving them listed would misrepresent the build.
* README §8 became a TABLE: 9 rows, one per spec §9 requirement, each naming the
  test functions, the truth source (wgrib2 `-lon` / `-ll2ij` / `-checksum data`,
  pyproj, analytic grids) and the tolerance.
* New README §10 decision log summarizes V1.1 choices; docs/DECISIONS.md stays the
  full per-phase record.
* `stat_ranges` visibility documented WHERE they actually are (per forecast hour in
  `manifest.json`), with an explicit note that `/v1/inventory` exposes only
  `stat_processed`/`stat_kind` plus decimal forecast hours — documentation follows
  the code, not the other way round.
* Metrics section documents `wxpoint_skipped_messages_total` and states it never
  increments `wxpoint_ingest_errors_total`.

## Gate

`cargo test --workspace`: green (**95 tests / 38 binaries**, unchanged from phase 5).
`cargo clippy --workspace -- -D warnings`: rc=0.
Tag: `v1.1`.

# V1.1 scope statement — AGENTS.md requirements, met how

| § | Requirement | Where met |
|---|---|---|
| 3 | Single TOML config, param→(discipline,category,number) table incl. the 13 named params + raw triples, level-type → CT 4.5 mapping, loud startup validation | `config.example.toml`; `wx-core/src/tables.rs` param/level tables; `config_validate.rs` (unknown param/level/retention/dupes fail with named messages) |
| 4 | Discovery (notify + periodic scan), stability window or sidecar, persistent ingest record with content hash, GRIB-template decode coverage, never trust filenames, skip-unpack for unconfigured params, bitmap→NaN, PDT 4.0 + 4.8 with process type and time ranges, parallel rayon ingest off the HTTP runtime, grid templates 3.0 + 3.30 with seam wrap and N→S scan flags, LCC forward transform with precomputed constants and CT 3.2 earth shapes, longitude normalization, wind rotation to earth-relative + single-component config failure, storage layout with atomic writes and per-fh visibility, retention after index removal | `pipeline.rs` (watch+scan, `stability.*`, `state/files.json` FNV-1a64 record), `grib2.rs` (DRT 5.0/5.2/5.3/5.200/5.4/5.40/5.41/5.42, sec-4-before-sec-7 filter, bitmap→NaN, PDT 4.0/4.8 + ranges), `grid.rs` (regular + Lambert, inline axes), `store.rs`/`fsync_path` (temp→fsync→rename→dir fsync), `retention.rs`, `preflight_windpair.rs` + `check_wind_pair`; proven by `stability.rs`, `packing_twins.rs`, `checksum_parity.rs`, `skip_unpack_counters`, `stat_pdt.rs`, `restart.rs`, `retention.rs`, `parallel_decode.rs` |
| 5 | Read-mostly index published with arc-swap, all mmaps opened at index build, hot path lock-free and allocation-free beyond the response body | `index.rs` + `arc-swap`; `hotpath_alloc.rs` and `hotpath_full_alloc.rs` (counting allocator: ZERO allocations over 20k queries) |
| 6 | Bilinear horizontal (exact-node shortcut, no extrapolation, NaN→null), linear vertical in the level coordinate with bracket echo and no extrapolation, linear time between forecast hours of ONE run with stat-field guard and `time_interp=none`, order H→V→T in f64 | `query_point` / `resolve_levels*` / `resolve_field_times` + `time_lerp`; seconds-based fh math; covered by `kernels.rs`, `alloc_free_kernels.rs`, `alloc_free_select.rs`, `stat_pdt.rs` |
| 7 | `/v1/point` parameter set and JSON shape, native units, null-with-reason, 400/404/422 error classes, `/v1/models`, `/v1/inventory`, `/health`, `/ready`, Prometheus `/metrics` | `httpapi.rs` + `queryapi.rs` + `stats.rs`; `point_http.rs`, `meta_json.rs`, `frac_http.rs`, smoke probe replay |
| 8 | p50 < 50 µs / p99 < 250 µs server-side, ≥50k req/s on 8 cores, one GFS forecast hour queryable within seconds of file stability, criterion benches, load-test script, release profile with lto=fat/codegen-units=1/panic=abort, target-cpu=native measured and documented | measured repeatedly (§7 of README): p50 ≤10 µs, p99 ≤50 µs, 286–287k req/s, first-queryable 3.9 s; `benches/{kernels,query_index,handler}.rs`, `scripts/loadtest.sh`, root `Cargo.toml` profiles; native variant measured WORSE and NOT shipped |
| 9 | The nine correctness verifications, wgrib2/pyproj truth sources, fixtures under version control + regeneration script | README §8 map table (9/9 rows); `tests/fixtures/*` + `scripts/gen_fixtures.sh` (idempotent, checksum-equal twins) + `scripts/regen_expected.sh` + `scripts/validate_rotation.py`; the ONLY spec item not run as written is §9.3's IPOLATES golden (environment-blocked, replaced by pyproj dual-shape check) |
| 10 | Suggested workspace layout with async/HTTP-free `wx-core`, benches/, tests/, config.example.toml, README | `crates/wx-core` (memmap2 only; rayon/serde confined to tests/benches), `crates/wx-ingest`, `crates/wx-server`, `benches` inside each crate, `tests/`, `config.example.toml`, this README |
| 11 | Each milestone leaves `cargo test` and `cargo clippy -- -D warnings` passing | every phase gate above records rc=0; phase 6 sweep re-confirms 95 tests green and clippy clean |
| 12 | Deliverables: building workspace, example config, README covering setup/config/API/storage/interpolation/benchmarks/limitations, list of ambiguity decisions | this repo: builds on stable rustc 1.99.1; `config.example.toml` covers GFS + HRRR; README §§1–10; ambiguities in docs/DECISIONS.md (per phase) + README §10 |

# V1.2 phase A decisions — storage WRITER PRIMITIVES

Scope: ONLY `crates/wx-core/src/storage.rs` + `crates/wx-ingest/src/store.rs`
(+ new tests and one golden fixture/script). The ingest PIPELINE FLOW
(`scan_round` / `ingest_messages` / `store_plane`) is UNTOUCHED: grouping
messages per (param, level type, forecast hour) is Phase B's job.

## Streaming atomic writer (audit fix: whole-file `Vec<u8>` materialization)

* New `write_bin_atomic_stream<S: PlaneSource>(path: String, hdr: &BinHeader, count: usize, src: S)`:
  header + planes are serialized THROUGH a fixed `WRITE_CHUNK_BYTES` = 256 KiB
  buffer straight into the temp file. Peak writer memory is ONE chunk instead of
  the whole file (a single GFS plane is already 4.1 MB, and pre-change the file
  existed THREE times: merged planes, `refs`, then the full byte buffer).
* `PlaneSource` (`fn next_plane(self: &mut Self) -> Option<&[f32]>`) yields
  BORROWED planes; wx-core has a private `RefPlanes` adapter over
  `&[&[f32]]`, wx-ingest has `MergedPlanes` over its merged `(level, plane)`
  vector. Planes are never copied by the writer.
* `write_bin_atomic(path, hdr, planes: &[&[f32]])` KEPT as a THIN WRAPPER over
  the streaming path, so every pre-existing caller (pipeline, restart, benches)
  compiles UNCHANGED.
* Source CONTRACT enforced: fewer than `count` planes → error; an EXTRA plane
  after `count` → error (never appends stray planes); plane length != nx*ny →
  error. All before the rename, so a bad source can never publish anything.

## Golden-byte proof (output format unchanged, bit for bit)

Expected bytes were captured FROM the V1.1 implementation BEFORE the change:
`tests/fixtures/writer_golden_v1.bin` (456 B, sha256 `161f4fc1…`), reproduced
independently by `scripts/gen_writer_golden.py` (Python `struct.pack` of the
documented layout — identical bytes, idempotent across re-runs). The test ALSO
embeds those bytes as hex and keeps a RETAINED REFERENCE IMPLEMENTATION of the
OLD algorithm (serialize the WHOLE file, then `write_all`), so FOUR things are
compared byte-for-byte: committed fixture, embedded hex, wrapper output,
streaming output, reference-impl output. A second test uses a 700×600 grid
(1.68 MB of planes ⇒ MANY internal flushes) to prove multi-chunk writes stay
bit-identical.

## Batch merge (audit fix: K writes + double clone)

* `merge_field` keeps its public signature (only adds `mut` to the parameter,
  source-compatible): existing levels are read back through
  `MmapField.plane_bytes` SLICES via `plane_from_bytes` (no per-element
  `mf.value(li, j, i)`), planes are MOVED into the result (ZERO clones), and
  canonical order is applied by selection sort IN PLACE using `Vec::swap`.
* New `merge_write_field(path, code, nx, ny, scan_ns, winds_grid, new) ->
  Result<usize, String>` merges a GROUP ONCE and writes ONCE (returns the
  resulting LEVEL COUNT): O(L+K) passes over L existing + K new planes, ONE
  temp file, ONE rename. `store_plane` still drives the single-level path until
  Phase B groups messages, so today's behavior is unchanged where K == 1.

## Level dedup fix (audit bug: f64-exact comparison)

Canonical LEVEL KEY = compare as f32: `(a as f64) as f32 == (b as f64) as f32`,
because the on-disk coordinate IS an f32 (`BinHeader.levels`). Pre-change the
code seeded from `mf.header.levels[li] as f64` and compared f64 EXACTLY, so a
level that is NOT exactly representable in f32 (e.g. 837.55 hPa, stored as
837.5498…) failed to match its own stored value and was APPENDED as a DUPLICATE
plane on every re-ingest. Note on the task text: its example 837.5 IS exactly
representable in f32, so the bug does not reproduce there; the test pins BOTH
(837.55 reproduces the duplication class, 837.5 covers the named path).
Ordering rule PRESERVED: isobaric DESCENDING, every other level type ASCENDING.
Duplicate keys inside ONE batch resolve to the FIRST entry in caller order
(same rule as V1.1; implemented with `pop()`-backwards + documented).

## Temp-file hygiene + directory-fsync batching

* `<path>.tmp.<pid>.<seq>` from PUBLIC `unique_tmp_path` (pid separates
  PROCESSES, a process-local atomic sequence separates THREADS/retries); shared
  by the .bin writer AND wx-ingest's byte writer. The old doc comment promised a
  pid the code never appended — now literally true.
* UNLINK ON ANY FAILURE: one tail in the writer removes the temp file for EVERY
  failure class (create error, short/extra plane, write error, flush error,
  fsync error, rename error). This dialect has NO user-defined destructor, so
  cleanup is explicit and centralized rather than RAII.
* `write_bin_atomic*` NO LONGER fsyncs the parent DIRECTORY (that was one
  `fsync(2)` per rename). NEW `TouchedDirs` collector (`note` / `note_dir` /
  `pending_count` / `flush`; duplicates collapse, list clears after flush) lets
  callers fsync each DISTINCT directory ONCE. For now `write_field` and
  `write_bytes_atomic_pub` still fsync their OWN directory, so crash durability
  of field files / grid.bin / manifest.json is UNCHANGED today; Phase B will
  batch them across a forecast-hour group. README claim update deferred to Phase
  D; noted in code comments now.
* Still plain `.tmp` and NOT counted: `state.rs`'s state-file writer (out of
  phase-A scope; named follow-up).

## Instrumentation counters

Module statics in wx-core storage — UPPER-CASE because this toolchain enforces
static naming — `RENAMES_TOTAL`, `BYTES_WRITTEN_TOTAL`, `FSYNCS_TOTAL` (+
`TMP_SEQ` for temp-name sequencing), all relaxed atomics. Accessors:
`counters_snapshot() -> StorageCounters { renames, bytes_written, fsyncs }`,
`counters_reset()`, plus `count_rename()` / `count_bytes_written(n)` so wx-ingest's
byte writer feeds the SAME totals. `fsync_path` counts every fsync it performs
(file AND directory); a failed `open` costs no fsync. As of V1.2 phase D these three
totals ARE exported by `/metrics` as `wxpoint_storage_renames_total`,
`wxpoint_storage_bytes_written_total` and `wxpoint_storage_fsyncs_total`, which is
what `scripts/ingestbench.sh` reads for the AFTER-side amplification numbers.

## Tests added (names)

* `crates/wx-core/tests/writer_golden.rs`: `streaming_writer_output_is_byte_identical_to_golden`,
  `streaming_writer_chunks_across_the_buffer_boundary_identically`
* `crates/wx-core/tests/writer_counters.rs`: `writer_counters_and_batched_dir_fsync`
  (4 writes ⇒ renames 4, fsyncs 4 — NO dir fsync per rename —, bytes == 4 ×
  `expected_size`; `TouchedDirs.flush` over 2 distinct directories adds EXACTLY
  2 fsyncs and a second flush costs 0; contract violations rename NOTHING, leave
  no `.tmp.` litter and no target file)
* `crates/wx-core/tests/writer_failures.rs`: `failed_writes_leave_no_temp_litter_and_keep_the_original`
  (read-only directory via libc `chmod`; mid-write plane-length failure; original
  values intact value-for-value; temp-name uniqueness + pid tag asserted)
* `crates/wx-ingest/tests/store_batch.rs`: `batch_merge_of_k_levels_onto_l_existing_costs_one_write`
  (K=2 onto L=3 ⇒ ONE rename, bytes delta == EXACT resulting file size and ≤
  (L+K)·plane + header slack; colliding level REPLACED not duplicated; `merge_field`
  alone performs no write at all)
* `crates/wx-ingest/tests/store_dedup.rs`: `reingesting_a_level_replaces_it_instead_of_duplicating`,
  `duplicates_inside_one_batch_resolve_first_entry_wins`,
  `slice_readback_matches_scalar_readback_and_order_survives_merges`

Counter-sensitive gates live in their OWN test binaries (ONE test each) because
the counters are process-wide while cargo runs tests INSIDE one binary
concurrently; scratch trees are wiped first so the gates are repeatable (a
leftover field file changed the expected LEVEL COUNT on the second run — caught
during development).

## Call-site adaptations

None beyond compilation-neutral ones: `merge_field` gained `mut` on its parameter;
`write_field` keeps its signature (streams instead of building `refs`, does its own
directory fsync); `pipeline.rs` is untouched.

## Gate

`cargo test --workspace`: green **103 tests / 43 binaries** (95/38 baseline + 8
new across 5 new binaries), executed TWICE back-to-back with identical results.
`cargo clippy --workspace -- -D warnings`: rc=0, zero lint diagnostics (only the
vendored-`bitflags` future-incompat NOTE). `--all-targets` still carries the
pre-existing test-code lint debt documented at phase 6 (now 138 diagnostics
including the new test files, same categories: `needless_return`,
`assertions_on_constants`, index-loops); PRODUCT code (lib+bin) is clean.


# V1.2 phase B0 decisions — correctness hotfix (per-slot level indices, monotonic stability clock, upgrade recovery)

Scope: the three P1 items of the V1.1 audit (`AUDIT_1.1.md`, untracked third-party
review file — its prose/ratings NOT trusted, only independently verified claims).
Item 1's CRITICAL claim was verified TRUE by reading the code; items 2 and 3 were
verified as described (item 2's reproduction RATE was NOT reproducible locally, see
below). Retention-skip / overlap-validation refactors stay in Phase C.

## 1. Per-slot vertical interpolation (silent wrong answer — FIXED)

* `index::evaluate_point` sampled TIME SLOT 1 with SLOT 0's level indices
  (`h_at(&sr, ss0.li0)` / `ss0.li1`) and bracketed it with SLOT 0's level VALUES.
  Two forecast hours of one run CAN carry different level stacks (normal during
  partial ingest), so slot B read the WRONG PLANE and was blended on the wrong
  pressure bracket: a plausible number, no error → §6.4 violated.
* Fix: each slot resolves and reads with ITS OWN `li0`/`li1` and ITS OWN
  `field.header.levels`. Spec §6.4 order implemented literally: horizontal bilinear
  at each (level,time), then VERTICAL PER TIME SLOT (`f_slot = vertical_at(...)`),
  THEN the two vertical results are time-blended. Degenerate cases handled per slot:
  an exact level hit in one slot (`li1 == li0`) skips that slot's vertical step (no
  divide-by-zero), while the other slot may still bracket; a level can be exact-present
  in one stack and bracketed in the other. NaN propagation UNCHANGED (any of the four
  samples NaN → `MissingData`, weights never renormalized).
* `build_context` still rejects when EITHER slot cannot bracket the requested level
  (never extrapolate), so a level inside one stack but OUTSIDE the other's stored
  range stays a 422 `LevelOutOfRange` — documented, deliberate.

## 2. JSON decision: additive `levels_per_time` (truthful AND back compatible)

Chosen over "report slot 0's pair only" (misleading) and over "omit metadata when
they differ" (loses information):

```json
"vertical":{"method":"linear","levels":[900,800],
            "levels_per_time":[[900,800],[950,850]]}
```

* `VertMeta` gained `levels_later: (f64,f64)` and `per_time: bool` (still `Copy`,
  stack-only — the ZERO-ALLOC hot-path guarantee is untouched; `hotpath_alloc` and
  `hotpath_full_alloc` still report 0 allocations).
* `"levels"` keeps the EARLIER slot's pair ⇒ every pre-existing client parses the
  spec §7 shape unchanged. The extra key is emitted ONLY when the pairs genuinely
  DIFFER, so uniform-stack bodies are BYTE-IDENTICAL to V1.1 (pinned by
  `point_json_omits_levels_per_time_when_brackets_agree`).
* Server-side rendering lives in `queryapi.render_entry`; README §5 documents the
  extension.

## 3. wx-core mirror: NO bug, contract documented + tested

`interp::query_point` resolves the bracket ONCE against `FieldSamples.levels` and
addresses slabs as `li * nt + ti`, so a level index can never be applied to another
time's data — the flat-index confusion was specific to the ingest evaluator. Its
data model DOES assume ONE level stack shared by all times (documented as the
MIRROR CONTRACT on `FieldSamples`: production queries use the ingest evaluator; a
caller with per-hour stacks must feed the union stack with duplicated slabs or
evaluate hour-by-hour). Cross-slot independence pinned by new
`crates/wx-core/tests/slot_independence.rs`:
`per_pair_planes_compose_horizontally_then_vertically_then_in_time` (planes distinct
per (level,time) pair ⇒ any transposition changes the number) and
`exact_forecast_hour_reads_only_its_own_slabs` (rewriting the non-selected hour's
planes leaves the answer BIT-IDENTICAL; rewriting the selected hour moves it exactly
as predicted).

## 4. Stability window moved to a MONOTONIC clock (flake root cause)

* `discovery::MonoClock` wraps `std::time::Instant` (`Instant::now()`, subtraction,
  `Duration::as_nanos()` — this toolchain has no `Instant.now()` VALUE form and no
  `std::chrono`; statics are called with `::`). `Pipeline` owns one; `scan_round`
  feeds `clock.elapsed_ns()` into `StabilityTracker::observe`.
* File IDENTITY is UNCHANGED (path + size + mtime_ns); mtime is now IDENTITY-ONLY.
  A changed (size,mtime) resets the observation stamp; window expiry is measured
  purely monotonically. `state::now_ns_marker` REMAINS for wall-clock uses
  (preflight staleness, perf-lag probes) but is no longer on the ingest path.
* Why it mattered: with `stable_window_secs = 0` the predicate is
  `now - first >= 0`, which a BACKWARD stamp makes NEGATIVE ⇒ the round ingests
  NOTHING. Reproduction evidence:
  * BEFORE fix: 10 consecutive `cargo test --test hotpath_full_alloc` runs, rc=0
    every time (the audit's 2-of-11 rate did NOT reproduce locally — the trigger is
    a wall-clock step-back during those ~25 s, not ordinary scheduling).
  * MECHANISM proven deterministically by injecting a NON-MONOTONIC stamp source
    (alternating 5 s backward jumps) into `scan_round`: `stability.rs` then fails
    at `r4 == 1` (file never becomes eligible) — the audit's failure mode, reproduced
    on demand. Reverted after capture.
  * AFTER fix: 10 more consecutive runs, rc=0 every time, AND the same hostile
    clock injection leaves `hotpath_full_alloc` GREEN (the settle loop recovers on
    the next round instead of failing the gate).
* New unit gates in `crates/wx-ingest/tests/stability_clock.rs`:
  `mono_clock_never_decreases_and_tracks_real_time`,
  `window_eligibility_is_measured_monotonically`,
  `backward_wall_clock_cannot_freeze_the_window_anymore` (asserts the OLD rule
  `(backward_stamp - first) >= win` would have been false forever),
  `identity_change_resets_the_monotonic_window` (size-only and mtime-only re-delivery
  both reset).
* `tests/stability.rs` spin replaced by a SLEEPING bounded wait on the monotonic
  clock with a 10 s deadline plus an assertion that the deadline was not hit — the
  old busy-loop could hang the suite FOREVER if the marker clock stopped advancing.
* Gate tests made deterministic by RULE: never assert WHICH scan round ingests;
  accumulate over a BOUNDED settle loop and assert the TOTAL
  (`hotpath_full_alloc`: total 4 / total 1; `grid_upgrade.settle`). This also absorbs
  the OTHER realistic skip class — a transient `stat_size_mtime` / `content_hash_file`
  error silently skips ONE candidate for that round.

## 5. GRID_VERSION 1→2 upgrade trap: automatic recovery (no silent empty inventory)

Chosen recovery (over "force grid rewrite" — cannot, the v1 bytes lack the earth
axes — and over "document `rm -rf <data_dir>`"): in `ingest_run_dir`, when
`decode_grid` FAILS on an existing `grid.bin`:
1. LOUD note naming the run dir, decode reason, found vs current version
   (`STORAGE-FORMAT RECOVERY: …`) + `counters.errors += 1` (so `/metrics` sees it);
2. DELETE the unusable run directory;
3. FORGET that MODEL's ingest records (`StateStore.drop_under(ingest_dir)`, matching
   on `<prefix>/` so sibling dirs are untouched) and persist state immediately, so
   the NEXT scan round re-decodes from source with the CURRENT format.
Idempotent and correctness-preserving (re-decode is deterministic); costs one re-ingest
per affected model. Requires the SOURCE files still be present in `ingest_dir`; if they
were pruned upstream the run is simply GONE, loudly — never a silent empty inventory.
README §9 documents the migration behavior.

Gates in `crates/wx-ingest/tests/grid_upgrade.rs`:
`v1_grid_bin_triggers_loud_recovery_and_the_run_rebuilds_from_source` (planted
GRID_VERSION=1 blob ⇒ log + errors==1 + run dir deleted + `"recs":[]` + no gfs run
indexed, then settle-scan rebuilds with GRID_VERSION=2 on disk and the node probe
answers 250 K exact-node again) and `healthy_run_dirs_are_left_untouched_by_the_recovery_path`
(current-format warm start: nothing deleted, log EMPTY, unchanged files NOT re-ingested).

## 6. Tests added (names)

* `crates/wx-ingest/tests/slot_levels.rs` — `differing_level_stacks_across_time_bracket_interpolate_per_slot`
  (f006 `[1000,900,800,700]` vs f012 `[950,850,800,700]`, analytic field
  `L*100 + F*10 + (j*10+i)`; expected 87535.5, pre-fix 85660.5 asserted away),
  `identical_stacks_keep_single_vertical_bracket_metadata`,
  `exact_level_in_one_slot_bracket_in_the_other`,
  `masked_plane_in_either_slot_still_returns_missing_data`.
  PROVEN to be real regression gates: with the pre-fix slot-B semantics re-injected,
  the first and third FAIL (`pv.value == 87535.5` / `== 85035.5`); with the fix all
  four pass.
* `crates/wx-core/tests/slot_independence.rs` — 2 tests (mirror check, above).
* `crates/wx-server/tests/point_json_brackets.rs` — `point_json_reports_both_brackets_when_stacks_differ`
  (real axum router over a hand-built snapshot; asserts
  `"vertical":{"method":"linear","levels":[900,800],"levels_per_time":[[900,800],[950,850]]}`
  and `"value":87535.5`) and `point_json_omits_levels_per_time_when_brackets_agree`.
* `crates/wx-ingest/tests/stability_clock.rs` — 4 tests (above).
* `crates/wx-ingest/tests/grid_upgrade.rs` — 2 tests (above).
* Modified: `crates/wx-ingest/tests/stability.rs` (bounded sleeping wait),
  `crates/wx-server/tests/hotpath_full_alloc.rs` (settle loops instead of
  `r2 == 4` / `r2x == 1`).

## 7. Gate

`cargo test --workspace`: green **117 tests / 48 binaries** (103/43 baseline + 14 new
across 5 new binaries), executed twice back-to-back with identical results, PLUS
**10 consecutive runs of `cargo test --test hotpath_full_alloc` at rc=0** (the previously
flaky binary) after the monotonic-clock change. `cargo clippy --workspace -- -D warnings`:
rc=0, zero diagnostics (vendored-`bitflags` future-incompat NOTE only; NEVER `-q`).
`AUDIT_1.1.md` left UNTRACKED and untouched (origin unknown; not trusted, not deleted).

# V1.2 phase B decisions — ingest loop restructure (chunked decode, write-once grouping, poison quarantine)

Audit anchors verified in the then-current source before touching anything: `scan_round`
decoded EVERY eligible file of a model in ONE `pool.install` call and only then wrote;
`store_plane` merged + wrote the field file, rewrote `manifest.json` and published
PER MESSAGE (78 plane serializations per GFS hour, 22 field renames + 22 manifest
renames per file); the Err branch recorded NOTHING in `state`, so a corrupt file was
re-decoded (and re-counted as an error) on every round; wind pairs were only removed from
the pending map when BOTH components arrived, so an orphan silently persisted across
messages with no note; progress/metrics surfaced only AFTER a whole round.

## 1. Chunked decode: new key `[ingest].decode_batch_files` (default 4, validated >= 1)

K bounds how many candidate files are DECODED before ANY is written. Cross-file
parallelism is preserved INSIDE a chunk (one rayon pass), peak decode RAM is ~K files,
and the FIRST queryable output lands after the first file of the first chunk instead of
after the slowest file of the whole directory. Documented in `config.example.toml` and
README §2; rejected at startup with `ingest.decode_batch_files must be >= 1`.

## 2. Per-FILE consumption, in this order

```
candidates -> ordered list -> chunks of K
  per chunk: magic precheck (non-GRIB -> kind=IGNORED, never decoded)
             one rayon decode pass over the surviving K
             per result:
               Err  -> kind=FAILED record WITH content hash (error counted ONCE
                       per new/changed hash, never once per round)
               Ok   -> ingest_file(): pair winds (map scoped to THIS file, drained
                        at end: orphans noted + counted as skips, removed)
                     -> group planes by (epoch, param_key, level_code, fh_secs)
                     -> merge_write_field_batch ONCE per GROUP (dir fsync deferred)
                     -> registry_put per group; manifest ONCE per affected run
                     -> TouchedDirs.flush ONCE; publish ONCE
                     -> state upsert + save; live counters; progress line
```

Group key extends the WindPending-style keying with the RUN EPOCH because ONE file can
carry TWO runs (`tests/fixtures/frac_small.grib2`). Levels inside a group keep caller
order, so `merge_field`'s documented "first entry wins" rule applies inside a group too.

## 3. Publish granularity: per FILE, not per message

Spec §4.5 ("a run becomes visible forecast hour by forecast hour") stays satisfied because
products arrive per forecast hour. This also closes most of the transient-422 window
documented in §9: a file's groups all become visible together rather than one message
at a time.

## 4. Poison quarantine

* `FileRec` gained `kind: u8` (0 OK / 1 IGNORED / 2 FAILED) with `#[serde(default)]`
  so pre-V1.2 `files.json` files (no `kind` key) still load as OK. The hand-written
  serializer emits `"kind":N`.
* `state::head_is_grib()` mmaps FOUR bytes: a glob-matching sidecar (`.idx`, `.xml`)
  never reaches the decoder. An unreadable/short file writes NO record (retried).
* FAILED records carry the content hash, so the existing "unchanged (size,mtime,hash)"
  check makes rounds 2..N free of re-decodes; mutating the file re-arms exactly ONE
  retry and exactly ONE more error.

## 5. Failure propagation

Any GROUP whose write fails (simulated ENOSPC via read-only level dir): the other groups
of that file stay written (each group is atomic on its own), the SOURCE FILE is NOT
upserted so the NEXT round retries it idempotently, and `errors` advances ONCE for the
file-with-failure, not once per failed group. No `.tmp.` litter (phase-A unlink-on-failure
still applies). A `TouchedDirs.flush()` failure marks the file failed the same way
(parity with the V1.1 per-write dir fsync).

## 6. Live counters (`Pipeline.live: Arc<LiveCounters>`)

`IngestCounters` gained `ignored_files`, `quarantined_files`, `groups_written`;
`LiveCounters` mirrors them plus `progress_seq` and `last_msg` as relaxed atomics,
updated after EVERY consumed file. `QueryStats.attach_live`/`new_stats_with` share the
pipeline's Arc; `/metrics` renders `max(legacy store, live)` so a scan round in flight
is visible immediately. New gauges: `wxpoint_ignored_files_total`,
`wxpoint_quarantined_files_total`, `wxpoint_ingest_groups_written_total`.
`set_ingest` keeps its absolute-store contract (back compat) and additionally merges
monotonically into the mirror; `add_ingest(...)` is the delta API for embedders that do
NOT hold the pipeline Arc. `main` spawns a printer thread that prints
`ingested <file>: G groups, W bytes, X ms` whenever `progress_seq` advances — the only
way to get truly mid-round lines without a callback type shared across packages.

## 7. Tests added (names)

* `crates/wx-ingest/tests/ingest_grouping.rs` — `one_file_writes_groups_once_not_messages`:
  gfs_small (39 messages, 36 stored) -> 18 field files, renames == 18+grid.bin+manifest
  == 20 (< 39), BYTES_WRITTEN delta == final store size EXACTLY, fsyncs ==
  fields + 2 aux writers + 6 DISTINCT dirs + 2 per state save; values unchanged
  (TMP@850 f006 node == 421 exact-node).
* `crates/wx-ingest/tests/ingest_perf_grouping.rs` — `perf_hour_costs_eight_group_writes_not_twenty_two`
  (skips when `tests/perfdata` absent): renames 10 then 9 per hour, 16 field files,
  bytes <= 1.1x final store (measured 1.0000x), prints the MEASURE line used below.
* `crates/wx-ingest/tests/poison_quarantine.rs` — `poison_files_are_quarantined_not_retried`:
  good + `.idx` sidecar + truncated GRIB; rounds 2..5 re-decode NOTHING and add no
  errors; mutating the truncated copy re-attempts exactly once; restart preserves
  kind=IGNORED/FAILED and adds no new error; `files.json` carries the kind column.
* `crates/wx-ingest/tests/disk_full_retry.rs` — `unwritable_group_dir_fails_once_and_retries_cleanly`:
  read-only level dir (libc chmod) -> ONE error, NO state record, NO `.tmp.` litter,
  unwritable field absent from disk AND index while writable groups landed; chmod back
  -> full retry ingests and answers 421.
* `crates/wx-ingest/tests/restart_midchunk.rs` — `per_file_saves_make_a_midchunk_restart_partial`:
  K=2 with three files (chunks [a,b] then [c]), ONE publish each; truncating the on-disk
  state to the saved prefix leaves exactly ONE file to re-ingest after reopen; also gates
  `decode_batch_files = 0` failing config validation.
* `crates/wx-ingest/tests/wind_orphan.rs` — `orphan_wind_components_are_drained_per_file`:
  new fixture `tests/fixtures/hrrr_orphan.grib2` (first four messages of hrrr_small: TMP x2
  + UGRD x2, no VGRD) -> both pending entries noted as ORPHANS, counted as skips, REMOVED,
  UGRD never stored; the next file pairs normally (no leak) and stores earth-relative
  (header flag bit1 clear); the sticky pair rule logs ONE violation note plus at most
  ONE skip note per file.
Existing concurrency tests (`concurrency_http.rs`, `concurrency.rs`) stayed green under the
new publish cadence.

## 8. Measured before/after (this machine, release build, perf dataset)

BEFORE numbers were measured, not estimated: a detached worktree of the pre-phase-B
HEAD ran the identical harness (`zz_measure.rs`, deleted afterwards).

| metric | BEFORE (per-message writes) | AFTER (K=4 default) | AFTER (K=1) |
|---|---|---|---|
| first-queryable (/ready) | 8276 ms | 4047 / 4083 ms | 4971 ms |
| last forecast hour visible | 9435 ms | 4568 ms | 5683 ms |
| server CPU time | 7.55 s | 4.2 s | 4.2 s |
| peak RSS (VmHWM) | 362 MB | 421-437 MB | 177-223 MB |
| renames / GFS hour | 45 (then 44) | 10 (then 9) | same 10/9 |
| bytes written vs final store | 647.0 MB vs 182.5 MB (**3.55x**, 78 plane serializations where 8 suffice) | 182.48 MB vs 182.48 MB (**1.0000x**) | same |
| fsyncs / GFS hour | 94 | 26 | 26 |

Decode cost itself is untouched (same decoder, same pool); the wall-time win comes from
publishing after the first file of a chunk instead of after the slowest file of the
directory, and the CPU win from removing ~4x redundant plane serialization.

## 9. Surprises / deviations worth remembering

* Peak RSS went UP (+15%) at the default K=4 versus the old code: grouping keeps a
  file's planes resident until its GROUP is written, whereas the old path streamed each
  plane out immediately. K is the knob (K=1 measures BELOW the old profile).
* Wind components dropped by the sticky pair rule now increment `skipped_messages`
  (previously silent), and orphaned components are noted. Counter semantics documented
  in README §"Metrics"; no existing test asserted those counts.
* In these fixtures the grid-relative flag lives in section 3 of the WIND messages only
  (`scripts/gen_grib2.py` passes `wind_grid=True` for UGRD/VGRD), so a pair-rule violator
  still stores TMP. Behavior is identical to V1.1; the test comments say so.
* `wx_core::storage::open_field()` on a field file written EARLIER IN THE SAME PROCESS
  returns "mmap failed" here; the product path opens mmaps only in a fresh process
  (warm start), so this is a test-harness limitation, not a storage bug. The wind test
  therefore reads the header BYTES (flags byte at offset 22) instead.
* `state.save()` runs once per consumed FILE plus once at round end; the round-end save
  is kept so an all-skip round still persists IGNORED/FAILED verdicts.

## Gate

`cargo test --workspace` green twice back-to-back (54 binaries, 0 failed), `cargo clippy
--workspace -- -D warnings` rc=0.

# V1.2 phase C decisions — retention-aware skip, config overlap guard, state hygiene, strict dates, exit codes

Scope: build ON TOP of phases A / B0 / B. Untouched here: slot-index interpolation (B0),
writer primitives (A), grouping / chunking / poison-quarantine core (B).

## 0. Audit claims verified against current source BEFORE fixing

Line numbers had drifted across phases, so each claim was re-checked in place:

| claim | status | where |
|---|---|---|
| `timeparse.parse_iso_z` validated the day as `d < 1 \|\| d > 31`, accepting impossible dates | **TRUE** | `crates/wx-server/src/timeparse.rs` (the shortcut sat inside the nested match, ~line 44) |
| startup failures printed a message and STILL exited 0 | **TRUE** | `crates/wx-server/src/main.rs`: every failure path fell through to the end of `main` |
| `run_dir_name` dropped SECONDS when the minute was 0 (`12:00:30` → `2026101912`) | **TRUE** | `crates/wx-ingest/src/pipeline.rs::run_dir_name` keyed `+mm` on MINUTE ALONE |

SURPRISE found while verifying #1: the SAME `da2 > 31` bug existed a SECOND TIME in
`wx_ingest::store::parse_iso_utc` (manifest statistical ranges). Beyond the literal task
text, BOTH parsers were fixed against ONE shared rule so they cannot drift again.

## 1. RETENTION-AWARE SKIP BEFORE DECODE — the exact semantics chosen

Decision function `Pipeline::retention_skip_reason(mdl, path, hash) -> Option<String>`,
called for EVERY ready candidate BEFORE the magic-byte precheck, the rayon decode AND
any write. It returns `Some(reason)` only when BOTH hold:

1. the model's registry already holds **at least `retention_runs` DISTINCT RUNS**
   (a registry with room left can always absorb another run — skipping would be wrong), AND
2. the **NEWEST SECTION-1 REFERENCE TIME declared anywhere in the file** is **STRICTLY
   OLDER** than the oldest run the registry keeps (`max_epoch < oldest_kept`).

Why that is SAFE rather than merely convenient: ingesting such a file could only ADD
runs older than everything kept, and `retention_prune` removes runs oldest-first, so the
new run(s) would be evicted again at the end of the SAME round — observable state is
IDENTICAL minus a full decode.

Chosen details, all deliberate:

* **Source of truth = the file.** The reference time comes from `grib2.peek_ref_times`,
  a framing-only walk that interprets ONLY section-1 payloads (sections 2..7 stepped over
  by their DECLARED byte lengths, so payload pages are never faulted in). Big-endian
  framing, edition byte must be 2, both terminators accepted — same rules as the decoder.
* **Multi-run files judged on the NEWEST** reference time (`frac_small.grib2` carries two
  runs): the conservative end, since one OLD message in a mostly-new product must not make
  the whole file look obsolete.
* **STRICT inequality.** Equality means the file belongs to a run that is ALREADY KEPT —
  the classic partially delivered / re-delivered product (and the `_c3` packing twin used
  in the tests). Skipping it would DROP the rest of a run the user can already query.
* **UNCERTAINTY NEVER SKIPS.** Any structural problem (bad magic, bad edition byte,
  truncated framing, no section 1, unreadable) returns Err and falls through to the NORMAL
  path, where the magic precheck and the poison quarantine apply exactly as in phase B.
* **NO persistent record is written** for a skipped file. The verdict is RE-DERIVED every
  round against the CURRENT registry, so if the newer runs later disappear upstream and
  the registry empties, the stale file becomes ingestible again instead of being stuck
  behind a false "already ingested" claim. A `kind=SKIPPED` record would have been the
  wrong tool: it survives restarts and implies an ingest ATTEMPT that never happened.
* **Note + counter deduped per `(path, content-hash)`** via the bounded memo
  `stale_verdicts` (cap `STALE_VERDICT_CAP = 4096`), mirroring the kind=FAILED quarantine
  rule: a still-stale file costs ONE note and ONE count, and a CONTENT CHANGE re-arms
  exactly one more of each.

New counters/gauge: `IngestCounters.retention_skipped_files`, `LiveCounters.retention_skipped`
(+ `bump_retention_skip()`), Prometheus gauge `wxpoint_retention_skipped_files_total`, read as
`max(legacy store, live mirror)` like every other ingest-side gauge.

## 2. Filename run hint is ORDERING ONLY (never a skip input)

`order_newest_first` sorts each model's ready list NEWEST-HINT-FIRST with a STABLE
`sort_by` on `filename_run_hint(path)`: scans the BASENAME's digit runs of length ≥ 10 for
a `YYYYMMDDHH` stamp and returns the max plausible epoch, or 0 (sorts LAST) when absent.

The hint decides only WHICH candidate is consumed FIRST, so a freshly delivered burst fills
the registry with its newest runs before older candidates are judged — maximizing how many
stale files the §1 skip can veto WITHOUT ever decoding them. It NEVER decides whether a file
is skipped. A wrong, misleading or absent hint therefore costs nothing but a little wasted
decode; because the sort is stable, names without a parseable stamp keep their previous
sorted-path order and every pre-existing fixture behaves exactly as in phase B.

Both wrong directions are tested (`retention_skip_hint.rs`): a name claiming an OLD run whose
CONTENT is new is STILL ingested (and its run survives retention), and a name claiming a
much NEWER run whose CONTENT is old IS skipped — with the note quoting the reference time
read from the file, never the filename stamp.

## 3. data_dir / ingest_dir OVERLAP GUARD (startup validation)

Ingest WRITES into `<data_dir>/<model>/...` and READS from `<ingest_dir>`. If one CONTAINS
the other (or they are the same directory spelled two ways), retention can delete run trees
that may BE the source products, and atomic renames land INSIDE the watched ingest tree where
the glob matches them again (self-ingestion). Rejected at startup naming BOTH paths, their
normalized forms and the relation ("same directory" / "data_dir contains the ingest_dir" /
"ingest_dir contains data_dir").

Normalization: `norm_dir_path` canonicalizes when the path EXISTS (which also resolves symlinks)
and otherwise ABSOLUTIZES lexically against the process CWD collapsing `.`/`..` — routinely
needed because `data_dir` does not exist on first boot. Comparison is COMPONENT-WISE
(`path_parts` + `dir_relation`), never string-prefix, so `/data-x` is NOT "under" `/data`.
Documented caveat: a symlink pointing at an EXISTING directory is resolved, while a symlink
whose target does not exist yet is compared lexically; the guard catches configuration
mistakes, not hostile filesystems.

## 4. STATE HYGIENE — persistent records and stability observations

`StateStore` keeps `recs: Vec<FileRec>` (UNCHANGED serialized order) plus a DERIVED
`idx: HashMap<String, usize>` rebuilt by `reindex()` after load / upsert / drop_under /
prune_missing. `find` is now O(1) instead of a linear scan over every file ever ingested
(`scan_round` calls it once per candidate). **FORMAT COMPATIBILITY: the on-disk
`state/files.json` is BYTE-COMPATIBLE with V1.1/V1.2-B** — same keys, same order, `kind`
still emitted, and legacy files without `kind` still load as `REC_OK` (`#[serde(default)]`).
No migration, no version bump; `FileRec` gained no fields.

`prune_missing(prefix)` forgets records whose SOURCE FILE vanished UNDER that prefix: ONE
stat per matching record, called ONCE per model per scan round, bounded by that model's
record count. Records for files that merely FAILED/IGNORED are KEPT while the file exists,
so pruning never RE-ARMS a quarantined file. Prefix matched on `<dir>/`, so a sibling named
`ing-archive` is untouched by `ing`.

`StabilityTracker` gained NO FIELDS (existing tests construct it as a literal): the `obs`
vector doubles as an LRU — `observe` bumps RECENCY by moving the entry to the BACK,
`trim_obs` enforces `STABILITY_OBS_CAP = 10_000` by evicting from the FRONT, and
`prune_vanished` (called ONCE per round) drops entries whose file is gone. Eviction only
restarts a stability WINDOW, which can never change an answer.

## 5. EVENT QUEUE DRAINED ONCE PER ROUND, NOT ONCE PER MODEL

`scan_round` hoists `drain_queue(self.queue.clone())` ABOVE the per-model loop; every model
filters the SAME drained list through its own glob. Previously the FIRST model consumed
every queued path and later models always saw an EMPTY queue — their events were silently
stolen, leaving them dependent on the next periodic scan. Correctness was never broken (the
directory walk reconciles), which is exactly why no existing test caught it; the new test
discriminates with a product file placed OUTSIDE the model's ingest dir, visible ONLY through
an event. Note that a drain CONSUMES the queue, so the test re-enqueues between rounds —
what a real watcher does during a CREATE+RENAME delivery.

## 6. STRICT CALENDAR DATES, ONE SHARED RULE

New helpers in `wx-core/src/tables.rs`: `DAYS_IN_MONTH`, `is_leap_year` (proleptic Gregorian:
÷4, except centuries unless ÷400), `days_in_month`, `valid_civil_date`. Consumed by BOTH
`wx_server::timeparse::parse_iso_z` (impossible date → `None` → HTTP 400 `bad_time`) and
`wx_ingest::store::parse_iso_utc` (a hand-edited manifest cannot smuggle `2026-02-31` into a
statistical range). The Hinnant day formula CARRIES overflow, so the pre-fix behaviour silently
shifted the query/range by DAYS instead of failing. Feb 29 valid in 2024 and 2000, rejected in
2026 and 2100; Feb 30/31 and Apr/Jun/Sep/Nov 31 always rejected; Dec 31 always fine.

## 7. EXIT CODES ARE MACHINE-OBSERVABLE

| code | meaning |
|---|---|
| 0 | served normally (or stopped cleanly) |
| 1 | config missing / unparsable / invalid (spec §3 loud failure) |
| 2 | usage error: no `--config` and no `WXPOINT_CONFIG` |
| 3 | pipeline init failed (state/grid/data-dir problems) |
| 4 | startup wind-pair preflight rejected the configuration (spec §4.4) |
| 5 | tokio runtime init failed |
| 6 | the configured `bind` address could not be bound |
| 8 | the HTTP server itself returned an error |

Distinct codes rather than "at least 1": an operator can fix a bad config WITHOUT reading
logs, and smoke/CI harnesses can gate on them. `start_pipeline` now returns `i64` and `main`
calls `std::process::exit`. Covered by `crates/wx-server/tests/exit_codes.rs`, which SPAWNS
the built binary (via `CARGO_BIN_EXE_wxpoint`, falling back to the debug output path) using
an `extern "C"` `system` binding and reads the captured rc back from a file.

## 8. `run_dir_name` made TOTAL and INVERTIBLE

Base `YYYYMMDDHH`; `+mm` whenever minute OR second is non-zero; `+ss` whenever the second is
non-zero → canonical lengths 10 / 12 / 14, which is what makes the length rule UNAMBIGUOUS
and lets `parse_run_dir` recover the exact instant. Pre-existing 10-char directories parse
exactly as before (minute/second default 0), and runs differing only in minutes or seconds
can never COLLIDE into one directory.

## 9. Tests added (names)

| file | test |
|---|---|
| `wx-ingest/tests/retention_skip_stale.rs` | `stale_run_arriving_late_is_skipped_without_decode_or_write` (zero renames/bytes, counter bumped ONCE, note counted, NO state record, dedup across rounds, newer run still ingests) |
| `wx-ingest/tests/retention_skip_peekfail.rs` | `unpeekable_header_falls_through_to_the_normal_quarantine_path` |
| `wx-ingest/tests/retention_skip_hint.rs` | `hint_says_old_run_content_is_new_run_so_it_is_still_ingested`, `hint_says_new_run_content_is_old_run_so_it_is_skipped` |
| `wx-ingest/tests/retention_skip_same_run.rs` | `same_run_second_file_is_never_skipped_and_merges_into_the_kept_run` |
| `wx-ingest/tests/state_hygiene.rs` | `find_uses_the_derived_index_and_stays_correct_across_structural_changes`, `prune_missing_forgets_only_records_whose_source_actually_vanished`, `state_file_on_disk_is_byte_compatible_and_survives_a_restart_including_legacy` |
| `wx-ingest/tests/stability_hygiene.rs` | `vanished_paths_are_forgotten_while_surviving_windows_keep_their_state`, `remembered_observations_are_bounded_and_evict_least_recently_seen_first` |
| `wx-ingest/tests/config_overlap.rs` | 5 tests: identical dirs, store-inside-delivery, delivery-inside-store, relative-vs-absolute collision (+`..` walk), sibling name-prefix accepted |
| `wx-ingest/tests/run_dir_roundtrip.rs` | `every_minute_and_second_instant_round_trips_through_its_directory_name` (40×4×4×5 instants, no collisions), `seconds_survive_a_boot_and_the_hour_only_name_stays_back_compatible` |
| `wx-ingest/tests/strict_dates_store.rs` | `february_29_is_accepted_only_in_a_real_leap_year`, `impossible_days_are_rejected_instead_of_rolling_over_into_the_next_month` |
| `wx-ingest/tests/queue_drain_fairness.rs` | `queued_events_reach_every_model_in_the_same_round_not_just_the_first_one` |
| `wx-server/tests/strict_dates.rs` | `the_parser_validates_days_against_the_real_calendar_including_leap_years`, `an_impossible_date_answers_400_bad_time_instead_of_answering_wrong_data` |
| `wx-server/tests/exit_codes.rs` | 4 tests: usage rc=2, missing config rc=1, overlap guard rc=1 (+ message names both paths), unknown param rc=1 |

## 10. Surprises / deviations worth remembering

* The date bug existed TWICE (HTTP parser AND manifest parser); fixing only the audited one
  would have left the same class of silent day-shift reachable.
* The queue-drain bug was LATENCY-ONLY because the periodic walk reconciles, so it had no
  observable data effect — a fairness test needed an out-of-tree product to discriminate at
  all. Re-enqueuing between rounds is REQUIRED in that test because a drain CONSUMES the queue.
* `state.save()` uses plain `std::fs::rename` (NOT the instrumented writer), so a retention-skip
  round shows ZERO storage-counter renames — which is exactly what makes the "nothing at all
  was written" assertion meaningful rather than trivially true.
* Skipped files STAY in the ingest dir and are re-peeked every round (a handful of bytes per
  section). That is intentional: cheap, self-healing, and it keeps the verdict honest if the
  registry later shrinks.
* Dialect friction while writing tests: this toolchain does NOT coerce `String` → `&str` at
  call sites (helpers taking `&str` must be called with `&x` / `.as_str()`), string literals
  need `.to_string()` where a `String` is required, `Type.method()` needs `Type::method`,
  slice temporaries must be borrowed (`let c = &s[a..b]` or compare inline), and mutex guards
  must be scoped so a later `scan_round()` cannot deadlock on the same lock.
* `patch_day` (12-byte sec1 pattern, day byte at offset 10, 39 hits in `gfs_small.grib2`) is
  reused to synthesize extra runs; the hint tests use NAMES carrying explicit `YYYYMMDDHH`
  stamps so the two directions of the lie are unambiguous.

## Gate

`cargo test --workspace` green twice back-to-back (**rc=0, 149 tests passed across 58
test binaries + 8 empty lib targets, 0 failed**, 54.0 s wall — a third consecutive run was
also green), `cargo clippy --workspace -- -D warnings` **rc=0** (gate scope: workspace
lib+bin targets; the only stderr line is cargo's future-incompat note about the vendored
`bitflags` deps). Three new lints had to be satisfied in phase-C source itself
(`manual_range_contains` ×2 in `wx-core::tables`, `comparison_to_empty` ×2 in
`wx-ingest::config`, `needless_return` in `main.rs`) — fixed rather than allowed.

# V1.2 phase D decisions — production-scale ingest benchmark, packing-coverage closure, honesty fixes

## 0. What this phase was FOR

Phases A/B/C optimized the ingest writer against the milestone-5 perf dataset, whose
isobaric stacks are SHORT (L≈8 planes per GROUP). The tester's complaint was about
PRODUCTION scale: a real GFS delivery has ~25-40 isobaric levels per parameter per
forecast hour, so any O(L^2) merge behaviour is invisible in the old benchmark and
dominant in reality. Phase D therefore builds a dataset at REAL geometry, re-measures
BEFORE (tag v1.1 = `82f4998`, i.e. phases A/B0/B/C ABSENT) vs AFTER, closes the two
remaining data-representation-template gaps the audit found unexercised (5.2 and
5.200), and corrects three documentation claims that were not literally true.

## 1. The PRODUCTION-SCALE realistic dataset (`--real-out`, `scripts/gen_realistic.sh`)

* Geometry: regular lat/lon template 3.0, **1440 x 720**, lon 0..359.75, lat 89.875
  down to -89.875, N->S scan — the real GFS 0.25 deg node rectangle.
* Levels: **every 25 hPa from 1000 to 100 = 37 levels**. That is a SUPERSET of the
  real NWS pgrb2 stack (25 levels: 1000,975,950,925,900,850,800,...,100); the 12
  extras (875,825,725,675,625,575,525,475,425,375,325,275) exist ONLY to make the
  per-GROUP merge cost visible. Documented deviation from the brief's "~40 levels"
  (the real list is 25; 37 is closer to "every level a model can carry" and strictly
  harder).
* Params TMP + HGT + UGRD + VGRD, ALL on `isobaric:*` -> **148 messages per file**;
  THREE runs (2026-10-19T00Z/T06Z/T12Z) x TWO forecast hours (f000, f006) as SIX
  SEPARATE files = the "6-file reality" retention must cope with.
* Filenames are AUTHENTIC NWS style (`gfs.t191200z.pgrb2.0p25.f006.grib2`) and
  deliberately carry NO 10-digit `YYYYMMDDHH` stamp, so the phase-C filename run-hint
  CANNOT help and ordering falls back to path order. Measured and documented rather
  than hidden: hint-less names cost nothing here because retention peeks SECTION 1.
* Wind messages carry flag-table-3.3 bit 0x08 (grid-relative components) ON a
  REGULAR lat/lon grid. For regular grids our rotation is the IDENTITY (documented
  decision matching wgrib2: a lat/lon grid's components are already earth-tangent),
  so stored values equal decoded values. This exercises the wind PAIR rule, the
  startup preflight probe and the rotate-to-earth code path end to end; it does NOT
  test rotation ANGLES (those stay covered by the LCC fixtures + pyproj checks).
* Values stay analytic and EXACT in f32: `BASE(param,level) + ((row*1440+col) %
  1024) + fc_hour*10`, so simple packing needs only 10-11 bits. Consequence stated
  honestly: files are ~192 MB each (1.15 GB for six) where a real JPEG-compressed
  NWS product would be ~30 MB — the fixture is BIGGER on disk than production, which
  makes the write-amplification numbers CONSERVATIVE in absolute bytes.
* Truth source: `scripts/validate_realistic.py` drives `tools/wgrib2-full` over all
  six files — 148 records/file, every record DRT 5.0, label order incl. the SECTION-1
  reference time, **592 node probes per file** (4 points x 148 records, chosen to hit
  the seam, both hemispheres and the last row/column) matching the analytic formula,
  and a section-3 flag-byte tally of 74 grid-relative / 74 earth-relative per file.

## 2. DRT 5.2 (complex packing WITHOUT spatial differencing): a real decoder gap, FOUND and FIXED

The audit's packing-coverage claim was incomplete: no fixture ever exercised 5.2, and
when one was added the decoder FAILED with `Truncated`. Root cause: wgrib2 writes a
**47-byte** section 5 for 5.2 but **49-byte** for 5.3 (the two spatial-differencing
octets are absent), and `dec_complex` required `s5.len() >= 44`. Fix in
`crates/wx-ingest/src/grib2.rs`: accept `s5.len() >= 42`, derive
`no_spatial_fields = s5.len() < 44` so the spatial-differencing parameters and
`eo` DEFAULT to 0 when absent, and keep `extra_octets = (pack == 2) ? 0 : sec[5][48]`
for 5.3 exactly as before. 5.3 behaviour is unchanged (its twin tests still pass).

Twin naming: `_c2` is named after the DATA REPRESENTATION TEMPLATE (like the existing
`_c3` = 5.3), not after a wgrib2 switch, because the mapping was MEASURED on this
build: `-set_grib_type complex1` -> **5.2**, while `complex2`, `complex3`, `c2`, `c3`
all -> 5.3. `rle` and `png` are REJECTED by this wgrib2 build (`bad type`).
`packing_twins.rs` now tags `_c2` and asserts element-wise equality plus point-probe
reproduction on it like every other twin (8 tests green).

## 3. DRT 5.200 (run-length): hand-crafted fixture because NO reference encoder exists

`wgrib2 -set_grib_type rle` fails on this build ("*** FATAL ERROR: set_grib_type:
bad type rle ***"), and no other wgrib2 option emits 5.200. So the fixture is
hand-written by `scripts/gen_grib2.py` (`sec5_runlength` / `pack_runs`: 33 runs x 7
points on a 21 x 11 regular grid) and the TRUTH CHAIN is instead:

1. **wgrib2's OWN rle DECODER** reads our file (it implements 5.200 even though its
   writer cannot emit it) — that pins the bit layout against a reference reader;
2. `-checksum data` of the 5.200 file EQUALS that of a SIMPLE-PACKED twin carrying
   the same values (both `3596891385`), pinning the decoded ARRAY;
3. our Rust decoder must reproduce the same array, run positions and NaN handling.

Tests: `runlength_5200_matches_simple_packing_and_wgrib2_checksum`,
`runlength_runs_land_on_the_right_grid_positions`,
`corrupt_runlength_stream_is_a_typed_error_not_garbage` (truncated run stream ->
typed error, never garbage). The absence of a reference ENCODER is a documented
limitation, not a silent gap.

## 4. Honesty fixes (three claims that were not literally true)

* **"fsync requires libc here" was WRONG.** A compile probe OUTSIDE the repo
  (`/tmp/opencode/fsync_probe.rs`, `fsync_probe2.rs`) shows `std::fs::File::sync_all()`
  EXISTS in this toolchain (rustc resolves it into `library/std/src/fs.rs`) and `strace`
  shows a REAL `fsync(3)` — including on a DIRECTORY handle (`File::open(dir)` ->
  `fsync(4) = 0`). The libc `open`/`fsync`/`close` bindings were REPLACED by
  `File::open(path)` + `sync_all()`: identical syscalls, signature, counter placement
  and error taxonomy, one fewer FFI surface. README §4 and the DECISIONS entry now
  record the original claim AS the audit trail and the correction. Lesson kept: an
  "unsupported in this environment" conclusion needs a COMPILE PROBE, not an
  error-message inference.
* **"real files" in the correctness map meant wgrib2-generated fixtures.** README §8
  rows 1-3 now say SYNTHETIC-FIXTURE-WITH-wgrib2-GROUND-TRUTH explicitly, with a note
  that production NOAA bytes are NOT fetched in CI (spec §13) and an optional MANUAL
  `scripts/fetch_real_sample.sh` (range-limited curl from the public aws-nws bucket,
  then replay the same probes) to close the gap by hand.
* **Clippy gate scope stated with numbers, not vibes.** The gate is
  `cargo clippy --workspace -- -D warnings` (**rc=0, zero diagnostics**). `--all-targets`
  is NOT clean: in the phase-D sweep it reports **153 diagnostics across 10 files, ALL
  of them in `crates/wx-core/tests/*` and `crates/wx-core/benches/*`** (dominated by
  `assert(false)`-style `always_false` ×53, `needless_return` ×41, `manual_op` ×12,
  doc-list-indentation, `vec_without_push`, single-pattern `match`, index-loop idioms).
  Six of those had been in `wx-ingest/src/store.rs` doc comments and WERE fixed when
  phase D touched that file; the test-code debt is deliberately left alone (fixing 150
  lint sites in test bodies would churn the very tests that pin correctness) and is
  recorded here instead of being described vaguely as "pre-existing debt".
* Stale instrumentation note removed: the storage counters ARE exported by `/metrics`
  as `wxpoint_storage_renames_total` / `_bytes_written_total` / `_fsyncs_total`.

## 5. Benchmark harness (`scripts/ingestbench.sh` rewritten)

* Measures WALL-TO-FIRST-QUERYABLE (`/ready` 200), WALL-TO-ALL-VISIBLE (every expected
  `model:run-dir:forecast-hour` triple present in `/v1/inventory`, retention-aware:
  the newest `min(runs, retention_runs)` run dirs), PEAK RSS via a `VmHWM` poller every
  100 ms, SERVER CPU time, PER-FILE ingest milliseconds (the pipeline's own progress
  lines), and storage accounting: renames, bytes-written vs FINAL store size
  (amplification ratio), fsyncs.
* AFTER-side counters come from `/metrics`; the v1.1 build has none, so BEFORE-side
  counters come from `strace -f -e trace=rename,renameat,renameat2,fsync,write,sendfile`
  LAUNCH mode, with a SECOND untraced run for wall/RSS so the syscall tracer does not
  contaminate the timing numbers.
* Dataset files are HARD-LINKED into the bench root when possible (192 MB x 6 copies
  otherwise), then `touch`ed so the delivery looks fresh — note this also bumps the
  SOURCE file's mtime, since a hard link shares the inode.
* Correctness rides along: `scripts/_probe_realistic.py` queries the LIVE server over
  the realistic dataset (exact nodes at 4 points x 4 params x 4 levels x 3 runs x 2
  fhs, plus vertical 837.5 mb between 850/825 and time fh3 between f000/f006) and
  compares against the SAME analytic formula wgrib2 confirmed.

## 6. The "DOUBLE INGEST" red herring in the benchmark log — diagnosed, NOT a decode bug

The phase-D benchmark log appeared to show the SAME file ingested TWICE (identical
path, identical byte counts, IDENTICAL millisecond timings), once even three times.
Verdict from the evidence: **NO second decode happened.**

* `/metrics` reported `wxpoint_ingested_files_total = 6` for six distinct paths (a real
  re-decode would have made it 12), `wxpoint_skipped_messages_total = 0`, and the ingest
  log's own `scan_round ingested 6 file(s)` line agrees. The SINGLE-RECORD property is
  pinned directly by the new regression test rather than by post-hoc inspection of that
  run's state file (its scratch dir is deleted): `progress_once_per_file.rs` asserts
  `p.state.recs.len() == 1`, exactly one record whose path equals the delivered file,
  `kind == REC_OK`, and `counters.ingested_files == 1` across FOUR scan rounds.
* A genuine second decode cannot reproduce an identical timing; the duplicates carried
  byte-identical strings INCLUDING the `ms` field, i.e. they were the SAME recorded
  EVENT printed more than once.

Root cause was TWO independent emitters of the same line, plus a visibility-parser bug
that made the bench hang (see §7):

1. per-FILE progress lines were pushed into `Pipeline.log`, which `main` streams once
   per scan round (`printed_logs` loop), AND
2. the server's LIVE printer thread echoed `LiveCounters.last_msg` whenever
   `progress_seq` advanced — so every progress line printed at least twice, sometimes
   three times, while lines that arrived faster than the printer's 0.1 s cadence were
   LOST entirely (the log showed 5 of 6 lines before the round summary).

Fix (minimal, product-side): progress lines no longer share the note log. `Pipeline`
gains `progress_log` (diagnostics only); `LiveCounters` gains a bounded FIFO
`progress_queue` (+ `drain_progress()`); the LIVE printer DRAINS the FIFO and prints
EVERY line EXACTLY ONCE oldest-first; the round streamer therefore only ever prints
NOTES. `last_msg` is kept for diagnostics/back-compat. Regression test
`crates/wx-ingest/tests/progress_once_per_file.rs`: one stable file across FOUR scan
rounds ⇒ `counters.ingested_files == 1`, exactly ONE progress record, ZERO progress
lines in the streamed note log, `progress_seq == 1`, FIFO drains ONCE then stays
empty across further rounds; then a CONTENT CHANGE (same path, DRT 5.2 packing of the
same fields) re-arms EXACTLY ONE ingest + ONE progress line. `restart_midchunk.rs`
was updated to count progress lines in the new channel and asserts they are ABSENT
from the streamed log.

## 7. Benchmark harness bugs found by the hang (script-side, both fixed)

* `scripts/_visible.py` did not know `/v1/inventory` wraps its per-model blocks under
  an `"inventory"` key, so the "all visible" condition could NEVER fire. It now accepts
  all three shapes (`{"inventory": [...]}`, `{"models": [...]}`, bare model/list) so a
  shape change can never masquerade as "data not visible", and supports an optional
  `:min_levels` component so a HALF-PUBLISHED parameter stack does not count as visible
  (the realistic spec demands 37 levels per field).
* `run_one` had NO success exit at all: it polled its whole budget (7200 x 0.1 s) even
  after ingest completed. It now breaks as soon as BOTH milestones are reached, has an
  explicit deadline (`MAXPOLLS`, default 1200 x 0.1 s), reports WHY it stopped
  (`exit=ok|deadline(...)|server-died`), prints what is still PENDING, snapshots
  `/metrics` regardless of how it stopped, and exits rc=3 on a deadline.
* Value probes run against the LIVE server (`scripts/_probe_realistic.py`) inside the
  timed window, so the benchmark measures correctness AND latency together.

## 8. Measured BEFORE/AFTER at production scale (bounded, honest about what is measured vs analytic)

The full-scale pre-V1.2 measurement was ABANDONED on purpose: at L=37 the old writer
needs `(L+1)/2 = 19x` its own output (~70 GB for a 3.68 GB store) and tracing that
under `strace` stalled the tracer — a PATHOLOGICAL measurement, not a product signal.
So the BEFORE side is measured on a REDUCED-BUT-SAME-SHAPE dataset (`--real-levels
16 --real-runs 1`: same 1440x720 geometry, same four params on `isobaric:*`, same
grid-relative wind flags, ONE run x f000+f006, 2 files x 83 MB) and extrapolated
ANALYTICALLY to L=37, labelled as such.

| column | pre-V1.2 (tag `v1.1`) reduced L=16 MEASURED | V1.2 K=4 reduced L=16 MEASURED | V1.2 realistic L=37 MEASURED |
|---|---|---|---|
| first-queryable | 2.49 s | 3.08 s | 5.62 s |
| all-visible | **20.93 s** | **3.69 s** | 15.73 s (K=4) / 25.35 s (K=1) |
| peak RSS | 766 MB | 670 MB | 3079 MB (K=4) / 1087 MB (K=1) |
| server CPU | 18.4 s | 3.8 s | 26.0 s (K=4) / 24.0 s (K=1) |
| renames | n/a (untraced by design) | 11 | 33 |
| bytes written / final store | ANALYTIC **(L+1)/2 = 8.50x** at L=16 (~4.5 GB for a 531 MB store) | 530.8/530.8 = **1.0000x** | 3682.7/3682.7 = **1.0000x** |
| fsyncs | n/a | 30 | 82 |
| per-file ingest | n/a (v1.1 emitted no per-file line in this capture) | 739 ms | 1.35-1.64 s |

Two measured points pin the law: phase B at L=8 gave **3.55x** (78 plane serializations
where 8 suffice, 45 renames per GFS hour) and V1.2 gives **1.0000x** at L=16 AND L=37.
`(L(L+1)/2)/L = (L+1)/2` predicts 4.5 / 8.5 / 19.0 for L=8 / 16 / 37; the L=8 case
measured slightly below prediction (3.55x) because manifest/grid rewrites and small
stacks dilute the ratio - recorded rather than smoothed over.

Honest regression recorded: wall-to-FIRST-QUERYABLE got WORSE (2.49 -> 3.08 s at L=16,
5.62 s at L=37) because publish moved from PER-MESSAGE to PER-FILE and up to K files
are decoded before any is published. The gain is 5.7x on all-visible, 4.8x on CPU,
elimination of half-published level stacks (and their transient 422s), and write
volume down to exactly the final store size.

## 9. V1.2 scope statement (one line per fix, A -> D)

* **A** streaming chunked writer + `TouchedDirs` batched directory fsync + storage
  counters; output bit-identical to the golden file.
* **B0** per-slot vertical level indices (silent wrong answer FIXED), monotonic
  stability clock, automatic storage-format upgrade recovery (no silent empty inventory).
* **B** chunked decode bounded by `decode_batch_files` (K), write-ONCE grouping (O(L^2)
  -> O(L)), per-FILE publish, poison quarantine with content hash, live counter mirror.
* **C** retention-aware skip BEFORE decode, filename run-hint used for ORDERING only,
  data_dir/ingest_dir overlap guard, state hygiene (prune vanished records), drained-once
  event queue, strict calendar dates, machine-observable exit codes, total+invertible
  `run_dir_name`.
* **D** production-scale benchmark harness (realistic + reduced datasets, value probes
  against the live server), packing-coverage closure (DRT 5.2 decoder gap FIXED, DRT
  5.200 hand-crafted fixture because no reference encoder exists), operator-log
  de-duplication (`progress_once_per_file.rs`), three documentation honesty fixes
  (fsync/libc claim, "real files" wording, clippy gate scope) and the stale
  instrumentation note removed.

Out of scope for V1.2 (unchanged): slant paths, bounding boxes, soundings, grid subsetting,
unit conversion, derived quantities, authentication, downloading model data.

## 10. V1.3 phase E0 — statistical-metadata durability (2026-10-22)

**The defect (AUDIT_1.2.md P1).** `stat_processed`, `stat_kind` and the PDT 4.8
begin/end window lived ONLY in `manifest.json`. Spec §6.3 says accumulations,
averages and max/min fields must never be time-interpolated, but the warm-start
rebuild read that guard back from the manifest, so the durable `.bin` files alone
were not enough to enforce it.

**Reproduction.** Ingest `stat_small.grib2`; APCP at a mid-forecast-hour valid
time answers 422 `stat_time_guard` (correct). `rm <data>/stat/2026101912/manifest.json`,
restart: the same query now answers **200 with linear interpolation between
forecast_hours [0, 6]** — a plausible, WRONG number (the sum of two 6 h
accumulations blended 50/50). Same state from an UNPARSABLE manifest, and the
amplifier: a FAILED manifest write counted an error but still recorded the source
file REC_OK, so a crash between the durable `.bin` writes and the manifest left
exactly that state permanently.

**The fix.** The metadata moved INTO each field file's own fixed header
(`wx_core::storage::StatMeta`: kind byte, processed byte, begin/end/step i64s),
`BIN_VERSION` 1→2 (fixed header 24→56 bytes, +32 B per field file; payload
layout `[level][y][x]` f32 LE unchanged). The warm start reads the guard FROM
THAT HEADER; `manifest.json` became human-facing inventory only. The reader is
strict (kind > 5, processed > 1 or non-zero reserved bytes → `HeaderError::BadStat`),
and negative window offsets round-trip.

**Compat strategy — chosen (b), B0-style storage-format recovery.** Two options
were on the table: (a) an UNKNOWN tri-state that fails closed with a new 422
reason when a header lacks stat bytes, or (b) treat a foreign field-file version
as the same "unusable store" condition phase B0 already handles for `grid.bin`.
Chosen: **(b)**. Rationale: (a) needs a new API-visible error reason and a
half-migrated index (some fields guarded, some unknown); (b) reuses machinery
that is already loud, counted, tested end-to-end (`grid_upgrade.rs`) and
self-healing — one generalized pre-index pass (`collect_field_files` +
`bin_version_of_path`, a 12-byte read per candidate) plus ONE distinct
`open_field` error string ("bad bin version") so the log can name found vs
current version. Cost is one re-decode per affected model, exactly as in B0;
no new states leak into `/v1/point`. Version 1 stores therefore UPGRADE
themselves on first boot instead of answering from mis-parsed headers.

**Behavior deltas (deliberate).**
* Warm start with missing/unparsable manifest: LOUD once-per-run note + exactly
  one counted ingest error each; indexing and answers proceed from the headers;
  the manifest is rewritten whenever that run ingests again.
* A failed manifest WRITE now increments the same `failed` counter a failed
  GROUP write does, so the source file is NOT marked ingested and retries next
  round (idempotent via merge dedup + atomic rename). Error accounting stays
  "exactly ONE error per file-with-failure": the inner manifest branch no longer
  double-counts alongside the outer `failed_groups > 0` branch.
* Golden fixtures: `writer_golden_v2.bin` (488 B, real 0–6 h accumulation window)
  is the byte contract; `writer_golden_v1.bin` (456 B) is kept and asserted to be
  REJECTED as an old format, not mis-parsed.

**API-visible changes: none new.** Same status codes and reasons; the change
makes existing ones truthful after manifest loss (inventory/`/ready` reflect the
headers, accumulation queries keep returning 422 `stat_time_guard`).

**Gates added:** `warm_boot_without_manifest_keeps_the_accumulation_guard`,
`corrupt_manifest_boot_is_loud_counted_and_still_answers_right`
(`stat_durability.rs`); `manifest_write_failure_blocks_state_record_and_retries_next_round`
(`manifest_fail_retry.rs`); `v1_field_bin_triggers_loud_recovery_and_the_run_rebuilds_from_source`
(`field_upgrade.rs`); `header_stat_metadata_round_trips_and_guards`,
`corrupt_stat_bytes_and_foreign_versions_are_refused` (`storage_roundtrip.rs`);
`v1_golden_is_rejected_as_an_old_format_not_misparsed` (`writer_golden.rs`).
Every one FAILS against pre-fix behavior by construction (pre-fix: 200 + linear
interp after manifest loss; no notes/errors; REC_OK stuck on manifest failure;
no field-version pre-check at all; no stat bytes to round-trip).

## 11. V1.3 phase E1 — per-forecast-hour stat guards + loud stat-collision rejection (2026-10-22)

**The defects (AUDIT-confirmed, verified by the orchestrator).**
(1) `find_group` keyed store groups on `(run epoch, param, level type, fh)` ONLY.
Two PDT 4.8 messages for the SAME param/level/fh with DIFFERENT statistical
kinds or windows (a plausible delivery: a 0–6 h AND a 0–3 h accumulation for one
valid time) merged into ONE group: `merge_field` silently kept the FIRST plane
and the survivor's window was applied to data that did not come from it. No
note, no counter, nothing observable.
(2) The §6.3 time guard was decided from ONE FIELD-LEVEL flag
(`FieldDef.stat_processed`), which came from whichever forecast hour REGISTERED
FIRST. A stack mixing an instantaneous fh with an accumulated one therefore
answered differently depending on message order: instantaneous-first let mid-time
queries SILENTLY LINEAR-BLEND across the accumulation (wrong number), while
accumulation-first wrongly refused brackets between two CLEAN hours. E0 had made
each `.bin` header carry its own stat state; the index/guard simply did not use
it per-fh yet.

**Guard semantics chosen (fail closed, exactness preserved).** The decision now
uses ONLY the slots the answer would touch, read from `IdxFh.stat_processed` +
`IdxFh.stat_kind_code` (per-fh, fed from each file's own header on warm start and
from its group's messages on cold ingest — identical bytes, identical truth):
* EXACT fh hit → answers from that slot's OWN state, guarded or not (§6.3 asks
  for exactness, not absence of processing).
* LINEAR interpolation → BOTH bracket endpoints must be instantaneous; if either
  guards, 422 `stat_time_guard` naming the refused bracket, the offending hour
  and its kind (`SelectError::StatTimeGuard(fh_a_secs, fh_b_secs, offender_secs,
  kind_code)` → richer HTTP message; reason token unchanged).
* NO bracket at all while ANY stored slot guards → fail closed with the same
  guard naming the FIRST guarded hour, instead of silently falling through to
  older runs. This preserves every uniform-stack behavior pinned before E1
  (incl. the single-slot accumulation in `frac_small`) and extends it
  conservatively to mixed stacks.
* `time_interp=none` outranks the guard (422 `time_exact_only` as before).
`resolve_field_times` became STRUCTURAL-only (exact → none → single → bracket);
the guard layer lives in `select_run` + `stat_guard_details` (cold error path
only — the hot success path pays two scalar flag reads, zero allocations).
Field-level `RegField.stat_processed/stat_kind` (inventory display only) is now
RECOMPUTED deterministically from the per-fh truth: true iff ANY fh guards,
kind = the first guarding fh's kind (fhs sorted ascending ⇒ warm start and cold
ingest converge regardless of registration order — killing the display half of
defect (2)). The registry REPLACE path also updates ALL per-fh stat fields now
(previously refreshed only the mmap handle, leaving stale metadata behind).

**Collision policy (orchestrator decision, spec silent).** The group key gained
the full statistical identity `(stat_processed, stat_kind, begin_secs, end_secs,
step_secs)`, so stat-distinct messages are DISTINCT groups — but storage layout
is one file per `(param, level_type, fh)` and the query API has no range
selector, so both cannot be stored without a filename-scheme change, which is
explicitly OUT for this release. Instead the write phase CLAIMS each target path
in MESSAGE ORDER (claim recorded BEFORE the write so ownership never depends on
I/O outcomes): the first group wins; every later distinct group targeting an
already-claimed path within the SAME source file is skipped-and-noted ONCE per
occurrence (naming both windows) and counted in `IngestCounters.stat_collisions`
+ `LiveCounters.stat_collisions` + Prometheus `wxpoint_stat_collisions_total` —
never an error, never counted among `groups_written`, so the file still records
ingested. Same-key-same-window duplicates keep merging as before. Cross-round
re-delivery of a DIFFERENT window for an existing fh still follows E0 merge
semantics (newest decode rewrites the header); only WITHIN-file collisions are
detected this release. Range selection (API parameter or window-carrying
filenames) is documented future work.

**API-visible changes.** No new status codes or reason tokens. The
`stat_time_guard` MESSAGE is now specific (bracket + offending hour + kind;
sentinel payload falls back to the old static text). New gauge
`wxpoint_stat_collisions_total`. `/v1/inventory` JSON shape unchanged (field-level
flags stay, now deterministic; per-fh ranges were already correct).

**Gates added:** `mixed_stack_instantaneous_first_guards_per_forecast_hour` and
`mixed_stack_accumulation_first_answers_clean_brackets` (`mixed_stat_stack.rs`;
fixtures `mixstat_small.grib2` / `mixstat_rev_small.grib2`, values analytic and
linear in fc hour) — forward order pins the guard payload where pre-fix blended
253.0 silently, reverse order pins the clean-bracket answer 265.0 where pre-fix
over-blocked, and both pin identical per-fh flags + field-level display
regardless of message order (order independence);
`two_accumulation_windows_one_valid_time_first_wins_loudly` (`stat_collision.rs`;
fixture `collide_small.grib2`) — winner's window + plane proven from RAW header
bytes and a value probe (loser's plane would read 510 vs 10), note exactly once,
counter == 1 (live mirror too), other params unaffected, rejected window absent
from the manifest. `point_http.rs` additionally pins the rendered message naming
the bracket/hour/kind and the presence of the new gauge. Discrimination was
verified EMPIRICALLY: with the guard reverted to the field-level flag and the
group key reverted to base-only (API surface kept, so the tests compile), all
three new tests FAIL — forward order accepts the silent blend, reverse order
refuses the clean bracket, and the collision counter/note do not exist.

## 12. V1.3 phase E2 — per-result run provenance + source_winds_grid rename + raw-param units override (2026-10-22)

**The defects (AUDIT-confirmed, verified by the orchestrator).**
(1) `answer_all` echoed ONE top-level `"run"` taken from `entries_at(&eb, 0)
.run_epoch` while every param resolves its run INDEPENDENTLY in
`answer_param`/`lookup_entries`. With two indexed runs where the newest covers
only one requested param, the response echoed the NEWER run even though another
result came from an older one — provenance lost or silently wrong, and the
per-result objects carried no run at all.
(2) `/v1/models` grid blocks reported `"winds_grid"` straight from `grid.bin`
(written once from the FIRST group's source message) while stored fields are
ALWAYS earth-relative post-rotation (field headers store `false`). The value
was nondeterministic w.r.t. message order and contradicted the stored
representation (reproduced on an HRRR LCC fixture: `true` vs `false` depending
on which param decoded first).
(3) Raw discipline/category/number params OUTSIDE the built-in table returned
`units: ""` and config offered NO override, so spec §7 ("return values in
native GRIB units ... and report the units") could not be satisfied for them.

**Provenance shape chosen (additive, never a lie).** EVERY result object
carries its own `"run"` (ISO 8601 Z, same format as the top level), APPENDED
LAST inside the object so existing prefix/substring consumers keep matching;
masked results carry it too (a masked answer still has a run). The TOP-LEVEL
`"run"` is echoed when ALL results share one run — byte-identical to pre-E2
responses for every single-run case, which is the overwhelming majority — and
emitted as JSON `null` when they differ. Chosen over "always echo entry 0"
(wrong half the time) and over "always null" (needless breakage): the echo is
now a PROVEN fact about the whole response or absent. `render_point_json`
dropped its `run_epoch` parameter entirely and derives the value from the
entry buffer itself (one scalar scan, zero allocations; only `httpapi` calls
it, so no other call sites moved).

**Rename scope (metadata truth, storage untouched).** Only the `/v1/models`
JSON KEY changed: `winds_grid` → `source_winds_grid`, both grid kinds. The
VALUE semantics stay exactly as written into `grid.bin` — how the SOURCE
messages arriving in `ingest_dir` encoded their components (flag table 3.3
bit 3) — informational, possibly varying between runs of one model. Field
headers, `grid.bin` layout and the internal `GridDef.winds_grid` field name
are UNTOUCHED (storage version unchanged; old stores keep working). Stored
and queried U/V are always earth-relative (§4.4), now stated in README §3.

**Units validation decision: HARD ERROR, not warn+override.** The optional
`units = "..."` key on `[[models.params]]` is legal ONLY on a raw triple the
built-in table does NOT know. Setting it on a named param, or on a raw triple
that resolves to a table param (e.g. 0/0/0 = TMP), fails startup naming the
param and its canonical units — silently replacing known canonical units
invites wrong-unit answers downstream, and config errors should be loud at
boot per spec §3 ("fail loudly"). A raw param WITHOUT units still BOOTS (the
operator may genuinely not know them) but logs exactly ONE note per affected
entry at `Pipeline::open`, BEFORE any ingest round, naming the model, key,
triple and the exact fix. `ResolvedParam.raw` (new) marks out-of-table triples
purely to drive that note; it never touches the query path. Units flow through
the existing chain (`ResolvedParam.units` → group → `FieldDef.units` →
manifest / `/v1/models` / point responses) with zero further plumbing.

**Fixtures + generator gotcha.** `prov_new_small.grib2`: NEWER run
2026-10-19T18Z carrying ONLY `TMP@isobaric-1000` fh 0 (base 300 → node
(10 N, 350 E) reads 300), paired with `gfs_small.grib2` (12Z run) so ONE
query at valid 18Z mixes runs: TMP 300 from 18Z, UGRD 1310 (fh 6) from 12Z.
`raw_small.grib2`: one message discipline 0 / category 19 / number 5
(out-of-table → key `d0c19n5`), surface, fh 0, node value 42. Generator bug
found while pinning them: `gen_grib2.py` REBINDS the variable `grid` to the
HRRR LCC 61x41 template inside the wind loop (~line 590), so the new block
initially wrote LCC section-3 bytes under 861-value planes — the decoder
over-read bit 8616 of a 1077-byte section 7 (`index out of bounds: len 1077
index 1077`) and panicked. Fixed by building a dedicated `egrid`
(`sec3_latlon(41, 21, ...)`); all pre-existing fixtures regenerate
byte-identical. These are also the FIRST simple-packing fixtures with
`nbits = 10` (span 860), exercising a width no earlier small fixture reached.

**API-visible changes.** `/v1/point`: additive per-result `"run"`; top-level
`"run"` may be `null` (mixed provenance) — documented in README §3/§9.
`/v1/models`: BREAKING key rename `winds_grid` → `source_winds_grid` (audit
judged the old key's implication worse than the rename cost; smoke.sh never
grepped it). Config: new optional `units` key (hard-error rules above). No
new status codes or reason tokens anywhere.

**Gates added:** `provenance_http.rs` (4 tests) — mixed provenance pins both
values (300 / 1310), each run string EXACTLY once on its own result, top-level
`"run":null`, and the old lie gone (no `"run":"<new>","valid"` prefix); common
run pins the unchanged echo + per-result copies (occurrence count 3) with
values 280 / 1280; single-param back-compat pins count 2; models endpoint pins
the new key AND absence of `"winds_grid":` (leading-quote pattern cannot match
`source_winds_grid`). Discrimination verified EMPIRICALLY: with only
`queryapi.rs` + `httpapi.rs` stashed to pre-E2, ALL FOUR FAIL.
`raw_units.rs` (3 tests) — with-override answers 42 with `kg/m^2` through
FieldDef + manifest and stays SILENT; without-override boots with exactly one
note naming `d0c19n5` before any ingest ran, answers 42 with `""`, and does
not re-note per scan round; the validation matrix pins all five accept/reject
cells (named+units, in-table-triple+units, out-of-table+units, out-of-table
bare, named bare + `raw` flag). Pre-E2 these fail semantically (units were
always `""`, no note mechanism existed; the matrix additionally references the
new `raw` field, so it would not compile).

## 13. V1.3 phase E3a — retryable-I/O split + batched state persistence + pread hashing + state-write counters (2026-10-22)

**The defects (AUDIT-confirmed, verified by the orchestrator).**
(1) `decode_file` returns `GribErr::Io` for `File::open`/`Mmap::map` failures,
and the scan_round Err branch recorded `REC_FAILED` keyed by content hash for
ANY error INCLUDING Io — one transient EMFILE/ENOMEM/EIO/NFS-ESTALE/permission
hiccup quarantined a perfectly good file until its CONTENT changed, although
nothing about the content had ever been observed. Inconsistent with
`head_is_grib`, which treats I/O failure as retryable. Worse, the branch
comment claimed "NO record is written" while the code fell through from the
magic-precheck Err arm into the IGNORED branch anyway.
(2) `state.save()` cloned + serialized EVERY record and fsynced after EVERY
ingested/ignored/failed file — O(N²) bytes written and fsynced per round at
thousands of files for data that changes linearly.
(3) `content_hash_file` and `head_is_grib` MMAPped every candidate —
`content_hash_file` runs BEFORE the stability verdict exists, so a producer
truncating in place could SIGBUS the ingest thread (a signal no Rust error
handling catches); `read_bytes_maybe` copied mmap bytes one at a time.
(4) The 1.0000x write-amplification headline comes from wx-core storage
counters that deliberately exclude the state writer, so state traffic was
invisible to every benchmark gauge.

**Io-vs-content classification rule.** Every OS-level open/mmap failure maps to
`GribErr::Io` (already true at HEAD: all `File::open`/`Mmap::map` error arms
return Io — PermissionDenied included, pinned directly by
`permission_denied_decodes_as_io_not_content`). The scan_round Err branch now
matches `Err(GribErr::Io)` FIRST: Io feeds the streak policy below and writes
NO record; every other variant keeps the pre-E3a content quarantine exactly as
before (record with real hash, one error per new/changed hash). The same
streak policy guards the two earlier read stages, because under a chmod/EACCES
outage the candidate dies at the content-hash open LONG before decode ever
runs: a fix scoped only to the decode branch would leave the audit's own
deterministic injection (chmod 000) un-reached. Stage names ("content hash",
"magic precheck", "decode") appear in the notes.

**Escalation policy final form.** Bounded IN-MEMORY table
(`io_streaks`, cap 512, evict-oldest) keyed by (path,size,mtime_ns,hash):
attempt 1 → exactly ONE note; attempts 2..4 → silent progress lines only;
attempt == `IO_FAIL_ESCALATION_ROUNDS` (5) → LOUD note + `errors` += 1 +
`quarantined_files` += 1 + live-mirror add + ONE kind=FAILED record, flagged
escalated so later rounds stay silent. Success, definitive IGNORED, or a
content verdict clears the path's entries; an identity change simply starts a
fresh key. Restart clears the whole table (in-memory by design: a fresh process
deserves fresh retries). The record's hash is whatever was known at failure:
EMPTY at the pre-hash stages, where nothing about the bytes was ever observed —
an empty stored hash can never equal a real digest, so the unchanged-skip check
lets the file back into the normal path automatically once reading works
again (SELF-CLEAR; the audit's "chmod restore => ingests" holds WITHOUT any
content touch). A decode-stage failure stores the REAL hash and keeps the
pre-E3a "until content changes" hammering stop — the right trade once the
bytes were readable and the I/O outage is proven persistent.

**Batched-save exact rule.** Mutations (`upsert`, `drop_under`, `prune_missing`)
set `StateStore.dirty`; `save()` is a cheap no-op while clean (no clone, no
serialize, no fsync) and clears dirty ONLY on success. The pipeline persists
via `save_state()` at: (a) the END OF EACH DECODE CHUNK (covers OK/IGNORED/
FAILED records of every file consumed in the chunk), (b) end of scan round —
which also serves as the required pre-retention-prune persist (round-end
immediately precedes `retention_prune`, and prune itself touches no records),
and (c) the two boot storage-format-recovery points. Per-file saves are gone.
A FAILED save logs loudly, counts ONE error per failed attempt, and leaves the
store dirty: the next opportunity retries the same records; ingestion is never
blocked. Documented trade-off: worst case a crash between two opportunities
loses at most one unfinished chunk's records (≤ K files), which re-ingest
idempotently via merge dedup + atomic renames. No new config keys — chunk size
already comes from `decode_batch_files`.
Tests adjusted for the honest granularity: `restart_midchunk.rs` keeps its
exact assertions (its manual prefix-truncation simulates a crash inside chunk
2; observable outcome identical) but its header/comments now describe CHUNK
granularity; `ingest_grouping.rs`'s fsync equation counted 2 fsyncs per state
SAVE and assumed 3 saves in the measured window (per-file + 2 round ends) —
with batching exactly ONE save occurs (observation round never dirties; the
round-end offer is a clean no-op after the chunk flushed), so `state_saves`
drops 3 → 1 with the reasoning inline. Nothing else reads files.json mid-round
(`disk_full_retry` checks the in-memory store; every other files.json probe
happens after a completed round, which the round-end save covers).

**Hash-format preservation proof.** The digest core was split into
`fnv_feed`/`fnv_finish` with the arithmetic untouched, and the new window
reader chains feed(head)+feed(tail)+finish exactly like the old whole-buffer
walk. `tests/hash_compat.rs` pins THREE ways: (1) `content_hash_file` ==
`content_hash_bytes(whole file)` at sizes 0,1,1000,65535,65536,65537,131071,
131072,131073,300000 (every window boundary); (2) literal digests computed by
an INDEPENDENT Python port of the pre-E3a algorithm (LCG + ramp corpora) —
old `files.json` hashes keep matching byte-for-byte after upgrade; (3) shrink
races: truncate-in-place between stat and read yields Ok(new digest) or Err,
never a panic, and the previously-Erroneous EMPTY file now hashes to the
pinned empty-buffer digest (benign documented improvement — the old mmap of a
zero-length mapping failed). `head_is_grib` short/empty/missing edges pinned
graceful. decode_file's post-stability mmap STAYS (page-cache sharing with
queries; residual SIGBUS risk accepted under the stability window, §9).

**Counters.** New dedicated relaxed statics in `state.rs` exposed as
`state_counters_snapshot() -> (writes, bytes)` and rendered on /metrics as
`wxpoint_state_writes_total` /
`wxpoint_state_bytes_written` (incremented only on SUCCESSFUL atomic saves,
bytes = serialized JSON length). The wx-core field-path counters are untouched,
so the 1.0000x headline stays valid WITH an explicit scope note (README §7.4);
`scripts/ingestbench.sh` prints the state line alongside it. Measured gate
numbers: 4 files at K=2 → exactly 2 state writes per ingest round, 0 for
observation/idle rounds (pre-E3a: 5 full rewrites).

**Gates added:** `hash_compat.rs` (3 tests: python-golden equivalence at all
boundaries; shrinking/empty tolerance; head_is_grib edges), `io_retry.rs` (2
tests: PermissionDenied ⇒ GribErr::Io directly; chmod-000 end-to-end — NO
record for the first four failed rounds with exactly one retry note and zero
counters, escalation at the fifth with kind=FAILED + empty hash persisted by
the batched round-end save, restore WITHOUT content change re-ingests and
clears the streak), `state_batching.rs` (1 test: exact save counts per chunk
via the new counters, zero writes on clean rounds, unwritable state dir →
dirty stays true + one note/error per failed ATTEMPT + ingestion proceeds +
next round flushes all five records). Discrimination: E3a's new APIs
(`dirty`, `state_counters_snapshot`, `io_streaks`) do not exist pre-E3a, so
`io_retry` and `state_batching` cannot even compile against the old tree —
semantically, pre-E3a the chmod file would have been silently skipped forever
(silent hash-stage `continue`) or quarantined on first Io at decode, and saves
were 5 where the gate asserts 2. `hash_compat`'s golden/equivalence cases pass
before AND after BY DESIGN (that IS the compatibility proof); its shrink/empty
case fails pre-E3a (mmap of an empty file Errs).
