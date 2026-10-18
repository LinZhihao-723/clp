# M2 results: search end to end

**Scope:** M2c (search integration). **Date:** 2026-10-18. **C++ build:** `build/core/clp-s`,
branch `timeline_size_config`, commit `78795ed1` (the corpus build). **Updated** after the r2
fixes: the summary, the divergence list and the per-query table come from a fresh release run;
the performance section below predates r2 (r2 changed only planner bookkeeping).

## What ran

- **Readers under test:** the Rust library (`SearchSession` over every archive of the case, into a
  `JsonLinesSink`) and the `clp-s-rs search` binary. Both give identical verdicts on every case.
- **Three oracles per stdout case:**
  1. C++ `clp-s s` (the corpus oracles, canonical multisets);
  2. the declared Rust expectation (`expected_rust_ids` / `expected_rust_count` /
     `expected_rust_exit`) for cases tagged with a divergence;
  3. `clp_s_query`'s reference evaluator (`Query::evaluate`): over the C++-loaded fixture records
     (`ReferenceRecords`, as `scripts/reference_eval.py` loads them) and, for datasets, over the
     records of the dataset's `x` oracle (`DatasetReference`, one streaming pass per dataset,
     compared by count and an order-independent multiset fingerprint). From M2c on, the reference
     check runs on **every** stdout case, not only on declared ones: a case passes only if Rust
     equals C++ (or the declared expectation) **and** the reference evaluator.
- **Commands** (release build, `PROTO_DATA_ROOT` default):
  - `clp-s-difftest queries --reader rust-library` and `--reader rust-cli --rust-cli <clp-s-rs>`
  - `clp-s-difftest datasets --reader rust-library` and `--reader rust-cli`
  - `clp-s-difftest fixtures --reader rust-library` and `--reader rust-cli`
  - `clp-s-difftest queries --reader cpp` (the harness self-test)
  - `clp-s-difftest perf --rust-cli <clp-s-rs> --runs 5 --cpus 8-15`
- **Tests that pin these results:** `clp-s-difftest/tests/queries.rs`
  (`rust_library_reproduces_every_fixture_query_up_to_registered_divergences`),
  `clp-s-difftest/tests/datasets.rs` (opt-in, `--ignored`), `clp-s-cli/tests/search.rs`
  (`every_fixture_query_matches_the_cpp_oracle_through_the_binary`).

## Summary

| stage | reader | passed | accepted (registered DV) | skipped | failed |
|---|---|---|---|---|---|
| fixture queries (310 case-outputs) | rust-library | 226 | 77 | 7 | 0 |
| fixture queries (310 case-outputs) | rust-cli | 226 | 77 | 7 | 0 |
| fixture `x` (59 archives) | rust-library | 52 | 7 | 0 | 0 |
| fixture `x` (59 archives) | rust-cli | 52 | 7 | 0 | 0 |
| datasets: `x` + 32 queries | rust-library | 33 | 2 | 0 | 0 |
| datasets: `x` + 32 queries | rust-cli | 33 | 2 | 0 | 0 |
| fixture queries, C++ self-test | cpp | 232 | 0 | 3 | 75 (one per declared case) |

- **Skipped (7):** the msgpack (4) and results-cache (3) outputs, which were M3a work when this
  report was written. **Update after M3 (final fix):** both Rust readers of the
  difftest now produce the msgpack output, so a release run gives 229 passed, 78 accepted, 3
  skipped and 0 failed for each. The msgpack cases give 3 passes and `wire-msgpack-split` accepted
  under DV-21. The 3 results-cache outputs stay skipped in the difftest, because they need
  MongoDB. They are judged by `clp-s-cli/tests/outputs.rs` and `clp-s-results-cache/tests/`
  (see `tests/README.md`, "Wire oracles").
- **Fixture `x` accepted (7):** `scalars.nofloat`, `floats.nofloat`, `polymorphic.nofloat`,
  `timestamps-float.ts-nofloat` and `search.ts-nofloat` under DV-01, and the two rejection fixtures
  under SX-01/SX-02 (52 + 7 = 59 archives).
- **C++ self-test:** 310 of 310 verdicts are consistent with C++ reproducing its own oracles; the
  75 failures are exactly the declared cases ("divergence no longer occurs").
- **Dataset divergences confirmed by the reference evaluator:** `bench-not-bare` (DV-05, 750,396
  vs C++ 1,000,000) and `synthetic-poly-range` (DV-11, 30,781 vs C++ 93,444). Every undeclared
  dataset query agrees with the reference evaluator record for record.
- **Round 2 additions:** five C++-written fixtures (`clp-negation.default`,
  `empty-key-wildcard.default`, `empty-key-order.default`, `empty-key-order-reversed.default`,
  `timestamps-escaped-key.ts-escaped`) and 20 suite cases pin DV-06, DV-38 and DV-39 (and DV-16,
  DV-15 and DV-18 next to them). Every case was checked against a deliberate defect in
  `clp-s-search` (storage rule, first-column-wins binding, `*` skipping `""` keys, and
  key-text timestamp binding), which makes it fail.
- **No reader bug was found by the suite.**

## Divergences hit

