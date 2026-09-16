#!/usr/bin/env python3
"""Dependency-free SVG charts for one or several simulation runs.

    python3 plot.py results/sidecar-... [results/sqlite-...] [--out DIR]

Each run directory must contain ``stages.csv`` (written by ``sweep.py``);
per-operation and time-series charts also read ``stage-*/summary.json``.
"""

from __future__ import annotations

import argparse
import csv
import html
import json
import math
from pathlib import Path

W, H = 1080, 480
LEFT, RIGHT, TOP, BOTTOM = 84, 250, 60, 64
PW, PH = W - LEFT - RIGHT, H - TOP - BOTTOM
PALETTE = ["#c99700", "#0969da", "#cf222e", "#1a7f37", "#8250df", "#bf3989", "#0a7ea4",
           "#9a6700", "#57606a", "#e16f24", "#2da44e", "#6e40c9", "#b35900", "#218bff",
           "#a40e26", "#116329"]


def _nice_ticks(lo: float, hi: float, n: int = 6) -> list[float]:
    if hi <= lo:
        hi = lo + 1
    span = hi - lo
    step = 10 ** math.floor(math.log10(span / n))
    for mult in (1, 2, 2.5, 5, 10):
        if span / (step * mult) <= n:
            step *= mult
            break
    start = math.floor(lo / step) * step
    ticks = []
    value = start
    while value <= hi + step * 0.5:
        ticks.append(round(value, 10))
        value += step
    return ticks


def _fmt(v: float) -> str:
    if v == 0:
        return "0"
    if abs(v) >= 1000:
        return f"{v / 1000:g}k" if v % 1000 == 0 or abs(v) >= 10000 else f"{v:,.0f}"
    if abs(v) < 0.01:
        return f"{v:.3g}"
    return f"{v:g}"


