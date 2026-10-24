# deploy/ — example systemd deployment

`wxpoint.service` is an EXAMPLE, not an installer. It encodes four decisions
that interact with how this binary actually behaves (all measured/committed
facts, cross-refs to the top-level README §1 gate table and §9 limitations):

* **`Restart=always` under `panic = "abort"`.** The release profile sets
  `panic = "abort"` (top-level README §1): an abort takes the whole process
  down with a non-zero/signal status. Phase B1 converted the known corrupt-
  input abort classes into typed errors (corruption sweep: 3720 seeded
  bit-flips + 186 truncation cuts, zero panics), but third-party codec
  internals still parse untrusted bytes past our guards and a page-fault
  SIGBUS on a truncated mmap inside the stability window remains a documented
  residual (README §9). `Restart=always` turns any such one-shot death into a
  sub-second recovery with zero operator action; deterministic config faults
  (exit 1/2/4/7) would loop, which is why the unit bounds the loop with
  `StartLimitIntervalSec`/`StartLimitBurst` instead of trusting luck.

* **Pre-created `data_dir`.** Since V1.4 phase B2 an unwritable
  `[server].data_dir` (or one that is a regular file) fails FAST at startup —
  the exit-3 pipeline-init path (`data_dir_guard.rs`) — rather than limping
  along half-ingesting. `StateDirectory=wxpoint` pre-creates
  `/var/lib/wxpoint` with the right owner before ExecStart, so the fail-fast
  fires on genuine misconfiguration only, not on first-boot absence.

* **The single-instance lock is ADVISORY.** `Pipeline::open` takes
  `flock(2)` on `<data_dir>/state/.wxpoint.lock`; a second PROCESS pointed at
  the same data_dir is refused at startup with a named error (exit 3). That
  protects against double-deploying the unit with different configs, but
  flock semantics on NFS-mounted data_dirs depend on mount options and lock
  daemons — do not run two instances against one shared-NFS data_dir and call
  the lock proof (§9 keeps this caveat honest).

* **Stop behaviour is graceful by design.** Since V1.4 phase A, SIGTERM and
  SIGINT make the server stop accepting new connections, drain in-flight
  requests, and exit **0** ("stopped cleanly"; pinned by `shutdown_drain.rs`).
  `systemctl stop` is therefore routine, not an event. `TimeoutStopSec=30` is
  a backstop for a wedged client connection, not the normal path.

Privilege claim, stated exactly: the service runs as an unprivileged user and
requires only READ access to each model's `ingest_dir` and READ-WRITE access to
`data_dir`. Nothing else is claimed — no capabilities, no root, no outbound
network (ingest never downloads; spec §13). The bind port must be > 1024 for
the unprivileged claim to hold (the example config uses 8080).

Log expectations for journald: boot prints `loaded models: ...; bind ...;
data_dir ...`, per-file outcome lines (`ingested/failed/ignored/skipped ...`),
and recovery notes (`STORAGE-FORMAT RECOVERY`, `GRID AUTO-HEAL`, corrupt-state
quarantine); shutdown prints the drain line then `server stopped`. Exit codes
and their meanings are in README §1.