| divergence | cases |
|---|---|
| DV-01 (shortest round-trip plain floats) | `floats-nofloat-eq`, `floats-nofloat-range`, `poly-nofloat-float`; fixture `x` of `scalars.nofloat`, `floats.nofloat`, `polymorphic.nofloat`, `timestamps-float.ts-nofloat`, `search.ts-nofloat` |
| DV-05 (`NOT <bare word>`) | `not-bare-var`, `bench-not-bare` |
| DV-06 (bug-254 dedupe) | `clp-negation-and`, `clp-negation-not-or` |
| DV-07 (`NOT` of an absent path) | `not-exists-absent-path` |
| DV-08 (`NOT` of a partial wildcard) | `not-partial-wildcard` |
| DV-09 (`null` / `*: null` / `NOT *`) | `bare-null`, `pure-wildcard-null`, `not-pure-wildcard-null`, `match-none` |
| DV-10 (`NOT x: null`) | `not-null` |
| DV-11 (array/poly ranges, float literals) | `range-poly-lt`, `bare-float`, `array-float-literal`, `array-range-gt`, `array-range-lt`, `array-not-range`, `array-object-path-range`, `arrays-range`, `arrays-float-elem`, `poly-float`, `synthetic-poly-range` |
| DV-12 (`null` against polymorphic/array values) | `eq-poly-null`, `array-null-elem`, `arrays-null` |
| DV-13 (negated array leaves) | `array-not-eq`, `array-not-range`, `array-not-string`, `array-object-path-not`, `arrays-not` |
| DV-14 (huge float literal) | `range-float-huge-literal` |
| DV-15 (integer literal vs timestamp in ms) | `range-ts-gt`, `range-ts-gte`, `range-ts-lt`, `eq-ts`, `range-ts-string-column`, `range-ts-int-precisions`, `time-escaped-key-eq`, `time-escaped-key-gt-disjoint`, `time-escaped-key-lt-covering` |
| DV-16 (empty keys not transparent) | `keys-empty-key-child`, `keys-empty-key-parent-exists`, `empty-key-wildcard-literal`, `empty-key-order-reversed-literal`, `empty-key-order-reversed-literal-below-empty-key` |
| DV-17 (GrepCore fixes) | `clp-question-one-char`, `clp-inner-star-span`, `clp-inner-star-numbers`, `clp-question-spans-delimiter`, `clp-edge-star-float`, `clp-edge-star-float-prefix`, `clp-backslash-before-star`, `clp-backslash-before-inner-star` |
| DV-19 (objects satisfy `Exists`) | `exists-poly`, `not-exists-poly`, `exists-empty-object`, `bool-nested-and-not-exists`, `proj-poly`, `poly-exists`, `poly-not-exists`, `poly-child-exists`, `nested-empty-object-exists`, `many-schemas-exists`, `many-schemas-split-or`, `empty-key-wildcard-tail` |
| DV-20 (`--tge` without a timestamp column: exit 0) | `time-no-ts-key` |
| DV-36 (wildcards descend into arrays) | `array-partial-wildcard`, `array-leading-wildcard` |
| DV-37 (archive-dependent array suffix) | `poly-child-exists`, `poly-child-array` |
| DV-38 (wildcard below a `""` key) | `empty-key-wildcard-leading`, `empty-key-wildcard-partial`, `empty-key-wildcard-exists`, `empty-key-wildcard-tail` |
| DV-39 (one descriptor, two columns of a schema) | `empty-key-order-wildcard`, `empty-key-order-partial-wildcard`, `empty-key-order-reversed-literal`, `empty-key-order-reversed-literal-below-empty-key` |
| DV-47 (range-index leaves after `NarrowTypes`) | `meta-not-filename`, `meta-not-filename-and-level`, `meta-not-exists-filename-nolog` |
| SX-01, SX-02 (scope exclusions) | `scalars.mfa`, `arrays.structurize-arrays.clps` |

DV-18 (timestamp index by node id) changes no result by itself and is pinned by the planner tests
`protocol::the_timestamp_index_binds_its_column_by_node_id` (escaped key) and
`protocol::the_timestamp_index_is_not_bound_to_the_same_key_in_another_namespace` (`@timestamp` on
`kvir-text.ts`, which C++ prunes by the `timestamp` index). No fixture has a same-key `@` column, so
the namespace half is not checked at the record level. Combined with DV-15 it does change one:
on `timestamps-escaped-key.ts-escaped` C++ loses the index pruning for the key `t\*s`, so its ns
row-level comparison returns every record for `t\*s > 1800000000000` (with an unescaped key the
index prunes and C++ returns nothing).

## Other findings

- **Repeated projection columns** are accepted and rendered once, as in C++
  (`--projection level level` prints `{"level":"INFO"}` in both).
- **`lev*` in a projection is a literal key** in both C++ and Rust (a `*` inside a token is not a
  wildcard), so both print `{}` for every match. Whole-token wildcards (`*`, `a.*.b`) are rejected
  by both (exit 1).
- **DV-20 diagnostic.** `clp-s-rs search` exits 0 with no output and logs a warning per archive
  that is pruned because the query is logically false or because it has no authoritative timestamp
  column to bound (C++ exits 1).
- **Duplicate keys inside array objects** reach only their first member, as in C++
  (`docs/design.md` Part 2 §2); the reference evaluators keep the last member, and no fixture has one.

## Per-query results

`C++` is the C++ record count from the oracle manifest; `Rust` the Rust record count (equal to
C++ for PASS, from the verdict detail otherwise). `library` and `CLI` are the verdicts of the two
Rust readers. Dataset rows come from `clp-s-difftest datasets`.

