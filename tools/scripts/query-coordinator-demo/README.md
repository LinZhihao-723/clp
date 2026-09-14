# Query coordinator demo: Spider vs. Celery, 16 workers each

One-shot commands that run a search on a running CLP package through Spider or Celery, switching
the package between the two modes when needed.

```shell
tools/scripts/query-coordinator-demo/spider-search.sh [-n N] [-i] '<query>'
tools/scripts/query-coordinator-demo/celery-search.sh [-n N] [-i] '<query>'
```

- Searches the `default` dataset. `-n N` prints at most N results, newest first (default 1000); `-i` ignores case; `-h` shows help.
- Requires [uv](https://docs.astral.sh/uv/), Docker, and the [query harness](../query-harness/README.md).

| Environment variable | Meaning |
| --- | --- |
| `CLP_PACKAGE_DIR` | Package to use (default: `<repo>/build/clp-package`). |
| `DEMO_RESOURCE_GROUP` | Spider mode only: sets `query_coordinator.resource_group.name` (default: left unchanged). Use it when the Spider DB volume holds a stale registration of the default resource group. |
| `DEMO_JOB_TIMEOUT_SECS` | How long to wait for the job (default: 300). |

## Behind the scenes

1. **Setup** (idempotent, with a fast path when the package is already in the right state):
   - `spider-search.sh` ensures `package.scheduler: spider`, `spider.worker.replicas: 16`, `dispatch_queue_capacity: 64`, `storage_poll_timeout_ms: 5` and coordinator `result_polling` of 10/10 ms, and that the query-coordinator and 16 spider-workers are running.
   - `celery-search.sh` ensures `package.scheduler: celery` and `num_archives_to_search_per_sub_job: 1024` (at least the number of archives, so there's no top-N early stop and results match Spider's), then that the query-worker runs `--concurrency 16`. That's a runtime override: only the query-worker is recreated, with `CLP_QUERY_WORKER_CONCURRENCY=16`. A manual `sbin/start-clp.sh` resets it; the next run re-applies it.
   - If the mode or config differs, it refuses to switch while any `query_jobs` row is unfinished. Otherwise it runs `stop-clp.sh`, edits `etc/clp-config.yaml` (keeping the other keys), runs `start-clp.sh` and waits for the services.
2. **Query:** submitted with `query_harness.py submit` (10 ms status polling, `max_num_results` 1000 per archive).
3. **Output:** a header (engine, workers, job ID, status, archives searched, result count), the results, the **query time** (harness wall-clock from the INSERT to the observed finish), the DB `duration` and the setup time.
4. **Cleanup:** the results are saved, then the job's MongoDB result collection is dropped. `query_jobs` rows are kept.

The first run after a mode switch includes a package restart of about 1–2 minutes. It's reported as setup time and isn't counted in the query time.

Runtime state lives in `.demo-state/` (gitignored): `runs/<engine>-<job_id>.jsonl` result dumps, `logs/` for Docker, start and stop output, and `.config.orig.yaml`, a backup of the config from before the demo first changed it.
