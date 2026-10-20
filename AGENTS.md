# Task: Weather Model Point-Query Server (Rust)

## 1. Goal

Build a Rust application that:

1. **Ingests** gridded weather model output (GRIB2) for GFS, HRRR, and future models from configured directories.
2. **Saves** only a configured list of parameters (e.g. geopotential height on isobaric levels, 2 m temperature) into a fast, query-optimized on-disk format.
3. **Serves** an HTTP API that returns the value of a parameter for a given **model, parameter, level, lat/lon, and valid time**, interpolating linearly when the request falls between grid points, levels, or forecast times.

**Priorities, in order:** correctness first (a fast wrong answer is a bug), then query latency, then everything else. Within those, latency matters more than ingest speed, memory, or disk use.

**Scope:** point queries only. Do **not** implement slant paths, soundings along a path, bounding boxes, or grid subsetting. Design so these could be added later, but do not build them.

---

## 2. Definitions

| Term | Meaning |
|---|---|
| Model | A named source such as `gfs` or `hrrr`. Each has its own grid, projection, and ingest directory. |
| Run / cycle | Model reference time (e.g. 2026-10-19 12Z). |
| Forecast hour | Lead time from the run. |
| Valid time | Run time + forecast hour. This is the "time on target" the user queries by. |
| Parameter | A physical field, identified internally by GRIB2 discipline/category/number (e.g. `TMP`, `HGT`, `UGRD`). |
| Level | Level type + value, e.g. `isobaric:850` (hPa), `height_above_ground:2` (m), `surface`, `msl`. |

---

## 3. Configuration

A single TOML file (path via CLI flag or env var). Example:

```toml
[server]
bind = "0.0.0.0:8080"
data_dir = "/var/lib/wxpoint"        # where ingested, query-optimized data lives

[ingest]
scan_interval_secs = 30
retention_runs = 4                   # keep the N most recent runs per model

[[models]]
name = "gfs"
ingest_dir = "/data/incoming/gfs"
file_glob = "gfs.t*z.pgrb2.0p25.f*"

  [[models.params]]
  name = "HGT"
  levels = ["isobaric:1000", "isobaric:925", "isobaric:850", "isobaric:700",
            "isobaric:500", "isobaric:300", "isobaric:250", "isobaric:200"]

  [[models.params]]
  name = "TMP"
  levels = ["height_above_ground:2", "isobaric:*"]   # "*" = every isobaric level present

  [[models.params]]
  name = "UGRD"
  levels = ["height_above_ground:10", "isobaric:*"]

  [[models.params]]
  name = "VGRD"
  levels = ["height_above_ground:10", "isobaric:*"]

[[models]]
name = "hrrr"
ingest_dir = "/data/incoming/hrrr"
file_glob = "hrrr.t*z.wrf*f*.grib2"

  [[models.params]]
  name = "TMP"
  levels = ["height_above_ground:2", "isobaric:*"]
```

Requirements:

- Parameter names map to GRIB2 (discipline, category, number) through a built-in table in code. Include at least: `TMP`, `HGT`, `UGRD`, `VGRD`, `RH`, `SPFH`, `DPT`, `PRMSL`, `MSLMA`, `PRES`, `VVEL`, `APCP`, `GUST`, `CAPE`, `CIN`. Allow the config to specify raw `discipline/category/number` for anything not in the table.
- Level type names map to GRIB2 Code Table 4.5 values (100 isobaric, 103 height above ground, 1 surface, 101 MSL, etc.).
- Validate config at startup and fail loudly on unknown names.

---

## 4. Ingestion

### 4.1 Discovery
- Watch each model's `ingest_dir` (use the `notify` crate) **and** do a periodic full scan as a fallback, since filesystem events are unreliable on network mounts.
- Files may still be in the middle of being written. Only ingest a file once its size and mtime have been stable for a configurable period (default 10 s), or once a sidecar marker appears if the upstream process writes one. Make the strategy configurable.
- Keep a persistent record of ingested files (path + size + mtime + content hash of first/last 64 KiB is fine) so restarts do not re-ingest, and so a re-delivered file with changed content *is* re-ingested.

