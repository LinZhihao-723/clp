#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = ["pyyaml>=5.4"]
# ///
"""
Runs the LAB search benchmark for every configured tool on one dataset.

For each repetition, mode (cold, hot), query and tool, it follows LAB `src/template.py`
`bench_search`: clear the page cache; in hot mode run the query `hot_run_warm_up_times` times; start
the RSS poller; time the measured run end to end while its stdout is counted through a pipe; stop
the poller. Every measured run is appended to `results.jsonl` as soon as it finishes; `summary.json`
and `report.md` are written at the end (or regenerated with `report.py`).
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import hashlib
import json
import logging
import os
import platform
import signal
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, BinaryIO

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lab_common
import prepare
import report

MODES = ("cold", "hot")
TIME_BINARY = Path("/usr/bin/time")
READ_CHUNK_SIZE = 1 << 20
STDERR_TAIL_SIZE = 4096
SMOKE_REPETITIONS = 2
PS_COLUMNS = ("pid", "ppid", "rss", "args")

# `/usr/bin/time -v` fields kept in each result, with their JSON names.
TIME_FIELDS = {
    "User time (seconds)": "user_time_s",
    "System time (seconds)": "system_time_s",
    "Maximum resident set size (kbytes)": "peak_rss_kB",
    "Major (requiring I/O) page faults": "major_page_faults",
    "Minor (reclaiming a frame) page faults": "minor_page_faults",
    "File system inputs": "file_system_inputs",
    "Voluntary context switches": "voluntary_context_switches",
    "Involuntary context switches": "involuntary_context_switches",
}

logger = logging.getLogger("lab.run")


class RssPoller:
    """
    Samples the summed RSS of a process tree's processes running one binary, like LAB
    `poll_memory`: on every multiple of the polling interval it runs `ps` and sums the RSS of the
    matching processes. A sample taken before the process starts, or after it exits, is 0, as in
    LAB.
    """

    def __init__(self, interval_s: float, binary: Path) -> None:
        """
        :param interval_s: The polling interval.
        :param binary: The binary whose processes are summed.
        """
        self._interval_s = interval_s
        self._binary_name = binary.name
        self._root_pid: int | None = None
        self._stop_event = threading.Event()
        self._thread = threading.Thread(target=self._poll, daemon=True)
        self.samples: list[int] = []

    def start(self) -> None:
        """Starts polling."""
        self._thread.start()

    def attach(self, root_pid: int) -> None:
        """:param root_pid: The root of the process tree to sample."""
        self._root_pid = root_pid

    def stop(self) -> list[int]:
        """
        Stops polling.

        :return: The samples, in bytes.
        """
        self._stop_event.set()
        self._thread.join()
        return self.samples

    def _poll(self) -> None:
        while not self._stop_event.wait(self._interval_s - (time.time() % self._interval_s)):
            self.samples.append(self._sample())

    def _sample(self) -> int:
        root_pid = self._root_pid
        if root_pid is None:
            return 0
        output = subprocess.run(
            [
                lab_common.executable("ps"),
                "-e",
                "-o",
                ",".join(f"{column}=" for column in PS_COLUMNS),
            ],
            check=False,
            capture_output=True,
            text=True,
        ).stdout
        children: dict[int, list[int]] = {}
        processes: dict[int, tuple[int, str]] = {}
        for line in output.splitlines():
            fields = line.split(None, len(PS_COLUMNS) - 1)
            if len(fields) < len(PS_COLUMNS):
                continue
            pid, ppid, rss_kib = int(fields[0]), int(fields[1]), int(fields[2])
            processes[pid] = (rss_kib, fields[3].split(None, 1)[0])
            children.setdefault(ppid, []).append(pid)
        total = 0
        pending = [root_pid]
        while pending:
            pid = pending.pop()
            pending.extend(children.get(pid, []))
            if pid in processes:
                rss_kib, program = processes[pid]
                if Path(program).name == self._binary_name:
                    total += rss_kib * 1024
        return total


@dataclass
class RunOutcome:
    """One execution of a search command."""

    argv: list[str]
    start_time: str
    time_taken_s: float
    exit_code: int | None
    timed_out: bool
    result: int
    stdout_bytes: int
    stderr_lines: int
    stderr_tail: str
    time_metrics: dict[str, float | int] = field(default_factory=dict)


@dataclass
class _StreamCount:
    num_bytes: int = 0
    num_newlines: int = 0
    last_byte: bytes = b""
    tail: bytes = b""


def _drain(stream: BinaryIO, count: _StreamCount, *, keep_tail: bool) -> None:
    fd = stream.fileno()
    while chunk := os.read(fd, READ_CHUNK_SIZE):
        count.num_bytes += len(chunk)
        count.num_newlines += chunk.count(b"\n")
        count.last_byte = chunk[-1:]
        if keep_tail:
            count.tail = (count.tail + chunk)[-STDERR_TAIL_SIZE:]


def _line_count(count: _StreamCount) -> int:
    if count.num_bytes == 0:
        return 0
    return count.num_newlines + (0 if count.last_byte == b"\n" else 1)


def _parse_time_metrics(text: str) -> dict[str, float | int]:
    metrics: dict[str, float | int] = {}
    for line in text.splitlines():
        name, separator, value = line.strip().rpartition(": ")
        if not separator or name not in TIME_FIELDS:
            continue
        key = TIME_FIELDS[name]
        metrics[key] = float(value) if "." in value else int(value)
    return metrics


def run_search(
    argv: list[str], cpu_set: str, *, poller: RssPoller | None, timeout_s: float | None
) -> RunOutcome:
    """
    Runs one search pinned to `cpu_set` under `/usr/bin/time -v`. Its stdout goes to a pipe whose
    lines are counted (LAB captures stdout and counts its lines); nothing is written to disk.

    :param argv: The search command.
    :param cpu_set: The CPU list given to `taskset -c`.
    :param poller: The RSS poller of a measured run, started before the timer like LAB.
    :param timeout_s: Kills the search after this long, if set.
    :return: The outcome.
    """
    descriptor, time_path = tempfile.mkstemp(prefix="lab-time-", suffix=".txt")
    os.close(descriptor)
    full_argv = [
        str(TIME_BINARY),
        "-v",
        "-o",
        time_path,
        lab_common.executable("taskset"),
        "-c",
        cpu_set,
        *argv,
    ]
    stdout_count = _StreamCount()
    stderr_count = _StreamCount()
    timed_out = threading.Event()
    try:
        if poller is not None:
            poller.start()
        start_time = dt.datetime.now().astimezone().isoformat(timespec="milliseconds")
        start = time.perf_counter()
        with subprocess.Popen(
            full_argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        ) as process:
            if poller is not None:
                poller.attach(process.pid)

            def kill() -> None:
                timed_out.set()
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)

            timer = threading.Timer(timeout_s, kill) if timeout_s is not None else None
            if timer is not None:
                timer.start()
            stderr_thread = threading.Thread(
                target=_drain, args=(process.stderr, stderr_count), kwargs={"keep_tail": True}
            )
            stderr_thread.start()
            _drain(process.stdout, stdout_count, keep_tail=False)
            stderr_thread.join()
            exit_code = process.wait()
            time_taken_s = time.perf_counter() - start
            if timer is not None:
                timer.cancel()
        if poller is not None:
            poller.stop()
        time_metrics = _parse_time_metrics(Path(time_path).read_text(errors="replace"))
    finally:
        Path(time_path).unlink(missing_ok=True)
    return RunOutcome(
        argv=argv,
        start_time=start_time,
        time_taken_s=time_taken_s,
        exit_code=exit_code,
        timed_out=timed_out.is_set(),
        result=_line_count(stdout_count),
        stdout_bytes=stdout_count.num_bytes,
        stderr_lines=_line_count(stderr_count),
        stderr_tail=stderr_count.tail.decode(errors="replace"),
        time_metrics=time_metrics,
    )


def clear_cache(config: lab_common.Config) -> float:
    """
    Flushes and drops the page cache like LAB `clear_cache`.

    :param config: The configuration.
    :return: How long it took.
    :raise subprocess.CalledProcessError: if the command fails.
    """
    start = time.monotonic()
    subprocess.run(
        list(config.search.clear_cache_command),
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    return time.monotonic() - start


@dataclass(frozen=True)
class RunPlan:
    """What one benchmark invocation runs."""

    config: lab_common.Config
    dataset: lab_common.Dataset
    tools: tuple[lab_common.Tool, ...]
    modes: tuple[str, ...]
    queries: tuple[lab_common.Query, ...]
    repetitions: int
    clear_caches: bool
    timeout_s: float | None


@dataclass(frozen=True)
class Step:
    """One LAB search step of a repetition."""

    tool: lab_common.Tool
    mode: str
    query: lab_common.Query
    repetition: int
    position: int


def measure(plan: RunPlan, step: Step) -> dict[str, Any]:
    """
    Runs one LAB search step: clear caches, warm up (hot only), then the measured run.

    :param plan: The plan.
    :param step: The step; its position is the tool's place in its (mode, query) group.
    :return: The result record.
    """
    tool, mode, query, repetition = step.tool, step.mode, step.query, step.repetition
    settings = plan.config.search
    archives = plan.dataset.archives[tool.layout]
    argv = tool.reader.search_argv(archives, query.kql)

    clear_cache_s = clear_cache(plan.config) if plan.clear_caches else None
    warm_ups = []
    if mode == "hot":
        for _ in range(settings.hot_run_warm_up_times):
            warm_up = run_search(argv, settings.cpu_set, poller=None, timeout_s=plan.timeout_s)
            warm_ups.append(
                {
                    "time_taken_s": warm_up.time_taken_s,
                    "result": warm_up.result,
                    "exit_code": warm_up.exit_code,
                }
            )

    poller = RssPoller(settings.memory_polling_interval_s, tool.reader.binary)
    outcome = run_search(argv, settings.cpu_set, poller=poller, timeout_s=plan.timeout_s)
    samples = poller.samples
    memory_average_b = sum(samples) / len(samples) if samples else -1
    nonzero_samples = [sample for sample in samples if sample > 0]
    peak_rss_kb = outcome.time_metrics.get("peak_rss_kB")

    record = {
        "dataset": plan.dataset.name,
        "tool": tool.name,
        "reader": tool.reader.name,
        "layout": tool.layout,
        "mode": mode,
        "query_index": query.index,
        "query": query.kql,
        "repetition": repetition,
        "position": step.position,
        "cache_cleared": plan.clear_caches,
        "clear_cache_s": clear_cache_s,
        "warm_ups": warm_ups,
        **asdict(outcome),
        "memory_samples_B": samples,
        "memory_average_B": memory_average_b,
        "memory_average_nonzero_B": (
            sum(nonzero_samples) / len(nonzero_samples) if nonzero_samples else -1
        ),
        "peak_rss_B": peak_rss_kb * 1024 if peak_rss_kb is not None else None,
        "expected_count": query.expected_count,
        "count_ok": query.expected_count is None or outcome.result == query.expected_count,
    }
    level = logging.INFO if record["count_ok"] and outcome.exit_code == 0 else logging.WARNING
    logger.log(
        level,
        "rep %d %-4s %s %-8s %8.3f s  result %d%s  avg RSS %s  peak RSS %s",
        repetition,
        mode,
        query.label,
        tool.name,
        outcome.time_taken_s,
        outcome.result,
        "" if record["count_ok"] else f" (expected {query.expected_count})",
        _mib(memory_average_b),
        _mib(record["peak_rss_B"]),
    )
    if outcome.exit_code != 0:
        logger.warning("exit code %s; stderr: %s", outcome.exit_code, outcome.stderr_tail)
    return record


def _mib(value: float | None) -> str:
    if value is None or value < 0:
        return "n/a"
    return f"{value / (1024 * 1024):.1f} MiB"


def _file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        while chunk := file.read(READ_CHUNK_SIZE):
            digest.update(chunk)
    return digest.hexdigest()


def _command_output(argv: list[str]) -> str | None:
    try:
        completed = subprocess.run(argv, check=False, capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return completed.stdout.strip() if completed.returncode == 0 else None


def _cpu_model() -> str | None:
    with contextlib.suppress(OSError):
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    return None


def _memory_total_b() -> int | None:
    with contextlib.suppress(OSError):
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                return int(line.split()[1]) * 1024
    return None


def environment_metadata(plan: RunPlan) -> dict[str, Any]:
    """
    :param plan: The plan.
    :return: The run's environment and parameters.
    """
    readers = {tool.reader.name: tool.reader for tool in plan.tools}
    binaries = {}
    for name, reader in readers.items():
        stat = reader.binary.stat()
        binaries[name] = {
            "path": str(reader.binary),
            "size_B": stat.st_size,
            "modified": dt.datetime.fromtimestamp(stat.st_mtime)
            .astimezone()
            .isoformat(timespec="seconds"),
            "sha256": _file_digest(reader.binary),
            "version": _command_output([str(reader.binary), "--version"]),
        }
    archives = {}
    for layout in sorted({tool.layout for tool in plan.tools}):
        directory = plan.dataset.archives[layout]
        files = lab_common.archive_files(directory)
        prepare_record = directory.with_name(directory.name + ".prepare.json")
        archives[layout] = {
            "path": str(directory),
            "num_archives": len(lab_common.archive_entries(directory)),
            "num_files": len(files),
            "size_B": sum(path.stat().st_size for path in files),
            "prepare": json.loads(prepare_record.read_text()) if prepare_record.exists() else None,
        }
    settings = plan.config.search
    return {
        "started_at": dt.datetime.now().astimezone().isoformat(timespec="seconds"),
        "host": {
            "hostname": platform.node(),
            "kernel": platform.release(),
            "cpu_model": _cpu_model(),
            "logical_cpus": os.cpu_count(),
            "memory_total_B": _memory_total_b(),
            "python": platform.python_version(),
        },
        "workspace_commit": _command_output(
            [lab_common.executable("git"), "-C", str(lab_common.WORKSPACE_DIR), "rev-parse", "HEAD"]
        ),
        "config_path": str(plan.config.path),
        "dataset": plan.dataset.name,
        "queries": [
            {
                "index": query.index,
                "label": query.label,
                "lab_query": query.lab_query,
                "kql": query.kql,
                "expected_count": query.expected_count,
            }
            for query in plan.queries
        ],
        "tools": [
            {
                "name": tool.name,
                "reader": tool.reader.name,
                "layout": tool.layout,
                "argv_template": list(tool.reader.search_command),
            }
            for tool in plan.tools
        ],
        "modes": list(plan.modes),
        "repetitions": plan.repetitions,
        "cpu_set": settings.cpu_set,
        "hot_run_warm_up_times": settings.hot_run_warm_up_times,
        "memory_polling_interval_s": settings.memory_polling_interval_s,
        "clear_caches": plan.clear_caches,
        "clear_cache_command": list(settings.clear_cache_command),
        "timeout_s": plan.timeout_s,
        "speedups": [list(pair) for pair in plan.config.speedups],
        "binaries": binaries,
        "archives": archives,
    }


def preflight(plan: RunPlan) -> None:
    """
    Checks that every binary, archive directory and helper command is usable.

    :param plan: The plan.
    :raise RuntimeError: if a check fails.
    """
    problems = []
    for tool in plan.tools:
        if not os.access(tool.reader.binary, os.X_OK):
            problems.append(f"{tool.name}: {tool.reader.binary} is not executable")
        directory = plan.dataset.archives[tool.layout]
        if not directory.is_dir() or not lab_common.archive_files(directory):
            problems.append(f"{tool.name}: {directory} holds no archives")
    if not os.access(TIME_BINARY, os.X_OK):
        problems.append(f"{TIME_BINARY} is missing")
    cpu_set = plan.config.search.cpu_set
    if (
        subprocess.run(
            [lab_common.executable("taskset"), "-c", cpu_set, lab_common.executable("true")],
            check=False,
        ).returncode
        != 0
    ):
        problems.append(f"`taskset -c {cpu_set}` fails")
    if problems:
        raise RuntimeError("; ".join(problems))
    if plan.clear_caches:
        logger.info("Clearing caches once to check the command (%.2f s).", clear_cache(plan.config))


def run(plan: RunPlan, output_dir: Path) -> None:
    """
    Runs the plan, writing `meta.json`, `results.jsonl`, `summary.json` and `report.md` into
    `output_dir`.

    :param plan: The plan.
    :param output_dir: The results directory.
    """
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "meta.json").write_text(json.dumps(environment_metadata(plan), indent=2) + "\n")
    logger.info("Writing results to %s.", output_dir)
    try:
        with (output_dir / "results.jsonl").open("a") as results:
            for repetition in range(plan.repetitions):
                shift = repetition % len(plan.tools)
                order = plan.tools[shift:] + plan.tools[:shift]
                for mode in plan.modes:
                    for query in plan.queries:
                        for position, tool in enumerate(order):
                            step = Step(tool, mode, query, repetition, position)
                            record = measure(plan, step)
                            results.write(json.dumps(record) + "\n")
                            results.flush()
    finally:
        report.write_outputs(output_dir)
        logger.info("Report: %s", output_dir / "report.md")


def main() -> int:
    """
    Parses the arguments and runs the benchmark.

    :return: The exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--config", type=Path, default=lab_common.DEFAULT_CONFIG_PATH)
    parser.add_argument("--dataset", default="mongod")
    parser.add_argument(
        "--smoke",
        action="store_true",
        help="Run on the `smoke` dataset (preparing its archives if needed) to check the harness.",
    )
    parser.add_argument("--data-root", help="Overrides `LAB_DATA_ROOT`.")
    parser.add_argument("--cpp-clp-s", help="Overrides `CLP_S`.")
    parser.add_argument("--rust-clp-s", help="Overrides `CLP_S_RS`.")
    parser.add_argument("--tools", nargs="+", help="Tools to run (default: all, config order).")
    parser.add_argument("--modes", nargs="+", choices=MODES, default=list(MODES))
    parser.add_argument("--queries", nargs="+", type=int, help="1-based query numbers.")
    parser.add_argument("--repetitions", type=int, help="Overrides `search.repetitions`.")
    parser.add_argument("--timeout-s", type=float, help="Kill a search after this long.")
    parser.add_argument(
        "--no-clear-cache",
        action="store_true",
        help="Skip dropping the page cache (debugging only; cold runs are then not cold).",
    )
    parser.add_argument("--output-dir", type=Path, help="Default: <data root>/<dataset>/results/.")
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")
    config = lab_common.load_config(
        args.config,
        {"LAB_DATA_ROOT": args.data_root, "CLP_S": args.cpp_clp_s, "CLP_S_RS": args.rust_clp_s},
    )
    dataset_name = "smoke" if args.smoke else args.dataset
    dataset = config.datasets[dataset_name]
    if args.smoke:
        for layout in dataset.archives:
            prepare.compress(config, dataset, layout, force=False)

    tool_names = args.tools or list(config.tools)
    queries = dataset.queries
    if args.queries:
        queries = tuple(dataset.queries[number - 1] for number in args.queries)
    repetitions = args.repetitions or (
        SMOKE_REPETITIONS if args.smoke else config.search.repetitions
    )
    plan = RunPlan(
        config=config,
        dataset=dataset,
        tools=tuple(config.tools[name] for name in tool_names),
        modes=tuple(args.modes),
        queries=queries,
        repetitions=repetitions,
        clear_caches=not args.no_clear_cache,
        timeout_s=args.timeout_s,
    )
    preflight(plan)
    stamp = dt.datetime.now().astimezone().strftime("%Y%m%d-%H%M%S")
    output_dir = args.output_dir or config.data_root / dataset.name / "results" / stamp
    run(plan, output_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
