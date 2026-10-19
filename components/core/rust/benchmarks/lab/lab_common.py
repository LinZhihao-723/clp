"""Configuration model shared by the LAB search harness scripts."""

from __future__ import annotations

import os
import re
import shlex
import shutil
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

HARNESS_DIR = Path(__file__).resolve().parent
WORKSPACE_DIR = HARNESS_DIR.parent.parent
REPO_DIR = WORKSPACE_DIR.parent.parent.parent
DEFAULT_CONFIG_PATH = HARNESS_DIR / "config.yaml"

_VARIABLE_PATTERN = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}")


class ConfigError(Exception):
    """The configuration is invalid."""


@dataclass(frozen=True)
class Query:
    """One benchmark query of a dataset."""

    index: int
    lab_query: str
    kql: str
    expected_count: int | None

    @property
    def label(self) -> str:
        """The query's label in reports, `Q1` for the first query."""
        return f"Q{self.index + 1}"


@dataclass(frozen=True)
class Dataset:
    """A dataset: its input, how it is compressed, its archives and its queries."""

    name: str
    source: str | None
    tarball: Path | None
    input: Path | None
    number_of_lines: int | None
    sfa_seed: tuple[Path, ...]
    timestamp_key: str
    target_encoded_size: int
    archives: dict[str, Path]
    queries: tuple[Query, ...]


@dataclass(frozen=True)
class Reader:
    """A search binary and its command template."""

    name: str
    binary: Path
    search_command: tuple[str, ...]

    def search_argv(self, archives: Path, kql: str) -> list[str]:
        """
        Builds the search command of a query.

        :param archives: The archive directory.
        :param kql: The KQL query.
        :return: The argv.
        """
        return [
            part.format(binary=self.binary, archives=archives, query=kql)
            for part in self.search_command
        ]


@dataclass(frozen=True)
class Tool:
    """A reader searching one archive layout."""

    name: str
    reader: Reader
    layout: str


@dataclass(frozen=True)
class SearchSettings:
    """The search methodology parameters."""

    hot_run_warm_up_times: int
    memory_polling_interval_s: float
    repetitions: int
    cpu_set: str
    clear_cache_command: tuple[str, ...]


@dataclass(frozen=True)
class Compression:
    """How archives are written."""

    reader: Reader
    command: tuple[str, ...]
    layout_flags: dict[str, tuple[str, ...]]

    def argv(self, dataset: Dataset, layout: str, output_dir: Path, input_path: Path) -> list[str]:
        """
        Builds the compression command of one archive layout.

        :param dataset: The dataset.
        :param layout: The archive layout (`mfa` or `sfa`).
        :param output_dir: The archive directory to write.
        :param input_path: The input file.
        :return: The argv.
        """
        argv: list[str] = []
        for part in self.command:
            if part == "{layout_flags}":
                argv.extend(self.layout_flags[layout])
                continue
            argv.append(
                part.format(
                    binary=self.reader.binary,
                    timestamp_key=dataset.timestamp_key,
                    target_encoded_size=dataset.target_encoded_size,
                    output_dir=output_dir,
                    input=input_path,
                )
            )
        return argv


@dataclass(frozen=True)
class Config:
    """The parsed harness configuration."""

    path: Path
    data_root: Path
    search: SearchSettings
    readers: dict[str, Reader]
    tools: dict[str, Tool]
    compression: Compression
    speedups: tuple[tuple[str, str], ...]
    datasets: dict[str, Dataset]


def _default_variables() -> dict[str, str]:
    cargo_target_dir = Path(os.environ.get("CARGO_TARGET_DIR") or WORKSPACE_DIR / "target")
    return {
        "CLP_S": str(REPO_DIR / "build" / "core" / "clp-s"),
        "CLP_S_RS": str(cargo_target_dir / "release" / "clp-s-rs"),
        "LAB_DATA_ROOT": str(WORKSPACE_DIR / "target" / "lab-data"),
        "PROTO_DATA_ROOT": str(WORKSPACE_DIR / "target" / "proto-data"),
    }


def _expand(value: str, variables: dict[str, str]) -> str:
    def substitute(match: re.Match[str]) -> str:
        name = match.group(1)
        if name not in variables:
            message = f"`${{{name}}}` is not set"
            raise ConfigError(message)
        return variables[name]

    return _VARIABLE_PATTERN.sub(substitute, value)


def _resolve_path(value: str, variables: dict[str, str], data_root: Path) -> Path:
    path = Path(_expand(value, variables)).expanduser()
    return path if path.is_absolute() else data_root / path


