# Conformance report: Rust clp-s reader prototype (M0-M3)

**Date:** 2026-10-18. **C++ reference:** `build/core/clp-s` (branch `timeline_size_config`, commit
`78795ed1`, the corpus build; not rebuilt). **Workspace state:** as found at review time; no code
was edited for this review. **Updated** after fixing the outstanding issues. The counts in §1 and
§2.2 come from the re-run, and the status of each issue is in [Outstanding
issues](#outstanding-issues).

## Verdict

The prototype meets the correctness goal of the round. Every gate and every test suite is green,
including the opt-in dataset suites and the docker-backed results-cache, S3 and Spider suites.
Every fixture archive and dataset archive written by the C++ `clp-s` opens, and decompresses to the
same canonical record multiset as C++ `clp-s x`. Every query result equals the C++ result, except
where a registered divergence explains the difference, and the Rust reference evaluator confirms
each of those cases. The prototype's scope rules hold: only SFAs are read, there are no IR
features, unsupported archives are rejected at open, and the archives ingested from kv-IR are
readable.

The remaining issues are style-rule violations, a few small drifts between the design and the
code, and some gaps in reporting and coverage. None of them is a correctness defect. They are
listed in [Outstanding issues](#outstanding-issues). After the final fix, only one item is left:
adding the DV-47 row to design report §8.3, which lives outside this workspace.

## 1. Gates and test counts

All commands used a dedicated `CARGO_TARGET_DIR` and `CARGO_BUILD_JOBS=6`.

| gate | command | result |
|---|---|---|
| rustfmt | `scripts/lint.sh` → `cargo +nightly fmt --all -- --check` | clean |
| clippy | `cargo +nightly clippy --workspace --all-targets --all-features -- -D warnings` (with the repo's `all`/`pedantic`/`nursery` denies) | clean; there are no `#[allow]`/`#[expect]` attributes in the workspace |
| tests (default) | `cargo test --workspace` | **677 passed, 0 failed, 21 ignored** (676 before the final fix) |

Default test counts per crate (unit tests plus integration tests):

| crate | passed | ignored (opt-in) |
|---|---|---|
| clp-s-format | 99 | 0 |
| clp-s-query | 118 | 1 |
| clp-s-difftest | 87 | 2 |
| clp-s-table | 65 | 1 |
| clp-s-search | 59 | 2 |
| clp-s-exec | 54 | 0 |
| clp-s-json | 31 | 1 |
| clp-s-cli (lib, bin, `tests/*`) | 27 | 2 |
| clp-s-spider-adapter | 27 | 7 |
| clp-s-encoding | 24 | 0 |
| clp-s-resources | 16 | 0 |
| clp-s-object-store | 15 | 0 |
| clp-s-timestamp | 14 | 0 |
| clp-s-io | 13 | 1 |
| clp-s-sinks | 11 | 0 |
| clp-s-wire | 7 | 0 |
| clp-s-reader | 5 | 0 |
| clp-s-results-cache | 5 | 4 |

All 21 ignored tests were run, as follows:

| suite | how | result |
|---|---|---|
| `clp-s-query --test suite` (dataset counts) | `--release -- --ignored` | 1 passed |
| `clp-s-difftest --test datasets` (`x` + 32 dataset queries, with the reference evaluator) | `--release -- --ignored` | 2 passed (30 pass + 2 accepted) |
| `clp-s-io --test storage` (dataset archives open and decode) | `--release -- --ignored` | 1 passed |
| `clp-s-json --test golden` (datasets render the C++ bytes) | `--release -- --ignored` | 1 passed |
| `clp-s-search --test planner` (dataset plans and filters vs. reference and C++ counts) | `--release -- --ignored` | 2 passed |
| `clp-s-table --test archives` (datasets decode to the C++ records) | `--release -- --ignored` | 1 passed |
| results cache (`mongo:8.0.21`, `proto-mongo-<suffix>`) | `crates/clp-s-results-cache/scripts/run-mongo-tests.sh` | 4 + 1 passed |
| S3 (`chrislusf/seaweedfs`, `proto-s3-<suffix>`) | `crates/clp-s-object-store/scripts/run-s3-tests.sh` | 1 passed (fixtures + datasets through S3, compared with local files and with C++ `--auth s3`) |
| Spider end to end (MongoDB + S3) | `crates/clp-s-spider-adapter/scripts/run-e2e-tests.sh` | 7 passed (fs and S3, with and without top-k, dataset top-k, retry idempotence, fallback on an unsupported archive) |

All `proto-*` containers were removed afterwards.

## 2. What was verified against C++

### 2.1 Format compatibility: fixtures and datasets (`clp-s-difftest`, release build)

| stage | reader | passed | accepted | failed |
|---|---|---|---|---|
| fixture `x` (59 archive files: 57 readable + 2 rejection) | rust-library | 52 | 7 | 0 |
| fixture `x` | rust-cli (`clp-s-rs x`) | 52 | 7 | 0 |
| fixture `x` | cpp (harness self-test) | 57 | - | 2 (the rejection fixtures, which C++ reads); 59/59 verdicts consistent |
| datasets: `x` + 32 queries | rust-library | 33 | 2 | 0 |
| datasets: `x` + 32 queries | rust-cli | 33 | 2 | 0 |
| `extract-datasets.sh` (`x` on bench, spider, synthetic) | both | `x:bench`, `x:spider`, `x:synthetic` PASS | | |

- The 7 accepted fixture `x` cases are the 5 `*.nofloat` archives under DV-01 (plain floats print
  shortest round-trip, not `%f`), plus `scalars.mfa` (SX-01) and
  `arrays.structurize-arrays.clps` (SX-02). With `should_emulate_cpp_plain_floats`, every
  readable fixture equals its C++ oracle exactly (`clp-s-difftest/tests/fixtures.rs`).
- Every fixture's records are compared as canonical multisets: keys sorted, number tokens kept
  verbatim, and the digests are byte-equal to `scripts/corpus.py`'s
  (`canonical_digests_equal_corpus_py_for_every_oracle`, 57 + 303 oracles).
- The datasets (bench 1,000,000 records, spider 1,057,323, synthetic 500,000; format 0.5.1)
  decompress to the C++ `x` multiset through the library, through `clp-s-rs` and through S3.
- Layer-level goldens:
  - every fixture and dataset table decodes to the C++ records (`clp-s-table`);
  - every table renders the C++ bytes, up to DV-01/DV-02 (`clp-s-json`);
  - every timestamp pattern in the archives parses and formats like the C++ output
    (`clp-s-timestamp/tests/cpp_archives.rs`, `cpp_goldens.rs`);
  - the variable encodings are byte-exact with `clp/ffi/encoding_methods`
    (`clp-s-encoding/tests/cpp_archives.rs`);
  - msgpack tuples are byte-equal to the C++ file output (`clp-s-wire/tests/cpp_tuples.rs`,
    `clp-s-sinks/tests/msgpack.rs`).
- **Robustness:** every single-byte and size-field mutation, and every truncation, of every fixture
  either reads or fails with a typed error, and never panics (`clp-s-format/tests/archives/mutations.rs`,
  `clp-s-table/tests/archives/mutations.rs`).

### 2.2 Search equivalence

| stage | reader | passed | accepted (registered DV) | skipped | failed |
|---|---|---|---|---|---|
| fixture queries (303 queries, 310 case-outputs) | rust-library | 229 | 78 | 3 | 0 |
| fixture queries | rust-cli | 229 | 78 | 3 | 0 |
| fixture queries | cpp (self-test) | 232 | - | 3 | 75 = exactly the declared cases; 310/310 consistent |

- The accepted cases break down by divergence as follows: DV-19 ×12, DV-11 ×10, DV-15 ×9,
  DV-17 ×8, DV-16 ×5, DV-13 ×5, DV-38 ×4, DV-39 ×4, DV-09 ×4, DV-47 ×3, DV-12 ×3, DV-01 ×3,
  DV-36 ×2, DV-37 ×2, DV-06 ×2, and one each of DV-05, DV-07, DV-08, DV-10, DV-14, DV-20 and
  DV-21 (a case can carry several tags). DV-21 is `wire-msgpack-split`, judged since the final fix.
- Every stdout case is also checked against `clp_s_query`'s reference evaluator (`Query::evaluate`,
  the executable design report §8.1 spec) over the C++-loaded records. A case passes only if Rust
  equals C++ (or its declared expectation) **and** equals the reference.
- The difftest's Rust readers produce the msgpack output since the final fix: the library through
  `MsgpackResultSink`, `clp-s-rs` through `--output file`. Of the 4 msgpack cases, 3 pass and
  `wire-msgpack-split` is accepted under DV-21, with the same verdicts for both readers. The 3
  skipped case-outputs are the results-cache outputs, which need MongoDB. They are covered by
  dedicated tests (`tests/README.md`, "Wire oracles"):
  - `clp-s-cli/tests/outputs.rs` (msgpack file and network outputs against `oracles/msgpack/`, and,
    with MongoDB, results-cache against `oracles/results-cache/`);
  - `clp-s-sinks/tests/msgpack.rs`;
  - `clp-s-results-cache/tests/results_cache.rs`.

  All of them pass.
- **Datasets:** all 32 dataset queries match C++ by count and by multiset fingerprint, and match
  the reference evaluator over the `x` records. The only differences are the two registered
  divergences:
  - `bench-not-bare` (DV-05): Rust returns 750,396 records and C++ returns 1,000,000;
  - `synthetic-poly-range` (DV-11): Rust returns 30,781 records and C++ returns 93,444.

  [`PERF-M3.md`](PERF-M3.md) also confirms that, on the other 30 queries and the 3 extracts, the
  sorted output is byte-identical to the C++ output.
- **Independent differential fuzz (this review):**
  - Setup: 1,300 random KQL queries (nested AND/OR/NOT, wildcard columns, ranges, lists, bare values,
    `-i`) over `search.ts`, `search.ts-tables0`, `search.default`, `search.ts-nofloat` and
    `search.ts-nolog`. Each query ran through C++ `clp-s s`, through `clp-s-rs search`, and through
    the independent Python model `scripts/reference_eval.py`.
  - 0 panics and 0 cases where Rust differs from the Python model.
  - 1,087 results are identical to C++.
  - 96 results differ from C++ and agree with the model. These are the registered semantic
    divergences: NOT over missing values or empty arrays, `NOT *`, polymorphic ranges, and arrays.
  - 117 malformed queries fail in both tools.
  - Harness: a throwaway script, not part of the repository.

### 2.3 Outputs, top-k, S3, Spider

- **msgpack file and network outputs:**
  - the tuples equal the C++ oracles, including `log_event_idx = 0` without log order and
    `timestamp = 0` without a timestamp (DV-34);
  - one writer serves the whole session (DV-21/DV-22): a multi-archive file is not truncated;
  - a closed downstream maps to exit 4.
- **Results cache:**
  - the documents equal the C++ oracle for single-archive top-k;
  - top-k is session-wide over 9 archives (DV-25);
  - `_id`s do not collide without log order (DV-24);
  - retried inserts are idempotent.
- **Top-k operator** (`LatestFirstKey` order, archive exclusion by the timestamp bound): checked by
  unit tests and `clp-s-exec/tests/topk.rs`.
  - The bound uses the archive's millisecond range from `TimestampEntry`, written with
    floor/ceiling bounds (verified in `TimestampEntry.cpp`), so excluding an archive is sound.
- **S3:**
  - the fixture corpus and the datasets read through `S3Source` (sequential ranged GETs) give
    exactly the local-file results, and the C++ results with `--auth s3`;
  - bad URLs and unreachable endpoints map to their exit codes.
- **Spider adapter:** the in-process results-cache documents equal the C++ `clp-s` subprocess
  documents for fixture and dataset queries, over both fs and S3 storage. Retried tasks insert no
  duplicates.

### 2.4 Scope

Checked through `clp-s-rs` by hand, and in tests:

| rule | evidence | result |
|---|---|---|
| SFA only; a directory MFA is rejected with `UnsupportedArchive` | `clp-s-rs x reject/scalars.mfa` → "is a multi-file archive", exit 2; `clp-s-io` unit test; difftest SX-01 | OK |
| structured arrays are rejected at open, before any output | `x` and `s` on `arrays.structurize-arrays.clps` → "unsupported archive feature in section /schema_tree: structured arrays", exit 2, no output; `clp-s-format` crafted test (unordered region); difftest SX-02 | OK |
| version window [0.5.0, 0.5.x] | header-patched copies: 0.4.0, 0.6.0 and 1.5.0 → "unsupported archive version", exit 2; 0.5.2 and 0.5.65535 read; `crafted.rs::versions_outside_the_window_are_unsupported` | OK |
| archives ingested from kv-IR are readable | `kvir-text.default`, `kvir-text.ts`: `x` passes, `wire-msgpack-kvir` passes, and a hand check of 6 queries on `kvir-text.ts` was byte-identical to C++ | OK |
| no IR features | no kv-IR search, `.clp` input or Velox code; the crate list matches [`DESIGN.md`](DESIGN.md) §2 | OK |
| unordered extract only; `--ordered`, `--count`, `--count-by-time` rejected with exit 1 | `clp-s-cli` tests | OK |
| `CurrentThread` only; no pool or prefetch; S3 issues one GET per coalesced range, in sequence | code review of `clp-s-exec` and `clp-s-object-store` | OK |
| DR1/DR2 layering | `cargo tree -p clp-s-exec -e normal` shows no tokio, aws or mongodb; no library reads env vars; table and search do not depend on json | OK |
| no `unsafe` without a measured need | one `unsafe impl WriteBuf for PrefixBuffer` (`clp-s-format/src/decode.rs`), which caps zstd at the planned prefix (design report: prefix ends cut decoded bytes by 18-59%); it has `// SAFETY:` comments | OK |

## 3. Divergences hit

| id | where it shows | treatment |
|---|---|---|
| DV-01 | 5 `*.nofloat` fixture extracts, 3 query cases | float normalization; exact under C++-float emulation |
| DV-05 | `bench-not-bare` (750,396 vs. 1,000,000), `not-bare-var`, fuzz | declared, reference-confirmed |
| DV-06, 07, 08, 09, 10, 12, 13, 14, 16, 17, 19, 36, 37, 38, 39 | fixture query suite (see the counts in §2.2), fuzz | declared, reference-confirmed |
| DV-11 | `synthetic-poly-range` (30,781 vs. 93,444), 10 fixture cases | declared, reference-confirmed |
| DV-15, DV-18 | timestamp-literal cases, `timestamps-escaped-key` | declared |
| DV-20 | logically false queries and `--tge` without a timestamp column: Rust exits 0 with a warning, C++ exits 1 (also seen in the hand checks: `a > "x"`, `a > 1e400`) | declared, exit-code check |
| DV-21, DV-22, DV-23, DV-24, DV-25, DV-27, DV-34, DV-45 | wire and results-cache outputs | output-shape tests |
| DV-29/DV-43, DV-30, DV-31, DV-32, DV-35, DV-41, DV-42 | format and planner unit tests | dedicated tests |
| **DV-47** (new) | `NOT $_filename: ...` (3 cases) | registered in `tests/divergences.toml` only; **not yet in design report §8.3** (issue 9) |
| SX-01, SX-02 | rejection fixtures | expected rejection |

## 4. Correctness hot spots reviewed

- **Format parsing:**
  - The header layout and the version window are checked, and the compressed size must equal the
    file size.
  - The file-info section table ends each section at the next offset, and rejects duplicate or
    missing sections.
  - Packets are parsed from exact slices; zero-length packets are treated as absent (DV-30);
    duplicate packets are corrupt.
  - `num_segments` must be 1.
  - Timestamp ranges are milliseconds (matches `TimestampEntry::ingest_timestamp`).
- **ClpString:**
  - integer and float variable encode/decode were compared line by line with
    `encoding_methods.inc` (sign and zero-padding rules, the 16-digit and length limits, the
    corrupt-encoding checks);
  - the variable offset is in bits ≥ 24 and checked per row at table load;
  - the fused decode-and-escape writer is valid because the escaping is byte-local, and it is
    tested against a byte-at-a-time reference.
- **Timestamp formatting:**
  - The pattern compiler follows `TimestampPattern::create`.
  - Date-time fields use floor division (`date::floor<days>`), and epoch fields use C++'s
    truncating division.
  - Quoted patterns are emitted as JSON strings.
  - Goldens come from the C++ archives and the C++ marshal cases.
- **Planner soundness:**
  - `tests/planner/differential.rs` checks every fixture archive against every suite query plus
    about 50 extra queries. Pruning never drops a matching table, match-all is set only when every
    row matches, and residual filters select exactly the reference rows.
  - The dataset variant also passes.
- **Kernels:**
  - Negated leaves select F only, and a negated multi-column leaf selects `(∪F) \ (∪T)` (DV-05).
  - AND and OR short-circuit on active masks.
  - The ClpString subquery variable match is a greedy in-order subsequence, the same as C++
    `SubQuery::matches_vars`.
  - The fuzzing in §2.2 found no kernel or planner error.
- **Top-k:**
  - The key order (timestamp descending with `None` last, then archive id, then record id) uses the
    millisecond floor (DV-27).
  - A record is rendered only once the heap admits it.
  - An archive is excluded only when the heap is full and the archive's bound is strictly worse.
  - Every archive is opened first to read its bound; past `MAX_NUM_ARCHIVES_HELD_OPEN` the
    archives are closed and reopened.

## 5. Gaps

- **Fuzzing breadth:** the fuzzing in §2.2 covers only the `search.*` fixtures (40 records). The
  other fixtures are covered only by the curated suite.
- **Wire outputs:** the difftest does not drive the results-cache output for the Rust readers (3
  SKIP lines), because it needs MongoDB. Dedicated tests cover it, so the DESIGN §11.3 rule that
  the library and CLI give identical digests is not checked for that output. The msgpack output is
  judged for both readers since the final fix.
- **Performance:** the report (`PERF-M3.md`) is indicative only: WSL2, one machine, archives of
  11-33 MB. In this review, `extract-datasets.sh` ran concurrently with the S3 suite and gave
  `x` best-of-3 times of 0.33/0.46/0.50 s (Rust) against 0.55/0.77/0.86 s (C++) on
  bench/spider/synthetic, in line with `PERF-M3.md`.
- **Out of scope** (not exercised, as the scope requires): MFA, structured arrays, IR, ordered
  extraction, aggregations, multi-threading, Arrow/FFI/Python.
- **Empty archives:** no fixture covers an empty archive (zero records). DV-30 says it is readable,
  but no C++-written oracle pins that.

## Outstanding issues

Severity: **Medium** = fix before this code moves into the repository workspace; **Low** = a
small cleanup; **Info** = a decision to record. There are no High issues.

**Status after the final fix (2026-10-18).** Each issue was verified against the code before it was
changed. Issues 1-8 and 10 are fixed. Issue 9 is fixed inside this workspace; its design-report half
lives outside the workspace and is still open. Issue 11 needs no change. Every gate and every
equivalence suite was rerun afterwards (see [Re-run after the final
fix](#re-run-after-the-final-fix)).

1. **Fixed. Medium (style: docstrings).** About 45 functions lacked docstrings, against the review
   guide rule that every function is documented. All of them were test helpers. Every helper the
   review listed now has a docstring with `# Returns`, `# Errors` and `# Panics` where they apply:
   `clp-s-query/tests/{kql,suite,semantics,normalize}.rs` (and `enum Outcome`, `struct Record`), the
   test module of `clp-s-spider-adapter/src/search.rs` (including `TestHome` and `FaultyPartial`),
   `clp-s-reader/tests/s3.rs` and `clp-s-cli/tests/s3.rs`. A workspace scan found the same gap in
   more in-source test modules, which were fixed too:
   `clp-s-query/src/{column,compare,evaluate,expr,value}.rs`, `kql/{lexer,literal}.rs`,
   `clp-s-spider-adapter/src/{subprocess,tracking}.rs` and
   `clp-s-object-store/src/{source,test_util}.rs`. Stale docstrings found on the way were corrected:
   `truth_with_timestamp` named a parameter that does not exist, and `integer_epoch_ns` described
   its result as a factor. The scan now reports only `main`, the compile-time `assert_send*`
   checks, getters and trait-impl methods.
2. **Fixed. Medium (style: dependencies).** Removed `tracing` and the dev-dependency `tempfile`
   from `clp-s-sinks`, and the dev-dependency `clp-s-format` from `clp-s-object-store`. None of
   them was referenced, including by the crates' scripts.
3. **Fixed. Low (style: assertions).** All the listed sites now print the failing value:
   - `assert_eq!` against the empty value where the type allows it (`RecordMultiset::default()`,
     `MultisetDiff::default()`, `Vec::<u8>::new()`, `set.to_vec()`, `plan.templates()` against
     `&[]`, `next_batch()` and the dictionary and range-index options through `.map(..)` against
     `None`);
   - where the type under test's own `is_empty` is the behaviour being checked (`TableSet`,
     `VariableDictionary`, `JsonRecordBuffer`, `SchemaTree`, the opener's metadata, the skipped
     archive summaries), the assertion keeps that call and prints the value in its message.
4. **Fixed. Low (style: docstring sections).** `# Type Parameters` was added to
   `clp-s-wire::tuple::msgpack_error`, `clp-s-cli/tests/s3.rs::{run, rust_extract, rust_search}`,
   `clp-s-exec/tests/session.rs::run_session` and `clp-s-sinks/tests/sinks.rs::run_session`. The
   two `run_session` helpers now forward each builder and `SearchSession::run` by name. `# Returns`
   or `# Errors` were added or split on the adapter test `run`, `largest_stream_size`, `url.rs`
   `parts`, `e2e.rs::{compare_paths, compare_documents}` (the mismatch description moved from
   `# Returns` to `# Errors`) and the `suite.rs` helpers. `mutations.rs::run` already had a
   correct `# Returns` (it returns `bool` and never fails), so it was left unchanged.
5. **Fixed. Low (API design).**
   - `explain::write_plan` is gone. Its body is now the `impl fmt::Display for ArchiveSearchPlan`
     in `clp-s-search/src/explain.rs`, so the EXPLAIN text is a method of the plan and stays in
     the explain module. `plan.rs` no longer carries a forwarding impl.
   - `OutcomeSet::existential_join` was removed from `OutcomeSet`. It is now a `#[cfg(test)]` free
     function `outcome::existential_join` used by the tests of `outcome.rs` and `archive.rs`, so
     the production type has no test-only surface.
6. **Fixed. Low (behaviour).** `clp-s-rs` and `clp-s-difftest` build their subscriber with
   `.with_ansi(std::io::stderr().is_terminal())`. Checked by hand: a piped stderr has no escape
   codes, and a pseudo-terminal (`script`) still gets colours.
   `clp-s-cli/tests/search.rs::archives_that_cannot_match_succeed_with_a_diagnostic` now asserts
   that the captured warning has no ESC byte. With `.with_ansi(true)` put back, the test fails.
7. **Fixed. Low (design drift).** The code is right and the design was stale, so DESIGN §7.3 now
   describes `ResultsCacheSink` as built: JSON and timestamps, `Commutative`. DESIGN §15 X3 records
   the reason: `_id` keys on the record id (DV-24), and the top-k winners arrive in the single
   `TopK` partition (D10).
8. **Fixed. Low (coverage and reporting).** Both Rust readers of `clp-s-difftest` now produce the
   msgpack output: the library through `MsgpackResultSink` into a buffer (via a shared
   `run_into_buffer` helper), and `clp-s-rs` through `--output file --path`. On the fixture
   queries, both readers now give 229 passed, 78 accepted (`wire-msgpack-split` under DV-21) and
   3 skipped. `tests/queries.rs` pins these counts.
   - New unit test `rust::tests::library_msgpack_search_reproduces_the_cpp_tuples` compares the
     library's tuples with two C++ oracles. Corrupting the sink's output makes it fail.
   - `clp-s-cli/tests/outputs.rs` now hands msgpack to `RustCliReader` and keeps only its
     MongoDB branch.
   - The results-cache output stays skipped in the difftest, because it needs MongoDB.
     `tests/README.md` ("Wire oracles", "Who judges them") now says which tests own it.
   - `tests/RESULTS-M2.md` has an update note in place of the "pending M3a" statement.
9. **Partly fixed. Low (divergence register).** DV-47 is now in the DESIGN §10 divergence table,
   with its three suite cases. Design report §8.3 lives outside this workspace, so **the row still
   has to be added there**. Proposed row:
   `| DV-47 | Search | Range-index leaves under NOT: NarrowTypes splits NOT $f: "s" into NOT f:VarString = s OR EXISTS f:ClpString, then rewrites range-index leaves into log_event_idx ranges, so NOT $_filename: "<file>" returns every record of the file and NOT $_filename: * returns nothing without a range index | K3 over each record's range-index fields |`.
10. **Fixed (recorded). Info (scope).** DESIGN §15 now records the two Spider-adapter behaviours
    as deviations, with their reasons and tests:
    - X1: the C++ `clp-s` subprocess fallback on an `UnsupportedArchive` raised before anything
      was written;
    - X2: the `WaitForBase` retry on a memory-budget `ResourceExhausted`. Inside the reader the
      budget stays try-only (D5).

    The code is unchanged.
11. **No change. Info (scope).** No `todo!("not implemented")` seams remain. Deferred features are
    rejected through typed errors and CLI arguments, which is consistent with the scope ("only
    where a reserved API shape must exist").

### Re-run after the final fix

All commands used a dedicated `CARGO_TARGET_DIR` and `CARGO_BUILD_JOBS=6`.

| check | result |
|---|---|
| `cargo +nightly fmt --all -- --check` | clean |
| `cargo +nightly clippy --workspace --all-targets --all-features -- -D warnings` | clean; still no `#[allow]`/`#[expect]` |
| `cargo test --workspace` | **677 passed, 0 failed, 21 ignored** (+1: the new difftest msgpack test) |
| opt-in release suites (`clp-s-query` suite, `clp-s-difftest` datasets, `clp-s-io` storage, `clp-s-json` golden, `clp-s-search` planner, `clp-s-table` archives) | 1 + 2 + 1 + 1 + 2 + 1 passed |
| `clp-s-difftest fixtures` | rust-library 52 / 7 accepted / 0 failed; rust-cli the same; cpp self-test 59/59 consistent |
| `clp-s-difftest queries` | rust-library and rust-cli 229 / 78 accepted / 3 skipped / 0 failed; cpp self-test 310/310 consistent (232 / 3 skipped / 75 declared) |
| `clp-s-difftest datasets --reader rust-cli` | `x` + 32 queries: 33 passed, 2 accepted (DV-05 750,396; DV-11 30,781), 0 failed; the rust-library run is the opt-in `datasets` test above |
| results cache (`run-mongo-tests.sh`, `mongo:8.0.21`) | 4 + 1 passed |
| S3 (`run-s3-tests.sh`, seaweedfs) | 1 passed (fixtures and datasets through S3, against local files and C++ `--auth s3`) |
| Spider end to end (`run-e2e-tests.sh`) | 7 passed (fs and S3, with and without top-k, dataset top-k, retry idempotence, fallback on an unsupported archive) |
| differential fuzz (the review's harness, 400 new queries, seed 9137, 5 fixtures) | 2,000 runs: 1,662 identical to C++, 169 registered divergences where Rust equals the Python model, 169 malformed queries failing in both; 0 panics, 0 cases where Rust differs from the model |

All `proto-*` containers were removed afterwards.