### 4.2 Decoding
- Decode GRIB2. Evaluate the `grib` crate (grib-rs) first. Confirm it supports every data representation template actually present in sample GFS and HRRR files (at minimum: simple packing 5.0, complex packing with spatial differencing 5.3, JPEG2000 5.40, PNG 5.41). If a needed template is unsupported, either contribute/implement the decoder or fall back to calling `ecCodes` via FFI. Document the decision.
- **Never trust filenames for metadata.** Read reference time, forecast time, parameter, and level from GRIB sections 1 and 4. Filenames are only used for the glob filter.
- Skip messages not in the configured parameter list without fully decoding them (read section 4 first, then decide whether to unpack section 7).
- Handle the bitmap (section 6): missing points must be stored as `NaN`.
- Handle both product definition templates 4.0 (instantaneous) and 4.8 (statistically processed: accumulations, averages, max/min). Store the statistical process type and time range; it affects time interpolation (see 6.3).
- Ingest decoding should be parallel across messages/files (`rayon`), but must not starve the query path. Run ingest on a dedicated thread pool with a configurable thread count, separate from the HTTP runtime.

### 4.3 Grids and projections
Store the grid definition (section 3) per model run and support at least:

- **Template 3.0, regular lat/lon** (GFS 0.25°, 1440 × 721). Longitudes run 0 to 359.75. Interpolation must wrap across the 359.75 → 0 seam. Respect the scanning mode flags (GFS scans north-to-south).
- **Template 3.30, Lambert Conformal Conic** (HRRR 3 km CONUS, about 1799 × 1059). Implement the forward LCC transform (lat/lon → fractional grid i, j) from the template's parameters: La1, Lo1, LoV, Latin1, Latin2, Dx, Dy, and the earth shape from Code Table 3.2. Precompute projection constants once per grid.
- Normalize input longitudes so that both `-180..180` and `0..360` are accepted.

### 4.4 Wind rotation (accuracy-critical)
HRRR wind components are typically **grid-relative**, not earth-relative (check the resolution and component flags, bit 5 of flag table 3.3). If a model's U/V are grid-relative and both components are configured, rotate them to earth-relative at ingest using the LCC rotation angle at each grid point. Store earth-relative U/V so queries are uniform across models. If only one component is configured for a grid-relative model, fail config validation with a clear message.

### 4.5 Storage format
Decode once, store as raw `f32` (GRIB packing precision is already below f32 precision), and memory-map at query time (`memmap2`). Suggested layout:

```
data_dir/
  <model>/
    <run YYYYMMDDHH>/
      grid.bin             # serialized grid definition + projection constants
      manifest.json        # what is present: params, levels, forecast hours, units, stat-process info
      <param>/<level_type>/f<FFF>.bin
```

- Each `.bin` file: a small fixed header (magic, version, nx, ny, level count, level values) followed by `f32` little-endian data laid out `[level][y][x]`, with levels sorted by pressure.
- Write to a temp file, `fsync`, then atomic rename. A query must never see a partial file.
- A run becomes visible to queries forecast hour by forecast hour as each completes. Do not wait for the whole run.
- The agent may benchmark an alternative layout (for example, chunking that puts all 16 values a query needs closer together) and switch if it is measurably faster. Record the numbers in the README.

### 4.6 Retention
Delete runs beyond `retention_runs` per model, but only after removing them from the in-memory index, and only once in-flight queries can no longer reference their mmaps (reference-count the mmap handles; let `Arc` drop handle the unmapping).

---

## 5. In-memory index and concurrency

- Keep a read-mostly index: model → runs → params → levels → forecast hours → mmap handle.
- Publish it with `arc-swap` (or similar) so query threads read with no locks. Ingest builds an updated index and swaps it atomically.
- All mmaps for available data are opened at index build time, not per request.
- The query hot path should do **no heap allocation** apart from building the response body, no locking, and no file I/O beyond page-faulting mmapped pages.

---

## 6. Interpolation (accuracy-critical)

A point query needs a value at (lat, lon, level, valid_time). Interpolate in this order, each step linear.

