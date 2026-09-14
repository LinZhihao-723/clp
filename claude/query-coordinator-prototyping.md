# Query coordinator end-to-end prototype

This branch holds an end-to-end prototype of the Spider-driven query coordinator. In a Docker Compose
deployment of the CLP package with `package.scheduler: spider`, plain clp-s searches are handled by
the Rust `query-coordinator` service, which runs one `query::clp_s_search` Spider task per selected
archive. Search results go straight to the MongoDB results cache.

The branch contains:

- The CLP component changes that make the query coordinator runnable.
- Docker Compose and package support for the query-coordinator service.
- A test/benchmark harness that submits search jobs directly to the database and reads results from
  the results cache. It works against both the Celery-driven and the Spider-driven deployments.
- Demo scripts that run a single search on either engine.

## Branch composition

The branch is based on:

- [Bill-hbrhbr/clp#4](https://github.com/Bill-hbrhbr/clp/pull/4) (`query-coordinator/coordinator-loop`).
  It stacks the query task contract, the coordinator crate and submitter, the job handle, graph
  submission, and the clp-s search TDL task.
- [y-scope/clp#2491](https://github.com/y-scope/clp/pull/2491), which selects and configures Spider
  as the scheduler through `clp-config.yaml`. It is merged into the base without conflicts.

The prototype commits on top are described below. The design target is the query coordinator design
in [Bill-hbrhbr/personal-yscope-docs](https://github.com/Bill-hbrhbr/personal-yscope-docs/tree/main/query-coordinator):
the MVP design, the planning design, the job-handler RFC and the TDL RFC.

## Scope

- **Plain search only.** No aggregation, no top-N early termination, no user-requested cancellation.
- **Single owner of search jobs.** In Spider mode, the legacy query stack (query-scheduler,
  query-worker, reducer) is not deployed, so there is no arbitration between the two schedulers.
- **No CLI or web UI integration.** `sbin/search.sh` still expects results over a TCP socket. The
  harness replaces it for testing and benchmarking. The web UI works for plain searches, but its
  timeline (aggregation) jobs are not claimed in Spider mode.
- **Filesystem input and output.** Logs input and archive output use the filesystem. Compression
  keeps using the Celery compression scheduler and worker; the Spider compression coordinator is not
  deployed.
- **Result limit.** `max_num_results` applies per archive through clp-s's `--max-num-results`. A
  persisted `0` omits the flag, so clp-s's default of 1000 applies.

## Code changes

### `clp-rust-utils`

- **`clp_config/package/config.rs`:**
  - `Config.query_coordinator: Option<QueryCoordinator>`.
  - `QueryCoordinator` now mirrors the Python model. It adds `max_datasets_per_query`,
    `database_connection_pool_size`, `termination_timeout_secs`, `search_task_max_num_instances`,
    `search_task_max_retry`, and the soft and hard search-task timeouts.
  - `ArchiveOutput.retention_period: Option<NonZeroU32>` (minutes).
  - `ResultsCache::uri()`, which mirrors Python's `ResultsCache.get_uri()`.
- **`database/mysql.rs`:** marks an uncompilable pre-existing doctest as `ignore` so that
  `cargo test` passes. This is unrelated to the prototype.

### `query-coordinator`

- **`job_handle.rs`:**
  - `prepare_task_inputs` (previously `todo!()`):
    - Validates the begin/end timestamp order.
    - Validates the requested datasets: the list must be non-empty, every dataset must exist, and
      the count must not exceed `max_datasets_per_query`. Duplicates are removed.
    - Selects archives per dataset by time-range overlap and retention cutoff, using the same
      filters as the legacy `get_archives_for_search`.
    - Orders the archives newest first and pairs each one with the configured `ExecutionPolicy`.
  - **Zero selected archives:** a guarded `PENDING → SUCCEEDED` update with `num_tasks = 0` and
    `duration = 0`. No Spider job is created.
  - **Results-cache preparation:** before graph registration, it creates the job's collection and
    the legacy `timestamp-descending` index. A failure here fails the job before it is `RUNNING`.
  - `PlanningOption`: validated planning config. The hard timeout must be greater than the soft
    timeout and no more than Spider's 24 h maximum.
  - `JobHandleContext`: coordinator-wide state shared by every handle.
  - Maps `QueryJobOutcome::UnexpectedlyCancelled` to `FAILED`.
- **`coordination.rs`:** builds the `PlanningOption`, the MongoDB client and the results-cache
  `OutputHandle` from config, and shares them with handles through `JobHandleContext`.
- **`query_job_submitter/`:**
  - Adds `QueryJobOutcome::UnexpectedlyCancelled`, and maps Spider's `Cancelled` state to it
    (previously `todo!()`).
  - Makes `run_query_job_to_completion` idempotent for recovery. It reads the job state first and
    only calls `start_job` on a `Ready` job. Previously, recovering a job that had already started
    failed on `start_job` and left the row `RUNNING` forever.
- **`error.rs`:** adds `Error::Mongo` and removes the dead `NoArchivesToSearch`.
- **`bin/query_coordinator.rs` (new):** the `query-coordinator` executable, mirroring the compression
  coordinator:
  - Loads the config and `CLP_DB_USER` / `CLP_DB_PASS`.
  - Creates the MySQL pool and runs the coordinator.
  - Stops on SIGTERM or ctrl-c, waiting up to `termination_timeout_secs`.
- **`Cargo.toml`:** adds the `[lib]` and `[[bin]]` targets and the `anyhow`, `clap`, `mongodb` and
  `secrecy` dependencies, plus the tokio `macros` and `signal` features.

### Python config and package controller

- **`clp_py_utils/clp_config.py`:**
  - A `QueryCoordinator` model with the same keys and defaults as the Rust struct, validated so the
    hard timeout is greater than the soft timeout.
  - `ClpConfig.query_coordinator`.
  - `SpiderWorker.container_image_ref`.
  - Validators:
    - Spider mode requires `query_coordinator` and requires `compression_coordinator` to be `null`.
      This is prototype-only: it follows from not deploying the compression coordinator.
    - `query_coordinator` requires clp-s and a `spider` section.
- **`clp_package_utils/controller.py`:**
  - `_set_up_env_for_query_coordinator`, which writes `CLP_QUERY_COORDINATOR_LOGGING_LEVEL`.
  - When `spider.worker.container_image_ref` is set, it writes
    `CLP_SPIDER_WORKER_CONTAINER_IMAGE_REF` and `SPIDER_PULL_POLICY=missing`, so a locally built
    spider-worker image can be used.
- **`package-template/src/etc/clp-config.template.*.yaml`:** documented `query_coordinator` and
  `spider.worker.container_image_ref` examples.

### Docker Compose and image

- **`docker-compose-all.yaml`:** a new `query-coordinator` service.
  - Runs from the clp-package image with DB credentials and `RUST_LOG`, and a read-only config
    mount.
  - Depends on `db-table-creator`, `results-cache-indices-creator` and `spider-storage`.
  - Command: `/opt/clp/bin/query-coordinator --config /etc/clp-config.yaml`.
- **`docker-compose-spider.yaml`:** adds `query-coordinator`, and drops `query-scheduler`,
  `query-worker`, `reducer` and `compression-coordinator`.
- **`compose.clp-spider.yaml`:** spider-worker gets a read-only mount of the archive output directory
  at `/var/data/archives`, plus `extra_hosts` for the database and results cache.
- **`tools/docker-images/clp-package/Dockerfile`:** ships `bin/query-coordinator`.

### Test harness: `tools/scripts/query-harness/`

`query_harness.py` is a standalone script (PEP 723 dependencies, run with `uv run`) that talks to the
package's published MySQL and MongoDB ports. It inserts `SEARCH_OR_AGGREGATION` jobs with a msgpack
`SearchJobConfig` (datasets `["default"]`, no `network_address`, `max_num_results` 1000). It then
polls the job to a terminal status and reads collection `<job_id>`. The same commands work in Celery
and Spider mode.

- `submit`: runs one job. Options: `--expect-status`, `--no-wait`, `--dump-results`, and
  `--malformed`, `--raw-job-config-hex` and `--count` for negative tests.
- `wait`: observes an existing job.
- `bench`: runs a query list with repetitions, warmup and concurrency. It writes JSONL/CSV records
  and a mean/p50/p90/max summary.
- `compare`: diffs two result sets by `(dataset, archive_id, log_event_ix)`.
- `cleanup`: drops result collections and optionally deletes job rows.

The README covers benchmark fairness: the legacy top-N cut, parallelism, polling latency, result
retention, and using a locally built spider-worker image.

### Demo: `tools/scripts/query-coordinator-demo/`

`spider-search.sh '<query>'` and `celery-search.sh '<query>'` each run one search through the harness
with 16 workers, in the default dataset. Each prints the results newest first and the query time,
measured from job insert until the terminal status is observed.

The scripts put the package into the right mode first, and skip that step when it already is:

- **Spider:** `package.scheduler: spider`, 16 spider-worker replicas, dispatch queue capacity 64,
  storage poll timeout 5 ms, and coordinator result polling at 10 ms.
- **Celery:** `package.scheduler: celery`, `num_archives_to_search_per_sub_job: 1024` (no top-N
  cut), and the query-worker recreated with `CLP_QUERY_WORKER_CONCURRENCY=16`. This is a runtime
  override, because the controller always uses `cpu_count // 2`.

Setup time is reported separately from query time. The README has the options (`-n`, `-i`) and the
environment variables.

## Configuration

```yaml
package:
  storage_engine: "clp-s"
  scheduler: "spider"            # or "celery"
spider:
  scheduler:
    round_robin:
      storage_poll_timeout_ms: 5
      dispatch_queue_capacity: 64
  worker:
    container_image_ref: "clp-spider-worker:dev-<user>-<id>"   # from `task docker-images:spider-worker`
    replicas: 16
query_coordinator:                # defaults shown; `{}` is enough
  logging_level: "INFO"
  resource_group: {name: "query-coordinator"}
  job_polling_interval_millisecs: 100
  max_concurrent_jobs: 1000
  max_datasets_per_query: 10
  result_polling: {init_backoff_millisecs: 10, max_backoff_millisecs: 10}   # default is 100/1000
  database_connection_pool_size: 10
  termination_timeout_secs: 30
  search_task_max_num_instances: 2
  search_task_max_retry: 1
  search_task_soft_timeout_secs: 600
  search_task_hard_timeout_secs: 1200
compression_coordinator: null
```

## How to run

1. Build the package with `task package`, and the spider-worker image with
   `task docker-images:spider-worker`. Put the resulting tag in
   `spider.worker.container_image_ref`.
2. Start from a fresh `var/data`. The new `query_jobs` columns exist only in
   `CREATE TABLE IF NOT EXISTS`, and there is no migration.
3. Run `sbin/start-clp.sh`, then ingest with `sbin/compress.sh` (Celery compression).
4. Search with `tools/scripts/query-coordinator-demo/{spider,celery}-search.sh '<query>'`, or with the
   harness directly.

## Validation

**End-to-end acceptance**, all driven through the harness, passed from the worktree and again from
this branch's checkout:

- Nonempty search, including time-bounded searches: exact against ground truth.
- No matches.
- A time range that selects no archives: `SUCCEEDED`, `num_tasks = 0`, no Spider job.
- Nonexistent dataset, and malformed job config: `FAILED` with a useful message.
- Aggregation job: left unclaimed.
- Coordinator restart while a job is `RUNNING` (SIGTERM, SIGKILL, and down while the job finishes):
  recovered with the same Spider job and identical results.
- A bench smoke run.
- Celery-mode parity: identical result sets.

**Benchmark.** Query `*NonDFS*` over 607 archives, from 211 GB of unstructured HDFS logs. Each run
is 1 warmup plus 10 sequential runs, with the harness polling job status every 10 ms. Every run
returned the same 16 results.

| Engine | Workers | Mean | p50 | p90 |
|---|---|---|---|---|
| Celery | 8 | 2.383 s | 2.342 s | 2.506 s |
| Spider | 8 | 1.000 s | 0.994 s | 1.051 s |
| Celery | 16 | 1.939 s | 1.889 s | 2.168 s |
| Spider | 16 | 0.761 s | 0.731 s | 0.877 s |

Spider's dispatch queue capacity was 32 at 8 workers and 64 at 16 workers.

## Known gaps and follow-ups

- **Resource-group get-or-create is not robust to a CLP database reset.** If the Spider database
  still has the group but the CLP database's mapping table is fresh, the coordinator crash-loops with
  "resource group already exists" (`coordination.rs`, `get_or_create_resource_group_id`). The
  workaround is to rename `query_coordinator.resource_group.name`.
- **Prototype-only validator.** Spider mode forbids `compression_coordinator`, which is stricter
  than #2491, where an all-S3 deployment could use it. This must be reconciled before upstreaming.
- **`datasets: None`** means the default dataset, or 0 tasks if the default dataset doesn't exist.
  The legacy scheduler treated it as clp-text.
- **Behaviour inherited from the base branch:**
  - The unguarded `Coordinator::mark_job_failed` leaves `duration` NULL.
  - A recovered job's `duration` includes the coordinator's downtime.
  - SIGTERM doesn't drain running jobs; they are recovered on the next start.
- **Spider mode bookkeeping:** no `query_tasks` rows are written, and `num_tasks_completed` stays 0.
- **Not implemented:** results-cache deduplication for retried tasks (#2509), cancellation
  (MVP+1), and aggregation (MVP+2).
- **Deployment:** no schema migration, the Helm chart is not updated, and the package build does
  not build or tag the spider-worker image.
