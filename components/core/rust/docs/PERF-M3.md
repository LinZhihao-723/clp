# M3 query performance: Rust `clp-s-rs` vs C++ `clp-s` (indicative)

Date: 2026-10-18.

> This is a **correctness-round** prototype. The numbers below are indicative only: one machine
> (WSL2), small archives (11-33 MB), the default cargo `release` profile (no LTO, 16 codegen
> units), five runs per point. The serious benchmark is a later round.

## Summary

- **Match counts agree on all 32 dataset queries and all 3 extracts.** On 30 of the 32 queries,
  the Rust output is byte-identical to the C++ output after sorting the lines (and so are all 3
  extracts). The other 2 differ only by a registered divergence, and Rust returns exactly
  `expected_rust_count` from `tests/queries/suite.toml`:
  - `bench-not-bare` (DV-05): C++ returns 1,000,000 records and Rust returns 750,396.
  - `synthetic-poly-range` (DV-11): C++ returns 93,444 records and Rust returns 30,781.

  Every C++ count matches the oracle manifest (`tests/queries/oracles/manifest.toml`). Every run
  of both binaries exited with 0.
- **Rust is faster than C++ on every case measured.** No query is slower in Rust.

  | sink | cases | geomean Rust/C++ wall | best | worst | geomean speedup C++/Rust (range) |
  |---|---|---|---|---|---|
  | JSON to file | 32 queries | 0.70 | 0.48 (`bench-not-bare`, DV-05: fewer results) | 0.89 (`bench-rare-clp-substr`) | 1.42x (1.12-2.10x) |
  | JSON to `/dev/null` | 32 queries | 0.69 | 0.46 | 0.88 | 1.45x (1.14-2.19x) |
  | `x` (extract to a directory) | 3 datasets | 0.58 | 0.56 | 0.60 | 1.72x (1.68-1.79x) |

  Per dataset (JSON to file; speedups are computed per query from the median wall times, then
  combined as a geometric mean):

  | dataset | queries | Rust/C++ wall, geomean (range) | speedup C++/Rust, geomean (range) | Rust/C++ peak RSS | extract Rust/C++ wall | extract speedup C++/Rust |
  |---|---|---|---|---|---|---|
  | bench | 11 | 0.70 (0.48-0.89) | 1.42x (1.12-2.10x) | 0.87-0.89 | 0.58 | 1.71x |
  | spider | 10 | 0.72 (0.58-0.85) | 1.38x (1.16-1.73x) | 0.82-0.86 | 0.56 | 1.79x |
  | synthetic | 11 | 0.69 (0.50-0.88) | 1.45x (1.14-2.00x) | 0.73-0.95 | 0.60 | 1.68x |

  The top speedups on bench (2.10x) and synthetic (2.00x) are the DV-05 and DV-11 queries, where
  Rust returns fewer records than C++ (750,396 vs 1,000,000 and 30,781 vs 93,444). Without them the
  bench maximum is about 1.8x (`bench-match-all`).

  - On the queries that return many results (match-all, `NOT` and common values, from about
    80k to 1M results), Rust takes 0.48-0.74x the C++ time. The gap there is in decoding and JSON
    rendering.
  - On the rare-match queries, Rust takes 0.80-0.89x the C++ time. Both are bound by zstd
    decompression of the table stream, which is the same libzstd 1.5.7 on both sides.
- **Peak RSS is 4-27% lower in Rust** (for example, 106 MiB vs 119-122 MiB on bench and
  46-61 MiB vs 62-75 MiB on synthetic).
- **No count-like mode was measured.** Only C++ has one (`--count`); aggregations are out of
  scope for this round, so `clp-s-rs` rejects `--count`. As the nearest stand-in, every query was
  also run with stdout sent to `/dev/null`. That run still renders JSON, but skips the cost of
  writing it to a file.

## Datasets

All three archives are single-file archives written by the C++ `clp-s c --single-file-archive`
(format 0.5.1, zstd level 3, defaults otherwise). Counts were read from the archives themselves;
they equal the multi-file counts of the C++ baseline measurements.

