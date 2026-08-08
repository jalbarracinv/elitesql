#!/usr/bin/env python3
"""Render dependency-free SVG charts from concurrent_writers benchmark CSV."""

from __future__ import annotations

import argparse
import csv
import html
import statistics
from collections import defaultdict
from pathlib import Path


WIDTH = 920
HEIGHT = 520
LEFT = 88
RIGHT = 36
TOP = 82
BOTTOM = 70
PLOT_WIDTH = WIDTH - LEFT - RIGHT
PLOT_HEIGHT = HEIGHT - TOP - BOTTOM
ENGINES = ("EliteSQL", "SQLite")
COLORS = {"EliteSQL": "#c99700", "SQLite": "#0969da"}


def args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("csv", type=Path, help="CSV emitted by concurrent_writers")
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("benchmark-results"),
        help="destination directory",
    )
    return parser.parse_args()


def load(path: Path) -> tuple[list[dict[str, str]], list[int]]:
    with path.open(newline="", encoding="utf-8") as source:
        rows = list(csv.DictReader(source))
    if not rows:
        raise SystemExit(f"no benchmark rows in {path}")
    writers = sorted({int(row["writers"]) for row in rows})
    for engine in ENGINES:
        missing = [
            writer
            for writer in writers
            if not any(row["engine"] == engine and int(row["writers"]) == writer for row in rows)
        ]
        if missing:
            raise SystemExit(f"missing {engine} results for writers={missing}")
    return rows, writers


def medians(rows: list[dict[str, str]], field: str) -> dict[tuple[str, int], float]:
    grouped: dict[tuple[str, int], list[float]] = defaultdict(list)
    for row in rows:
        grouped[(row["engine"], int(row["writers"]))].append(float(row[field]))
    return {key: statistics.median(values) for key, values in grouped.items()}


def compact_number(value: float) -> str:
    if value >= 1_000_000:
        return f"{value / 1_000_000:.2f}M"
    if value >= 1_000:
        return f"{value / 1_000:.0f}k"
    return f"{value:.0f}"


def latency_ms(value_us: float) -> str:
    value = value_us / 1_000
    return f"{value:.1f} ms" if value >= 10 else f"{value:.2f} ms"


