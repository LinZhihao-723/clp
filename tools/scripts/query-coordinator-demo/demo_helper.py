#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "PyMySQL>=1.1.1",
#   "PyYAML>=6.0.3",
#   "ruamel.yaml>=0.18",
# ]
# ///
"""Helper for the CLP search demo: config checks and edits, unfinished-job checks, and rendering."""

from __future__ import annotations

import argparse
import datetime
import json
import os
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import pymysql
import yaml
from ruamel.yaml import YAML
from ruamel.yaml.comments import CommentedMap
from ruamel.yaml.scalarstring import ScalarString

_ENV_SOURCE = "env"
_RENDERED_SOURCE = "rendered"


@dataclass(frozen=True)
class _Setting:
    key: str
    value: Any
    running_source: str
    running_key: str


_SPIDER_SETTINGS = (
    _Setting("package.scheduler", "spider", _RENDERED_SOURCE, "package.scheduler"),
    _Setting("spider.worker.replicas", 16, _ENV_SOURCE, "SPIDER_WORKER_REPLICAS"),
    _Setting(
        "spider.scheduler.round_robin.dispatch_queue_capacity",
        64,
        _ENV_SOURCE,
        "SPIDER_SCHEDULER_ROUND_ROBIN_DISPATCH_QUEUE_CAPACITY",
    ),
    _Setting(
        "spider.scheduler.round_robin.storage_poll_timeout_ms",
        5,
        _ENV_SOURCE,
        "SPIDER_SCHEDULER_ROUND_ROBIN_STORAGE_POLL_TIMEOUT_MS",
    ),
    _Setting(
        "query_coordinator.result_polling.init_backoff_millisecs",
        10,
        _RENDERED_SOURCE,
        "query_coordinator.result_polling.init_backoff_millisecs",
    ),
    _Setting(
        "query_coordinator.result_polling.max_backoff_millisecs",
        10,
        _RENDERED_SOURCE,
        "query_coordinator.result_polling.max_backoff_millisecs",
    ),
)

_CELERY_SETTINGS = (
    _Setting("package.scheduler", "celery", _RENDERED_SOURCE, "package.scheduler"),
    _Setting(
        "query_scheduler.num_archives_to_search_per_sub_job",
        1024,
        _RENDERED_SOURCE,
        "query_scheduler.num_archives_to_search_per_sub_job",
    ),
)

_RESOURCE_GROUP_KEY = "query_coordinator.resource_group.name"
_RESOURCE_GROUP_SETTINGS = (
    (
        _Setting(
            _RESOURCE_GROUP_KEY,
            os.environ["DEMO_RESOURCE_GROUP"],
            _RENDERED_SOURCE,
            _RESOURCE_GROUP_KEY,
        ),
    )
    if os.environ.get("DEMO_RESOURCE_GROUP")
    else ()
)

_SETTINGS_BY_ENGINE = {
    "spider": _SPIDER_SETTINGS + _RESOURCE_GROUP_SETTINGS,
    "celery": _CELERY_SETTINGS,
}

_UNFINISHED_STATUSES = {0: "PENDING", 1: "RUNNING", 4: "CANCELLING"}

_EXIT_UNREACHABLE = 2
_EXIT_UNFINISHED = 3


def _emit(text: str = "") -> None:
    sys.stdout.write(text)
    sys.stdout.write("\n")


def _get_path(tree: Any, dotted_key: str) -> Any:
    node = tree
    for part in dotted_key.split("."):
        if not isinstance(node, dict) or part not in node:
            return None
        node = node[part]
    return node


def _read_env_file(path: Path) -> dict[str, str]:
    env: dict[str, str] = {}
    if not path.exists():
        return env
    for line in path.read_text().splitlines():
        key, sep, value = line.partition("=")
        if sep:
            env[key.strip()] = value.strip()
    return env


def _same(current: Any, desired: Any) -> bool:
    return current is not None and str(current) == str(desired)


def _cmd_config_diff(args: argparse.Namespace) -> int:
    package_dir: Path = args.package_dir
    settings = _SETTINGS_BY_ENGINE[args.engine]
    config = yaml.safe_load((package_dir / "etc" / "clp-config.yaml").read_text()) or {}
    mismatches = [
        f"etc/clp-config.yaml {s.key}: {_get_path(config, s.key)} (want {s.value})"
        for s in settings
        if not _same(_get_path(config, s.key), s.value)
    ]
    if args.check_running:
        rendered_path = package_dir / "var" / "log" / ".clp-config.yaml"
        rendered = yaml.safe_load(rendered_path.read_text()) if rendered_path.exists() else {}
        env = _read_env_file(package_dir / ".env")
        for s in settings:
            if _RENDERED_SOURCE == s.running_source:
                current = _get_path(rendered or {}, s.running_key)
                where = "running config"
            else:
                current = env.get(s.running_key)
                where = "running .env"
            if not _same(current, s.value):
                mismatches.append(f"{where} {s.running_key}: {current} (want {s.value})")
    for mismatch in mismatches:
        _emit(mismatch)
    return 0