| | bench | spider | synthetic |
|---|---|---|---|
| Source | `bench.jsonl` (`$PROTO_BENCH_JSONL`) | 9 Spider perf-run `tracing` logs (`$PROTO_SPIDER_FILE_LIST`) | `scripts/gen_synthetic.py`, seed 20261017 (`$PROTO_SYNTHETIC_JSONL`) |
| Shape | flat, 9-10 fields, integer epoch-ms `timestamp`, free-text `message` | nested `fields` object, ISO-8601 µs timestamps | 2-3-level nesting, unstructured arrays, polymorphic `payload`, high-cardinality ids |
| Log events | 1,000,000 | 1,057,323 | 500,000 |
| Log types (logtype dictionary entries) | 1,268 | 2 | 94 |
| Variable dictionary entries | 14,516 | 10,007 | 849,496 |
| Schemas (ERTs) / packed streams | 1 / 1 | 3 / 3 | 7,189 / 74 |
| Size before compression (JSONL) | 260,580,713 B (248.5 MiB) | 479,587,536 B (457.4 MiB) | 156,718,727 B (149.5 MiB) |
| Size after compression (archive) | 14,044,866 B (13.4 MiB) | 11,163,824 B (10.6 MiB) | 33,159,645 B (31.6 MiB) |
| Compression ratio | 18.6x | 43.0x | 4.7x |
| Decoded packed streams (total / largest) | 97.5 / 97.5 MiB | 127.3 / 65.9 MiB | 74.5 / 1.06 MiB |
| Time range (epoch ms) | 1735689600009-1735699604936 | 1787691040223-1787760875552 | 1767225600002-1767229099993 |

spider is highly repetitive (3 record shapes, 2 log types), and most of its strings contain no
space, so they are stored as variable-dictionary ids rather than encoded strings. synthetic is the
schema-heavy case: 2,907 of its 7,189 tables hold 10 records or fewer.

## Method

- **Binaries:**
  - C++: `build/core/clp-s` of this repository (sha256 `c2b93a72...`, the binary that generated
    the oracles; not rebuilt).
  - Rust: `clp-s-rs` built with `cargo build --release -p clp-s-cli` (rustc 1.96.0, sha256
    `b5397427...`), with the default features (results-cache and s3).
  - zstd: C++ links the shared `libzstd.so.1.5.7`. Rust statically links `zstd-sys 2.0.16`, which
    bundles zstd 1.5.7.
- **Archives:** the three dataset SFAs in `$PROTO_DATA_ROOT/datasets/` (format 0.5.1):
  - bench: 14.0 MB, 1,000,000 records.
  - spider: 11.2 MB, 1,057,323 records.
  - synthetic: 33.2 MB, 500,000 records.
- **Queries:** every `dataset:*` query in `tests/queries/suite.toml` (11 bench, 10 spider and 11
  synthetic), plus `x`/`extract` of each archive. Both command lines are built as in
  `crates/clp-s-difftest/src/perf.rs`. C++ runs `clp-s s <archive> <kql> [-i] [--tge] [--tle]
  [--projection ...]`, and Rust runs `clp-s-rs search <archive> -q <kql> ...` with the same
  flags.
- **Procedure:**
  1. The archives are read once to warm the page cache.
  2. Each (case, sink) gets one unmeasured warm-up run per tool.
  3. Five measured rounds follow, alternating C++ and Rust. The order flips every round
     (C++→Rust, then Rust→C++) to cancel drift.
  4. Every process runs as `/usr/bin/time -v taskset -c 8-15 <cmd>`, single-threaded (Rust uses
     `Executor::CurrentThread`, and C++ search is single-threaded).
- **Metrics:**
  - Wall time is taken with `perf_counter` around the process. It includes the `time` and
    `taskset` exec overhead, which is the same for both tools.
  - Peak RSS, user time and sys time come from `time -v`.
  - Spread is the min-max range over the five runs, shown as ± half the range relative to the
    median.
