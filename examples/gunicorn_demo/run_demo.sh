#!/usr/bin/env bash
# Phase 4 acceptance demo: gunicorn with 4 workers against `elitesql serve`.
#
# 1. Starts the sidecar (one process owning the engine).
# 2. Starts gunicorn with 4 worker processes.
# 3. Fires concurrent visitors that read and write simultaneously.
# 4. Verifies: all writes landed, reads never blocked, several workers served.
#
# Usage: examples/gunicorn_demo/run_demo.sh [visitors] [requests-each]

set -euo pipefail
cd "$(dirname "$0")/../.."

VISITORS="${1:-8}"
REQUESTS="${2:-25}"
DEMO_DIR="$(mktemp -d)"
export ELITESQL_SOCKET="$DEMO_DIR/elitesql.sock"
DB="$DEMO_DIR/demo.esql"
HTTP_PORT=8137

cleanup() {
  kill "$GUNICORN_PID" "$SIDECAR_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$DEMO_DIR"
}
trap cleanup EXIT

echo "==> building elitesql"
cargo build --release -p elitesql-cli >/dev/null 2>&1

echo "==> preparing schema"
./target/release/elitesql query "$DB" \
  "CREATE TABLE visits (who text NOT NULL, worker_pid int64, at timestamp)" >/dev/null
./target/release/elitesql query "$DB" "CREATE INDEX ON visits (who)" >/dev/null

echo "==> starting sidecar (elitesql serve)"
# Logs go to files: inheriting our stdout pipe would keep readers of it
# alive past the script's exit.
./target/release/elitesql serve "$DB" "$ELITESQL_SOCKET" >"$DEMO_DIR/sidecar.log" 2>&1 &
SIDECAR_PID=$!
for _ in $(seq 50); do [ -S "$ELITESQL_SOCKET" ] && break; sleep 0.1; done
[ -S "$ELITESQL_SOCKET" ] || { echo "sidecar socket never appeared"; cat "$DEMO_DIR/sidecar.log"; exit 1; }

echo "==> starting gunicorn with 4 workers"
# exec: the subshell PID becomes the gunicorn master, so cleanup can kill it.
(cd examples/gunicorn_demo && exec gunicorn -w 4 -b "127.0.0.1:$HTTP_PORT" \
  --log-level warning app:app) >"$DEMO_DIR/gunicorn.log" 2>&1 &
GUNICORN_PID=$!
for _ in $(seq 50); do
  curl -s "http://127.0.0.1:$HTTP_PORT/whoami" >/dev/null 2>&1 && break
  sleep 0.2
done

echo "==> $VISITORS visitors x $REQUESTS requests (writes + reads interleaved)"
python3 - "$VISITORS" "$REQUESTS" "$HTTP_PORT" <<'PYEOF'
import concurrent.futures
import json
import sys
import time
import urllib.request

visitors, requests_each, port = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
base = f"http://127.0.0.1:{port}"

def get(path):
    with urllib.request.urlopen(base + path, timeout=10) as r:
        return json.loads(r.read())

def visitor(v):
    workers, slowest = set(), 0.0
    for i in range(requests_each):
        t0 = time.monotonic()
        w = get(f"/visit?user=visitor{v}")
        assert "inserted" in w, w
        r = get("/visits")
        assert "total" in r, r
        slowest = max(slowest, time.monotonic() - t0)
        workers.add(w["worker"])
        workers.add(r["worker"])
    return workers, slowest

t0 = time.monotonic()
with concurrent.futures.ThreadPoolExecutor(max_workers=visitors) as pool:
    results = list(pool.map(visitor, range(visitors)))
elapsed = time.monotonic() - t0

workers = set()
slowest = 0.0
for w, s in results:
    workers |= w
    slowest = max(slowest, s)

total = get("/visits")["total"]
expected = visitors * requests_each
print(f"    writes expected={expected} recorded={total}")
print(f"    workers that served: {len(workers)} distinct pids {sorted(workers)}")
print(f"    wall time {elapsed:.2f}s, slowest single write+read {slowest*1000:.0f}ms")
assert total == expected, f"lost writes: {total} != {expected}"
assert len(workers) >= 2, "traffic should be served by multiple worker processes"
assert slowest < 5.0, f"a request appears to have blocked: {slowest:.1f}s"
print("    OK: no lost writes, no blocking, multiple workers")
PYEOF

echo "==> demo passed"
