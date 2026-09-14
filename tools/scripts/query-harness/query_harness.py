#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "msgpack>=1.1.2",
#   "PyMySQL>=1.1.1",
#   "pymongo>=4.10.1",
#   "PyYAML>=6.0.3",
# ]
# ///
"""Submits CLP search jobs through the metadata database and benchmarks them."""

from __future__ import annotations

import argparse
import contextlib
import csv
import json
import logging
import math
import os
import statistics
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict, dataclass, fields, replace
from enum import IntEnum
from pathlib import Path
from typing import Any

import msgpack
import pymongo
import pymongo.errors
import pymysql
import pymysql.cursors
import yaml

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
logger = logging.getLogger(__name__)


class _QueryJobStatus(IntEnum):
    PENDING = 0
    RUNNING = 1
    SUCCEEDED = 2
    FAILED = 3
    CANCELLING = 4
    CANCELLED = 5
    KILLED = 6


_TERMINAL_STATUSES = frozenset(
    {
        _QueryJobStatus.SUCCEEDED,
        _QueryJobStatus.FAILED,
        _QueryJobStatus.CANCELLED,
        _QueryJobStatus.KILLED,
    }
)
_TERMINAL_STATUS_NAMES = frozenset(status.name for status in _TERMINAL_STATUSES)
_ERROR_STATUS = "ERROR"

_SEARCH_OR_AGGREGATION_JOB_TYPE = 0
_SPIDER_SCHEDULER = "spider"
_DEFAULT_DATASET = "default"
_DEFAULT_MAX_NUM_RESULTS = 1000
_MALFORMED_JOB_CONFIG = b"\xc1"
_RAW_JOB_CONFIG_QUERY_LABEL = "<raw-job-config>"
_RESULT_KEY_FIELDS = ("dataset", "archive_id", "log_event_ix")
_ALL_QUERIES_GROUP = "<all>"
_SUMMARY_METRICS = ("wall_clock_secs", "db_duration_secs", "result_fetch_secs", "num_results")

_BASE_JOB_COLUMNS = (
    "status",
    "status_msg",
    "creation_time",
    "start_time",
    "duration",
    "num_tasks",
    "num_tasks_completed",
)
_OPTIONAL_JOB_COLUMNS = ("spider_id",)

_INSERT_JOB_SQL = "INSERT INTO `query_jobs` (`job_config`, `type`) VALUES (%s, %s)"
_SHOW_JOB_COLUMNS_SQL = "SHOW COLUMNS FROM `query_jobs`"
_SELECT_JOB_STATUS_SQL = "SELECT `status` FROM `query_jobs` WHERE `id` = %s"
_DELETE_TASKS_SQL = "DELETE FROM `query_tasks` WHERE `job_id` = %s"
_DELETE_JOB_SQL = "DELETE FROM `query_jobs` WHERE `id` = %s"

_REPO_ROOT = Path(__file__).resolve().parents[3]


class HarnessError(Exception):
    """Raised when the harness cannot proceed."""


_JOB_ERRORS = (HarnessError, OSError, ValueError, pymysql.MySQLError, pymongo.errors.PyMongoError)


@dataclass(frozen=True)
class _Endpoints:
    db_host: str
    db_port: int
    db_name: str
    db_user: str | None
    db_password: str | None
    results_cache_uri: str
    results_cache_db: str
    scheduler: str
    label: str


@dataclass(frozen=True)
class _JobSpec:
    query: str
    datasets: tuple[str, ...]
    begin_timestamp: int | None
    end_timestamp: int | None
    ignore_case: bool
    path_filter: str | None
    max_num_results: int
    count_aggregation: bool
    raw_job_config: bytes | None

    def label(self) -> str:
        return _RAW_JOB_CONFIG_QUERY_LABEL if self.raw_job_config is not None else self.query

    def encode(self) -> bytes:
        if self.raw_job_config is not None:
            return self.raw_job_config
        aggregation_config = None
        if self.count_aggregation:
            aggregation_config = {
                "job_id": None,
                "reducer_host": None,
                "reducer_port": None,
                "do_count_aggregation": True,
                "count_by_time_bucket_size": None,
            }
        return msgpack.packb(
            {
                "datasets": list(self.datasets),
                "query_string": self.query,
                "max_num_results": self.max_num_results,
                "begin_timestamp": self.begin_timestamp,
                "end_timestamp": self.end_timestamp,
                "ignore_case": self.ignore_case,
                "path_filter": self.path_filter,
                "network_address": None,
                "aggregation_config": aggregation_config,
                "write_to_file": False,
            }
        )


