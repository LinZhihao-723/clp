# 50-job DB-ingestion contention benchmark (2026-07-05)

Results from running the `log-ingestor-benchmark` harness at **50 concurrent ingestion jobs**. The
headline: the compression-**submission** DB path degrades catastrophically under concurrency
(≈38 s/call), making it the dominant cost and the ingestion bottleneck. This is a strong empirical
confirmation of the [#2358](https://github.com/y-scope/clp/issues/2358) lock-contention hotspot.

This document is a self-contained handoff for a follow-up analysis session.

## TL;DR

| DB-call metric | 2-job baseline | **50-job run** | Blow-up |
|---|---|---|---|
| Ingestion per entry (`ingested_s3_object_metadata` insert) | 1.6 ms | **11.4 ms** | ~7× |
| **Compression submission per call** (`submit_for_compression` txn) | 47 ms | **38 400 ms** | **~800×** |
| Compression completion per call (final status UPDATE) | 23 ms | **1 080 ms** | ~47× |

Achieved throughput at 50 jobs was **~5 800 rows/s**, well under the configured **12 500 rows/s**
throttle ceiling (50 jobs × 15 000/min) — i.e. the DB, not the synthetic ingest rate, was the
binding constraint.

## Run configuration

- **Jobs:** `num_jobs: 50`, `tasks_per_job: 8`
- **Rate:** `entries_per_minute_per_job: 15000` (→ ceiling 12 500 rows/s aggregate)
- **Batch:** `batch_size_min: 1`, `batch_size_max: 10`, `object_size_bytes_mean: 1024`
- **Buffer:** `flush_threshold_bytes: 4 GiB`, `timeout_sec: 60`, `channel_capacity: 16`
  (with ~1 KB objects, flushes are **time-driven** at 60 s, not size-driven)
- **Scheduler:** `poll_interval_ms: 100`, `max_concurrent_jobs: 16`,
  `simulated_compression_duration_ms: 0`
- **DB:** MySQL 8.4.0 (`mysql:8.4.0`), default `max_connections` (151); connector pool max 100,
  scheduler pool `max_concurrent_jobs + 8 = 24`
- **Host:** 32 cores, ~47 GiB RAM, Docker 29.1.3 / Compose v2.40.3 (WSL2)

## Results

### DB-call cost averages (cumulative, process-global accumulators)

Final metrics line emitted by `mock-ingestor` right before graceful stop:

```
ingestion_per_entry_us=11368.4  submission_per_call_ms=38410.53  completion_db_per_call_ms=1084.05
(entries=52555394  submits=8250  completions=8250)
```

- **Ingestion:** 11.37 ms/entry over **52 555 394** entries
- **Compression submission:** 38.41 s/call over **8 250** calls
- **Compression completion:** 1.08 s/call over **8 250** calls

### Final DB state (at freeze)

- `ingested_s3_object_metadata`: **52 643 302 rows**
- `compression_jobs`: **8 250 rows, all `status = 2` (Succeeded)** — the mock scheduler kept up
  100%; it never became the bottleneck
- `ingestion_job`: 50 rows, `SUM(num_files_compressed) = 52 331 971` (~99.4% marked compressed;
  ~311 k rows trailing in `Submitted` at shutdown)

### Throughput / timing

- Wall-clock: **~2 h 31 m** (first metrics report 18:45:45Z → last 21:17:09Z)
- Average ingest rate: **~5 790 rows/s** (52.56 M entries / ~9 084 s)
- Submission cost climbed monotonically as the tables grew (0 → 38 s), so the cumulative average is
  weighted toward the higher-contention tail (see caveat).

## ⚠️ Caveats — read before analyzing

1. **Row-count overshoot: 52.6 M, not 20 M.** The target was 20 M rows, but the harness has **no
   built-in total-row stop condition** and ~2.5 h of wall-clock elapsed before the run was frozen,
   so it sailed past 20 M to **52.6 M**. The cumulative averages therefore over-weight the later,
   higher-contention samples. **The submission cost at 20 M would have been lower than 38 s** — the
   metric grows with table/row-set size. Treat 38 s as "cost near 50 M rows," not "cost at 20 M".
   For a clean 20 M-pinned number, add a `--stop-at-rows` cap to `mock_ingestor` (see Next steps).
2. **Cumulative, not windowed.** All three metrics are lifetime averages from process-global
   `AtomicU64` accumulators, not per-interval rates. To see the *trend* (cost vs. row count) you'd
   need to diff successive metrics lines from the `mock-ingestor` logs, or add windowed reporting.
3. **`GROUP BY status` was not captured.** An exact status histogram on the 52 M-row unindexed
   `status` column exceeded a 4-minute query timeout; the compressed count is derived from
   `ingestion_job.num_files_compressed` instead. (Under active writes, even `COUNT(*)` timed out —
   itself a contention signal.)
4. **Submission batch size = 10 000 (baseline).** This run used the upstream chunk size; the
   experimental `10000 → 1000` batch-size change from the #2358 exploration is **not** on this
   branch.

## What each metric measures (code references)

Branch `log-ingestor-db-benchmark`; instrumentation in product code.

- **`record_ingestion_db`** — `components/log-ingestor/.../telemetry.rs:27`; called at
  `clp_ingestion.rs:912` in `ingest_and_send`. Times the per-batch insert of `ObjectMetadata` into
  `ingested_s3_object_metadata`, normalized per entry.
- **`record_compression_submission`** — `telemetry.rs:33`; called at `clp_ingestion.rs:1149` in
  `submit_for_compression`. Timer starts at function entry (`:1092`) and stops after `tx.commit()`
  (`:1148`). **Wraps one transaction:** `INSERT INTO compression_jobs` **plus** a batched
  `UPDATE ingested_s3_object_metadata SET compression_job_id=?, status='Submitted' WHERE id IN (…)
  AND status='Buffered'` (chunks of 10 000). This whole multi-row UPDATE, contended across 50
  concurrent jobs, is the 38 s cost — the #2358 hotspot.
- **`record_compression_completion_db`** — `telemetry.rs:42`; called at `clp_ingestion.rs:1235` in
  `wait_for_compression_and_update_submitted_metadata`. The timer starts **after** the poll-wait for
  the scheduler to finish, so it isolates the final "mark compressed" UPDATE (poll latency excluded).

Accumulators + snapshot: `telemetry.rs:89-104` (`INGESTION_DB_NANOS`/`INGESTION_ENTRIES`/
`SUBMISSION_NANOS`/`SUBMISSION_CALLS`/`COMPLETION_DB_NANOS`/`COMPLETION_CALLS`),
`snapshot_db_call_metrics` at `:53`.

## Interpretation

- The **submission transaction is the bottleneck.** Each flush (time-driven, ~6 400 rows/flush on
  average = 52.6 M / 8 250) runs a single large `UPDATE … WHERE id IN (…)` inside a transaction. With
  50 jobs flushing concurrently against one table, these transactions serialize on row/index locks,
  and the cost grows with both concurrency and table size.
- **Completion is comparatively cheap** (1.08 s) despite touching a similar row count, because it
  updates `WHERE compression_job_id = ?` (a single indexed value) rather than submission's
  `WHERE id IN (…thousands of binds…)`, and the scheduler never fell behind (all 8 250 succeeded).
- **Ingestion inserts** rose 7× (1.6 → 11.4 ms) — real but an order of magnitude below submission.
- The DB path caps aggregate throughput at ~5 800 rows/s here, ~46% of the throttle ceiling.

## Reproduction

```bash
cd components/log-ingestor-benchmark
# set num_jobs: 50 in config/ingestor.yaml (default is 4)
docker compose up --build -d
# monitor the ingestor's own entry counter (COUNT(*) can time out under load):
docker compose logs --tail=5 mock-ingestor | grep ingestion_per_entry_us
# freeze when desired (SIGTERM → graceful buffer flush):
docker compose stop mock-ingestor
docker compose logs mock-ingestor | grep ingestion_per_entry_us | tail -1
docker compose down -v      # destroys the data volume
```

DB creds default to `clp-user` / `clp-password` (see `docker-compose.yaml`).

## Next steps for analysis

1. **Add `--stop-at-rows N`** to `mock_ingestor` so runs pin to an exact row count (20 M), for
   apples-to-apples numbers and reproducibility.
2. **Windowed metrics** (per-interval deltas) to plot submission cost vs. row count and separate the
   concurrency effect from the table-growth effect.
3. **Sweep `num_jobs`** (1, 4, 8, 16, 32, 50) at a fixed row target to get the scaling curve of
   `submission_per_call`.
4. **Test the #2358 mitigation:** re-run with the `10000 → 1000` submission batch size (and/or
   smaller/serialized UPDATEs) and compare submission cost.
5. Capture `SHOW ENGINE INNODB STATUS` / `performance_schema` lock waits during a run to attribute
   the 38 s to specific lock contention.
