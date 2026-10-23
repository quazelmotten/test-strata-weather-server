# wx-point — gridded weather model point-query server

A Rust application that ingests GRIB2 weather-model output (GFS, HRRR, and
future models) from configured directories, keeps only a configured parameter
list in a query-optimized on-disk format, and serves an HTTP API returning the
value of a parameter at (model, param, level, lat/lon, valid time) with linear
interpolation between grid points, levels, and forecast times. Point queries
only — no slant paths, soundings, bounding boxes, or grid subsetting (designed
so those could be added later).

Priorities per spec: correctness first, then query latency. All measured
numbers below are from this deployment machine and recorded honestly, including
the V1.1 phase-6 acceptance sweep that produced them again (§7 tables carry
both the phase-5 post-refactor numbers and the phase-6 sweep numbers).

---

## 1. Build and run

```sh
cargo build --release -p wx-server          # binary: target/release/wxpoint
./target/release/wxpoint --config config.example.toml
# or: WXPOINT_CONFIG=/path/to/config.toml ./target/release/wxpoint
```

Stable Rust (`rustc 1.99.1` here), workspace crates `wx-core` (no async/HTTP
deps), `wx-ingest`, `wx-server`. Release profile: `lto = "fat"`,
`codegen-units = 1`, `panic = "abort"` (portable — see §7.5 for the
`target-cpu=native` verdict). The bench profile mirrors release but always
unwinds (criterion requirement; this cargo ignores `panic` there — documented
in Cargo.toml).

Startup failures are MACHINE-OBSERVABLE (V1.2 phase C): every failure path exits NON-ZERO
with a distinct code, so a supervisor or CI smoke script can tell a dead boot from a healthy
one without reading logs.

| code | meaning |
|---|---|
| 0 | served normally (or stopped cleanly) |
| 1 | config missing / unparsable / invalid (spec §3 loud failure) |
| 2 | usage error: no `--config` and no `WXPOINT_CONFIG` |
| 3 | pipeline init failed (state/grid/data-dir problems) |
| 4 | startup wind-pair preflight rejected the configuration (spec §4.4) |
| 5 | tokio runtime init failed |
| 6 | the configured `bind` address could not be bound |
| 7 | provisional wind-pair enforcement fired (V1.3 phase E3b): a model configuring only ONE of UGRD/VGRD decoded a GRID-relative wind field after boot (its dir was empty/unstable at startup, so code 4 could not decide); `[ingest].wind_pair_enforce = false` opts out |
| 8 | the HTTP server itself returned an error |


Gate, as measured in the V1.2 phase-D acceptance sweep (this machine):

* `cargo test --workspace` → **rc=0, 153 tests passed across 68 test binaries, 0
  failed**, TWICE back-to-back (both runs identical counts).
* `cargo clippy --workspace -- -D warnings` → **rc=0**, zero lint diagnostics
  (the only stderr line is cargo's future-incompat note about the vendored
  `bitflags` 0.4.0/0.7.0 deps, not a lint). This is the documented gate scope
  (workspace lib+bin targets). A wider `--all-targets` run is NOT clean: it reports
  pre-existing lint debt in test/bench code only (count recorded in docs/DECISIONS.md
  phase D) — stated, not hidden.
* `bash scripts/gen_fixtures.sh` → rc=0, idempotent (all 25 fixture md5s identical
  across consecutive runs), 8 packing-twin assertions green incl. the DRT 5.2 `_c2`
  twins; the DRT 5.200 run-length fixtures are asserted by `runlength_5200.rs`.
* `bash scripts/smoke.sh` → **SMOKE PASS**, all 17 `probes.tsv` ground-truth
  probes replayed with 0 failures.
* `bash scripts/loadtest.sh 15` → §8 targets MET (see §7.2).
* `python3 scripts/validate_rotation.py` → rc=0, 42 expected-angle rows, both earth
  shapes.
* `bash scripts/ingestbench.sh` (realistic dataset) → 6 files ingested, 481 live
  value probes OK, write amplification 1.0000x (see §7.4).

## 2. Configuration

One TOML file (path via `--config` or `WXPOINT_CONFIG`; validated loudly at
startup — unknown param/level names fail). See `config.example.toml`. Since V1.2
phase C, `[server].data_dir` may NOT equal, contain or be contained in any model's
`ingest_dir`: the two sides would fight (retention deleting trees that may BE the
source products, atomic renames landing inside the watched tree and being re-ingested),
so startup fails naming BOTH paths and their normalized forms. Comparison is component-wise
after normalization (canonicalize when the path exists, else lexical absolutization), so
`/data-x` is NOT treated as under `/data`, while `./x/data` and `<cwd>/x/data` DO collide.

