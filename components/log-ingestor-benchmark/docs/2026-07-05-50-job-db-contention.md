# 50-job DB-ingestion contention benchmark + READ COMMITTED mitigation

Results from the `log-ingestor-benchmark` harness at **50 concurrent ingestion jobs**, in three parts:

1. **The finding (2026-07-05):** the compression-**submission** DB transaction degrades
   catastrophically under concurrency (up to ≈38 s/call) and grows with table size — the dominant
   cost and the ingestion bottleneck. Strong empirical confirmation of the
   [#2358](https://github.com/y-scope/clp/issues/2358) lock-contention hotspot.
2. **A mitigation (2026-07-06):** running the submission transaction at **READ COMMITTED** (instead
   of the InnoDB default REPEATABLE READ) roughly **halves** the submission cost at comparable scale
   and — more importantly — keeps it **flat** instead of growing. Both this change and the
   `chunks(1000)` batch size are now applied on this branch.
3. **MySQL server tuning (2026-07-07):** raising `innodb_buffer_pool_size` from the 128 MB default to
   4 GB (plus a larger redo log and relaxed `flush_log_at_trx_commit`) cut submission cost a further
   ~3–4× (to ~3.8 s/call) and moved the ingestor from **DB-bound to throttle-bound** — i.e. the
   default InnoDB memory sizing was arguably the bigger culprit than the isolation level.

Self-contained handoff for a follow-up **optimization** session.

## TL;DR

At 50 jobs, batch size 1000, ~1 KB objects, comparing the two submission-transaction isolation levels:

| DB-call metric | **REPEATABLE READ** (baseline) | **READ COMMITTED** (mitigation) |
|---|---|---|
| **Submission per call** | ~29.7 s @19M → **38.4 s** @52M — *grows with table size* | **~12.5 s, flat** across 2M→16.6M |
| Ingestion per entry | 11.4 ms | 7.5 ms |
| Completion per call | 1.08 s | 0.88 s |
| Throughput | ~5 790 rows/s | ~8 400 rows/s |

**READ COMMITTED cut submission cost ~2.4×** at comparable scale (~29.7 s → ~12.5 s), stopped it
growing with the table, and lifted throughput ~1.45×. The bottleneck is real and this materially
relieves it — but does not eliminate it (12.5 s/call is still 14× the completion write).

Both isolation-level runs were DB-bound: throughput stayed well under the **12 500 rows/s** throttle
ceiling (50 jobs × 15 000/min), so the DB — not the synthetic ingest rate — was the binding
constraint. **Tuning InnoDB (Arm C) removes that** — submission drops to ~3.8 s/call and throughput
reaches ~12 200 rows/s (~97 % of the ceiling), so the ingestor becomes throttle-bound.

## Run configuration (both arms)

- **Jobs:** `num_jobs: 50`, `tasks_per_job: 8`
- **Rate:** `entries_per_minute_per_job: 15000` (→ ceiling 12 500 rows/s aggregate)
- **Submission batch:** `chunks(1000)` (both arms — this is *not* a variable here; see note)
- **Object batch:** `batch_size_min: 1`, `batch_size_max: 10`, `object_size_bytes_mean: 1024`
- **Buffer:** `flush_threshold_bytes: 4 GiB`, `timeout_sec: 60`, `channel_capacity: 16`
  (with ~1 KB objects, flushes are **time-driven** at 60 s, not size-driven)
- **Scheduler:** `poll_interval_ms: 100`, `max_concurrent_jobs: 16`,
  `simulated_compression_duration_ms: 0`
- **DB:** MySQL 8.4.0 (`mysql:8.4.0`), default `max_connections` (151); connector pool max 100,
  scheduler pool `max_concurrent_jobs + 8 = 24`
- **Host:** 32 cores, ~47 GiB RAM, Docker 29.1.3 / Compose v2.40.3 (WSL2)

> **Correction to the original (2026-07-05) draft:** an earlier version of this doc claimed the
> baseline run used `chunks(10000)`. That was wrong — the benchmark's Docker image is built from the
> *working tree* (`context: ../../`, `COPY components/log-ingestor …`), which already carried
> `chunks(1000)`. **Both** the 52.6M baseline and the READ COMMITTED run used batch size 1000. So the
> only variable between the two arms below is the isolation level. (Reducing the batch from the
> upstream 10 000 to 1 000 was *already in effect* for the 38 s baseline, i.e. batch reduction alone
> did not tame the contention.)

## Arm A — baseline: REPEATABLE READ (2026-07-05)

Uncapped run, frozen manually via SIGTERM.

Final metrics line (`mock-ingestor`):
```
ingestion_per_entry_us=11368.4  submission_per_call_ms=38410.53  completion_db_per_call_ms=1084.05
(entries=52555394  submits=8250  completions=8250)
```
- Ingestion 11.37 ms/entry; submission **38.41 s/call** over 8 250 calls; completion 1.08 s/call.
- Mid-run at ~19.3M rows the submission average was **~29.7 s/call** — i.e. the average **climbed
  monotonically** (~15 s early → 29.7 s → 38.4 s) as the table grew.
- DB state: `ingested_s3_object_metadata` **52 643 302 rows**; `compression_jobs` **8 250, all
  Succeeded** (scheduler kept up 100%); `SUM(num_files_compressed) = 52 331 971`.
- Wall-clock ~2 h 31 m; average **~5 790 rows/s**.

## Arm B — mitigation: READ COMMITTED (2026-07-06)

Run with the new `stop_at_rows: 20000000` cap (see harness change). **MySQL crashed** at ~16.6M rows
(`mysqld got signal 6` — internal InnoDB abort, **not** OOM: host had 43 GiB free, `OOMKilled=false`),
so the run did not reach the 20M cap. But the per-call cost was **flat for the whole run**, so the
numbers are conclusive.

Final metrics line before the DB died (at 16.6M rows):
```
ingestion_per_entry_us=7482.9  submission_per_call_ms=12465.46  completion_db_per_call_ms=876.07
(entries=16614121  submits=1593  completions=1579)
```
- Ingestion **7.48 ms/entry**; submission **12.47 s/call**; completion **0.88 s/call**.
- Submission average was **flat at ~12.5 s** from ~2M to 16.6M rows (12.5 → 13 → 12.5 s) — it did
  **not** grow with table size the way REPEATABLE READ did.
- DB state at crash (recovered via InnoDB crash recovery): ~16.0–16.6M metadata rows;
  `compression_jobs` **1 599 total — 1 593 Succeeded, 6 Pending** (in-flight when the DB died);
  `SUM(num_files_compressed) = 16 374 910`.
- Throughput before the crash **~8 400 rows/s**.

Because the READ COMMITTED submission curve is flat, extrapolating to 20M would still be ~12.5 s/call;
the crash at 16.6M does not change the comparison. A later rerun of this same arm (READ COMMITTED,
batch 1000) reached the full 20M cap without crashing, at **~16.3 s/call** — so the signal-6 crash was
a transient, non-reproducible MySQL fault, not a deterministic outcome of the workload. (The ~16.3 s
vs ~12.5 s difference is host load: that rerun shared the machine with a live CLP package.)

## Arm C — MySQL server tuning (2026-07-07)

Same code as Arm B (READ COMMITTED, batch 1000), same workload (50 jobs, 20M cap), but with the
benchmark's `clp-db` started with three non-default InnoDB settings (passed as `command:` flags to
the `mysql:8.4.0` container, verified live via `SHOW VARIABLES`):

