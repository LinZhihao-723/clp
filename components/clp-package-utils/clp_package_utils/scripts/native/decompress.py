import argparse
import asyncio
import logging
import os
import pathlib
import subprocess
import sys
import uuid
from typing import Any

from clp_py_utils.clp_config import (
    CLP_DB_PASS_ENV_VAR_NAME,
    CLP_DB_USER_ENV_VAR_NAME,
    CLP_DEFAULT_CONFIG_FILE_RELATIVE_PATH,
    CLP_DEFAULT_DATASET_NAME,
    ClpConfig,
    ClpDbNameType,
    ClpDbUserType,
    Database,
)
from clp_py_utils.sql_adapter import SqlAdapter
from job_orchestration.scheduler.constants import QueryJobStatus

from clp_package_utils.general import (
    EXTRACT_FILE_CMD,
    EXTRACT_JSON_CMD,
    get_clp_home,
    load_config_file,
)
from clp_package_utils.scripts.native.utils import (
    run_function_in_process,
    submit_query_job,
    validate_datasets_exist,
    wait_for_query_job,
)

logger = logging.getLogger(__file__)


def submit_and_monitor_extraction_job_in_db(
    db_config: Database,
    job_config: dict[str, Any],
) -> int:
    """
    Submits a stream extraction job to the scheduler and waits until it finishes.
    :param db_config:
    :param job_config: The config of the extract job.
    :return: 0 on success, -1 otherwise.
    """
    sql_adapter = SqlAdapter(db_config)
    job_id = submit_query_job(sql_adapter, "extract", job_config)
    job_status = wait_for_query_job(sql_adapter, job_id)

    if QueryJobStatus.SUCCEEDED == job_status:
        logger.info(f"Finished extraction job {job_id}.")
        return 0

    logger.error(f"Extraction job {job_id} finished with unexpected status: {job_status.to_str()}.")
    return -1


def handle_extract_stream_cmd(
    parsed_args: argparse.Namespace,
    clp_home: pathlib.Path,
    default_config_file_path: pathlib.Path,
) -> int:
    """
    Handles the stream extraction command.
    :param parsed_args:
    :param clp_home:
    :param default_config_file_path:
    :return: 0 on success, -1 otherwise.
    """
    # Validate and load config file
    clp_config = validate_and_load_config_file(
        clp_home, pathlib.Path(parsed_args.config), default_config_file_path
    )
    if clp_config is None:
        return -1

    command = parsed_args.command
    if EXTRACT_JSON_CMD != command:
        logger.error(f"Unsupported stream extraction command: {command}")
        return -1

    target_chunk_size = parsed_args.target_chunk_size
    if target_chunk_size is not None and target_chunk_size <= 0:
        logger.error(f"--target-chunk-size must be positive, got {target_chunk_size}.")
        return -1

    dataset = parsed_args.dataset
    dataset = CLP_DEFAULT_DATASET_NAME if dataset is None else dataset
    try:
        validate_datasets_exist(clp_config.database, [dataset])
    except Exception as e:
        logger.error(e)
        return -1

    job_config = {
        "dataset": dataset,
        "archive_id": parsed_args.archive_id,
        "target_chunk_size": target_chunk_size,
    }

    try:
        return asyncio.run(
            run_function_in_process(
                submit_and_monitor_extraction_job_in_db,
                clp_config.database,
                job_config,
            )
        )
    except asyncio.CancelledError:
        logger.error("Stream extraction cancelled.")
        return -1


def validate_and_load_config_file(
    clp_home: pathlib.Path,
    config_file_path: pathlib.Path,
    default_config_file_path: pathlib.Path,
) -> ClpConfig | None:
    """
    Validates and loads the config file.
    :param clp_home:
    :param config_file_path:
    :param default_config_file_path:
    :return: clp_config on success, None otherwise.
    """
    try:
        clp_config = load_config_file(config_file_path)
        clp_config.validate_archive_output_config()
        clp_config.validate_logs_dir()
        clp_config.database.load_credentials_from_env()
        return clp_config
    except Exception:
        logger.exception("Failed to load config.")
        return None


