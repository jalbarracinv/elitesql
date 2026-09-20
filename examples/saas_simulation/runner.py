"""Runs one *stage* (a fixed number of concurrent virtual users) of the
mini-SaaS simulation and aggregates its measurements.

Load generation is spread over several worker processes so the Python GIL
does not become the bottleneck before the database does. Each virtual user is
a thread in one of those processes; each request takes a connection from the
process-local pool, runs the operation, records latency, pool wait, outcome
and retries, and, optionally, sleeps a think time.

Transports:
  embedded  one process, one shared ``EliteSQL`` handle (thread-safe FFI).
  sidecar   P processes, pools of ``SidecarClient`` connections to a server
            started by the sweep (``elitesql serve`` over a Unix socket).
  sqlite    P processes, pools of ``sqlite3`` connections to one WAL file.
"""

from __future__ import annotations

import csv
import gzip
import random
import json
import math
import multiprocessing as mp
import os
import shutil
import statistics
import subprocess
import sys
import threading
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Optional

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(REPO / "bindings" / "python"))
sys.path.insert(0, str(HERE))

from saas import drivers, schema, service  # noqa: E402
from saas.vuser import READ_OPS, VirtualUser  # noqa: E402

SAMPLE_FIELDS = ("op", "start_ms", "latency_us", "pool_wait_us", "status", "retries")
_connect_refusals = [0]   # per load-generator process
LIVE_PROCESSES: list = []  # load generators of the stage in progress (for signal cleanup)


def terminate_live_processes() -> None:
    """Kill the load generators of the current stage (used on SIGTERM/SIGINT).

    A killed sweep must not leave hundreds of threads hammering the database
    and later writing samples into a directory that no longer exists."""
    for p in list(LIVE_PROCESSES):
        if p.is_alive():
            p.terminate()
    for p in list(LIVE_PROCESSES):
        p.join(timeout=10)
    LIVE_PROCESSES.clear()


@dataclass
class StageConfig:
    transport: str                 # embedded | sidecar | sqlite
    db_path: str
    users: int
    duration: float = 60.0         # measured seconds
    warmup: float = 5.0            # excluded seconds after the ramp
    ramp: float = 5.0              # seconds over which users start
    think: tuple[float, float] = (0.0, 0.0)
    processes: int = 0             # 0 = auto
    connections: int = 0           # 0 = auto (min(users, 2000))
    socket_path: str = ""
    durability: str = "balanced"
    product_count: int = 5000
    account_count: int = 20000
    seed: int = 1
    run_tag: str = "run"
    keep_samples: bool = False
    sidecar_timeout: float = 60.0

    def resolved(self) -> "StageConfig":
        cfg = StageConfig(**asdict(self))
        cfg.think = tuple(self.think)
        if cfg.processes <= 0:
            cfg.processes = 1 if cfg.transport == "embedded" else max(1, min(os.cpu_count() or 4, cfg.users))
        if cfg.transport == "embedded":
            cfg.processes = 1
            if cfg.users > 3500:
                raise SystemExit("embedded: macOS caps a process at 4096 threads; "
                                 "use --transport sidecar for more than 3500 users")
        if cfg.connections <= 0:
            cfg.connections = min(cfg.users, 2000)
        cfg.connections = min(cfg.connections, cfg.users)
        return cfg


# ----------------------------------------------------------------- worker --


CONNECT_RETRY_SECONDS = 120.0


def _connect(cfg: StageConfig):
    if cfg.transport == "embedded":
        return drivers.open_embedded(cfg.db_path, cfg.durability)
    if cfg.transport == "sidecar":
        # A level with thousands of users opens its connections in a burst;
        # the server's listen backlog (128) then refuses some of them with
        # ECONNREFUSED. Retry with backoff, as a connection pool would, and
        # count the refusals so the report shows the storm.
        deadline = time.time() + CONNECT_RETRY_SECONDS
        delay = 0.02
        while True:
            try:
                return drivers.open_sidecar(cfg.socket_path, timeout=cfg.sidecar_timeout)
            except OSError as error:
                _connect_refusals[0] += 1
                if time.time() > deadline:
                    raise
                time.sleep(delay + random.random() * delay)
                delay = min(delay * 2, 1.0)
    if cfg.transport == "sqlite":
        return drivers.SqliteConnection(cfg.db_path, timeout_s=cfg.sidecar_timeout)
    raise ValueError(cfg.transport)


