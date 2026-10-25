# Streaming search integration with Spider

**Date:** 2026-10-19. **Tree:** branch `query-coordinator/streaming-search-rust-reader` at
`e10364cc`, plus uncommitted changes (listed in [Design as built](#3-design-as-built)). **C++
reference and package:** `7365387b`, the head of the streaming-search API branch, built
separately. **Status:** smoke-tested on the real dataset; the benchmark has not run yet. The design
sections describe stream protocol v2 (a per-task trailer and the listener's acknowledgement),
which replaced protocol v1 after the smoke test; the smoke test in [§6](#6-smoke-test-results) ran
protocol v1.

This document records how the Rust clp-s reader was put into the Spider streaming-search path,
and what the smoke test of that integration found. The reader itself is described in
[design.md](design.md), and its conformance and standalone performance are in
[prototyping-and-benchmark.md](prototyping-and-benchmark.md).

## 1. Summary

- **V3 ("Spider + Rust")** runs the Spider search task `query::clp_s_search` through the reader
  library, in the Spider task executor's own process, instead of spawning the C++ `clp-s` for
  each archive. It streams results to the search-result listener with the same protocol bytes as
  the C++ `network` output.
  - V2 (Spider + C++ `clp-s`) and V3 ship in one `libclp_tdl_package.so` and one worker image.
    The environment variable `CLP_SPIDER_SEARCH_ENGINE` selects the engine for each worker
    container.
- **Correctness.** On 607 archives of Hadoop logs (158 GiB raw), every query returned the same
  results in all three variants (V1 Celery, V2, V3):
  - `*Transmitted*` (1,071 results), `*Exception*` (107,867) and `*ProtobufRpcEngine*`
    (3,221,319; V2 and V3 only) have identical counts and identical sorted digests of the
    messages, of `<timestamp> <message>` and of `<archive_id> <timestamp>: <message>`.
  - Within each archive, V3 sends its results in the same order as V2, on every archive with
    results.
  - V3 never spawned `clp-s`: a process sampler saw no `clp-s` process in any worker, and all
    3,642 V3 task-completion log lines report `is_clp_s_subprocess_used: false`.
  - With two workers killed mid-stream, the job still succeeded with the reference results. The
    listener dropped the 1,597 results that the retries sent again.
- **Smoke-test timings (single runs, not a benchmark).** Total time to the last result, V2 → V3:

  | query | client | V2 Spider + C++ | V3 Spider + Rust |
  |---|---|---|---|
  | `*Transmitted*` | `POST /query/stream` | 2.14 s | 0.82 s |
  | `*Exception*` | `POST /query/stream` | 9.03 s | 3.55 s |
  | `*Exception*` | `sbin/streaming-search.sh --raw` | 10.70 s | 4.81 s |
  | `*ProtobufRpcEngine*` | `sbin/streaming-search.sh` | 12.84 s | 6.03 s |

  The first result arrived after 179 ms (V2) and 119 ms (V3) on `*Transmitted*`, and after 126 ms
  and 109 ms on `*Exception*`, both over `POST /query/stream`. The Celery CLI (V1) took 9.80 s
  and 17.30 s for the two queries. Every value is one run; [§6](#6-smoke-test-results) has the
  full table and the conditions of each run.
- **Review.** A code review found four small items. Three are applied and verified, and one is left
  as is because it conflicts with a denied clippy lint. The smoke-tested worker image predates all
  four. None of them changes behaviour on the measured path ([§7](#7-review-status)).

## 2. Context

### 2.1 The streaming-search path on Spider

The streaming-search path replaces the results cache with a direct stream from each search task
to a listener that the client hosts:

1. **Client.** `sbin/streaming-search.sh` (the Rust `clp-streaming-search`) or the API server's
   `POST /query/stream` opens a session on a `ResultListener`, then inserts a query job whose
   config carries a network output: the listener's host and port, and the session token (a
   UUID). Streaming jobs always have `max_num_results = 0`.
2. **Coordinator.** The `query-coordinator` submits one Spider job with one `query::clp_s_search`
   task per archive. Each task gets a task index: the archive's position after sorting the
   archives by end timestamp, newest first. Spider persists the task graph, so a retry reuses
   the same index.
3. **Search task (V2).** The TDL task spawns
   `clp-s s <dir> --archive-id <id> <query> [--tge] [--tle] [--ignore-case] network --host H
   --port P --session-token T --task-index I`.
   A failed task is retried by Spider (`query_task_max_retry`).
4. **Protocol v2.** The task connects only when it has its first result, so an archive without
   results opens no connection. It then sends:
   - the handshake `[2, session_token, task_index, archive_id]`;
   - one frame `[result_index, timestamp_ms, message]` per result, with `result_index` counted
     from 0 for each attempt and `message` the JSON record followed by `\n`;
   - the trailer `[num_results]` (the count in its smallest unsigned encoding), then EOF by
     shutting down its sending side.

   It then waits for the listener's acknowledgement, the 10 bytes `[num_results]` with the count
   always a msgpack `uint 64` (`0x91 0xcf` and the count in big-endian), checks the prefix and the
   count, and only then succeeds. EOF or a reset before the acknowledgement, or a wrong one, fails
   the task, and Spider retries it. There is no timeout on the wait: a back-pressured consumer
   keeps the task waiting, and Spider's task timeouts bound the rest.
5. **Listener.** For each task index, the listener keeps a cursor holding the next unclaimed
   result index. A connection that receives result `r` claims it with one compare-and-swap from
   `r` to `r + 1`. If the cursor is already past `r`, an earlier attempt emitted the result, and
   the listener drops it as a duplicate. Each connection's indices must also be exactly 0, 1, 2,
   ... in order (`UnexpectedResultIndex` otherwise), so an index can never be above the cursor.
   - The listener acknowledges a connection only once its trailer counts every frame the
     connection sent (claimed or duplicate), no byte follows the trailer, EOF has arrived, and the
     last batch has been handed to the consumer (or discarded, once the consumer is gone). A
     wrong count (`ResultCountMismatch`) or bytes after the trailer (`DataAfterTrailer`) are
     protocol errors. A connection that ends without its trailer or is cut off by the drain is
     counted in `num_incomplete_connections`. Neither is acknowledged.
   - Dedup by `(task_index, result_index)` is correct only if every attempt of a task produces
     the same results in the same order. A Rust engine must keep that property
     ([§3.6](#36-determinism)).
6. **Completion.** The client learns that the job is over by polling `query_jobs.status` every
   100 ms, not from the connections. It then drains the open connections and reports the
   terminal status. Once the job has succeeded, every task has had its results acknowledged, so
   the drain stops reading the remaining (stale) connections at once; a batch already claimed is
   still delivered.

### 2.2 The three variants

| | V1 Celery CLI | V2 Spider + C++ | V3 Spider + Rust |
|---|---|---|---|
| scheduler | query-scheduler and a Celery query-worker | query-coordinator and Spider | query-coordinator and Spider |
| 8 search slots | one query-worker with `--concurrency 8` | 8 spider-worker containers × 1 executor | the same 8 containers, `CLP_SPIDER_SEARCH_ENGINE=rust` |
| dispatch | 38 sequential sub-jobs of up to 16 archives | 1 Spider job, 607 tasks | 1 Spider job, 607 tasks |
| per-archive search | `clp-s` subprocess | `clp-s` subprocess | the reader library, in the executor process |
| clients | `sbin/search.sh` (Python, no dedup, 0.5 s status poll) | `sbin/streaming-search.sh`, `POST /query/stream` | the same as V2 |

All three variants read the same 607 single-file archives and use the same C++ `clp-s` build.

## 3. Design as built

V3 extends the existing in-process adapter `clp-s-spider-adapter`, which until now wrote only into
the results cache, so that it also streams protocol v2. It adds one encoder in `clp-s-wire` and
one sink in `clp-s-sinks`, and the TDL search task dispatches to the adapter when the engine is
`rust`.

Files changed:

- Repository workspace: `Cargo.toml`, `Cargo.lock`, `components/clp-tdl-package/{Cargo.toml,
  src/common.rs, src/lib.rs, src/task/query/mod.rs}`, `components/search-result-listener/
  {Cargo.toml, tests/spider_adapter.rs (new)}` and `tools/deployment/package/
  compose.clp-spider.yaml`.
- Reader workspace (`components/core/rust`): `Cargo.lock`, `README.md`,
  `crates/clp-s-wire/src/{encode.rs (new), stream.rs (new), error.rs, lib.rs, tuple.rs}`,
  `crates/clp-s-sinks/{src/network.rs (new), src/lib.rs, tests/network.rs (new)}`,
  `crates/clp-s-spider-adapter/{Cargo.toml, src/lib.rs, src/search.rs, src/subprocess.rs,
  tests/e2e.rs, tests/network.rs (new)}` and `crates/clp-s-resources/src/budget.rs`.
- No new third-party crate, and no change to the worker Dockerfile or the coordinator. Protocol v2
  also changes the listener (`components/search-result-listener`), the api-server's `end` event
  and the CLI's outcome log (`num_incomplete_connections`), the C++ `NetworkOutputHandler` and the
  Celery path's Python reader.

### 3.1 The engine switch: `CLP_SPIDER_SEARCH_ENGINE`

- **Values.** Unset or `clp-s` selects `SearchEngine::ClpS`, the unchanged C++ path (V2). `rust`
  selects `SearchEngine::Rust` (V3).
- **Fail fast.** Any other value, including an empty string, `Rust` or `cpp`, makes
  `package_init` fail, and so does a value that is not valid Unicode. A typo therefore cannot run
  V2 under the V3 label.
- **Once per process.** `package_init` reads the variable into a `OnceLock` and logs
  `clp TDL package initialized.` with a `search_engine` field (`ClpS` or `Rust`).
  - Executors are long-lived and run one task at a time, so `package_init` runs once per
    executor process, and the engine is fixed for its lifetime.
  - With `rust`, `package_init` also installs the adapter's process-wide resources: the package's
    multi-thread Tokio runtime, a `ReaderContext` with the default (unlimited) memory budget, the
    CLP home, and a 60 s base admission timeout. These stay alive across tasks.
- **Dispatch.** `query::clp_s_search` matches on the engine: `ClpS` calls the package's own
  `search::search`, and `Rust` calls `clp_s_spider_adapter::search` with the same inputs. Errors
  still become `TdlError::ExecutionError`, so Spider retries a failed task as before.
- **One image.** Both engines are in the same `libclp_tdl_package.so`. The `spider-worker`
  service in `compose.clp-spider.yaml` gains
  `CLP_SPIDER_SEARCH_ENGINE: "${CLP_SPIDER_SEARCH_ENGINE:-clp-s}"`. The execution manager passes
  its whole environment to its executors, so switching engines means recreating the
  `spider-worker` containers with another value.

### 3.2 `clp-s-wire`: `StreamHandshake`, `StreamResult`, `StreamTrailer` and `StreamAck`

`stream.rs` mirrors `NetworkOutputHandler` in `components/core/src/clp_s/OutputHandlerImpl.cpp`:

- `StreamHandshake { session_token, task_index, archive_id }` encodes as a msgpack array of 4:
  the version `STREAM_PROTOCOL_VERSION = 2`, the token string, the task index and the archive id
  string.
- `StreamResult { result_index, timestamp_ms, message }` encodes as a msgpack array of 3: the
  result index, the timestamp and the message. The message is the record's JSON followed by `\n`,
  with invalid UTF-8 replaced by U+FFFD, as the reader's msgpack tuples already do (DV-45). The
  listener rejects invalid UTF-8 as a protocol error.
- Integers use the smallest encoding (`rmp::encode::write_uint`/`write_sint`), as msgpack-c's
  `pack_uint64`/`pack_int64` do: a non-negative epoch in milliseconds is a `0xcf` uint64, not a
  `0xd3` int64. Strings use fixstr/str8/str16/str32 like msgpack-c.
- `StreamTrailer { num_results }` encodes as a msgpack array of 1 with the count in its smallest
  unsigned encoding.
- `StreamAck { num_results }` encodes as exactly `StreamAck::ENCODED_LEN` (10) bytes: `0x91 0xcf`
  and the count in big-endian. `StreamAck::decode` rejects any other prefix with
  `WireError::MalformedAck`.
- On an encoding error the output buffer is truncated back to where the frame started, like the
  existing `SearchResultTuple::encode`.

The msgpack helpers that `tuple.rs` had inline (string headers, the message with `\n`, the error
mapping) moved into the private module `encode.rs` and are shared by both formats. The tuple
format's bytes and tests are unchanged.

The result: the Rust bytes equal the C++ bytes, frame by frame, apart from the float formatting
divergence of the JSON text ([§5.2](#52-comparison-with-the-c-network-output)).

### 3.3 `clp-s-sinks`: `NetworkSink`

`NetworkSink` streams one search task to a listener. Its destination is the host, port, session
token, task index and archive id.

- **Sink contract.** It requires the rendered JSON and the timestamps, uses `MergeOrder::Scan`,
  and outputs the number of results streamed.
- **Lazy connect.** A partition connects (`TcpStream::connect`) only when it consumes a batch
  with at least one record. A search without results never connects, exactly like C++. The
  listener closes a connection that sends no handshake within 10 s of connecting, so an early
  connect would be a bug.
- **Numbering from 0.** The first write carries the handshake followed by the first batch's
  frames. Result indices start at 0 and stay contiguous across batches.
- **One `write_all` per batch.** Each batch is encoded into one buffer and written with one
  `write_all`. C++ instead calls `send()` once per result. Neither sets `TCP_NODELAY`.
- **Timestamps.** `epoch_ns_to_wire_ms` floors nanoseconds to milliseconds; C++ truncates, which
  differs only before 1970 (DV-27). A record without a timestamp is sent with 0 (DV-34).
- **Close.** `finish` sends the trailer, shuts down the write side, and blocks reading exactly the
  10 bytes of the acknowledgement, with no timeout. It succeeds only if the acknowledgement has
  the right prefix and counts every result. A partition dropped without `finish` (a failed
  session) sends no trailer, so the listener never acknowledges it.
- **Errors.** A broken pipe, a reset, an aborted connection, or EOF before the acknowledgement is
  `SinkError::DownstreamClosed`. A malformed acknowledgement or a wrong count is
  `SinkError::Custom`. Any of them, or any write or connect error, fails the session, the task
  returns `Err`, and Spider retries it.
- **Per-partition state, so a rerun renumbers.** The connection and the result counter live in
  the partial sink of the partition, not in the sink. The adapter reruns a session with the same
  sink when the memory budget refuses a stream (the `WaitForBase` retry). A counter shared on the
  sink would continue its numbering on the rerun, and the listener would emit the rerun's first
  results under new indices, i.e. a second time. With per-partition state, every rerun opens a
  new connection numbered from 0, and the listener drops what it already has.

### 3.4 The Spider adapter (`clp-s-spider-adapter`)

- **`TaskOutput`.** `SearchTask` now carries `output: TaskOutput`, either
  `ResultsCache { uri }` or `Network { host, port, session_token }`, plus the `task_index`.
  `SearchTask::new` accepts `OutputHandle::ResultsCache` and `OutputHandle::Network`, and still
  rejects `OutputHandle::File`.
- **`ArchiveToSearch`.** `search`, `search_with_clp_s_subprocess` and `SearchTask::new` take
  `ArchiveToSearch { id, dataset, task_index }` instead of separate arguments. Separate arguments
  would exceed clippy's `too_many_arguments`, and the package's own search function already uses
  this shape. The type is exported from the adapter.
- **Scan order for network output.** A network task always uses `ResultOrder::Scan` and ignores
  `max_num_results`, because the C++ network command line never passes `--max-num-results`
  (streaming jobs set it to 0 anyway). `ResultOrder::LatestFirst` stays for the results cache
  with a limit.
- **C++ fallback only on `UnsupportedArchive` before any write.** The adapter wraps the sink in
  `WriteTrackingSink`. It spawns the C++ `clp-s` only if the in-process search fails with
  `ErrorKind::UnsupportedArchive` and nothing was written. Because the sink connects lazily,
  nothing has reached the listener at that point, so the fallback's stream, numbered from 0, is
  the only one for that attempt. The fallback builds the network command line with the new
  `build_clp_s_search_args_for_network`, which follows the package's argument order. Any error
  after the first batch returns `Err`, and the retry starts again from index 0.
- **Unchanged.** `catch_unwind` at the task boundary (spider-tdl does not catch panics, and a
  panic would abort the executor), the `WaitForBase` admission loop, and the
  `clp-s query task completed successfully` log line with `is_clp_s_subprocess_used`, which the
  smoke test counts.

### 3.5 Cargo topology

The reader is a standalone Cargo workspace under `components/core/rust`, inside the repository
workspace's directory.

- **Root `exclude`.** Without it, a path dependency on a reader crate makes that crate an implicit
  member of the repository workspace. Its `workspace = true` keys then resolve against the root
  manifest and fail (`error inheriting 'publish' from workspace root manifest's
  'workspace.package.publish'`). The root `[workspace]` now has
  `exclude = ["components/core/rust"]`, and the reader crates keep inheriting from their own
  workspace.
- **Path workspace dependencies.** The root `[workspace.dependencies]` gains `clp-s-reader` and
  `clp-s-spider-adapter` as path dependencies, and `clp-tdl-package` depends on both. The
  listener's dev-dependencies add them (with `spider-core` and `spider-tdl`) for its new test.
- **Lockfile.** The root `Cargo.lock` gains exactly the 16 local `clp-s-*` packages and no
  registry package. `mongodb` stays at 3.8.0 in the root lock (the reader lock has 3.9.1); the
  reader compiles against it, so no bump was needed. The shared crates (`clp-rust-utils`,
  `spider-tdl`, `spider-core`, `tokio`, `rmp`, ...) resolve to one package each, so
  `OutputHandle` and `TaskContext` are the same types on both sides.
- **Lints.** The repository's `.cargo/config.toml` clippy denies apply to the reader crates when
  the package is built from the root.
- **Artifact.** `cargo build --release -p clp-tdl-package` produces a `libclp_tdl_package.so` of
  about 43 MB that links only libc, libm and libgcc_s and needs glibc 2.34 or newer. The worker
  base image (`ghcr.io/y-scope/spider/worker:sha-278d06f`) is Ubuntu 22.04 with glibc 2.35.

### 3.6 Determinism

Dedup by `(task_index, result_index)` needs every attempt of a task to send the same results in
the same order:

- The reader runs every scan on the calling thread (`Executor::CurrentThread`).
- Streams are visited in table-directory order, and the selected rows of each table in ascending
  order. Batch boundaries depend on the configuration (4,096 rows, 256 KiB of rendered JSON), but
  never change the order. This is the C++ order (`get_schema_ids()` in metadata order; DV-28).
- A task searches one archive, so its session has one partition.
- The C++ fallback can happen only before the first write
  ([§3.4](#34-the-spider-adapter-clp-s-spider-adapter)).

Evidence: two in-process attempts send identical bytes on every fixture case; the sink's rerun
test gets identical bytes over a new connection; and on the 607 real archives the per-archive
order of V3 equals that of V2 ([§6.2](#62-correctness-on-607-archives)).

## 4. Environment

### 4.1 Host and builds

- **Host:** 32 CPUs and 47 GiB as reported by Docker, Linux 6.6.87.2 on WSL2, Docker Desktop with
  WSL integration (compose `v2.40.3-desktop.1`). `PATH` had no `/mnt/c` entries.
- **Package (all non-worker services, and V1's `clp-s`):** a full `task package` of `7365387b` in
  a separate worktree, producing the image `clp-package:dev-lzh-4d47`. This tree changes no
  non-test code of the api-server, coordinator or listener, so the image serves all three
  variants.
- **Worker image (V2 and V3):** `sbench-clp-spider-worker:rust-ecbacb0e` (image
  `sha256:912e491c71c7…`), built from a minimal context with this tree's unmodified
  `tools/docker-images/clp-spider-worker/Dockerfile`:
  - this tree's `libclp_tdl_package.so` (sha256 `ecbacb0e1ace…`);
  - the C++ `clp-s` (sha256 `9f12dd8be138…`), `indexer` and `log-converter` of the `7365387b`
    build. V1 and V2 therefore run the same C++ binary.
- **Compose:** this tree's `compose.clp-spider.yaml`, which adds `CLP_SPIDER_SEARCH_ENGINE`,
  replaces the package's copy.

### 4.2 Dataset and compression

- **Input:** 658 files, 169,641,337,939 B (158 GiB) of plain-text Hadoop datanode logs (the
  HDFS dataset of the earlier streaming-search reports), with multi-line events.
- **Command:** `sbin/compress.sh --config etc/clp-config.yaml --unstructured <input dir>` in
  Celery mode, with the default compression concurrency (16 on this host), target archive size
  (256 MiB) and level (3). `--unstructured` runs `log-converter` to kv-IR, then
  `clp-s c ... --timestamp-key timestamp`.
- **Result:** 607 archives, all single files. The run took 239.3 s (237.457 s by the job's own
  duration). The archives total 4,958,627,287 B. CLP counts 211,244,814,088 B uncompressed (the
  kv-IR bytes, not the raw input) and reported a 42.60x ratio.
- **Why the single-file override.** The Rust reader reads only single-file archives; a directory
  archive fails at open with `UnsupportedArchive`, and the adapter would fall back to C++ on every
  task, so V3 would really measure V2. Celery compression on a file system writes directory
  archives, because `compression_task.py` passes `use_single_file_archive=enable_s3_write`. A
  patched copy of `compression_task.py` with `use_single_file_archive=True` was bind-mounted
  into the compression-worker container through a compose override file; no repository file was
  changed. C++ `clp-s s <dir> --archive-id <id>` detects a single-file archive by itself, so one
  archive set serves V1, V2 and V3.
- **Pilot gate.** Before the full run, two 256 MiB input files were compressed into a pilot
  dataset (2 single-file archives). On them, C++ `clp-s s ... file` and `clp-s-rs search
  --output file` wrote byte-identical msgpack output for `*` (946,181 and 934,432 records) and
  for `*Exception*`, with no zero timestamp. The pilot dataset was then deleted.

### 4.3 Package configuration and slots

- **Config:** telemetry off; `package: {storage_engine: clp-s, scheduler: celery | spider}`;
  `spider.worker.replicas: 8`; `compression_coordinator: {}` and `query_coordinator: {}`; file
  system input and archive output; everything else default, including 16 archives per Celery
  sub-job (38 sub-jobs for 607 archives).
- **8 search slots per side:**
  - Celery: the query-worker's concurrency is `max(1, cpu_count() // 2)` = 16 on this host, with
    no config key. `CLP_QUERY_WORKER_CONCURRENCY=8` is set in `.env`, and the query-worker
    command line was checked to show `--concurrency 8`.
  - Spider: 8 `spider-worker` containers with one executor each.
- **Spider mode needs three `.env` additions:** `CLP_SPIDER_WORKER_CONTAINER_IMAGE_REF` (the
  local worker image; the default is the registry image), `SPIDER_PULL_POLICY=missing` (the
  Spider compose file defaults to `always`) and `CLP_SPIDER_SEARCH_ENGINE`. The compression
  coordinator exits when the input is not S3, so Spider mode is started with
  `--scale compression-coordinator=0`.
- **Switching the scheduler** (V1 ↔ V2/V3): stop the package, change `package.scheduler`, run
  `sbin/start-clp.sh --setup-only`, re-apply the `.env` overrides, and start with
  `docker compose` directly. Archives, the database and `query_jobs` carry over.
- **Switching the engine** (V2 ↔ V3) only recreates the workers:
  `CLP_SPIDER_SEARCH_ENGINE=rust docker compose --project-name <project> -f
  docker-compose-spider.yaml up -d --no-deps --force-recreate spider-worker`. The shell value
  overrides `.env`. It was checked to reach the `spider-task-executor` process
  (`/proc/<pid>/environ`), and all 8 workers logged the expected `search_engine` after each
  switch.

## 5. Tests

### 5.1 Tests added

| crate | test | needs | what it checks |
|---|---|---|---|
| `clp-tdl-package` | `common::tests` (1 unit test) | - | unset, `clp-s`, `rust` and invalid values of the engine variable |
| `clp-s-wire` | `stream::tests` (9 unit tests) | - | handshake golden bytes (36-character UUID strings as str8); every integer width of the result index, the timestamp and the trailer's count; string-length boundaries with the `\n` counted; invalid UTF-8; round trip and append; the acknowledgement's exact 10 bytes, and the rejection of another prefix |
| `clp-s-sinks` | `tests/network.rs` (12 tests) | - | no connection without results; one handshake, then indices contiguous from 0 across batches, a missing timestamp sent as 0, frames equal to the in-memory collector's output plus `\n`, then the trailer and EOF, byte for byte; a rerun streams again from 0 over a new connection with identical bytes; a delayed acknowledgement is awaited; a peer that closes early, closes without acknowledging or resets gives `DownstreamClosed`; a wrong count or prefix gives `Custom`; a dropped partition sends no trailer; no listener gives an I/O error; another archive's partition, and a top-k partition, fail the session |
| `clp-s-spider-adapter` | unit tests (31 in total, new ones included) | - | the network argument order; an in-process network task (one connection, the handshake, indices from 0, the trailer, no connection without results); an in-process task fails when the listener closes without acknowledging; an unsupported archive streamed by C++ with the network arguments |
| `clp-s-spider-adapter` | `tests/network.rs` (3 ignored tests) | `CLP_S` | `fs_network_streams_equal_the_clp_s_subprocess` (§5.2); `network_unsupported_archives_fall_back_to_clp_s` (the multi-file and structured-array fixtures stream through C++); `network_tasks_fail_without_an_acknowledgement` (in-process, subprocess and fallback) |
| `search-result-listener` | `tests/spider_adapter.rs` (1 ignored test) | `CLP_S_BINARY` | 6 fixture archives through a real listener: the C++ and Rust tasks give the same set of `(archive_id, timestamp, message)`; running each Rust task twice drops exactly one copy of every result; per-archive order is the same across two Rust sessions |

Mutation checks: sending nanoseconds instead of milliseconds, restarting the index at every batch,
repeating the handshake, connecting eagerly, and honouring the result limit on network output each
made the intended tests fail.

### 5.2 Comparison with the C++ network output

`fs_network_streams_equal_the_clp_s_subprocess` runs every comparable fixture query over its
archive three times against local capture listeners: twice in-process, and once through the C++
`clp-s` network output with the same session token and task index (a result limit of 3 is set to
check that network output ignores it).

- The two in-process attempts send identical bytes in every case, and none spawns `clp-s`.
- 225 cases and 4,397 frames were compared. **222 cases are byte-identical** to the C++ stream:
  the handshake and every frame, in order.
- The other **3 cases differ only in float formatting** of the JSON text (DV-01: plain floats
  print as the shortest round trip, not `%f`); their 12 differing frames match after float
  normalization. DV-01 is a registered divergence of the reader. The HDFS records hold only a
  string message and a timestamp, so it does not apply to the smoke-test data.

### 5.3 Gates and regression checks

- `cargo +nightly fmt --check` is clean in both workspaces.
- Repository workspace: `cargo +nightly clippy -p clp-tdl-package -p search-result-listener
  --all-targets --all-features -- -D warnings` is clean. `cargo test -p clp-tdl-package` passes.
- Reader workspace: clippy on `clp-s-wire`, `clp-s-sinks`, `clp-s-spider-adapter` and
  `clp-s-resources` is clean with nightly 1.98, the toolchain the reader was linted with. No
  `#[allow]` was added.
  - The tree's newer nightly (1.101) flags new lints (`missing_const_for_fn`,
    `chunks_exact_to_as_chunks`) in the untouched `clp-s-table`; they were left alone.
  - The tree's stable 1.99.1 deprecates `AtomicU64::fetch_update`, which failed `-D warnings` in
    `clp-s-resources/src/budget.rs`. It was renamed to `try_update`, which also compiles on
    stable 1.96.
- The adapter's existing tests were adapted to this branch's `clp-rust-utils` (`Package` gained
  `scheduler`; `count_by_time_bucket_size_millisecs` was removed from `ClpSQueryOption`).
- The existing results-cache end-to-end tests (MongoDB and S3 in containers) passed 6 of 7. The
  failing one needs the large dataset archives, which this tree does not have. The listener's
  existing `tests/clp_s.rs` passes.
- During the smoke test the tests were run again: `cargo test -p clp-tdl-package`, the reader
  tests of the three crates, both `tests/network.rs` tests with the `7365387b` `clp-s`, and the
  listener's `tests/spider_adapter.rs` all passed. `tests/e2e.rs` (MongoDB and S3) was not run
  again.

## 6. Smoke-test results

Date: 2026-10-19. All runs used the 607 archives and 8 search slots per variant (§4).

> These are smoke-test runs, not a benchmark. Each time is a single run, without warm-up control,
> repetition, interleaving or a load gate. Some runs had a `/proc` sampler polling every ~20 ms
> inside all 8 workers, and one V1 run overlapped a clippy build. The CLI first-result times are
> dominated by about 0.9 s of `docker compose run` and `docker run` start-up.

### 6.1 Digests

Each result is normalized from the client's output: the non-raw CLI prints
`<archive_id> <timestamp>: <message>`, the SSE stream carries `{archive_id, timestamp, message}`
as JSON, and `--raw` prints only the message. The trailing `\n` of the message is removed. Then:

- `D_msg` is the sha256 of the sorted messages, each followed by `\n`;
- `D_ts` is the sha256 of the sorted `<timestamp> <message>` lines;
- `D_full` is the sha256 of the sorted `<archive_id> <timestamp>: <message>` lines.

A `--raw` run can only give `D_msg`. `D_full` depends on the archive UUIDs, so it is comparable
only within this compression. `D_msg` of `*Transmitted*` equals that of the earlier
streaming-search reports (a different compression of the same data).

| query | results | archives with results | `D_msg` | `D_ts` | `D_full` |
|---|---|---|---|---|---|
| `*Transmitted*` | 1,071 | 92 | `d66c5a58…` | `f36dc776…` | `bd64996c…` |
| `*Exception*` | 107,867 | 606 | `40609df5…` | `2c04969d…` | `171a7d1f…` |
| `*ProtobufRpcEngine*` | 3,221,319 | 607 | `1aa33886…` | `5736e77d…` | `7735871f…` |

```text
*Transmitted*        D_msg  d66c5a58c82db9fef0019fe494c4936c670ac1c8daa630880bb5e7c5d04de12d
                     D_ts   f36dc776a76eb5136684ecf87231962b5cdaf9f02afcf3a5131dfca5cf094bfa
                     D_full bd64996cbd6d77608e49d2ace025f43333cafcb5ba0668143c2edeae6f5e2f69
*Exception*          D_msg  40609df5d593e48dea1bb23acf7cb3d4fe0029f814a2254ecd7d92f37bb341bb
                     D_ts   2c04969d61ba1556eeadd1fd9747f480fd7a3d179ce80b4eaf118e753ddcd2c6
                     D_full 171a7d1fb0c90a6b5845ee6321429cbaf41af2f9299735fceb986aa021cba41c
*ProtobufRpcEngine*  D_msg  1aa3388687e5197a06abc815c22d2bbb9d6e95347394bae16ed885ee1d9fb2c9
                     D_ts   5736e77d61f6112e86c88523d4a3ccaf97b9e737b38ecd704619cf8ac43ab815
                     D_full 7735871f230b661005fad52dc747d679ee8230a7d0c5850eb463859564da141d
```

### 6.2 Correctness on 607 archives

#### `*Transmitted*`

| variant | client | results | digests | first result | total |
|---|---|---|---|---|---|
| V1 Celery | `sbin/search.sh --raw` | 1,071 | `D_msg` = reference | 1.32 s | 9.80 s |
| V1 Celery | `sbin/search.sh` | 1,071 | all = reference | 1.69 s | 10.92 s (during a clippy build) |
| V2 Spider + C++ | `sbin/streaming-search.sh --raw` | 1,071 | `D_msg` = reference | 1.01 s | 3.12 s |
| V2 Spider + C++ | `sbin/streaming-search.sh` | 1,071 | all = reference | 0.91 s | 3.20 s |
| V2 Spider + C++ | `POST /query/stream` | 1,071 | all = reference | 179 ms | 2.14 s |
| V3 Spider + Rust | `sbin/streaming-search.sh --raw` | 1,071 | `D_msg` = reference | 0.93 s | 1.97 s (first query after the worker recreate, sampler on) |
| V3 Spider + Rust | `sbin/streaming-search.sh` | 1,071 | all = reference | 0.90 s | 1.87 s |
| V3 Spider + Rust | `POST /query/stream` | 1,071 | all = reference | 119 ms | 0.82 s |

#### `*Exception*`

| variant | client | results | digests | first result | total |
|---|---|---|---|---|---|
| V1 Celery | `sbin/search.sh --raw` | 107,867 | `D_msg` = reference | 1.22 s | 17.30 s |
| V1 Celery | `sbin/search.sh` | 107,867 | all = reference | 1.18 s | 17.35 s |
| V2 Spider + C++ | `sbin/streaming-search.sh --raw` | 107,867 | `D_msg` = reference | 0.92 s | 10.70 s (sampler on) |
| V2 Spider + C++ | `sbin/streaming-search.sh` | 107,867 | all = reference | 0.93 s | 10.52 s |
| V2 Spider + C++ | `POST /query/stream` | 107,867 | all = reference | 126 ms | 9.03 s |
| V3 Spider + Rust | `sbin/streaming-search.sh --raw` | 107,867 | `D_msg` = reference | 0.91 s | 4.81 s (sampler on) |
| V3 Spider + Rust | `sbin/streaming-search.sh` | 107,867 | all = reference | 0.87 s | 4.64 s |
| V3 Spider + Rust | `POST /query/stream` | 107,867 | all = reference | 109 ms | 3.55 s |

#### `*ProtobufRpcEngine*` (V2 and V3 only)

| variant | client | results | digests | first result | total |
|---|---|---|---|---|---|
| V2 Spider + C++ | `sbin/streaming-search.sh` | 3,221,319 | all = reference | 0.95 s | 12.84 s |
| V3 Spider + Rust | `sbin/streaming-search.sh` | 3,221,319 | all = reference | 0.83 s | 6.03 s |

Further checks:

- **Per-archive order, V2 vs V3:** the sequence of results of each archive is identical on 606 of
  606 archives for `*Exception*` and 607 of 607 for `*ProtobufRpcEngine*`.
- **Jobs and logs:** every query job of the smoke test (ids 15-44) is SUCCEEDED with `num_tasks`
  607. Every SSE `end` event was `Succeeded` with 0 duplicates and 0 protocol errors. The
  spider-scheduler, query-coordinator and api-server logs have no WARN or ERROR line, and the
  CLI's `--verbose` listener logs show no `IndexGap`, UTF-8 or handshake error. The workers
  logged "Task completed successfully" for every task started, apart from the deliberately killed
  attempts (§6.4).
- **Zero results** (`*sbenchnomatchzz*`, V2 and V3): the CLI exits 0 with no result and no
  listener connection, and the SSE `end` is `Succeeded` with 0 results.
- **Case-insensitive** (`*transmitted*` with `ignore_case: true` over the API, V2 and V3): 1,071
  results with the reference digests.

The earlier V2 runs on a worker image built from `7365387b` alone (before this integration) gave
the same counts and digests, with totals of 3.24-3.53 s (CLI) and 2.13-2.14 s warm (SSE) for
`*Transmitted*`, and 11.34 s (CLI) and 10.04 s (SSE) for `*Exception*`.

### 6.3 V3 runs no `clp-s`

- **Process sampler.** A loop inside each of the 8 workers listed `/proc` about every 20 ms. It
  saw **0** `clp-s` processes during the V3 `*Transmitted*` and `*Exception*` runs (343 and 554
  sweeps per worker). During the V2 `*Exception*` run, the same sampler saw 606 distinct `clp-s`
  PIDs.
- **Task logs.** Since the switch to V3, all 3,642 `clp-s query task completed successfully` lines
  (6 searches × 607 tasks) have `is_clp_s_subprocess_used: false`. There are 0 lines with `true`,
  0 `panicked` and 0 `UnsupportedArchive`.
- **Engine log.** All 8 workers logged `search_engine: Rust` after the switch, and `ClpS` after
  switching back.

### 6.4 Worker kills (V3)

| query and client | fault | outcome |
|---|---|---|
| `*Exception*`, `POST /query/stream` | worker 3 killed 1.5 s into the query, then restarted | `Succeeded`, 107,867 results with the reference digests. The killed attempt (task 259) had sent nothing, so 0 duplicates were dropped. |
| `*ProtobufRpcEngine*`, `sbin/streaming-search.sh` | workers 2 and 5 killed 3.5 s into the query, mid-stream | `Succeeded`, 3,221,319 results, **1,597 duplicates dropped**, 0 protocol errors. The listener logged "connection closed in the middle of a frame" for task indices 305 and 308. All digests and the per-archive order equal those of the run without kills. |

The second run shows that a retried V3 task restarts its indices at 0 and that the listener drops
what the killed attempt had already delivered. In both runs, Spider re-dispatched the killed tasks
only about 70-80 s later, after logging that "the number of living task instances has reached the
upper limit" (Spider's reaping of dead instances; the earlier C++ prototype report saw the same
83 s delay).

## 7. Review status

A code review of the change found four items:

1. **An empty batch would connect.** `NetworkPartial::consume` connected and sent the handshake
   even for a batch with no records. Fix: return early on an empty batch, so only a batch with a
   result can connect.
2. **One archive partition per sink.** The sink assumed it receives exactly the destination
   archive's partition. Fix: `open_partial` rejects any other partition, including another
   archive and the top-k partition, with `SinkError::Custom`; the destination's archive id is now
   an `ArchiveId`. Two sink tests cover it (`other_archives_fail_the_session`,
   `top_k_partitions_are_rejected`).
3. **Naming.** The loop over a batch now iterates `(record_index, timestamp_ns)` instead of
   indexing with `record`, and the missing-text error names the record
   ("rendered text for record N").
4. **Visibility.** The `clp-s-wire` encode helpers (`write_message`, `write_str`,
   `write_str_header`, `msgpack_error`) are `pub` inside a private module; they should be
   `pub(crate)`.

Items 1-3 are applied, and the tests and lint gates pass on them; the C++ comparison has not been
rerun. Item 4 is not applied: `pub(crate)` inside a private module fails clippy's denied
`redundant_pub_crate` (nursery), and `pub` there is already visible only inside the crate. The
smoke-tested worker image (`libclp_tdl_package.so` `ecbacb0e…`) was built before the review fixes.

None of them changes behaviour on the measured path: the scan skips windows without a match, so
it hands the sink no empty batch; an adapter session holds exactly the task's archive partition
(network output never uses top-k); and items 3 and 4 do not change what the code does.

## 8. Known gaps and next steps

- **The three-variant benchmark has not run.** The smoke-test timings are single runs. The planned
  method: per variant, query (`*Transmitted*`, `*Exception*`) and client, 2 discarded warm-ups
  after each (re)start or worker recreate, 10 measured runs in blocks ordered V1, V2, V3, V3, V2,
  V1 with queries and clients interleaved, a 2 s gap and a CPU load gate. It will report the time
  to the first result, the time to the last result and the time to exit separately, plus
  job-relative times from `query_jobs.creation_time` with clock-step detection, and will reject
  any run with a wrong count or digest, a failed job or a V3 fallback.
- **The Presto comparison is in progress.**
- **Batching and first-result latency.** V3 hands over results per batch (up to 4,096 rows or
  256 KiB of rendered JSON), while C++ sends each result as it is found. The effect on the time to
  the first result has not been measured separately; `batch_num_rows` is the knob.
- **Memory.** The reader's buffer pool can keep up to about 1 GiB of decode buffers per executor
  across tasks (a bound from the code, not a measurement), i.e. up to about 8 GiB for 8 workers.
  The smoke test did not record worker RSS.
- **Not covered by this smoke test:**
  - S3 archives. For S3 storage, V3's handshake carries the task's archive UUID; whether C++
    sends the same string there was not checked. The benchmark uses the file system.
  - A byte-level wire diff on an HDFS archive. The fixture comparison (§5.2) and the per-archive
    order equality with V2 on all 607 archives cover it indirectly.
  - The encoder's truncate-on-error path, which needs a string over 4 GiB.
  - The reader's `tests/e2e.rs` (MongoDB and S3) after the change.
- **The change is uncommitted**, and the review fixes and protocol v2 still need a new worker image
  (§7).
- **Pre-existing issues seen** (not caused by this change):
  - Spider jobs leave `query_jobs.num_tasks_completed` at 0 and `duration` NULL, although their
    status is SUCCEEDED. Benchmarks must time from the client or from `creation_time`.
  - `sbin/start-clp.sh --setup-only` rewrites `.env`, so the Celery `CLP_QUERY_WORKER_CONCURRENCY`
    override (and the Spider image, pull-policy and engine overrides) is lost and must be
    re-applied after every setup. Celery query concurrency has no config key.
  - In Spider mode with file-system input, the compression coordinator exits, so `start-clp.sh`
    fails unless the coordinator is scaled to 0.
  - After a worker is killed, Spider re-dispatches its tasks only after about 70-80 s. Workers
    must never be killed inside timed runs.
  - The first V3 query after a worker recreate pays for `package_init` and the creation of the
    reader context (1.97 s total for `*Transmitted*`, still faster than V2), so each mode needs
    a warm-up.

## 9. Reproduction notes

All package commands run from the package directory (`CLP_HOME`, a copy of the `7365387b` build's
`build/clp-package` kept outside any `build/` directory, since `task package` starts with
`rm -rf build/clp-package`).

**Worker image.** In this tree:

```bash
cargo build --release -p clp-tdl-package        # build/rust-targets/release/libclp_tdl_package.so
mkdir -p build/core
cp <base build>/build/core/{clp-s,indexer,log-converter} build/core/
tools/docker-images/clp-spider-worker/build.sh  # or docker build from a minimal context
cp tools/deployment/package/compose.clp-spider.yaml <CLP_HOME>/
```

`build.sh` removes the previous image whose ID is in `build/clp-spider-worker-image.id`, so
recreate the workers with the new tag after every rebuild.

**Compression with single-file archives** (Celery mode):

```bash
sed 's/use_single_file_archive=enable_s3_write,/use_single_file_archive=True,/' \
  components/job-orchestration/job_orchestration/executor/compress/compression_task.py \
  > <overrides>/compression_task.py
# compress-sfa.yaml mounts it read-only into the compression-worker at
# /opt/clp/lib/python3/site-packages/job_orchestration/executor/compress/compression_task.py
sbin/compress.sh --config etc/clp-config.yaml --unstructured <input dir>
find <archives>/default -mindepth 1 -maxdepth 1 -type d | wc -l   # must print 0
```

**Celery mode (V1):**

```bash
sbin/start-clp.sh --config etc/clp-config.yaml --setup-only
sed -i 's/^CLP_QUERY_WORKER_CONCURRENCY=.*/CLP_QUERY_WORKER_CONCURRENCY=8/' .env
docker compose --project-name <project> -f docker-compose.yaml -f <overrides>/compress-sfa.yaml \
  up -d --wait
```

**Spider mode (V2, then V3):**

```bash
sbin/start-clp.sh --config etc/clp-config.yaml --setup-only
printf 'CLP_SPIDER_WORKER_CONTAINER_IMAGE_REF=%s\nSPIDER_PULL_POLICY=missing\nCLP_SPIDER_SEARCH_ENGINE=clp-s\n' \
  "<worker image>" >> .env
docker compose --project-name <project> -f docker-compose-spider.yaml up -d --wait \
  --scale compression-coordinator=0
CLP_SPIDER_SEARCH_ENGINE=rust docker compose --project-name <project> \
  -f docker-compose-spider.yaml up -d --no-deps --force-recreate spider-worker
docker logs <project>-spider-worker-1 2>&1 | grep 'clp TDL package initialized'
```

**Queries:**

```bash
sbin/search.sh --raw '*Transmitted*'                     # V1
sbin/streaming-search.sh --raw '*Transmitted*'           # V2, V3
curl -sS -N -H 'Content-Type: application/json' \
  -d '{"query_string":"*Transmitted*"}' http://127.0.0.1:3001/query/stream
```

- The first-result and total times came from a small client-side timer started just before the
  client process or HTTP request, using the monotonic clock. The digests follow §6.1; for the raw
  output, `D_msg` equals `LC_ALL=C sort <output> | sha256sum`.
- The no-`clp-s` check ran a shell loop inside each worker that listed `/proc/*/comm` about every
  20 ms, and counted the `is_clp_s_subprocess_used` values in the worker logs.
- V1 exits 0 even when its job fails, so check `query_jobs.status` for every V1 run.

**Tests:**

```bash
# Repository workspace
cargo test -p clp-tdl-package
CLP_S_BINARY=<clp-s> cargo test -p search-result-listener --test spider_adapter -- --ignored
# Reader workspace (components/core/rust)
cargo test -p clp-s-wire -p clp-s-sinks -p clp-s-spider-adapter
CLP_S=<clp-s> cargo test -p clp-s-spider-adapter --test network -- --ignored
```
