"""Ground-truth corpus builder for the Rust clp-s reader prototype.

Drives the C++ `clp-s` (and `log-converter`) to build the fixture archives, their decompression
oracles, the query oracles of `tests/queries/suite.toml`, and the dataset archives and oracles.

Usage (normally through `build-fixtures.sh` / `gen-oracles.sh`):

    python corpus.py build-fixtures
    python corpus.py fixture-oracles
    python corpus.py query-oracles [--only-fixtures | --only-datasets] [--id ID ...]
    python corpus.py datasets [--name NAME ...]
    python corpus.py canonical-sha FILE

Environment: `CLP_S`, `LOG_CONVERTER` (C++ binaries), `PROTO_DATA_ROOT` (large artifacts,
default `target/proto-data`), `MONGO_IMAGE` (results-cache oracles), and the dataset inputs named
in `tests/queries/suite.toml` (`PROTO_BENCH_JSONL`, `PROTO_SPIDER_FILE_LIST`,
`PROTO_SYNTHETIC_JSONL`; see tests/README.md).
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

import tomli

sys.path.insert(0, str(Path(__file__).resolve().parent))
import reference_eval  # noqa: E402

PROTOTYPE_ROOT = Path(__file__).resolve().parent.parent
REPO_ROOT = PROTOTYPE_ROOT.parent.parent.parent
TESTS = PROTOTYPE_ROOT / "tests"
FIXTURES = TESTS / "fixtures"
INPUTS = FIXTURES / "inputs"
ARCHIVES = FIXTURES / "archives"
FIXTURE_ORACLES = FIXTURES / "oracles"
QUERIES = TESTS / "queries"
QUERY_ORACLES = QUERIES / "oracles"

CLP_S = Path(os.environ.get("CLP_S", REPO_ROOT / "build/core/clp-s"))
LOG_CONVERTER = Path(os.environ.get("LOG_CONVERTER", REPO_ROOT / "build/core/log-converter"))
DATA_ROOT = Path(os.environ.get("PROTO_DATA_ROOT", PROTOTYPE_ROOT / "target/proto-data"))
WORK = DATA_ROOT / "work"
MONGO_IMAGE = os.environ.get("MONGO_IMAGE", "mongo:8.0.21")
MONGO_CONTAINER = "proto-mongo-oracle"
MONGO_PORT = int(os.environ.get("PROTO_MONGO_PORT", "27917"))


class RawNumber(str):
    """A JSON number kept as its verbatim token text."""


class Pairs(list):
    """A JSON object kept as its list of (key, value) pairs, so duplicate keys survive."""


def _dump(value):
    if isinstance(value, Pairs):
        items = sorted(value, key=lambda kv: kv[0])
        return "{" + ",".join(_dump_str(k) + ":" + _dump(v) for k, v in items) + "}"
    if isinstance(value, list):
        return "[" + ",".join(_dump(v) for v in value) + "]"
    if isinstance(value, RawNumber):
        return str(value)
    if isinstance(value, str):
        return _dump_str(value)
    if value is True:
        return "true"
    if value is False:
        return "false"
    if value is None:
        return "null"
    raise TypeError(f"unexpected JSON value {value!r}")


def _dump_str(s):
    return json.dumps(s, ensure_ascii=False)


def canonical_line(line):
    """Returns the canonical form of one JSON line (see tests/README.md), or None if invalid."""
    try:
        value = json.loads(
            line,
            parse_float=RawNumber,
            parse_int=RawNumber,
            parse_constant=RawNumber,
            object_pairs_hook=Pairs,
        )
    except json.JSONDecodeError:
        return None
    return _dump(value)


def canonical_digest(data):
    """Returns (record count, canonical sha256, invalid line count) of JSON-lines bytes."""
    lines = []
    invalid = 0
    for raw in data.split(b"\n"):
        if not raw:
            continue
        text = raw.decode("utf-8", errors="surrogateescape")
        canon = canonical_line(text)
        if canon is None:
            invalid += 1
            canon = "INVALID:" + text
        lines.append(canon.encode("utf-8", errors="surrogateescape"))
    lines.sort()
    h = hashlib.sha256()
    for line in lines:
        h.update(line)
        h.update(b"\n")
    return len(lines), h.hexdigest(), invalid


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def canonical_digest_file(path):
    """Streaming variant of `canonical_digest` for large files (sorts canonical lines on disk)."""
    canon_path = Path(str(path) + ".canon")
    count = 0
    invalid = 0
    with open(path, "rb") as src, open(canon_path, "wb") as dst:
        for raw in src:
            raw = raw.rstrip(b"\n")
            if not raw:
                continue
            text = raw.decode("utf-8", errors="surrogateescape")
            canon = canonical_line(text)
            if canon is None:
                invalid += 1
                canon = "INVALID:" + text
            dst.write(canon.encode("utf-8", errors="surrogateescape"))
            dst.write(b"\n")
            count += 1
    sorted_path = Path(str(path) + ".canon.sorted")
    env = dict(os.environ, LC_ALL="C")
    subprocess.run(
        ["sort", "-S", "2G", "-T", str(WORK), "-o", str(sorted_path), str(canon_path)],
        check=True,
        env=env,
    )
    digest = sha256_file(sorted_path)
    canon_path.unlink()
    sorted_path.unlink()
    return count, digest, invalid


def _toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, (list, tuple)):
        return "[" + ", ".join(_toml_value(v) for v in value) + "]"
    if isinstance(value, dict):
        return (
            "{ " + ", ".join(f"{_toml_key(k)} = {_toml_value(v)}" for k, v in value.items()) + " }"
        )
    raise TypeError(f"unsupported TOML value {value!r}")


def _toml_key(key):
    if key and all(c.isalnum() or c in "-_" for c in key):
        return key
    return json.dumps(key)


def dump_toml(header_comment, tables, arrays):
    """Emits `tables` (name -> dict) then arrays of tables (name -> list of dicts)."""
    out = [f"# {line}".rstrip() for line in header_comment.splitlines()]
    out.append("")
    for name, table in tables.items():
        out.append(f"[{name}]")
        for k, v in table.items():
            if v is not None:
                out.append(f"{_toml_key(k)} = {_toml_value(v)}")
        out.append("")
    for name, rows in arrays.items():
        for row in rows:
            out.append(f"[[{name}]]")
            for k, v in row.items():
                if v is not None:
                    out.append(f"{_toml_key(k)} = {_toml_value(v)}")
            out.append("")
    return "\n".join(out)


def run(cmd, cwd=None, check=True, capture=True, timeout=3600):
    result = subprocess.run(
        [str(c) for c in cmd],
        cwd=cwd,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
        timeout=timeout,
    )
    if check and result.returncode != 0:
        sys.stderr.write(result.stderr.decode(errors="replace") if result.stderr else "")
        raise RuntimeError(f"command failed ({result.returncode}): {cmd}")
    return result


def fresh_dir(path):
    if path.exists():
        shutil.rmtree(path)
    path.mkdir(parents=True)
    return path


def read_archive_version(path):
    with open(path, "rb") as f:
        header = f.read(8)
    version = int.from_bytes(header[4:8], "little")
    return f"{version >> 24}.{(version >> 16) & 0xFF}.{version & 0xFFFF}", header[:4].hex()


def repo_relative(path):
    """Returns `path` relative to the repository if it is inside it, else its file name."""
    try:
        return str(Path(path).resolve().relative_to(REPO_ROOT.resolve()))
    except ValueError:
        return Path(path).name


def build_info():
    commit = run(["git", "-C", REPO_ROOT, "rev-parse", "HEAD"]).stdout.decode().strip()
    branch = (
        run(["git", "-C", REPO_ROOT, "rev-parse", "--abbrev-ref", "HEAD"]).stdout.decode().strip()
    )
    return {
        "clp_s": repo_relative(CLP_S),
        "clp_s_sha256": sha256_file(CLP_S),
        "log_converter": repo_relative(LOG_CONVERTER),
        "log_converter_sha256": sha256_file(LOG_CONVERTER),
        "source_branch": branch,
        "source_commit": commit,
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }


def load_toml(path):
    with open(path, "rb") as f:
        return tomli.load(f)


def archive_paths(index, name):
    """Returns the archive files of fixture `name` (relative to FIXTURES), in creation order."""
    for row in index["archive"]:
        if row["fixture"] == name:
            return row["files"]
    raise KeyError(f"unknown fixture {name!r}")


def compress(input_dir, input_name, flags, out_dir, single_file=True):
    cmd = [CLP_S, "c", "--print-archive-stats"]
    if single_file:
        cmd.append("--single-file-archive")
    cmd += list(flags) + [out_dir, input_name]
    result = run(cmd, cwd=input_dir)
    stats = [json.loads(line) for line in result.stdout.decode().splitlines() if line.strip()]
    if not stats:
        raise RuntimeError(f"no archive produced for {input_name} {flags}")
    return stats


def cmd_build_fixtures(_args):
    spec = load_toml(FIXTURES / "fixtures.toml")
    fresh_dir(ARCHIVES)
    (ARCHIVES / "reject").mkdir()
    work = fresh_dir(WORK / "build-fixtures")
    rows = []
    for fixture in spec["fixture"]:
        name = fixture["name"]
        kind = fixture.get("kind", "sfa")
        flags = fixture.get("flags", [])
        out_dir = work / name
        input_dir = INPUTS
        input_name = fixture["input"]
        if kind == "kvir":
            ir_dir = fresh_dir(work / (name + ".ir"))
            run([LOG_CONVERTER, INPUTS / input_name, "--output-dir", ir_dir])
            ir_files = sorted(p.name for p in ir_dir.iterdir())
            if len(ir_files) != 1:
                raise RuntimeError(f"expected one kv-IR file, got {ir_files}")
            input_dir = ir_dir
            input_name = ir_files[0]
        stats = compress(input_dir, input_name, flags, out_dir, single_file=(kind != "reject-mfa"))
        files = []
        if kind in ("reject-mfa", "reject-sfa"):
            if len(stats) != 1:
                raise RuntimeError(f"rejection fixture {name} split into {len(stats)} archives")
            suffix = "" if kind == "reject-mfa" else ".clps"
            dest = ARCHIVES / "reject" / (name + suffix)
            shutil.move(str(out_dir / stats[0]["id"]), dest)
            files.append(str(dest.relative_to(FIXTURES)))
        else:
            for k, stat in enumerate(stats):
                file_name = f"{name}.clps" if len(stats) == 1 else f"{name}.{k}.clps"
                dest = ARCHIVES / file_name
                shutil.move(str(out_dir / stat["id"]), dest)
                files.append(str(dest.relative_to(FIXTURES)))
        header_file = FIXTURES / files[0]
        if kind == "reject-mfa":
            header_file = header_file / "header"
        version, magic = read_archive_version(header_file)
        rows.append(
            {
                "fixture": name,
                "kind": kind,
                "input": fixture["input"],
                "flags": flags,
                "files": files,
                "format_version": version,
                "magic": magic,
                "original_ids": [s["id"] for s in stats],
                "sizes": [s["size"] for s in stats],
                "uncompressed_sizes": [s["uncompressed_size"] for s in stats],
                "begin_timestamps": [s["begin_timestamp"] for s in stats],
                "end_timestamps": [s["end_timestamp"] for s in stats],
            }
        )
        print(f"built {name}: {files}")
    header = (
        "GENERATED by scripts/build-fixtures.sh from tests/fixtures/fixtures.toml; do not edit.\n"
        "Archive files are relative to tests/fixtures/. `original_ids` are the UUIDs clp-s chose;\n"
        "the files are renamed, and C++ uses the file name as the archive id of an SFA."
    )
    text = dump_toml(header, {"build": build_info()}, {"archive": rows})
    (ARCHIVES / "index.toml").write_text(text, encoding="utf-8")


def cpp_extract(archive_path, out_dir):
    fresh_dir(out_dir)
    result = run([CLP_S, "x", archive_path, out_dir], check=False)
    produced = sorted(out_dir.iterdir())
    data = b""
    for p in produced:
        data += p.read_bytes()
    return result, data


REJECT_REASONS = {
    "reject-mfa": "multi-file archive (directory): SFA-only scope",
    "reject-sfa": "schema tree contains a StructuredArray node (--structurize-arrays)",
}


def cmd_fixture_oracles(_args):
    index = load_toml(ARCHIVES / "index.toml")
    fresh_dir(FIXTURE_ORACLES)
    work = fresh_dir(WORK / "fixture-oracles")
    rows = []
    for row in index["archive"]:
        kind = row["kind"]
        for k, rel in enumerate(row["files"]):
            archive = FIXTURES / rel
            result, data = cpp_extract(archive, work / f"{row['fixture']}.{k}")
            entry = {
                "fixture": row["fixture"],
                "kind": kind,
                "file": rel,
                "input": row["input"],
                "flags": row["flags"],
                "format_version": row.get("format_version"),
                "expect": "reject" if kind.startswith("reject") else "readable",
                "reject_reason": REJECT_REASONS.get(kind),
                "cpp_x_exit": result.returncode,
            }
            if entry["expect"] == "readable":
                if result.returncode != 0:
                    raise RuntimeError(f"C++ failed to extract readable fixture {rel}")
                stem = Path(rel).name[: -len(".clps")]
                oracle = FIXTURE_ORACLES / f"{stem}.jsonl"
                oracle.write_bytes(data)
                count, canon_sha, invalid = canonical_digest(data)
                entry.update(
                    {
                        "oracle": str(oracle.relative_to(FIXTURES)),
                        "records": count,
                        "oracle_sha256": sha256_bytes(data),
                        "oracle_canonical_sha256": canon_sha,
                        "invalid_json_lines": invalid,
                        "archive_sha256": sha256_file(archive),
                    }
                )
            else:
                entry["cpp_x_stderr"] = result.stderr.decode(errors="replace").strip()[-300:]
                entry["cpp_x_records"] = len([line for line in data.split(b"\n") if line])
            rows.append(entry)
            print(f"oracle {rel}: {entry.get('records', 'reject')}")
    header = (
        "GENERATED by scripts/gen-oracles.sh; do not edit.\n"
        "One [[archive]] per archive file. `oracle` is the C++ `clp-s x` output (unordered,\n"
        "table order). Compare as canonicalized multisets: see tests/README.md."
    )
    text = dump_toml(header, {"build": index["build"]}, {"archive": rows})
    (FIXTURES / "manifest.toml").write_text(text, encoding="utf-8")


def query_targets(query, fixture_index, suite):
    """Returns (list of archive paths, is_dataset)."""
    target = query["target"]
    if target.startswith("dataset:"):
        name = target[len("dataset:") :]
        for ds in suite["dataset"]:
            if ds["name"] == name:
                return [DATA_ROOT / ds["archive"]], True
        raise KeyError(f"unknown dataset {name!r}")
    return [FIXTURES / rel for rel in archive_paths(fixture_index, target)], False


def search_args(query):
    args = []
    if query.get("ignore_case"):
        args.append("-i")
    if "tge" in query:
        args += ["--tge", str(query["tge"])]
    if "tle" in query:
        args += ["--tle", str(query["tle"])]
    return args


def run_search(archives, query, handler_args, work):
    """Runs C++ `clp-s s` over the archives (a temp dir of copies when there are several)."""
    if len(archives) == 1:
        archive_arg = archives[0]
    else:
        archive_arg = fresh_dir(work / (query["id"] + ".archives"))
        for a in archives:
            os.symlink(a, archive_arg / a.name)
    cmd = [CLP_S, "s", archive_arg, query["kql"]] + search_args(query) + handler_args
    if query.get("projection"):
        cmd += ["--projection"] + query["projection"]
    return run(cmd, check=False)


def mongo_start():
    run(["docker", "rm", "-f", MONGO_CONTAINER], check=False)
    run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            MONGO_CONTAINER,
            "-p",
            f"127.0.0.1:{MONGO_PORT}:27017",
            MONGO_IMAGE,
        ]
    )
    import pymongo

    client = pymongo.MongoClient(f"mongodb://127.0.0.1:{MONGO_PORT}", serverSelectionTimeoutMS=2000)
    for _ in range(60):
        try:
            client.admin.command("ping")
            return client
        except Exception:  # noqa: BLE001 - waiting for the server to come up
            time.sleep(1)
    raise RuntimeError("mongo did not start")


def mongo_stop():
    run(["docker", "rm", "-f", MONGO_CONTAINER], check=False)


def bson_canonical(doc):
    from bson import json_util

    return json.loads(json_util.dumps(doc, json_options=json_util.CANONICAL_JSON_OPTIONS))


def decode_msgpack_tuples(data):
    import msgpack

    unpacker = msgpack.Unpacker(raw=True, strict_map_key=False)
    unpacker.feed(data)
    out = []
    for item in unpacker:
        out.append(
            [
                x.decode("utf-8", errors="surrogateescape") if isinstance(x, bytes) else x
                for x in item
            ]
        )
    return out


def id_key(fixture):
    if fixture.startswith("search."):
        return "rid"
    if fixture.startswith("keys."):
        return "kid"
    return "id"


def timestamp_key(flags):
    if "--timestamp-key" in flags:
        return flags[flags.index("--timestamp-key") + 1]
    return None


class ReferenceChecker:
    """Runs scripts/reference_eval.py over a fixture and compares it with the C++ oracle."""

    def __init__(self, fixture_index):
        self.rows = {row["fixture"]: row for row in fixture_index["archive"]}
        self.records = {}

    def load(self, fixture, archives, work):
        if fixture in self.records:
            return self.records[fixture]
        row = self.rows[fixture]
        records = []
        for archive in archives:
            mp_path = work / "reference-all.msgpack"
            if mp_path.exists():
                mp_path.unlink()
            run([CLP_S, "s", archive, "*", "file", "--path", mp_path])
            tuples = decode_msgpack_tuples(mp_path.read_bytes())
            loaded = reference_eval.load_records(tuples, timestamp_key(row["flags"]))
            for record, tup in zip(loaded, tuples):
                record.filename = (
                    None if "--disable-log-order" in row["flags"] else row["input"]
                )
                record.canonical = canonical_line(tup[1])
            records += loaded
        self.records[fixture] = records
        return records

    def check(self, query, archives, stdout, work):
        fixture = query["target"]
        if query.get("projection"):
            return {"reference_status": "skipped"}
        records = self.load(fixture, archives, work)
        key = id_key(fixture)
        try:
            indices = reference_eval.run_query(
                records,
                query["kql"],
                query.get("ignore_case", False),
                query.get("tge"),
                query.get("tle"),
            )
        except reference_eval.QueryError as e:
            return {"reference_status": f"reference-error: {e}"}
        ref_lines = sorted(records[i].canonical for i in indices)
        cpp_lines = sorted(canonical_line(line) for line in stdout.decode().splitlines() if line)
        ref_ids = sorted(
            (records[i].value.get(key) for i in indices if isinstance(records[i].value, dict)),
            key=lambda v: (str(type(v)), v),
        )
        cpp_ids = sorted(
            (json.loads(line).get(key) for line in stdout.decode().splitlines() if line),
            key=lambda v: (str(type(v)), v),
        )
        out = {"reference_count": len(ref_lines)}
        if all(v is not None for v in ref_ids + cpp_ids):
            out["cpp_ids"] = cpp_ids
            out["reference_ids"] = ref_ids
        if "divergence" not in query:
            out["reference_status"] = "match" if ref_lines == cpp_lines else "UNEXPLAINED-MISMATCH"
            return out
        expected = query.get("expected_rust_ids")
        if expected is None:
            expected_count = query.get("expected_rust_count")
            if expected_count is None:
                out["reference_status"] = "registered-divergence"
            elif expected_count != len(ref_lines):
                out["reference_status"] = "EXPECTED-RUST-COUNT-DISAGREES-WITH-REFERENCE"
            elif expected_count == len(cpp_lines):
                out["reference_status"] = "DIVERGENCE-NOT-REPRODUCED"
            else:
                out["reference_status"] = "registered-divergence"
            return out
        expected = sorted(expected, key=lambda v: (str(type(v)), v))
        if ref_ids != expected:
            out["reference_status"] = "EXPECTED-RUST-IDS-DISAGREE-WITH-REFERENCE"
        elif cpp_ids == expected:
            out["reference_status"] = "DIVERGENCE-NOT-REPRODUCED"
        else:
            out["reference_status"] = "registered-divergence"
        return out


def dataset_reference_count(suite, query):
    """Counts the matches of the reference model over the dataset's C++ `x` oracle."""
    name = query["target"][len("dataset:") :]
    oracle = DATA_ROOT / "oracles" / "datasets" / f"{name}.x.jsonl"
    node_count = 0
    if query.get("tge") is not None or query.get("tle") is not None:
        raise ValueError("dataset reference counts do not model time bounds")
    batch = []
    with open(oracle, encoding="utf-8", errors="surrogateescape") as f:
        for line in f:
            batch.append(
                reference_eval.Record(reference_eval._wrap_int64(json.loads(line)), None, None)
            )
            if len(batch) == 100_000:
                node_count += len(
                    reference_eval.run_query(batch, query["kql"], query.get("ignore_case", False))
                )
                batch = []
    if batch:
        node_count += len(
            reference_eval.run_query(batch, query["kql"], query.get("ignore_case", False))
        )
    return node_count