def svg_chart(
    *,
    title: str,
    description: str,
    subtitle: str,
    writers: list[int],
    values: dict[tuple[str, int], float],
    y_label: str,
    tick_format,
    value_format,
) -> str:
    maximum = max(values.values())
    y_max = maximum * 1.18 if maximum else 1.0
    x_step = PLOT_WIDTH / max(1, len(writers) - 1)

    def x(index: int) -> float:
        return LEFT + (x_step * index if len(writers) > 1 else PLOT_WIDTH / 2)

    def y(value: float) -> float:
        return TOP + PLOT_HEIGHT - value / y_max * PLOT_HEIGHT

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" role="img" viewBox="0 0 {WIDTH} {HEIGHT}">',
        f"<title>{html.escape(title)}</title>",
        f"<desc>{html.escape(description)}</desc>",
        """<style>
          :root { color-scheme: light dark; }
          .title { font: 600 22px system-ui, sans-serif; fill: #1f2328; }
          .subtitle, .axis-label, .tick, .value, .legend { font-family: system-ui, sans-serif; fill: #57606a; }
          .subtitle { font-size: 13px; }
          .axis-label { font-size: 13px; font-weight: 600; }
          .tick, .legend { font-size: 12px; }
          .value { font-size: 11px; font-weight: 600; }
          .grid { stroke: #d0d7de; stroke-width: 1; }
          .axis { stroke: #8c959f; stroke-width: 1.25; }
          .series { fill: none; stroke-width: 3; stroke-linejoin: round; stroke-linecap: round; }
          .point { stroke: Canvas; stroke-width: 2; }
          @media (prefers-color-scheme: dark) {
            .title { fill: #f0f6fc; }
            .subtitle, .axis-label, .tick, .value, .legend { fill: #b1bac4; }
            .grid { stroke: #30363d; }
            .axis { stroke: #6e7681; }
          }
        </style>""",
        f'<text class="title" x="{LEFT}" y="32">{html.escape(title)}</text>',
        f'<text class="subtitle" x="{LEFT}" y="55">{html.escape(subtitle)}</text>',
    ]

    for tick in range(6):
        value = y_max * tick / 5
        py = y(value)
        parts.append(
            f'<line class="grid" x1="{LEFT}" y1="{py:.1f}" x2="{WIDTH - RIGHT}" y2="{py:.1f}" />'
        )
        parts.append(
            f'<text class="tick" x="{LEFT - 12}" y="{py + 4:.1f}" text-anchor="end">{html.escape(tick_format(value))}</text>'
        )

    parts.extend(
        [
            f'<line class="axis" x1="{LEFT}" y1="{TOP}" x2="{LEFT}" y2="{TOP + PLOT_HEIGHT}" />',
            f'<line class="axis" x1="{LEFT}" y1="{TOP + PLOT_HEIGHT}" x2="{WIDTH - RIGHT}" y2="{TOP + PLOT_HEIGHT}" />',
            f'<text class="axis-label" x="{LEFT + PLOT_WIDTH / 2:.1f}" y="{HEIGHT - 18}" text-anchor="middle">Concurrent writers</text>',
            f'<text class="axis-label" transform="translate(22 {TOP + PLOT_HEIGHT / 2:.1f}) rotate(-90)" text-anchor="middle">{html.escape(y_label)}</text>',
        ]
    )

    for index, writer in enumerate(writers):
        px = x(index)
        parts.append(
            f'<text class="tick" x="{px:.1f}" y="{TOP + PLOT_HEIGHT + 25}" text-anchor="middle">{writer}</text>'
        )

    for engine in ENGINES:
        color = COLORS[engine]
        points = " ".join(
            f"{x(index):.1f},{y(values[(engine, writer)]):.1f}"
            for index, writer in enumerate(writers)
        )
        parts.append(f'<polyline class="series" stroke="{color}" points="{points}" />')
        for index, writer in enumerate(writers):
            value = values[(engine, writer)]
            px = x(index)
            py = y(value)
            label_y = py - 13 if engine == "EliteSQL" else py + 22
            label_x = px
            anchor = "middle"
            if index == 0:
                label_x += 12
                anchor = "start"
            elif index == len(writers) - 1:
                label_x -= 12
                anchor = "end"
            parts.append(f'<circle class="point" fill="{color}" cx="{px:.1f}" cy="{py:.1f}" r="5" />')
            parts.append(
                f'<text class="value" x="{label_x:.1f}" y="{label_y:.1f}" text-anchor="{anchor}">{html.escape(value_format(value))}</text>'
            )

    legend_x = WIDTH - RIGHT - 190
    for index, engine in enumerate(ENGINES):
        ly = 27 + index * 23
        parts.append(
            f'<line x1="{legend_x}" y1="{ly}" x2="{legend_x + 25}" y2="{ly}" stroke="{COLORS[engine]}" stroke-width="3" />'
        )
        parts.append(
            f'<text class="legend" x="{legend_x + 34}" y="{ly + 4}">{engine}</text>'
        )

    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def main() -> None:
    options = args()
    rows, writers = load(options.csv)
    options.output_dir.mkdir(parents=True, exist_ok=True)
    first = rows[0]
    repetitions = len(
        {
            row["repetition"]
            for row in rows
            if row["engine"] == first["engine"] and row["writers"] == first["writers"]
        }
    )
    subtitle = (
        f'{int(first["rows"]):,} total rows · {first["batch_size"]} rows/transaction · '
        f'{repetitions} runs · median · {first["durability"]}'
    )

    throughput = medians(rows, "rows_per_second")
    throughput_svg = svg_chart(
        title="Concurrent write throughput",
        description="Median rows per second for EliteSQL and SQLite with one, two, four and eight concurrent writers.",
        subtitle=subtitle,
        writers=writers,
        values=throughput,
        y_label="Rows per second",
        tick_format=compact_number,
        value_format=compact_number,
    )
    (options.output_dir / "concurrent-throughput.svg").write_text(
        throughput_svg, encoding="utf-8"
    )

    p99 = medians(rows, "p99_us")
    latency_svg = svg_chart(
        title="p99 transaction latency",
        description="Median p99 transaction latency for EliteSQL and SQLite with one, two, four and eight concurrent writers.",
        subtitle=subtitle,
        writers=writers,
        values=p99,
        y_label="p99 transaction latency",
        tick_format=latency_ms,
        value_format=latency_ms,
    )
    (options.output_dir / "concurrent-p99-latency.svg").write_text(
        latency_svg, encoding="utf-8"
    )

    maximum_latency = medians(rows, "max_us")
    maximum_latency_svg = svg_chart(
        title="Worst concurrent transaction latency",
        description="Median of the maximum transaction latency in each run for EliteSQL and SQLite with one, two, four and eight concurrent writers.",
        subtitle=subtitle,
        writers=writers,
        values=maximum_latency,
        y_label="Maximum transaction latency",
        tick_format=latency_ms,
        value_format=latency_ms,
    )
    (options.output_dir / "concurrent-max-latency.svg").write_text(
        maximum_latency_svg, encoding="utf-8"
    )

    print(options.output_dir / "concurrent-throughput.svg")
    print(options.output_dir / "concurrent-p99-latency.svg")
    print(options.output_dir / "concurrent-max-latency.svg")


if __name__ == "__main__":
    main()