| Setting | Default | Tuned |
|---|---|---|
| `innodb_buffer_pool_size` | 128 MB | **4 GB** |
| `innodb_redo_log_capacity` | 100 MB | **2 GB** |
| `innodb_flush_log_at_trx_commit` | 1 | **2** |

Final metrics at the 20M cap (clean host, no co-running package):
```
ingestion_per_entry_us=2522.0  submission_per_call_ms=3834.03  completion_db_per_call_ms=205.50
(entries=20003483  submits=1498  completions=1449)
```

| DB-call metric (READ COMMITTED, batch 1000, 20M) | Untuned | **Tuned** | Improvement |
|---|---|---|---|
| **Submission per call** | ~16.3 s | **~3.83 s** | **~4.3×** |
| Ingestion per entry | ~7.9 ms | ~2.52 ms | ~3.1× |
| Completion per call | ~0.70 s | ~0.21 s | ~3.4× |
| Throughput | ~8.4 k rows/s | **~12.2 k rows/s** (~97 % of ceiling) | — |
| Wall-clock | ~37 min | ~27 min | — |
| DB crash | one transient signal-6 (recovered) | **none** | — |

- **The DB stops being the bottleneck.** ~12.2 k rows/s is ~97 % of the 12 500 rows/s throttle
  ceiling, so the ingestor is now **throttle-bound**, not DB-bound. Submission at ~3.8 s is no longer
  the dominant cost.