def cmd_query_oracles(args):
    suite = load_toml(QUERIES / "suite.toml")
    fixture_index = load_toml(ARCHIVES / "index.toml")
    work = fresh_dir(WORK / "query-oracles")
    manifest_path = QUERY_ORACLES / "manifest.toml"
    existing = {}
    if manifest_path.exists() and (args.id or args.only_fixtures or args.only_datasets):
        for row in load_toml(manifest_path).get("oracle", []):
            existing[(row["id"], row["output"])] = row
    dataset_oracles = DATA_ROOT / "oracles" / "queries"
    dataset_oracles.mkdir(parents=True, exist_ok=True)
    QUERY_ORACLES.mkdir(parents=True, exist_ok=True)
    (QUERY_ORACLES / "msgpack").mkdir(exist_ok=True)
    (QUERY_ORACLES / "results-cache").mkdir(exist_ok=True)
    mongo = None
    rows = dict(existing)
    reference = ReferenceChecker(fixture_index)
    problems = []
    try:
        for query in suite["query"]:
            if args.id and query["id"] not in args.id:
                continue
            archives, is_dataset = query_targets(query, fixture_index, suite)
            if args.only_fixtures and is_dataset:
                continue
            if args.only_datasets and not is_dataset:
                continue
            base = {
                "id": query["id"],
                "target": query["target"],
                "archives": [
                    str(a.relative_to(DATA_ROOT if is_dataset else FIXTURES)) for a in archives
                ],
            }
            out_dir = dataset_oracles if is_dataset else QUERY_ORACLES
            for output in query.get("outputs", ["stdout"]):
                if output == "stdout":
                    result = run_search(archives, query, [], work)
                    oracle = out_dir / f"{query['id']}.jsonl"
                    oracle.write_bytes(result.stdout)
                    if is_dataset:
                        count, canon, invalid = canonical_digest_file(oracle)
                    else:
                        count, canon, invalid = canonical_digest(result.stdout)
                    row = dict(base, output="stdout")
                    row.update(
                        {
                            "file": str(oracle.relative_to(DATA_ROOT if is_dataset else QUERIES)),
                            "cpp_exit": result.returncode,
                            "count": count,
                            "sha256": sha256_file(oracle),
                            "canonical_sha256": canon,
                            "invalid_json_lines": invalid,
                        }
                    )
                    stderr = result.stderr.decode(errors="replace").strip()
                    if stderr:
                        row["cpp_stderr"] = stderr[-300:]
                    if is_dataset and "expected_rust_count" in query:
                        ref_count = dataset_reference_count(suite, query)
                        row["reference_count"] = ref_count
                        if ref_count != query["expected_rust_count"]:
                            row["reference_status"] = "EXPECTED-RUST-COUNT-DISAGREES-WITH-REFERENCE"
                        elif ref_count == count:
                            row["reference_status"] = "DIVERGENCE-NOT-REPRODUCED"
                        else:
                            row["reference_status"] = "registered-divergence"
                        if row["reference_status"] != "registered-divergence":
                            problems.append(f"{query['id']}: {row['reference_status']}")
                    if not is_dataset:
                        row.update(reference.check(query, archives, result.stdout, work))
                        status = row["reference_status"]
                        if status not in ("match", "registered-divergence", "skipped"):
                            problems.append(f"{query['id']}: {status}")
                elif output == "msgpack":
                    mp_path = work / f"{query['id']}.msgpack"
                    if mp_path.exists():
                        mp_path.unlink()
                    result = run_search(archives, query, ["file", "--path", mp_path], work)
                    data = mp_path.read_bytes() if mp_path.exists() else b""
                    raw_oracle = QUERY_ORACLES / "msgpack" / f"{query['id']}.msgpack"
                    raw_oracle.write_bytes(data)
                    tuples = decode_msgpack_tuples(data)
                    decoded = QUERY_ORACLES / "msgpack" / f"{query['id']}.json"
                    decoded.write_text(
                        json.dumps(tuples, ensure_ascii=False, indent=1) + "\n", encoding="utf-8"
                    )
                    row = dict(base, output="msgpack")
                    row.update(
                        {
                            "file": str(raw_oracle.relative_to(QUERIES)),
                            "decoded": str(decoded.relative_to(QUERIES)),
                            "cpp_exit": result.returncode,
                            "count": len(tuples),
                            "sha256": sha256_bytes(data),
                        }
                    )
                elif output == "results-cache":
                    if mongo is None:
                        mongo = mongo_start()
                    collection = query["id"].replace("-", "_")
                    db = mongo["proto"]
                    db.drop_collection(collection)
                    max_num = query.get("max_num_results", 10)
                    handler = [
                        "results-cache",
                        "--uri",
                        f"mongodb://127.0.0.1:{MONGO_PORT}/proto",
                        "--collection",
                        collection,
                        "--max-num-results",
                        str(max_num),
                    ]
                    if query.get("dataset_name"):
                        handler += ["--dataset", query["dataset_name"]]
                    result = run_search(archives, query, handler, work)
                    docs = [bson_canonical(d) for d in db[collection].find({})]
                    for d in docs:
                        d.pop("_id_generated", None)
                    docs.sort(key=lambda d: json.dumps(d, sort_keys=True))
                    oracle = QUERY_ORACLES / "results-cache" / f"{query['id']}.json"
                    oracle.write_text(
                        json.dumps(docs, ensure_ascii=False, indent=1, sort_keys=True) + "\n",
                        encoding="utf-8",
                    )
                    row = dict(base, output="results-cache")
                    row.update(
                        {
                            "file": str(oracle.relative_to(QUERIES)),
                            "cpp_exit": result.returncode,
                            "count": len(docs),
                            "max_num_results": max_num,
                        }
                    )
                else:
                    raise ValueError(f"unknown output {output!r}")
                rows[(query["id"], output)] = row
                print(
                    f"query {query['id']} [{output}]: exit={row['cpp_exit']} count={row['count']}"
                )
    finally:
        if mongo is not None:
            mongo_stop()
    for problem in problems:
        print(f"REFERENCE CHECK: {problem}")
    order = {q["id"]: i for i, q in enumerate(suite["query"])}
    ordered = sorted(rows.values(), key=lambda r: (order.get(r["id"], 1 << 30), r["output"]))
    header = (
        "GENERATED by scripts/gen-oracles.sh from tests/queries/suite.toml; do not edit.\n"
        "One [[oracle]] per (query, output). Fixture archives are relative to tests/fixtures/ and\n"
        "fixture oracle files to tests/queries/; dataset archives and oracle files are relative\n"
        "to PROTO_DATA_ROOT (large). `count` is the number of result lines/tuples/documents;\n"
        "`canonical_sha256` follows tests/README.md."
    )
    manifest_path.write_text(
        dump_toml(header, {"build": build_info()}, {"oracle": ordered}), encoding="utf-8"
    )