- **Result checks:** the JSON-to-file run of each query is checked by count and by the sha256 of
  the sorted output lines, Rust against C++. Extracts are checked the same way on
  `<out>/original`. The full canonical-multiset comparison is still the job of `clp-s-difftest`.
  This check is a stricter byte-level spot check that happens to pass.
- **Host:** Intel i9-14900K (CPUs 8-15 are 4 P-cores with hyperthreading) on Linux 6.6.87.2 WSL2.
  The load average was about 1-2 during the run.
- **Harness:** a throwaway Python script (not part of the repository) ran the procedure above,
  saved the raw per-run data as JSON, and rendered the tables below. `clp-s-difftest perf` follows
  the same command lines.

## Column guide

- **wall ms:** median, then min-max and ± half the range relative to the median.
- **Rust/C++:** the ratio of the median wall times.
- **RSS:** median peak RSS.
- **user+sys:** median CPU seconds.
- **count:** the number of output lines in the JSON-to-file run.
- **check:**
  - `identical`: the sorted outputs are byte-identical.
  - `DV-xx (expected N)`: the outputs differ by a registered divergence, and Rust returns exactly
    the expected count.

## Results per dataset

### bench

| case | sink | C++ wall ms (median, min-max) | Rust wall ms (median, min-max) | Rust/C++ | C++ RSS MiB | Rust RSS MiB | C++ user+sys s | Rust user+sys s | C++ count | Rust count | check |
|---|---|---|---|---|---|---|---|---|---|---|---|
| x:bench | directory | 538 (529-539, ±1%) | 315 (311-319, ±1%) | 0.58 | 119.0 | 106.0 | 0.53 | 0.31 | 1000000 | 1000000 | identical |
| bench-rare-clp-substr | file | 112 (111-115, ±1%) | 100 (100-103, ±2%) | 0.89 | 121.2 | 106.5 | 0.11 | 0.09 | 71 | 71 | identical |
| bench-rare-clp-substr | devnull | 113 (112-115, ±1%) | 99 (98-101, ±1%) | 0.88 | 121.2 | 106.5 | 0.10 | 0.09 | 71 | 71 | identical |
| bench-rare-int-eq | file | 102 (101-103, ±1%) | 87 (85-94, ±5%) | 0.85 | 121.2 | 106.5 | 0.09 | 0.07 | 1 | 1 | identical |
| bench-rare-int-eq | devnull | 103 (102-111, ±4%) | 87 (85-96, ±6%) | 0.85 | 121.2 | 106.5 | 0.09 | 0.07 | 1 | 1 | identical |
| bench-common-var-eq | file | 219 (218-227, ±2%) | 149 (145-152, ±2%) | 0.68 | 121.2 | 106.5 | 0.21 | 0.13 | 249604 | 249604 | identical |
| bench-common-var-eq | devnull | 199 (197-202, ±1%) | 129 (128-134, ±2%) | 0.65 | 121.2 | 106.5 | 0.19 | 0.13 | 249604 | 249604 | identical |
| bench-int-range | file | 156 (152-163, ±3%) | 116 (114-121, ±3%) | 0.74 | 121.2 | 106.5 | 0.15 | 0.11 | 100100 | 100100 | identical |
| bench-int-range | devnull | 144 (142-145, ±1%) | 108 (106-111, ±2%) | 0.75 | 121.2 | 106.5 | 0.14 | 0.10 | 100100 | 100100 | identical |
| bench-clp-wildcard-rowlevel | file | 114 (113-120, ±3%) | 97 (95-99, ±2%) | 0.85 | 121.2 | 106.2 | 0.11 | 0.08 | 564 | 564 | identical |
| bench-clp-wildcard-rowlevel | devnull | 115 (113-117, ±2%) | 100 (97-107, ±5%) | 0.87 | 121.2 | 106.5 | 0.10 | 0.09 | 564 | 564 | identical |
| bench-bare-value | file | 173 (171-174, ±1%) | 109 (106-110, ±2%) | 0.63 | 119.2 | 106.2 | 0.17 | 0.10 | 41723 | 41723 | identical |
| bench-bare-value | devnull | 169 (168-172, ±1%) | 103 (102-104, ±1%) | 0.61 | 119.5 | 106.5 | 0.16 | 0.10 | 41723 | 41723 | identical |
| bench-not-eq | file | 447 (441-456, ±2%) | 266 (258-275, ±3%) | 0.60 | 121.2 | 106.5 | 0.43 | 0.26 | 750568 | 750568 | identical |
| bench-not-eq | devnull | 384 (378-392, ±2%) | 216 (214-221, ±2%) | 0.56 | 121.5 | 106.5 | 0.37 | 0.20 | 750568 | 750568 | identical |
| bench-ts-range | file | 110 (109-111, ±1%) | 91 (90-97, ±4%) | 0.83 | 122.2 | 106.5 | 0.10 | 0.08 | 9960 | 9960 | identical |
| bench-ts-range | devnull | 109 (109-113, ±2%) | 90 (88-90, ±1%) | 0.82 | 122.2 | 106.2 | 0.10 | 0.08 | 9960 | 9960 | identical |
| bench-match-all | file | 565 (550-592, ±4%) | 317 (315-322, ±1%) | 0.56 | 119.2 | 106.2 | 0.56 | 0.31 | 1000000 | 1000000 | identical |
| bench-match-all | devnull | 460 (458-462, ±0%) | 246 (242-249, ±1%) | 0.54 | 119.2 | 106.2 | 0.46 | 0.24 | 1000000 | 1000000 | identical |
| bench-not-bare | file | 552 (541-554, ±1%) | 263 (262-273, ±2%) | 0.48 | 119.2 | 106.5 | 0.55 | 0.26 | 1000000 | 750396 | DV-05 (expected 750396) |
| bench-not-bare | devnull | 464 (461-468, ±1%) | 212 (211-214, ±1%) | 0.46 | 119.2 | 106.5 | 0.46 | 0.21 | 1000000 | 750396 | DV-05 (expected 750396) |
| bench-projection-ci | file | 121 (118-122, ±2%) | 95 (92-95, ±1%) | 0.78 | 122.5 | 106.2 | 0.10 | 0.08 | 21101 | 21101 | identical |
| bench-projection-ci | devnull | 119 (118-119, ±1%) | 93 (92-93, ±0%) | 0.78 | 122.2 | 106.2 | 0.11 | 0.08 | 21101 | 21101 | identical |