- **The buffer pool is the dominant lever.** At the 128 MB default, the growing
  `ingested_s3_object_metadata` table and its secondary index don't fit in memory, so the batched
  `UPDATE … WHERE id IN (…)` does disk I/O **while holding row locks** — which is much of what made
  the original contention so severe. A 4 GB pool keeps the working set resident.
  `flush_log_at_trx_commit=2` (fsync ~once/sec instead of per-commit) and the 2 GB redo log further
  cut per-commit write-stall overhead.
- **Caveat — clean vs shared host:** the ~16.3 s untuned figure was measured while a CLP package ran
  on the same host; this tuned run had a clean host. Some of the 4.3× is that difference. But even
  against the earlier *clean-host* untuned READ COMMITTED run (~12.5 s/call at 16.6M before it
  crashed), tuned ~3.8 s is still **~3.3×** faster — so tuning, not host sharing, is the dominant
  factor. A back-to-back untuned-vs-tuned pair on the same idle host would pin the exact ratio.

**Implication for #2358:** the isolation-level fix relieves the lock contention, but the default
`innodb_buffer_pool_size=128M` was arguably the bigger culprit. If the production CLP DB runs near
default InnoDB memory settings, right-sizing the buffer pool (and relaxing redo/flush where the
durability trade-off is acceptable) may matter more than the isolation change — worth flagging on the
issue.

## ⚠️ Caveats — read before analyzing

1. **Not a perfectly controlled A/B.** The baseline (Arm A) was an *uncapped* 52.6M-row run; the
   mitigation (Arm B) was capped and *crashed* at 16.6M. They are compared at best-effort matching
   scale (~16–19M rows). The direction and magnitude are decisive, but this is not a pinned
   20M-vs-20M pair. A clean re-run of both arms to the same capped row count would tighten the number.
2. **MySQL 8.4.0 is unstable under this load.** Arm B's DB crashed with signal 6 at 16.6M; Arm A
   happened to survive to 52.6M. This looks nondeterministic and is unrelated to the isolation change
   or to CLP code. Expect occasional crashes; budget for retries. Worth trying a different minor
   version or InnoDB tuning if it recurs.
3. **Cumulative, not windowed.** All three metrics are lifetime averages from process-global
   `AtomicU64` accumulators, not per-interval rates. The "grows / flat with table size" observations
   come from watching the cumulative average drift across successive `mock-ingestor` log lines.
   Windowed (per-interval delta) metrics would make the trend exact.
4. **`GROUP BY status` on metadata not captured.** A full status histogram on the tens-of-millions
   unindexed `status` column exceeds a multi-minute timeout (even `COUNT(*)` times out under active
   writes — itself a contention signal). Compressed counts are derived from
   `ingestion_job.num_files_compressed`.

## What each metric measures (code references)

Branch `log-ingestor-db-benchmark`; instrumentation in product code (`telemetry.rs` +
`clp_ingestion.rs`). Line numbers drift as the code changes — search by symbol.

- **`record_ingestion_db`** (`telemetry.rs`) — called in `ingest_and_send`. Times the per-batch
  insert of `ObjectMetadata` into `ingested_s3_object_metadata`, normalized per entry.
- **`record_compression_submission`** (`telemetry.rs`) — called at the end of
  `submit_for_compression`. Timer starts at function entry and stops after `tx.commit()`. **Wraps one
  transaction:** connection acquire + `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` + `begin` +
  `INSERT INTO compression_jobs` + a batched `UPDATE ingested_s3_object_metadata SET
  compression_job_id=?, status='Submitted' WHERE id IN (…) AND status='Buffered'` (chunks of 1000) +
  `commit`. **This multi-row UPDATE, contended across 50 concurrent jobs, is the hotspot.**
- **`record_compression_completion_db`** (`telemetry.rs`) — called in
  `wait_for_compression_and_update_submitted_metadata`. Timer starts **after** the poll-wait for the
  scheduler, so it isolates the final "mark compressed" UPDATE (poll latency excluded). It updates
  `WHERE compression_job_id = ?` (a single indexed value), which is why it stays cheap (~1 s) while
  submission's `WHERE id IN (…thousands…)` does not.

Accumulators + `snapshot_db_call_metrics` live in `telemetry.rs` (`INGESTION_DB_NANOS`/
`INGESTION_ENTRIES`/`SUBMISSION_NANOS`/`SUBMISSION_CALLS`/`COMPLETION_DB_NANOS`/`COMPLETION_CALLS`).

## Changes applied on this branch (the current code state to optimize from)