def expand_input_path(path):
    """Expands `~` and environment variables in a dataset input path of the suite."""
    expanded = os.path.expandvars(os.path.expanduser(path))
    if "$" in expanded:
        raise RuntimeError(f"dataset input {path!r} names an unset environment variable")
    return Path(expanded)


def dataset_inputs(ds):
    if "inputs" in ds:
        return [expand_input_path(p) for p in ds["inputs"]]
    with open(expand_input_path(ds["inputs_from"])) as f:
        return [expand_input_path(line.strip()) for line in f if line.strip()]


def cmd_datasets(args):
    suite = load_toml(QUERIES / "suite.toml")
    ds_dir = DATA_ROOT / "datasets"
    ds_dir.mkdir(parents=True, exist_ok=True)
    oracle_dir = DATA_ROOT / "oracles" / "datasets"
    oracle_dir.mkdir(parents=True, exist_ok=True)
    rows = []
    manifest_path = QUERIES / "datasets.toml"
    previous = {}
    if manifest_path.exists():
        for row in load_toml(manifest_path).get("dataset", []):
            previous[row["name"]] = row
    for ds in suite["dataset"]:
        if args.name and ds["name"] not in args.name:
            rows.append(previous[ds["name"]])
            continue
        inputs = dataset_inputs(ds)
        work = fresh_dir(WORK / f"dataset-{ds['name']}")
        flags = ["--timestamp-key", ds["timestamp_key"]] + ds.get("flags", [])
        cmd = [CLP_S, "c", "--single-file-archive", "--print-archive-stats"] + flags + [work]
        cmd += [str(p) for p in inputs]
        result = run(cmd)
        stats = [json.loads(line) for line in result.stdout.decode().splitlines() if line.strip()]
        if len(stats) != 1:
            raise RuntimeError(f"dataset {ds['name']} produced {len(stats)} archives")
        archive = DATA_ROOT / ds["archive"]
        if archive.exists():
            archive.unlink()
        shutil.move(str(work / stats[0]["id"]), archive)
        xdir = work / "x"
        xresult, _ = cpp_extract_to_file(archive, xdir)
        oracle = oracle_dir / f"{ds['name']}.x.jsonl"
        shutil.move(str(xdir / "original"), oracle)
        count, canon, invalid = canonical_digest_file(oracle)
        version, _ = read_archive_version(archive)
        rows.append(
            {
                "name": ds["name"],
                "archive": ds["archive"],
                "archive_sha256": sha256_file(archive),
                "archive_size": archive.stat().st_size,
                "format_version": version,
                "flags": flags,
                "inputs": [p.name for p in inputs],
                "input_records": sum(count_lines(p) for p in inputs),
                "begin_timestamp": stats[0]["begin_timestamp"],
                "end_timestamp": stats[0]["end_timestamp"],
                "x_oracle": str(oracle.relative_to(DATA_ROOT)),
                "x_exit": xresult.returncode,
                "x_records": count,
                "x_sha256": sha256_file(oracle),
                "x_canonical_sha256": canon,
                "x_invalid_json_lines": invalid,
            }
        )
        print(f"dataset {ds['name']}: {count} records, archive {archive.stat().st_size} B")
        shutil.rmtree(work)
    header = (
        "GENERATED by scripts/gen-oracles.sh (datasets step); do not edit.\n"
        "Dataset SFAs and their C++ `clp-s x` oracles live under PROTO_DATA_ROOT (large);\n"
        "`archive` and `x_oracle` are relative to it, and `inputs` lists the input file names."
    )
    manifest_path.write_text(
        dump_toml(header, {"build": build_info()}, {"dataset": rows}), encoding="utf-8"
    )


