# Deploying CLP compression tasks into a Spider executor image

`clp-tdl-package` is a Spider TDL package (a `cdylib`) that registers CLP's compression tasks —
`compression::clp_s_compress` and `compression::clp_s_commit` (package name **`clp`**). At runtime a
Spider **task-executor** `dlopen`s it and invokes the tasks, which in turn shell out to the `clp-s`
and `indexer` binaries and talk to S3 and the CLP metadata DB.

This guide lists everything the executor image needs and how to bake it in. It's the reference for
standing up a CLP-capable Spider worker image (used by the e2e test harness).

## What the executor needs

| Dependency | Where it comes from | In-container location |
|---|---|---|
| TDL package `.so` | `cargo build -p clp-tdl-package` -> `target/{release,debug}/libclp_tdl_package.so` | `${SPIDER_TDL_PACKAGE_DIR}/clp/libclp.so` |
| `clp-s`, `indexer` | CLP core build -> `build/core/{clp-s,indexer}` | `$CLP_HOME/bin/{clp-s,indexer}` |
| Runtime shared libs | apt: `ca-certificates libcurl4 libmariadb3 libssl3 libstdc++6` | system paths |
| `libzstd.so.1` (1.5.7) | CLP core's build deps (`/usr/local/lib/libzstd.so.1.5.7`); apt jammy only ships 1.4.8 | `/usr/local/lib` (+ `ldconfig`) |
| Executor config | a `SpiderTaskExecutorConfig` YAML (see `clp-rust-utils` `clp_config::package`) | `$CLP_CONFIG_PATH` |
| Writable staging/tmp dirs | created in the image or bind-mounted | `staging_directory` / `tmp_directory` (see below) |

**Package resolution.** The executor resolves a package by name as
`${SPIDER_TDL_PACKAGE_DIR}/<pkg>/lib<pkg>.so`. The execution manager sets `SPIDER_TDL_PACKAGE_DIR`
(the e2e stack uses `/spider/packages`), so package `clp` must live at
`/spider/packages/clp/libclp.so` — note the file is `libclp.so` (from the package name), **not**
`libclp_tdl_package.so` (the crate's output name); rename on copy.

## Environment (read by the task code)

Set these on the **execution-manager** process; the EM spawns the task-executor with
`std::process::Command` and never `env_clear()`s, so the child (and the `clp-s`/`indexer` it spawns)
inherit them. No `inherited_env` / config regeneration is required.

| Env var | Read by | Purpose |
|---|---|---|
| `CLP_HOME` | `clp_binary_path`; also the root for relative `staging_directory`/`tmp_directory` | `$CLP_HOME/bin/{clp-s,indexer}` |
| `CLP_CONFIG_PATH` | the config loader | path to the `SpiderTaskExecutorConfig` YAML |
| `CLP_DB_USER` / `CLP_DB_PASS` | the commit task; inherited by `indexer` | CLP metadata-DB credentials |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` | the Rust AWS SDK (archive upload) + `clp-s` (S3 reads) | S3 access |

`staging_directory` and `tmp_directory` in the config are resolved **relative to `CLP_HOME`** at
load (absolute paths are left as-is), mirroring `clp_py_utils`. They must exist and be writable by
the executor's user.

## Sample Dockerfile (derive from the base Spider worker)

The base Spider `worker` image is a bare `ubuntu:jammy` (glibc 2.35) with no extra libraries. `clp-s`
and `indexer` need only GLIBC_2.34 / GLIBCXX_3.4.30, so they run there once their libraries are
present. Build the CLP binaries + `.so` first, then:

```dockerfile
ARG BASE=spider-worker:latest
FROM ${BASE}
USER root

# Runtime libs. libcurl4 transitively pulls gnutls/ldap/krb5/brotli/nghttp2/psl/rtmp/ssh/idn2/... .
# ca-certificates is REQUIRED: without the system CA bundle the Rust AWS SDK (rustls native roots)
# panics building its TLS trust store, and clp-s's libcurl can't verify S3 over TLS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates libcurl4 libmariadb3 libssl3 libstdc++6 \
    && rm -rf /var/lib/apt/lists/*

# clp-s links libzstd 1.5.7 (its build-time dep); apt jammy only has 1.4.8. Ship 1.5.7.
COPY libzstd.so.1.5.7 /usr/local/lib/
RUN ln -sf libzstd.so.1.5.7 /usr/local/lib/libzstd.so.1 && ldconfig

# CLP binaries + the TDL package (renamed to lib<pkg>.so).
COPY clp-s indexer /clp/bin/
COPY libclp_tdl_package.so /spider/packages/clp/libclp.so
RUN chmod +x /clp/bin/clp-s /clp/bin/indexer

USER spider-user   # or whatever non-root user the base image runs as
```

Build the inputs first: `task core` (or the CLP core build) produces `build/core/{clp-s,indexer}`;
`cargo build -p clp-tdl-package` produces the `.so` (prefer `--release`: the debug `.so` is ~300 MB
and slow to load); copy `libzstd.so.1.5.7` from the CLP core build's `/usr/local/lib`.

## Gotchas

- **`ca-certificates` is mandatory** — the #1 silent failure (executor panics on TLS before any task
  logic runs).
- **libzstd must be 1.5.7**, not apt's 1.4.8 (same SONAME, but clp-s uses newer symbols).
- **Prefer a release `.so`** — the debug build is ~300 MB.
- **The executor is a pooled, long-lived process.** Swapping the binaries or `.so` on disk does not
  affect already-spawned executors; restart the workers so the EM respawns them.
- **glibc/ABI**: build `clp-s`/`indexer` and the `.so` against a base compatible with the worker
  image's glibc (jammy = 2.35). The binaries here need only GLIBC_2.34 / GLIBCXX_3.4.30.
- **Task-side logs**: the `.so` installs no `tracing` subscriber, so the task's own `tracing` events
  are dropped. `clp-s` stderr is surfaced in the task error (and thus the Spider job error);
  `indexer` stderr and panics go to the executor's stderr (the EM writes it under its `log_dir`).
