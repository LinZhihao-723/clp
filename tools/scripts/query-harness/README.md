# Query harness

A host-side script that drives CLP search jobs without `sbin/search.sh`. It inserts
`SEARCH_OR_AGGREGATION` jobs directly into the `query_jobs` table, polls them to a terminal status,
and reads their results from the results cache collection named after the job ID. It works
unchanged against Celery-mode (`package.scheduler: "celery"`) and Spider-mode
(`package.scheduler: "spider"`) deployments, except for the Spider-mode-only acceptance options
`--malformed`, `--raw-job-config-hex` and `--count` (see [`submit`](#submit)).

## Requirements

* [uv](https://docs.astral.sh/uv/); dependencies are declared inline (PEP 723).
* A running CLP package with the bundled database and results cache. Both publish their ports on
  the host at the IP of `database.host` / `results_cache.host` (default `localhost` →
  `127.0.0.1`) and `database.port` / `results_cache.port` (default `3306` / `27017`).

## Usage

```shell
cd tools/scripts/query-harness
uv run query_harness.py <command> --package-dir /path/to/clp-package [options]
```

The harness reads `<package-dir>/etc/clp-config.yaml` and the credentials file it references
(default `etc/credentials.yaml`). `--package-dir` defaults to `$CLP_HOME`, or
`<repo>/build/clp-package`. Every endpoint and credential can be overridden (`--db-host`,
`--db-port`, `--db-name`, `--db-user`, `--db-password`, `--results-cache-host`,
`--results-cache-port`, `--results-cache-db`). Records are labelled with `package.scheduler` unless
`--label` is given.

Jobs are submitted with `datasets = ["default"]` (override with `--dataset`, repeatable), no
`network_address`, no aggregation, and `max_num_results = 1000`. clp-s rejects
`--max-num-results 0` when writing to the results cache, so the legacy path needs a non-zero value.
`datasets = null` isn't engine-neutral (the legacy scheduler treats it as CLP Text, while the query
coordinator searches `default`), so it's only reachable through `--raw-job-config-hex` (which also
needs `--force-raw-job-config` in Celery mode).

### `submit`

```shell
uv run query_harness.py submit 'level: ERROR' --show-results 5
uv run query_harness.py submit 'x' --dataset nonexistent --expect-status FAILED
uv run query_harness.py submit 'x' --begin-time 0 --end-time 1
uv run query_harness.py submit 'x' --no-wait
```

Prints the job's record (see below). `--dump-results FILE` writes all results as JSONL. The exit
code is 0 only if the last observed status matches `--expect-status` (default `SUCCEEDED`).

The following acceptance scenarios (5 and 6) only behave as shown in Spider mode:

```shell
uv run query_harness.py submit 'x' --malformed --expect-status FAILED
uv run query_harness.py submit 'x' --count --timeout 15 --expect-status PENDING
```

`--malformed` submits an undecodable `job_config`, and `--raw-job-config-hex HEX` submits arbitrary
`job_config` bytes. In Celery mode, the legacy query scheduler doesn't guard `job_config` decoding:
one undecodable `job_config` stops it for good (it exits with code 0, so Docker Compose doesn't
restart it), and every later job stays `PENDING`. The harness therefore refuses both options unless
`package.scheduler` is `"spider"` or `--force-raw-job-config` is given. An undecodable job left in
`query_jobs` (e.g. a Spider-mode `--malformed --no-wait` job the coordinator hasn't failed yet) stops
the legacy query scheduler the same way, so remove it with
`cleanup --job-id <job-id> --delete-jobs --force` before starting Celery mode. In Celery mode, a
`--count` job runs through the reducer instead of staying `PENDING`.

A job that doesn't reach a terminal status stays in `query_jobs`: the `--count` job above (the
query coordinator never claims aggregation jobs), a `--no-wait` job, or any job that hits
`--timeout`. The legacy query scheduler claims every pending search job when it starts, so remove
such jobs before switching to Celery mode. Wait for a running job to finish (`wait <job-id>`), then
run `cleanup --job-id <job-id> --delete-jobs`; for a job that stays `PENDING`, add `--force`. The
harness logs the IDs of the jobs it leaves unfinished.

### `wait`

```shell
uv run query_harness.py wait <job-id> --timeout 120
```

Polls an existing job (e.g. one submitted with `--no-wait` before restarting the coordinator).
`wall_clock_secs` is left empty because the insert time is unknown.

### `bench`

```shell
uv run query_harness.py bench --queries-file queries.txt --repetitions 10 --warmup 1 \
  --concurrency 4 --records-jsonl out/spider.jsonl --records-csv out/spider.csv \
  --summary-json out/spider-summary.json --drop-results
```

Runs every query (from `--query` and/or `--queries-file`, one per line, `#` comments allowed)
`--repetitions` times after `--warmup` rounds, keeping `--concurrency` jobs in flight. It prints
mean/p50/p90/max of each metric over `SUCCEEDED` jobs, overall and per query. Warmup jobs are
written to `--records-jsonl` / `--records-csv` with `warmup = true` (so `cleanup --records` covers
them), but are excluded from the summary and the exit code. Use `--dump-dir DIR` to keep every job's
results for `compare`.

Per-job record fields:

| Field | Meaning |
| --- | --- |
| `label` | Engine label. |
| `query`, `repetition`, `job_id` | Job identity. |
| `status`, `status_msg`, `timed_out` | Last observed status, or `ERROR` with the exception if the harness failed to insert or observe the job. |
| `warmup` | Whether the job belongs to a `--warmup` round. |
| `wall_clock_secs` | Harness time from the `INSERT` to observing a terminal status. |
| `result_fetch_secs`, `num_results` | Time to read the whole results collection, and its size. |
| `db_creation_time`, `db_start_time`, `db_duration_secs` | `query_jobs` timing columns. |
| `num_tasks`, `num_tasks_completed` | `query_jobs` task counters. |
| `spider_id` | `query_jobs.spider_id`, if the column exists. |

Status polling adds up to `--poll-interval` (default 0.05 s) to `wall_clock_secs`.

The `db_*` timestamps are naive and come from different clocks. `creation_time` is always the
database clock. `start_time` and `duration` come from the query scheduler container's clock in
Celery mode, but from the database clock in Spider mode. Use `wall_clock_secs` (measured by the
harness) for comparisons across modes.

### `compare`

```shell
uv run query_harness.py compare 12 34
uv run query_harness.py compare out/celery-12.jsonl out/spider-34.jsonl
```

Each side is a job ID in the current results cache or a JSONL file from `--dump-results` /
`--dump-dir`. Results are keyed by `(dataset, archive_id, log_event_ix)`. The report lists
keys only on one side, keys whose documents differ, and duplicate keys. The exit code is 0 only if
the sets are identical.

### `cleanup`

```shell
uv run query_harness.py cleanup --records out/spider.jsonl --delete-jobs
uv run query_harness.py cleanup --all-results
```

Drops the result collections of the given jobs (`--job-id`, `--records`) or of every job
(`--all-results`). `--delete-jobs` also deletes the given jobs' `query_tasks` and `query_jobs` rows.
Given jobs that haven't reached a terminal status are skipped unless `--force` is set.
`--all-results` drops every numeric results collection regardless of status, including the results
of non-harness jobs (e.g. web UI searches), and leaves their results-metadata documents behind.

## Using a locally built spider-worker image

`task docker-images:spider-worker` tags the image as `clp-spider-worker:dev-<user>-<id>` (see
`docker images clp-spider-worker`). Set it in `etc/clp-config.yaml`:

```yaml
spider:
  worker:
    container_image_ref: "clp-spider-worker:dev-<user>-<id>"
```

The controller then writes `CLP_SPIDER_WORKER_CONTAINER_IMAGE_REF` and `SPIDER_PULL_POLICY=missing`
to `.env`, so Docker Compose doesn't try to pull the local tag.

## Benchmark fairness

* **Top-N early termination.** The legacy query scheduler searches archives newest-first in batches
  of `query_scheduler.num_archives_to_search_per_sub_job` (default 16). After each batch, it stops
  once it has `max_num_results` results newer than every remaining archive. The query coordinator
  searches every selected archive. For like-for-like runs, set
  `num_archives_to_search_per_sub_job` to at least the number of archives.
* **Per-archive cap.** Both engines pass `max_num_results` to clp-s, which keeps the latest
  `max_num_results` results per archive.
* **Parallelism.** In Celery mode, the query worker runs `max(1, nproc / 2)` concurrent tasks
  (`CLP_QUERY_WORKER_CONCURRENCY`, computed by the controller; not configurable). In Spider mode,
  each `spider-worker` replica's execution manager runs one task at a time (single task executor),
  so parallelism is `spider.worker.replicas` (default 4). Set `spider.worker.replicas` to
  `max(1, nproc / 2)` to match.
* **Polling latency.** Legacy: `query_scheduler.jobs_poll_delay` (0.1 s). Spider mode:
  `query_coordinator.job_polling_interval_millisecs` (100 ms), `query_coordinator.result_polling`
  (100–1000 ms backoff) and `spider.worker.scheduler_poll_wait_ms` (default 1000 ms).
* **Result retention.** The garbage collector drops result collections older than
  `results_cache.retention_period` minutes (default 60). Set it to `null` (or use `--dump-dir`)
  when comparing runs across a restart or mode switch.
* Both modes share one `var/data`. Switching modes requires stopping the package, and the Spider
  query path needs a `var/data` whose `query_jobs` table was created with the current schema.
  Before stopping the package, remove every job that didn't reach a terminal status (see
  [`submit`](#submit)); otherwise the legacy query scheduler runs it alongside the first Celery
  benchmark jobs.
