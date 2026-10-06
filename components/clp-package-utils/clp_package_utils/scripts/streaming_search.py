"""Script to search the compressed logs and stream the results from the search tasks."""

import argparse
import logging
import pathlib
import shlex
import subprocess
import sys

from clp_py_utils.clp_config import (
    CLP_DB_PASS_ENV_VAR_NAME,
    CLP_DB_USER_ENV_VAR_NAME,
    CLP_DEFAULT_CONFIG_FILE_RELATIVE_PATH,
    CLP_DEFAULT_DATASET_NAME,
    ClpDbUserType,
    CompressionOrchestration,
    CONTAINER_CLP_HOME,
)
from clp_py_utils.core import resolve_host_path_in_container

from clp_package_utils.general import (
    dump_container_config,
    generate_container_config,
    generate_container_name,
    generate_container_start_cmd,
    get_clp_home,
    get_container_config_filename,
    JobType,
    load_config_file,
    validate_and_load_db_credentials_file,
    validate_dataset_name,
)

STREAMING_SEARCH_BIN_PATH = CONTAINER_CLP_HOME / "bin" / "clp-streaming-search"

logger = logging.getLogger(__name__)


def main(argv: list[str]) -> int:
    """
    Searches the compressed logs through the query coordinator and streams the results.

    :param argv:
    :return: The exit code of `clp-streaming-search`, or -1 if an error is encountered before it
        runs.
    """
    clp_home = get_clp_home()
    parsed_args = _parse_args(argv, clp_home / CLP_DEFAULT_CONFIG_FILE_RELATIVE_PATH)

    if parsed_args.verbose:
        logger.setLevel(logging.DEBUG)
    else:
        logger.setLevel(logging.INFO)

    # Validate and load config file
    try:
        config_file_path = pathlib.Path(parsed_args.config)
        clp_config = load_config_file(resolve_host_path_in_container(config_file_path))
        clp_config.validate_logs_dir(True)

        # Validate and load necessary credentials
        validate_and_load_db_credentials_file(clp_config, clp_home, False)
    except Exception:
        logger.exception("Failed to load config.")
        return -1

    scheduler = clp_config.package.scheduler
    if CompressionOrchestration.SPIDER != scheduler:
        logger.error(
            "Streaming search requires `package.scheduler` to be `%s`, but it's `%s`. Use"
            " `sbin/search.sh` instead.",
            CompressionOrchestration.SPIDER,
            scheduler,
        )
        return -1

    datasets = [CLP_DEFAULT_DATASET_NAME] if parsed_args.dataset is None else parsed_args.dataset
    try:
        clp_db_connection_params = clp_config.database.get_clp_connection_params_and_type(True)
        for ds in datasets:
            validate_dataset_name(clp_db_connection_params["table_prefix"], ds)
    except ValueError:
        logger.exception("Invalid dataset.")
        return -1

    container_name = generate_container_name(str(JobType.SEARCH))

    container_clp_config, mounts = generate_container_config(clp_config, clp_home)
    generated_config_path_on_container, generated_config_path_on_host = dump_container_config(
        container_clp_config, clp_config, get_container_config_filename(container_name)
    )
    necessary_mounts = [mounts.logs_dir]
    credentials = clp_config.database.credentials
    extra_env_vars = {
        CLP_DB_PASS_ENV_VAR_NAME: credentials[ClpDbUserType.CLP].password,
        CLP_DB_USER_ENV_VAR_NAME: credentials[ClpDbUserType.CLP].username,
        "RUST_LOG": (
            "INFO,clp_streaming_search=DEBUG,search_result_listener=DEBUG"
            if parsed_args.verbose
            else "INFO"
        ),
    }
    container_start_cmd = generate_container_start_cmd(
        container_name, necessary_mounts, clp_config.container_image_ref, extra_env_vars
    )
    cmd = container_start_cmd + _build_search_cmd(
        parsed_args, datasets, generated_config_path_on_container
    )

    try:
        proc = subprocess.run(cmd, check=False)
        ret_code = proc.returncode
        if 0 != ret_code:
            logger.error("Search failed.")
            logger.debug("Docker command failed: %s", shlex.join(cmd))
    finally:
        # Remove generated files
        resolved_generated_config_path_on_host = resolve_host_path_in_container(
            generated_config_path_on_host
        )
        resolved_generated_config_path_on_host.unlink()

    return ret_code


def _parse_args(argv: list[str], default_config_file_path: pathlib.Path) -> argparse.Namespace:
    """
    Parses the command line arguments.

    :param argv:
    :param default_config_file_path:
    :return: The parsed arguments.
    """
    args_parser = argparse.ArgumentParser(
        description="Searches the compressed logs and streams the results from the search tasks."
    )
    args_parser.add_argument(
        "--config",
        "-c",
        default=str(default_config_file_path),
        help="CLP package configuration file.",
    )
    args_parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        help="Enable debug logging.",
    )
    args_parser.add_argument("wildcard_query", help="Wildcard query.")
    args_parser.add_argument(
        "--dataset",
        action="append",
        default=None,
        help="A dataset to search. Can be specified multiple times.",
    )
    args_parser.add_argument(
        "--begin-time",
        type=int,
        help="Time range filter lower-bound (inclusive) as milliseconds from the UNIX epoch.",
    )
    args_parser.add_argument(
        "--end-time",
        type=int,
        help="Time range filter upper-bound (inclusive) as milliseconds from the UNIX epoch.",
    )
    args_parser.add_argument(
        "--ignore-case",
        action="store_true",
        help="Ignore case distinctions between values in the query and the compressed data.",
    )
    args_parser.add_argument(
        "--raw", action="store_true", help="Output the search results as raw logs."
    )
    args_parser.add_argument(
        "--advertised-host",
        help="Host the search tasks connect to in order to stream results to this tool.",
    )
    return args_parser.parse_args(argv[1:])


def _build_search_cmd(
    parsed_args: argparse.Namespace, datasets: list[str], config_path: pathlib.Path
) -> list[str]:
    """
    Builds the `clp-streaming-search` command to run in the container.

    :param parsed_args:
    :param datasets:
    :param config_path: Path to the generated config file in the container.
    :return: The command.
    """
    # fmt: off
    search_cmd = [
        str(STREAMING_SEARCH_BIN_PATH),
        "--config", str(config_path),
    ]
    # fmt: on
    for ds in datasets:
        search_cmd.append("--dataset")
        search_cmd.append(ds)
    if parsed_args.begin_time is not None:
        search_cmd.append("--begin-time")
        search_cmd.append(str(parsed_args.begin_time))
    if parsed_args.end_time is not None:
        search_cmd.append("--end-time")
        search_cmd.append(str(parsed_args.end_time))
    if parsed_args.ignore_case:
        search_cmd.append("--ignore-case")
    if parsed_args.raw:
        search_cmd.append("--raw")
    if parsed_args.advertised_host is not None:
        search_cmd.append("--advertised-host")
        search_cmd.append(parsed_args.advertised_host)
    search_cmd.append("--")
    search_cmd.append(parsed_args.wildcard_query)
    return search_cmd


if "__main__" == __name__:
    sys.exit(main(sys.argv))