| case | target | output | C++ | Rust | library | CLI | divergences |
|---|---|---|---|---|---|---|---|
| `eq-int` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `eq-int-zero` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-int-neg` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-int-max` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-int-min` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-int-quoted` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `eq-int-vs-float-column` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `eq-float-integral` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-float` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-float-neg` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-float-exp` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-float-as-int-literal` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-float-max` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-bool-true` | search.ts | stdout | 9 | 9 | PASS | PASS |  |
| `eq-bool-false` | search.ts | stdout | 7 | 7 | PASS | PASS |  |
| `eq-null` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `eq-var` | search.ts | stdout | 27 | 27 | PASS | PASS |  |
| `eq-var-error` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `eq-clp-exact` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-clp-level` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-clp-unicode` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-clp-escaped-quote` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-clp-backslash` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-clp-control-escapes` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-nested` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `eq-nested-string` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `eq-nested-braces` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `eq-nested-braces-and` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-poly-int` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `eq-poly-float` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-poly-bool` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-poly-var` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-poly-clp` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-poly-null` | search.ts | stdout | 2 | 1 | ACCEPTED | ACCEPTED | DV-12 |
| `eq-poly-object-child` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-escaped-dot-key` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `eq-quoted-dot-key` | search.ts | stdout | 0 | 0 | PASS | PASS |  |
| `eq-space-key` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `range-int-gt` | search.ts | stdout | 7 | 7 | PASS | PASS |  |
| `range-int-gte` | search.ts | stdout | 8 | 8 | PASS | PASS |  |
| `range-int-lt` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `range-int-lte` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `range-int-near-max` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `range-int-near-min` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `range-int-float-literal-gt` | search.ts | stdout | 14 | 14 | PASS | PASS |  |
| `range-int-float-literal-lt` | search.ts | stdout | 5 | 5 | PASS | PASS |  |
| `range-float-gt` | search.ts | stdout | 9 | 9 | PASS | PASS |  |
| `range-float-lt` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `range-float-gte-exp` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `range-float-gt-int-literal` | search.ts | stdout | 10 | 10 | PASS | PASS |  |
| `range-float-lte-neg` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `range-float-huge-literal` | search.ts | stdout | 2 | 1 | ACCEPTED | ACCEPTED | DV-14 |
| `range-poly` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `range-poly-lt` | search.ts | stdout | 3 | 2 | ACCEPTED | ACCEPTED | DV-11 |
| `range-text-column` | search.ts | stdout | 0 | 0 | PASS | PASS |  |
| `range-ts-gt` | search.ts | stdout | 38 | 4 | ACCEPTED | ACCEPTED | DV-15 |
| `range-ts-gte` | search.ts | stdout | 38 | 5 | ACCEPTED | ACCEPTED | DV-15 |
| `range-ts-lt` | search.ts | stdout | 0 | 2 | ACCEPTED | ACCEPTED | DV-15 |
| `eq-ts` | search.ts | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-15 |
| `range-ts-string-column` | timestamps-string.ts | stdout | 17 | 2 | ACCEPTED | ACCEPTED | DV-15 |
| `range-ts-int-precisions` | timestamps-int.ts | stdout | 8 | 5 | ACCEPTED | ACCEPTED | DV-15 |
| `range-ts-not-authoritative` | timestamps-int.default | stdout | 5 | 5 | PASS | PASS |  |
| `clp-substr` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `clp-no-wildcard-substring` | search.ts | stdout | 0 | 0 | PASS | PASS |  |
| `clp-question-one-char` | search.ts | stdout | 2 | 0 | ACCEPTED | ACCEPTED | DV-17 |
| `clp-question-digit` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `clp-escaped-star` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `clp-escaped-question` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `clp-escaped-backslash` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `clp-escaped-quote` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `clp-unicode-substr` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `clp-unicode-escape` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `clp-unquoted-wildcard` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `clp-suffix` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `clp-inner-wildcards` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `clp-inner-star-span` | search.ts | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-17 |
| `clp-inner-star-numbers` | search.ts | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-17 |
| `clp-question-spans-delimiter` | search.ts | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-17 |
| `clp-edge-star-float` | search.ts | stdout | 1 | 2 | ACCEPTED | ACCEPTED | DV-17 |
| `clp-edge-star-float-prefix` | search.ts | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-17 |
| `clp-backslash-before-star` | search.ts | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-17 |
| `clp-backslash-before-inner-star` | search.ts | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-17 |
| `var-prefix` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `var-suffix-both-encodings` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `var-question` | search.ts | stdout | 27 | 27 | PASS | PASS |  |
| `var-exact` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `bare-var` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `bare-var-lowercase` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `bare-clp` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `bare-int` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `bare-int-many-columns` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `bare-float` | search.ts | stdout | 3 | 2 | ACCEPTED | ACCEPTED | DV-11 |
| `bare-bool` | search.ts | stdout | 11 | 11 | PASS | PASS |  |
| `bare-null` | search.ts | stdout | 40 | 5 | ACCEPTED | ACCEPTED | DV-09 |
| `pure-wildcard-column` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `pure-wildcard-column-int` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `pure-wildcard-null` | search.ts | stdout | 40 | 5 | ACCEPTED | ACCEPTED | DV-09 |
| `not-pure-wildcard-null` | search.ts | stdout | 40 | 35 | ACCEPTED | ACCEPTED | DV-09 |
| `not-bare-var` | search.ts | stdout | 40 | 38 | ACCEPTED | ACCEPTED | DV-05 |
| `match-all` | search.ts | stdout | 40 | 40 | PASS | PASS |  |
| `match-all-column` | search.ts | stdout | 40 | 40 | PASS | PASS |  |
| `match-none` | search.ts | stdout | 40 | 0 | ACCEPTED | ACCEPTED | DV-09 |
| `partial-wildcard-deep` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `partial-wildcard-one-level` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `partial-wildcard-zero-levels` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `partial-wildcard-leading` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `partial-wildcard-two-stars` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `partial-wildcard-exists` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `partial-wildcard-range` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `partial-wildcard-nested-string` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `not-partial-wildcard` | search.ts | stdout | 2 | 1 | ACCEPTED | ACCEPTED | DV-08 |
| `exists-poly` | search.ts | stdout | 8 | 9 | ACCEPTED | ACCEPTED | DV-19 |
| `not-exists-poly` | search.ts | stdout | 32 | 31 | ACCEPTED | ACCEPTED | DV-19 |
| `exists-empty-object` | search.ts | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-19 |
| `exists-empty-array` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `exists-array` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `exists-ts` | search.ts | stdout | 38 | 38 | PASS | PASS |  |
| `not-exists-ts` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `not-exists-sparse` | search.ts | stdout | 37 | 37 | PASS | PASS |  |
| `not-exists-absent-path` | search.ts | stdout | 0 | 40 | ACCEPTED | ACCEPTED | DV-07 |
| `exists-absent-path` | search.ts | stdout | 0 | 0 | PASS | PASS |  |
| `bool-left-assoc-1` | search.ts | stdout | 10 | 10 | PASS | PASS |  |
| `bool-left-assoc-2` | search.ts | stdout | 4 | 4 | PASS | PASS |  |
| `bool-parens` | search.ts | stdout | 10 | 10 | PASS | PASS |  |
| `bool-not-binds-tight` | search.ts | stdout | 4 | 4 | PASS | PASS |  |
| `bool-not-group` | search.ts | stdout | 15 | 15 | PASS | PASS |  |
| `bool-double-not` | search.ts | stdout | 6 | 6 | PASS | PASS |  |
| `bool-or-chain` | search.ts | stdout | 4 | 4 | PASS | PASS |  |
| `bool-and-not-mixed` | search.ts | stdout | 4 | 4 | PASS | PASS |  |
| `bool-not-or` | search.ts | stdout | 5 | 5 | PASS | PASS |  |
| `bool-nested-and-not-exists` | search.ts | stdout | 1 | 0 | ACCEPTED | ACCEPTED | DV-19 |
| `bool-contradiction` | search.ts | stdout | 0 | 0 | PASS | PASS |  |
| `not-bool` | search.ts | stdout | 7 | 7 | PASS | PASS |  |
| `not-null` | search.ts | stdout | 2 | 0 | ACCEPTED | ACCEPTED | DV-10 |
| `array-string-elem` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `array-clp-elem` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `array-int-elem` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `array-float-literal` | search.ts | stdout | 2 | 1 | ACCEPTED | ACCEPTED | DV-11 |
| `array-int-text-elem` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `array-quoted-text-elem` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `array-bool-elem` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `array-null-elem` | search.ts | stdout | 3 | 1 | ACCEPTED | ACCEPTED | DV-12 |
| `array-range-gt` | search.ts | stdout | 3 | 2 | ACCEPTED | ACCEPTED | DV-11 |
| `array-range-lt` | search.ts | stdout | 3 | 2 | ACCEPTED | ACCEPTED | DV-11 |
| `array-not-eq` | search.ts | stdout | 3 | 2 | ACCEPTED | ACCEPTED | DV-13 |
| `array-not-range` | search.ts | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-11, DV-13 |
| `array-not-string` | search.ts | stdout | 3 | 1 | ACCEPTED | ACCEPTED | DV-13 |
| `array-object-path` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `array-object-path-int` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `array-object-path-nested-array` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `array-object-path-range` | search.ts | stdout | 2 | 1 | ACCEPTED | ACCEPTED | DV-11 |
| `array-object-path-exists` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `array-object-path-not` | search.ts | stdout | 2 | 0 | ACCEPTED | ACCEPTED | DV-13 |
| `array-object-braces` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `array-object-braces-and` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `array-partial-wildcard` | search.ts | stdout | 0 | 2 | ACCEPTED | ACCEPTED | DV-36 |
| `array-leading-wildcard` | search.ts | stdout | 0 | 2 | ACCEPTED | ACCEPTED | DV-36 |
| `arrays-int` | arrays.default | stdout | 3 | 3 | PASS | PASS |  |
| `arrays-clp-elem` | arrays.default | stdout | 1 | 1 | PASS | PASS |  |
| `arrays-object-elem` | arrays.default | stdout | 2 | 2 | PASS | PASS |  |
| `arrays-deep-object-elem` | arrays.default | stdout | 1 | 1 | PASS | PASS |  |
| `arrays-nested-array-elem` | arrays.default | stdout | 2 | 2 | PASS | PASS |  |
| `arrays-range` | arrays.default | stdout | 8 | 3 | ACCEPTED | ACCEPTED | DV-11 |
| `arrays-null` | arrays.default | stdout | 18 | 3 | ACCEPTED | ACCEPTED | DV-12 |
| `arrays-not` | arrays.default | stdout | 19 | 6 | ACCEPTED | ACCEPTED | DV-13 |
| `arrays-exists` | arrays.default | stdout | 19 | 19 | PASS | PASS |  |
| `arrays-int-max-elem` | arrays.default | stdout | 1 | 1 | PASS | PASS |  |
| `arrays-u64-elem` | arrays.default | stdout | 1 | 1 | PASS | PASS |  |
| `arrays-under-object` | arrays.default | stdout | 1 | 1 | PASS | PASS |  |
| `arrays-recursive-key` | arrays.default | stdout | 1 | 1 | PASS | PASS |  |
| `arrays-escaped-string-elem` | arrays.default | stdout | 1 | 1 | PASS | PASS |  |
| `arrays-float-elem` | arrays.default | stdout | 6 | 2 | ACCEPTED | ACCEPTED | DV-11 |
| `ci-var` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `ci-var-mixed` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `ci-clp-substr` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `ci-array` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `ci-bool-text` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `ci-bare` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `ci-non-ascii` | search.ts | stdout | 0 | 0 | PASS | PASS |  |
| `time-window` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `time-lower-and-filter` | search.ts | stdout | 7 | 7 | PASS | PASS |  |
| `time-upper` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `time-inclusive-edge` | search.ts | stdout | 1 | 1 | PASS | PASS |  |
| `time-past-end` | search.ts | stdout | 0 | 0 | PASS | PASS |  |
| `time-not-exists` | search.ts | stdout | 0 | 0 | PASS | PASS |  |
| `time-int-precisions` | timestamps-int.ts | stdout | 4 | 4 | PASS | PASS |  |
| `time-negative` | timestamps-int.ts | stdout | 2 | 2 | PASS | PASS |  |
| `time-float` | timestamps-float.ts | stdout | 3 | 3 | PASS | PASS |  |
| `time-string-patterns` | timestamps-string.ts | stdout | 2 | 2 | PASS | PASS |  |
| `time-string-patterns-upper` | timestamps-string.ts | stdout | 15 | 15 | PASS | PASS |  |
| `time-nested-key` | timestamps-string.ts-nested | stdout | 1 | 1 | PASS | PASS |  |
| `time-text-literal-on-ts` | timestamps-string.ts | stdout | 0 | 0 | PASS | PASS |  |
| `time-no-ts-key` | search.default | stdout | 0 | 0 | ACCEPTED | ACCEPTED | DV-20 |
| `proj-basic` | search.ts | stdout | 6 | 6 | PASS | PASS |  |
| `proj-nested` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `proj-missing-columns` | search.ts | stdout | 3 | 3 | PASS | PASS |  |
| `proj-poly` | search.ts | stdout | 8 | 9 | ACCEPTED | ACCEPTED | DV-19 |
| `proj-repeated` | search.ts | stdout | 6 | 6 | PASS | PASS |  |
| `proj-all-missing` | search.ts | stdout | 2 | 2 | PASS | PASS |  |
| `meta-filename` | search.ts | stdout | 40 | 40 | PASS | PASS |  |
| `meta-filename-other` | search.ts | stdout | 0 | 0 | PASS | PASS |  |
| `meta-not-filename` | search.ts | stdout | 40 | 0 | ACCEPTED | ACCEPTED | DV-47 |
| `meta-not-filename-and-level` | search.ts | stdout | 2 | 0 | ACCEPTED | ACCEPTED | DV-47 |
| `meta-not-filename-other` | search.ts | stdout | 40 | 40 | PASS | PASS |  |
| `meta-not-exists-filename` | search.ts | stdout | 0 | 0 | PASS | PASS |  |
| `meta-filename-nolog` | search.ts-nolog | stdout | 0 | 0 | PASS | PASS |  |
| `meta-not-exists-filename-nolog` | search.ts-nolog | stdout | 0 | 40 | ACCEPTED | ACCEPTED | DV-47 |
| `floats-formatted-eq` | floats.default | stdout | 2 | 2 | PASS | PASS |  |
| `floats-tiny` | floats.default | stdout | 9 | 9 | PASS | PASS |  |
| `floats-dictionary-float` | floats.default | stdout | 1 | 1 | PASS | PASS |  |
| `floats-signed-zero` | floats.default | stdout | 2 | 2 | PASS | PASS |  |
| `floats-range-huge` | floats.default | stdout | 1 | 1 | PASS | PASS |  |
| `floats-nofloat-eq` | floats.nofloat | stdout | 1 | 1 | ACCEPTED | ACCEPTED | DV-01 |
| `floats-nofloat-range` | floats.nofloat | stdout | 10 | 10 | ACCEPTED | ACCEPTED | DV-01 |
| `floats-array-elem` | floats.default | stdout | 1 | 1 | PASS | PASS |  |
| `scalars-wrapped-u64` | scalars.default | stdout | 2 | 2 | PASS | PASS |  |
| `scalars-out-of-range-literal` | scalars.default | stdout | 0 | 0 | PASS | PASS |  |
| `scalars-text-number` | scalars.default | stdout | 1 | 1 | PASS | PASS |  |
| `scalars-clp-number` | scalars.default | stdout | 1 | 1 | PASS | PASS |  |
| `scalars-nolog-range` | scalars.nolog | stdout | 3 | 3 | PASS | PASS |  |
| `strings-empty` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-space-only` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-unicode-escape` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-cjk` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-emoji-substr` | strings.default | stdout | 2 | 2 | PASS | PASS |  |
| `strings-quote` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-backslash` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-control` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-glob-chars` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-int-literal` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-bool-literal` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-null-literal` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-ci` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-clp-vars` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-clp-var-wildcard` | strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `strings-nolog` | strings.nolog | stdout | 5 | 5 | PASS | PASS |  |
| `keys-escaped-dot` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-nested` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-space` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-star` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-backslash` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-quote` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-at-escaped` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-dollar-escaped` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-unicode` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-keyword` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-escaped-dot-nested` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `keys-empty-key-child` | keys.default | stdout | 1 | 0 | ACCEPTED | ACCEPTED | DV-16 |
| `keys-empty-key-parent-exists` | keys.default | stdout | 3 | 1 | ACCEPTED | ACCEPTED | DV-16 |
| `keys-bare-under-empty-key` | keys.default | stdout | 1 | 1 | PASS | PASS |  |
| `poly-int` | polymorphic.default | stdout | 3 | 3 | PASS | PASS |  |
| `poly-float` | polymorphic.default | stdout | 3 | 2 | ACCEPTED | ACCEPTED | DV-11 |
| `poly-range` | polymorphic.default | stdout | 5 | 5 | PASS | PASS |  |
| `poly-exists` | polymorphic.default | stdout | 14 | 19 | ACCEPTED | ACCEPTED | DV-19 |
| `poly-not-exists` | polymorphic.default | stdout | 6 | 1 | ACCEPTED | ACCEPTED | DV-19 |
| `poly-child-exists` | polymorphic.default | stdout | 2 | 4 | ACCEPTED | ACCEPTED | DV-19, DV-37 |
| `poly-deep-bool` | polymorphic.default | stdout | 1 | 1 | PASS | PASS |  |
| `poly-child-array` | polymorphic.default | stdout | 1 | 2 | ACCEPTED | ACCEPTED | DV-37 |
| `poly-nofloat-float` | polymorphic.nofloat | stdout | 1 | 1 | ACCEPTED | ACCEPTED | DV-01 |
| `poly-tables0` | polymorphic.tables0 | stdout | 2 | 2 | PASS | PASS |  |
| `nested-deep` | nested.default | stdout | 1 | 1 | PASS | PASS |  |
| `nested-empty-object-exists` | nested.default | stdout | 0 | 4 | ACCEPTED | ACCEPTED | DV-19 |
| `nested-poly-leaf` | nested.tables0 | stdout | 2 | 2 | PASS | PASS |  |
| `nested-deep-wildcard` | nested.default | stdout | 1 | 1 | PASS | PASS |  |
| `many-schemas-exists` | many-schemas.default | stdout | 96 | 120 | ACCEPTED | ACCEPTED | DV-19 |
| `many-schemas-clp-wildcard` | many-schemas.default | stdout | 10 | 10 | PASS | PASS |  |
| `many-schemas-poly-text` | many-schemas.tables0 | stdout | 32 | 32 | PASS | PASS |  |
| `many-schemas-nested-range` | many-schemas.tables256 | stdout | 3 | 3 | PASS | PASS |  |
| `many-schemas-array` | many-schemas.nolog | stdout | 1 | 1 | PASS | PASS |  |
| `many-schemas-split-or` | many-schemas.split | stdout | 94 | 125 | ACCEPTED | ACCEPTED | DV-19 |
| `logs-var` | logs.ts | stdout | 283 | 283 | PASS | PASS |  |
| `logs-range` | logs.ts-tables4k | stdout | 42 | 42 | PASS | PASS |  |
| `logs-clp` | logs.ts | stdout | 246 | 246 | PASS | PASS |  |
| `logs-time` | logs.ts-split | stdout | 133 | 133 | PASS | PASS |  |
| `logs-split-var` | logs.ts-split | stdout | 49 | 49 | PASS | PASS |  |
| `logs-split-array` | logs.ts-split | stdout | 104 | 104 | PASS | PASS |  |
| `logs-level1-float` | logs.level1 | stdout | 3 | 3 | PASS | PASS |  |
| `logs-nested` | logs.ts | stdout | 5 | 5 | PASS | PASS |  |
| `kvir-clp` | kvir-text.default | stdout | 2 | 2 | PASS | PASS |  |
| `kvir-bare` | kvir-text.default | stdout | 1 | 1 | PASS | PASS |  |
| `kvir-time` | kvir-text.ts | stdout | 0 | 0 | PASS | PASS |  |
| `long-clp-substr` | long-strings.default | stdout | 1 | 1 | PASS | PASS |  |
| `long-var-prefix` | long-strings.level19 | stdout | 0 | 0 | PASS | PASS |  |
| `clp-negation-and` | clp-negation.default | stdout | 1 | 2 | ACCEPTED | ACCEPTED | DV-06 |
| `clp-negation-not-or` | clp-negation.default | stdout | 1 | 2 | ACCEPTED | ACCEPTED | DV-06 |
| `clp-negation-single` | clp-negation.default | stdout | 3 | 3 | PASS | PASS |  |
| `empty-key-wildcard-leading` | empty-key-wildcard.default | stdout | 1 | 2 | ACCEPTED | ACCEPTED | DV-38 |
| `empty-key-wildcard-partial` | empty-key-wildcard.default | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-38 |
| `empty-key-wildcard-exists` | empty-key-wildcard.default | stdout | 1 | 2 | ACCEPTED | ACCEPTED | DV-38 |
| `empty-key-wildcard-tail` | empty-key-wildcard.default | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-19, DV-38 |
| `empty-key-wildcard-literal` | empty-key-wildcard.default | stdout | 1 | 0 | ACCEPTED | ACCEPTED | DV-16 |
| `empty-key-order-literal` | empty-key-order.default | stdout | 1 | 1 | PASS | PASS |  |
| `empty-key-order-literal-below-empty-key` | empty-key-order.default | stdout | 0 | 0 | PASS | PASS |  |
| `empty-key-order-wildcard` | empty-key-order.default | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-39 |
| `empty-key-order-partial-wildcard` | empty-key-order.default | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-39 |
| `empty-key-order-reversed-literal` | empty-key-order-reversed.default | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-16, DV-39 |
| `empty-key-order-reversed-literal-below-empty-key` | empty-key-order-reversed.default | stdout | 1 | 0 | ACCEPTED | ACCEPTED | DV-16, DV-39 |
| `empty-key-order-reversed-wildcard` | empty-key-order-reversed.default | stdout | 1 | 1 | PASS | PASS |  |
| `time-escaped-key-eq` | timestamps-escaped-key.ts-escaped | stdout | 0 | 1 | ACCEPTED | ACCEPTED | DV-15 |
| `time-escaped-key-tge` | timestamps-escaped-key.ts-escaped | stdout | 2 | 2 | PASS | PASS |  |
| `time-escaped-key-tle` | timestamps-escaped-key.ts-escaped | stdout | 2 | 2 | PASS | PASS |  |
| `time-escaped-key-gt-disjoint` | timestamps-escaped-key.ts-escaped | stdout | 3 | 0 | ACCEPTED | ACCEPTED | DV-15 |
| `time-escaped-key-lt-covering` | timestamps-escaped-key.ts-escaped | stdout | 0 | 3 | ACCEPTED | ACCEPTED | DV-15 |
| `wire-msgpack-search` | search.ts | msgpack | 6 | - | SKIP | SKIP |  |
| `wire-msgpack-nolog` | search.ts-nolog | msgpack | 9 | - | SKIP | SKIP |  |
| `wire-msgpack-kvir` | kvir-text.ts | msgpack | 8 | - | SKIP | SKIP |  |
| `wire-msgpack-split` | logs.ts-split | msgpack | 6 | - | SKIP | SKIP | DV-21 |
| `wire-cache-topk` | logs.ts | results-cache | 10 | - | SKIP | SKIP |  |
| `wire-cache-split` | logs.ts-split | results-cache | 90 | - | SKIP | SKIP | DV-25 |
| `wire-cache-nolog` | search.ts-nolog | results-cache | 1 | - | SKIP | SKIP | DV-24 |
| `bench-rare-clp-substr` | dataset:bench | stdout | 71 | 71 | PASS | PASS |  |
| `bench-rare-int-eq` | dataset:bench | stdout | 1 | 1 | PASS | PASS |  |
| `bench-common-var-eq` | dataset:bench | stdout | 249604 | 249604 | PASS | PASS |  |
| `bench-int-range` | dataset:bench | stdout | 100100 | 100100 | PASS | PASS |  |
| `bench-clp-wildcard-rowlevel` | dataset:bench | stdout | 564 | 564 | PASS | PASS |  |
| `bench-bare-value` | dataset:bench | stdout | 41723 | 41723 | PASS | PASS |  |
| `bench-not-eq` | dataset:bench | stdout | 750568 | 750568 | PASS | PASS |  |
| `bench-ts-range` | dataset:bench | stdout | 9960 | 9960 | PASS | PASS |  |
| `bench-match-all` | dataset:bench | stdout | 1000000 | 1000000 | PASS | PASS |  |
| `bench-not-bare` | dataset:bench | stdout | 1000000 | 750396 | ACCEPTED | ACCEPTED | DV-05 |
| `bench-projection-ci` | dataset:bench | stdout | 21101 | 21101 | PASS | PASS |  |
| `spider-rare-int-range` | dataset:spider | stdout | 40 | 40 | PASS | PASS |  |
| `spider-rare-var-eq` | dataset:spider | stdout | 96 | 96 | PASS | PASS |  |
| `spider-common-var-eq` | dataset:spider | stdout | 480000 | 480000 | PASS | PASS |  |
| `spider-int-range` | dataset:spider | stdout | 4022 | 4022 | PASS | PASS |  |
| `spider-clp-substr` | dataset:spider | stdout | 40000 | 40000 | PASS | PASS |  |
| `spider-bare-value` | dataset:spider | stdout | 97323 | 97323 | PASS | PASS |  |
| `spider-not-eq` | dataset:spider | stdout | 577323 | 577323 | PASS | PASS |  |
| `spider-ts-range` | dataset:spider | stdout | 10696 | 10696 | PASS | PASS |  |
| `spider-match-all` | dataset:spider | stdout | 1057323 | 1057323 | PASS | PASS |  |
| `spider-partial-wildcard` | dataset:spider | stdout | 20 | 20 | PASS | PASS |  |
| `synthetic-rare-clp-substr` | dataset:synthetic | stdout | 5 | 5 | PASS | PASS |  |
| `synthetic-rare-nested-int-eq` | dataset:synthetic | stdout | 9 | 9 | PASS | PASS |  |
| `synthetic-common-var-eq` | dataset:synthetic | stdout | 82762 | 82762 | PASS | PASS |  |
| `synthetic-poly-range` | dataset:synthetic | stdout | 93444 | 30781 | ACCEPTED | ACCEPTED | DV-11 |
| `synthetic-array-search` | dataset:synthetic | stdout | 84087 | 84087 | PASS | PASS |  |
| `synthetic-clp-wildcard-rowlevel` | dataset:synthetic | stdout | 26937 | 26937 | PASS | PASS |  |
| `synthetic-bare-value` | dataset:synthetic | stdout | 25008 | 25008 | PASS | PASS |  |
| `synthetic-not-eq` | dataset:synthetic | stdout | 249594 | 249594 | PASS | PASS |  |
| `synthetic-ts-range` | dataset:synthetic | stdout | 5000 | 5000 | PASS | PASS |  |
| `synthetic-match-all` | dataset:synthetic | stdout | 500000 | 500000 | PASS | PASS |  |
| `synthetic-array-exists-not` | dataset:synthetic | stdout | 14922 | 14922 | PASS | PASS |  |

## Query performance (first look)

Release builds, `taskset -c 8-15`, warm cache, one warm-up then 5 runs, median [min-max] wall time
and peak RSS. `DevNull` writes stdout to `/dev/null`, `File` to a file; `x` writes the
`original` file. Serial (`Executor::CurrentThread`) Rust against single-threaded C++ `clp-s`; no
performance gate this round, and a more careful benchmark follows. Raw results:
`$PROTO_DATA_ROOT/perf/2026-10-18.json`.

| case | sink | C++ wall s (median [min-max]) | C++ RSS MiB | Rust wall s (median [min-max]) | Rust RSS MiB | Rust/C++ |
|---|---|---|---|---|---|---|
| x:bench | Directory | 0.553 [0.540-0.567] | 118.8 | 0.324 [0.314-0.343] | 102.8 | 0.59 |
| x:spider | Directory | 0.758 [0.751-0.805] | 86.2 | 0.440 [0.439-0.466] | 71.2 | 0.58 |
| x:synthetic | Directory | 0.830 [0.819-0.857] | 58.0 | 0.501 [0.485-0.511] | 48.7 | 0.60 |
| bench-rare-clp-substr | DevNull | 0.115 [0.115-0.115] | 121.0 | 0.100 [0.099-0.102] | 103.0 | 0.87 |
| bench-rare-clp-substr | File | 0.115 [0.114-0.118] | 121.0 | 0.101 [0.099-0.103] | 103.0 | 0.88 |
| bench-rare-int-eq | DevNull | 0.105 [0.105-0.112] | 120.8 | 0.086 [0.086-0.088] | 103.0 | 0.82 |
| bench-rare-int-eq | File | 0.104 [0.103-0.104] | 121.0 | 0.085 [0.085-0.087] | 103.0 | 0.82 |
| bench-common-var-eq | DevNull | 0.209 [0.208-0.210] | 120.8 | 0.138 [0.135-0.140] | 103.0 | 0.66 |
| bench-common-var-eq | File | 0.237 [0.230-0.243] | 121.0 | 0.150 [0.148-0.151] | 103.0 | 0.63 |
| bench-int-range | DevNull | 0.146 [0.144-0.148] | 121.0 | 0.112 [0.109-0.117] | 103.0 | 0.77 |
| bench-int-range | File | 0.157 [0.155-0.162] | 121.0 | 0.118 [0.117-0.121] | 103.0 | 0.75 |
| bench-clp-wildcard-rowlevel | DevNull | 0.116 [0.115-0.118] | 121.0 | 0.098 [0.097-0.099] | 103.0 | 0.84 |
| bench-clp-wildcard-rowlevel | File | 0.115 [0.115-0.116] | 121.2 | 0.100 [0.099-0.103] | 103.0 | 0.87 |
| bench-bare-value | DevNull | 0.174 [0.173-0.180] | 119.2 | 0.105 [0.103-0.105] | 103.0 | 0.60 |
| bench-bare-value | File | 0.177 [0.176-0.178] | 119.0 | 0.111 [0.108-0.115] | 103.0 | 0.63 |
| bench-not-eq | DevNull | 0.379 [0.373-0.387] | 121.0 | 0.215 [0.214-0.222] | 103.0 | 0.57 |
| bench-not-eq | File | 0.456 [0.448-0.459] | 121.2 | 0.270 [0.267-0.282] | 103.0 | 0.59 |
| bench-ts-range | DevNull | 0.111 [0.109-0.114] | 122.0 | 0.091 [0.089-0.091] | 103.0 | 0.82 |
| bench-ts-range | File | 0.111 [0.110-0.112] | 122.0 | 0.091 [0.090-0.094] | 103.0 | 0.82 |
| bench-match-all | DevNull | 0.475 [0.465-0.476] | 119.0 | 0.258 [0.246-0.260] | 103.0 | 0.54 |
| bench-match-all | File | 0.549 [0.544-0.573] | 119.0 | 0.327 [0.325-0.344] | 103.0 | 0.60 |
| bench-not-bare | DevNull | 0.459 [0.452-0.463] | 119.0 | 0.214 [0.212-0.221] | 103.0 | 0.47 |
| bench-not-bare | File | 0.556 [0.550-0.589] | 119.0 | 0.267 [0.264-0.272] | 103.0 | 0.48 |
| bench-projection-ci | DevNull | 0.116 [0.116-0.118] | 122.0 | 0.091 [0.090-0.091] | 102.8 | 0.78 |
| bench-projection-ci | File | 0.117 [0.117-0.118] | 122.0 | 0.092 [0.090-0.099] | 102.8 | 0.78 |
| spider-rare-int-range | DevNull | 0.052 [0.051-0.052] | 73.5 | 0.041 [0.041-0.041] | 57.0 | 0.79 |
| spider-rare-int-range | File | 0.052 [0.051-0.054] | 73.5 | 0.041 [0.040-0.041] | 57.0 | 0.78 |
| spider-rare-var-eq | DevNull | 0.095 [0.094-0.096] | 87.8 | 0.081 [0.080-0.081] | 71.1 | 0.85 |
| spider-rare-var-eq | File | 0.094 [0.094-0.095] | 88.0 | 0.080 [0.078-0.082] | 71.1 | 0.85 |
| spider-common-var-eq | DevNull | 0.352 [0.346-0.354] | 87.8 | 0.202 [0.198-0.208] | 71.5 | 0.57 |
| spider-common-var-eq | File | 0.426 [0.418-0.428] | 87.8 | 0.267 [0.259-0.269] | 71.6 | 0.63 |
| spider-int-range | DevNull | 0.073 [0.073-0.075] | 87.5 | 0.059 [0.058-0.059] | 71.0 | 0.80 |
| spider-int-range | File | 0.075 [0.074-0.076] | 87.8 | 0.060 [0.059-0.061] | 71.0 | 0.81 |
| spider-clp-substr | DevNull | 0.125 [0.123-0.132] | 87.8 | 0.097 [0.096-0.099] | 71.6 | 0.78 |
| spider-clp-substr | File | 0.128 [0.126-0.129] | 87.8 | 0.105 [0.102-0.106] | 71.5 | 0.82 |
| spider-bare-value | DevNull | 0.175 [0.172-0.184] | 86.8 | 0.109 [0.102-0.109] | 71.1 | 0.62 |
| spider-bare-value | File | 0.196 [0.192-0.206] | 86.8 | 0.112 [0.110-0.114] | 71.2 | 0.57 |
| spider-not-eq | DevNull | 0.352 [0.351-0.355] | 87.8 | 0.201 [0.200-0.202] | 71.2 | 0.57 |
| spider-not-eq | File | 0.437 [0.430-0.443] | 87.8 | 0.273 [0.265-0.280] | 71.2 | 0.62 |
| spider-ts-range | DevNull | 0.103 [0.101-0.104] | 88.5 | 0.082 [0.081-0.083] | 71.6 | 0.80 |
| spider-ts-range | File | 0.104 [0.103-0.105] | 88.0 | 0.084 [0.084-0.086] | 71.6 | 0.80 |
| spider-match-all | DevNull | 0.610 [0.608-0.634] | 86.8 | 0.317 [0.312-0.322] | 71.5 | 0.52 |
| spider-match-all | File | 0.798 [0.762-0.815] | 86.8 | 0.445 [0.437-0.448] | 71.6 | 0.56 |
| spider-partial-wildcard | DevNull | 0.101 [0.100-0.107] | 88.2 | 0.083 [0.082-0.084] | 71.1 | 0.82 |
| spider-partial-wildcard | File | 0.101 [0.100-0.103] | 88.2 | 0.082 [0.082-0.085] | 71.2 | 0.81 |
| synthetic-rare-clp-substr | DevNull | 0.160 [0.156-0.160] | 64.8 | 0.134 [0.131-0.142] | 51.6 | 0.84 |
| synthetic-rare-clp-substr | File | 0.159 [0.156-0.160] | 64.8 | 0.135 [0.132-0.135] | 51.8 | 0.85 |
| synthetic-rare-nested-int-eq | DevNull | 0.147 [0.146-0.153] | 65.2 | 0.126 [0.125-0.130] | 51.9 | 0.86 |
| synthetic-rare-nested-int-eq | File | 0.145 [0.145-0.147] | 65.2 | 0.127 [0.127-0.130] | 51.9 | 0.88 |
| synthetic-common-var-eq | DevNull | 0.274 [0.273-0.305] | 64.8 | 0.199 [0.198-0.201] | 51.6 | 0.73 |
| synthetic-common-var-eq | File | 0.285 [0.281-0.288] | 64.8 | 0.212 [0.209-0.219] | 51.6 | 0.74 |
| synthetic-poly-range | DevNull | 0.265 [0.262-0.267] | 61.5 | 0.137 [0.135-0.140] | 43.9 | 0.52 |
| synthetic-poly-range | File | 0.294 [0.285-0.308] | 61.8 | 0.142 [0.140-0.146] | 43.9 | 0.48 |
| synthetic-array-search | DevNull | 0.384 [0.376-0.386] | 61.5 | 0.215 [0.212-0.218] | 47.5 | 0.56 |
| synthetic-array-search | File | 0.390 [0.385-0.407] | 61.5 | 0.230 [0.225-0.242] | 47.5 | 0.59 |
| synthetic-clp-wildcard-rowlevel | DevNull | 0.254 [0.246-0.260] | 64.8 | 0.191 [0.189-0.195] | 51.6 | 0.75 |
| synthetic-clp-wildcard-rowlevel | File | 0.255 [0.252-0.256] | 64.8 | 0.195 [0.195-0.198] | 51.6 | 0.77 |
| synthetic-bare-value | DevNull | 0.311 [0.304-0.312] | 63.8 | 0.270 [0.268-0.275] | 57.6 | 0.87 |
| synthetic-bare-value | File | 0.312 [0.310-0.321] | 63.8 | 0.276 [0.269-0.284] | 57.6 | 0.88 |
| synthetic-not-eq | DevNull | 0.492 [0.486-0.498] | 65.0 | 0.303 [0.297-0.305] | 51.6 | 0.62 |
| synthetic-not-eq | File | 0.523 [0.515-0.539] | 65.0 | 0.334 [0.329-0.347] | 51.6 | 0.64 |
| synthetic-ts-range | DevNull | 0.193 [0.190-0.199] | 74.5 | 0.141 [0.140-0.142] | 54.8 | 0.73 |
| synthetic-ts-range | File | 0.208 [0.198-0.209] | 74.5 | 0.145 [0.144-0.146] | 54.8 | 0.70 |
| synthetic-match-all | DevNull | 0.786 [0.785-0.820] | 63.2 | 0.436 [0.433-0.446] | 48.7 | 0.56 |
| synthetic-match-all | File | 0.849 [0.840-0.877] | 63.2 | 0.493 [0.492-0.505] | 48.7 | 0.58 |
| synthetic-array-exists-not | DevNull | 0.105 [0.105-0.106] | 63.0 | 0.060 [0.058-0.061] | 42.9 | 0.57 |
| synthetic-array-exists-not | File | 0.109 [0.108-0.110] | 62.8 | 0.062 [0.061-0.063] | 42.9 | 0.57 |

Across the 67 measurements Rust takes 0.47-0.88x the C++ wall time (full scans and common matches
0.5-0.6x, selective queries dominated by open and dictionary costs 0.8-0.88x) and 10-30% less peak
RSS.