def cpp_extract_to_file(archive_path, out_dir):
    fresh_dir(out_dir)
    result = run([CLP_S, "x", archive_path, out_dir], check=True)
    return result, out_dir / "original"


def count_lines(path):
    n = 0
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 22), b""):
            n += chunk.count(b"\n")
    return n


def cmd_canonical_sha(args):
    count, digest, invalid = canonical_digest_file(Path(args.file))
    print(json.dumps({"count": count, "canonical_sha256": digest, "invalid": invalid}))


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("build-fixtures")
    sub.add_parser("fixture-oracles")
    q = sub.add_parser("query-oracles")
    q.add_argument("--only-fixtures", action="store_true")
    q.add_argument("--only-datasets", action="store_true")
    q.add_argument("--id", action="append")
    d = sub.add_parser("datasets")
    d.add_argument("--name", action="append")
    c = sub.add_parser("canonical-sha")
    c.add_argument("file")
    args = parser.parse_args()
    WORK.mkdir(parents=True, exist_ok=True)
    {
        "build-fixtures": cmd_build_fixtures,
        "fixture-oracles": cmd_fixture_oracles,
        "query-oracles": cmd_query_oracles,
        "datasets": cmd_datasets,
        "canonical-sha": cmd_canonical_sha,
    }[args.cmd](args)


if __name__ == "__main__":
    main()
