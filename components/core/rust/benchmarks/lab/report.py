#!/usr/bin/env python3
"""
Summarizes a LAB search benchmark results directory into `summary.json` and `report.md`.

Every statistic is over the repetitions of one (tool, mode, query): the median and the min/max of
the end-to-end time, LAB's average RSS (the mean of the `ps` samples of one run; runs without a
sample are left out and counted), the peak RSS from `/usr/bin/time -v` and the CPU time.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

MIB = 1024 * 1024
GIB = 1024 * MIB
SECONDS_WITH_MILLISECONDS_BELOW = 100


def load_records(results_dir: Path) -> list[dict[str, Any]]:
    """
    :param results_dir: A results directory.
    :return: The records of `results.jsonl`, or an empty list if there is none.
    """
    path = results_dir / "results.jsonl"
    if not path.exists():
        return []
    with path.open() as file:
        return [json.loads(line) for line in file if line.strip()]


def _spread(values: list[float]) -> dict[str, float] | None:
    if not values:
        return None
    return {"median": statistics.median(values), "min": min(values), "max": max(values)}


def summarize(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """
    Groups the records by (tool, mode, query) and computes their statistics.

    :param records: The measured runs.
    :return: One summary row per group, in first-appearance order.
    """
    groups: dict[tuple[str, str, int], list[dict[str, Any]]] = {}
    for record in records:
        key = (record["tool"], record["mode"], record["query_index"])
        groups.setdefault(key, []).append(record)

    rows = []
    for (tool, mode, query_index), runs in groups.items():
        memory_averages = [run["memory_average_B"] for run in runs if run["memory_average_B"] >= 0]
        peaks = [run["peak_rss_B"] for run in runs if run.get("peak_rss_B") is not None]
        cpu_times = [
            run["time_metrics"]["user_time_s"] + run["time_metrics"]["system_time_s"]
            for run in runs
            if "user_time_s" in run["time_metrics"] and "system_time_s" in run["time_metrics"]
        ]
        expected = runs[0]["expected_count"]
        results = sorted({run["result"] for run in runs})
        rows.append(
            {
                "tool": tool,
                "mode": mode,
                "query_index": query_index,
                "query": runs[0]["query"],
                "num_runs": len(runs),
                "time_s": _spread([run["time_taken_s"] for run in runs]),
                "memory_average_B": _spread(memory_averages),
                "num_runs_without_memory_sample": len(runs) - len(memory_averages),
                "peak_rss_B": _spread(peaks),
                "cpu_time_s": _spread(cpu_times),
                "major_page_faults": _spread(
                    [run["time_metrics"].get("major_page_faults", 0) for run in runs]
                ),
                "results": results,
                "expected_count": expected,
                "count_ok": all(run["count_ok"] for run in runs),
                "exit_codes": sorted({run["exit_code"] for run in runs}, key=str),
                "num_runs_with_stderr": sum(1 for run in runs if run["stderr_lines"] > 0),
            }
        )
    return rows


def _escape(text: str) -> str:
    return text.replace("|", "\\|")


def _seconds(value: float) -> str:
    return f"{value:.3f}" if value < SECONDS_WITH_MILLISECONDS_BELOW else f"{value:.1f}"


def _cell(spread: dict[str, float] | None, scale: float, formatter: Any) -> str:
    if spread is None:
        return "n/a"
    median = formatter(spread["median"] / scale)
    low = formatter(spread["min"] / scale)
    high = formatter(spread["max"] / scale)
    return f"{median} ({low}-{high})"


def _table(header: list[str], body: list[list[str]]) -> list[str]:
    lines = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    lines.extend("| " + " | ".join(row) + " |" for row in body)
    return lines


@dataclass(frozen=True)
class _ReportContext:
    meta: dict[str, Any]
    tools: list[str]
    queries: list[dict[str, Any]]
    index: dict[tuple[str, str, int], dict[str, Any]]
    speedups: list[tuple[str, str]]

    def summary(self, tool: str, mode: str, query: dict[str, Any]) -> dict[str, Any] | None:
        return self.index.get((tool, mode, query["index"]))


def _header_lines(meta: dict[str, Any]) -> list[str]:
    host = meta["host"]
    lines = [
        f"# LAB search benchmark: `{meta['dataset']}`",
        "",
        f"- Started: {meta['started_at']}",
        (
            f"- Host: {host['cpu_model']}, {host['logical_cpus']} logical CPUs, "
            f"{(host['memory_total_B'] or 0) / GIB:.0f} GiB RAM, kernel {host['kernel']}"
        ),
        (
            f"- Searches pinned with `taskset -c {meta['cpu_set']}`; {meta['repetitions']} "
            f"repetitions; hot mode warms up {meta['hot_run_warm_up_times']} times; RSS polled "
            f"every {meta['memory_polling_interval_s']} s; caches cleared: {meta['clear_caches']}"
        ),
        f"- Workspace commit: `{meta['workspace_commit']}`",
    ]
    for name, binary in meta["binaries"].items():
        version = f", `{binary['version']}`" if binary["version"] else ""
        lines.append(
            f"- Reader `{name}`: `{binary['path']}` (sha256 `{binary['sha256'][:12]}`, built "
            f"{binary['modified']}{version})"
        )
    for layout, archives in meta["archives"].items():
        lines.append(
            f"- Archives `{layout}` (`{archives['path']}`): "
            f"{archives.get('num_archives', 'n/a')} archive(s), {archives['num_files']} file(s), "
            f"{archives['size_B'] / GIB:.3f} GiB"
        )
    lines.append("")
    return lines


def _count_lines(context: _ReportContext) -> list[str]:
    body = []
    for query in context.queries:
        row = [query["label"], f"`{_escape(query['kql'])}`", str(query["expected_count"])]
        for tool in context.tools:
            cells = set()
            for mode in context.meta["modes"]:
                summary = context.summary(tool, mode, query)
                if summary is not None:
                    mismatch = "" if summary["count_ok"] else " MISMATCH"
                    cells.add("/".join(map(str, summary["results"])) + mismatch)
            row.append(", ".join(sorted(cells)) or "n/a")
        body.append(row)
    return [
        "## Result counts",
        "",
        *_table(["Query", "KQL", "Expected", *context.tools], body),
        "",
    ]


def _speedup(baseline: float | None, candidate: float | None) -> str:
    if baseline is None or candidate is None or candidate <= 0:
        return "n/a"
    return f"{baseline / candidate:.2f}x"


def _latency_lines(context: _ReportContext, mode: str) -> list[str]:
    header = ["Query", *context.tools, *(f"{c} vs {b}" for c, b in context.speedups)]
    body = []
    totals: dict[str, float | None] = dict.fromkeys(context.tools, 0.0)
    for query in context.queries:
        medians: dict[str, float] = {}
        row = [query["label"]]
        for tool in context.tools:
            summary = context.summary(tool, mode, query)
            spread = summary["time_s"] if summary else None
            row.append(_cell(spread, 1, _seconds))
            total = totals[tool]
            if spread is None:
                totals[tool] = None
            else:
                medians[tool] = spread["median"]
                totals[tool] = None if total is None else total + spread["median"]
        row.extend(_speedup(medians.get(b), medians.get(c)) for c, b in context.speedups)
        body.append(row)
    total_row = ["Sum of medians"]
    total_row.extend(
        "n/a" if totals[tool] is None else _seconds(totals[tool]) for tool in context.tools
    )
    total_row.extend(_speedup(totals[b], totals[c]) for c, b in context.speedups)
    body.append(total_row)
    return [
        f"## Query latency (s), {mode}",
        "",
        "Median (min-max) end-to-end time of the measured run.",
        "",
        *_table(header, body),
        "",
    ]


def _metric_lines(
    context: _ReportContext, title: str, metric: str, scale: float, formatter: Any
) -> list[str]:
    lines = []
    for mode in context.meta["modes"]:
        body = []
        for query in context.queries:
            row = [query["label"]]
            for tool in context.tools:
                summary = context.summary(tool, mode, query)
                row.append(_cell(summary[metric] if summary else None, scale, formatter))
            body.append(row)
        lines.extend([f"## {title}, {mode}", "", *_table(["Query", *context.tools], body), ""])
    return lines


def _group_label(row: dict[str, Any]) -> str:
    return f"{row['tool']} {row['mode']} Q{row['query_index'] + 1}"


def render(meta: dict[str, Any], rows: list[dict[str, Any]]) -> str:
    """
    Renders the report.

    :param meta: The run's `meta.json`.
    :param rows: The summary rows.
    :return: The markdown report.
    """
    tools = [tool["name"] for tool in meta["tools"]]
    context = _ReportContext(
        meta=meta,
        tools=tools,
        queries=meta["queries"],
        index={(row["tool"], row["mode"], row["query_index"]): row for row in rows},
        speedups=[
            (candidate, baseline)
            for candidate, baseline in meta.get("speedups", [])
            if candidate in tools and baseline in tools
        ],
    )
    lines = _header_lines(meta) + _count_lines(context)
    for mode in meta["modes"]:
        lines.extend(_latency_lines(context, mode))
    lines.extend(["A speedup `A vs B` is B's median time over A's: above 1 means A is faster.", ""])

    mebibytes = "{:.1f}".format
    lines.extend(
        _metric_lines(context, "Average RSS (MiB, LAB metric)", "memory_average_B", MIB, mebibytes)
    )
    no_sample = [
        f"{_group_label(row)} ({row['num_runs_without_memory_sample']})"
        for row in rows
        if row["num_runs_without_memory_sample"]
    ]
    if no_sample:
        lines.extend(
            [
                (
                    "Runs without any RSS sample (shorter than one polling interval; LAB's -1) "
                    "are left out of the average RSS: " + ", ".join(no_sample) + "."
                ),
                "",
            ]
        )
    lines.extend(
        _metric_lines(context, "Peak RSS (MiB, /usr/bin/time)", "peak_rss_B", MIB, mebibytes)
    )
    lines.extend(_metric_lines(context, "CPU time (s, user + system)", "cpu_time_s", 1, _seconds))

    problems = [
        f"{_group_label(row)}: exit codes {row['exit_codes']}"
        for row in rows
        if row["exit_codes"] != [0]
    ]
    problems.extend(
        f"{_group_label(row)}: stderr output in {row['num_runs_with_stderr']} runs"
        for row in rows
        if row["num_runs_with_stderr"]
    )
    if problems:
        lines.extend(["## Problems", "", *(f"- {problem}" for problem in problems), ""])
    return "\n".join(lines)


def write_outputs(results_dir: Path) -> None:
    """
    Writes `summary.json` and `report.md` of a results directory.

    :param results_dir: The results directory (with `meta.json` and `results.jsonl`).
    """
    meta = json.loads((results_dir / "meta.json").read_text())
    rows = summarize(load_records(results_dir))
    (results_dir / "summary.json").write_text(json.dumps(rows, indent=2) + "\n")
    (results_dir / "report.md").write_text(render(meta, rows) + "\n")


def main() -> int:
    """
    Regenerates the summary and report of a results directory.

    :return: The exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("results_dir", type=Path)
    parser.add_argument("--print", action="store_true", help="Also print the report.")
    args = parser.parse_args()
    write_outputs(args.results_dir)
    if args.print:
        sys.stdout.write((args.results_dir / "report.md").read_text())
    return 0


if __name__ == "__main__":
    sys.exit(main())
