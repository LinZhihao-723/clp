# clp-s C API harness

A self-contained test harness that, **in a single process**, compresses sample JSON logs into a
clp-s archive and then searches it, using **only** the C API exported by the prebuilt
`libclp-s.so`. Nothing shells out to the `clp-s` binary: both compression and search go through
`clp_s_compress` and `clp_s_search` (declared in `clp_s/capi.h`).

The harness is written in plain C to prove the C ABI is usable from a non-C++ consumer.

## Files

- `sample_logs.jsonl` — 10 newline-delimited JSON log records (`level`, `service`, `id`,
  `message`), designed so each test query has a known, deterministic match count.
- `harness.c` — the C program: cleans a fresh archives dir, calls `clp_s_compress`, runs 11
  `clp_s_search` queries (exact / wildcard / numeric / AND / OR / NOT / zero-match), asserts each
  observed count equals the expected count, and prints per-query `PASS`/`FAIL` plus a summary. Exits
  non-zero if any assertion fails.
- `CMakeLists.txt` — standalone build. Wraps the prebuilt `libclp-s.so` in an IMPORTED SHARED
  target and bakes an RPATH so the harness finds the `.so` at runtime. This is an **external
  consumer** build and is intentionally NOT part of the core CMake tree / `task core`.

## Build & run (one command each)

```sh
cd /home/lzh/dev/clp/components/core/tools/clp-s-c-api-harness

# Build
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build

# Run (must run from this dir so the default relative paths resolve)
./build/harness sample_logs.jsonl ./harness-work/archives
```

`harness [input_jsonl] [archives_dir]` — both arguments are optional and default to
`sample_logs.jsonl` and `./harness-work/archives`.

If your CLP build tree lives elsewhere, override the library/header locations at configure time:

```sh
cmake -S . -B build \
  -DCLP_S_LIB_DIR=/path/to/build/core \
  -DCLP_S_INCLUDE_DIR=/path/to/components/core/src
```

## Runtime dependencies

- `libclp-s.so` — found via the RPATH baked into the executable (`/home/lzh/dev/clp/build/core`).
- `libarchive.so.18` — found via the system loader path (`/usr/local/lib`).

## Notes

- A clp-s archive is a **directory** of multiple files (`schema_tree`, `var.dict`, `log.dict`,
  `table_metadata`, etc.) named by a UUID under `archives_dir`.
- KQL quick reference used here: `field: "value"` exact match, `field: "*substr*"` wildcard,
  `field > N` / `field < N` numeric comparison, combined with `AND` / `OR` / `NOT`.
