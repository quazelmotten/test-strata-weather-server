#!/usr/bin/env bash
# Milestone-5 profiling pass (spec §8 item 4): identify the top cost centers
# of the query hot path while the oha mix hammers the release server.
#
# This machine has perf_event_paranoid=4 and yama ptrace_scope=1, so perf/
# gdb/strace ATTACH sampling is impossible (verified: perf record fails,
# gdb attach refused). Fallback used here: scripts/profshim.c LD_PRELOADs a
# per-thread ITIMER_PROF statistical sampler (no ptrace needed); scripts/
# profile.sh symbolizes the raw instruction-pointer samples offline against
# the release binary's .symtab (nm) plus /proc/<pid>/maps for libc regions.
# On a machine with perf privileges the same mix would be profiled with
# `perf stat -p PID` + `perf record --call-graph dwarf`; we detect and say
# which path ran.
#
# Usage: ./scripts/profile.sh [seconds]        (default 12)
#        STRACE=1 ./scripts/profile.sh        # extra short pass: syscall
#                                             # accounting via strace LAUNCH
#                                             # mode (launch mode needs no
#                                             # attach privilege; it slows the
#                                             # server hard, counts stay exact)
set -u
cd "$(dirname "$0")/.."
BIN=target/release/wxpoint
DUR=${1:-12}
BASE=http://127.0.0.1:8094
ROOT=/tmp/opencode/wxpoint-profile
OHA=${OHA:-tools/oha}
PASS=1

rm -rf "$ROOT"; mkdir -p "$ROOT/gfs" "$ROOT/hrrr"
cp tests/perfdata/gfs_perf_f00*.grib2 "$ROOT/gfs/"
cp tests/perfdata/hrrr_perf_f00*.grib2 "$ROOT/hrrr/"
touch "$ROOT"/gfs/*.grib2 "$ROOT"/hrrr/*.grib2

# config: identical semantics to loadtest.sh, port swapped 8093 -> 8094
python3 - <<'EOF'
import re
s = open("scripts/loadtest.sh").read()
m = re.search(r'cat > "\$ROOT/config\.toml" <<EOF\n(.*?)\nEOF', s, re.S)
cfg = m.group(1).replace("$ROOT", "/tmp/opencode/wxpoint-profile").replace("8093", "8094")
open("/tmp/opencode/wxpoint-profile/config.toml", "w").write(cfg + "\n")
EOF

gcc -shared -fPIC -o "$ROOT/profshim.so" scripts/profshim.c -ldl || exit 1

LD_PRELOAD="$ROOT/profshim.so" WXPOINT_PROF_PATH="$ROOT/samples.bin" \
  "$BIN" --config "$ROOT/config.toml" > "$ROOT/server.log" 2>&1 &
SRV=$!

ready=0
for i in $(seq 1 300); do
  sleep 0.2
  code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/ready" || echo 000)
  if [ "$code" = "200" ]; then ready=1; break; fi
done
if [ "$ready" != "1" ]; then
  echo "FAIL: never became ready"; cat "$ROOT/server.log"; kill $SRV; exit 1
fi
echo "ready (perf dataset indexed)"

RUN="2026-10-19T12:00:00Z"   # from GRIB sec1, not filenames
U1="$BASE/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=39.875&lon=25.0&valid=$RUN"
U2="$BASE/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=0.125&lon=359.9&valid=$RUN"
U3="$BASE/v1/point?model=gfs&param=TMP&level=isobaric:775&lat=39.875&lon=25.0&valid=$RUN"
U4="$BASE/v1/point?model=gfs&param=TMP&level=isobaric:1000&lat=39.875&lon=25.0&valid=2026-10-19T15:00:00Z"
U5="$BASE/v1/point?model=gfs&param=TMP,HGT&level=isobaric:850&lat=39.875&lon=25.0&valid=$RUN"
U6="$BASE/v1/point?model=hrrr&param=TMP&level=isobaric:850&lat=35.009136&lon=-96.979227&valid=$RUN"
printf '%s\n%s\n%s\n%s\n%s\n%s\n' "$U1" "$U2" "$U3" "$U4" "$U5" "$U6" > "$ROOT/urls.txt"

idx=0
for u in "$U1" "$U2" "$U3" "$U4" "$U5" "$U6"; do
  code=$(curl -s -o /dev/null -w '%{http_code}' "$u")
  [ "$code" = "200" ] || { echo "FAIL: url $idx http=$code"; PASS=0; }
  idx=$((idx+1))
done
[ "$PASS" = "1" ] || { kill $SRV; rm -rf "$ROOT"; exit 1; }

# attribution aids captured while the server lives
cp "/proc/$SRV/maps" "$ROOT/maps" 2>/dev/null || true
snap() { # per-thread utime/stime ticks + process aggregate, one line per task
  for f in "/proc/$SRV/task/"*/stat; do
    awk '{print FILENAME, $1, $14, $15}' "$f"
  done
}
snap > "$ROOT/cpu.before"

PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 4)
YAMA=$(cat /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || echo 1)

echo "== oha mix: 6 URLs, --worker-threads 8, -c 64, keep-alive, ${DUR}s =="
"$OHA" --worker-threads 8 -c 64 -z "${DUR}s" --no-tui --urls-from-file "$ROOT/urls.txt" \
  > "$ROOT/oha.out" 2>&1
grep -E 'Requests/sec|Success rate' "$ROOT/oha.out" || true

snap > "$ROOT/cpu.after"
kill $SRV 2>/dev/null; wait $SRV 2>/dev/null

echo
if [ "$PARANOID" -le 1 ] && [ "$YAMA" -le 0 ]; then
  echo "perf privileges present on this machine (not exercised here)"
else
  echo "profiling path used: SIGPROF shim (perf_event_paranoid=$PARANOID," \
     "yama ptrace_scope=$YAMA block perf/gdb attach sampling)"
fi

echo "== leaf-sample attribution (poor-man's profiler; ${DUR}s x 8 worker threads) =="
python3 - <<EOF
import bisect, collections, re, struct, os

def nm_syms(path):
    syms = []
    for ln in os.popen(f"nm -C -S --defined-only '{path}' 2>/dev/null"):
        p = ln.split(None, 3)
        if len(p) == 4 and p[2] in "tTwW":
            try:
                start = int(p[0], 16); size = int(p[1], 16)
            except ValueError:
                continue
            if size > 0:
                syms.append((start, start + size, p[3]))
    syms.sort()
    return syms

syms = nm_syms("target/release/wxpoint")
starts = [s[0] for s in syms]

maps = []   # (start, end, file_off, path) file-backed segments from /proc/pid/maps
for ln in open("$ROOT/maps"):
    m = re.match(r"([0-9a-f]+)-([0-9a-f]+)\s+\S+\s+([0-9a-f]+)\s+\S+\s+\S+\s+(/\S+)", ln)
    if m:
        maps.append((int(m.group(1), 16), int(m.group(2), 16), int(m.group(3), 16), m.group(4)))

d = open("$ROOT/samples.bin", "rb").read()
n = struct.unpack("<Q", d[:8])[0]
rips = [struct.unpack_from("<Q", d, 8 + 8 * i)[0] for i in range(n)]

# PIE base: lowest exec segment of the binary in maps
binpath = "target/release/wxpoint"
base = None
for st, en, foff, pth in maps:
    rp = os.path.realpath(pth)
    if rp.endswith("/wxpoint") or "release/wxpoint" in rp:
        base = st if base is None else min(base, st)