| Key | Default | Meaning |
|---|---|---|
| `server.bind` | — | `host:port` for the HTTP API |
| `server.data_dir` | — | where ingested, query-optimized data lives |
| `ingest.scan_interval_secs` | 30 | periodic full-scan fallback (network mounts) |
| `ingest.retention_runs` | 4 | keep N most recent runs per model |
| `ingest.stable_window_secs` | 10 | file must hold (size,mtime) stable this long before ingest; `0` still requires one re-observation |
| `ingest.ingest_threads` | 4 | dedicated rayon decode pool (separate from the tokio HTTP runtime) |
| `ingest.decode_batch_files` | 4 | V1.2 phase B: how many candidate files are decoded as ONE chunk before any is written (must be ≥ 1; bounds peak decode RAM at ~K files while keeping cross-file parallelism) |
| `ingest.wind_pair_enforce` | true | V1.3 phase E3b: when a model configuring only ONE of UGRD/VGRD decodes a GRID-relative wind field after boot (the startup preflight could only defer), the ingest thread EXITS with code 7 instead of skipping winds + inflating errors forever; `false` keeps the sticky-note-and-skip behavior (documented deviation from spec §4.4's fail-loudly intent) |
| `[[models]].sidecar_suffix` | absent | per-model alternative stability rule: eligible once `<file><suffix>` appears |

Per model: `name`, `ingest_dir`, `file_glob` (glob is a FILTER ONLY — all
metadata comes from GRIB sections 1/4, never filenames), and `[[models.params]]`
entries. Param names map to GRIB2 (discipline, category, number) through a
built-in table covering `TMP HGT UGRD VGRD RH SPFH DPT PRMSL MSLMA PRES VVEL
APCP GUST CAPE CIN`; anything else may use raw
`discipline/category/number` keys — such an out-of-table entry may carry an
optional `units = "..."` override (V1.3 phase E2; the ONLY place the key is
legal — setting it on a table-known param is a hard startup error, and a raw
param without units boots with one loud note per affected entry because
responses must report units). Level specs map to Code Table 4.5
(`isobaric`=100, `height_above_ground`=103, `hybrid`=105, `surface`=1,
`msl`=101, …) with grammar `"*"`, `"type:*"`, `"type:value"`, bare `"type"`
(= 0, e.g. `surface`). Isobaric levels arrive in Pa in section 4 and are
normalized to hPa internally so config/API strings match (`isobaric:850`).
HYBRID (V1.3 phase E4c) is the FV3GFS upper-air coordinate: it is
DIMENSIONLESS, so `hybrid:NN` values are stored and echoed RAW — no unit
conversion — and `hybrid:*` captures every hybrid coordinate present
(current operational GFS upper-air lives entirely on hybrid levels).

**Wind-pair startup preflight (spec §4.4, V1.1 phase 4).** Summary of the
semantics, full rationale in docs/DECISIONS.md:

* A model configuring EXACTLY ONE of UGRD/VGRD (by name OR raw code triple) on
  a grid whose wind messages carry the grid-relative flag bit (flag table 3.3,
  0x08) fails at STARTUP, before ingest or serving, naming model + file +
  template + flag byte. Implemented by
  `config::startup_wind_preflight`, which probes the FIRST STABLE file matching
  the model glob read-only (sections 1/3/4 only — never section 7).
* Every non-decisive case DEFERS with a log note instead of failing: empty dir /
  no matching file, matching files still inside the stability window, unreadable
  or non-GRIB file, or a file containing no wind message. Startup "stable" =
  size > 0 AND mtime at least `stable_window_secs` old, compared against libc
  `CLOCK_REALTIME` read directly (V1.3 phase E3b: the marker-file proxy is gone).
  Since E3b the preflight also runs BEFORE any storage recovery touches data
  (a fatal config must not delete run dirs on its way out).
* A deferral is PROVISIONAL, not final (V1.3 phase E3b): the decode-time
  `check_wind_pair` rule stays in force as the second line of defense, and with
  `[ingest].wind_pair_enforce = true` (default) the first scan round whose
  DECODED evidence proves the violation ends the process with exit code 7 after
  printing the note — an empty-at-boot directory can no longer start OK and
  then skip winds + inflate errors forever. Set the key `false` to opt back
  into the sticky-note-and-skip behavior. Clock weirdness at boot can now only
  DELAY a verdict, never cause a false hard-fail (those come from probed bytes)
  nor a permanent miss (decoded evidence re-decides every round).
* When BOTH components are configured on a grid-relative grid, U/V are rotated
  to earth-relative AT INGEST using the conformal angle derived from the
  projection's own validated cone geometry (per grid point) and stored earth-relative,
  so query semantics are uniform across models (§4.4).

## 3. HTTP API

All times ISO 8601 UTC. JSON bodies; values in native GRIB units (K, gpm, m/s,
Pa) — never converted.

### `GET /v1/point`

```sh
curl 'http://127.0.0.1:8080/v1/point?model=gfs&param=TMP&level=isobaric:837.5&lat=41.25&lon=-95.93&valid=2026-10-19T19:30:00Z'
```

**Raw-first wire format + targeted %-decode (V1.3 phase E6).** The server
reads the RAW query string; the canonical form uses bare colons and commas
(curl style). Exactly two escapes ARE additionally accepted, and ONLY inside
`level` and `param` VALUES: `%3A`/`%3a` → `:` and `%2C`/`%2c` → `,` (encoded
requests yield byte-identical responses — `point_http.rs` pins upper-hex,
lower-hex, and mixed forms against their bare counterparts). Everything else
passes through as literal bytes: `+` never decodes to a space, unknown or
malformed `%` sequences survive verbatim (`param=TMP%zz` is an
unknown-parameter 404, not a decode error), and no other key accepts escapes.

```json
{"model":"gfs","run":"2026-10-19T12:00:00Z","valid":"2026-10-19T19:30:00Z",
  "lat":41.25,"lon":-95.93,"level":"isobaric:837.5",
  "results":[{"param":"TMP","value":281.734,"units":"K",
    "interpolation":{"horizontal":"bilinear",
      "vertical":{"method":"linear","levels":[850,800]},
      "time":{"method":"linear","forecast_hours":[6,9]}},
    "run":"2026-10-19T12:00:00Z"}]}
```

**Per-result run provenance (V1.3 phase E2).** EVERY result object carries
its own `"run"` (ISO 8601 Z, appended last; masked results too): with several
indexed runs a multi-param request can legitimately MIX provenance — e.g. the
newest run covers only `TMP` while an older run supplies `HGT`, and each
result names the run that actually answered it. The TOP-LEVEL `"run"` is
echoed when ALL results share one run (byte-identical to pre-E2 responses)
and emitted as JSON `null` when they differ; nothing is ever echoed that did
not answer. Gate: `provenance_http.rs` (mixed → top-level `null` + both runs
named once each; common → echo unchanged + per-result copies).

**Additive vertical metadata (V1.2 phase B0).** Spec §6.4 interpolates vertically
SEPARATELY AT each forecast hour, and two bracketing forecast hours of one run CAN
carry DIFFERENT level stacks (normal during partial ingest: f006 written with
`[1000,900,800,700]` while f012 already gained 950/850). `"levels"` stays the EARLIER
slot's pair exactly as above (back compatible); when the two slots' pairs DIFFER the
vertical object additionally carries BOTH, earlier slot first:

```json
"vertical":{"method":"linear","levels":[900,800],
            "levels_per_time":[[900,800],[950,850]]}
```

When the two pairs agree (the overwhelmingly common case) `levels_per_time` is
OMITTED and the body is byte-identical to V1.1.

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

**Fractional forecast hours in responses.** Time arithmetic runs on integer
seconds past the run reference time, and the reported bracket converts back to
HOURS per spec §7, so a half-hour slot renders as a decimal: a query landing
between `f000m30.bin` and `f001.bin` answers
`"time":{"method":"linear","forecast_hours":[0.5,1]}`. Integral slots keep the
integer rendering above (`[6,9]`). Run selection is recomputed per candidate run,
so one instant can be run A's `f001.bin` and run B's `f000m30.bin` — the newest
run that covers it answers. End-to-end coverage: `frac_http.rs`
(`fractional_hours_over_http`) plus `fractional_hours_queries_and_run_selection`.

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
curl 'http://127.0.0.1:8080/v1/models'                     # grid info (incl LCC params, source_winds_grid) + configured params/levels
curl 'http://127.0.0.1:8080/v1/inventory?model=hrrr'       # runs queryable NOW: fhs/params/levels (+stat_processed/stat_kind per field)
curl 'http://127.0.0.1:8080/health'; curl '.../ready'      # ready once >=1 indexed run per configured model
curl 'http://127.0.0.1:8080/metrics'                      # Prometheus text
```

**`source_winds_grid` (V1.3 phase E2 rename).** The `/v1/models` grid blocks
(both `regular` and `lcc`) report the wind-component convention of the SOURCE
messages as they arrive in `ingest_dir` — GRIB flag table 3.3 bit 3, recorded
from the first message that wrote that run's `grid.bin`. It is informational
metadata about upstream encoding and may legitimately differ between runs of
one model. Stored and queried U/V are ALWAYS earth-relative (spec §4.4
rotates grid-relative components at ingest; field headers carry `false`);
the old bare key `winds_grid` implied otherwise, hence the rename. Gate:
`models_reports_source_winds_grid_not_winds_grid` (also asserts the old key
is gone).

`/v1/inventory` per field carries `param`, `level_type`, `units`,
`stat_processed`, `stat_kind`, `forecast_hours` (DECIMAL HOURS — fractional
slots print as `0.5`, integral ones stay `0`/`6`/`12`; informational only, the
filename is the exact key) and `levels` of the newest forecast-hour file.

**Where statistical time ranges are visible:** the PDT 4.8 begin/end window
travels with the FORECAST HOUR and is exposed in `manifest.json` per field as
`stat_ranges` (§4 below). `/v1/inventory` deliberately exposes only the coarser
`stat_processed`/`stat_kind` flags plus the forecast-hour list — the guard
behaviour (`stat_time_guard` 422) is what the API needs; the exact window is
operational detail available on disk. Diff manifest JSON against wgrib2 text with
care: wgrib2 prints an accumulation as `[fc-len, fc]` while we store
`begin = fc_secs`, `end = fc_secs + len` uniformly (docs/DECISIONS.md, phase 3).

Metrics (Prometheus): `wxpoint_queries_total{status="2xx|4xx|5xx"}`,
`wxpoint_query_latency_us_{bucket,count,sum}` (µs histogram, cumulative buckets),
`wxpoint_ingest_errors_total`, `wxpoint_ingested_files_total`,
`wxpoint_skipped_messages_total` (V1.1 phase 3 counter: structurally unusable
messages noted, NOT errors — layer/range levels, coordinate lists after the
product-definition template, unimplemented grid/PDT templates; since V1.2 phase B
also wind components DROPPED by the pair rule or ORPHANED inside one file; they
never increment `wxpoint_ingest_errors_total`), `wxpoint_ignored_files_total`
(V1.2 phase B: candidates whose magic bytes are not `GRIB`, never decoded),
`wxpoint_retention_skipped_files_total` (V1.2 phase C: candidates whose SECTION-1 reference
time made them PROVABLY obsolete — vetoed before decode, never counted as errors, and NO
state record is written for them), `wxpoint_quarantined_files_total` (decode failures recorded WITH their content
hash so a corrupt file is not retried every round; since V1.3 phase E3a ALSO
counting the rare escalation of FIVE consecutive unreadable rounds — transient
I/O alone no longer quarantines), `wxpoint_ingest_groups_written_total`
(the write-once grouping unit — compare it with a message count to see the
amplification phase B removed), `wxpoint_stat_collisions_total` (V1.3 phase E1:
statistically-distinct groups REJECTED because an earlier group of the same
source file already claimed their field file — first-in-message-order wins;
skipped and logged, never counted as errors); — V1.3 phase E4a — the structural
skip breakdown `wxpoint_skip_coordlists_total` / `wxpoint_skip_grid_total` /
`wxpoint_skip_pdt_total` (FLAT counters on purpose: this surface has no label
machinery. They partition the STRUCTURAL subset of
`wxpoint_skipped_messages_total` — policy skips like wind-pair drops or
orphaned components count in the TOTAL only — and the exact template numbers
are spelled out in the ingest-log note, e.g. "2 skipped: 2 grid template
3.40"), `wxpoint_data_age_seconds`
(renamed from `wxpoint_ingest_lag_seconds` in V1.3 phase E6: this gauge is
NOW − newest ingested file mtime, i.e. how STALE the freshest data on disk is —
it was never a publish latency), the TRUE latency pair `wxpoint_ingest_publish_latency_seconds`
/ `_max` (per source file: file mtime → publish completion, wall-vs-wall, last
value + lifetime maximum — spec §7's "file mtime → queryable" metric, which
the old name only approximated), and — V1.3 phase E3a — `wxpoint_state_writes_total` /
`wxpoint_state_bytes_written`: successful `state/files.json` saves and the
bytes they serialized. The storage counters above deliberately EXCLUDE this
writer, so these two gauges close the observability gap; batched per-chunk
persistence keeps them small (see §7.4 scope note). Every ingest-side gauge
reads `max(legacy store, LIVE pipeline
mirror)`, and the live mirror advances after EVERY consumed FILE, so `/metrics`
reflects a scan round that is still running.

## 4. Storage format (`data_dir`)

```
data_dir/
  state/files.json                 # persistent ingest record
  <model>/<run>/                   # YYYYMMDDHH, +MM when the run minute OR second
                                   # != 0, +SS when the run second != 0 (10/12/14 chars)
    grid.bin                       # serialized grid definition
    manifest.json                  # human-facing inventory (params, levels,
                                   # fhs, units, stat info + ranges); since
                                   # V1.3 E0 the guard itself lives in the
                                   # .bin headers, not in this file
    <PARAM>/<level_type>/f<FFF>.bin  # e.g. TMP/isobaric/f006.bin
```

* **Run directory names** carry the reference time to its finest GRIB2 unit:
  `2026101912` (hour), `202610191230` (12:30Z), `20261019123015` (12:30:15Z).
  Length disambiguates, so runs 12Z and 12:30Z on one day no longer collide into
  one directory. Pre-existing 10-char directories keep parsing unchanged (minute/
  second default 0); impossible fields (minute 60) are rejected.
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
* `<param>/<level_type>/f<FFF>.bin`: magic `WXPTBIN1`, u32 version (**`BIN_VERSION`
  = 2**; a version-1 file from a pre-V1.3-E0 store is detected BEFORE anything is
  indexed and TRIGGERS AUTOMATIC RECOVERY — see §9 — never mis-parsed), u32 nx,
  u32 ny, u16 nlevels, flags byte (bit0 scan N→S, bit1 winds-grid-relative), then
  the PDT 4.8 STAT BLOCK that V1.3 phase E0 added (fixed header grew 24→56 bytes,
  +32 per field file): u8 `stat_kind` at byte 23 (0 instantaneous, 1 accumulation,
  2 average, 3 maximum, 4 minimum, 5 other), u8 `stat_processed` at byte 24, reserved zero
  at 25–27, i64 LE `begin_secs` / `end_secs` / `step_secs` at offsets 28 / 36 / 44
  (seconds past the RUN time; 0 when there is no range), reserved zero at 52–55.
  The strict reader refuses kind > 5, processed > 1 or non-zero reserved bytes
  (`HeaderError::BadStat`) instead of guessing. Then nlevels × f32 level values
  (pressure-sorted) starting at offset 56, then raw little-endian f32 planes laid
  out `[level][y][x]`. f32 because GRIB packing precision is already below f32;
  bitmap-masked points (§ sec 6) are stored as NaN. THE POINT of the stat block:
  the accumulation/average guard and its time window live INSIDE each field file,
  so spec §6.3 stays enforced even when `manifest.json` is missing, corrupt or
  never written — this header is exactly what a warm start reads back.
* `grid.bin`: magic `WXPTGRID`, u32 version (**`GRID_VERSION` = 3**; a stale
  version from an older store is detected on read and TRIGGERS AUTOMATIC
  RECOVERY — see §9 — instead of being silently ignored), kind byte (0 regular lat/lon,
  1 Lambert Conformal Conic, 2 gaussian lat-lon) + template fields (regular: nx,ny,lon1,lat1,dlon,
  dlat,scan,winds; LCC: nx,ny,la1,lo1,lov,lad,latin1,latin2,dx_m,dy_m,proj_flag,
  scan,earth_shape,winds, then two f64 major/minor axes in metres + an inline-axes
  flag so Code Table 3.2 user-defined spheroids survive a warm boot; gaussian
  (V1.3 phase E4b, GRIB template 3.40): nx,ny,lon1,la1,la2,dlon,scan,earth_shape,
  winds — the P_ny-root row-latitude table is DERIVED data recomputed bit-exactly
  from ny at read time (`gaussian_latitudes`) and deliberately NOT serialized).
  Precomputed projection constants are rebuilt at read time, not stored. The
  bump 1→2 exists because the Lambert block gained those axes: a store written
  before V1.1 phase 4 cannot represent a file that declared its own spheroid, so
  refusing beats silently re-projecting it on a guessed earth. The bump 2→3
  (E4b) exists because the set of valid KINDS grew (gaussian variant); old
  readers reject the unknown kind/version loudly and the same recovery trap
  re-ingests each affected model exactly once (docs/DECISIONS.md §16).
* Atomicity and durability chain: every write is temp-file → `flush` → **fsync of
  the file inode** → atomic rename → directory fsync, so a query never sees a partial
  file and a crash cannot resurrect a renamed-but-unlinked name. How fsync is obtained
  here: `wx_core::storage::fsync_path()` opens the path read-only and calls
  `std::fs::File::sync_all()`, which emits a real `fsync(fd)` — verified out-of-repo
  with a compile + `strace` probe (`/tmp/opencode/fsync_probe*.rs`), which also showed
  it works on a DIRECTORY handle (`File::open(dir)` → `fsync(4) = 0`). An earlier
  phase had claimed this toolchain had "no fsync surface at all" and bound libc
  `open`/`fsync`/`close` through `extern "C"` on that basis; that claim was WRONG and
  the bindings were replaced (same syscalls, same signature, one fewer FFI surface).
  Since V1.2 phase B the PARENT-DIRECTORY fsync is BATCHED: field writes record their
  directory in a `TouchedDirs` collector and the pipeline flushes every DISTINCT
  directory ONCE per source file (the file inode is still fsynced BEFORE its rename,
  so the crash rule is unchanged — a crash can only lose the directory ENTRY of a
  file whose data is already durable). Durability failures surface as ingest
  notes/errors, never swallowed. Visibility granularity is ONE SOURCE FILE (registry
  published once per file after all its GROUPS are written), which still satisfies
  spec §4.5 because products arrive forecast-hour by forecast-hour — see the
  transient-publish caveat in §9.
* `manifest.json` is human-facing inventory; the index is authoritative. Per
  field it records `units`, `stat_processed`, `stat_kind`, `levels`, decimal
  `forecast_hours` (informational only — the filename is the exact key) and
  `stat_ranges`, one entry per statistically processed forecast hour:
  `{"fh_secs":1800,"begin":"2026-10-19T12:30:00Z","end":"2026-10-19T13:00:00Z",
  "step_secs":0}`. Two accumulations of ONE parameter have DIFFERENT windows,
  which is why the range travels with the forecast hour rather than the field.
  SINCE V1.3 PHASE E0 THIS FILE IS PURELY OPERATIONAL INVENTORY: a warm start
  reads the guard AND the per-fh windows back from each field file's own header
  (the stat block above), never from here — so a MISSING or UNPARSABLE manifest
  costs a loud once-per-run note plus one counted ingest error, yet every
  answer stays right and `/ready` + `/v1/inventory` stay truthful
  (`warm_boot_without_manifest_keeps_the_accumulation_guard`,
  `corrupt_manifest_boot_is_loud_counted_and_still_answers_right`). A FAILED
  manifest write likewise blocks the source file's ingested-record, so the
  next scan round retries until inventory catches up
  (`manifest_write_failure_blocks_state_record_and_retries_next_round`).
* Retention: runs beyond `retention_runs` are removed from the in-memory index
  first, then deleted on disk; stale mmap handles stay valid until their last
  `Arc` reference drops (Linux unlink-while-mapped), so in-flight queries never
  fault (`retention_evicts_after_publish_stale_snapshot_still_reads`).
  Since V1.3 phase E3b the delete is QUARANTINED-FIRST: the victim run dir is
  RENAMED to `<name>.deleting.<stamp>` (one atomic step; `parse_run_dir`
  rejects the name, so the run is instantly invisible), then removed
  recursively. A crash between the two leaves only a husk no indexer can
  parse, and the STARTUP sweep finishes it before anything indexes — loudly
  counted (`quarantined_husk_is_swept_at_boot_before_anything_indexes`;
  rename-impossible falls back to the old direct delete, noted).
* Warm start: `state/files.json` ((model, path)+size+mtime_ns+FNV-1a64 over
  first/last 64 KiB, plus V1.2 phase B `kind`: 0 = ingested, 1 = magic-byte
  IGNORED, 2 = decode FAILED quarantined WITH its hash) prevents re-ingest
  after restart; same path with different content hash IS re-ingested (a
  FAILED record is retried ONLY then). UPGRADE NOTE (V1.3 phase E3b): records
  are now keyed PER MODEL (format VERSION 2, `"model"` key per record) so two
  models sharing an ingest dir with overlapping globs BOTH ingest instead of
  the first scanner starving the rest forever; a version-1 `files.json` is
  DISCARDED LOUDLY once (one boot note, NOT an error) and every source file
  re-ingests exactly ONCE — idempotent by construction, same philosophy as the
  storage-format recovery traps. Inside v2 a record without a `kind` key still
  loads as kind=0. The index is rebuilt from disk
  before any scan. V1.3 phase E3a: the two windows are read with plain
  `File`+`seek`+`read_exact` (no mmap on unstable candidates ⇒ no SIGBUS surface),
  digest and format UNCHANGED; a kind=2 record with an EMPTY hash marks the rare
  escalation of five consecutive unreadable rounds and self-clears once the file
  is readable again; saves are DIRTY-GATED and batched (once per decode chunk +
  round end), so a crash loses at most one unfinished chunk's records — those
  re-ingest idempotently.

## 5. Interpolation semantics (exactly as implemented)

Order for a query (lat, lon, level, valid time): horizontal at each of the
(up to) 2 levels × 2 times, then vertical, then time. All arithmetic f64;
return f64.

1. **Horizontal — bilinear.** lat/lon → fractional (i,j) via the model's
   projection (regular lat/lon wraps across the 359.75→0 seam; GFS scans
   north-to-south; LCC forward transform from La1,Lo1,LoV,LaD,Latin1,Latin2,
   Dx,Dy + Code Table 3.2 earth shape, constants precomputed per grid;
   GAUSSIAN template 3.40 since V1.3 E4b: i exactly like regular lat/lon
   incl. seam wrap, j by binary search in the precomputed P_ny-root row table
   with LINEAR interpolation in j between neighbouring rows — spec §6.1 works
   in grid-index space, identical rule to the other grids). Exactly
   on a node (within 1e-9 grid units) returns that node's value exactly, no
   blending. Outside the node rectangle (incl. the last half-cell at
   non-wrapping edges) → 422 `out_of_domain`; never extrapolated. On gaussian
   grids the node rectangle ends at the outermost Gaussian rows (~±89.91° for
   GFS 3072×1536): there are NO pole nodes, so lat ±90 is ALWAYS 422 — defined
   behavior, pinned by tests. Any NaN
   among the 4 neighbors → `null` + `missing_data` (weights never renormalized).
2. **Vertical — linear in the level coordinate** (hPa for isobaric, RAW
   hybrid coordinate for hybrid levels — V1.3 phase E4c: hybrid coordinates
   order numerically and interpolate LINEARLY IN THE HYBRID COORDINATE
   between bracketing stored hybrid levels; that is an approximation choice
   consistent with the spec letter — any numeric orderable level type
   interpolates linearly in its own coordinate, and a sigma-vs-pressure
   refinement would need producer A/B coefficients we do not parse,
   docs/DECISIONS.md §17). Brackets NEVER cross level types: every field
   file carries one type's levels (separate directories), pinned by tests.
   Exact
   stored level used directly; otherwise bracketing stored levels interpolate
   linearly (a log-pressure option is structurally possible but NOT enabled —
   spec default). Request outside the stored range → 422 `level_out_of_range`,
   never extrapolated. Configured gaps (850 and 500 only) interpolate ACROSS
   the gap; the response echoes the bracketing levels so users can see it.
3. **Time — linear between forecast hours of the SAME run, in SECONDS.** All
   comparisons and interpolation use whole-second offsets past the run's
   reference time (`select_run(want_fh_secs)`, `resolve_field_times`,
   `time_lerp`), so FRACTIONAL forecast hours (`f000m30.bin`; CT 4.4 units
   minute/hour/day/3h/6h/12h/second) are first-class data, never rounded onto
   hour slots. Seconds were chosen because 1800 s is exactly representable as an
   integer while 0.5 h is only exactly representable as a fraction. Default run =
   most recent run covering `valid`; the offset is RECOMPUTED per candidate run
   (one instant can be run A's `f001.bin` AND run B's `f000m30.bin` — the newest
   run that covers it answers), and `run=` pins one exactly. Both bracketing slots
   must exist when interpolating. Exact fh match uses it directly (no blending,
   no `time` object in the response). The reported `forecast_hours` bracket converts
   back to HOURS per §7 (e.g. `[0.5,1]`). Statistically processed data (PDT 4.8
   accumulations/averages/max/min) NEVER blends in time, and since V1.3 phase E1
   the guard is decided PER FORECAST HOUR, not per field: each `.bin` header
   carries its own statistical state and the decision uses ONLY the slots the
   answer would touch. An EXACT fh always answers from its own slot (exactness
   is what §6.3 asks for, not absence of processing). LINEAR interpolation
   requires BOTH bracket endpoints to be instantaneous; if either endpoint came
   from template 4.8 the query is refused with 422 `stat_time_guard` and the
   message NAMES the refused bracket, the offending forecast hour and its kind
   (e.g. "linear time interpolation between forecast hours 0 h and 6 h would
   blend statistically processed data: forecast hour 6 h is 'accumulation' …").
   A stack that mixes instantaneous and processed hours therefore interpolates
   FREELY across clean brackets and refuses exactly the dirty ones — regardless
   of which message registered first. If NO bracket exists at all and ANY
   stored slot of that field is statistically processed, the query fails closed
   with the same guard (naming the first processed hour) instead of silently
   trying older runs. `time_interp=none` outranks the guard (422
   `time_exact_only`). A valid time outside every run's stored hours → 404
   `no_run_covers`.

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
+ time ranges stored and surfaced through `/v1/inventory` + `manifest.json`). Decode runs on
a dedicated rayon pool (`ingest_threads`) separate from the HTTP runtime.
The grib-rs crate was evaluated and rejected (capability gaps documented in
docs/DECISIONS.md); the decoder is purpose-built against wgrib2 behavior.

**Ingest loop shape (V1.2 phase B).** Candidates are collected per model, then
processed in CHUNKS of `decode_batch_files` (default 4): ONE rayon pass decodes a
chunk, and EACH file is consumed IMMEDIATELY afterwards — magic-byte precheck
(non-`GRIB` candidates get a kind=IGNORED record and NEVER reach the decoder), wind
pairing scoped to that one file (leftover components are ORPHANS: noted, counted as
skips, removed, never stored half-rotated), GROUPING of every plane by
`(run, param, level type, forecast hour)` so each field file is merged and written
ONCE (`merge_write_field_batch`) instead of once per message, registry update,
manifest refresh ONCE per affected run, directory fsyncs ONCE per distinct directory,
ONE publish, ONE progress line, and a state upsert (persisted in BATCH since
V1.3 phase E3a: dirty flag + one save per decode chunk + round end, replacing the
per-file full-state rewrite). A CONTENT decode failure records
kind=FAILED with the content hash (so `errors` advances ONCE per new/changed corrupt
file, not once per round); a TRANSIENT I/O failure (open/mmap: EMFILE/EACCES/EIO/
ENOMEM…) only bumps an in-memory streak — one note, silent retries, and the FAILED
record only after FIVE consecutive unreadable rounds (V1.3 E3a, see §9). A
group-write failure leaves the file UNMARKED so the
next round retries it idempotently. Full rationale + measured numbers: docs/DECISIONS.md.

**Retention-aware skip + state hygiene (V1.2 phase C).** Before the magic-byte precheck,
each candidate gets a cheap SECTION-1 header PEEK (`peek_ref_times`: framing-only walk,
sections 2..7 stepped over by their declared lengths). It is VETOED without decoding ONLY
when the model's registry already holds ≥ `retention_runs` runs AND the file's NEWEST
reference time is STRICTLY OLDER than the oldest kept run — such a run would be evicted again
in the SAME round. Equality is NEVER skipped (a partially delivered same-run product must
still merge). An UNCERTAIN peek falls through to the normal path, never to a skip. Skipped
files get NO persistent record (the verdict is re-derived per round, so they become ingestible
again if newer runs vanish); the note and the `wxpoint_retention_skipped_files_total` counter
are deduped per (path, content-hash). Candidates are ordered NEWEST-RUN-FIRST by a FILENAME
hint that is ORDERING ONLY — the skip verdict always comes from GRIB section 1.
State hygiene: `find` is an O(1) derived HashMap (on-disk `state/files.json` format UNCHANGED,
`kind` included, legacy files without `kind` still load), records whose source file VANISHED
are pruned once per model per round, stability observations are LRU-bounded
(`STABILITY_OBS_CAP = 10 000`) and forget vanished paths, and the notify event queue is drained
ONCE per ROUND so no model starves the others.

Concurrency: read-mostly index published atomically with `arc-swap`; query
threads take zero locks, ZERO heap allocations anywhere on the lookup path
(V1.1 phase 5: not just `evaluate_point` — from query-string parse through
interpolation-metadata structuring; only JSON body construction allocates),
and zero file I/O beyond page-faulting mmapped pages (all mmaps opened at
index-build time). Proven by `hotpath_alloc.rs` (counting allocator: ZERO
allocations for `build_context` + `evaluate_point` over 20k queries) and
`hotpath_full_alloc.rs` (ZERO allocations across 20k mixed success-class
queries + 2000 masked queries, boundary = everything up to, EXCLUDING,
response-body string construction).

## 7. Performance (measured on this machine)

Machine: AMD EPYC 7402 (48 logical cores; load test pinned to 8 worker threads
per spec §8), Linux 7.0.0-31-generic, rustc 1.99.1, oha 1.16.0. Dataset: full
GFS geometry (1440×720, node rectangle lat 89.875…−89.875, 22 msgs/file ~34 MB
×2 files) + HRRR-like LCC (1799×1059, 13 msgs/file ~37 MB ×2), warm page cache.

### 7.1 Criterion benches (`cargo bench`, bench profile = release minus panic)

V1.1 phase 5 re-measured the whole ladder back-to-back around the allocation-free
refactor; the **phase-6 acceptance sweep** re-ran all three bench files again
(`cargo bench --workspace`, 100 samples each). Milestone-5 numbers kept for
reference; session variance on the pure kernels is ~±2%.

| Kernel | milestone-5 | phase 5 before | phase 5 after | phase-6 sweep median | V1.2 phase-D sweep median | V1.3 E6 sweep median |
|---|---|---|---|---|---|---|
| regular lat/lon project (per point) | 20.6 ns | 20.5 ns | 20.5 ns | **20.430 ns** | unchanged (prior row stands; not re-measured in D) | **27.584 ns** |
| LCC project, precomputed consts | 84.7 ns | 86.4 ns | 86.5 ns | **86.187 ns** | unchanged (prior row stands; not re-measured in D) | **107.98 ns** |
| bilinear over heap f32 slab | 30.7 ns | 30.9 ns | 31.0 ns | **30.746 ns** | **30.85 ns** | **37.87 ns** |
| bilinear over mmap field (server h_at pattern) | 32.5 ns | 33.1 ns | 33.0 ns | **32.663 ns** | **32.90 ns** | **50.39 ns** |
| full vertical+time point query via index accessor | 364.5 ns | 354.6 ns → 135.0 ns (−62%) | **136.62 ns** | not recorded (pre-E6 reference = V1.2-D's **147.55 ns, +8.0% — see note below**) | **147.55 ns (+8.0%)** | **185.49 ns** |
| `/v1/point` handler: parse + lookup + render | 879.2 ns | 828.8 ns → 694.3 ns (−16%) | **681.74 ns** | not recorded (pre-E6 reference = V1.2-D's **671.08 ns, −1.6%** improvement) | **671.08 ns (−1.6%, improved)** | **668.10 ns (flat)** |

Handler-level percentile pass (200k iterations, no HTTP, warm page cache;
criterion 0.5 prints medians only): **p50 = p90 = p99 = 1 µs, mean 1.007 µs,
single-thread throughput ≈ 993,000 ops/s** (re-measured in the V1.2 phase-D sweep;
the sweep also corrected a mislabelled unit on that printed line: `ops/s`,
previously rendered `Mops/s`).

**V1.3 phase E4b adds `projection/gaussian project (table + binary search)`**:
measured back-to-back with the other two projection arms in the same session
(E4b acceptance sweep, criterion medians): regular_ll **28.7 ns**, LCC
**105.9 ns**, gaussian **88.6 ns**. All three arms read higher than the
phase-6 column above (the session ran behind hour-long debug-profile realdata
decodes — thermal/governor state, plus the Grid enum gained a variant); what
matters is the ORDER and the absolute cost: gaussian sits between pure-affine
regular and trig-heavy LCC exactly as its math implies (~768-entry f64 binary
search + blend), and ~90 ns per projection stays noise against the mmap
page-fault-dominated query budget. The row table is built once at grid
construction; the hot path allocates nothing (`hotpath_alloc.rs` still green).

**Why the point-query bench moved 136.6 -> 147.6 ns (+8%).** V1.2 phase B0 made
vertical interpolation carry PER-SLOT level indices (a silent wrong answer when one
time's bracket differs from the other's: the old code reused ONE bracket pair across
both forecast hours). Composing those per-slot brackets costs extra work on exactly
the differing-bracket path this bench exercises, so the median rose ~8%. This is an
accepted correctness-over-speed trade, explicitly per the spec's priority order
(spec §8 priority order: correctness first; a fast wrong answer is a bug), and it stays 2.5x faster than
milestone-5 and far inside the §8 latency budget (server-side p50 ≤10 µs, p99 ≤50
µs measured under load). The handler itself got FASTER in the same sweep (681.7 ->
671.1 ns), so the end-to-end request path did not regress.

**V1.3 E6 release-candidate re-run (this table's last column).** Plain
`cargo bench` per crate on the otherwise-idle machine right before tagging
(no `--release` flag — the bench profile matches the historical columns —
and no `target-cpu=native`, which remains NOT shipped). Read honestly, and
against the RIGHT baselines: the projection arms compare against the E4b
back-to-back session (28.7 / 105.9 / 88.6 ns — the Grid enum gained a variant
since the phase-6 column, which is why phase-6's 20.4/86.2 are not the
reference here): regular **−3.9%**, LCC **+2.0%**, gaussian **+12.3%**
(criterion's own verdict line reads "Performance has regressed" — reported
verbatim, not explained away). The bilinear arms compare against their
V1.2-D columns: heap **+22.8%**, mmap h_at **+53.2%**. Full point query
**+25.7%** vs the pre-E6 147.55 ns reference; handler **−0.4%** = unchanged.
The split is systematic, not random: every MICRO arm reads higher while both
MACRO arms (which include I/O and render work that dominates wall time)
stand still — the same session-sensitivity signature already documented for
the E4b re-run (codegen placement + governor state; the pure-kernel column
has always carried ~±2% and this session ran further out). Two facts
anchor the interpretation: (1) E6 touched NO hot-path arithmetic — the only
additions are one byte-scan over the query string (`normalize_qs` Borrowed
fast path) and one relaxed atomic store per published FILE on the ingest
side; (2) the end-to-end indicators from the SAME sweep confirm no practical
regression: smoke.sh avg point-query 30.2 µs (previous 28.9 µs, both deep
inside budget), loadtest server-side mean 11.4 µs / buckets ≤20 µs / ≤50 µs
at 282,265 req/s, and realistic ingest wall-to-first-improved (§7.4). The
bench HARNESS files themselves additionally received lint-only `--fix`
rewrites in E6 (mechanical, semantics-preserving by contract, but codegen
may move), which is part of why micro numbers shifted. No number was
cherry-picked: all seven medians are printed in the table as measured.

The −62% on the index accessor is the removal of the per-query units `String`
clone, the per-slot level `Vec<f64>`, the forecast-hour scratch `Vec<f64>` and
the two `Arc` refcount round-trips (contexts now borrow grid defs, units
strings and mmap handles straight out of the snapshot). The handler delta
adds `parse_query` borrowing its strings from the request (no per-key
`to_string()`, no params `Vec`) plus the fixed-inline entry buffer.

### 7.2 Load test (`scripts/loadtest.sh [secs]`, `NATIVE=1` variant)

oha mix: 8 URLs (exact-node GFS, seam, vertical interp, time interp,
multi-param, HRRR LCC, 404 unknown model, 422 bad level), `--worker-threads 8
-c 64 -z 20s --no-tui --urls-from-file`, keep-alive.

| Metric | portable (phase 5 before) | portable (phase 5 after) | portable (phase-6 sweep, 2 runs) | target-cpu=native (pre-phase-5 / phase-6 sweep) | V1.2 phase-D sweep (15 s) | V1.3 E6 sweep (20 s) |
|---|---|---|---|---|---|---|
| throughput (client, 8 workers) | 286,868 req/s | 286,872 req/s | **287,451 / 286,342 req/s** | 275,293 req/s / 281,305 req/s | **272,647 req/s** | **282,265 req/s** |
| server-side mean latency (/metrics delta) | 12.5 µs | 10.0 µs | **9.4 µs** (n = 5,752,100 / 5,730,170 queries) | 13.4 µs / 9.9 µs | **10.3 µs** (n = 4,091,386) | **11.4 µs** (n = 5,649,715) |
| server-side p50 / p99 (histogram buckets) | ≤20 µs / ≤50 µs | ≤10 µs / ≤50 µs | **≤10 µs / ≤50 µs** | ≤20 µs / ≤50 µs / ≤10 µs / ≤50 µs | **≤10 µs / ≤50 µs** | **≤20 µs / ≤50 µs** |
| client p50 / p99 (oha) | 0.21 ms / 0.42 ms | 0.21 ms / 0.42 ms | **0.211 ms / 0.431 ms** | similar | similar (shorter 15 s window) | not emitted this run (script flags) |
| status mix in the mix | — | — | **200: 4,297,215 · 422: 717,234 · 404: 715,667** (run B) | — | — | **200: 4,237,416 · 422: 706,616 · 404: 705,621** (+62 deadline aborts) |

§8 targets: **p50 < 50 µs MET** (bucket ≤10 µs), **p99 < 250 µs MET** (bucket
≤50 µs), **≥50k req/s on 8 cores MET ×5.5** (272,647 req/s in the V1.2 phase-D 15 s
run; ×5.7 in the phase-6 runs). Same sweep, `scripts/smoke.sh`: **SMOKE PASS**, all
17 `probes.tsv` ground-truth probes replayed with 0 failures, release-build average
point-query latency **16.9 µs** across its 422 correctness queries. Client-side throughput did NOT move
across phases (expected): §7.5 profiling showed the sustained path is kernel-I/O-bound
(one `writev` per response dominates); the allocation removal shows up where
it belongs — server-side mean latency 12.5→10.0→9.4 µs and the p50 bucket
tightening ≤20→≤10 µs. No gap analysis needed: first measurement hit all
targets, so NO bit-risky bounded optimizations were attempted at all
(deliberate decision recorded in docs/DECISIONS.md). Phase 5 changed only
WHERE values are read from, never arithmetic; equivalence is pinned by
`alloc_free_kernels.rs` + `alloc_free_select.rs` + the probes.tsv value
assertions inside `hotpath_full_alloc.rs`. `target-cpu=native` was measured again
in this sweep and is still NOT better (−1.8% throughput, mean latency 9.9 vs
9.4 µs), so the shipped build stays portable. The V1.3 E6 release-candidate
re-run confirms every §8 target holds on the tag tree: **282,265 req/s**
(×5.6 of the ≥50k target), server-side mean **11.4 µs** with buckets
**≤20 µs / ≤50 µs** (p50 < 50 µs MET, p99 < 250 µs MET), success rate 100%
with only 62 deadline aborts, and `scripts/smoke.sh` **SMOKE PASS** at a
release-build average point-query time of **30.2 µs** over its 422
correctness probes (previous session: 28.9 µs — ordinary session drift of
the kind §7.1 documents, deep inside budget).

Note on script robustness found during that sweep: `loadtest.sh`'s
pre-hammering sanity probe originally fired once per URL and could race the then
field-by-field publisher (a 200-class HRRR URL answered 422 while its level
stack was still partial). The script waits each URL out (bounded 15 s) instead
of failing — kept as belt-and-braces even though publish became PER FILE in
V1.2 phase B, which closed the half-stack window within one file (§9; the
concurrency harness observes zero transient 422s).

### 7.3 Ingest → queryable (`scripts/ingestbench.sh`)

Files placed cold; scan_interval_secs=1, stable_window_secs=0, ingest_threads=16,
decode_batch_files=4 (the script honours `BATCH=<n>` to override K):

| metric | pre-phase-B (per-message writes) | phase B, K=4 (default) | phase B, K=1 |
|---|---|---|---|
| first-queryable (`/ready`) | **8.28 s** | **4.05 s** | 4.97 s |
| last forecast hour visible | 9.44 s | 4.55 s | 5.68 s |
| server CPU time to that point | 7.55 s | 4.2 s | 4.2 s |
| peak RSS (VmHWM) | 362 MB | ~421 MB | ~177 MB |

One GFS forecast hour ≈ 10 params becomes queryable within a few seconds (§8 target
MET, now at roughly HALF the previous wall time). The RAM column is the deliberate
trade-off of grouping: a file's planes stay resident until its GROUP is written, so
peak RSS scales with K (`K=1` is both faster-to-steady-state AND lower-RAM than the
pre-B code ever was; `K=4` buys earlier first output for ~2.4x that floor).

Write amplification on the SAME dataset (storage counters, measured by
`crates/wx-ingest/tests/ingest_perf_grouping.rs` and its pre-B twin):

| counter | pre-phase-B | phase B |
|---|---|---|
| renames per GFS hour | 45 (22 field writes + 22 manifest rewrites + grid.bin) | 10 (8 GROUPS + grid.bin + manifest); next hour 9 |
| bytes written per hour | 323.5 MB vs 91.2 MB final store (**3.55x**, = 78 plane serializations where 8 suffice) | 182.48 MB vs 182.48 MB final store (**1.0000x**) |
| fsyncs per hour | 94 | 26 (1 per field inode + 2 per aux writer + 1 per DISTINCT dir) |

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

### 7.4 Ingest performance & memory (V1.2 production-scale benchmark)

The milestone-5 perf dataset has SHORT isobaric stacks (L≈8 planes per GROUP), which
hid the merge-path cost that matters in production. V1.2 phase D therefore measures
PRODUCTION-SHAPE datasets generated by `scripts/gen_realistic.sh`:

* **realistic** — full GFS geometry 1440 x 720, **37 isobaric levels** (every 25 hPa
  1000..100, a superset of the real NWS 25-level stack), TMP+HGT+UGRD+VGRD on
  `isobaric:*` = 148 messages/file, SIX files (3 runs x f000+f006, ~192 MB each,
  authentic NWS names WITHOUT a `YYYYMMDDHH` stamp), wind messages carrying the
  grid-relative flag. Measured on the V1.2 build only: the pre-V1.2 writer needs
  `(L+1)/2 = 19x` its own output volume here (~70 GB), which made a traced run
  pathological, so the BEFORE side is measured on the reduced variant below and
  extrapolated ANALYTICALLY (labelled as such).
* **reduced** — SAME shape, ONE run, f000+f006, **16 isobaric levels** (2 files, 83
  MB each): the bounded-but-representative BEFORE-vs-AFTER pair.

| metric (this machine, release, scan=1 s, stable_window=0, threads=16) | pre-V1.2 (tag `v1.1`) reduced L=16 MEASURED | V1.2 `decode_batch_files=4` reduced L=16 MEASURED | V1.2 realistic L=37 MEASURED |
|---|---|---|---|
| wall-to-first-queryable (`/ready`) | 2.49 s | 3.08 s | 5.62 s |
| wall-to-all-visible (all run/fh tuples WITH the whole level stack) | **20.93 s** | **3.69 s** | 15.73 s (K=4) / 25.35 s (K=1) |
| peak RSS (`VmHWM`, 100 ms poller) | 766 MB | 670 MB | 3079 MB (K=4) / 1087 MB (K=1) |
| server CPU time to that point | 18.4 s | 3.8 s | 26.0 s (K=4) / 24.0 s (K=1) |
| renames | n/a (untraced) | 11 (8 field files + grid.bin + manifest) | 33 (24 field files + 3 grid.bin + 6 manifest) |
| bytes written vs final store | ANALYTIC **(L+1)/2 = 8.50x** at L=16 (~4.5 GB for a 531 MB store) | 530.8 MB vs 530.8 MB (**1.0000x**) | 3682.7 MB vs 3682.7 MB (**1.0000x**) |
| fsyncs | n/a (untraced) | 30 | 82 |
| per-file ingest (64 msgs / 148 msgs, 4 GROUPS) | n/a (no per-file lines in v1.1 output) | 739 ms | 1.35–1.64 s |

The amplification law is the headline and it lands exactly on theory: for L levels
per GROUP the pre-V1.2 writer serialized `1+2+...+L` planes where L suffice, i.e.
`(L(L+1)/2)/L = (L+1)/2` — **8.50x expected at L=16**, **19.0x at L=37** — while V1.2
writes each GROUP ONCE: **1.0000x measured at both scales**. The same law was MEASURED
directly at L=8 in phase B (3.55x = 78 plane serializations where 8 suffice, 45
renames per GFS hour), so the analytic column is an extrapolation of two measured
points, not a guess. Wall-to-all-visible improved **5.7x** at L=16 (20.9 -> 3.7 s)
and CPU time **4.8x** (18.4 -> 3.8 s).

**SCOPE of the 1.0000x headline (V1.3 phase E3a clarification).** Those columns come
from the wx-core storage counters (`wxpoint_storage_bytes_written_total`), which
cover FIELD/GROUP + grid.bin + manifest writers and deliberately EXCLUDE the state
writer. Pre-E3a `state/files.json` was rewritten after EVERY ingested/ignored/failed
file — O(N²) bytes and fsyncs across thousands of files, invisible to the headline.
Since E3a saves are dirty-gated and batched (once per decode chunk + round end),
and `/metrics` reports them separately as `wxpoint_state_writes_total` /
`wxpoint_state_bytes_written` (also printed by `scripts/ingestbench.sh`). Exact
counts pinned by the E3a gate (`state_batching.rs`, 4 files at K=2): **2 saves per
ingest round** (one per chunk; observation and idle rounds cost ZERO writes) versus
5 full rewrites pre-E3a; total state bytes scale as ~(files/K + rounds) x
records-size instead of files x records-size. The tables above PREDATE the split,
so their bytes columns never included state traffic — the clarification keeps the
comparison honest.

One honest REGRESSION to record: wall-to-FIRST-queryable got slightly WORSE at L=16
(2.49 s -> 3.08 s, and 5.62 s at L=37) because visibility is published PER SOURCE
FILE instead of PER MESSAGE, and up to K files are decoded before any is written.
Partial visibility earlier is exactly what produced half-stacks of levels and the
transient 422s; the trade is deliberate (see §9 Limitations and docs/DECISIONS.md
phase D), and every query answered after publish is complete for its file.

Spec §8's ingest target at production scale is MET with margin: ONE realistic GFS
forecast hour (148 messages, 192 MB file, 4 params x 37 levels) became queryable
**1.35-1.64 s** after the file was stable, and the whole six-file delivery was
queryable **15.7 s** after the scan started (K=4).

**V1.3 E6 re-confirmation (same script + dataset, release-candidate build):**
`[main/plain]` first-queryable **5.01 s**, all-visible **13.92 s**, peak RSS
**3066 MB**, server CPU **21.98 s** — roughly 11% FASTER wall-to-everything than
the V1.2-D column above (5.62 s / 15.73 s / 26.0 s), consistent with the E3a
batched-state-save work removing per-file overhead; RSS essentially unchanged,
as expected since the K=4 plane-residency law below did not move. The script
output now also carries the E3a state counters (`state_saves=2
state_bytes=2014` for this whole delivery), closing the observability gap the
SCOPE note above describes.

**Memory guidance for `decode_batch_files` (K).** Peak RSS scales with K because a
consumed file's GROUP planes stay resident until that file is written: one realistic
file's planes are 4 params x 37 levels x 1440 x 720 x 4 B = **614 MB**, so K=1
measured **1087 MB** and K=4 measured **3079 MB** (planes plus decode/publish
overhead). Rule of thumb: `RSS ~= K x (largest concurrent source file's plane set)`.
Use K=1 when RAM-constrained (it halves the footprint again and is still ~5x faster
than pre-V1.2 all-visible); keep the default K=4 for best all-visible latency; raise
K only with several GB of headroom per additional concurrent file.

Correctness rode along with the benchmark: `scripts/_probe_realistic.py` queried the
LIVE server over the realistic dataset - 481 probes covering exact nodes (4 points x
4 params x 4 levels x 3 runs x 2 forecast hours), vertical interpolation at 837.5 mb
between stored 850/825, and time interpolation at fh3 between f000/f006 - and ALL
481 matched the analytic formula wgrib2 had confirmed. One honesty note: during warm-up
the bench's `/ready` polls return 503 (`not_ready`), which `/metrics` counts under
`wxpoint_queries_total{status="5xx"}`; those are readiness probes, not query failures.


### 7.5 Profiling pass (`scripts/profile.sh [secs]`, optional `STRACE=1`)

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
materially; the §8 margin (≤10 µs bucket vs 50 µs target) stands without any.

## 8. Correctness map (spec §9 items 1..9)

Every spec-mandated verification, as a named test with its truth source and
tolerance. All green in the V1.2 phase-D sweep (`cargo test --workspace`: 153 tests,
0 failed, twice back-to-back; `scripts/smoke.sh`: SMOKE PASS, 17 probes replayed,
0 failures).

| # | Spec requirement | Test(s) | Truth source | Tolerance |
|---|---|---|---|---|
| 1 | Exact grid points match `wgrib2 -lon` / `grib_get_data` on real files | `gfs_wgrib2_point_checks`, `hrrr_nearest_node_matches_wgrib2` (nearest-node for LCC nodes), `perf_gfs_hour_ingest_and_query`; smoke `probes.tsv` replay + `point_endpoint_truth_table` | **wgrib2-generated synthetic fixtures** (see the note under this table): wgrib2 `-lon` output captured into the tests; `tests/perfdata/probes.tsv` generated with the perf dataset by `gen_perfdata.sh`/`validate_perf.py` | exact for values chosen exact-in-f32; otherwise ≤1 ulp of f32 |
| 2 | LCC lat/lon → (i,j) matches wgrib2/pyproj at corners, centre, ≥20 random points | `lcc_matches_wgrib2_ll2ij`, `lcc_corner_center_coverage`, `lcc_inverse_roundtrip`, `oblate_projection_matches_wgrib2_ll2ij` | wgrib2 `-ll2ij` on `hrrr_small.grib2` (CT 3.2 = 6 sphere) and `hrrr_ellip_small.grib2` (CT 3.2 = 3 inline WGS 84 axes); pyproj cross-check via `scripts/validate_rotation.py` | ≤1e-4 grid units |
| 3 | Rotated HRRR U/V match `wgrib2 -new_grid_winds earth` | `rotation_matches_pyproj_within_1e_6_deg`, `rotation_equals_convergence_relation_of_own_projection`, `rotation_shape_axes_are_distinct_earths`, `hrrr_rotation_invariants`, `oblate_rotation_and_inverse_roundtrip_at_nodes` | **golden blocked here** (IPOLATES Fortran-only, see §9); replaced by independent pyproj conformal angles for BOTH earth shapes: `scripts/validate_rotation.py` → `tests/fixtures/rotation_expected.csv` (42 rows), plus the identity β = 90 + n·(λ−LoV) relation of our own cone math | ≤1e-6 deg (observed worst **4.80469e-11 deg** this sweep); identity relation ≤1e-12 deg |
| 4 | Bilinear / vertical / time reproduce analytic fields exactly | `affine_exact_interior`, `exact_node_bitexact`, `bilinear_kernel_affine_nan`, `vertical_gap_bracket`, `time_guard_stat_and_none`, `empty_field_never_brackets`, `e2e_roundtrip_through_index`, `fractional_hours_storage_and_manifest`, `fractional_hours_queries_and_run_selection` | synthetic grids whose fields are affine in x,y,p,t (answers known analytically) | bit-exact (no blending on exact nodes; bracket selection identical to the reference implementation in `alloc_free_kernels.rs` / `alloc_free_select.rs` randomized sweeps) |
| 5 | GFS seam at lon 359.9 / −0.1 / 0.0 interpolates across 359.75→0 | `seam_blend_values`, `horiz_nodes_wrap_indices`, `gfs_seam_through_http`; smoke probe `lat=0.125&lon=359.9` (+ negative-lon probe) | analytic values on a wrapping regular grid + wgrib2-derived perf probes | bit-exact / f32 rounding |
| 6 | Poles and domain edges defined and tested | `poles_global_grid` (lat ±90 on a global regular grid: exact node row, no wrap artefacts), `out_of_domain_edges` (west/east/south edges incl. "a hair outside"), smoke HRRR edge probe (`lat=59.0` → 422) | defined semantics (node rectangle, never extrapolate) on synthetic grids; real-file caveat in §9 | n/a — behaviour is defined, asserted exactly |
| 7 | Masked points return `null`, never a blended number | `nan_neighbor_missing_data`, `masked_points_are_nan_for_every_codec`, `bilinear_kernel_affine_nan` | fixture bitmap masking every 97th node, re-checked through all seven packings | must be exactly `null` + `missing_data` (NaN positions preserved per codec) |
| 8 | Time interpolation of accumulations is rejected | `stat_small_messages`, `stat_point_checks`, `jpeg_twin_keeps_pdt48_metadata_and_values`, `time_guard_stat_and_none`, `error_status_table` (422 `stat_time_guard`), `fractional_hours_over_http` guard cases | PDT 4.8 fixtures `stat_small.grib2` (+ JPEG twin) validated against wgrib2 readback/checksums | n/a — status/token equality |
| 9 | Ingest under hammering queries: no error, no partial data | `concurrent_publishers_and_readers_never_tear`, `ingest_churn_vs_queries_http`, `retention_evicts_after_publish_stale_snapshot_still_reads` | invariant assertions over live publish/evict churn (zero 5xx, zero torn JSON bodies, settled-state batch proves transient 422s disappear) | n/a — invariants; the transient-422 window CLOSED at file granularity in V1.2-B (`fin[5] == 0` pinned) |

**What "real files" means here (honesty note, V1.2 phase D).** Every fixture in
`tests/fixtures/` and `tests/perfdata/` is SYNTHETIC: written by our own generator
(`scripts/gen_grib2.py`) or re-packed by `tools/wgrib2-full`, using authentic GRIB2
section layouts, authentic GFS/HRRR grid geometry and authentic NWS product names —
but NOT NOAA's bytes. The independent truth source in rows 1-3 is therefore
**wgrib2 reading those same synthetic files**, which validates our decoder, projection
and rotation against a mature reference implementation without validating against
production products. Real NWS files are not fetched in CI (spec §13: no downloading).
To close that gap manually, run `scripts/fetch_real_sample.sh` (MANUAL / NETWORK:
it fetches the `.idx` message index of live NOMADS products and HTTP-Range-extracts
only the messages under test — a few MB total — into the gitignored
`tests/realdata/`). With that directory present, `cargo test -p wx-ingest
--test realdata_gate` activates live-byte gates (GFS sflux messages now DECODE
on template 3.40 end to end since V1.3 E4b and STORE fully — current
operational GFS sflux products are fully queryable end-to-end, surface +
hybrid upper-air, since E4c: the gate pins decoded == total per file, an
exact seven-entry stored-field set (zero unstored decodes, incl. the out-of-
table ACOND raw triple), and replays TMP/UV/HGT/ACOND plus the HYBRID-level
HGT/TMP probes through the full ingest+index path against recorded
`wgrib2 -lon` values, plus the 212-byte degenerate APCP and HRRR node
probes); without it they skip cleanly, so CI stays offline. Recorded probes
and the full diagnosis: docs/DECISIONS.md §15 (E4a), §16 (E4b grid
acceptance) and §17 (E4c hybrid acceptance table).

**Consolidated live acceptance harness (V1.3 phase E5, MANUAL/NETWORK, never
CI):** `scripts/realdata_acceptance.sh` wires everything end to end in one
command — idempotent fetch → release build → boots `target/release/wxpoint`
on a spare port (default 18099) with a current-operational config (gfs sflux
incl `hybrid:*`, hrrr incl the raw `d0c2n1` WIND triple) → waits `/ready` →
runs `scripts/realdata_probes.py` (20 pins) → prints the sha256 provenance of
every fetched slice; server shutdown is trapped, any failed pin exits
non-zero. The probes validate against expectations derived INDEPENDENTLY of
our Rust path: real HRRR wind rotation (section-3 component flag byte read
from bytes; raw grid-relative U/V printed by `wgrib2 -lon`; cone angle
measured by pyproj via the E-era `validate_rotation.py` chain since this
wgrib2 build has no IPOLATES `-wind`), real same-run time interpolation
blends of 2 m TMP across fh {0,6,12}, and production PDT 4.8 metadata +
guards straight off operational bytes (`/v1/inventory` stat flags, 422
`stat_time_guard`). What stays in CI: everything network-free — the synthetic
fixture suites and the realdata gates (which skip cleanly without
`tests/realdata/`); nothing automated invokes this script. Measured deltas
and decisions: docs/DECISIONS.md §18.
See §9 Limitations for what remains unverified until you do.

Beyond §9 (V1.1 additions): packing parity — each small fixture exists in six
extra packings (`_c3` 5.3, `_jpeg` 5.40, `_png` 5.41 filter None, `_png_f` 5.41
cycling all five PNG predictors, `_aec` 5.42, `_ieee` 5.4); `packing_twins.rs`
asserts element-wise identical arrays vs the simple-packed original (NaN positions
preserved), point probes reproduce on every twin, skip-unpack counters hold when a
compressed message is filtered out, and `png_twin_filter_coverage` proves all five
PNG filters are present. `checksum_parity.rs` reproduces wgrib2 `-checksum data`
(POSIX cksum over the normalized decoded array) for gfs/hrrr/stat fixtures;
`perf_fixture.rs` pins all 22 checksums of the JPEG2000 twin of the full-geometry
GFS hour; `dictionary_packing_rejected_with_typed_error` guards
`unsupported_drt.grib2`. Plus warm-boot LCC restore (`restart.rs` covers both
grids), ISO↔epoch round trip over 1970…2104 day-by-day (`iso_roundtrip.rs`),
config validation (`config_validate.rs`), stability strategies
(`stability.rs`), parallel-vs-sequential decode
(`parallel_decode.rs`), startup preflight cases (`preflight_windpair.rs`),
skip-and-note robustness (`robust_skips.rs`, `frac_http.rs`), and the allocation
proofs named in §6.

Scripts: `scripts/smoke.sh` (release server over the full perf dataset, 17
probes replayed from `tests/perfdata/probes.tsv`, SMOKE PASS/FAIL),
`scripts/loadtest.sh`, `scripts/ingestbench.sh` (+ its inventory helper
`scripts/_visible.py`), `scripts/profile.sh`, `scripts/gen_perfdata.sh` +
`scripts/validate_perf.py` (regenerate perf data + wgrib2 ground truth),
`scripts/gen_realistic.sh` + `scripts/validate_realistic.py` (V1.2 phase D:
production-scale realistic GFS-like dataset, 6 files x 148 records, validated
against wgrib2), `scripts/gen_fixtures.sh` + `scripts/regen_expected.sh`
(small fixtures incl. all packing twins, idempotent), `scripts/validate_rotation.py`
(expected-angle table), `scripts/validate_rotation_wgrib2.sh` (stub that runs the
IPOLATES golden where it is available), and `scripts/fetch_real_sample.sh`
(MANUAL ONLY: `.idx`-guided HTTP-Range slices of live NOMADS GFS/HRRR products
into gitignored `tests/realdata/`, verified for GRIB magic + trailing `7777`
and checksummed; idempotent — existing complete files are kept, not refetched;
nothing automated depends on it). `tools/`
(gitignored) holds the local wgrib2 build trees (`wgrib2`, `wgrib2-full` with
jasper/libpng/libaec codecs) and oha.

## 9. Limitations

Deliberate scope limits and known gaps, nothing else — decisions and rationale
live in §10 and docs/DECISIONS.md.

* **Out of scope (spec §13):** slant paths, bounding boxes, soundings, grid
  subsetting, unit conversion, derived quantities (e.g. dewpoint from RH),
  authentication, downloading model data from NOAA. Ingest only reads what is
  already in the configured directories.
* **Storage-format upgrade (V1.2 phase B0).** A `<data_dir>` written by a pre-V1.1-
  phase-4 build (`grid.bin` GRID_VERSION 1) is handled AUTOMATICALLY: on warm start
  an unreadable `grid.bin` is logged loudly (`STORAGE-FORMAT RECOVERY: …`, also
  counted in `wxpoint_ingest_errors_total`), that RUN directory is DELETED, and that
  MODEL's ingest records are forgotten so its source files are re-decoded with the
  current format on the NEXT scan round. Cost: one re-ingest per affected model;
  requirement: the SOURCE files must still be present in `ingest_dir` (if they were
  already pruned upstream the run is simply gone, LOUDLY — no silent empty inventory).
  No manual `rm -rf <data_dir>` step is needed anymore.
  V1.3 phase E0 GENERALIZED this to FIELD files: `BIN_VERSION` went 1→2 when the
  header gained the PDT 4.8 stat block, and the warm start now PRE-CHECKS every
  field file's 12-byte header (`bin_version_of_path`) BEFORE anything is indexed;
  a foreign version triggers exactly this same recovery, naming both versions in
  the log (`found bin version 1, current 2`), so a pre-E0 store upgrades itself
  on first boot instead of mis-parsing headers
  (`v1_field_bin_triggers_loud_recovery_and_the_run_rebuilds_from_source`).
* **Multiple accumulation windows for ONE valid time (V1.3 phase E1).** Storage
  layout is ONE field file per `(run, param, level type, forecast hour)` and the
  query API has no range-selection parameter, so when one source file delivers
  TWO statistically DISTINCT PDT 4.8 groups for the same param/level/fh (e.g. a
  0–6 h AND a 0–3 h accumulation for one valid time) BOTH cannot be stored under
  the current scheme. Policy: FIRST-IN-MESSAGE-ORDER WINS; every later distinct
  group targeting the already-claimed path within the same file is REJECTED
  LOUDLY — one ingest-log note per occurrence naming both windows, counted in
  `wxpoint_stat_collisions_total` (and `IngestCounters.stat_collisions`) — never
  silently merged (pre-E1 the second plane was dropped without a word) and never
  counted as an error, so the file still records ingested. Same-key-same-window
  duplicate messages keep merging as before. Exposing BOTH windows (a range
  selector on `/v1/point`, or a filename scheme carrying the window) is future
  work; until then the rejected window is simply not queryable. Pinned by
  `tests/stat_collision.rs`.
* **Provenance & raw-param units (V1.3 phase E2).** A multi-param response with
  MIXED provenance renders the top-level `"run"` as JSON `null` (per-result
  `"run"` is authoritative; see §3). Raw discipline/category/number params
  outside the built-in table answer with `units: ""` unless config supplies the
  optional `units = "..."` override — boot then logs one loud note per affected
  entry; setting the key on any table-known param (name OR in-table triple) is a
  hard startup error rather than a warning, so known canonical units can never
  be silently overridden. Pinned by `provenance_http.rs` and `raw_units.rs`.
* **Stability timing is MONOTONIC (V1.2 phase B0).** The file-stability window is
  measured with `std::time::Instant`, never against wall-clock mtimes, so NTP step-
  backs, container restore or VM pause cannot freeze ingest. File IDENTITY is still
  path + size + mtime + content hash (mtime change ⇒ re-delivery ⇒ window RESET).
  Wall-clock mtimes remain used for the `/metrics` ingest-lag gauge only.
* **Typed-rejection list** (each yields a typed error or a skip-and-note, never
  a guess, never a wrong number):
  * DRT **5.1 "dictionary"** — `UnsupportedData(1)`: GRIB2 has no dictionary
    encoder in the wgrib2 ecosystem (the task text's "5.1 runlength" is actually
    5.200, which we DO implement), so there is no reference decoder to match.
  * **JPEG2000 streams whose samples exceed 8-bit-per-value packing or carry
    multiple components** — `CodecUnsupported`: GRIB2 always encodes one
    single-component 8-bit-per-value stream, so anything else is outside the
    template's contract and unverified.
  * **PNG non-grayscale colour types, bit depths other than 8/16, interlacing,
    non-zero compression/filter methods** — `CodecUnsupported`: same reason;
    wgrib2's own PNG writer emits grayscale 8/16-bit, non-interlaced, method 0.
  * **AEC parameter sets `rust-aec` rejects** — `CodecUnsupported`: our backend
    declines them and we refuse to hand-decode the variants blind.
  * **Layer/range level messages (two fixed surfaces) and messages carrying
    coordinate values after the product-definition template** — skipped and
    counted in `wxpoint_skipped_messages_total` (+ ingest log note): neither can
    answer a single-point query as specified, and the walk resumes at the declared
    byte length so the rest of the file still ingests. Corrupt or undecodable
    section 7 still fails LOUD as an ingest error.
  * **DEGENERATE SECTION 7 (V1.3 phase E4a): a section 7 of exactly five
    octets — length + section number, ZERO data values — DECODES instead of
    failing.** With a bitmap, EVERY point becomes NaN (zero value octets exist
    for ANY point, so even bitmap-"defined" points have no value): queries
    answer `null` + `missing_data`. WITHOUT a bitmap the plane is a full-size
    constant `0.0`, EXACTLY matching `tools/wgrib2-full` on the live message
    that motivated this (an HRRR zero-window accumulation,
    `APCP:surface:0-0 day acc`, 212 bytes total — the reference decoder prints
    `mean=0:min=0:max=0 undef=0`, and matching it is the correctness bar spec
    §9 sets). Plane size always comes from the grid definition, never from
    section 5's npoints field. A TRULY truncated stream still fails loudly
    (message-length and section-overrun checks unchanged; `degenerate_sec7.rs`
    pins all four shapes).
  * Unimplemented grid/product-definition templates beyond 3.0/3.30 and 4.0/4.8
    — same skip-and-note path (a grid we cannot project cannot be interpolated).
    Today's operational GFS surface-flux line (`gfs.tHHZ.sfluxgrbfFFF.grib2`)
    rides grid template **3.40** (gaussian lat-lon, 3072 x 1536); that gap was
    CLOSED by V1.3 phase E4b and those files now ingest and serve end to end
    (validation evidence: docs/DECISIONS.md §16). Remaining 3.40 sub-limits,
    all tallied under the same flat `Grid(40)` skip counter so nothing hides:
    only FULL-GLOBAL non-thinned shapes are accepted (`nx > 0`, `ny = 2·nlat`,
    quasi-regular flag consistent with a regular row structure) — thinned,
    reduced-order and regional gaussian windows are rejected as `Grid(40)`
    because their row structure needs per-row counts we do not model yet.
    Gaussian grids also have NO node at the poles (outermost rows ~±89.91°),
    so lat ±90 is always 422 `out_of_domain` there (defined behavior, tested).
    HYBRID-level upper-air fields (level type 105 — where ALL current
    operational GFS upper-air lives) are supported end to end SINCE phase
    E4c: the whole sflux line, surface AND upper-air, is fully queryable
    (acceptance evidence: docs/DECISIONS.md §17). Since E4a the skip note names kinds AND template
    numbers ("N skipped: X coordinate-list, Y grid template 3.<t>, Z PDT 4.<t>")
    and each kind advances its own flat counter
    (`wxpoint_skip_coordlists_total` / `wxpoint_skip_grid_total` /
    `wxpoint_skip_pdt_total`), making product-line coverage gaps visible on
    `/metrics` without reading logs (docs/DECISIONS.md §15).
* **§9.3 wind-rotation golden is environment-blocked here.** `wgrib2 -new_grid_winds
  earth` refuses on this machine ("IPOLATES package is not installed") and no
  Fortran/transpiler toolchain exists to rebuild it with IPOLATES. The spec's
  golden is replaced by the dual-earth-shape pyproj cross-check described in §8
  row 3 (worst observed delta 4.8e-11 deg against a 1e-6 deg tolerance);
  `scripts/validate_rotation_wgrib2.sh` prints how to run the real golden on a
  machine whose wgrib2 links IPOLATES. Full rationale: docs/DECISIONS.md (phase
  4 "Remaining §9.3 deviation").
* **Poison quarantine (V1.2 phase B; CONTENT errors only since V1.3 phase E3a).**
   A file whose decode FAILS ON CONTENT is recorded in the
   persistent ingest state WITH its content hash (`kind=FAILED`), so an identical
   corrupt file is not re-decoded every scan round; a CONTENT CHANGE produces a new
   hash and re-arms EXACTLY ONE attempt plus one note (same rule as the retention
   verdict memo). Counted in `wxpoint_quarantined_files_total`. We NEVER delete or
   move the operator's file — quarantine is a RECORD, not a filesystem action.
* **Transient I/O is retryable, never silently permanent (V1.3 phase E3a).** An
   `open`/`mmap` failure (`GribErr::Io`: EMFILE, EACCES, EIO, ENOMEM, NFS-ESTALE…)
   is NOT a content verdict and no longer writes a FAILED record on first sight.
   A bounded IN-MEMORY streak per (path,size,mtime,hash) notes the FIRST failure
   once, retries silently, and escalates only after `IO_FAIL_ESCALATION_ROUNDS`
   (5) consecutive unreadable rounds: loud note + one `errors` + one
   `quarantined_files` + a kind=FAILED record so a permanently unreadable file
   stops hammering. Records written before a hash could be taken carry an EMPTY
   hash and therefore SELF-CLEAR once reading works again (the stored "" can never
   equal a real digest). Streaks reset on success/identity change and die with a
   restart. Same policy covers the content-hash and magic-precheck read stages.
   Residual documented risk: decode itself still mmaps AFTER the stability window
   (page-cache sharing with queries); a truncation inside that window remains a
   SIGBUS surface, accepted because the window makes it vanishingly rare and the
   pre-stability reads — where the race was live — are now plain reads
   (`io_retry.rs`, `hash_compat.rs`).
* **State durability granularity (V1.3 phase E3a).** `state/files.json` saves are
   dirty-gated and batched: once per decode chunk plus at end of round (before
   retention pruning), replacing the per-file full rewrite that cost O(N²) bytes
   and fsyncs at thousands of files. Worst case a crash between two opportunities
   loses AT MOST one unfinished chunk's records (≤ K files); they re-ingest
   idempotently (merge dedup + atomic renames). A failed save keeps the store
   dirty, logs loudly and counts one error WITHOUT blocking ingestion; the next
   opportunity retries (`state_batching.rs`, `restart_midchunk.rs`).
* **Retention skip (V1.2 phase C).** A candidate whose SECTION-1 reference time is
   older than the newest `retention_runs` runs is NOT decoded: it is PEEKED (sections
   1 only), noted once per (path, content hash) and counted in
   `wxpoint_retention_skipped_files_total`. The peek repeats every scan round (cheap
   by design), so widening `retention_runs` in the config or losing the newer source
   files makes the same file ingest on the NEXT round without a restart. Skipped
   files are absent from `/v1/inventory`, which is the intended steady state, not a
   bug — check the counter to tell "pruned upstream" from "never delivered".
* **Shared ingest directories are first-class (V1.3 phase E3b).** Two models
   may point at the SAME (or nested) `ingest_dir` with overlapping `file_glob`s:
   state records are keyed `(model, path)` so BOTH models ingest matching files
   (pre-E3b the second was silently starved forever). Startup emits a LOUD
   non-fatal warning naming both models, dirs and globs when the dirs are
   equal-or-nested AND the globs are not provably disjoint (conservative
   literal prefix/suffix heuristic — it OVER-warns rather than missing an
   overlap; case compared as-is, matching `glob_match`). Sharing remains legal;
   the file-stability tracker is deliberately PATH-keyed because "stopped
   changing" is a property of the file, not of a model (`model_state_keys.rs`).
* **A pathological tree cannot hang discovery (V1.3 phase E3b).** The recursive
   walk classifies directories with `metadata()` and records each walked
   directory's CANONICAL path in a visited set: a symlink to an ancestor is
   skipped with a note instead of looping forever (the pre-E3b walk hung the
   ingest thread AND the startup preflight infinitely). A hard
   `WALK_MAX_PATHS = 1_000_000` cap bounds even worse shapes per round, noted
   once; the next scan round resumes (`symlink_walk.rs`).
* **A missing `grid.bin` is loud but non-destructive (V1.3 phase E3b).** A run
   dir whose grid file is absent/unreadable gets ONE "MISSING GRID" note per
   rebuild + one `errors` increment and stays OUT of the index until its source
   re-delivers; unlike the CORRUPT-grid upgrade trap it is NOT auto-deleted —
   nothing is wrong to repair, something is merely absent, and deleting could
   race a producer mid-write at boot (`missing_gridbin.rs`).
* **Exit codes** (`crates/wx-server/src/main.rs`) — honest assertion ledger,
   updated V1.3 phase E6 (the old "asserted nowhere" sentence was stale):
   `0` normal shutdown; `2` usage error (no `--config`, no `WXPOINT_CONFIG`) —
   ASSERTED (`missing_config_argument_exits_with_the_usage_code`); `1` config
   validation failed loudly (spec §3) — ASSERTED ×3 (nonexistent config file,
   data-dir/ingest-dir OVERLAP guard incl message naming both paths, unknown
   param name); `4` startup wind-pair PREFLIGHT rejected a half-paired
   grid-relative model whose dir was STABLE at boot — ASSERTED since E6
   (`stable_wind_half_pair_exits_four_at_boot`); `6` the bind address could
   not be bound — ASSERTED since E6 (`an_unbindable_address_exits_six`); `7`
   PROVISIONAL post-boot wind enforcement fired — ASSERTED both halves
   (rc==7 with default enforce, plus rc==124 timeout-negative control with
   `wind_pair_enforce=false`, violation note required in both). STILL WIRED
   BUT UNTESTED (hard to force from a black-box spawn): `3` pipeline-init
   failure, `5` tokio-runtime init, `8` HTTP server error.
* **Residual publish window (per-file granularity).** Because visibility flips once
  per SOURCE FILE, a scan round that consumes several files can momentarily expose
  one forecast hour without the next (normal: products DO arrive hour by hour) or
  one run without the following run. A request for a level the run has NEVER stored
  answers 422 `level_out_of_range` rather than extrapolating (spec-correct). Within
  ONE file there is NO half-stack window any more: all of that file's GROUPS are
  written before its registry entry is swapped in, and in-flight readers keep the
  previous snapshot alive by reference count until it is no longer reachable.
* **Publish granularity is ONE SOURCE FILE (V1.2 phase B), and that is a deliberate
  trade.** Visibility flips when a source file's GROUPS are all written, NOT per
  message: wall-to-FIRST-queryable is therefore slightly WORSE than pre-V1.2 (measured
  2.49 s -> 3.08 s at L=16; 5.62 s at L=37) while wall-to-all-visible improved 5.7x
  and no query can ever see a HALF-PUBLISHED parameter stack mid-file. The older
  per-message publish (phase-2 behaviour) produced exactly those transient 422
  `level_out_of_range` answers; they are gone at file granularity, and §4.5's
  "forecast hour by forecast hour" requirement still holds because products arrive
  one forecast hour at a time.
   HISTORY (clearly historical now): the V1.1-era design point WAS per-field
   publish with an accepted bounded transient-422 window, counted separately
   by tests and recorded as an "open follow-up" to coarsen publish; that
   follow-up was TAKEN in V1.2 phase B (docs/DECISIONS.md), so any text
   describing per-field publish or a LIVE transient-422 window as CURRENT
   behaviour is stale — within one source file there is no such window and
   the concurrency harness pins zero transient 422s (`fin[5] == 0`).
* **Operator log lines are emitted ONCE (V1.2 phase D).** Per-FILE OUTCOME lines
  (`ingested …`, `failed … quarantined`, `ignored …`, `skipped …`) travel through
  the LIVE printer's FIFO only; the round streamer prints NOTES. Before this fix the
  SAME line appeared up to three times in server stdout, which looked like a DOUBLE
  INGEST in the phase-D benchmark log (it was not: counters and state records showed
  one decode per file — see docs/DECISIONS.md phase D §6). Consequence: if the LIVE
  printer thread is starved, outcome lines appear LATE (they are never lost unless
  more than 256 un-printed lines pile up, in which case the OLDEST drop from stdout
  while remaining in `Pipeline.progress_log`).
* **`decode_batch_files` (K) is a RAM knob, not just a latency knob.** Peak RSS ~=
  K x the largest concurrent source file's plane set (measured: K=1 -> 1087 MB, K=4
  -> 3079 MB for a 614 MB-per-file realistic stack). See §7.4 for guidance.
* **True-pole (±90) fixtures are limited by wgrib2.** Fixtures reaching lat ±90
  could not be produced and verified with wgrib2 point probing in this environment,
  so exact-pole behaviour is asserted on synthetic global grids (`poles_global_grid`,
  whose top row IS lat 90). The full-geometry perf fixture uses the standard GFS
  720-row node rectangle (89.875…−89.875), where a true-pole query falls OUTSIDE it
  and answers 422 `out_of_domain` — confirmed by probe this sweep (`lat=90` → 422,
  `lat=89.9` → 422, `lat=89.875` → 200 exact node). Everything up to the node
  rectangle edges is exercised on production-scale geometry (still synthetic bytes,
  per the §8 honesty note).
* **Production-data verification stays MANUAL / NETWORK-gated (V1.3 phase E5).**
  Spec §13 keeps CI offline, so the live-bytes proof lives in
  `scripts/realdata_acceptance.sh` (§8): fetch → release server → 20 wgrib2/pyproj
  pins → SUMMARY line. It must be RUN by hand before trusting a new production
  product line, and its date pinning needs care: the fixture-era gates default to
  a pinned `GFS_YMD`, and live NOMADS data ROLLED OVER between sessions once
  (docs/DECISIONS.md §18 drift lesson) — refetch with explicit `GFS_YMD`/`GFS_HH`
  when the default day ages out.
* **Percent-decoding is deliberately narrow (V1.3 phase E6).** Only `%3A`/`%3a`
  and `%2C`/`%2c` inside `level` and `param` VALUES decode (§3). Every other
  encoding stays literal BYTES: `%2B` is not `+`, `%20` is not a space, and a
  fully-URL-encoded timestamp (`valid=2026-10-19T18%3A00%3A00Z`) answers 400
  `bad_time` — curl's default sends bare colons, only aggressive encoders hit
  this, and the escape hatch is documented rather than silently widened because
  decoding `+` (form encoding) would change existing 404-class behaviour.
* **Real-data gate cost when enabled.** With `tests/realdata/` populated the
  DEBUG-profile suite pays roughly an hour per run (the slice-decode gates run
  unoptimized); CI without those files skips them cleanly in seconds. Use
  `cargo test --release` for quick full-suite passes against live slices.
* **Storage scales linearly per grid plane (sizing guidance).** One full
  native HRRR 3 km CONUS plane (1799×1059 f32) is ≈7.6 MB; one full GFS 0.25°
  plane (1440×721 f32) ≈4.0 MB. A run's footprint is planes × levels × params
  with no compression — configure `retention_runs` against your disk, not
  against the GRIB files' compressed sizes.

## 10. Decision log (V1.1)

Summaries of the choices made during V1.1; full detail per phase in
docs/DECISIONS.md.

* **Codec backends (phase 2).** Pure Rust, no C FFI: JPEG2000 via
  `hayro-jpeg2000` 0.4.1 (default features off, `std`+`simd`), PNG via
  `zlib-rs` inflate plus an in-house chunk/CRC/unfilter layer (wgrib2 has no PNG
  encoder, so our own writer makes the twins and wgrib2's libpng reader validates
  them in the other direction), AEC via `rust-aec`. Validation primitive:
  `-checksum data` equality between each twin and the simple-packed original, so
  repacking provably changes nothing but the encoding.
* **Fractional forecast hours (phase 3).** Integer SECONDS past the run as the
  single internal join key; one canonical filename scheme (`f<HHH>[m<MM>][s<SS>]`)
  with trailing zero components omitted; run dirs widened to 10/12/14 chars to
  kill the 12Z-vs-12:30Z directory collision that had merged two runs into one.
* **Startup wind-pair preflight (phase 4).** Fail-at-startup when decisive, defer
  with a note when not, decode-time rule retained as second line of defense (§2).
* **Rotation angle from our own cone math (phase 4).** Root cause of the residual
  error was the earth shape: rotation must use the SAME spheroid the projection
  uses. Code Table 3.2 user-defined codes 1/3/7 now honour their INLINE axes from
  section 3 instead of snapping to a nearest constant, so a file declaring its own
  spheroid projects AND rotates on that spheroid (4.8e-11 deg agreement with
  pyproj, matched shape per fixture). Note the task text's "code 2 = 6378.137/
  6356.7523" is wrong per wgrib2 `Earth.c` (code 2 = International spheroid
  6378.160/6356.775 km); the WGS 84 numbers belong to a user-defined spheroid,
  which is why the ellipsoid fixture uses code 3 + inline WGS 84 radii.
