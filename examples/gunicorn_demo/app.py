"""Phase 4 acceptance demo: gunicorn multi-worker against `elitesql serve`.

Each gunicorn worker (a separate OS process) opens one SidecarClient to the
single engine process. Visitors read and write concurrently: MVCC inside the
engine means readers never block writers and writers only meet at commit.

Endpoints:
  POST /visit?user=NAME   -> inserts a visit row, returns its id
  GET  /visits            -> total count + last 5 visits
  GET  /whoami            -> worker pid (to prove multiple workers serve)

Run via examples/gunicorn_demo/run_demo.sh
"""

import json
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "bindings", "python"))
from elitesql import SidecarClient  # noqa: E402

SOCKET = os.environ.get("ELITESQL_SOCKET", "/tmp/elitesql-demo.sock")
_client = None


def client() -> SidecarClient:
    global _client
    if _client is None:
        _client = SidecarClient(SOCKET)
    return _client


def app(environ, start_response):
    path = environ.get("PATH_INFO", "/")
    query = dict(
        pair.split("=", 1)
        for pair in environ.get("QUERY_STRING", "").split("&")
        if "=" in pair
    )
    try:
        if path == "/visit":
            user = query.get("user", "anon").replace("'", "''")
            out = client().query(
                f"INSERT INTO visits (who, worker_pid, at) VALUES ('{user}', {os.getpid()}, {int(time.time() * 1e6)})"
            )
            body = {"inserted": out["inserted"], "worker": os.getpid()}
        elif path == "/visits":
            count = client().query("SELECT count(*) AS n FROM visits")
            last = client().query(
                "SELECT who, worker_pid FROM visits ORDER BY at DESC LIMIT 5"
            )
            body = {
                "total": count["rows"][0][0],
                "last": last["rows"],
                "worker": os.getpid(),
            }
        elif path == "/whoami":
            body = {"worker": os.getpid()}
        else:
            start_response("404 Not Found", [("Content-Type", "application/json")])
            return [b'{"error": "not found"}']
        payload = json.dumps(body).encode()
        start_response("200 OK", [("Content-Type", "application/json")])
        return [payload]
    except Exception as e:  # surface engine errors to the test driver
        payload = json.dumps({"error": str(e), "worker": os.getpid()}).encode()
        start_response("500 Internal Server Error", [("Content-Type", "application/json")])
        return [payload]
