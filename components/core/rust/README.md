# Rust clp-s reader (prototype)

A prototype of a Rust reader for clp-s archives, differentially tested against the C++ `clp-s`.

Scope:

- single-file archives (SFA) of format 0.5.x only; multi-file archives, structured arrays and
  versions outside the window are rejected with `UnsupportedArchive`;
- no IR features (archives ingested from kv-IR are readable; kv-IR search is out of scope);
- unordered extraction (`x`) and search (KQL, time bounds, case-insensitive matching, projection);
- outputs: JSON lines, msgpack file/network tuples, and the MongoDB results cache with session-wide
  top-k;
- archives read from local files, memory or S3, plus an in-process Spider search task.

Documentation:

- [`docs/design.md`](docs/design.md): the architecture, the prototype design and the search
  semantics.
- [`docs/prototyping-and-benchmark.md`](docs/prototyping-and-benchmark.md): conformance against
  the C++ `clp-s` and indicative query performance.
- [`docs/streaming-search-integration-with-spider.md`](docs/streaming-search-integration-with-spider.md):
  the in-process Rust search on the Spider streaming-search path, and its smoke test.
- [`docs/streaming-search-performance-report.md`](docs/streaming-search-performance-report.md):
  end-to-end streaming search with Spider and the Rust reader against the old Celery path and
  Presto.
- [`docs/api-overview.md`](docs/api-overview.md): the public archive iteration API.
- [`tests/README.md`](tests/README.md): the C++ ground-truth corpus.

## Crates

| crate | purpose |
|---|---|
| `clp-s-resources` | Cancellation, memory budget, decode-buffer pool and the shared error categories. |
| `clp-s-encoding` | Byte-exact CLP variable encoding, message reconstruction and the tokenizer. |
| `clp-s-timestamp` | Timestamp patterns of format 0.5.x archives: parsing and C++-identical formatting. |
| `clp-s-format` | Sans-IO parsing, validation and decoding of single-file archives. |
| `clp-s-io` | Byte sources (file, memory, host-provided) and the opened archive storage. |
| `clp-s-object-store` | S3 objects as byte sources (ranged GETs) and S3 URL parsing. |
| `clp-s-table` | Table layouts, loaded tables and window-at-a-time column scanning. |
| `clp-s-json` | JSON rendering of table batches through compiled templates. |
| `clp-s-query` | KQL parsing, the query IR, normalization and the reference evaluator. |
| `clp-s-search` | Archive-bound planning (pruning, dictionary prefilter) and vectorized filters. |
| `clp-s-wire` | Wire formats shared with C++ consumers (msgpack tuples, protocol-v2 stream, results-cache fields). |
| `clp-s-exec` | The execution engine: archives, scans, sessions, sinks protocol, top-k, errors. |
| `clp-s-reader` | The public facade of the reader library. |
| `clp-s-sinks` | Built-in sinks: JSON lines, msgpack file/network, protocol-v2 stream, in-memory collector. |
| `clp-s-results-cache` | The MongoDB results-cache sink. |
| `clp-s-cli` | `clp-s-rs`, the command-line front end (`extract`, `search`). |
| `clp-s-spider-adapter` | An in-process Spider search task that writes into the results cache or a protocol-v2 stream. |
| `clp-s-difftest` | `clp-s-difftest`, the differential harness against the C++ `clp-s`. |

## Build, lint and test

This directory is a standalone Cargo workspace (not a member of the repository's workspace). Run
the commands from here:

```bash
cargo build --workspace
cargo +nightly fmt --all -- --check
cargo +nightly clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

- rustfmt reads the repository-root `.rustfmt.toml`, which is generated (and ignored by git): run
  `task lint:check-rust` or `task lint:fix-rust` once (their `rust-lint-configs` step copies it
  from `tools/yscope-dev-utils`).
- The repository-root `.cargo/config.toml` also applies here and denies clippy `all`, `pedantic`
  and `nursery`.
- `scripts/lint.sh [-p CRATE]...` runs the three gates above.
- Tests that need the large dataset artifacts are `#[ignore]`d; run them in release mode with
  `--ignored`. Tests that need MongoDB, S3 or both run through
  `crates/clp-s-results-cache/scripts/run-mongo-tests.sh`,
  `crates/clp-s-object-store/scripts/run-s3-tests.sh` and
  `crates/clp-s-spider-adapter/scripts/run-e2e-tests.sh` (docker). The protocol-v2 network tests
  of the Spider adapter only need a C++ `clp-s` with that output:
  `CLP_S=<clp-s> cargo test -p clp-s-spider-adapter --test network -- --ignored`.

## Differential tests

`clp-s-difftest` compares a reader with the C++ oracles under `tests/` and accepts a mismatch only
when a registered divergence (`tests/divergences.toml`) explains it:

```bash
cargo build --release -p clp-s-cli -p clp-s-difftest
target/release/clp-s-difftest fixtures --reader rust-library   # or rust-cli
target/release/clp-s-difftest queries --reader rust-library    # fixture queries
target/release/clp-s-difftest datasets --reader rust-cli       # dataset `x` and queries
```

- `--reader cpp` (the default) is the harness self-test: the C++ binary against its own oracles.
- The C++ binary is `$CLP_S`, else `build/core/clp-s` of this repository (build it with
  `task core`); `--cpp-clp-s` overrides both. The Rust tests that run C++ also read `CLP_S`.
- The dataset archives and oracles are large and live under the data root: `$PROTO_DATA_ROOT`, or
  `--data-root`, default `target/proto-data`. `datasets` needs them; `fixtures` and `queries` use
  only the corpus in `tests/`.

## Regenerating the corpus

The fixture archives, the dataset archives and every oracle are produced by the C++ `clp-s`:

```bash
scripts/build-fixtures.sh   # fixture inputs and archives
scripts/gen-oracles.sh      # all oracles (steps: fixtures, queries, datasets, dataset-queries)
```

The scripts need `uv`, docker (for the results-cache oracles) and, for the datasets, the input
files named by `PROTO_BENCH_JSONL`, `PROTO_SPIDER_FILE_LIST` and `PROTO_SYNTHETIC_JSONL`. See
[`tests/README.md`](tests/README.md) for the layout, the environment and the canonical comparison.