def _set_path(tree: CommentedMap, dotted_key: str, value: Any) -> Any:
    parts = dotted_key.split(".")
    node = tree
    for part in parts[:-1]:
        if not isinstance(node.get(part), dict):
            node[part] = CommentedMap()
        node = node[part]
    old = node.get(parts[-1])
    if isinstance(old, ScalarString) and isinstance(value, str):
        value = type(old)(value)
    node[parts[-1]] = value
    return old


def _cmd_config_apply(args: argparse.Namespace) -> int:
    config_path: Path = args.package_dir / "etc" / "clp-config.yaml"
    round_trip = YAML()
    round_trip.preserve_quotes = True
    round_trip.indent(mapping=2, sequence=4, offset=2)
    round_trip.width = 4096
    round_trip.representer.add_representer(
        type(None),
        lambda representer, _: representer.represent_scalar("tag:yaml.org,2002:null", "null"),
    )
    tree = round_trip.load(config_path.read_text())
    if tree is None:
        tree = CommentedMap()

    changes = []
    for s in _SETTINGS_BY_ENGINE[args.engine]:
        old = _get_path(tree, s.key)
        if _same(old, s.value):
            continue
        _set_path(tree, s.key, s.value)
        changes.append(f"{s.key} {old}->{s.value}")
    if not changes:
        _emit("config already up to date")
        return 0

    backup: Path = args.backup
    if not backup.exists():
        shutil.copy2(config_path, backup)
    with tempfile.NamedTemporaryFile(
        "w", dir=config_path.parent, prefix=".clp-config.", suffix=".tmp", delete=False
    ) as tmp:
        round_trip.dump(tree, tmp)
    shutil.copymode(config_path, tmp.name)
    Path(tmp.name).replace(config_path)
    _emit(", ".join(changes))
    return 0


def _to_tcp_host(host: str) -> str:
    return "127.0.0.1" if "localhost" == host else host


def _cmd_unfinished_jobs(args: argparse.Namespace) -> int:
    package_dir: Path = args.package_dir
    config = yaml.safe_load((package_dir / "etc" / "clp-config.yaml").read_text()) or {}
    credentials_path = Path(config.get("credentials_file_path", "etc/credentials.yaml"))
    if not credentials_path.is_absolute():
        credentials_path = package_dir / credentials_path
    credentials = (yaml.safe_load(credentials_path.read_text()) or {}).get("database") or {}
    database = config.get("database") or {}
    try:
        connection = pymysql.connect(
            host=_to_tcp_host(database.get("host", "localhost")),
            port=int(database.get("port", 3306)),
            user=credentials.get("username"),
            password=credentials.get("password"),
            database=(database.get("names") or {}).get("clp", "clp-db"),
            connect_timeout=5,
        )
    except pymysql.MySQLError as e:
        _emit(f"database unreachable: {e}")
        return _EXIT_UNREACHABLE
    with connection, connection.cursor() as cursor:
        placeholders = ", ".join(["%s"] * len(_UNFINISHED_STATUSES))
        cursor.execute(
            "SELECT `id`, `status`, `creation_time` FROM `query_jobs`"  # noqa: S608
            f" WHERE `status` IN ({placeholders}) ORDER BY `id`",
            tuple(_UNFINISHED_STATUSES),
        )
        rows = cursor.fetchall()
    for job_id, status, creation_time in rows:
        _emit(f"job {job_id} {_UNFINISHED_STATUSES[status]} (created {creation_time})")
    return _EXIT_UNFINISHED if rows else 0


def _format_epoch_ms(value: Any) -> str:
    if not isinstance(value, (int, float)):
        return "-"
    moment = datetime.datetime.fromtimestamp(value / 1000, tz=datetime.timezone.utc)
    return moment.strftime("%Y-%m-%d %H:%M:%S,%f")[:-3]


def _decode_result(document: dict[str, Any]) -> tuple[str, str]:
    raw = document.get("message")
    timestamp = None
    message: Any = raw
    if isinstance(raw, str):
        try:
            inner = json.loads(raw)
        except ValueError:
            inner = None
        if isinstance(inner, dict) and "message" in inner:
            message = inner["message"]
            timestamp = inner.get("timestamp")
    if message is None:
        message = ""
    elif not isinstance(message, str):
        message = json.dumps(message)
    message = message.rstrip("\r\n").lstrip(" ")
    if timestamp is None:
        timestamp = _format_epoch_ms(document.get("timestamp"))
    return str(timestamp), message