### spider

| case | sink | C++ wall ms (median, min-max) | Rust wall ms (median, min-max) | Rust/C++ | C++ RSS MiB | Rust RSS MiB | C++ user+sys s | Rust user+sys s | C++ count | Rust count | check |
|---|---|---|---|---|---|---|---|---|---|---|---|
| x:spider | directory | 751 (730-757, ±2%) | 419 (417-423, ±1%) | 0.56 | 86.8 | 74.7 | 0.74 | 0.41 | 1057323 | 1057323 | identical |
| spider-rare-int-range | file | 53 (53-54, ±1%) | 42 (41-43, ±3%) | 0.80 | 73.8 | 60.2 | 0.04 | 0.03 | 40 | 40 | identical |
| spider-rare-int-range | devnull | 52 (52-53, ±1%) | 41 (41-42, ±2%) | 0.79 | 73.8 | 60.2 | 0.04 | 0.03 | 40 | 40 | identical |
| spider-rare-var-eq | file | 93 (93-104, ±6%) | 80 (78-82, ±2%) | 0.85 | 87.8 | 74.7 | 0.09 | 0.07 | 96 | 96 | identical |
| spider-rare-var-eq | devnull | 93 (93-95, ±1%) | 80 (79-80, ±1%) | 0.85 | 87.8 | 74.7 | 0.08 | 0.07 | 96 | 96 | identical |
| spider-common-var-eq | file | 426 (421-432, ±1%) | 256 (251-261, ±2%) | 0.60 | 88.0 | 74.9 | 0.42 | 0.24 | 480000 | 480000 | identical |
| spider-common-var-eq | devnull | 349 (346-364, ±3%) | 191 (190-195, ±1%) | 0.55 | 88.0 | 74.9 | 0.34 | 0.19 | 480000 | 480000 | identical |
| spider-int-range | file | 74 (73-75, ±1%) | 60 (60-61, ±0%) | 0.82 | 88.0 | 74.5 | 0.06 | 0.05 | 4022 | 4022 | identical |
| spider-int-range | devnull | 73 (73-75, ±1%) | 60 (59-62, ±2%) | 0.82 | 88.0 | 74.5 | 0.06 | 0.05 | 4022 | 4022 | identical |
| spider-clp-substr | file | 129 (127-132, ±2%) | 104 (102-104, ±1%) | 0.80 | 88.0 | 74.9 | 0.12 | 0.10 | 40000 | 40000 | identical |
| spider-clp-substr | devnull | 123 (120-123, ±1%) | 96 (95-106, ±5%) | 0.78 | 88.0 | 74.9 | 0.11 | 0.09 | 40000 | 40000 | identical |
| spider-bare-value | file | 185 (184-189, ±1%) | 113 (112-117, ±2%) | 0.61 | 87.2 | 74.5 | 0.18 | 0.10 | 97323 | 97323 | identical |
| spider-bare-value | devnull | 172 (170-174, ±1%) | 104 (100-106, ±3%) | 0.60 | 87.2 | 74.5 | 0.17 | 0.10 | 97323 | 97323 | identical |
| spider-not-eq | file | 428 (424-429, ±1%) | 260 (256-278, ±4%) | 0.61 | 88.0 | 74.7 | 0.43 | 0.27 | 577323 | 577323 | identical |
| spider-not-eq | devnull | 354 (350-372, ±3%) | 195 (193-207, ±4%) | 0.55 | 88.0 | 74.7 | 0.35 | 0.19 | 577323 | 577323 | identical |
| spider-ts-range | file | 105 (104-115, ±5%) | 88 (86-90, ±2%) | 0.84 | 88.8 | 74.9 | 0.10 | 0.07 | 10696 | 10696 | identical |
| spider-ts-range | devnull | 103 (101-105, ±2%) | 85 (84-86, ±1%) | 0.83 | 88.5 | 74.9 | 0.09 | 0.07 | 10696 | 10696 | identical |
| spider-match-all | file | 770 (760-800, ±3%) | 446 (426-471, ±5%) | 0.58 | 87.0 | 74.8 | 0.75 | 0.44 | 1057323 | 1057323 | identical |
| spider-match-all | devnull | 613 (607-629, ±2%) | 307 (304-310, ±1%) | 0.50 | 87.0 | 74.8 | 0.61 | 0.30 | 1057323 | 1057323 | identical |
| spider-partial-wildcard | file | 101 (98-106, ±4%) | 82 (81-83, ±1%) | 0.81 | 88.5 | 74.5 | 0.09 | 0.07 | 20 | 20 | identical |
| spider-partial-wildcard | devnull | 99 (97-100, ±1%) | 82 (81-84, ±2%) | 0.83 | 88.5 | 74.6 | 0.09 | 0.07 | 20 | 20 | identical |

