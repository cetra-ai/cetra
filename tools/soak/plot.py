"""Plots and a summary table of a storage soak run (track R M7, R7.7).

Reads `<out>/samples.jsonl` (or `.gz`) and `<out>/run.json`, writes nine PNGs to
`<out>/plots/` and prints a Markdown table of the post-warm-up series. The data is
the soak's own: counts and sizes, never payload.
"""

from __future__ import annotations

import argparse
import gzip
import json
import statistics
from pathlib import Path
from typing import Any

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

Sample = dict[str, Any]
Series = tuple[str, list[float], list[float]]

GIB = 1024.0**3
MIB = 1024.0**2
WIDTH_PX = 1200
HEIGHT_PX = 500
DPI = 100
QUERY_KINDS = ["list", "filter", "detail", "sessions", "metrics", "scores"]


def load_samples(out: Path) -> list[Sample]:
    """Every sample line, from `samples.jsonl` or `samples.jsonl.gz`."""
    gz = out / "samples.jsonl.gz"
    plain = out / "samples.jsonl"
    if gz.is_file():
        with gzip.open(gz, "rt", encoding="utf-8") as handle:
            return [json.loads(line) for line in handle if line.strip()]
    with plain.open(encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def run_warmup_hours(out: Path) -> float:
    """`warmup_hours` from `run.json`, or 0 when the file is missing."""
    run_json = out / "run.json"
    if not run_json.is_file():
        return 0.0
    run: dict[str, Any] = json.loads(run_json.read_text(encoding="utf-8"))
    return float(run.get("config", {}).get("warmup_hours", 0.0))


def hours(samples: list[Sample]) -> list[float]:
    return [float(sample["elapsed_s"]) / 3600.0 for sample in samples]


def values(samples: list[Sample], key: str, scale: float = 1.0) -> list[float]:
    """One field across the samples; a missing or null field is 0."""
    return [float(sample.get(key) or 0.0) / scale for sample in samples]


def level_values(samples: list[Sample], table: str, level: str) -> list[float]:
    """L0 or L1 file counts of one table; 0 when the table is absent."""
    return [
        float(sample.get("files", {}).get(table, {}).get(level, 0)) for sample in samples
    ]


def interval_rate(samples: list[Sample], key: str) -> tuple[list[float], list[float]]:
    """Per-interval rate of a cumulative counter: (hours at the interval end, per second)."""
    xs: list[float] = []
    ys: list[float] = []
    for previous, current in zip(samples, samples[1:], strict=False):
        dt = float(current["elapsed_s"]) - float(previous["elapsed_s"])
        if dt <= 0:
            continue
        delta = float(current.get(key) or 0) - float(previous.get(key) or 0)
        xs.append(float(current["elapsed_s"]) / 3600.0)
        ys.append(delta / dt)
    return xs, ys


def hit_ratio(samples: list[Sample]) -> tuple[list[float], list[float]]:
    """Cache hit ratio per interval; empty for runs without a read cache (`cache: null`)."""
    xs: list[float] = []
    ys: list[float] = []
    for previous, current in zip(samples, samples[1:], strict=False):
        before = previous.get("cache") or {}
        after = current.get("cache") or {}
        if not after:
            continue
        hits = float(after.get("hits", 0)) - float(before.get("hits", 0))
        misses = float(after.get("misses", 0)) - float(before.get("misses", 0))
        if hits + misses <= 0:
            continue
        xs.append(float(current["elapsed_s"]) / 3600.0)
        ys.append(hits / (hits + misses))
    return xs, ys


def query_series(samples: list[Sample]) -> list[Series]:
    """p95 per query kind, in milliseconds; kinds missing from a sample count as 0."""
    out: list[Series] = []
    for kind in QUERY_KINDS:
        xs = [float(sample["elapsed_s"]) / 3600.0 for sample in samples]
        ys = [
            float(sample.get("queries", {}).get(kind, {}).get("p95_ms", 0.0))
            for sample in samples
        ]
        out.append((kind, xs, ys))
    return out


def plot(
    path: Path,
    title: str,
    ylabel: str,
    series: list[Series],
    warmup: float,
    bounds: list[tuple[str, float]] | None = None,
    log: bool = False,
) -> None:
    """One figure: the series against hours, the warm-up shaded, bounds drawn as lines."""
    figure, axes = plt.subplots(figsize=(WIDTH_PX / DPI, HEIGHT_PX / DPI), dpi=DPI)
    for label, xs, ys in series:
        axes.plot(xs, ys, label=label, linewidth=1.0)
    for label, value in bounds or []:
        axes.axhline(value, linestyle="--", linewidth=0.8, label=label)
    if warmup > 0:
        axes.axvspan(0, warmup, color="grey", alpha=0.15, label="warm-up")
    if log:
        axes.set_yscale("log")
    axes.set_title(title)
    axes.set_xlabel("hours")
    axes.set_ylabel(ylabel)
    axes.grid(visible=True, linewidth=0.3)
    if series or bounds:
        axes.legend(loc="upper left", fontsize="small")
    figure.tight_layout()
    figure.savefig(path)
    plt.close(figure)


def summary_table(rows: list[tuple[str, list[float]]]) -> str:
    """Markdown: min, median, 95th percentile and max of each series after warm-up."""
    lines = ["| series | min | median | p95 | max |", "|---|---:|---:|---:|---:|"]
    for name, data in rows:
        if not data:
            lines.append(f"| {name} | – | – | – | – |")
            continue
        ordered = sorted(data)
        p95 = ordered[min(len(ordered) - 1, round(0.95 * (len(ordered) - 1)))]
        lines.append(
            f"| {name} | {ordered[0]:.4g} | {statistics.median(ordered):.4g} | "
            f"{p95:.4g} | {ordered[-1]:.4g} |"
        )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out", type=Path, help="the soak's output directory")
    parser.add_argument(
        "--warmup-hours",
        type=float,
        default=None,
        help="shade this many hours (default: run.json's warmup_hours)",
    )
    args = parser.parse_args(argv)
    out: Path = args.out
    samples = load_samples(out)
    if not samples:
        msg = f"no samples in {out}"
        raise SystemExit(msg)
    warmup = args.warmup_hours if args.warmup_hours is not None else run_warmup_hours(out)
    xs = hours(samples)
    plots = out / "plots"
    plots.mkdir(exist_ok=True)

    def draw(
        name: str,
        title: str,
        ylabel: str,
        series: list[Series],
        bounds: list[tuple[str, float]] | None = None,
        log: bool = False,
    ) -> None:
        plot(plots / name, title, ylabel, series, warmup, bounds, log)

    draw(
        "memory.png",
        "Memory",
        "GiB",
        [
            ("RSS", xs, values(samples, "rss_bytes", GIB)),
            ("high-water mark", xs, values(samples, "hwm_bytes", GIB)),
        ],
    )
    draw(
        "fds_threads.png",
        "File descriptors and threads",
        "count",
        [
            ("fds", xs, values(samples, "fds")),
            ("threads", xs, values(samples, "threads")),
        ],
    )
    target_xs, target_rate = interval_rate(samples, "target_spans_total")
    appended_xs, appended_rate = interval_rate(samples, "appended_spans_total")
    backpressure_xs, backpressure_ms = interval_rate(samples, "backpressure_wait_ms_total")
    draw(
        "ingest.png",
        "Ingest",
        "spans/s",
        [
            ("target", target_xs, target_rate),
            ("appended (first groups)", appended_xs, appended_rate),
            ("backpressure wait, ms/s", backpressure_xs, backpressure_ms),
        ],
    )
    draw(
        "wal_memtable.png",
        "WAL and memtable",
        "MiB",
        [
            ("WAL", xs, values(samples, "wal_bytes", MIB)),
            ("memtable", xs, values(samples, "memtable_bytes", MIB)),
        ],
    )
    draw(
        "files.png",
        "Files per table",
        "count",
        [
            (f"{table} {level}", xs, level_values(samples, table, level))
            for table in ("spans", "traces", "scores")
            for level in ("l0", "l1")
        ],
    )
    draw(
        "lag.png",
        "Lag",
        "minutes (L0 age) / hours (deletions)",
        [
            ("oldest L0 file, min", xs, values(samples, "l0_oldest_age_s", 60.0)),
            (
                "oldest pending deletion, h",
                xs,
                values(samples, "oldest_pending_deletion_s", 3600.0),
            ),
        ],
        bounds=[("compaction bound", 30.0), ("deletion bound", 24.0)],
    )
    draw(
        "disk.png",
        "Disk",
        "GiB",
        [
            ("objects", xs, values(samples, "objects_bytes", GIB)),
            ("free", xs, values(samples, "free_bytes", GIB)),
            ("tmp", xs, values(samples, "tmp_bytes", GIB)),
        ],
    )
    hit_xs, hit_ys = hit_ratio(samples)
    draw(
        "cache.png",
        "Read cache hit ratio per interval (S3 runs)",
        "ratio",
        [("hit ratio", hit_xs, hit_ys)],
    )
    draw(
        "queries.png",
        "Query p95",
        "ms (log scale)",
        query_series(samples),
        log=True,
    )

    post = [sample for sample in samples if float(sample["elapsed_s"]) >= warmup * 3600.0]
    table = summary_table(
        [
            ("RSS, GiB", values(post, "rss_bytes", GIB)),
            ("fds", values(post, "fds")),
            ("threads", values(post, "threads")),
            ("WAL, MiB", values(post, "wal_bytes", MIB)),
            ("memtable, MiB", values(post, "memtable_bytes", MIB)),
            ("spans L0 files", level_values(post, "spans", "l0")),
            ("oldest L0 file, s", values(post, "l0_oldest_age_s")),
            ("objects, GiB", values(post, "objects_bytes", GIB)),
        ]
    )
    print(table)


if __name__ == "__main__":
    main()
