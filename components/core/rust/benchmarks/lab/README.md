# LAB search benchmark

A harness that runs the search part of [Log Archival Bench][lab] (LAB) against the C++ `clp-s` and
the Rust `clp-s-rs` on the same archives. It keeps LAB's datasets, queries and per-query procedure,
and adds repetitions and a few extra metrics. Ingestion is not measured: every archive is written
by the C++ `clp-s` with LAB's flags.

Tools (`config.yaml` `tools`):

| tool | reader | archives |
|---|---|---|
| `cpp-mfa` | C++ `clp-s s` | LAB's default multi-file archives (reference row) |
| `cpp-sfa` | C++ `clp-s s` | single-file archives |
| `rust-sfa` | `clp-s-rs search` | the same single-file archives |

The Rust reader reads single-file archives (SFA) only, so the head-to-head comparison is
`rust-sfa` against `cpp-sfa`. `cpp-mfa` shows how the C++ reader performs on the archives LAB
actually uses.

## Files

| file | purpose |
|---|---|
| `config.yaml` | Methodology parameters, reader commands, tools and datasets (inputs, archive locations, LAB's queries verbatim and expected counts). |
| `prepare.py` | Builds a dataset's input and compresses its MFA and SFA archives (not measured). |
| `run.py` | Runs the benchmark and writes the raw results, the summary and the report. |
| `report.py` | Regenerates `summary.json` and `report.md` from a results directory. |
| `lab_common.py` | The configuration model shared by the scripts. |

The scripts need Python 3.10+ and PyYAML. They carry inline script metadata, so `uv run run.py`
provides PyYAML; `python3 run.py` works where PyYAML is installed.

## Running

Build the Rust reader in release mode and point the harness at both binaries and a data root:

```bash
cargo build --release -p clp-s-cli
export CLP_S=/path/to/build/core/clp-s       # default: <repo>/build/core/clp-s
export CLP_S_RS=/path/to/release/clp-s-rs    # default: $CARGO_TARGET_DIR/release/clp-s-rs
export LAB_DATA_ROOT=/path/to/lab-data       # default: <workspace>/target/lab-data
```

Validate the harness on the small `smoke` dataset first. It uses the `bench` dataset archive of the
differential tests (`$PROTO_DATA_ROOT/datasets/bench.clps`, plus its decompressed JSON lines under
`$PROTO_DATA_ROOT/oracles/datasets/`), compresses the MFA copy on first use, and runs every tool,
mode and query twice:

```bash
uv run benchmarks/lab/run.py --smoke
```

Prepare and run a LAB dataset (here MongoDB, the only dataset LAB queries). The commands run from
this workspace directory:

```bash
uv run benchmarks/lab/prepare.py --dataset mongod --steps download input mfa sfa clean-input
uv run benchmarks/lab/run.py --dataset mongod             # 5 repetitions by default
uv run benchmarks/lab/report.py "$LAB_DATA_ROOT/mongod/results/<stamp>" --print
```

Datasets can be prepared and benchmarked one at a time to bound disk usage: `clean-input` deletes
the uncompressed input once both archive layouts exist, and the archive directories can be removed
after the run. The MongoDB input is 64.8 GiB; its archives are a small fraction of that.

Useful options of `run.py`: `--tools`, `--modes`, `--queries` (1-based), `--repetitions`,
`--timeout-s`, `--output-dir`, and `--no-clear-cache` (debugging only: cold runs are then not
cold).

## Methodology

LAB's search procedure (`src/template.py` `bench_search`, `assets/clp/main.py`) and how the harness
executes it:

| LAB | harness |
|---|---|
| For each mode (cold, then hot), for each query: | Same order, inside each repetition. Within a (mode, query), every tool runs its own full step in turn. |
| Clear caches: `sync; echo 1 > /proc/sys/vm/drop_caches` inside the privileged tool container. | The same command in a privileged throwaway container (`search.clear_cache_command`), so no host root access is needed. It drops the shared kernel's page cache, so it needs a container that shares the host kernel (native Docker, or Docker on WSL2). |
| Hot only: run the query 3 times to warm up. | Same (`hot_run_warm_up_times: 3`), with the same command as the measured run. |
| Start a thread that runs `ps aux` every 0.1 s (aligned to multiples of 0.1 s) and sums the RSS of the tool's processes. | Same interval and alignment. `ps` sums the RSS of the processes in the measured run's process tree whose program is the reader binary. A sample taken before the reader starts or after it exits is 0, as in LAB. |
| Time the query end to end (`docker exec clp-s s ARCHIVES QUERY`, output captured). | Time the reader end to end: `/usr/bin/time -v taskset -c CPUS READER ...`, from spawn until stdout is drained and the process is reaped. |
| Average the samples; -1 if there is none. | Same (`memory_average_B`). Runs without a sample are left out of the summary's median and counted in the report. |
| Result = number of output lines (`count('\n') + 1` of the stripped output, 0 if empty). | Number of stdout lines, counted from a pipe in 1 MiB reads; nothing is written to disk. This equals LAB's count for newline-terminated output. |
| `check_results`: compare with the expected counts. | Same counts (`expected_count`); every run records `count_ok`. |

If a global `sync` blocks on the host (for example, on a hung FUSE mount in a shared WSL2 kernel),
pass `--config` a copy whose `clear_cache_command` flushes only the data filesystem
(`sync -f <data dir>`) before the same `drop_caches`.

The queries are LAB's `assets/clp/config.yaml` strings, verbatim (`lab_query`). LAB builds the
command as one string and splits it with `shlex.split`, so the KQL passed to a reader is
`shlex.split(lab_query)`, which the harness requires to be a single argument.

The archives are compressed like LAB's `ingest`: `clp-s c --timestamp-key "t.$date"
--target-encoded-size 268435456`. The SFA set uses the same flags plus `--single-file-archive`.
`prepare.py input` rebuilds LAB's `mongod.log` from the dataset tarball in one streaming pass: it
concatenates the files, ends every line with a newline and drops empty lines, like the `awk` and
`sed` commands of `scripts/download_all.py`. LAB orders the files by name; the pass streams them in
tarball order and warns if the two differ (the order changes the archive layout, not the records).

Both readers get the archive directory. C++ searches every archive in it. `clp-s-rs` treats a
directory that is not itself a multi-file archive as a directory of single-file archives and
searches its entries, sorted by name, as one session. Both see the same set of archives.

### Deliberate differences from LAB

- **CPU limit.** LAB runs each tool in a container with `--cpus=4`, a CFS quota of four CPUs'
  worth of time. The harness runs the readers natively, pinned with `taskset -c 8-11`
  (`search.cpu_set`). Pinning limits the reader to those CPUs; on a machine with SMT, check the
  topology (`lscpu -e`), because four logical CPUs may be only two physical cores. Both readers
  search on one thread (their CPU time stays close to their wall time), so the limit mainly keeps
  other work off the measured cores.
- **No container around the reader.** LAB's timing includes `docker exec` and captures output
  through Docker; the harness has no such overhead. Wrapping in `/usr/bin/time` and `taskset` adds
  under a millisecond per run.
- **Single-file archives.** The Rust reader does not read multi-file archives, so the comparison
  uses SFAs written with LAB's flags. The `cpp-mfa` row keeps LAB's own archive layout as a
  reference.
- **Repetitions.** LAB runs the whole procedure once. The harness repeats it N times (default 5)
  and keeps every run. Within a repetition, the tools run interleaved per (mode, query), and their
  order rotates every repetition, so drift and order effects reach every tool equally. The summary
  reports the median and the min/max over repetitions.
- **Extra metrics.** Each run also records `/usr/bin/time -v`: peak RSS (`ru_maxrss` of the
  reader), user and system CPU time, page faults, file-system inputs and context switches. The raw
  `ps` samples and their average over non-zero samples are kept too. LAB's average RSS includes the
  0 samples taken while the process starts or exits. For runs shorter than a few polling intervals,
  that average is noise; the peak RSS is the steadier metric.
- **stderr.** LAB merges stderr into the counted output; the harness counts stdout only, records the
  number of stderr lines and their tail, and lists runs with stderr output in the report.

## Outputs

`run.py` writes a results directory (default `$LAB_DATA_ROOT/<dataset>/results/<stamp>/`):

- `meta.json`: host (CPU, kernel, memory), workspace commit, parameters, the queries, each reader
  binary (path, size, build time, SHA-256, `--version`), and each archive set (count, files, size,
  and the `prepare.py` record).
- `results.jsonl`: one record per measured run, appended as it finishes. It holds the tool,
  mode, query, repetition and position, argv, start time, `time_taken_s`, exit code, `result`,
  `count_ok`, stdout bytes, stderr lines and tail, the `ps` samples and their averages, the
  `/usr/bin/time` metrics, the cache-clear time, and the warm-up runs (time, result, exit code).
- `summary.json`: per (tool, mode, query), the median/min/max of time, average RSS, peak RSS, CPU
  time and major page faults, with the distinct result counts and the count check.
- `report.md`: result counts against the expected counts, latency tables per mode with speedup
  columns (`report.speedups` in the config), then average RSS, peak RSS and CPU time.

## Adding a dataset

Add an entry under `datasets` with its input (or `tarball` and `source`), `timestamp_key`,
`target_encoded_size`, `archives.mfa` and `archives.sfa` (relative to the data root), and its
queries as `lab_query` strings with their `expected_count`. Then run `prepare.py --dataset NAME`
and `run.py --dataset NAME`.

[lab]: https://github.com/y-scope/log-archival-bench