def line_chart(path: Path, title: str, series: dict[str, list[tuple[float, float]]],
               xlabel: str, ylabel: str, logx: bool = True, logy: bool = False,
               subtitle: str = "", dashed: set[str] | None = None) -> None:
    dashed = dashed or set()
    xs = [x for pts in series.values() for x, _ in pts]
    ys = [y for pts in series.values() for _, y in pts if y is not None and not math.isnan(y)]
    if not xs or not ys:
        return
    if logy:
        ys = [y for y in ys if y > 0] or [1.0]

    def fx(x: float) -> float:
        if logx:
            lo, hi = math.log10(min(xs)), math.log10(max(xs))
            return LEFT + (0 if hi == lo else (math.log10(x) - lo) / (hi - lo)) * PW
        lo, hi = min(xs), max(xs)
        return LEFT + (0 if hi == lo else (x - lo) / (hi - lo)) * PW

    if logy:
        ylo, yhi = math.floor(math.log10(min(ys))), math.ceil(math.log10(max(ys)))
        if yhi == ylo:
            yhi += 1
        yticks = [10 ** e for e in range(ylo, yhi + 1)]
    else:
        yticks = _nice_ticks(0, max(ys))
        ylo, yhi = yticks[0], yticks[-1]

    def fy(y: float) -> float:
        if logy:
            return TOP + PH - (math.log10(max(y, 10 ** ylo)) - ylo) / (yhi - ylo) * PH
        return TOP + PH - (y - ylo) / (yhi - ylo) * PH

    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" '
           f'font-family="ui-sans-serif, system-ui, sans-serif" font-size="12">',
           f'<rect width="{W}" height="{H}" fill="#ffffff"/>',
           f'<text x="{LEFT}" y="26" font-size="17" font-weight="600" fill="#1f2328">{html.escape(title)}</text>']
    if subtitle:
        out.append(f'<text x="{LEFT}" y="44" fill="#57606a">{html.escape(subtitle)}</text>')
    # grid + y ticks
    for t in yticks:
        y = fy(t)
        out.append(f'<line x1="{LEFT}" y1="{y:.1f}" x2="{LEFT + PW}" y2="{y:.1f}" stroke="#eaeef2"/>')
        out.append(f'<text x="{LEFT - 8}" y="{y + 4:.1f}" text-anchor="end" fill="#57606a">{_fmt(t)}</text>')
    xticks = sorted(set(xs))
    if not logx and len(xticks) > 12:
        xticks = _nice_ticks(min(xs), max(xs), 8)
    for t in xticks:
        x = fx(t)
        out.append(f'<line x1="{x:.1f}" y1="{TOP}" x2="{x:.1f}" y2="{TOP + PH}" stroke="#f0f2f4"/>')
        out.append(f'<text x="{x:.1f}" y="{TOP + PH + 18}" text-anchor="middle" fill="#57606a">{_fmt(t)}</text>')
    out.append(f'<rect x="{LEFT}" y="{TOP}" width="{PW}" height="{PH}" fill="none" stroke="#d0d7de"/>')
    out.append(f'<text x="{LEFT + PW / 2}" y="{H - 14}" text-anchor="middle" fill="#57606a">{html.escape(xlabel)}</text>')
    out.append(f'<text x="18" y="{TOP + PH / 2}" text-anchor="middle" fill="#57606a" '
               f'transform="rotate(-90 18 {TOP + PH / 2})">{html.escape(ylabel)}</text>')
    legend_y = TOP + 8
    for i, (name, pts) in enumerate(series.items()):
        color = PALETTE[i % len(PALETTE)]
        pts = [(x, y) for x, y in pts if y is not None and not math.isnan(y) and (not logy or y > 0)]
        if not pts:
            continue
        d = " ".join(f"{'M' if j == 0 else 'L'}{fx(x):.1f},{fy(y):.1f}" for j, (x, y) in enumerate(pts))
        dash = ' stroke-dasharray="6 4"' if name in dashed else ""
        out.append(f'<path d="{d}" fill="none" stroke="{color}" stroke-width="2"{dash}/>')
        for x, y in pts:
            out.append(f'<circle cx="{fx(x):.1f}" cy="{fy(y):.1f}" r="3" fill="{color}">'
                       f'<title>{html.escape(name)}: {_fmt(x)} → {y:g}</title></circle>')
        lx = LEFT + PW + 18
        ly = legend_y + i * 16
        out.append(f'<line x1="{lx}" y1="{ly}" x2="{lx + 22}" y2="{ly}" stroke="{color}" stroke-width="2"{dash}/>')
        out.append(f'<text x="{lx + 28}" y="{ly + 4}" fill="#1f2328">{html.escape(name)}</text>')
    out.append("</svg>")
    path.write_text("\n".join(out))


def read_stages(run_dir: Path) -> list[dict]:
    with open(run_dir / "stages.csv", newline="") as fh:
        rows = list(csv.DictReader(fh))
    for r in rows:
        for k, v in r.items():
            try:
                r[k] = float(v) if v not in ("", "None", "True", "False") else (v == "True" if v in ("True", "False") else None)
            except ValueError:
                pass
        r["users"] = int(r["users"])
    return rows


def read_summaries(run_dir: Path) -> list[dict]:
    result = []
    for path in sorted(run_dir.glob("stage-*/summary.json")):
        with open(path) as fh:
            result.append(json.load(fh))
    return result