### synthetic

| case | sink | C++ wall ms (median, min-max) | Rust wall ms (median, min-max) | Rust/C++ | C++ RSS MiB | Rust RSS MiB | C++ user+sys s | Rust user+sys s | C++ count | Rust count | check |
|---|---|---|---|---|---|---|---|---|---|---|---|
| x:synthetic | directory | 841 (834-854, ±1%) | 502 (493-510, ±2%) | 0.60 | 58.2 | 51.9 | 0.82 | 0.48 | 500000 | 500000 | identical |
| synthetic-rare-clp-substr | file | 155 (154-157, ±1%) | 134 (131-141, ±4%) | 0.86 | 65.0 | 55.1 | 0.15 | 0.12 | 5 | 5 | identical |
| synthetic-rare-clp-substr | devnull | 156 (155-169, ±4%) | 133 (133-136, ±1%) | 0.86 | 65.0 | 55.1 | 0.14 | 0.12 | 5 | 5 | identical |
| synthetic-rare-nested-int-eq | file | 148 (145-152, ±2%) | 128 (126-130, ±2%) | 0.86 | 65.2 | 55.3 | 0.14 | 0.12 | 9 | 9 | identical |
| synthetic-rare-nested-int-eq | devnull | 147 (145-147, ±1%) | 129 (124-131, ±3%) | 0.88 | 65.5 | 55.3 | 0.14 | 0.12 | 9 | 9 | identical |
| synthetic-common-var-eq | file | 287 (281-293, ±2%) | 210 (207-220, ±3%) | 0.73 | 65.0 | 55.1 | 0.27 | 0.20 | 82762 | 82762 | identical |
| synthetic-common-var-eq | devnull | 284 (278-287, ±1%) | 204 (202-206, ±1%) | 0.72 | 65.2 | 55.1 | 0.26 | 0.18 | 82762 | 82762 | identical |
| synthetic-poly-range | file | 286 (279-295, ±3%) | 143 (140-147, ±3%) | 0.50 | 62.0 | 47.4 | 0.28 | 0.13 | 93444 | 30781 | DV-11 (expected 30781) |
| synthetic-poly-range | devnull | 269 (263-290, ±5%) | 136 (133-138, ±2%) | 0.50 | 61.8 | 47.4 | 0.26 | 0.13 | 93444 | 30781 | DV-11 (expected 30781) |
| synthetic-array-search | file | 386 (383-397, ±2%) | 225 (223-229, ±1%) | 0.58 | 62.0 | 50.7 | 0.37 | 0.21 | 84087 | 84087 | identical |
| synthetic-array-search | devnull | 368 (367-382, ±2%) | 215 (211-219, ±2%) | 0.58 | 61.8 | 50.7 | 0.37 | 0.21 | 84087 | 84087 | identical |
| synthetic-clp-wildcard-rowlevel | file | 251 (246-255, ±2%) | 195 (193-199, ±2%) | 0.78 | 65.0 | 55.1 | 0.24 | 0.19 | 26937 | 26937 | identical |
| synthetic-clp-wildcard-rowlevel | devnull | 243 (242-254, ±2%) | 190 (189-194, ±1%) | 0.78 | 65.0 | 55.1 | 0.24 | 0.18 | 26937 | 26937 | identical |
| synthetic-bare-value | file | 304 (303-309, ±1%) | 267 (263-271, ±2%) | 0.88 | 64.0 | 61.1 | 0.29 | 0.25 | 25008 | 25008 | identical |
| synthetic-bare-value | devnull | 299 (294-307, ±2%) | 260 (258-266, ±1%) | 0.87 | 64.0 | 61.1 | 0.30 | 0.26 | 25008 | 25008 | identical |
| synthetic-not-eq | file | 536 (514-553, ±4%) | 337 (331-342, ±2%) | 0.63 | 65.2 | 55.1 | 0.51 | 0.33 | 249594 | 249594 | identical |
| synthetic-not-eq | devnull | 496 (491-497, ±1%) | 309 (303-321, ±3%) | 0.62 | 65.2 | 55.1 | 0.49 | 0.30 | 249594 | 249594 | identical |
| synthetic-ts-range | file | 197 (195-198, ±1%) | 143 (142-145, ±1%) | 0.73 | 74.8 | 58.0 | 0.18 | 0.13 | 5000 | 5000 | identical |
| synthetic-ts-range | devnull | 197 (189-204, ±4%) | 143 (141-152, ±4%) | 0.72 | 74.8 | 58.0 | 0.18 | 0.14 | 5000 | 5000 | identical |
| synthetic-match-all | file | 846 (839-869, ±2%) | 481 (479-497, ±2%) | 0.57 | 63.5 | 52.2 | 0.86 | 0.48 | 500000 | 500000 | identical |
| synthetic-match-all | devnull | 773 (767-778, ±1%) | 428 (423-448, ±3%) | 0.55 | 63.8 | 52.2 | 0.77 | 0.42 | 500000 | 500000 | identical |
| synthetic-array-exists-not | file | 107 (107-108, ±1%) | 63 (61-64, ±3%) | 0.59 | 63.2 | 46.4 | 0.09 | 0.06 | 14922 | 14922 | identical |
| synthetic-array-exists-not | devnull | 109 (103-114, ±5%) | 63 (59-70, ±9%) | 0.57 | 63.2 | 46.4 | 0.10 | 0.06 | 14922 | 14922 | identical |

