# Rust clp-s reader: prototyping and benchmark

This document has three parts: the conformance of the prototype against the C++ `clp-s` (Part 1, correctness), indicative query performance on three datasets (Part 2), and the query part of Log Archival Bench on its MongoDB dataset (Part 3). Section numbers without a part refer to the part they appear in. The design is in [design.md](design.md).

## Part 1. Conformance against the C++ clp-s

**Date:** 2026-10-18. **C++ reference:** `build/core/clp-s` (branch `timeline_size_config`, commit
`78795ed1`, the corpus build; not rebuilt). **Workspace state:** as found at review time; no code
was edited for this review. **Updated** after fixing the outstanding issues. The counts in §1 and
§2.2 come from the re-run, and the status of each issue is in [Outstanding
issues](#outstanding-issues).

### Verdict

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

### 1. Gates and test counts

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

### 2. What was verified against C++

#### 2.1 Format compatibility: fixtures and datasets (`clp-s-difftest`, release build)

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

#### 2.2 Search equivalence

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

  Part 2 also confirms that, on the other 30 queries and the 3 extracts, the
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

#### 2.3 Outputs, top-k, S3, Spider

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

#### 2.4 Scope

Checked through `clp-s-rs` by hand, and in tests:

| rule | evidence | result |
|---|---|---|
| SFA only; a directory MFA is rejected with `UnsupportedArchive` | `clp-s-rs x reject/scalars.mfa` → "is a multi-file archive", exit 2; `clp-s-io` unit test; difftest SX-01 | OK |
| structured arrays are rejected at open, before any output | `x` and `s` on `arrays.structurize-arrays.clps` → "unsupported archive feature in section /schema_tree: structured arrays", exit 2, no output; `clp-s-format` crafted test (unordered region); difftest SX-02 | OK |
| version window [0.5.0, 0.5.x] | header-patched copies: 0.4.0, 0.6.0 and 1.5.0 → "unsupported archive version", exit 2; 0.5.2 and 0.5.65535 read; `crafted.rs::versions_outside_the_window_are_unsupported` | OK |
| archives ingested from kv-IR are readable | `kvir-text.default`, `kvir-text.ts`: `x` passes, `wire-msgpack-kvir` passes, and a hand check of 6 queries on `kvir-text.ts` was byte-identical to C++ | OK |
| no IR features | no kv-IR search, `.clp` input or Velox code; the crate list matches [design.md](design.md) §2 | OK |
| unordered extract only; `--ordered`, `--count`, `--count-by-time` rejected with exit 1 | `clp-s-cli` tests | OK |
| `CurrentThread` only; no pool or prefetch; S3 issues one GET per coalesced range, in sequence | code review of `clp-s-exec` and `clp-s-object-store` | OK |
| DR1/DR2 layering | `cargo tree -p clp-s-exec -e normal` shows no tokio, aws or mongodb; no library reads env vars; table and search do not depend on json | OK |
| no `unsafe` without a measured need | one `unsafe impl WriteBuf for PrefixBuffer` (`clp-s-format/src/decode.rs`), which caps zstd at the planned prefix (design report: prefix ends cut decoded bytes by 18-59%); it has `// SAFETY:` comments | OK |

### 3. Divergences hit

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

### 4. Correctness hot spots reviewed

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

### 5. Gaps

- **Fuzzing breadth:** the fuzzing in §2.2 covers only the `search.*` fixtures (40 records). The
  other fixtures are covered only by the curated suite.
- **Wire outputs:** the difftest does not drive the results-cache output for the Rust readers (3
  SKIP lines), because it needs MongoDB. Dedicated tests cover it, so the DESIGN §11.3 rule that
  the library and CLI give identical digests is not checked for that output. The msgpack output is
  judged for both readers since the final fix.
- **Performance:** the report (Part 2) is indicative only: WSL2, one machine, archives of
  11-33 MB. In this review, `extract-datasets.sh` ran concurrently with the S3 suite and gave
  `x` best-of-3 times of 0.33/0.46/0.50 s (Rust) against 0.55/0.77/0.86 s (C++) on
  bench/spider/synthetic, in line with Part 2.
- **Out of scope** (not exercised, as the scope requires): MFA, structured arrays, IR, ordered
  extraction, aggregations, multi-threading, Arrow/FFI/Python.
- **Empty archives:** no fixture covers an empty archive (zero records). DV-30 says it is readable,
  but no C++-written oracle pins that.

### Outstanding issues

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

#### Re-run after the final fix

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

## Part 2. Query performance: Rust `clp-s-rs` vs C++ `clp-s` (indicative)

Date: 2026-10-18.

> This is a **correctness-round** prototype. The numbers below are indicative only: one machine
> (WSL2), small archives (11-33 MB), the default cargo `release` profile (no LTO, 16 codegen
> units), five runs per point. The serious benchmark is a later round.

### Summary

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

### Datasets

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

### Method

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

### Column guide

- **wall ms:** median, then min-max and ± half the range relative to the median.
- **Rust/C++:** the ratio of the median wall times.
- **RSS:** median peak RSS.
- **user+sys:** median CPU seconds.
- **count:** the number of output lines in the JSON-to-file run.
- **check:**
  - `identical`: the sorted outputs are byte-identical.
  - `DV-xx (expected N)`: the outputs differ by a registered divergence, and Rust returns exactly
    the expected count.

### Results per dataset

#### bench

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

#### spider

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

#### synthetic

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

### Profiles

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

### Caveats

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

## Part 3. Log Archival Bench query benchmark (MongoDB)

Date: 2026-10-19.

[Log Archival Bench](https://github.com/y-scope/log-archival-bench) (LAB) is YScope's framework for
comparing log archival and analytics tools ([blog post](https://blog.yscope.com/log-archival-bench-a-framework-for-comparing-log-archival-analytics-tools-160e98b9d189)).
It measures ingestion (compression ratio, speed and memory) and search (query latency and memory,
cold and hot). This part reproduces only LAB's **search** benchmark for `clp-s`, with the C++
`clp-s` and the Rust `clp-s-rs` reading the same archives. It keeps LAB's dataset, LAB's six
queries (verbatim) and LAB's per-query procedure. Ingestion is not compared: every archive is
written by the C++ `clp-s` with LAB's flags.

The harness lives in [`benchmarks/lab/`](../benchmarks/lab/README.md). Its README maps each LAB
step to the harness and lists the deliberate differences.

### Summary

- **Every result is correct.** All 180 measured runs (5 repetitions × cold/hot × 6 queries × 3
  tools) exited with 0, wrote nothing to stderr, and returned LAB's expected count. Before the
  timing, the sorted outputs of the three tools were compared for all six queries, and they are
  byte-identical (see [Correctness](#correctness)).
- **Rust is faster than C++ on every query, cold and hot.** Against the C++ reader on the same
  single-file archives:

  | mode | speedup vs C++ SFA, per query | sum of medians: C++ MFA | C++ SFA | Rust SFA | speedup vs C++ SFA, on the sums |
  |---|---|---|---|---|---|
  | cold | 1.44-1.68x | 8.266 s | 8.010 s | 5.095 s | 1.57x |
  | hot | 1.49-1.96x | 7.424 s | 7.450 s | 4.673 s | 1.59x |

- **Q2 dominates the totals.** It returns 336,293 records and takes 3.9 s in Rust against 6.2 s in
  C++ (cold), or 3.8 s against 6.0 s (hot). The C++ readers spend about 1.6-1.75 s of it in the
  kernel, with 1.42M minor page faults per run. Rust spends 0.1-0.2 s there, with about 54k.
- **The small queries are bound by the variable dictionary** (Q3 and Q4 were profiled). Each
  reader decompresses the variable dictionary of all 65 archives (about 50 MiB), with the same
  libzstd on both sides. Rust wins by skipping the per-entry work C++ does around that
  decompression (see [Where the time goes](#where-the-time-goes)).
- **Rust has the lower peak RSS on every query** (17-209 MiB against 46-244 MiB). LAB's own memory
  metric (the average of the RSS samples) is also lower for Rust on every query except Q2, where
  Rust holds close to its 209 MiB peak for the whole run (about 204 MiB on average), while C++
  peaks at 242-244 MiB but averages 157-163 MiB.
- **The archive layout barely matters to the C++ reader.** C++ on LAB's multi-file archives is
  2-10% slower than on single-file archives when cold, and the same within noise when hot.
- The spread is small: min-max over the 5 repetitions is within about 5% of the median in most
  cells. The widest are Rust Q4 (up to 15%; its runs fall into two groups, see
  [Where the time goes](#where-the-time-goes)) and C++ SFA Q3 hot (up to 8%).

### What was reproduced

| LAB | this run |
|---|---|
| Datasets: five; only MongoDB is queried. | MongoDB (`mongod`), the only queried dataset. |
| Ingestion: `clp-s c` per tool, measured. | Not measured. Both archive sets were written once by the C++ `clp-s` with LAB's flags. |
| Search: six KQL queries from `assets/clp/config.yaml`, checked against fixed expected counts. | The same six queries, verbatim, and the same expected counts. |
| Search procedure per query and mode (below). | The same procedure, repeated 5 times. |
| Metrics: query latency, average RSS. | The same two, plus peak RSS, CPU time and page faults from `/usr/bin/time -v`. |

### Dataset and archives

| | value |
|---|---|
| Source | LAB `data/mongod/metadata.yaml`: Zenodo record 11075361, `mongodb.tar.gz` (1,590,845,765 B, 260 log files) |
| Content | MongoDB server logs written while running YCSB workloads A-E repeatedly |
| Log events | 186,287,600 (equals LAB's `number_of_lines`) |
| Raw size | 69,582,861,765 B (64.80 GiB) |
| Input | LAB's `mongod.log`, rebuilt in one streaming pass by `prepare.py input`; the result is equivalent to the `awk`/`sed` concatenation in LAB's `scripts/download_all.py` |

Both archive sets were compressed by the C++ `clp-s` (`build/core/clp-s`, single-threaded, not
rebuilt) from the same `mongod.log`:

| | multi-file (MFA, LAB default) | single-file (SFA) |
|---|---|---|
| Flags | `c --timestamp-key "t.\$date" --target-encoded-size 268435456` | the same plus `--single-file-archive` |
| Archives | 65 directories, 520 files (8 per archive) | 65 files, 713 KB to 8.09 MB (mean 5.38 MB) |
| Size | 349,770,584 B (0.326 GiB) | 349,770,573 B (0.326 GiB) |
| Compression ratio (raw / archive bytes) | 198.9 | 198.9 |
| Records (`clp-s s DIR '*'`) | 186,287,600 | 186,287,600 |

The two sets were compressed concurrently, each in 322 s of wall time with about 370 MiB peak
RSS. This is for reference only; ingestion is not part of the comparison.

### Queries

The queries are LAB's `assets/clp/config.yaml` strings, verbatim. As in LAB, each string goes
through `shlex.split`, and the resulting single argument is the KQL passed to the reader.

| # | KQL | meaning (LAB) | expected count |
|---|---|---|---|
| Q1 | `attr.tickets:*` | `attr.tickets` exists | 38,611 |
| Q2 | `id: 22419` | `id` equals 22419 | 336,293 |
| Q3 | `attr.message.msg: log_release* AND attr.message.session_name: connection` | prefix match on a nested string, and an equality | 1 |
| Q4 | `ctx: initandlisten AND (NOT msg: "WiredTiger message" OR attr.message.msg: log_remove*)` | equality with a negated phrase or a prefix match | 122 |
| Q5 | `c: WTWRTLOG AND attr.message.ts_sec > 1679490000` | equality and an integer range | 52,421 |
| Q6 | `ctx: FlowControlRefresher AND attr.numTrimmed: 0` | equality on a string and an integer | 38,611 |

LAB picks Q1 and Q6 to return the same count as a sanity check. They return different records.

### Method

- **Tools** (a reader searching one archive set):
  - `cpp-sfa`: `clp-s s <archives-sfa> <kql>`.
  - `rust-sfa`: `clp-s-rs search <archives-sfa> --query <kql>`. Given a directory of single-file
    archives, `clp-s-rs` searches its entries, sorted by name, as one session (the same set of
    archives C++ searches).
  - `cpp-mfa`: `clp-s s <archives-mfa> <kql>`, LAB's own layout, as a reference row. The Rust
    reader does not read multi-file archives, so the head-to-head comparison is `rust-sfa` against
    `cpp-sfa`.
- **Binaries:**
  - C++: `build/core/clp-s` of this repository (12.4 MiB plus 34 shared libraries; sha256
    `c2b93a72...`), the same binary as in Parts 1 and 2. Not rebuilt.
  - Rust: `clp-s-rs 0.1.0-dev` from `cargo build --release -p clp-s-cli` at workspace commit
    `fa5e4c2a` (rustc 1.96.0, default cargo `release` profile, statically linked zstd 1.5.7;
    31.6 MiB; sha256 `fcfd955b...`). No code was changed for this benchmark.
- **Procedure** (LAB's `bench_search`). For each repetition, each mode (cold, then hot) and each
  query, every tool runs these steps in turn:
  1. Flush and drop the page cache (`echo 1 > /proc/sys/vm/drop_caches` in a privileged
     container).
  2. Hot mode only: run the same query 3 times to warm up.
  3. Start polling RSS with `ps` every 0.1 s, on a wall-clock grid, summing the reader processes
     of the run. A sample with no reader process counts as 0, and a run without samples records
     -1, as in LAB.
  4. Time the measured run end to end, from spawn until stdout is drained and the process is
     reaped. stdout is counted in memory (the result count is the number of output lines, as in
     LAB) and never written to disk.
- **Repetitions:** the whole procedure ran 5 times. The tool order rotates every repetition, so
  drift and order effects reach every tool equally. Tables show the median, with min-max, over
  the 5 runs.
- **Metrics:**
  - *Query latency* (LAB): end-to-end wall time of the measured run.
  - *Average RSS* (LAB): the mean of the `ps` samples, including the 0 samples taken as the
    process starts or exits.
  - *Peak RSS*, user and system CPU time, page faults and file-system inputs, from
    `/usr/bin/time -v`.
- **Deliberate differences from LAB:**
  - **CPU limit.** LAB runs each tool in a Docker container limited with `--cpus=4`. Here the
    readers run natively, pinned with `taskset -c 8-11`. On this host those are 2 physical cores
    with hyperthreading. Both readers search on one thread (CPU time stays close to wall time), so
    the limit mainly keeps other work off the measured cores.
  - **No container around the reader**, so there is no `docker exec` overhead in the timing.
    `/usr/bin/time` and `taskset` add under a millisecond per run.
  - **Single-file archives** for the comparison, because the Rust reader reads only SFAs. They use
    LAB's flags plus `--single-file-archive`.
  - **5 repetitions** instead of LAB's single pass, with rotation, and medians reported.
  - **stderr** is counted separately instead of being merged into the counted output. Every run
    here had empty stderr, so the counts are the same either way.
  - **Cache flush.** LAB runs a global `sync` before dropping the caches, and so does the
    harness's `config.yaml`. On this host the global `sync` blocked forever on a FUSE filesystem
    elsewhere in the shared WSL2 kernel, so this run used a copy of the config that flushes only the
    data filesystem (`sync -f <data dir>`) before the same `drop_caches`. The drop itself works:
    every cold run read 65-349 MiB from the file system, and every hot run read 0. A first
    attempt with the global `sync` hung in its third repetition. Its 97 completed runs have medians
    within 10% of the final ones and are not used here.
- **Environment:** Intel i9-14900K (32 logical CPUs), 47 GiB of memory, Linux 6.6.87.2 under
  WSL2, archives on an SSD-backed ext4 virtual disk. The load average was about 1-1.5 before the
  run. LAB's published numbers come from a Xeon E5-2630v3 with 128 GB of memory and a 7200 RPM SATA
  disk, running CLP v0.4.0, so the absolute numbers here are not comparable with LAB's. Only the
  ratios between the readers are meaningful.

### Correctness

Before the timing, each query ran once per tool, and the outputs were compared:

| # | expected | C++ MFA | C++ SFA | Rust SFA | outputs |
|---|---|---|---|---|---|
| Q1 | 38,611 | 38,611 | 38,611 | 38,611 | identical after sorting (8,589,948 B); line order differs |
| Q2 | 336,293 | 336,293 | 336,293 | 336,293 | identical after sorting (43,381,797 B); line order differs |
| Q3 | 1 | 1 | 1 | 1 | byte-identical |
| Q4 | 122 | 122 | 122 | 122 | byte-identical |
| Q5 | 52,421 | 52,421 | 52,421 | 52,421 | byte-identical |
| Q6 | 38,611 | 38,611 | 38,611 | 38,611 | identical after sorting (6,872,758 B); line order differs |

No canonicalization was needed and no registered divergence applies: no output contains a float
printed in C++'s six-digit `%f` form, which DV-01 and DV-02 cover. The C++ outputs on MFAs and
SFAs are also identical after sorting. Every measured run of the benchmark returned the expected
count again.

### Results

Speedup is the C++ median divided by the Rust median, so above 1 means Rust is faster.

#### Query latency, cold (s, median (min-max) over 5 runs)

| query | C++ MFA | C++ SFA | Rust SFA | C++ SFA / Rust | C++ MFA / Rust |
|---|---|---|---|---|---|
| Q1 | 0.372 (0.364-0.374) | 0.344 (0.334-0.354) | 0.205 (0.197-0.209) | 1.68x | 1.82x |
| Q2 | 6.253 (6.240-6.292) | 6.158 (6.119-6.210) | 3.908 (3.859-3.917) | 1.58x | 1.60x |
| Q3 | 0.332 (0.326-0.348) | 0.306 (0.304-0.308) | 0.213 (0.208-0.223) | 1.44x | 1.56x |
| Q4 | 0.456 (0.446-0.466) | 0.414 (0.407-0.414) | 0.255 (0.253-0.293) | 1.62x | 1.79x |
| Q5 | 0.482 (0.475-0.483) | 0.448 (0.438-0.451) | 0.283 (0.278-0.291) | 1.58x | 1.70x |
| Q6 | 0.371 (0.365-0.387) | 0.341 (0.331-0.346) | 0.231 (0.222-0.243) | 1.48x | 1.61x |
| sum of medians | 8.266 | 8.010 | 5.095 | 1.57x | 1.62x |

#### Query latency, hot (s, median (min-max) over 5 runs)

| query | C++ MFA | C++ SFA | Rust SFA | C++ SFA / Rust | C++ MFA / Rust |
|---|---|---|---|---|---|
| Q1 | 0.263 (0.261-0.266) | 0.269 (0.262-0.272) | 0.137 (0.134-0.143) | 1.96x | 1.91x |
| Q2 | 5.965 (5.930-6.027) | 5.985 (5.928-6.050) | 3.789 (3.692-3.852) | 1.58x | 1.57x |
| Q3 | 0.235 (0.229-0.240) | 0.240 (0.238-0.259) | 0.154 (0.153-0.156) | 1.55x | 1.52x |
| Q4 | 0.344 (0.337-0.347) | 0.342 (0.337-0.350) | 0.229 (0.194-0.236) | 1.49x | 1.50x |
| Q5 | 0.359 (0.352-0.369) | 0.355 (0.346-0.358) | 0.206 (0.203-0.210) | 1.72x | 1.75x |
| Q6 | 0.258 (0.251-0.265) | 0.260 (0.248-0.268) | 0.158 (0.152-0.159) | 1.64x | 1.63x |
| sum of medians | 7.424 | 7.450 | 4.673 | 1.59x | 1.59x |

#### Memory (MiB, median over 5 runs)

Average RSS is LAB's metric. Peak RSS is `ru_maxrss` from `/usr/bin/time`.

| query | mode | avg RSS: C++ MFA / C++ SFA / Rust SFA | peak RSS: C++ MFA / C++ SFA / Rust SFA |
|---|---|---|---|
| Q1 | cold | 43.6 / 41.5 / 13.1 | 53.4 / 52.3 / 20.9 |
| Q2 | cold | 156.5 / 160.2 / 203.6 | 241.9 / 243.6 / 208.8 |
| Q3 | cold | 38.3 / 37.0 / 10.9 | 45.6 / 45.6 / 17.3 |
| Q4 | cold | 104.8 / 81.9 / 45.5 | 131.1 / 137.2 / 111.1 |
| Q5 | cold | 52.0 / 48.0 / 24.0 | 76.6 / 77.7 / 42.3 |
| Q6 | cold | 46.8 / 42.8 / 14.1 | 51.7 / 51.7 / 20.1 |
| Q1 | hot | 43.5 / 50.4 / 15.6 | 53.9 / 52.5 / 21.1 |
| Q2 | hot | 161.8 / 162.6 / 204.7 | 242.2 / 243.5 / 208.8 |
| Q3 | hot | 40.8 / 36.5 / 12.7 | 46.0 / 46.1 / 17.7 |
| Q4 | hot | 96.3 / 82.8 / 44.8 | 131.5 / 137.6 / 111.0 |
| Q5 | hot | 57.7 / 56.3 / 24.7 | 76.9 / 78.1 / 42.6 |
| Q6 | hot | 48.9 / 50.2 / 14.1 | 51.9 / 52.1 / 20.1 |

Peak RSS varies by at most 0.8 MiB (about 2%) between runs. Average RSS is noisy for runs under
0.5 s, because the 0.1 s grid takes only a few samples and some of them land on the start or exit
of the process (for example, C++ SFA Q1 cold ranges from 28.7 to 49.9 MiB).

#### CPU time, hot (median over 5 runs)

| query | user s: C++ SFA / Rust | system s: C++ SFA / Rust | minor page faults: C++ SFA / Rust |
|---|---|---|---|
| Q1 | 0.25 / 0.08 | 0.02 / 0.05 | 11,097 / 51,356 |
| Q2 | 4.75 / 3.90 | 1.61 / 0.11 | 1,417,975 / 54,477 |
| Q3 | 0.23 / 0.11 | 0.02 / 0.04 | 17,361 / 46,395 |
| Q4 | 0.31 / 0.16 | 0.05 / 0.07 | 39,440 / 70,352 |
| Q5 | 0.32 / 0.16 | 0.05 / 0.04 | 20,379 / 54,548 |
| Q6 | 0.24 / 0.12 | 0.03 / 0.04 | 10,951 / 44,192 |

`/usr/bin/time` reports CPU time in 10 ms steps. Cold runs have about the same user time and up to
0.13 s more system time. C++ MFA matches C++ SFA within about 0.02 s in both.

### Where the time goes

The two queries with the smallest hot-mode margins, Q4 (1.49x) and Q3 (1.55x), were profiled with
`perf record -e cpu-clock` under the same pinning, after 3 warm-up runs. The C++ binary was used
as is, with DWARF call graphs. For Rust, the measured binary was left unchanged; call stacks come
from two profiling builds of the same commit in separate target directories, one with line tables
and one with forced frame pointers (used only for proportions, as it runs about 5% slower).

**User CPU per hot run (ms):**

| phase | Q3 C++ | Q3 Rust | Q4 C++ | Q4 Rust |
|---|---:|---:|---:|---:|
| variable dictionary: zstd decoding | 45.6 | 41.8 | 43.6 | 41.6 |
| C++ streaming zstd wrapper (`try_read`, `ZSTD_decompressStream`, `memcpy`) | 51.6 | - | 51.3 | - |
| zstd Huffman loop (assembly without unwind info; dictionary and tables together) | 14.4 | 15.1 | 15.2 | 16.3 |
| variable dictionary: building entries (C++: one `std::string` per entry; Rust: one buffer plus offsets) | ~55 | 7.2 | ~55 | 7.5 |
| variable dictionary: freeing the previous archive's entries at the next open (C++) | 30.0 | - | 26.5 | - |
| exact variable-dictionary lookup | 3.7 | **30.3** | 3.3 | **30.3** |
| wildcard dictionary match | 12.3 | 13.8 | 9.6 | 13.7 |
| table stream zstd | 0.5 | 0.5 | 55.0 | 49.0 |
| filter (C++ `ColumnScan`; Rust `CompiledTableFilter` and table scan) | 0.1 | 0.0 | 14.3 | 8.5 |
| output (JSON rendering and writing) | 0 | 0 | 0.1 | 0.2 |
| archive open, startup, other | ~15 | ~7 | ~26 | ~8 |
| **total user** | ~228 | ~116 | ~300 | ~175 |

The rows do not overlap and add up to the total. The C++ wrapper row holds every sample under
`try_read` that is not entropy decoding, so the entry-building row leaves those out.

- **These queries are bound by the variable dictionary.** Each reader decompresses the whole
  variable dictionary of all 65 archives, about 50 MiB. Q3 decompresses almost no table data; Q4
  also decodes the `initandlisten` tables. Output costs under 0.3 ms (1 and 122 results).
- **The decompression itself is the same work on both sides:** the same libzstd 1.5.7 on the same
  bytes. The difference is the work around it. C++ reads the dictionary entry by entry through a
  streaming wrapper, allocates one `std::string` per entry, and frees them all when the next
  archive opens: about 130-135 ms per run. Rust decompresses each section in one call into one
  buffer with an offset table, which costs about 7 ms.
- **Why the margin is narrow here:**
  1. The shared zstd work is half or more of Rust's time (about 57 of 116 ms in Q3, about 107 of
     175 ms in Q4).
  2. `VariableDictionary::find_exact` (`crates/clp-s-format/src/dictionary.rs`) hashes every
     dictionary entry with the default SipHash hasher to look up one or two probe strings. That
     costs about 30 ms per run, against 3-4 ms in C++.
  3. **Heap churn.** In Q3, Rust grows the heap by 184.8 MiB and shrinks it by 184.4 MiB over a
     run, while its peak heap is only 11.3 MiB. glibc returns the memory to the OS after each
     archive, so the next archive's dictionary buffers land on fresh pages. That costs 46k-70k
     minor page faults (against 17k-39k for C++), or about 30-40 ms of system time. 76% of these
     faults are under `load_dictionaries`.
  4. **This churn is why Q4 has the smallest ratio.** Rust's Q4 times fall into two groups. Runs
     with about 29k faults take 0.194-0.200 s hot, and runs with about 70k faults take
     0.229-0.236 s. The hot median fell into the slow group, and the cold median into the fast one,
     which is also why Q4 is the only query with a higher ratio cold (1.62x) than hot (1.49x).
- **Q2.** C++ spends 1.6-1.75 s of its 6 s in the kernel, with 1.42M minor faults per run; Rust
  spends 0.1-0.2 s with about 54k. User time is 4.75 s against 3.90 s. So a large part of Q2's
  1.58x comes from allocator behaviour on the C++ side, and the rest from decoding and JSON
  rendering, as in Part 2.

**Cold against hot.** Cold mode adds about 60-95 ms to the SFA runs of both readers; Q2 adds more
(C++ 173 ms, Rust 119 ms). Rust Q4 adds only 26 ms, because its cold median falls into the fast
fault group and its hot median into the slow one. Dropping the caches and then re-warming only the
binary or only the archives splits that cost:

- **Archive I/O:** about 50-60 ms for both readers. Both read 50-58 MiB synchronously without
  prefetch: C++ in about 1,800 reads, Rust in about 200 larger ones.
- **Program loading:** C++ needs about 30-40 ms to load its binary and its 34 shared libraries
  (curl, OpenSSL, GnuTLS, Kerberos, LDAP, libssh and others), which the cache drop evicts. Rust is
  one statically linked file and needs about 10 ms.
- A similar fixed penalty on a smaller Rust time lowers the cold ratio of Q1, Q3, Q5 and Q6. Q2
  does not move.
- The MFA archives cost C++ about 25-95 ms more than the SFAs when cold, because each archive is 8
  files to open and read. When hot, the two layouts are within noise.

**What would widen the gap** (not applied; the benchmark measures the code as it is):

1. **Stop the heap churn (measured).** Rerunning both binaries with glibc tunables that disable
   heap trimming (`glibc.malloc.trim_threshold` and `glibc.malloc.mmap_threshold` at 1 GiB) left
   peak RSS unchanged and moved Q3 from 151-162 to 117-120 ms for Rust (C++: 238-244 to
   223-229 ms), and Q4 from 225-232 to 192-204 ms for Rust (C++: 339-348 to 325-350 ms). That is
   about 1.9x for Q3 and 1.7x for Q4. Product options: a session-wide pool for the dictionary
   decode buffers, `mallopt` in the CLI, or mimalloc or jemalloc.
2. **Make `find_exact` cheap (estimated).** With one or two probes, compare lengths and then bytes,
   or use a fast non-cryptographic hasher. That should save about 25 ms, giving about 2.4x for Q3
   and 2.0x for Q4. Q5 and Q6 take the same lookup path.
3. **A literal-prefix fast path** for patterns like `log_release*` and `log_remove*`, before the
   general wildcard matcher. Matching costs about 14 ms now; this would save up to about 10 ms.
4. **Below the zstd floor:** searching archives in parallel (LAB allows 4 CPUs and both readers use
   one, so about 3x in wall time is plausible, but the prototype is single-threaded by design), or
   a format change so that a query does not decompress the whole variable dictionary.
5. **Cold mode:** prefetching the next archive's dictionary while the current one is decoded. CPU
   per archive is about 1.8 ms and I/O about 0.9 ms, so most of the I/O could be hidden. The
   prototype does not prefetch by design.

### Caveats

- The archives total only 0.326 GiB, so cold and hot differ by about 0.1 s per query. Under WSL2,
  a cold read may also be served from the Windows host's cache of the virtual disk, which makes
  cold mode warmer than on LAB's bare-metal SATA disk.
- Rust's cold runs read about 40-45 MiB more from the file system than C++ SFA on the small
  queries (Q2 reads 341-349 MiB for every reader). Part of this is reloading its 31.6 MiB
  statically linked binary after the cache drop.
- The pinned CPUs are 2 physical cores with hyperthreading rather than LAB's quota of 4 CPUs.
  Both readers are single-threaded, so this matters little here, but a parallel reader would need
  the full 4 cores to be compared fairly.
- LAB's average-RSS metric includes the 0 samples at process start and exit, so it is noisy for
  runs under 0.5 s; peak RSS is the steadier memory metric.
- The Rust binary uses the plain cargo `release` profile (no LTO, 16 codegen units).
- The C++ binary is a current development build of `clp-s`, not the CLP v0.4.0 release LAB
  benchmarked.
- The host is a shared WSL2 machine. The spread is small, but the absolute values are not
  portable.