def handle_extract_file_cmd(
    parsed_args: argparse.Namespace, clp_home: pathlib.Path, default_config_file_path: pathlib.Path
) -> int:
    """
    Handles the file extraction command.
    :param parsed_args:
    :param clp_home:
    :param default_config_file_path:
    :return: 0 on success, -1 otherwise.
    """
    # Validate paths were specified using only one method
    if len(parsed_args.paths) > 0 and parsed_args.files_from is not None:
        logger.error("Paths cannot be specified both on the command line and through a file.")
        return -1

    # Validate extraction directory
    extraction_dir = pathlib.Path(parsed_args.extraction_dir)
    if not extraction_dir.is_dir():
        logger.error(f"extraction-dir ({extraction_dir}) is not a valid directory.")
        return -1

    # Validate and load config file
    clp_config = validate_and_load_config_file(
        clp_home, pathlib.Path(parsed_args.config), default_config_file_path
    )
    if clp_config is None:
        return -1

    paths = parsed_args.paths
    list_path = parsed_args.files_from

    logs_dir = clp_config.logs_directory
    archives_dir = clp_config.archive_output.get_directory()

    # Configure CLP metadata DB connection params.
    clp_db_connection_params = clp_config.database.get_clp_connection_params_and_type(True)
    # fmt: off
    extract_cmd = [
        str(clp_home / "bin" / "clp"),
        "x", str(archives_dir), str(extraction_dir),
        "--db-type", clp_db_connection_params["type"],
        "--db-host", clp_db_connection_params["host"],
        "--db-port", str(clp_db_connection_params["port"]),
        "--db-name", clp_db_connection_params["names"][ClpDbNameType.CLP],
        "--db-table-prefix", clp_db_connection_params["table_prefix"],
    ]
    # fmt: on
    credentials = Database.model_validate(clp_db_connection_params).credentials
    extract_env = {
        **os.environ,
        CLP_DB_USER_ENV_VAR_NAME: credentials[ClpDbUserType.CLP].username,
        CLP_DB_PASS_ENV_VAR_NAME: credentials[ClpDbUserType.CLP].password,
    }

    files_to_extract_list_path = None
    if list_path is not None:
        extract_cmd.append("-f")
        extract_cmd.append(str(list_path))
    elif len(paths) > 0:
        # Write paths to file
        files_to_extract_list_path = logs_dir / f"paths-to-extract-{uuid.uuid4()}.txt"
        with open(files_to_extract_list_path, "w") as stream:
            stream.writelines(path + "\n" for path in paths)

        extract_cmd.append("-f")
        extract_cmd.append(str(files_to_extract_list_path))

    proc = subprocess.Popen(extract_cmd, env=extract_env)
    return_code = proc.wait()
    if 0 != return_code:
        logger.error(f"File extraction failed, return_code={return_code}")
        return return_code

    # Remove generated files
    if files_to_extract_list_path is not None:
        files_to_extract_list_path.unlink()

    return 0


def main(argv):
    clp_home = get_clp_home()
    default_config_file_path = clp_home / CLP_DEFAULT_CONFIG_FILE_RELATIVE_PATH

    args_parser = argparse.ArgumentParser(description="Decompresses logs.")
    args_parser.add_argument(
        "--config",
        "-c",
        required=True,
        default=str(default_config_file_path),
        help="CLP configuration file.",
    )
    args_parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        help="Enable debug logging.",
    )
    command_args_parser = args_parser.add_subparsers(dest="command", required=True)

    # File extraction command parser
    file_extraction_parser = command_args_parser.add_parser(EXTRACT_FILE_CMD)
    file_extraction_parser.add_argument(
        "paths", metavar="PATH", nargs="*", help="Files to extract."
    )
    file_extraction_parser.add_argument(
        "-f", "--files-from", help="A file listing all files to extract."
    )
    file_extraction_parser.add_argument(
        "-d", "--extraction-dir", metavar="DIR", default=".", help="Extract files into DIR."
    )

    # JSON extraction command parser
    json_extraction_parser = command_args_parser.add_parser(EXTRACT_JSON_CMD)
    json_extraction_parser.add_argument("archive_id", type=str, help="Archive ID")
    json_extraction_parser.add_argument(
        "--dataset",
        type=str,
        default=None,
        help="The dataset that the archives belong to.",
    )
    json_extraction_parser.add_argument(
        "--target-chunk-size", type=int, help="Target chunk size (B)."
    )

    parsed_args = args_parser.parse_args(argv[1:])
    if parsed_args.verbose:
        logger.setLevel(logging.DEBUG)
    else:
        logger.setLevel(logging.INFO)

    command = parsed_args.command
    if EXTRACT_FILE_CMD == command:
        return handle_extract_file_cmd(parsed_args, clp_home, default_config_file_path)
    if EXTRACT_JSON_CMD == command:
        return handle_extract_stream_cmd(parsed_args, clp_home, default_config_file_path)
    logger.exception(f"Unexpected command: {command}")
    return -1


if "__main__" == __name__:
    sys.exit(main(sys.argv))