## Profiles

No query is slower in Rust, so I profiled the two queries with the smallest Rust advantage
instead. The profiler was `/usr/lib/linux-tools-5.15.0-198/perf record -e cpu-clock -F 20000 -g`,
pinned to CPU 8. The Rust binary was rebuilt with `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`
in a separate target directory, so the measured binary was left unchanged.

**`bench-rare-int-eq` (`seq: 424242`, 1 match; Rust/C++ 0.85).** Both tools spend most of the
query in zstd:

- Rust: 85% in `ZSTD_decompressSequences`. After that come `TableScanner::next_window` (2.6%),
  `ZSTD_buildFSETable` (1.8%) and `write_word_masks` (1.7%).
- C++: 67% in `ZSTD_decompressSequences`, about 15% in libc (most likely `memcpy`/`memset` on
  the decompressed buffers) and 3% in Huffman decode.

Rust wins here by spending less time outside zstd. The remaining floor is set by decompressing
the table stream, which takes about 75 ms of Rust's 87 ms on bench. Both readers must do this
unless a later round adds finer-grained pruning.

For reference, a query on a key that does not exist is pruned by the schema in both tools. It
takes 6-8 ms in C++ and 2-3 ms in Rust, so archive open and metadata cost are negligible.

**`synthetic-bare-value` (`svc-tax`, 25,008 matches; Rust/C++ 0.87).** A bare value is checked
against every column, including the unstructured arrays.

