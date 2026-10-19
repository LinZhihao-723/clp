#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = ["pyyaml>=5.4"]
# ///
"""
Prepares a dataset's archives for the LAB search benchmark.

Steps (each skipped when its output exists, unless `--force`):

- `download`: fetches the dataset tarball from its `source`.
- `input`: builds the input file from the tarball like LAB `scripts/download_all.py`: every file of
  the tarball concatenated, one record per line, empty lines dropped.
- `mfa`, `sfa`: compresses the input with the C++ `clp-s` into the dataset's multi-file and
  single-file archive directories with LAB's flags. A dataset with `sfa_seed` copies those archives
  into the SFA directory instead.
- `clean-input`: deletes the input file (to save disk space once the archives exist).

Compression is not part of the benchmark; its timing is recorded only for reference, in
`<archive directory>.prepare.json`.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import logging
import shutil
import subprocess
import sys
import tarfile
import time
from pathlib import Path
from typing import BinaryIO

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lab_common

STEPS = ("download", "input", "mfa", "sfa", "clean-input")
DEFAULT_STEPS = ("input", "mfa", "sfa")

logger = logging.getLogger("lab.prepare")


def download(dataset: lab_common.Dataset, *, force: bool) -> None:
    """
    Fetches the dataset tarball.

    :param dataset: The dataset.
    :param force: Whether to fetch it again if it exists.
    :raise subprocess.CalledProcessError: if `curl` fails.
    """
    if dataset.source is None or dataset.tarball is None:
        logger.info("%s: no source tarball; skipping download.", dataset.name)
        return
    if dataset.tarball.exists() and not force:
        logger.info("%s: %s exists; skipping download.", dataset.name, dataset.tarball)
        return
    dataset.tarball.parent.mkdir(parents=True, exist_ok=True)
    partial = dataset.tarball.with_name(dataset.tarball.name + ".partial")
    subprocess.run(
        [lab_common.executable("curl"), "-sS", "-L", "-o", str(partial), dataset.source], check=True
    )
    partial.rename(dataset.tarball)


def _concatenate_tar_stream(stream: BinaryIO, output: BinaryIO) -> tuple[list[str], int]:
    """
    Writes the lines of every regular file of a tar stream, each ending with a newline and empty
    lines dropped.

    :param stream: The uncompressed tar stream.
    :param output: The output file.
    :return: A tuple of the member names in stream order and the number of lines written.
    """
    member_names = []
    num_lines = 0
    with tarfile.open(fileobj=stream, mode="r|") as tarball:
        for member in tarball:
            member_file = tarball.extractfile(member) if member.isfile() else None
            if member_file is None:
                continue
            member_names.append(member.name)
            logger.info("Appending %s.", member.name)
            for line in member_file:
                record = line[:-1] if line.endswith(b"\n") else line
                if record:
                    output.write(record)
                    output.write(b"\n")
                    num_lines += 1
    return member_names, num_lines


def build_input(dataset: lab_common.Dataset, *, force: bool) -> None:
    """
    Builds the input file from the tarball in one streaming pass. Like LAB's `awk` and `sed`
    commands, every line of every file ends with a newline and empty lines are dropped. LAB orders
    the files by name (a shell glob); this pass streams them in tarball order and warns if that
    order differs (the order changes the archive layout, not the records).

    :param dataset: The dataset.
    :param force: Whether to rebuild an existing input.
    :raise FileNotFoundError: if the tarball does not exist.
    """
    if dataset.input is None:
        return
    if dataset.input.exists() and not force:
        logger.info("%s: %s exists; skipping input.", dataset.name, dataset.input)
        return
    if dataset.tarball is None or not dataset.tarball.exists():
        message = f"{dataset.name}: no tarball to build {dataset.input} from"
        raise FileNotFoundError(message)

    partial = dataset.input.with_name(dataset.input.name + ".partial")
    start = time.monotonic()
    gunzip_argv = [lab_common.executable("gzip"), "-dc", str(dataset.tarball)]
    with (
        subprocess.Popen(gunzip_argv, stdout=subprocess.PIPE) as gunzip,
        partial.open("wb", buffering=16 * 1024 * 1024) as output,
    ):
        member_names, num_lines = _concatenate_tar_stream(gunzip.stdout, output)
    if gunzip.returncode != 0:
        message = f"gzip exited with {gunzip.returncode}"
        raise RuntimeError(message)
    partial.rename(dataset.input)

    if member_names != sorted(member_names):
        logger.warning("%s: tarball order differs from LAB's name order.", dataset.name)
    if dataset.number_of_lines is not None and num_lines != dataset.number_of_lines:
        logger.warning(
            "%s: %d lines, expected %d.", dataset.name, num_lines, dataset.number_of_lines
        )
    logger.info(
        "%s: wrote %d lines to %s in %.1f s.",
        dataset.name,
        num_lines,
        dataset.input,
        time.monotonic() - start,
    )


def compress(
    config: lab_common.Config, dataset: lab_common.Dataset, layout: str, *, force: bool
) -> None:
    """
    Writes one archive layout of the dataset.

    :param config: The configuration.
    :param dataset: The dataset.
    :param layout: The archive layout (`mfa` or `sfa`).
    :param force: Whether to replace existing archives.
    :raise FileNotFoundError: if the input (or a seed archive) does not exist.
    :raise subprocess.CalledProcessError: if the compression fails.
    """
    output_dir = dataset.archives[layout]
    if output_dir.exists() and lab_common.archive_files(output_dir):
        if not force:
            logger.info("%s: %s is not empty; skipping %s.", dataset.name, output_dir, layout)
            return
        shutil.rmtree(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)

    start = time.monotonic()
    if layout == "sfa" and dataset.sfa_seed:
        for seed in dataset.sfa_seed:
            shutil.copy2(seed, output_dir / seed.name)
        argv = ["cp", *map(str, dataset.sfa_seed), str(output_dir)]
    else:
        if dataset.input is None or not dataset.input.exists():
            message = f"{dataset.name}: input {dataset.input} does not exist"
            raise FileNotFoundError(message)
        argv = config.compression.argv(dataset, layout, output_dir, dataset.input)
        logger.info("%s: %s", dataset.name, " ".join(argv))
        subprocess.run(argv, check=True)
    duration_s = time.monotonic() - start

    files = lab_common.archive_files(output_dir)
    record = {
        "dataset": dataset.name,
        "layout": layout,
        "argv": argv,
        "finished_at": dt.datetime.now().astimezone().isoformat(timespec="seconds"),
        "duration_s": duration_s,
        "input_size_B": dataset.input.stat().st_size
        if dataset.input is not None and dataset.input.exists()
        else None,
        "num_archives": len(lab_common.archive_entries(output_dir)),
        "num_archive_files": len(files),
        "archive_size_B": sum(path.stat().st_size for path in files),
    }
    output_dir.with_name(output_dir.name + ".prepare.json").write_text(
        json.dumps(record, indent=2) + "\n"
    )
    logger.info(
        "%s: %s has %d files, %d B (%.1f s).",
        dataset.name,
        output_dir,
        record["num_archive_files"],
        record["archive_size_B"],
        duration_s,
    )


def clean_input(config: lab_common.Config, dataset: lab_common.Dataset) -> None:
    """
    Deletes the input file if it lives under the data root and every archive layout exists.

    :param config: The configuration.
    :param dataset: The dataset.
    """
    if dataset.input is None or not dataset.input.exists():
        return
    if not dataset.input.resolve().is_relative_to(config.data_root):
        logger.info("%s: %s is outside the data root; keeping it.", dataset.name, dataset.input)
        return
    missing = [
        layout
        for layout, directory in dataset.archives.items()
        if not directory.exists() or not lab_common.archive_files(directory)
    ]
    if missing:
        logger.warning("%s: missing %s archives; keeping the input.", dataset.name, missing)
        return
    dataset.input.unlink()
    logger.info("%s: deleted %s.", dataset.name, dataset.input)


def main() -> int:
    """
    Runs the requested preparation steps.

    :return: The exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--config", type=Path, default=lab_common.DEFAULT_CONFIG_PATH)
    parser.add_argument("--dataset", default="mongod")
    parser.add_argument("--data-root", help="Overrides `LAB_DATA_ROOT`.")
    parser.add_argument("--cpp-clp-s", help="Overrides `CLP_S`.")
    parser.add_argument("--steps", nargs="+", choices=STEPS, default=list(DEFAULT_STEPS))
    parser.add_argument("--force", action="store_true", help="Redo steps whose output exists.")
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")
    config = lab_common.load_config(
        args.config, {"LAB_DATA_ROOT": args.data_root, "CLP_S": args.cpp_clp_s}
    )
    dataset = config.datasets[args.dataset]
    for step in STEPS:
        if step not in args.steps:
            continue
        if step == "download":
            download(dataset, force=args.force)
        elif step == "input":
            build_input(dataset, force=args.force)
        elif step in ("mfa", "sfa"):
            compress(config, dataset, step, force=args.force)
        else:
            clean_input(config, dataset)
    return 0


if __name__ == "__main__":
    sys.exit(main())
