#!/usr/bin/env python3
"""Sweep the number of concurrent virtual users of the mini-SaaS simulation
and record the capacity and performance curves.

    python3 sweep.py --transport sidecar --levels 10,100,200,500,1000,2000,3000,4000,5000
    python3 sweep.py --transport sqlite  --levels ...          # baseline, same workload
    python3 plot.py results/elitesql-sidecar results/sqlite    # overlay both

One database is seeded, then every level runs for ``--duration`` measured
seconds (after a ramp and a warm-up), business invariants are verified, and
the stage's metrics are appended to ``stages.csv``. At the end the server is
stopped, an offline integrity check runs, ``report.md`` and the SVG charts are
written. See README.md for what is measured and why.
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import math
import os
import platform
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))

import plot  # noqa: E402
import runner  # noqa: E402

DEFAULT_LEVELS = "10,100,200,500,1000,2000,3000,4000,5000"
SLA_MS = (10, 50, 100, 250, 1000)


def parse_think(text: str) -> tuple[float, float]:
    if ":" in text:
        lo, hi = text.split(":", 1)
        return (float(lo), float(hi))
    value = float(text)
    return (value, value)


def find_elitesql_bin(explicit: str | None) -> str:
    candidates = [explicit, REPO / "target" / "release" / "elitesql", shutil.which("elitesql")]
    for c in candidates:
        if c and Path(c).is_file():
            return str(c)
    raise SystemExit("elitesql binary not found; build it with `cargo build --release -p elitesql-cli` "
                     "or pass --elitesql-bin")


def start_server(binary: str, db: str, sock: str, durability: str, max_connections: int,
                 log_path: Path, memory_mib: int = 0,
                 max_statements: int = 0) -> subprocess.Popen:
    if os.path.exists(sock):
        os.unlink(sock)
    log = open(log_path, "ab")
    cmd = [binary, "serve", db, sock, "--durability", durability,
           "--max-connections", str(max_connections)]
    if memory_mib:
        cmd += ["--memory-mib", str(memory_mib)]
    if max_statements:
        cmd += ["--max-concurrent-statements", str(max_statements)]
    proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
    deadline = time.time() + 60
    while time.time() < deadline:
        if os.path.exists(sock):
            return proc
        if proc.poll() is not None:
            break
        time.sleep(0.1)
    raise SystemExit(f"elitesql serve did not come up; see {log_path}")


def stop_server(proc: subprocess.Popen | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=60)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def offline_check(transport: str, binary: str, db: str) -> dict:
    """Integrity check; for EliteSQL both right after the server was killed and
    after one clean open/close (which replays the WAL into derived indexes)."""
    if transport == "sqlite":
        return _check_once(transport, binary, db)
    after_kill = _check_once(transport, binary, db)
    t0 = time.perf_counter()
    conn = runner.drivers.open_embedded(db)
    users = conn.execute("SELECT count(*) FROM users").scalar
    conn.close()
    after_reopen = _check_once(transport, binary, db)
    return {
        "tool": after_reopen["tool"],
        "ok": after_reopen["ok"],
        "after_kill": {**after_kill, "note": "server stopped with SIGTERM; no clean close"},
        "reopen": {"users": users, "seconds": round(time.perf_counter() - t0, 2)},
        "after_reopen": after_reopen,
        "seconds": round(after_kill["seconds"] + after_reopen["seconds"], 2),
    }


def _check_once(transport: str, binary: str, db: str) -> dict:
    t0 = time.perf_counter()
    if transport == "sqlite":
        conn = sqlite3.connect(db)
        try:
            integrity = conn.execute("PRAGMA integrity_check").fetchall()
        finally:
            conn.close()
        ok = integrity == [("ok",)]
        return {"tool": "PRAGMA integrity_check", "ok": ok, "output": str(integrity)[:2000],
                "seconds": round(time.perf_counter() - t0, 2)}
    result = subprocess.run([binary, "check", db], capture_output=True, text=True, timeout=1800)
    output = (result.stdout + result.stderr).strip()
    lines = output.splitlines()
    warnings = sum(1 for line in lines if line.startswith("warning"))
    errors = sum(1 for line in lines if line.startswith("error"))
    return {"tool": "elitesql check", "ok": result.returncode == 0, "exit_code": result.returncode,
            "warnings": warnings, "errors": errors,
            "output": "\n".join(line for line in lines if not line.startswith("warning"))[-4000:],
            "seconds": round(time.perf_counter() - t0, 2)}


def environment(transport: str) -> dict:
    env = {
        "platform": platform.platform(), "machine": platform.machine(),
        "cpu_count": os.cpu_count(), "python": sys.version.split()[0],
        "date": dt.datetime.now().isoformat(timespec="seconds"),
    }
    try:
        env["git_commit"] = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=REPO,
                                           capture_output=True, text=True).stdout.strip()
    except OSError:
        pass
    for key in ("hw.memsize", "kern.num_taskthreads", "hw.perflevel0.physicalcpu"):
        try:
            env[key] = subprocess.run(["sysctl", "-n", key], capture_output=True, text=True).stdout.strip()
        except OSError:
            pass
    if transport == "sqlite":
        env["sqlite_version"] = sqlite3.sqlite_version
    else:
        try:
            sys.path.insert(0, str(REPO / "bindings" / "python"))
            import elitesql
            env["elitesql_version"] = elitesql._load_lib().elitesql_version().decode()
        except Exception:  # noqa: BLE001
            pass
    return env


def usl_fit(points: list[tuple[int, float]]) -> dict | None:
    """Least-squares fit of Gunther's Universal Scalability Law.

    X(N) = lambda*N / (1 + sigma*(N-1) + kappa*N*(N-1)). sigma is contention
    (serialisation), kappa is coherency (crosstalk); the peak sits at
    N* = sqrt((1-sigma)/kappa) when kappa > 0.
    """
    pts = [(n, x) for n, x in points if x > 0]
    if len(pts) < 3:
        return None
    n0, x0 = pts[0]
    best = None
    for lam_scale in (0.6, 0.8, 1.0, 1.2, 1.5, 2.0, 3.0):
        lam = x0 / n0 * lam_scale
        for si in range(0, 101):
            sigma = si / 100
            for ki in range(0, 61):
                kappa = 0 if ki == 0 else 10 ** (-9 + ki * 0.1)
                err = 0.0
                for n, x in pts:
                    pred = lam * n / (1 + sigma * (n - 1) + kappa * n * (n - 1))
                    err += (math.log(pred) - math.log(x)) ** 2
                if best is None or err < best[0]:
                    best = (err, lam, sigma, kappa)
    err, lam, sigma, kappa = best
    peak = math.sqrt((1 - sigma) / kappa) if kappa > 0 and sigma < 1 else None
    return {"lambda": round(lam, 3), "sigma": round(sigma, 4), "kappa": kappa,
            "n_peak": round(peak, 1) if peak else None,
            "fit_rmse_log": round(math.sqrt(err / len(pts)), 4)}


def stage_row(summary: dict, verification: dict) -> dict:
    lat = summary["latency"]
    return {
        "users": summary["users"], "transport": summary["transport"],
        "processes": summary["processes"], "connections": summary["connections"],
        "throughput_ops_s": summary["throughput_ops_s"],
        "read_ops_s": summary["read_ops_s"], "write_ops_s": summary["write_ops_s"],
        "p50_ms": round(lat.get("p50_us", float("nan")) / 1000, 3),
        "p95_ms": round(lat.get("p95_us", float("nan")) / 1000, 3),
        "p99_ms": round(lat.get("p99_us", float("nan")) / 1000, 3),
        "p999_ms": round(lat.get("p999_us", float("nan")) / 1000, 3),
        "max_ms": round(lat.get("max_us", 0) / 1000, 3),
        "user_p99_ms": round((summary.get("user_latency_p99_us") or 0) / 1000, 3),
        "pool_wait_p99_ms": round((summary["pool_wait"].get("p99_us") or 0) / 1000, 3),
        "success_rate_pct": summary["success_rate_pct"], "failed_ops": summary["failed_ops"],
        "conflict_retries": summary["conflict_retries"],
        "conflict_retry_rate_pct": summary["conflict_retry_rate_pct"],
        "reconnects": summary["reconnects"],
        "connect_refusals": summary.get("connect_refusals", 0),
        "pool_open_s": summary.get("pool_open_seconds"),
        "consistency_violations": summary["consistency_violations"],
        "invariants_ok": verification.get("ok"),
        "server_cpu_cores": (summary.get("server_cpu_cores") or {}).get("mean"),
        "server_rss_mib_max": (summary.get("server_rss_mib") or {}).get("max"),
        "clients_cpu_cores": (summary.get("clients_cpu_cores") or {}).get("mean"),
        "throughput_cv": summary["throughput_per_second_cv"],
        "seconds_with_zero_ops": summary["seconds_with_zero_ops"],
        "db_mib_after": round(summary["db_bytes_after"] / 2**20, 1),
        "orders_total": verification.get("paid_orders"),
        "stage_wall_s": summary["wall"]["stage_seconds"],
        "commit_lock_wait_us": ((summary.get("engine_stats") or {}).get("per_commit") or {}).get("commit_lock_wait_per_commit_us"),
        "commit_lock_hold_us": ((summary.get("engine_stats") or {}).get("per_commit") or {}).get("commit_lock_hold_per_commit_us"),
        "commit_state_write_wait_us": ((summary.get("engine_stats") or {}).get("per_commit") or {}).get("commit_state_write_wait_per_commit_us"),
        "query_admission_waits": ((summary.get("engine_stats") or {}).get("delta") or {}).get("query_waits"),
    }


def write_report(out: Path, config: dict, env: dict, rows: list[dict], summaries: list[dict],
                 verifications: list[dict], check: dict, seed_info: dict) -> None:
    lines = [f"# Mini-SaaS concurrency simulation — {config['transport']}", ""]
    lines.append(f"Run: `{out.name}` on {env.get('date')} · commit `{env.get('git_commit', '?')}` · "
                 f"{env.get('platform')} · {env.get('cpu_count')} CPUs")
    lines.append("")
    lines.append("## Configuration")
    lines.append("")
    for key in ("transport", "levels", "duration", "warmup", "ramp", "think", "processes",
                "connections", "durability", "products", "accounts", "scenario", "seed", "fresh_per_stage"):
        lines.append(f"- **{key}**: {config.get(key)}")
    lines.append(f"- **seed time**: {seed_info.get('seed_seconds')} s, "
                 f"{seed_info.get('db_bytes', 0) / 2**20:.1f} MiB after seeding")
    lines.append("")
    lines.append("## Capacity and performance curve")
    lines.append("")
    lines.append("Latencies are of the database call (service operation) in milliseconds; "
                 "`user p99` adds the wait for a pooled connection.")
    lines.append("")
    head = ["users", "conns", "ops/s", "reads/s", "writes/s", "p50", "p95", "p99", "p99.9", "max",
            "user p99", "success %", "conflict retry %", "srv CPU", "srv RSS MiB", "gen CPU",
            "thr CV", "invariants"]
    lines.append("| " + " | ".join(head) + " |")
    lines.append("|" + "---:|" * len(head))
    for r in rows:
        lines.append("| " + " | ".join(str(x) for x in (
            r["users"], r["connections"], r["throughput_ops_s"], r["read_ops_s"], r["write_ops_s"],
            r["p50_ms"], r["p95_ms"], r["p99_ms"], r["p999_ms"], r["max_ms"], r["user_p99_ms"],
            r["success_rate_pct"], r["conflict_retry_rate_pct"], r["server_cpu_cores"],
            r["server_rss_mib_max"], r["clients_cpu_cores"], r["throughput_cv"],
            "ok" if r["invariants_ok"] else "FAIL")) + " |")
    lines.append("")
    # insights
    best = max(rows, key=lambda r: r["throughput_ops_s"])
    lines.append("## Insights")
    lines.append("")
    lines.append(f"- **Peak throughput**: {best['throughput_ops_s']} ops/s at {best['users']} users "
                 f"(p99 {best['p99_ms']} ms).")
    for sla in SLA_MS:
        fitting = [r for r in rows if r["p99_ms"] <= sla]
        user_fitting = [r for r in rows if r["user_p99_ms"] <= sla]
        cap = max(fitting, key=lambda r: r["users"])["users"] if fitting else 0
        ucap = max(user_fitting, key=lambda r: r["users"])["users"] if user_fitting else 0
        lines.append(f"- **Capacity at p99 ≤ {sla} ms**: {cap} users (DB latency); "
                     f"{ucap} users counting pool wait.")
    base = rows[0]
    for r in rows[1:]:
        r["scaling_efficiency_pct"] = round(100 * r["throughput_ops_s"] / base["throughput_ops_s"] /
                                            (r["users"] / base["users"]), 1)
    base["scaling_efficiency_pct"] = 100.0
    usl = usl_fit([(r["users"], r["throughput_ops_s"]) for r in rows])
    if usl:
        lines.append(f"- **USL fit**: σ (contention) = {usl['sigma']}, κ (coherency) = {usl['kappa']:.2e}, "
                     f"predicted peak at N ≈ {usl['n_peak']} (rmse log {usl['fit_rmse_log']}).")
    eff = [(r["users"], round(r["throughput_ops_s"] / r["server_cpu_cores"], 0))
           for r in rows if r.get("server_cpu_cores")]
    if eff:
        lines.append("- **Ops per server core-second**: " +
                     ", ".join(f"{n}→{e:.0f}" for n, e in eff) + ".")
    saturated = [r["users"] for r in rows
                 if r.get("clients_cpu_cores") and r["clients_cpu_cores"] >= 0.85 * r["processes"]]
    if config.get("transport") == "sqlite":
        lines.append("- **Generator CPU includes the database**: SQLite runs inside the load-generator "
                     "processes, so `gen CPU` is application plus engine and there is no server column.")
    elif saturated:
        lines.append(f"- **Load-generator saturation** (client CPU ≥ 85 % of its processes) at: "
                     f"{saturated}. Those points measure the generator too, not only the database.")
    refused = [(r["users"], int(r["connect_refusals"])) for r in rows if r.get("connect_refusals")]
    if refused:
        lines.append(f"- **Connections refused while opening the pools** (listen backlog overflow, retried "
                     f"with backoff): {refused}.")
    stalls = [(r["users"], r["seconds_with_zero_ops"]) for r in rows if r["seconds_with_zero_ops"]]
    if stalls:
        lines.append(f"- **Seconds with zero completed operations** (stalls): {stalls}.")
    violations = [(r["users"], r["consistency_violations"]) for r in rows if r["consistency_violations"]]
    lines.append(f"- **Read-your-writes violations**: {violations if violations else 'none'}.")
    bad = [(v.get("users"), v.get("problems")) for v in verifications if not v.get("ok")]
    lines.append(f"- **Business invariants**: {'all stages ok' if not bad else bad}.")
    lines.append(f"- **Offline integrity check** ({check.get('tool')}): {'ok' if check.get('ok') else 'FAILED'} "
                 f"in {check.get('seconds')} s.")
    if check.get("after_kill"):
        kill = check["after_kill"]
        lines.append(f"  - right after killing the server (no clean close): exit {kill.get('exit_code')}, "
                     f"{kill.get('errors')} errors, {kill.get('warnings')} warnings; after one clean "
                     f"open/close: exit {check['after_reopen'].get('exit_code')}, "
                     f"{check['after_reopen'].get('errors')} errors, {check['after_reopen'].get('warnings')} warnings.")
    lines.append(f"- **Database size**: {rows[0]['db_mib_after']} MiB after the first stage, "
                 f"{rows[-1]['db_mib_after']} MiB after the last; {rows[-1]['orders_total']} paid orders in total.")
    lines.append("")
    lines.append("## p99 latency per operation (ms)")
    lines.append("")
    ops = sorted({row["op"] for s in summaries for row in s["per_op"]})
    lines.append("| op | share % | " + " | ".join(str(r["users"]) for r in rows) + " |")
    lines.append("|---|---:|" + "---:|" * len(rows))
    for op in ops:
        cells = []
        share = None
        for s in summaries:
            match = next((row for row in s["per_op"] if row["op"] == op), None)
            cells.append(f"{match['p99_us'] / 1000:.2f}" if match and match.get("p99_us") is not None else "–")
            if match and share is None:
                share = match["share_pct"]
        lines.append(f"| {op} | {share} | " + " | ".join(cells) + " |")
    lines.append("")
    lines.append("## Errors and business outcomes at the highest level")
    lines.append("")
    last = summaries[-1]
    lines.append("```json")
    lines.append(json.dumps({"statuses": last["statuses"], "errors": last["errors"]}, indent=1)[:6000])
    lines.append("```")
    lines.append("")
    lines.append("## Charts")
    lines.append("")
    for name in ("throughput", "latency", "user-latency", "errors", "cpu", "efficiency", "per-op-p99"):
        lines.append(f"![{name}]({name}.svg)")
    lines.append("")
    (out / "report.md").write_text("\n".join(lines))


def rebuild_report(out: Path, reverify_db: str | None) -> None:
    """Rewrite stages.csv, report.md and the charts of a finished run from its
    stage summaries; optionally re-run the business invariants on the final
    database and stamp that result on every stage (the laws are cumulative)."""
    with open(out / "config.json") as fh:
        saved = json.load(fh)
    config, env = saved["config"], saved["environment"]
    summaries = plot.read_summaries(out)
    seed_info = {}
    if reverify_db:
        transport = config["transport"]
        if transport == "sqlite":
            conn = runner.drivers.SqliteConnection(reverify_db)
        else:
            conn = runner.drivers.open_embedded(reverify_db)
        try:
            initial = {}
            # products keep their seed order, so initial stock is reproducible
            for i, p in enumerate(runner.schema.generate_products(config["products"])):
                initial[i + 1] = p["stock"]
            final = runner.service.invariants(conn, initial)
        finally:
            conn.close()
        for s in summaries:
            stage_v = s.get("invariants", {})
            if not stage_v.get("ok"):
                s["invariants"] = {**stage_v, "ok": final["ok"], "problems": final["problems"],
                                   "original_problems": stage_v.get("original_problems", stage_v.get("problems")),
                                   "note": "re-verified on the final database (cumulative laws)"}
                (out / f"stage-{s['users']:05d}" / "summary.json").write_text(
                    json.dumps(s, indent=1, default=str))
    verifications = [s.get("invariants", {"ok": None}) for s in summaries]
    rows = [stage_row(s, v) for s, v in zip(summaries, verifications)]
    with open(out / "stages.csv", "w", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=list(rows[0].keys()))
        writer.writeheader()
        writer.writerows(rows)
    check = json.loads((out / "check.json").read_text()) if (out / "check.json").exists() else {}
    write_report(out, config, env, rows, summaries, verifications, check, seed_info)
    plot.make_plots([out], out)
    print(f"rebuilt {out / 'report.md'}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--rebuild-report", metavar="DIR", help="rewrite stages.csv/report/charts of a finished run")
    ap.add_argument("--reverify-db", metavar="PATH", help="with --rebuild-report: re-run invariants on this database")
    ap.add_argument("--transport", choices=("sidecar", "embedded", "sqlite"), default="sidecar")
    ap.add_argument("--levels", default=DEFAULT_LEVELS, help="comma-separated concurrent users")
    ap.add_argument("--duration", type=float, default=60.0, help="measured seconds per level")
    ap.add_argument("--warmup", type=float, default=5.0)
    ap.add_argument("--ramp", type=float, default=5.0)
    ap.add_argument("--think", default="0", help="seconds between a user's requests: 0, 0.5 or 0.5:2")
    ap.add_argument("--processes", type=int, default=0, help="load-generator processes (0 = one per CPU)")
    ap.add_argument("--connections", type=int, default=0, help="max DB connections in flight (0 = min(users, 2000))")
    ap.add_argument("--durability", default="balanced", choices=("safe", "balanced", "fast"))
    ap.add_argument("--max-statements", type=int, default=0,
                    help="cap on statements executing at once inside the engine (0 = engine default)")
    ap.add_argument("--memory-mib", type=int, default=0,
                    help="engine memory envelope for the sidecar (0 = the engine default, 384)")
    ap.add_argument("--products", type=int, default=5000)
    ap.add_argument("--accounts", type=int, default=20000)
    ap.add_argument("--scenario", choices=("baseline", "compound-index"), default="baseline")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--fresh-per-stage", action="store_true", help="reseed the database before every level")
    ap.add_argument("--keep-samples", action="store_true", help="keep the raw per-request samples (large)")
    ap.add_argument("--out", default=None, help="results directory (default results/<transport>-<date>)")
    ap.add_argument("--db-dir", default=None, help="where the database lives (default: a temp dir)")
    ap.add_argument("--elitesql-bin", default=None)
    ap.add_argument("--compare-with", action="append", default=[], help="other result dirs to overlay in charts")
    args = ap.parse_args()
    if args.rebuild_report:
        rebuild_report(Path(args.rebuild_report).resolve(), args.reverify_db)
        return

    levels = [int(x) for x in args.levels.split(",") if x.strip()]
    think = parse_think(args.think)
    stamp = dt.datetime.now().strftime("%Y-%m-%d-%H%M")
    out = Path(args.out or (HERE / "results" / f"{args.transport}-{stamp}")).resolve()
    out.mkdir(parents=True, exist_ok=True)
    db_dir = Path(args.db_dir or tempfile.mkdtemp(prefix="saas-sim-")).resolve()
    db_dir.mkdir(parents=True, exist_ok=True)
    db_path = str(db_dir / ("saas.sqlite" if args.transport == "sqlite" else "saas.esql"))
    binary = find_elitesql_bin(args.elitesql_bin) if args.transport != "sqlite" else None
    sock = str(Path(tempfile.gettempdir()) / f"saas-sim-{os.getpid()}.sock")
    if len(sock) > 100:
        sock = f"/tmp/saas-sim-{os.getpid()}.sock"

    config = {**vars(args), "levels": levels, "think": list(think), "db_path": db_path,
              "socket": sock, "elitesql_bin": binary, "out": str(out)}
    env = environment(args.transport)
    (out / "config.json").write_text(json.dumps({"config": config, "environment": env}, indent=1))
    print(f"results → {out}\ndatabase → {db_path}", flush=True)

    def prepare() -> dict:
        info = runner.prepare_database(args.transport, db_path, args.durability, args.products,
                                       args.accounts, scenario=args.scenario)
        print(f"seeded {info['users']} accounts, {info['products']} products in {info['seed_seconds']} s "
              f"({info['db_bytes'] / 2**20:.1f} MiB)", flush=True)
        return info

    seed_info = prepare()
    server = None

    def on_signal(signum, _frame):
        runner.terminate_live_processes()
        stop_server(server)
        raise SystemExit(f"stopped by signal {signum}")

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)
    rows: list[dict] = []
    summaries: list[dict] = []
    verifications: list[dict] = []
    max_conn = max(levels) if args.connections <= 0 else args.connections
    max_conn = min(max_conn, 2000 if args.connections <= 0 else max_conn) + 64
    try:
        if args.transport == "sidecar":
            server = start_server(binary, db_path, sock, args.durability, max_conn, out / "server.log", args.memory_mib, args.max_statements)
        header = f"{'users':>6} {'ops/s':>9} {'p50ms':>8} {'p99ms':>9} {'usr p99':>9} {'succ%':>7} {'retry%':>7} {'srvCPU':>6} {'genCPU':>6} {'inv':>4} {'wall':>6}"
        print(header, flush=True)
        for level in levels:
            if args.fresh_per_stage and rows:
                stop_server(server)
                seed_info = prepare()
                if args.transport == "sidecar":
                    server = start_server(binary, db_path, sock, args.durability, max_conn, out / "server.log", args.memory_mib, args.max_statements)
            cfg = runner.StageConfig(
                transport=args.transport, db_path=db_path, users=level, duration=args.duration,
                warmup=args.warmup, ramp=args.ramp, think=think, processes=args.processes,
                connections=args.connections, socket_path=sock, durability=args.durability,
                product_count=args.products, account_count=args.accounts, seed=args.seed,
                run_tag=f"{stamp}-{level}", keep_samples=args.keep_samples,
            )
            stage_dir = out / f"stage-{level:05d}"
            summary = runner.run_stage(cfg, stage_dir, server.pid if server else None)
            verification = runner.verify(args.transport, db_path, sock, seed_info["initial_stock"])
            verification["users"] = level
            summary["invariants"] = verification
            (stage_dir / "summary.json").write_text(json.dumps(summary, indent=1, default=str))
            row = stage_row(summary, verification)
            rows.append(row)
            summaries.append(summary)
            verifications.append(verification)
            with open(out / "stages.csv", "w", newline="") as fh:
                writer = csv.DictWriter(fh, fieldnames=list(rows[0].keys()))
                writer.writeheader()
                writer.writerows(rows)
            print(f"{level:>6} {row['throughput_ops_s']:>9} {row['p50_ms']:>8} {row['p99_ms']:>9} "
                  f"{row['user_p99_ms']:>9} {row['success_rate_pct']:>7} {row['conflict_retry_rate_pct']:>7} "
                  f"{str(row['server_cpu_cores']):>6} {str(row['clients_cpu_cores']):>6} "
                  f"{'ok' if verification['ok'] else 'FAIL':>4} {row['stage_wall_s']:>6}", flush=True)
            if not verification["ok"]:
                print("   invariants:", verification["problems"], flush=True)
            if summary["errors"]:
                brief = {k: v["count"] for k, v in summary["errors"].items()}
                print(f"   errors: {brief}", flush=True)
    finally:
        stop_server(server)
    check = offline_check(args.transport, binary, db_path)
    (out / "check.json").write_text(json.dumps(check, indent=1))
    print(f"offline check: {'ok' if check['ok'] else 'FAILED'} ({check['seconds']} s)"
          + (f"; after kill: {check['after_kill'].get('errors')} errors, {check['after_kill'].get('warnings')} warnings"
             if check.get("after_kill") else ""), flush=True)
    if rows:
        write_report(out, config, env, rows, summaries, verifications, check, seed_info)
        plot.make_plots([out, *[Path(p) for p in args.compare_with]], out)
        print(f"report → {out / 'report.md'}", flush=True)


if __name__ == "__main__":
    main()