def _sort_key(document: dict[str, Any]) -> tuple[float, str, int]:
    timestamp = document.get("timestamp")
    log_event_ix = document.get("log_event_ix")
    return (
        timestamp if isinstance(timestamp, (int, float)) else float("-inf"),
        str(document.get("archive_id", "")),
        log_event_ix if isinstance(log_event_ix, int) else -1,
    )


def _format_secs(value: Any, digits: int = 3) -> str:
    return "n/a" if value is None else f"{value:.{digits}f} s"


def _cmd_render(args: argparse.Namespace) -> int:
    record = json.loads(args.record.read_text())
    status = record.get("status")
    num_results = record.get("num_results")
    query_label = repr(args.query) + (" (ignore-case)" if args.ignore_case else "")
    _emit(
        f"[{args.engine} | {args.workers} workers] query {query_label} | job {record.get('job_id')}"
        f" | {status} | {record.get('num_tasks')} archives searched"
        f" | {'n/a' if num_results is None else num_results} results"
    )
    _emit()

    succeeded = "SUCCEEDED" == status
    if succeeded:
        results = []
        if args.dump.exists():
            with args.dump.open() as f:
                results = [json.loads(line) for line in f if line.strip()]
        results.sort(key=_sort_key, reverse=True)
        shown = results[: args.limit]
        for document in shown:
            timestamp, message = _decode_result(document)
            indent = "\n" + " " * (len(timestamp) + 2)
            _emit(f"{timestamp}  {indent.join(message.splitlines()) or message}")
        if shown:
            _emit()
        num_omitted = len(results) - len(shown)
        if num_omitted > 0:
            _emit(
                f"... {num_omitted} more result(s) omitted (showing the newest {len(shown)} of"
                f" {len(results)}; use -n to show more)."
            )
        _emit(f"Full results ({len(results)}): {args.dump}")
    elif record.get("timed_out"):
        _emit(
            f"Job {record.get('job_id')} did not finish within the timeout; last status: {status}."
        )
        _emit(
            "It stays in query_jobs and blocks mode switches until it finishes: wait for it with"
            f" `query_harness.py wait {record.get('job_id')}`, or remove it with"
            f" `query_harness.py cleanup --job-id {record.get('job_id')} --delete-jobs --force`."
        )
    else:
        _emit(
            f"Job {record.get('job_id')} {status}: {record.get('status_msg') or '(no status_msg)'}"
        )

    _emit()
    _emit(
        f"  Query time:   {_format_secs(record.get('wall_clock_secs'))}"
        "   (harness wall-clock: job INSERT -> terminal status observed, 10 ms polling)"
    )
    _emit(f"  DB duration:  {_format_secs(record.get('db_duration_secs'))}   (query_jobs.duration)")
    _emit(
        f"  Setup:        {_format_secs(args.setup_secs, 1)}   ({args.setup_note};"
        " NOT part of the query time)"
    )
    return 0 if succeeded else 1


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    config_diff = subparsers.add_parser("config-diff", help="List settings that need changing.")
    config_diff.add_argument("engine", choices=sorted(_SETTINGS_BY_ENGINE))
    config_diff.add_argument("--package-dir", type=Path, required=True)
    config_diff.add_argument(
        "--check-running",
        action="store_true",
        help="Also check the running package's rendered config and `.env`.",
    )
    config_diff.set_defaults(handler=_cmd_config_diff)

    config_apply = subparsers.add_parser("config-apply", help="Write the engine's settings.")
    config_apply.add_argument("engine", choices=sorted(_SETTINGS_BY_ENGINE))
    config_apply.add_argument("--package-dir", type=Path, required=True)
    config_apply.add_argument("--backup", type=Path, required=True)
    config_apply.set_defaults(handler=_cmd_config_apply)

    unfinished = subparsers.add_parser("unfinished-jobs", help="List unfinished query jobs.")
    unfinished.add_argument("--package-dir", type=Path, required=True)
    unfinished.set_defaults(handler=_cmd_unfinished_jobs)

    render = subparsers.add_parser("render", help="Print a finished job's summary and results.")
    render.add_argument("--engine", required=True)
    render.add_argument("--workers", required=True)
    render.add_argument("--query", required=True)
    render.add_argument("--ignore-case", action="store_true")
    render.add_argument("--record", type=Path, required=True)
    render.add_argument("--dump", type=Path, required=True)
    render.add_argument("--limit", type=int, required=True)
    render.add_argument("--setup-secs", type=float, required=True)
    render.add_argument("--setup-note", required=True)
    render.set_defaults(handler=_cmd_render)
    return parser


def main() -> int:
    """Main."""
    args = _build_parser().parse_args()
    return args.handler(args)


if "__main__" == __name__:
    sys.exit(main())
