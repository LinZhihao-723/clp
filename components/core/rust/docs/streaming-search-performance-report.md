# Streaming search performance report

End-to-end streaming search on Spider with the Rust reader (Spider+Rust), compared with the old CLP
package path (Celery + the C++ `clp-s` CLI) and with Presto (CLP connector), after the search-path
optimizations. The details of each optimization are in [`design.md`](design.md) (Part 2 §16) and
[`streaming-search-integration-with-spider.md`](streaming-search-integration-with-spider.md) (§10);
the benchmark method is in [`prototyping-and-benchmark.md`](prototyping-and-benchmark.md) (Part 4).

## Summary at 16 workers

Median end-to-end time in ms. Speedups are old time ÷ new time, so above 1× means the second system
is faster.

| Query | Results | Presto pushdown | Old package (Celery + C++ CLI) | Presto | Spider+Rust | Presto over Celery | Spider+Rust over Celery | Spider+Rust over Presto |
|---|---|---|---|---|---|---|---|---|
| q1 | 1,087 | Yes: `LIKE '<prefix>%'` | 8,298 | 1,189 | 581 | 7.0× | 14× | 2.0× |
| q2 | 107,226 | Yes: `LIKE '%<suffix>\n'` | 11,334 | 5,985 | 2,815 | 1.9× | 4.0× | 2.1× |
| q3 | 1,439,548 | Yes: `LIKE '<prefix>%'` | 12,361 | 6,884 | 2,960 | 1.8× | 4.2× | 2.3× |
| q3alt | 1,875,605 | Yes: `LIKE '%<suffix>\n'` | 11,902 | 6,258 | 2,936 | 1.9× | 4.1× | 2.1× |
| lab1 | 38,611 | Yes: `IS NOT NULL` → `NOT NOT attr.tickets: *` | 2,335 | 2,114 | 70 | 1.1× | 33× | 30× |
| lab2 | 336,293 | Yes: `id = 22419` | 3,389 | 19,438 | 401 | 0.17× | 8.5× | 48× |
| lab3 | 1 | Yes: `substr(...) = 'log_release'` → `"log_release*"`, plus an equality | 2,348 | 83 | 55 | 28× | 43× | 1.5× |
| lab4 | 122 | Yes: equality, `NOT`, `substr` prefix | 2,341 | 289 | 110 | 8.1× | 21× | 2.6× |
| lab5 | 52,421 | Yes: equality plus `> 1679490000` | 2,323 | 2,891 | 76 | 0.80× | 31× | 38× |
| lab6 | 38,611 | Yes: two equalities | 2,323 | 2,162 | 64 | 1.1× | 36× | 34× |

- q1–q3alt run on the HDFS dataset (607 single-file archives); lab1–lab6 run the Log Archival Bench
  queries on its MongoDB dataset (65 single-file archives).
- Every Presto query pushes down to CLP: `EXPLAIN` shows the KQL query in the table scan layout and
  no separate filter.
- Spider+Rust is the fastest on every query: 4.0–43× faster than the old package and 1.5–48× faster
  than Presto.
- Presto is slower than the old package on lab2 and lab5. Even with pushdown, it renders about 345
  nested columns into JSON for every returned record (`SELECT *`), while the other two return the
  stored JSON record directly.
- The old package can't go below about 2.3 s: its CLI starts two containers and polls the job status
  every 0.5 s.

## Reader optimizations

Single-threaded A/B per archive, each against the previous change.

| # | Change | Layer (crate) | Effect |
|---|---|---|---|
| 1 | Exact variable-dictionary lookups by length and bytes, binary search past 8 probes | format (`clp-s-format`) | lab3 0.75×, lab4 0.84×, lab5 0.86×, lab6 0.84× CPU; HDFS 1.0× |
| 2 | Prefix and suffix glob fast path | query (`clp-s-query`) | lab4 0.97× |
| 3 | One-pass logtype candidate selection with an offset bitset | search (`clp-s-search` kernel) | q1–q3alt 0.96–0.97×; filter kernel 7.9% → about 3.4% of samples |
| 4 | Filter-first lazy table-tail decode | io / table / exec | lab2 0.53× (17.1 → 9.1 GB decoded; 88.8 → 43.5 ms per archive at 8 concurrent copies); lab4 and lab5 0.93–1.0×; HDFS within noise |