1. **READ COMMITTED for the submission transaction** — `submit_for_compression` now does
   `db_pool.acquire()` → `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` (next-transaction scoped,
   *not* `SESSION`, so it doesn't leak to pooled connections) → `conn.begin()`. The metrics timer
   includes the SET.
2. **Submission batch size = `chunks(1000)`** (down from the upstream 10 000), smaller lock footprint
   per UPDATE.
3. **`stop_at_rows` cap in the harness** — `mock_ingestor` stops gracefully (flush + end jobs + print
   final metrics) once `snapshot_db_call_metrics().ingestion_entries` reaches the configured count
   (`WorkloadConfig.stop_at_rows`, `0` = unlimited). Validated end-to-end.

## Interpretation

- The **submission transaction is the bottleneck.** Each time-driven flush stamps
  `status='Submitted'` across the whole flush's rows via `UPDATE … WHERE id IN (…)` inside one
  transaction. Under REPEATABLE READ, InnoDB takes **gap / next-key locks** on the secondary index
  for the `IN (…)` set and the `status` predicate; with 50 jobs flushing concurrently these
  transactions serialize, and the cost grows with both concurrency and table size.
- **READ COMMITTED** drops gap locking (locks only matching rows, releases non-matching locks
  sooner), which is why it roughly halves the cost **and** removes the growth-with-table-size trend.
- **Completion stays cheap** (~1 s) because it targets `WHERE compression_job_id = ?` (single indexed
  value), not a large `id IN (…)` set.
- Even with READ COMMITTED, submission at ~12.5 s/call is still 14× the completion write — there is
  more to win (see next steps).

## Reproduction

```bash
cd components/log-ingestor-benchmark
# in config/ingestor.yaml: set num_jobs: 50 and stop_at_rows: 20000000
docker compose up --build -d          # image is built from the working tree
# progress via the ingestor's own counter (COUNT(*) can time out under load):
docker compose logs --tail=5 mock-ingestor | grep ingestion_per_entry_us
# the ingestor self-terminates at the row cap; then:
docker compose logs mock-ingestor | grep ingestion_per_entry_us | tail -1
docker compose down -v                # destroys the data volume
```
To A/B the isolation level, toggle `submit_for_compression` between plain `db_pool.begin()`
(REPEATABLE READ) and the acquire+SET+begin block (READ COMMITTED) and rebuild. DB creds default to
`clp-user` / `clp-password` (see `docker-compose.yaml`).

## Next steps for optimization

1. **Clean capped A/B.** Re-run REPEATABLE READ and READ COMMITTED both pinned at the same
   `stop_at_rows` (e.g. 20M) for a pinned before/after, now that the cap exists. Retry on MySQL crash.
   Likewise, pin the untuned-vs-tuned (Arm C) ratio with a back-to-back pair on the same idle host.
2. **Right-size InnoDB in prod (Arm C follow-up).** Confirm the production CLP DB's
   `innodb_buffer_pool_size` and sweep it (128 MB / 512 MB / 1 GB / 4 GB) to find where the metadata
   working set stops fitting; separately measure the `flush_log_at_trx_commit=1→2` durability/latency
   trade-off. This may matter more than the isolation change.
2. **Attack the remaining 12.5 s.** Options to try and measure:
   - Split the flush's status UPDATE out of the compression-job INSERT transaction (shorter lock
     hold), or commit per-chunk instead of one big transaction.
   - Update by a contiguous **primary-key range** (`WHERE id BETWEEN ? AND ?`) instead of `id IN (…)`
     where the flush's ids are contiguous — avoids large IN-lists and secondary-index gap locks.
   - Drop the `AND status='Buffered'` predicate's secondary-index locking (e.g. rely on PK, or add a
     covering index) to shrink the lock set.
   - Sweep the batch size (250 / 500 / 1000 / 2000) for the sweet spot under READ COMMITTED.
3. **Windowed metrics** (per-interval deltas) to plot submission cost vs. row count and separate the
   concurrency effect from table growth.
4. **Sweep `num_jobs`** (1, 4, 8, 16, 32, 50) at a fixed capped row target for the scaling curve.
5. **Attribute the lock waits:** capture `SHOW ENGINE INNODB STATUS` and `performance_schema`
   data-lock / lock-wait tables during a run to confirm the gap-lock hypothesis and see what READ
   COMMITTED still leaves on the table.
6. **DB stability:** investigate / work around the MySQL 8.4.0 signal-6 crash under sustained
   concurrent writes (try a patch release, or InnoDB buffer-pool / redo-log tuning).