- Rust: 23% in zstd, 10.5% in `clp_s_table::array::scan_array`, and about 3% each in
  `array::Parser::string`, `filter::array::Walk::on_event`, `json::escape::escape_into` and
  `VariableDictionary::get`.
- C++: 19% in zstd. It parses arrays with simdjson (`stage1 ... index` at 3.9%,
  `evaluate_wildcard_array_filter` at 1.8%, cached array decompression at 1.7%), and its per-row
  string handling (`malloc`, `_M_append`, message decode at 4.4%) costs more than in Rust.

The likely cause of the narrower margin is the scalar array scanner in Rust (`scan_array` plus the
`Walk` callbacks). It is the clearest single hotspot left that C++ handles with SIMD. If array-heavy
search matters for the serious benchmark, it is a candidate for a SIMD structural-index pass.
`synthetic-array-search` (0.58) and `synthetic-array-exists-not` (0.59) show that array search
is not slow in general; the cost appears when every array of every row must be scanned for a
bare value.

## Caveats

- The results were taken on WSL2 on a shared machine, where other builds may run concurrently.
  The spread is small (mostly under ±5%), but the absolute values are not portable.
- The archives are small, and every query here finishes in under 1 s. The rare-match cases are
  dominated by single-threaded decompression of whole tables; process start, archive open and
  dictionary load are only 2-8 ms.
- The Rust binary uses the plain cargo `release` profile. Enabling LTO or `codegen-units = 1`
  could change the ratios a little in either direction.
- The two registered-divergence queries (`bench-not-bare` and `synthetic-poly-range`) return
  fewer results in Rust, so their Rust/C++ ratios are partly about output volume, not speed.
- The tables do not include S3, msgpack or results-cache outputs. They are outside this report's
  task, which covers JSON output only.
