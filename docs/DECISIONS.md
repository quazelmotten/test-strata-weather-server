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

## Durability: real fsync requires libc here

This toolchain's `std::fs::File` exposes only `flush()`; `strace` confirms it emits
NO `fsync` syscall, and there is no `fd()`/`FileDescriptor`/`flush_sync` accessor
to reach the descriptor. So `wx_core::storage::fsync_path()` binds libc directly:

```rust
extern "C" {
    #[link_name = "open"]  fn libc_open(path: *const i8, flags: i32, ...) -> i32;
    #[link_name = "fsync"] fn libc_fsync(fd: i32) -> i32;
    #[link_name = "close"] fn libc_close(fd: i32) -> i32;
}
```
(`extern "C"` block + explicit `#[link_name]` + `*const i8` avoids the suspicious-
runtime-symbol warning this toolchain emits for an implicit `open`, and keeps the
declaration from colliding with the runtime's own symbol.) Order per file:
write → flush → fsync(inode) → rename → **fsync(parent dir)**, so a crash cannot
resurrect a renamed-but-unlinked name. Errors are surfaced, not swallowed. Used by
`write_bin_atomic`, the state-file writer, and the manifest/grid writer. The earlier
README claim "(+dir fsync)" was aspirational; it is now literally true.

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