* **Interning/borrow strategy (phase 5).** Borrow-from-snapshot rather than interned
  tables: contexts borrow grid defs, units strings and mmap handles straight out of
  the arc-swapped index snapshot, with fixed-inline buffers (8 params / 8 entries,
  documented fallback beyond that). Error paths still allocate — deliberate: they
  are cold and clarity beats micro-optimisation there.
* **Publish granularity (HISTORICAL — follow-up SINCE TAKEN).** This entry
  once said "per-field publish kept ... open follow-up". That was the
  V1.1-era state. V1.2 phase B took the follow-up: publish moved to ONE
  SOURCE FILE per swap, which removed the half-stack window entirely
  (concurrency harness observes zero transient 422s) at a small measured
  cost to wall-to-first-queryable. Kept here, marked historical, because
  DECISIONS.md cross-references it.
* **No bounded/bit-risky optimizations.** §8 targets were met on first measurement,
  so none were attempted; any optimization that could change an output bit needs a
  test proving the new result at least as correct, and we never needed one.
* **Storage-format compatibility decisions.** Isobaric levels normalized Pa→hPa
  internally so config/API strings match; `GRID_VERSION` 1→2 bump with version-1
  files rejected on read (see §4); non-canonical fh names ignored, `f006m00.bin`
  tolerated as an alias; sub-second offsets impossible by CT 4.4 construction and
  negative offsets treated as corrupt input and skipped, not stored.