def _status_of(outcome: Optional[service.Outcome], error: Optional[BaseException]) -> str:
    if error is None:
        return outcome.business
    if isinstance(error, drivers.Conflict):
        return "conflict_exhausted"
    if isinstance(error, drivers.NotSupported):
        return "not_supported"
    code = getattr(error, "code", None)
    return f"error:{code}" if code is not None else f"error:{type(error).__name__}"


def worker_main(cfg_dict: dict, worker_index: int, user_indexes: list[int],
                pool_size: int, t_zero: float, out_path: str) -> None:
    """Entry point of one load-generator process."""
    cfg = StageConfig(**cfg_dict)
    cfg.think = tuple(cfg.think)
    threading.stack_size(512 * 1024)
    t_open = time.time()
    pool = drivers.ConnectionPool(lambda: _connect(cfg), size=pool_size,
                                  shared=cfg.transport == "embedded")
    t_end = t_zero + cfg.ramp + cfg.warmup + cfg.duration
    samples: list[list] = []
    samples_lock = threading.Lock()
    errors: dict[str, dict] = {}
    stats = {"reconnects": 0, "consistency_violations": 0,
             "connect_refusals": _connect_refusals[0], "pool_open_seconds": round(time.time() - t_open, 2)}
    stats_lock = threading.Lock()

    def run_user(vu: VirtualUser, start_delay: float) -> None:
        local: list[tuple] = []
        think_lo, think_hi = cfg.think
        rng = vu.rng
        wait_until = t_zero + start_delay
        now = time.time()
        if wait_until > now:
            time.sleep(wait_until - now)
        while time.time() < t_end:
            op = vu.next_op()
            t_req = time.perf_counter()
            conn = pool.acquire()
            t_got = time.perf_counter()
            outcome = error = None
            broken = False
            try:
                outcome = vu.step(conn, op)
            except BaseException as exc:  # noqa: BLE001 - every failure is a sample
                error = exc
                code = getattr(exc, "code", None)
                broken = cfg.transport == "sidecar" and code == 1
                key = _status_of(None, exc)
                with stats_lock:
                    entry = errors.setdefault(key, {"count": 0, "first": str(exc)[:300], "ops": {}})
                    entry["count"] += 1
                    entry["ops"][op] = entry["ops"].get(op, 0) + 1
                    if broken:
                        stats["reconnects"] += 1
            finally:
                try:
                    pool.release(conn, broken=broken)
                except Exception as exc:  # noqa: BLE001 - reconnect failed: keep going
                    with stats_lock:
                        entry = errors.setdefault("reconnect_failed", {"count": 0, "first": str(exc)[:300], "ops": {}})
                        entry["count"] += 1
                    pool.release(conn, broken=False)
            t_done = time.perf_counter()
            local.append((op, int((time.time() - t_zero) * 1000) - int((t_done - t_req) * 1000),
                          int((t_done - t_got) * 1e6), int((t_got - t_req) * 1e6),
                          _status_of(outcome, error), outcome.retries if outcome else 0))
            if think_hi > 0:
                time.sleep(rng.uniform(think_lo, think_hi))
        with samples_lock:
            samples.extend(local)
        with stats_lock:
            stats["consistency_violations"] += vu.consistency_violations

    threads = []
    total_users = cfg.users
    for k, index in enumerate(user_indexes):
        vu = VirtualUser(index=index, account=index % cfg.account_count,
                         product_count=cfg.product_count, seed=cfg.seed, run_tag=cfg.run_tag)
        delay = cfg.ramp * index / max(1, total_users)
        threads.append(threading.Thread(target=run_user, args=(vu, delay), daemon=True))
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    pool.close()
    with gzip.open(out_path, "wt", newline="") as fh:
        writer = csv.writer(fh)
        writer.writerow(SAMPLE_FIELDS)
        writer.writerows(samples)
    with open(out_path + ".meta.json", "w") as fh:
        json.dump({"worker": worker_index, "users": len(user_indexes), "pool": pool_size,
                   "errors": errors, **stats}, fh)


# ------------------------------------------------------------ monitoring --