def _parse_queries(raw_queries: list[dict[str, Any]], dataset_name: str) -> tuple[Query, ...]:
    queries = []
    for index, raw_query in enumerate(raw_queries):
        lab_query = raw_query["lab_query"]
        tokens = shlex.split(lab_query)
        if len(tokens) != 1:
            message = f"{dataset_name} query {index}: `{lab_query}` is not one shell word"
            raise ConfigError(message)
        queries.append(
            Query(
                index=index,
                lab_query=lab_query,
                kql=tokens[0],
                expected_count=raw_query.get("expected_count"),
            )
        )
    return tuple(queries)


def _parse_dataset(
    name: str, raw: dict[str, Any], variables: dict[str, str], data_root: Path
) -> Dataset:
    def optional_path(key: str) -> Path | None:
        value = raw.get(key)
        return None if value is None else _resolve_path(value, variables, data_root)

    return Dataset(
        name=name,
        source=raw.get("source"),
        tarball=optional_path("tarball"),
        input=optional_path("input"),
        number_of_lines=raw.get("number_of_lines"),
        sfa_seed=tuple(
            _resolve_path(value, variables, data_root) for value in raw.get("sfa_seed", [])
        ),
        timestamp_key=raw["timestamp_key"],
        target_encoded_size=int(raw["target_encoded_size"]),
        archives={
            layout: _resolve_path(value, variables, data_root)
            for layout, value in raw["archives"].items()
        },
        queries=_parse_queries(raw["queries"], name),
    )


def load_config(path: Path, overrides: dict[str, str | None]) -> Config:
    """
    Loads the harness configuration.

    :param path: The configuration file.
    :param overrides: Variable values that take precedence over the environment (`None` values
        are ignored).
    :return: The configuration.
    :raise ConfigError: if the configuration is invalid.
    """
    variables = {**os.environ}
    for name, default in _default_variables().items():
        variables[name] = os.environ.get(name) or default
    variables.update({name: value for name, value in overrides.items() if value is not None})

    with path.open() as file:
        raw = yaml.safe_load(file)

    data_root = Path(_expand(raw["data_root"], variables)).expanduser().resolve()
    raw_search = raw["search"]
    search = SearchSettings(
        hot_run_warm_up_times=int(raw_search["hot_run_warm_up_times"]),
        memory_polling_interval_s=float(raw_search["memory_polling_interval_s"]),
        repetitions=int(raw_search["repetitions"]),
        cpu_set=str(raw_search["cpu_set"]),
        clear_cache_command=tuple(raw_search["clear_cache_command"]),
    )
    readers = {
        name: Reader(
            name=name,
            binary=Path(_expand(reader["binary"], variables)).expanduser(),
            search_command=tuple(reader["search_command"]),
        )
        for name, reader in raw["readers"].items()
    }
    tools = {}
    for name, tool in raw["tools"].items():
        if tool["reader"] not in readers:
            message = f"tool `{name}` names unknown reader `{tool['reader']}`"
            raise ConfigError(message)
        tools[name] = Tool(name=name, reader=readers[tool["reader"]], layout=tool["archives"])
    raw_compression = raw["compression"]
    compression = Compression(
        reader=readers[raw_compression["reader"]],
        command=tuple(raw_compression["command"]),
        layout_flags={
            layout: tuple(flags) for layout, flags in raw_compression["layout_flags"].items()
        },
    )
    speedups = tuple(
        (pair["candidate"], pair["baseline"]) for pair in raw.get("report", {}).get("speedups", [])
    )
    datasets = {
        name: _parse_dataset(name, dataset, variables, data_root)
        for name, dataset in raw["datasets"].items()
    }
    for tool in tools.values():
        for dataset in datasets.values():
            if tool.layout not in dataset.archives:
                message = f"dataset `{dataset.name}` has no `{tool.layout}` archives"
                raise ConfigError(message)
    return Config(
        path=path,
        data_root=data_root,
        search=search,
        readers=readers,
        tools=tools,
        compression=compression,
        speedups=speedups,
        datasets=datasets,
    )


def executable(name: str) -> str:
    """
    :param name: A program name.
    :return: The program's path on `PATH`.
    :raise FileNotFoundError: if the program is not on `PATH`.
    """
    path = shutil.which(name)
    if path is None:
        message = f"`{name}` is not on PATH"
        raise FileNotFoundError(message)
    return path


def archive_entries(directory: Path) -> list[Path]:
    """
    :param directory: An archive directory.
    :return: Its top-level entries (one per archive: an SFA file or an MFA directory), sorted.
    """
    return sorted(directory.iterdir())


def archive_files(directory: Path) -> list[Path]:
    """
    :param directory: An archive directory.
    :return: The regular files under `directory`, recursively, sorted.
    """
    return sorted(path for path in directory.rglob("*") if path.is_file())