dyn_cache = {}
def lib_sym(path, vaddr):
    key = path
    if key not in dyn_cache:
        tbl = []
        for ln in os.popen(f"nm -D -S --defined-only '{path}' 2>/dev/null"):
            p = ln.split(None, 3)
            if len(p) == 4 and p[2] in "TtWwVvIi":
                try:
                    a = int(p[0], 16); s = int(p[1], 16)
                except ValueError:
                    continue
                tbl.append((a, a + max(s, 1), p[3]))
        tbl.sort()
        dyn_cache[key] = tbl
    tbl = dyn_cache[key]
    ks = [t[0] for t in tbl]
    k = bisect.bisect_right(ks, vaddr) - 1
    if k >= 0 and tbl[k][0] <= vaddr < tbl[k][1]:
        return tbl[k][2]
    return None
def label(rip):
    if base is not None:
        off = rip - base
        k = bisect.bisect_right(starts, off) - 1
        if k >= 0 and syms[k][0] <= off < syms[k][1]:
            return syms[k][2]
    for st, en, foff, pth in maps:
        if st <= rip < en:
            nm = lib_sym(pth, rip - st + foff)
            bn = pth.split("/")[-1]
            if nm is not None:
                return f"{bn}!{nm}"
            return f"{bn}+0x{rip - st + foff:x}"
    return "[vdso/anon]"

cnt = collections.Counter(label(r) for r in rips)
print(f"samples={n}  distinct_sites={len(set(rips))}")
print("-- top cost centers (leaf attribution, % of CPU samples) --")
for name, c in cnt.most_common(8):
    print(f"  {100.0*c/n:5.1f}%  {c:7d}  {name}")
EOF

if [ -s "$ROOT/cpu.before" ] && [ -s "$ROOT/cpu.after" ]; then
  echo "== per-thread CPU split (tokio workers vs ingest pool) =="
python3 - <<'EOF'
def snap(fn):
    out = {}
    for ln in open(fn):
        p = ln.split()
        if len(p) >= 4:
            out[(p[1],)] = (out.get((p[1],), (0, 0))[0] + int(p[2]),
                            out.get((p[1],), (0, 0))[1] + int(p[3]))
    return out
b = snap("/tmp/opencode/wxpoint-profile/cpu.before")
a = snap("/tmp/opencode/wxpoint-profile/cpu.after")
rows = []
for k in set(a) | set(b):
    au, as_ = a.get(k, (0, 0)); bu, bs = b.get(k, (0, 0))
    rows.append((k[0], (au - bu) + (as_ - bs)))
rows.sort(key=lambda r: -r[1])
tot = sum(r[1] for r in rows) or 1
for name, dt in rows[:6]:
    print(f"  {100.0*dt/tot:5.1f}%  {dt:6d} ticks  {name}")
EOF
fi

if [ "${STRACE:-0}" = "1" ]; then
  # optional syscall accounting: strace LAUNCH mode needs no attach privilege
  # (yama only blocks ATTACH); it slows the server hard, so keep the window
  # small — counts stay exact regardless of speed.
  SDUR=${STRACE_DUR:-2}
  # NOTE: strace -f can linger waiting for traced LWPs after the inferior
  # takes SIGTERM; the outer `timeout` guarantees the pass always ends.
  timeout 120 strace -f -c -o "$ROOT/strace.txt" \
    env LD_PRELOAD="$ROOT/profshim.so" WXPOINT_PROF_PATH="$ROOT/samples2.bin" \
      "$BIN" --config "$ROOT/config.toml" > "$ROOT/server2.log" 2>&1 &
  SRV2=$!
  ready2=0
  for i in $(seq 1 300); do
    sleep 0.2
    code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/ready" || echo 000)
    if [ "$code" = "200" ]; then ready2=1; break; fi
  done
  if [ "$ready2" = "1" ]; then
    "$OHA" --worker-threads 8 -c 8 -z "${SDUR}s" --no-tui --urls-from-file "$ROOT/urls.txt" >/dev/null 2>&1
  else
    echo "strace pass never became ready"
  fi
  kill $SRV2 2>/dev/null; wait $SRV2 2>/dev/null
  echo "== top syscalls by count (strace -c, launch mode) =="
  grep -E '^[a-z]' "$ROOT/strace.txt" 2>/dev/null | sort -k4 -nr | head -6 || true
fi

rm -rf "$ROOT"