def _ps(pids: list[int]) -> dict[int, tuple[int, float]]:
    """rss KiB and cumulative CPU seconds per pid (macOS/Linux ``ps``)."""
    if not pids:
        return {}
    try:
        out = subprocess.run(["ps", "-o", "pid=,rss=,cputime=", "-p", ",".join(map(str, pids))],
                             capture_output=True, text=True, timeout=5).stdout
    except (subprocess.SubprocessError, OSError):
        return {}
    result = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) != 3:
            continue
        pid, rss, cpu = int(parts[0]), int(parts[1]), parts[2]
        secs = 0.0
        days = 0
        if "-" in cpu:
            d, cpu = cpu.split("-", 1)
            days = int(d)
        for piece in cpu.split(":"):
            secs = secs * 60 + float(piece)
        result[pid] = (rss, secs + days * 86400)
    return result


class ResourceMonitor(threading.Thread):
    """Samples RSS and CPU of the server and of the load generators every second."""

    def __init__(self, server_pid: Optional[int], client_pids: list[int], interval: float = 1.0):
        super().__init__(daemon=True)
        self.server_pid = server_pid
        self.client_pids = list(client_pids)
        self.interval = interval
        self.samples: list[dict] = []
        self._stop = threading.Event()

    def run(self) -> None:
        pids = ([self.server_pid] if self.server_pid else []) + self.client_pids
        prev = _ps(pids)
        prev_t = time.time()
        while not self._stop.wait(self.interval):
            cur = _ps(pids)
            now = time.time()
            dt = max(1e-6, now - prev_t)
            def cpu(pid_list):
                return sum((cur.get(p, (0, 0.0))[1] - prev.get(p, (0, 0.0))[1]) for p in pid_list) / dt
            self.samples.append({
                "t": now,
                "server_rss_mib": round(cur.get(self.server_pid, (0, 0.0))[0] / 1024, 1) if self.server_pid else None,
                "server_cpu_cores": round(cpu([self.server_pid]), 2) if self.server_pid else None,
                "clients_rss_mib": round(sum(cur.get(p, (0, 0.0))[0] for p in self.client_pids) / 1024, 1),
                "clients_cpu_cores": round(cpu(self.client_pids), 2),
            })
            prev, prev_t = cur, now

    def stop(self) -> None:
        self._stop.set()


def dir_size_bytes(path: str) -> int:
    p = Path(path)
    if p.is_file():
        total = p.stat().st_size
        for extra in (p.with_name(p.name + "-wal"), p.with_name(p.name + "-shm")):
            if extra.exists():
                total += extra.stat().st_size
        return total
    return sum(f.stat().st_size for f in p.rglob("*") if f.is_file())


# ----------------------------------------------------------- aggregation --


def percentile(sorted_values: list[int], q: float) -> float:
    if not sorted_values:
        return float("nan")
    k = (len(sorted_values) - 1) * q
    lo, hi = math.floor(k), math.ceil(k)
    if lo == hi:
        return float(sorted_values[lo])
    return sorted_values[lo] + (sorted_values[hi] - sorted_values[lo]) * (k - lo)


def summarize_latencies(values: list[int]) -> dict:
    values.sort()
    if not values:
        return {"count": 0}
    return {
        "count": len(values),
        "mean_us": round(statistics.fmean(values), 1),
        "p50_us": round(percentile(values, 0.50), 1),
        "p90_us": round(percentile(values, 0.90), 1),
        "p95_us": round(percentile(values, 0.95), 1),
        "p99_us": round(percentile(values, 0.99), 1),
        "p999_us": round(percentile(values, 0.999), 1),
        "max_us": values[-1],
    }