@dataclass(frozen=True)
class _RunOptions:
    poll_interval_secs: float
    timeout_secs: float
    fetch_results: bool
    drop_results: bool
    dump_dir: Path | None


@dataclass
class _JobRecord:
    label: str
    query: str
    repetition: int | None
    job_id: int | None
    status: str
    status_msg: str
    timed_out: bool
    warmup: bool = False
    wall_clock_secs: float | None = None
    result_fetch_secs: float | None = None
    num_results: int | None = None
    db_creation_time: str | None = None
    db_start_time: str | None = None
    db_duration_secs: float | None = None
    num_tasks: int | None = None
    num_tasks_completed: int | None = None
    spider_id: int | None = None


class _Harness:
    def __init__(self, endpoints: _Endpoints, options: _RunOptions) -> None:
        self._endpoints = endpoints
        self._options = options
        self._mongo_client: pymongo.MongoClient = pymongo.MongoClient(
            endpoints.results_cache_uri, directConnection=True
        )
        self._results_db = self._mongo_client[endpoints.results_cache_db]
        self._thread_local = threading.local()
        self._connections: list[pymysql.connections.Connection] = []
        self._connections_lock = threading.Lock()
        self._select_job_sql: str | None = None

    def close(self) -> None:
        with self._connections_lock:
            for connection in self._connections:
                connection.close()
            self._connections.clear()
        self._mongo_client.close()

    def prepare(self) -> None:
        self._get_select_job_sql()

    def run_job(self, spec: _JobSpec, repetition: int | None) -> _JobRecord:
        job_config = spec.encode()
        job_id = None
        try:
            connection = self._db()
            start = time.perf_counter()
            job_id = _insert_job(connection, job_config)
            record = self.observe_job(job_id, spec.label(), repetition, start)
        except _JOB_ERRORS as e:
            if isinstance(e, pymysql.MySQLError):
                self._discard_connection()
            logger.warning("Query job %s (`%s`) errored: %s", job_id, spec.label(), e)
            return _JobRecord(
                label=self._endpoints.label,
                query=spec.label(),
                repetition=repetition,
                job_id=job_id,
                status=_ERROR_STATUS,
                status_msg=str(e),
                timed_out=False,
            )
        return record

    def insert_job(self, job_config: bytes) -> int:
        return _insert_job(self._db(), job_config)

    def observe_job(
        self, job_id: int, query: str, repetition: int | None, start: float | None
    ) -> _JobRecord:
        row, terminal_at = self._wait_for_terminal_status(job_id)
        timed_out = terminal_at is None
        wall_clock_secs = None
        if terminal_at is not None and start is not None:
            wall_clock_secs = terminal_at - start

        num_results = None
        result_fetch_secs = None
        if not timed_out and self._options.fetch_results:
            num_results, result_fetch_secs = self._fetch_results(job_id)

        return _JobRecord(
            label=self._endpoints.label,
            query=query,
            repetition=repetition,
            job_id=job_id,
            status=_QueryJobStatus(row["status"]).name,
            status_msg=row["status_msg"],
            timed_out=timed_out,
            wall_clock_secs=wall_clock_secs,
            result_fetch_secs=result_fetch_secs,
            num_results=num_results,
            db_creation_time=_isoformat_or_none(row["creation_time"]),
            db_start_time=_isoformat_or_none(row["start_time"]),
            db_duration_secs=row["duration"],
            num_tasks=row["num_tasks"],
            num_tasks_completed=row["num_tasks_completed"],
            spider_id=row.get("spider_id"),
        )

    def read_results(self, job_id: int) -> list[dict[str, Any]]:
        return list(self._results_db[str(job_id)].find({}, projection={"_id": False}))

    def drop_results(self, job_ids: list[int]) -> None:
        for job_id in job_ids:
            self._results_db.drop_collection(str(job_id))

    def list_result_collections(self) -> list[int]:
        return sorted(
            int(name) for name in self._results_db.list_collection_names() if name.isdigit()
        )

    def skip_non_terminal_jobs(self, job_ids: list[int]) -> list[int]:
        remaining_job_ids = []
        with self._db().cursor() as cursor:
            for job_id in job_ids:
                cursor.execute(_SELECT_JOB_STATUS_SQL, (job_id,))
                row = cursor.fetchone()
                if row is not None and row["status"] not in _TERMINAL_STATUSES:
                    logger.warning(
                        "Skipping query job %d, whose status is %s; pass --force to clean it up.",
                        job_id,
                        _QueryJobStatus(row["status"]).name,
                    )
                    continue
                remaining_job_ids.append(job_id)
        return remaining_job_ids

    def delete_jobs(self, job_ids: list[int]) -> None:
        with self._db().cursor() as cursor:
            for job_id in job_ids:
                cursor.execute(_DELETE_TASKS_SQL, (job_id,))
                cursor.execute(_DELETE_JOB_SQL, (job_id,))

    def _db(self) -> pymysql.connections.Connection:
        connection = getattr(self._thread_local, "connection", None)
        if connection is not None:
            return connection
        if self._endpoints.db_user is None or self._endpoints.db_password is None:
            msg = "Database credentials are unavailable; pass --db-user and --db-password."
            raise HarnessError(msg)
        connection = pymysql.connect(
            host=self._endpoints.db_host,
            port=self._endpoints.db_port,
            user=self._endpoints.db_user,
            password=self._endpoints.db_password,
            database=self._endpoints.db_name,
            autocommit=True,
            cursorclass=pymysql.cursors.DictCursor,
        )
        self._thread_local.connection = connection
        with self._connections_lock:
            self._connections.append(connection)
        return connection

    def _discard_connection(self) -> None:
        connection = getattr(self._thread_local, "connection", None)
        if connection is None:
            return
        self._thread_local.connection = None
        with self._connections_lock:
            self._connections.remove(connection)
        with contextlib.suppress(pymysql.MySQLError, OSError):
            connection.close()

    def _get_select_job_sql(self) -> str:
        if self._select_job_sql is None:
            with self._db().cursor() as cursor:
                cursor.execute(_SHOW_JOB_COLUMNS_SQL)
                existing_columns = {row["Field"] for row in cursor.fetchall()}
            columns = list(_BASE_JOB_COLUMNS)
            columns += [column for column in _OPTIONAL_JOB_COLUMNS if column in existing_columns]
            column_list = ", ".join(f"`{column}`" for column in columns)
            self._select_job_sql = f"SELECT {column_list} FROM `query_jobs` WHERE `id` = %s"  # noqa: S608
        return self._select_job_sql

    def _fetch_job_row(self, job_id: int) -> dict[str, Any]:
        select_job_sql = self._get_select_job_sql()
        with self._db().cursor() as cursor:
            cursor.execute(select_job_sql, (job_id,))
            row = cursor.fetchone()
        if row is None:
            msg = f"Query job {job_id} doesn't exist."
            raise HarnessError(msg)
        return row

    def _wait_for_terminal_status(self, job_id: int) -> tuple[dict[str, Any], float | None]:
        deadline = time.monotonic() + self._options.timeout_secs
        while True:
            row = self._fetch_job_row(job_id)
            if row["status"] in _TERMINAL_STATUSES:
                return row, time.perf_counter()
            if time.monotonic() >= deadline:
                return row, None
            time.sleep(self._options.poll_interval_secs)

    def _fetch_results(self, job_id: int) -> tuple[int, float]:
        start = time.perf_counter()
        results = self.read_results(job_id)
        result_fetch_secs = time.perf_counter() - start

        if self._options.dump_dir is not None:
            _write_jsonl(
                self._options.dump_dir / f"{self._endpoints.label}-{job_id}.jsonl", results
            )
        if self._options.drop_results:
            self.drop_results([job_id])
        return len(results), result_fetch_secs