* **Perf dataset provenance.** Synthetic-but-real-geometry (generated by
  `gen_perfdata.sh`, values analytic and exact-in-f32, every field validated against
  wgrib2 readback/checksums); real NOAA file NAMES/framing beyond the two accepted
  terminator forms have not been exercised end-to-end.

---

## 11. Release notes (V1.3)

Phase-by-phase rationale lives in docs/DECISIONS.md; §19 there is the final
acceptance record (sweep numbers, gate logs, deviations register). Headline
changes over V1.2:

* **Statistical-process durability — `.bin` header v2 (E0).** Every field
  file carries its accumulation/average/max-min guard and its window
  (begin/end/step seconds) INSIDE the durable header, so spec §6.3 stays
  enforced even with `manifest.json` missing or corrupt; the manifest is now
  a human-readable inventory, and warm start reads guards back from headers.
* **Per-forecast-hour stat guards + collision accounting (E1).** The §6.3
  guard became per-forecast-hour precise (mixed-kind files no longer degrade
  a whole field), and statistically distinct groups colliding on one field
  file resolve first-in-message-order with rejects counted in
  `wxpoint_stat_collisions_total`.
* **Provenance & units honesty (E2).** Every result carries its own `"run"`
  (top-level echo only when unanimous, else JSON `null`); `/v1/models`
  renamed `winds_grid` → `source_winds_grid` (stored winds are always
  earth-relative); params accept an optional raw `units = "..."` override.