### 6.1 Horizontal: bilinear
- Convert lat/lon to fractional grid coordinates (i, j) with the model's projection.
- Bilinearly interpolate from the 4 surrounding grid points.
- If the point is exactly on a grid node (within 1e-9 in grid units), return that node's value exactly, with no floating-point blending.
- If the point is outside the grid (including the last half-cell at a non-wrapping edge), return an error. Never extrapolate horizontally.
- If any of the 4 neighbors is `NaN` (masked/missing), return `null` with reason `missing_data`. Do not silently renormalize weights.

### 6.2 Vertical: linear between levels
- Only for level types with numeric, orderable values (isobaric, height above ground with multiple levels, etc.).
- If the requested level exists, use it directly.
- Otherwise find the bracketing stored levels and interpolate **linearly in the level coordinate** (in pressure for isobaric levels). This is the specified behavior. Implement it so a `log-pressure` option could be added later, but default to linear and do not add log-p unless asked.
- If the requested level is outside the stored range, return an error. Never extrapolate.
- Note that a configured level list with gaps (e.g. only 850 and 500) will interpolate across the gap. Return the bracketing levels in the response so users can see that.

### 6.3 Time: linear between forecast hours
- The user supplies a valid time. By default, select the **most recent run** that has data for that valid time (both bracketing forecast hours if interpolating). An optional `run` parameter pins a specific run.
- If the valid time matches a stored forecast hour, use it directly.
- Otherwise, linearly interpolate between the two bracketing forecast hours **from the same run**. Never blend across different runs.
- For statistically processed fields (accumulations, averages, max/min from template 4.8), **do not interpolate in time**: require an exact match and otherwise return an error explaining why. Linearly interpolating an accumulation produces wrong answers.
- Support `time_interp=none` to require an exact match for any field.

### 6.4 Order and precision
Do horizontal interpolation at each of the (up to) 2 levels × 2 times, then vertical, then time. Do the arithmetic in `f64` and return `f64`.

---

## 7. HTTP API

Use `axum` on `tokio`. JSON responses. All times are ISO 8601 UTC.

### `GET /v1/point`

Query parameters:

| Param | Required | Example | Notes |
|---|---|---|---|
| `model` | yes | `gfs` | |
| `param` | yes | `TMP` | Comma-separated list allowed (`TMP,HGT`); all share the point, level and time. |
| `level` | yes | `isobaric:850`, `isobaric:837.5`, `height_above_ground:2`, `surface` | Non-stored numeric values trigger vertical interpolation. |
| `lat` | yes | `41.25` | -90..90 |
| `lon` | yes | `-95.93` | -180..360 |
| `valid` | yes | `2026-10-19T18:00:00Z` | |
| `run` | no | `2026-10-19T12:00:00Z` | Defaults to latest run covering `valid`. |
| `time_interp` | no | `linear` (default) / `none` | |

Response (200):

```json
{
  "model": "gfs",
  "run": "2026-10-19T12:00:00Z",
  "valid": "2026-10-19T19:30:00Z",
  "lat": 41.25,
  "lon": -95.93,
  "level": "isobaric:837.5",
  "results": [
    {
      "param": "TMP",
      "value": 281.734,
      "units": "K",
      "interpolation": {
        "horizontal": "bilinear",
        "vertical": { "method": "linear", "levels": [850, 800] },
        "time": { "method": "linear", "forecast_hours": [6, 9] }
      }
    }
  ]
}
```

- Return values in native GRIB units (K, gpm, m/s, Pa) and report the units. Do not convert.
- `value` is `null` with a `reason` when data is missing/masked.

Errors use proper status codes and a JSON body `{ "error": "<code>", "message": "..." }`:
- `400` malformed input.
- `404` unknown model/param/level, or no run covers the valid time.
- `422` valid request that cannot be answered: out of domain, outside level range, accumulation time mismatch.

### Other endpoints
- `GET /v1/models`: models, grid info, configured params and levels.
- `GET /v1/inventory?model=gfs`: available runs and, for each, the forecast hours/params/levels that are queryable right now.
- `GET /health`: liveness. `GET /ready`: ready once at least one run is indexed per model.
- `GET /metrics`: Prometheus format. Include query latency histogram, query count by status, ingest lag (file mtime → queryable), ingest errors.