def aggregate(stage_dir: Path, cfg: StageConfig, monitor_samples: list[dict],
              wall: dict, db_bytes: tuple[int, int]) -> dict:
    """Read every worker's samples, keep the measured window, compute metrics."""
    window_start_ms = int((cfg.ramp + cfg.warmup) * 1000)
    window_end_ms = window_start_ms + int(cfg.duration * 1000)
    all_lat: list[int] = []
    all_wait: list[int] = []
    per_op: dict[str, dict] = {}
    per_second: dict[int, list[int]] = {}
    statuses: dict[str, int] = {}
    retries_total = 0
    ops_with_retry = 0
    errors: dict[str, dict] = {}
    reconnects = 0
    violations = 0
    connect_refusals = 0
    pool_open_seconds = 0.0
    total_in_window = 0
    total_all = 0
    read_ops = write_ops = 0
    for sample_file in sorted(stage_dir.glob("samples-*.csv.gz")):
        with open(str(sample_file) + ".meta.json") as fh:
            meta = json.load(fh)
        reconnects += meta.get("reconnects", 0)
        violations += meta.get("consistency_violations", 0)
        connect_refusals += meta.get("connect_refusals", 0)
        pool_open_seconds = max(pool_open_seconds, meta.get("pool_open_seconds", 0.0))
        for key, entry in meta.get("errors", {}).items():
            slot = errors.setdefault(key, {"count": 0, "first": entry.get("first"), "ops": {}})
            slot["count"] += entry["count"]
            for op, n in entry.get("ops", {}).items():
                slot["ops"][op] = slot["ops"].get(op, 0) + n
        with gzip.open(sample_file, "rt", newline="") as fh:
            reader = csv.reader(fh)
            next(reader, None)
            for op, start_ms, latency_us, wait_us, status, retries in reader:
                total_all += 1
                start_ms = int(start_ms)
                if not (window_start_ms <= start_ms < window_end_ms):
                    continue
                total_in_window += 1
                latency_us, wait_us, retries = int(latency_us), int(wait_us), int(retries)
                all_lat.append(latency_us)
                all_wait.append(wait_us)
                slot = per_op.setdefault(op, {"lat": [], "wait": [], "statuses": {}, "retries": 0})
                slot["lat"].append(latency_us)
                slot["wait"].append(wait_us)
                slot["statuses"][status] = slot["statuses"].get(status, 0) + 1
                slot["retries"] += retries
                statuses[status] = statuses.get(status, 0) + 1
                retries_total += retries
                ops_with_retry += 1 if retries else 0
                per_second.setdefault((start_ms - window_start_ms) // 1000, []).append(latency_us + wait_us)
                if op in READ_OPS:
                    read_ops += 1
                else:
                    write_ops += 1
        if not cfg.keep_samples:
            sample_file.unlink()
            Path(str(sample_file) + ".meta.json").unlink()

    duration = cfg.duration
    ok = sum(n for s, n in statuses.items() if not s.startswith("error") and s not in ("conflict_exhausted", "not_supported"))
    failed = total_in_window - ok
    per_op_rows = []
    for op, slot in sorted(per_op.items()):
        lat = summarize_latencies(slot["lat"])
        wait = summarize_latencies(slot["wait"])
        errs = sum(n for s, n in slot["statuses"].items() if s.startswith("error") or s == "conflict_exhausted")
        per_op_rows.append({
            "op": op, "count": lat["count"], "ops_per_s": round(lat["count"] / duration, 2),
            "share_pct": round(100 * lat["count"] / max(1, total_in_window), 2),
            **{k: v for k, v in lat.items() if k != "count"},
            "pool_wait_p99_us": wait.get("p99_us"),
            "retries": slot["retries"], "errors": errs,
            "business": {s: n for s, n in slot["statuses"].items() if s not in ("ok",)},
        })
    series = []
    for sec in range(int(duration)):
        vals = sorted(per_second.get(sec, []))
        series.append({"second": sec, "ops": len(vals),
                       "p50_ms": round(percentile(vals, 0.5) / 1000, 3) if vals else None,
                       "p99_ms": round(percentile(vals, 0.99) / 1000, 3) if vals else None,
                       "max_ms": round(vals[-1] / 1000, 3) if vals else None})
    per_second_ops = [s["ops"] for s in series]
    stalls = sum(1 for s in series if s["ops"] == 0)

    def mon(key):
        vals = [s[key] for s in monitor_samples if s.get(key) is not None]
        return {"mean": round(statistics.fmean(vals), 2), "max": round(max(vals), 2)} if vals else None

    lat_summary = summarize_latencies(all_lat)
    wait_summary = summarize_latencies(all_wait)
    summary = {
        "users": cfg.users, "transport": cfg.transport, "processes": cfg.processes,
        "connections": cfg.connections, "think_s": list(cfg.think), "duration_s": duration,
        "durability": cfg.durability,
        "ops_total_window": total_in_window, "ops_total_including_rampup": total_all,
        "throughput_ops_s": round(total_in_window / duration, 1),
        "throughput_per_user": round(total_in_window / duration / max(1, cfg.users), 3),
        "read_ops_s": round(read_ops / duration, 1), "write_ops_s": round(write_ops / duration, 1),
        "latency": lat_summary, "pool_wait": wait_summary,
        "user_latency_p99_us": round(percentile(sorted(a + b for a, b in zip(all_lat, all_wait)), 0.99), 1) if all_lat else None,
        "success_rate_pct": round(100 * ok / max(1, total_in_window), 3),
        "failed_ops": failed,
        "conflict_retries": retries_total, "ops_that_retried": ops_with_retry,
        "conflict_retry_rate_pct": round(100 * ops_with_retry / max(1, total_in_window), 3),
        "statuses": statuses, "errors": errors, "reconnects": reconnects,
        "connect_refusals": connect_refusals, "pool_open_seconds": pool_open_seconds,
        "consistency_violations": violations,
        "throughput_per_second_cv": round(statistics.pstdev(per_second_ops) / statistics.fmean(per_second_ops), 3) if per_second_ops and statistics.fmean(per_second_ops) else None,
        "seconds_with_zero_ops": stalls,
        "server_cpu_cores": mon("server_cpu_cores"), "server_rss_mib": mon("server_rss_mib"),
        "clients_cpu_cores": mon("clients_cpu_cores"), "clients_rss_mib": mon("clients_rss_mib"),
        "db_bytes_before": db_bytes[0], "db_bytes_after": db_bytes[1],
        "wall": wall,
        "per_op": per_op_rows,
    }
    with open(stage_dir / "summary.json", "w") as fh:
        json.dump(summary, fh, indent=1, default=str)
    with open(stage_dir / "per-op.csv", "w", newline="") as fh:
        fields = [k for k in per_op_rows[0].keys() if k != "business"] if per_op_rows else ["op"]
        writer = csv.DictWriter(fh, fieldnames=fields + ["business"])
        writer.writeheader()
        for row in per_op_rows:
            writer.writerow({**row, "business": json.dumps(row["business"])})
    with open(stage_dir / "timeseries.csv", "w", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=["second", "ops", "p50_ms", "p99_ms", "max_ms"])
        writer.writeheader()
        writer.writerows(series)
    with open(stage_dir / "resources.csv", "w", newline="") as fh:
        if monitor_samples:
            writer = csv.DictWriter(fh, fieldnames=list(monitor_samples[0].keys()))
            writer.writeheader()
            writer.writerows(monitor_samples)
    return summary


# ------------------------------------------------------------------ stage --


def _engine_stats(cfg: StageConfig) -> Optional[dict]:
    """Cumulative engine counters from the sidecar (`{"op":"stats"}`), or None."""
    if cfg.transport != "sidecar":
        return None
    try:
        from elitesql import SidecarClient
        client = SidecarClient(cfg.socket_path, timeout=30)
        try:
            return client._call({"op": "stats"})
        finally:
            client.close()
    except Exception:  # noqa: BLE001 - an older server has no stats op
        return None


def _stats_delta(before: Optional[dict], after: Optional[dict]) -> Optional[dict]:
    if not before or not after:
        return None
    delta = {k: after[k] - before[k] for k in after if isinstance(after[k], (int, float)) and k in before}
    commits = max(1, delta.get("commits", 0))
    per_commit = {k.replace("_us", "_per_commit_us"): round(v / commits, 1)
                  for k, v in delta.items() if k.startswith("commit_") and k.endswith("_us")}
    return {"delta": delta, "per_commit": per_commit}


def run_stage(cfg: StageConfig, stage_dir: Path, server_pid: Optional[int] = None) -> dict:
    cfg = cfg.resolved()
    stage_dir.mkdir(parents=True, exist_ok=True)
    for old in stage_dir.glob("samples-*"):
        old.unlink()
    ctx = mp.get_context("spawn")
    per_process = [list(range(cfg.users))[i::cfg.processes] for i in range(cfg.processes)]
    pool_sizes = []
    remaining = cfg.connections
    for i, users in enumerate(per_process):
        share = math.ceil(remaining / (cfg.processes - i))
        size = max(1, min(len(users), share))
        pool_sizes.append(size)
        remaining -= size
    db_before = dir_size_bytes(cfg.db_path)
    # Workers need time to import and to open their share of the connections
    # (a burst can be refused and retried; see _connect).
    t_zero = time.time() + 1.5 + 0.25 * cfg.processes + cfg.connections / 400
    procs = []
    for i, users in enumerate(per_process):
        if not users:
            continue
        p = ctx.Process(target=worker_main,
                        args=(asdict(cfg), i, users, pool_sizes[i], t_zero,
                              str(stage_dir / f"samples-{i:02d}.csv.gz")), daemon=False)
        p.start()
        procs.append(p)
    LIVE_PROCESSES[:] = procs
    monitor = ResourceMonitor(server_pid if cfg.transport == "sidecar" else None,
                              [p.pid for p in procs])
    # Let the workers import and connect before the first CPU sample.
    time.sleep(max(0.0, t_zero - time.time()))
    monitor.start()
    t_start = time.time()
    stats_before = _engine_stats(cfg)
    for p in procs:
        p.join()
    stats_after = _engine_stats(cfg)
    LIVE_PROCESSES.clear()
    monitor.stop()
    wall = {"stage_seconds": round(time.time() - t_start, 2),
            "expected_seconds": cfg.ramp + cfg.warmup + cfg.duration}
    failed = [p.exitcode for p in procs if p.exitcode]
    if failed:
        raise RuntimeError(f"{len(failed)} load-generator processes failed: exit codes {failed}")
    # The measured window only counts samples whose start fell inside it; the
    # monitor covers the whole stage, so keep the samples of the window too.
    window_lo = t_zero + cfg.ramp + cfg.warmup
    window_hi = window_lo + cfg.duration
    monitor_window = [s for s in monitor.samples if window_lo <= s["t"] < window_hi] or monitor.samples
    db_after = dir_size_bytes(cfg.db_path)
    summary = aggregate(stage_dir, cfg, monitor_window, wall, (db_before, db_after))
    summary["engine_stats"] = _stats_delta(stats_before, stats_after)
    (stage_dir / "summary.json").write_text(json.dumps(summary, indent=1, default=str))
    return summary


# ------------------------------------------------------------ preparation --


def prepare_database(transport: str, db_path: str, durability: str, products: int,
                     accounts: int, elitesql_bin: Optional[str] = None,
                     scenario: str = "baseline") -> dict:
    """Create and seed a fresh database; returns seed info + initial stock."""
    path = Path(db_path)
    if path.exists():
        shutil.rmtree(path) if path.is_dir() else path.unlink()
        for extra in (path.with_name(path.name + "-wal"), path.with_name(path.name + "-shm")):
            if extra.exists():
                extra.unlink()
    if transport == "sqlite":
        conn = drivers.SqliteConnection(db_path)
        sqlite = True
    else:
        conn = drivers.open_embedded(db_path, durability)
        sqlite = False
    service.create_schema(conn, sqlite=sqlite)
    info = service.seed(conn, users=accounts, products=products, sqlite=sqlite)
    if scenario == "compound-index":
        ddl = "CREATE INDEX ON products (category, price_cents)"
        conn.execute(drivers.sqlite_ddl(ddl) if sqlite else ddl)
    initial = {pid: stock for pid, stock in conn.execute("SELECT id, stock FROM products LIMIT 10000").rows}
    conn.checkpoint()
    conn.close()
    info["initial_stock"] = initial
    info["db_bytes"] = dir_size_bytes(db_path)
    return info


def verify(transport: str, db_path: str, socket_path: str, initial_stock: dict[int, int]) -> dict:
    if transport == "sidecar":
        conn = drivers.open_sidecar(socket_path, timeout=120)
    elif transport == "sqlite":
        conn = drivers.SqliteConnection(db_path)
    else:
        conn = drivers.open_embedded(db_path)
    try:
        t0 = time.perf_counter()
        result = service.invariants(conn, initial_stock)
        result["seconds"] = round(time.perf_counter() - t0, 2)
        return result
    finally:
        conn.close()