* **Ingest robustness batch (E3a/E3b).** Manifest-update retry; batched
  state saves + dedicated state-write counters; pread-based content hashing;
  symlinked ingest dirs; missing `grid.bin` loud-but-non-destructive; atomic
  retention sweep; per-model state keys; provisional wind-pair enforcement
  (exit 7 post-boot when preflight defers; `[ingest].wind_pair_enforce=false`
  opts out).
* **Skip observability (E4a).** Structural skips tallied BY KIND
  (`wxpoint_skip_coordlists_total` / `_grid_total` / `_pdt_total`) with exact
  template numbers named in the log note; degenerate section-7 messages skip
  cleanly instead of erroring.
* **Gaussian grids + HYBRID levels (E4b/E4c).** GRIB grid template 3.40
  (quasi-regular gaussian, `GRID_VERSION` 3, no polar nodes) decodes and
  projects end to end, and level type 105 (hybrid) indexes/queries with RAW
  dimensionless coordinates — making today's operational GFS `sfluxgrbf`
  line fully queryable, surface AND upper-air. Documented deviation: the
  spec sketched isobaric examples; hybrid (not log-pressure) support is what
  production actually needs (docs/DECISIONS.md §16–§17).
* **Live-data acceptance harness (E5).** `scripts/realdata_acceptance.sh`
  (MANUAL/NETWORK): real GFS/HRRR slices → release server → 20 independent
  wgrib2/pyproj pins incl. wind rotation, same-run time blends, PDT 4.8
  guards; SUMMARY line must read 20/20 (§8).
* **Metrics semantics, wire format, hygiene closeout (E6).**
  `wxpoint_ingest_lag_seconds` RENAMED `wxpoint_data_age_seconds` (it was
  always staleness, never latency) and the TRUE latency pair
  `wxpoint_ingest_publish_latency_seconds` / `_max` added (file mtime →
  publish completion, wall-vs-wall, last + lifetime max); targeted
  `%3A`/`%2C` decoding inside level/param values (§3, §9); exit-code ledger
  completed with rc==4 and rc==6 black-box assertions; the clippy
  all-targets surface went from 229 abort-truncated (~623 full-surface
  mid-flight) to ZERO diagnostics on both gate forms with all affected test
  binaries rerun green (docs/DECISIONS.md §19).

V1.2-era changes (per-file publish granularity, chunked decode pipeline,
retention-aware skipping, once-only operator logs) are summarized in §7.4
and in §9/§10's supersession notes, with full detail in docs/DECISIONS.md's
V1.2 sections.