---

## 8. Performance targets

Measure on the deployment-class machine and record the results in the README.

- Single point query, data warm in page cache: **p50 < 50 µs, p99 < 250 µs server-side** (excluding network).
- At least 50k queries/s sustained on 8 cores with keep-alive.
- One forecast hour of GFS with ~10 configured params becomes queryable within a few seconds of the file being stable.

Provide:
- `criterion` benchmarks for projection math, bilinear lookup, and the full query function.
- A load-test script (e.g. `oha` or `wrk`) with documented commands.
- Release profile: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`. Benchmark `target-cpu=native` and document whether it is used.

Do not trade correctness for speed anywhere. If an optimization changes any output bit, it needs a test proving the new result is at least as correct.

---

## 9. Correctness verification (required, not optional)

1. **Exact grid points:** For a sample of grid points from real GFS and HRRR files, the server's value must match `wgrib2 -lon <lon> <lat>` (or ecCodes `grib_get_data`) to within f32 rounding.
2. **Projection:** LCC lat/lon → (i, j) must match `wgrib2 -ij`/`-ll` or `pyproj` for HRRR at the four corners, the center, and at least 20 random points, to within 1e-4 grid units.
3. **Wind rotation:** Rotated HRRR U/V must match `wgrib2 -new_grid_winds earth` output.
4. **Bilinear / vertical / time:** Unit tests with synthetic grids where the answer is known analytically (e.g. a field linear in x, y, p, t must be reproduced exactly at arbitrary points).
5. **GFS seam:** Queries at lon 359.9 / -0.1 / 0.0 interpolate across the seam correctly.
6. **Poles and edges:** Behavior at lat ±90 for GFS and at HRRR domain edges is defined and tested.
7. **Missing data:** Masked points return `null`, not a blended number.
8. **Accumulation guard:** Time interpolation of `APCP` is rejected.
9. **Concurrency:** A test that ingests while hammering queries and checks no request ever errors or sees partial data.

Keep small trimmed GRIB2 fixtures in `tests/fixtures/` (use `wgrib2` to extract a few messages) plus a script that regenerates the expected values from `wgrib2`.

---

## 10. Project structure (suggested)

```
Cargo.toml                 # workspace
crates/
  wx-core/                 # grid defs, projections, interpolation, storage format (no I/O frameworks)
  wx-ingest/               # discovery, GRIB decode, wind rotation, writing storage
  wx-server/               # axum app, index, endpoints, metrics, main binary
benches/
tests/
config.example.toml
README.md
```

Keep `wx-core` free of async and HTTP dependencies so it is easy to test and benchmark in isolation.

---

## 11. Milestones

1. `wx-core`: lat/lon and LCC grids, bilinear/vertical/time interpolation, storage read/write, with all synthetic tests passing.
2. GRIB2 decode of GFS and HRRR fixtures, validated against `wgrib2` (tests 1–3).
3. Ingest pipeline: discovery, stability check, parallel decode, atomic writes, manifest, retention.
4. Server: index with `arc-swap`, `/v1/point` and the other endpoints, error handling.
5. Benchmarks, load test, profiling pass, and README with measured numbers.

Each milestone should leave `cargo test` and `cargo clippy -- -D warnings` passing.

---

## 12. Deliverables

- Working workspace that builds with stable Rust.
- `config.example.toml` covering GFS and HRRR.
- README covering: setup, config reference, API reference with curl examples, storage format, interpolation semantics (exactly as in section 6), benchmark results, and known limitations.
- A list of any decisions where the spec was ambiguous and what was chosen.

## 13. Out of scope

Slant paths, bounding boxes, soundings, grid subsetting, unit conversion, derived quantities (e.g. computing dewpoint from RH), authentication, downloading model data from NOAA. Ingest only reads what is already in the configured directories.

## 14. Flag before implementing

Stop and ask rather than guess if:
- A required GRIB template or projection is not decodable by available crates and the fallback would add a heavy system dependency.
- Sample files contain grids or level types not covered here.
- Any performance target appears unreachable without weakening a correctness rule.