- **Cumulative, with Arrow output:** q2 0.96×, q3alt 0.97×, lab2 0.53×, lab4 and lab5 about 0.80×.
- **Measured and not kept:**
  - mmap of local archives: 0.983×, which misses its 0.97× gate, and a truncated file would raise
    SIGBUS.
  - A vectorized eager offset check: 1.01×, because the pass is memory-bound.
  - Streaming ring decode: the same speed single-threaded, and it would need streaming evaluation.
  - An inlined byte reader: 0.993×, within noise.
  - Transparent huge pages: no gain under contention.
  - Several archives per Spider task: at most 4% at 8 workers and nothing at 16. Every query task
    searches exactly one archive.
- **Where the time goes now:** zstd is still about 80% of the per-archive time on HDFS, almost all
  of it the message column. Cutting it needs per-column or seekable frames, which is a format change.

## Service-side changes

- **Job completion:** the API server waits for all streaming searches' jobs with one shared poller
  every 10 ms, the query coordinator polls for jobs every 10 ms and for Spider job states every
  5 ms, and Spider's `resource_group_round_robin` scheduler dispatches every 1 ms with a queue of
  128. The client's stream now ends a median 8–9 ms after the job is marked done, against 44–47 ms
  before (p90 12–15 ms against about 97 ms).
- **Memory:** the Arrow passthrough copies messages shorter than 64 KiB out of the connection's read
  buffer. The API server's peak RSS with a 10 MB/s client at 8 workers fell from 128 to 63 MB on
  q3alt and from 58 to 54 MB on lab2.

## End to end at 8 and 16 workers

Median ms, 8 / 16 workers. "Before" is the Arrow prototype without the optimizations.

| Query | Old package (Celery + C++ CLI) | Before | Spider+Rust | Presto |
|---|---|---|---|---|
| q1 | 8,817 / 8,298 | 821 / 720 | 731 / 581 | 1,684 / 1,189 |
| q2 | 13,344 / 11,334 | 3,551 / 2,945 | 3,397 / 2,815 | 8,548 / 5,985 |
| q3 | 14,357 / 12,361 | 3,651 / 3,047 | 3,556 / 2,960 | 9,222 / 6,884 |
| q3alt | 13,891 / 11,902 | 3,747 / 2,943 | 3,621 / 2,936 | 9,010 / 6,258 |
| lab1 | 2,331 / 2,335 | 109 / 110 | 77 / 70 | 2,131 / 2,114 |
| lab2 | 3,352 / 3,389 | 922 / 823 | 441 / 401 | 18,863 / 19,438 |
| lab3 | 2,324 / 2,348 | 112 / 110 | 64 / 55 | 92 / 83 |
| lab4 | 2,312 / 2,341 | 211 / 210 | 120 / 110 | 298 / 289 |
| lab5 | 2,316 / 2,323 | 109 / 109 | 80 / 76 | 2,943 / 2,891 |
| lab6 | 2,325 / 2,323 | 110 / 110 | 81 / 64 | 2,258 / 2,162 |

## Conclusions

1. Spider+Rust is 1.00–1.24× faster than before on HDFS and 1.36–2.09× on the MongoDB queries. lab2
   halves its time and worker CPU (6.39 → 3.26 CPU-s), and the small MongoDB queries no longer sit
   on the 100 ms job-status poll grid.
2. Against Presto, Spider+Rust is 2.0–2.6× faster on HDFS and 1.4–48× on the MongoDB queries, and
   now also wins lab3. Against the old package it is 3.8–4.2× faster on q2–q3alt, 12–14× on q1 and
   19–43× on the small MongoDB queries, while the old package uses 2–3.3× the worker CPU.
3. HDFS is bounded by zstd decompression of the message column (about 80%). Going further needs a
   format change or more cores, not more reader tuning.

## Caveats

1. "Before" times move by a full 100 ms step between passes, so the small-query confidence intervals
   are wide.
2. Presto was measured a day earlier in the same environment, with `SELECT *` on the MongoDB
   queries, and its client parses rows inside the timed window.
3. Everything ran on one WSL2 host (i9-14900K, 32 vCPUs) that hits its CPU ceiling at 16 workers.
4. The old package silently returned short results in 2 of 12 q3 runs at 16 workers while the job
   reported SUCCEEDED; its q3 medians use the 10 exact runs.
