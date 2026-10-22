#!/usr/bin/env python3
"""Are all expected (model, run-dir, forecast-hour[, min-level-count]) tuples queryable yet?

Reads /v1/inventory JSON from argv[1] (a file path) and a whitespace-separated
spec of tokens `model:dir:fh` or `model:dir:fh:min_levels`. Exit 0 when EVERY
token is satisfied, exit 1 otherwise (prints what is missing). The optional
4th component demands that EVERY field carrying that forecast hour exposes AT
LEAST `min_levels` levels -- i.e. the parameter's whole level stack arrived,
not just part of it (the transient-publish window would otherwise look complete).
Used by scripts/ingestbench.sh to time "wall-to-all-visible".
"""
import json
import sys

raw = open(sys.argv[1]).read() if len(sys.argv) > 1 else sys.stdin.read()
spec = sys.argv[2:]
if not spec:
    print("usage: _visible.py INVENTORY_JSON model:dir:fh[:min_levels] ...")
    sys.exit(2)
obj = json.loads(raw)
# /v1/inventory wraps the per-model blocks as {"inventory": [ {...}, ... ]};
# older/manual captures may be a bare model object or a bare list -- accept all
# three shapes so a shape change can never masquerade as "data not visible".
models = obj
if isinstance(obj, dict):
    if "inventory" in obj:
        models = obj["inventory"]
    elif "models" in obj:
        models = obj["models"]
    else:
        models = [obj]
have = {}
for m in models:
    for r in m.get("runs", []):
        for f in r.get("fields", []):
            nlev = len(f.get("levels", []))
            for h in f.get("forecast_hours", []):
                key = (m.get("model"), r.get("dir"), int(float(h)))
                prev = have.get(key)
                have[key] = nlev if prev is None else min(prev, nlev)
missing = []
for tok in spec:
    parts = tok.split(":")
    if len(parts) not in (3, 4):
        print("FATAL: bad spec token %r" % tok)
        sys.exit(2)
    key = (parts[0], parts[1], int(parts[2]))
    minlev = int(parts[3]) if len(parts) == 4 else 0
    nlev = have.get(key)
    if nlev is None:
        missing.append(tok + " (absent)")
    elif nlev < minlev:
        missing.append("%s (levels %d < %d)" % (tok, nlev, minlev))
if missing:
    print("pending: " + " ".join(missing))
    sys.exit(1)
sys.exit(0)