def _insert_job(connection: pymysql.connections.Connection, job_config: bytes) -> int:
    with connection.cursor() as cursor:
        cursor.execute(_INSERT_JOB_SQL, (job_config, _SEARCH_OR_AGGREGATION_JOB_TYPE))
        return int(cursor.lastrowid)


def _isoformat_or_none(value: Any) -> str | None:
    return None if value is None else value.isoformat()


def _read_yaml(path: Path) -> dict[str, Any]:
    with path.open() as f:
        content = yaml.safe_load(f)
    return content if isinstance(content, dict) else {}


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        msg = f"File '{path}' doesn't exist."
        raise HarnessError(msg)
    with path.open() as f:
        return [json.loads(line) for line in f if line.strip()]


def _write_jsonl(path: Path, documents: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w") as f:
        for document in documents:
            f.write(json.dumps(document, default=str))
            f.write("\n")


def _emit(text: str) -> None:
    sys.stdout.write(text)
    sys.stdout.write("\n")


def _to_tcp_host(host: str) -> str:
    return "127.0.0.1" if "localhost" == host else host


def _load_endpoints(args: argparse.Namespace) -> _Endpoints:
    package_dir: Path = args.package_dir.expanduser().resolve()
    config_path: Path = args.config or package_dir / "etc" / "clp-config.yaml"
    if config_path.exists():
        config = _read_yaml(config_path)
    elif args.config is not None:
        msg = f"Config file '{config_path}' doesn't exist."
        raise HarnessError(msg)
    else:
        logger.warning("Config file '%s' doesn't exist; using defaults.", config_path)
        config = {}

    credentials_path: Path | None = args.credentials
    if credentials_path is None:
        credentials_path = Path(config.get("credentials_file_path", "etc/credentials.yaml"))
        if not credentials_path.is_absolute():
            credentials_path = package_dir / credentials_path
    db_credentials = {}
    if credentials_path.exists():
        db_credentials = _read_yaml(credentials_path).get("database") or {}

    database = config.get("database") or {}
    results_cache = config.get("results_cache") or {}
    package = config.get("package") or {}
    results_cache_host = _to_tcp_host(
        args.results_cache_host or results_cache.get("host", "localhost")
    )
    results_cache_port = args.results_cache_port or int(results_cache.get("port", 27017))
    scheduler = str(package.get("scheduler", "celery"))
    return _Endpoints(
        db_host=_to_tcp_host(args.db_host or database.get("host", "localhost")),
        db_port=args.db_port or int(database.get("port", 3306)),
        db_name=args.db_name or (database.get("names") or {}).get("clp", "clp-db"),
        db_user=args.db_user or db_credentials.get("username"),
        db_password=args.db_password or db_credentials.get("password"),
        results_cache_uri=f"mongodb://{results_cache_host}:{results_cache_port}",
        results_cache_db=args.results_cache_db or results_cache.get("db_name", "clp-query-results"),
        scheduler=scheduler,
        label=args.label or scheduler,
    )


def _job_spec_from_args(args: argparse.Namespace, query: str) -> _JobSpec:
    raw_job_config = None
    if args.malformed:
        raw_job_config = _MALFORMED_JOB_CONFIG
    elif args.raw_job_config_hex is not None:
        raw_job_config = bytes.fromhex(args.raw_job_config_hex)

    return _JobSpec(
        query=query,
        datasets=tuple(args.dataset or (_DEFAULT_DATASET,)),
        begin_timestamp=args.begin_time,
        end_timestamp=args.end_time,
        ignore_case=args.ignore_case,
        path_filter=args.path_filter,
        max_num_results=args.max_num_results,
        count_aggregation=args.count,
        raw_job_config=raw_job_config,
    )


def _run_options_from_args(args: argparse.Namespace) -> _RunOptions:
    return _RunOptions(
        poll_interval_secs=args.poll_interval,
        timeout_secs=args.timeout,
        fetch_results=not getattr(args, "no_fetch", False),
        drop_results=getattr(args, "drop_results", False),
        dump_dir=getattr(args, "dump_dir", None),
    )


def _record_matches(record: _JobRecord, expected_status: str) -> bool:
    return record.status == expected_status.upper()


def _warn_about_unfinished_jobs(records: list[_JobRecord]) -> None:
    job_ids = [
        record.job_id
        for record in records
        if record.job_id is not None and record.status not in _TERMINAL_STATUS_NAMES
    ]
    if not job_ids:
        return
    logger.warning(
        "Query job(s) %s didn't reach a terminal status. Before switching modes, wait for them to"
        " finish or run `cleanup --job-id <id> --delete-jobs --force`.",
        ", ".join(str(job_id) for job_id in job_ids),
    )


def _check_raw_job_config_allowed(args: argparse.Namespace, endpoints: _Endpoints) -> None:
    is_raw_job_config = args.malformed or args.raw_job_config_hex is not None
    if not is_raw_job_config or args.force_raw_job_config:
        return
    if _SPIDER_SCHEDULER == endpoints.scheduler:
        return
    msg = (
        "`--malformed` and `--raw-job-config-hex` are Spider-mode-only: in Celery mode, an"
        " undecodable `job_config` stops the legacy query scheduler. Pass --force-raw-job-config to"
        " submit anyway."
    )
    raise HarnessError(msg)


def _cmd_submit(args: argparse.Namespace, endpoints: _Endpoints) -> int:
    _check_raw_job_config_allowed(args, endpoints)
    spec = _job_spec_from_args(args, args.query)
    harness = _Harness(endpoints, _run_options_from_args(args))
    try:
        if args.no_wait:
            job_id = harness.insert_job(spec.encode())
            _emit(json.dumps({"job_id": job_id}))
            return 0
        harness.prepare()
        record = harness.run_job(spec, None)
        _emit(json.dumps(asdict(record), indent=2))
        _warn_about_unfinished_jobs([record])
        if record.num_results is not None and (
            args.show_results > 0 or args.dump_results is not None
        ):
            results = harness.read_results(record.job_id)
            for result in results[: args.show_results]:
                _emit(json.dumps(result, default=str))
            if args.dump_results is not None:
                _write_jsonl(args.dump_results, results)
    finally:
        harness.close()
    return 0 if _record_matches(record, args.expect_status) else 1


def _cmd_wait(args: argparse.Namespace, endpoints: _Endpoints) -> int:
    harness = _Harness(endpoints, _run_options_from_args(args))
    try:
        record = harness.observe_job(args.job_id, "", None, None)
    finally:
        harness.close()
    _emit(json.dumps(asdict(record), indent=2))
    return 0 if _record_matches(record, args.expect_status) else 1


def _load_queries(args: argparse.Namespace) -> list[str]:
    queries = list(args.query or [])
    if args.queries_file is not None:
        with args.queries_file.open() as f:
            for line in f:
                query = line.strip()
                if query and not query.startswith("#"):
                    queries.append(query)
    return queries


def _percentile(sorted_values: list[float], fraction: float) -> float:
    rank = fraction * (len(sorted_values) - 1)
    lower = math.floor(rank)
    upper = math.ceil(rank)
    return sorted_values[lower] + (sorted_values[upper] - sorted_values[lower]) * (rank - lower)


def _summarize_group(label: str, query: str, records: list[_JobRecord]) -> dict[str, Any]:
    succeeded = [record for record in records if _QueryJobStatus.SUCCEEDED.name == record.status]
    summary: dict[str, Any] = {
        "label": label,
        "query": query,
        "num_jobs": len(records),
        "num_succeeded": len(succeeded),
        "num_timed_out": sum(1 for record in records if record.timed_out),
    }
    for metric in _SUMMARY_METRICS:
        values = sorted(
            value for record in succeeded if (value := getattr(record, metric)) is not None
        )
        if not values:
            continue
        summary[f"{metric}_mean"] = statistics.fmean(values)
        summary[f"{metric}_p50"] = _percentile(values, 0.5)
        summary[f"{metric}_p90"] = _percentile(values, 0.9)
        summary[f"{metric}_max"] = values[-1]
    return summary


def _summarize(label: str, records: list[_JobRecord]) -> list[dict[str, Any]]:
    records_by_query: dict[str, list[_JobRecord]] = {}
    for record in records:
        records_by_query.setdefault(record.query, []).append(record)
    summaries = [_summarize_group(label, _ALL_QUERIES_GROUP, records)]
    summaries += [
        _summarize_group(label, query, query_records)
        for query, query_records in records_by_query.items()
    ]
    return summaries


def _format_summary(summaries: list[dict[str, Any]]) -> str:
    header = f"{'query':<40} {'jobs':>5} {'ok':>5}"
    for metric in ("wall_clock_secs", "db_duration_secs", "result_fetch_secs"):
        short_name = metric.removesuffix("_secs")
        header += f" {short_name + ' mean/p50/p90/max (s)':>44}"
    lines = [header]
    for summary in summaries:
        line = f"{summary['query'][:40]:<40} {summary['num_jobs']:>5} {summary['num_succeeded']:>5}"
        for metric in ("wall_clock_secs", "db_duration_secs", "result_fetch_secs"):
            if f"{metric}_mean" not in summary:
                line += f" {'-':>44}"
                continue
            stats = "/".join(
                f"{summary[f'{metric}_{stat}']:.3f}" for stat in ("mean", "p50", "p90", "max")
            )
            line += f" {stats:>44}"
        lines.append(line)
    return "\n".join(lines)


def _write_records(args: argparse.Namespace, records: list[_JobRecord]) -> None:
    if args.records_jsonl is not None:
        _write_jsonl(args.records_jsonl, [asdict(record) for record in records])
    if args.records_csv is not None:
        args.records_csv.parent.mkdir(parents=True, exist_ok=True)
        with args.records_csv.open("w", newline="") as f:
            writer = csv.DictWriter(f, fieldnames=[field.name for field in fields(_JobRecord)])
            writer.writeheader()
            writer.writerows(asdict(record) for record in records)


def _cmd_bench(args: argparse.Namespace, endpoints: _Endpoints) -> int:
    queries = _load_queries(args)
    if not queries:
        msg = "No queries given; use --query and/or --queries-file."
        raise HarnessError(msg)
    _check_raw_job_config_allowed(args, endpoints)
    specs = [_job_spec_from_args(args, query) for query in queries]

    harness = _Harness(endpoints, _run_options_from_args(args))
    warmup_records: list[_JobRecord] = []
    try:
        harness.prepare()
        with ThreadPoolExecutor(max_workers=args.concurrency) as pool:
            if args.warmup > 0:
                logger.info("Running %d warmup round(s)...", args.warmup)
                warmup_futures = [
                    pool.submit(harness.run_job, spec, None)
                    for _ in range(args.warmup)
                    for spec in specs
                ]
                warmup_records = [
                    replace(future.result(), warmup=True) for future in warmup_futures
                ]

            logger.info(
                "Running %d query(ies) x %d repetition(s) with concurrency %d...",
                len(specs),
                args.repetitions,
                args.concurrency,
            )
            futures = [
                pool.submit(harness.run_job, spec, repetition)
                for repetition in range(args.repetitions)
                for spec in specs
            ]
            records = [future.result() for future in futures]
    finally:
        harness.close()

    _warn_about_unfinished_jobs(warmup_records + records)
    _write_records(args, warmup_records + records)
    summaries = _summarize(endpoints.label, records)
    if args.summary_json is not None:
        args.summary_json.parent.mkdir(parents=True, exist_ok=True)
        with args.summary_json.open("w") as f:
            json.dump(summaries, f, indent=2)
    _emit(_format_summary(summaries))
    return 0 if all(_QueryJobStatus.SUCCEEDED.name == record.status for record in records) else 1


def _load_result_set(source: str, harness: _Harness) -> list[dict[str, Any]]:
    if source.isdigit():
        return harness.read_results(int(source))
    return _read_jsonl(Path(source))


def _index_results(
    results: list[dict[str, Any]],
) -> tuple[dict[tuple[Any, ...], dict[str, Any]], int]:
    indexed: dict[tuple[Any, ...], dict[str, Any]] = {}
    num_duplicates = 0
    for result in results:
        key = tuple(result.get(field) for field in _RESULT_KEY_FIELDS)
        if key in indexed:
            num_duplicates += 1
        indexed[key] = result
    return indexed, num_duplicates


def _cmd_compare(args: argparse.Namespace, endpoints: _Endpoints) -> int:
    harness = _Harness(endpoints, _run_options_from_args(args))
    try:
        left, left_duplicates = _index_results(_load_result_set(args.left, harness))
        right, right_duplicates = _index_results(_load_result_set(args.right, harness))
    finally:
        harness.close()

    only_left = sorted(left.keys() - right.keys(), key=str)
    only_right = sorted(right.keys() - left.keys(), key=str)
    mismatched = sorted(
        (key for key in left.keys() & right.keys() if left[key] != right[key]), key=str
    )
    report = {
        "left": args.left,
        "right": args.right,
        "key": list(_RESULT_KEY_FIELDS),
        "num_left": len(left),
        "num_right": len(right),
        "num_left_duplicate_keys": left_duplicates,
        "num_right_duplicate_keys": right_duplicates,
        "num_common": len(left.keys() & right.keys()),
        "num_only_left": len(only_left),
        "num_only_right": len(only_right),
        "num_mismatched": len(mismatched),
        "only_left_examples": [left[key] for key in only_left[: args.max_examples]],
        "only_right_examples": [right[key] for key in only_right[: args.max_examples]],
        "mismatched_examples": [
            {"left": left[key], "right": right[key]} for key in mismatched[: args.max_examples]
        ],
    }
    _emit(json.dumps(report, indent=2, default=str))
    is_identical = not (
        only_left or only_right or mismatched or left_duplicates or right_duplicates
    )
    return 0 if is_identical else 1


def _cmd_cleanup(args: argparse.Namespace, endpoints: _Endpoints) -> int:
    harness = _Harness(endpoints, _run_options_from_args(args))
    try:
        job_ids = set(args.job_id or [])
        if args.records is not None:
            for record in _read_jsonl(args.records):
                if not isinstance(record, dict) or "job_id" not in record:
                    msg = f"'{args.records}' isn't a bench records file (no `job_id` field)."
                    raise HarnessError(msg)
                if record["job_id"] is not None:
                    job_ids.add(int(record["job_id"]))
        if args.all_results:
            collection_job_ids = harness.list_result_collections()
            harness.drop_results(collection_job_ids)
            logger.info("Dropped %d result collection(s).", len(collection_job_ids))
        sorted_job_ids = sorted(job_ids)
        if not args.force:
            sorted_job_ids = harness.skip_non_terminal_jobs(sorted_job_ids)
        harness.drop_results(sorted_job_ids)
        logger.info("Dropped result collections of %d job(s).", len(sorted_job_ids))
        if args.delete_jobs:
            harness.delete_jobs(sorted_job_ids)
            logger.info("Deleted %d job row(s) and their task rows.", len(sorted_job_ids))
    finally:
        harness.close()
    return 0


def _add_connection_args(parser: argparse.ArgumentParser) -> None:
    group = parser.add_argument_group("connection")
    group.add_argument(
        "--package-dir",
        type=Path,
        default=Path(os.environ.get("CLP_HOME", _REPO_ROOT / "build" / "clp-package")),
        help="CLP package directory (default: $CLP_HOME or <repo>/build/clp-package).",
    )
    group.add_argument(
        "--config", type=Path, help="CLP config file (default: <package-dir>/etc/clp-config.yaml)."
    )
    group.add_argument(
        "--credentials",
        type=Path,
        help="Credentials file (default: the config's `credentials_file_path`).",
    )
    group.add_argument("--db-host", help="Override the database host.")
    group.add_argument("--db-port", type=int, help="Override the database port.")
    group.add_argument("--db-name", help="Override the database name.")
    group.add_argument("--db-user", help="Override the database user.")
    group.add_argument("--db-password", help="Override the database password.")
    group.add_argument("--results-cache-host", help="Override the results cache host.")
    group.add_argument("--results-cache-port", type=int, help="Override the results cache port.")
    group.add_argument("--results-cache-db", help="Override the results cache database name.")
    group.add_argument(
        "--label", help="Engine label for records (default: the config's `package.scheduler`)."
    )


def _add_polling_args(parser: argparse.ArgumentParser) -> None:
    group = parser.add_argument_group("polling")
    group.add_argument(
        "--poll-interval",
        type=float,
        default=0.05,
        help="Seconds between job status polls (default: %(default)s).",
    )
    group.add_argument(
        "--timeout",
        type=float,
        default=600.0,
        help="Seconds to wait for a terminal status (default: %(default)s).",
    )
    group.add_argument(
        "--no-fetch", action="store_true", help="Don't read the job's results after it finishes."
    )


def _add_job_spec_args(parser: argparse.ArgumentParser) -> None:
    group = parser.add_argument_group("job config")
    group.add_argument(
        "--dataset",
        action="append",
        help=f"Dataset to search; repeatable (default: {_DEFAULT_DATASET}).",
    )
    group.add_argument("--begin-time", type=int, help="Lower bound (inclusive) in epoch ms.")
    group.add_argument("--end-time", type=int, help="Upper bound (inclusive) in epoch ms.")
    group.add_argument("--ignore-case", action="store_true", help="Case-insensitive search.")
    group.add_argument("--path-filter", help="Path filter.")
    group.add_argument(
        "--max-num-results",
        type=int,
        default=_DEFAULT_MAX_NUM_RESULTS,
        help="`max_num_results` (default: %(default)s).",
    )
    group.add_argument(
        "--count",
        action="store_true",
        help="Submit a count aggregation job instead (Spider mode only; stays PENDING).",
    )
    group.add_argument(
        "--malformed",
        action="store_true",
        help="Submit an undecodable `job_config` (Spider mode only).",
    )
    group.add_argument(
        "--raw-job-config-hex", help="Submit these raw `job_config` bytes (hex; Spider mode only)."
    )
    group.add_argument(
        "--force-raw-job-config",
        action="store_true",
        help=(
            "Allow --malformed / --raw-job-config-hex in Celery mode, where an undecodable"
            " `job_config` stops the legacy query scheduler until the job row is deleted."
        ),
    )


def _add_expect_status_arg(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--expect-status",
        default=_QueryJobStatus.SUCCEEDED.name,
        choices=[status.name for status in _QueryJobStatus],
        type=str.upper,
        help="Exit with 0 only if the final observed status matches (default: %(default)s).",
    )


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    submit = subparsers.add_parser("submit", help="Submit one job and wait for it.")
    submit.add_argument("query", help="Query string.")
    _add_job_spec_args(submit)
    _add_polling_args(submit)
    _add_expect_status_arg(submit)
    submit.add_argument("--no-wait", action="store_true", help="Print the job ID and exit.")
    submit.add_argument("--show-results", type=int, default=0, help="Print the first N results.")
    submit.add_argument("--dump-results", type=Path, help="Write all results to a JSONL file.")
    submit.set_defaults(handler=_cmd_submit)

    wait = subparsers.add_parser("wait", help="Wait for an existing job.")
    wait.add_argument("job_id", type=int)
    _add_polling_args(wait)
    _add_expect_status_arg(wait)
    wait.set_defaults(handler=_cmd_wait)

    bench = subparsers.add_parser("bench", help="Benchmark a list of queries.")
    bench.add_argument("--query", action="append", help="Query string; repeatable.")
    bench.add_argument("--queries-file", type=Path, help="File with one query per line.")
    bench.add_argument("--repetitions", type=int, default=5, help="(default: %(default)s)")
    bench.add_argument("--warmup", type=int, default=1, help="Unrecorded rounds.")
    bench.add_argument("--concurrency", type=int, default=1, help="Jobs in flight.")
    bench.add_argument("--records-jsonl", type=Path, help="Write per-job records as JSONL.")
    bench.add_argument("--records-csv", type=Path, help="Write per-job records as CSV.")
    bench.add_argument("--summary-json", type=Path, help="Write the summary as JSON.")
    bench.add_argument("--dump-dir", type=Path, help="Write each job's results as JSONL here.")
    bench.add_argument(
        "--drop-results", action="store_true", help="Drop each job's results after reading them."
    )
    _add_job_spec_args(bench)
    _add_polling_args(bench)
    bench.set_defaults(handler=_cmd_bench)

    compare = subparsers.add_parser("compare", help="Diff two result sets.")
    compare.add_argument("left", help="Job ID in the results cache, or a JSONL results file.")
    compare.add_argument("right", help="Job ID in the results cache, or a JSONL results file.")
    compare.add_argument("--max-examples", type=int, default=5, help="(default: %(default)s)")
    compare.set_defaults(handler=_cmd_compare, poll_interval=0.0, timeout=0.0)

    cleanup = subparsers.add_parser("cleanup", help="Drop results and optionally job rows.")
    cleanup.add_argument("--job-id", type=int, action="append", help="Job ID; repeatable.")
    cleanup.add_argument("--records", type=Path, help="JSONL records file from `bench`.")
    cleanup.add_argument(
        "--all-results",
        action="store_true",
        help="Drop every job result collection, including non-harness jobs' (e.g. web UI) results.",
    )
    cleanup.add_argument(
        "--delete-jobs",
        action="store_true",
        help="Also delete the selected jobs' `query_jobs` and `query_tasks` rows.",
    )
    cleanup.add_argument(
        "--force",
        action="store_true",
        help="Also clean up selected jobs that haven't reached a terminal status.",
    )
    cleanup.set_defaults(handler=_cmd_cleanup, poll_interval=0.0, timeout=0.0)

    for subparser in (compare, cleanup):
        _add_connection_args(subparser)
    for subparser in (submit, wait, bench):
        _add_connection_args(subparser)
    return parser


def main() -> int:
    """Main."""
    args = _build_parser().parse_args()
    try:
        endpoints = _load_endpoints(args)
        return args.handler(args, endpoints)
    except _JOB_ERRORS as e:
        logger.error("%s", e)  # noqa: TRY400
        return 1


if "__main__" == __name__:
    sys.exit(main())