def make_plots(run_dirs: list[Path], out: Path) -> None:
    out.mkdir(parents=True, exist_ok=True)
    runs = {}
    for d in run_dirs:
        if (d / "stages.csv").exists():
            runs[d.name] = read_stages(d)
    if not runs:
        return
    sub = "concurrent virtual users, closed loop"
    line_chart(out / "throughput.svg", "Throughput vs concurrent users",
               {name: [(r["users"], r["throughput_ops_s"]) for r in rows] for name, rows in runs.items()},
               "concurrent users (log)", "operations / s", subtitle=sub)
    latency = {}
    dashed = set()
    for name, rows in runs.items():
        latency[f"{name} p99"] = [(r["users"], r["p99_ms"]) for r in rows]
        latency[f"{name} p50"] = [(r["users"], r["p50_ms"]) for r in rows]
        dashed.add(f"{name} p50")
    line_chart(out / "latency.svg", "Operation latency vs concurrent users", latency,
               "concurrent users (log)", "latency ms (log)", logy=True, dashed=dashed,
               subtitle="database call only; p50 dashed, p99 solid")
    line_chart(out / "user-latency.svg", "User-perceived p99 (incl. wait for a pooled connection)",
               {name: [(r["users"], r["user_p99_ms"]) for r in rows] for name, rows in runs.items()},
               "concurrent users (log)", "latency ms (log)", logy=True)
    errs = {}
    for name, rows in runs.items():
        errs[f"{name} failed %"] = [(r["users"], 100 - r["success_rate_pct"]) for r in rows]
        errs[f"{name} conflict retry %"] = [(r["users"], r["conflict_retry_rate_pct"]) for r in rows]
        dashed.add(f"{name} conflict retry %")
    line_chart(out / "errors.svg", "Failures and optimistic-commit retries", errs,
               "concurrent users (log)", "% of operations", dashed=dashed)
    cpu = {}
    for name, rows in runs.items():
        if any(r.get("server_cpu_cores") for r in rows):
            cpu[f"{name} server"] = [(r["users"], r["server_cpu_cores"]) for r in rows]
        cpu[f"{name} load generators"] = [(r["users"], r["clients_cpu_cores"]) for r in rows]
        dashed.add(f"{name} load generators")
    line_chart(out / "cpu.svg", "CPU used (cores) during the measured window", cpu,
               "concurrent users (log)", "cores", dashed=dashed)
    eff = {}
    for name, rows in runs.items():
        pts = [(r["users"], r["throughput_ops_s"] / r["server_cpu_cores"])
               for r in rows if r.get("server_cpu_cores")]
        if pts:
            eff[name] = pts
    if eff:
        line_chart(out / "efficiency.svg", "Operations per server core-second", eff,
                   "concurrent users (log)", "ops / core / s")
    rss = {name: [(r["users"], r["server_rss_mib_max"]) for r in rows if r.get("server_rss_mib_max")]
           for name, rows in runs.items()}
    rss = {k: v for k, v in rss.items() if v}
    if rss:
        line_chart(out / "memory.svg", "Server peak RSS", rss, "concurrent users (log)", "MiB")
    line_chart(out / "db-size.svg", "Database size after each stage",
               {name: [(r["users"], r["db_mib_after"]) for r in rows] for name, rows in runs.items()},
               "concurrent users (log)", "MiB")
    # per-operation p99, one chart per run
    for d in run_dirs:
        summaries = read_summaries(d)
        if not summaries:
            continue
        per_op: dict[str, list[tuple[float, float]]] = {}
        for s in summaries:
            for row in s["per_op"]:
                if row.get("p99_us") is not None:
                    per_op.setdefault(row["op"], []).append((s["users"], row["p99_us"] / 1000))
        per_op = dict(sorted(per_op.items(), key=lambda kv: -kv[1][-1][1]))
        suffix = "" if len(run_dirs) == 1 else f"-{d.name}"
        line_chart(out / f"per-op-p99{suffix}.svg", f"p99 latency per operation — {d.name}", per_op,
                   "concurrent users (log)", "p99 ms (log)", logy=True)
        for s in summaries:
            ts_path = d / f"stage-{s['users']:05d}" / "timeseries.csv"
            if not ts_path.exists():
                continue
            with open(ts_path, newline="") as fh:
                ts = [r for r in csv.DictReader(fh)]
            ops = [(int(r["second"]), float(r["ops"])) for r in ts]
            p99 = [(int(r["second"]), float(r["p99_ms"])) for r in ts if r["p99_ms"] not in ("", "None")]
            line_chart(out / f"timeseries-{s['users']:05d}{suffix}.svg",
                       f"Per-second throughput and p99 — {s['users']} users ({d.name})",
                       {"ops/s": ops, "p99 ms": p99}, "second of the measured window", "ops/s · ms",
                       logx=False, dashed={"p99 ms"})


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("runs", nargs="+", type=Path)
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()
    make_plots(args.runs, args.out or args.runs[0])


if __name__ == "__main__":
    main()
