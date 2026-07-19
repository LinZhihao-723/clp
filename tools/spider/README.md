# Running CLP compression on a local Spider cluster

This guide stands up a local **Spider (Huntsman)** cluster plus a **CLP metadata DB**, with a
**CLP-capable worker image**, so CLP's `clp-s` compression/commit tasks run as Spider TDL tasks
end-to-end. It is written to be reproducible from scratch by a fresh session.

It complements two other docs, which remain authoritative for their halves:

- **[`components/clp-tdl-package/README.md`](../../components/clp-tdl-package/README.md)** — *what* CLP
  dependencies the executor image needs and *why* (binaries, shared libs, env, package resolution).
- **Spider base compose** — `tools/docker/README.md` in the Spider repo
  (<https://github.com/LinZhihao-723/spider/blob/integration-test-dev-compose/tools/docker/README.md>)
  — how the base cluster (MariaDB / storage / scheduler / workers) is built and generated.

This doc is the glue: how the CLP side layers onto that base cluster.

> **Never commit secrets.** AWS keys and DB passwords are supplied through the shell environment at
> bring-up time and through a local (untracked) config file. The templates below use placeholders.

---

## 1. Topology

Everything runs on one user-defined bridge network, **`spider_net`** (`172.40.0.0/24`), created by the
base Spider compose project (`spider-e2e`). The CLP metadata DB attaches to that existing network.

| Service | In-cluster address | Host-published port | Notes |
|---|---|---|---|
| MariaDB (Spider's own storage DB) | `172.40.0.20:3306` | `127.0.0.1:3307` | base compose |
| Storage gRPC | `172.40.0.30:50051` | `127.0.0.1:50051` | base compose |
| Scheduler | `172.40.0.40` | — | base compose |
| Workers (execution managers) | dynamic IPs | — | 8 workers, run the **CLP** image |
| **CLP metadata DB (`clp-db`)** | `172.40.0.21:3306` | `127.0.0.1:3306` | CLP side (this guide) |

**Two compose projects, one network:**
- `spider-e2e` — the base cluster + the CLP worker overlay (order matters, overlay second).
- `clp-db` — the standalone CLP metadata DB, attached to `spider-e2e_spider_net` as `external`.

> **WSL2 / host access:** the host generally cannot reach the bridge IPs (`172.40.0.x`) directly. Use
> the **host-published** ports above (`127.0.0.1:...`) when driving the cluster from the host (e.g. the
> `e2e_compress` example). In-cluster clients keep using the bridge IPs.

---

## 2. Prerequisites

- **Docker** with the Compose plugin.
- Two repo checkouts:
  - **CLP** — this repo (referred to below as `$CLP_REPO`, e.g. `~/dev/clp`).
  - **Spider** — the fork, branch `integration-test-dev-compose` (`$SPIDER_REPO`, e.g. `~/dev/spider-e2e`).
- **CLP core binaries** built: `clp-s` and `indexer` at `$CLP_REPO/build/core/{clp-s,indexer}`
  (`task core` — see the core build docs; do **not** invoke cmake directly).
- **`libzstd.so.1.5.7`** — from the CLP core build's deps, typically at `/usr/local/lib/libzstd.so.1`
  (a symlink to `...1.5.7`). `clp-s` links 1.5.7; apt jammy only ships 1.4.8.
- **The CLP TDL package `.so`**: `cargo build --release -p clp-tdl-package`
  → `$CLP_REPO/target/release/libclp_tdl_package.so` (prefer release; the debug `.so` is ~300 MB).
- **Spider release binaries** for the base images: `task build:rust` in `$SPIDER_REPO` (the docker
  build does this for you if you build the images via compose).

> **Post-#409 (init hook) requirement:** the CLP `.so` now relies on the executor invoking the
> package **init hook** at load (`__spider_tdl_package_init`) to initialize its Tokio runtime, config,
> and tracing subscriber. That dispatch only exists in the **merged** executor (Spider PR #409). Build
> the worker image from a `$SPIDER_REPO` checkout that includes #409 (branch tip), or the accessors
> will panic on first task. The CLP git-dep in `Cargo.toml` must point at the same merged commit.

---

## 3. Build the CLP artifacts

```sh
# CLP binaries -> build/core/{clp-s,indexer}
cd "$CLP_REPO"
task core

# TDL package .so (release) -> target/release/libclp_tdl_package.so
cargo build --release -p clp-tdl-package

# Sanity: the binaries run and the .so exports the 4 FFI symbols (incl. the init hook)
./build/core/clp-s c --help >/dev/null && echo "clp-s OK"
nm -D target/release/libclp_tdl_package.so | grep __spider_tdl_package_
#  -> __spider_tdl_package_{execute,get_name,get_version,init}
```

---

## 4. Build the CLP-capable worker image

The workers run a **derived** image, `spider-e2e-worker-clp:latest`, that bakes `clp-s`, `indexer`,
their runtime shared libs, and `libzstd 1.5.7` on top of the base Spider worker image. (The `.so` and
the CLP config are **not** baked — they iterate during development and are bind-mounted / staged.)

First build the base Spider images (see the Spider docker README), which produces
`spider-e2e-worker-1:latest`. Then build the derived image from this Dockerfile:

```dockerfile
# tools/spider/worker-clp.Dockerfile
ARG BASE=spider-e2e-worker-1:latest
FROM ${BASE}
USER root

# ca-certificates is REQUIRED: without the system CA bundle the Rust AWS SDK (rustls native roots)
# panics building its TLS trust store, and clp-s's libcurl can't verify S3 over TLS. libcurl4 pulls
# in the long tail of transitive libs (gnutls/ldap/krb5/brotli/nghttp2/...).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates libcurl4 libmariadb3 libssl3 libstdc++6 \
    && rm -rf /var/lib/apt/lists/*

# clp-s links libzstd 1.5.7; apt jammy ships only 1.4.8. Ship 1.5.7 and let ldconfig index it.
COPY libzstd.so.1.5.7 /usr/local/lib/
RUN ln -sf libzstd.so.1.5.7 /usr/local/lib/libzstd.so.1 \
    && ln -sf libzstd.so.1 /usr/local/lib/libzstd.so \
    && ldconfig

# clp-s + indexer at $CLP_HOME/bin (the overlay sets CLP_HOME=/clp).
COPY clp-s indexer /clp/bin/
RUN chmod +x /clp/bin/clp-s /clp/bin/indexer
USER spider-user
```

Build it with a context holding the three copied files:

```sh
ctx="$(mktemp -d)"
cp "$CLP_REPO/build/core/clp-s" "$CLP_REPO/build/core/indexer" "$ctx/"
cp "$(readlink -f /usr/local/lib/libzstd.so.1)" "$ctx/libzstd.so.1.5.7"
cp "$CLP_REPO/tools/spider/worker-clp.Dockerfile" "$ctx/Dockerfile"
docker build --build-arg BASE=spider-e2e-worker-1:latest -t spider-e2e-worker-clp:latest "$ctx"
rm -rf "$ctx"
```

> Rebuild this image whenever `clp-s`/`indexer` change **or** the base Spider images change (e.g. after
> merging new upstream spider — the executor binary lives in the base image).

See `components/clp-tdl-package/README.md` for the full dependency table and rationale.

---

## 5. CLP config + environment

### 5a. Executor config (`clp-config.yaml`)

Mounted read-only at `$CLP_CONFIG_PATH` inside each worker; deserialized into
`SpiderTaskExecutorConfig`. Only `package`, `archive_output`, `tmp_directory`, and `database` are read.

```yaml
package:
  storage_engine: "clp-s"          # structured (clp-s) compression path

archive_output:
  storage:
    type: "s3"
    # RELATIVE to CLP_HOME -> resolves to /clp/var/data/staged-archives (a writable bind mount).
    # clp-s writes single-file archives here before upload, then deletes them.
    staging_directory: "var/data/staged-archives"
    s3_config:
      bucket: "<your-archive-bucket>"
      region_code: "<your-region>"          # e.g. us-east-2
      key_prefix: "<prefix>/archives/"       # object key = key_prefix + <dataset>/<archive_id>; keep the trailing slash
      # endpoint_url omitted => default AWS endpoint. Set it only for MinIO/custom S3.
      aws_authentication:
        type: "credentials"
        credentials:
          access_key_id: "<AWS_ACCESS_KEY_ID>"        # DO NOT COMMIT real keys
          secret_access_key: "<AWS_SECRET_ACCESS_KEY>"
  # These target sizes are NOT read by the task (clp-s gets --target-encoded-size / --compression-level
  # from the per-job ClpSCompressionOption). Kept for schema validity.
  target_archive_size: 268435456
  target_dictionaries_size: 33554432
  target_encoded_file_size: 268435456
  target_segment_size: 268435456
  compression_level: 3
  retention_period: null

tmp_directory: "var/tmp"           # RELATIVE to CLP_HOME -> /clp/var/tmp (writable bind mount); clp-s --files-from list

database:
  type: "mariadb"
  host: "172.40.0.21"              # the clp-db container on spider_net (see §6)
  port: 3306
  names:
    clp: "clp-db"                  # the copied CLP metadata DB name
    spider: "spider-db"
```

> **Relative `staging_directory` / `tmp_directory`** are joined with `CLP_HOME` at runtime (mirroring
> `clp_py_utils.core.make_config_path_absolute`), so keep them relative and bind-mount the resolved
> targets writable (see the overlay in §6). The dirs must be owned by uid 1000 (the container's
> `spider-user`) so `clp-s` can write.

### 5b. Environment (exported in your shell before bring-up)

The execution manager spawns the task-executor with `std::process::Command` and never `env_clear`s, so
the executor (and the `clp-s`/`indexer` it spawns) **inherit** this env — no `inherited_env` config is
needed.

| Var | Purpose |
|---|---|
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` | S3 access (Rust AWS SDK for archive upload + `clp-s` S3 reads) |
| `CLP_DB_USER` / `CLP_DB_PASS` | CLP metadata-DB credentials (the commit task + `indexer`) |
| `SPIDER_RUST_LOG` (optional) | `RUST_LOG` for the EM/executor (default `info`) |

```sh
export AWS_ACCESS_KEY_ID=...      AWS_SECRET_ACCESS_KEY=...
export CLP_DB_USER=clp-user       CLP_DB_PASS=...            # any consistent value; the DB is created with it
```

`CLP_HOME` (`/clp`) and `CLP_CONFIG_PATH` (`/clp/etc/clp-config.yaml`) are set by the overlay, not the
shell.

---

## 6. The CLP worker overlay

Layer this compose file **onto** the base e2e compose (this file *second*). It swaps the workers to the
CLP image, injects the CLP/AWS env, and bind-mounts the config + writable dirs + a host log dir. Adjust
the host paths to your checkout.

```yaml
# tools/spider/docker-compose.harness.yml
x-clp-worker: &clp-worker
  image: spider-e2e-worker-clp:latest
  environment:
    CLP_HOME: /clp
    CLP_CONFIG_PATH: /clp/etc/clp-config.yaml
    RUST_LOG: ${SPIDER_RUST_LOG:-info}
    CLP_DB_USER: ${CLP_DB_USER:?export CLP_DB_USER}
    CLP_DB_PASS: ${CLP_DB_PASS:?export CLP_DB_PASS}
    AWS_ACCESS_KEY_ID: ${AWS_ACCESS_KEY_ID:?export AWS_ACCESS_KEY_ID}
    AWS_SECRET_ACCESS_KEY: ${AWS_SECRET_ACCESS_KEY:?export AWS_SECRET_ACCESS_KEY}
  volumes:
    - <CLP_HARNESS>/clp-config.yaml:/clp/etc/clp-config.yaml:ro
    - <CLP_HARNESS>/var/data/staged-archives:/clp/var/data/staged-archives:rw
    - <CLP_HARNESS>/var/tmp:/clp/var/tmp:rw
    # Each executor's stderr (clp-s + indexer output, task panics, and — post-#409 — the package's
    # own tracing) lands here: <log_dir>/<em_id>-<executor_id>.log. em_id is unique per worker.
    - <CLP_HARNESS>/logs/em-logs:/home/spider-user/spider-run/em-logs:rw

services:
  worker-1: *clp-worker
  worker-2: *clp-worker
  worker-3: *clp-worker
  worker-4: *clp-worker
  worker-5: *clp-worker
  worker-6: *clp-worker
  worker-7: *clp-worker
  worker-8: *clp-worker
```

**Stage the `.so`** into the base compose's existing `build/tdl_packages` → `/spider/packages` mount.
The executor resolves package `clp` as `${SPIDER_TDL_PACKAGE_DIR}/clp/libclp.so`, so the file must be
named `libclp.so` (from the package name), **not** `libclp_tdl_package.so`:

```sh
install -D -m0644 "$CLP_REPO/target/release/libclp_tdl_package.so" \
    "$SPIDER_REPO/build/tdl_packages/clp/libclp.so"
mkdir -p <CLP_HARNESS>/var/data/staged-archives <CLP_HARNESS>/var/tmp <CLP_HARNESS>/logs/em-logs
```

---

## 7. The CLP metadata DB (`clp-db`)

A standalone MariaDB holding the copied CLP metadata DB (`compression_jobs`,
`ingested_s3_object_metadata`, `<prefix><dataset>_archives`, `clp_datasets`, ...), loaded from a SQL
snapshot on first init. It attaches to the network the base cluster creates.

```yaml
# tools/spider/clp-db.compose.yml
services:
  clp-db:
    image: mariadb:10.11            # match the snapshot's source MariaDB version
    container_name: clp-db
    restart: unless-stopped
    networks:
      spider-e2e_spider_net:
        ipv4_address: 172.40.0.21
    ports:
      - "3306:3306"                 # host access at 127.0.0.1:3306
    environment:
      MYSQL_DATABASE: "clp-db"
      MYSQL_USER: "${CLP_DB_USER:-clp-user}"
      MYSQL_PASSWORD: "${CLP_DB_PASS:?export CLP_DB_PASS}"
      MYSQL_ROOT_PASSWORD: "${CLP_DB_ROOT_PASSWORD:-clp-root-password}"
    volumes:
      # First-init only: loaded when the data dir is empty. `down -v` drops the volume -> reload.
      - <CLP_HARNESS>/clp-db-snapshot.sql:/docker-entrypoint-initdb.d/01-clp-db-snapshot.sql:ro
    healthcheck:
      test: ["CMD", "healthcheck.sh", "--connect", "--innodb_initialized"]
      interval: 2s
      timeout: 5s
      retries: 30
      start_period: 10s

networks:
  spider-e2e_spider_net:            # created by the base cluster (project spider-e2e); attach, don't create
    external: true
```

> The snapshot user/password come from `CLP_DB_USER`/`CLP_DB_PASS` at init — the **same** values the
> executor config/env use, so the created user matches what the compression/commit tasks authenticate
> with. The snapshot itself contains no `CREATE USER`/`GRANT`.

---

## 8. Bring up / tear down

Order matters: the Spider cluster **creates** `spider_net`; `clp-db` attaches to it.

```sh
# 0) one-time: export secrets (§5b) and build artifacts (§3) + images (§4)

# 1) generate the base per-service Spider configs (idempotent)
cd "$SPIDER_REPO"
uv run --script tools/scripts/stack/generate.py \
    --config tools/scripts/stack/spider-compose.yaml \
    --output-dir build/spider-compose

# 2) stage the .so + writable dirs (§6)

# 3) bring up the Spider cluster + CLP overlay (creates the network). Do NOT pass --build.
docker compose \
    -f "$SPIDER_REPO/tools/docker/docker-compose.e2e.yml" \
    -f "$CLP_REPO/tools/spider/docker-compose.harness.yml" \
    -p spider-e2e up -d --wait

# 4) bring up the CLP metadata DB (attaches to the network from step 3)
docker compose -f "$CLP_REPO/tools/spider/clp-db.compose.yml" -p clp-db up -d --wait
```

Teardown (drops volumes so the next bring-up reloads a fresh snapshot):

```sh
docker compose -f "$CLP_REPO/tools/spider/clp-db.compose.yml" -p clp-db down -v
docker compose \
    -f "$SPIDER_REPO/tools/docker/docker-compose.e2e.yml" \
    -f "$CLP_REPO/tools/spider/docker-compose.harness.yml" \
    -p spider-e2e down -v
```

> `docker compose ... down` still needs the overlay's variable substitutions to resolve, so keep the
> secret env vars exported (or pass throwaways) when tearing down.

---

## 9. Verify

```sh
# cluster health
docker compose -f "$SPIDER_REPO/tools/docker/docker-compose.e2e.yml" -p spider-e2e ps

# snapshot loaded (via docker exec; MYSQL_PWD keeps the password off the command line)
docker exec -e MYSQL_PWD="${CLP_DB_ROOT_PASSWORD:-clp-root-password}" clp-db \
    mariadb -uroot -N -B -e "SELECT COUNT(*) FROM \`clp-db\`.compression_jobs;"

# drive a compression job from the host (uses the HOST-PUBLISHED ports; bridge IPs are unreachable)
cd "$CLP_REPO"
HARNESS_DB_HOST=127.0.0.1 HARNESS_DB_PORT=3306 \
HARNESS_DB_USER="$CLP_DB_USER" HARNESS_DB_PASSWORD="$CLP_DB_PASS" HARNESS_DB_NAME=clp-db \
SPIDER_STORAGE_ENDPOINT=http://127.0.0.1:50051 RUST_LOG=info \
cargo run --example e2e_compress -p compression-coordinator

# task-side logs (post-#409, the package installs its own subscriber -> its events reach the EM log)
grep -h "CLP compression task started\|CLP commit task started" <CLP_HARNESS>/logs/em-logs/*.log
```

A successful run ends in `E2E PASS`, with archives in `clp_default_archives`, a row in `clp_datasets`,
and the job `Succeeded`.

---

## 10. Gotchas

- **`ca-certificates` is mandatory** — the #1 silent failure (executor panics on TLS before any task
  logic runs).
- **`libzstd` must be 1.5.7**, not apt's 1.4.8 (same SONAME, newer symbols).
- **The executor is pooled and long-lived.** Swapping the `.so` (or `clp-s`/`indexer`) on disk does
  nothing to already-spawned executors — **restart the workers** so the EM respawns them:
  `docker compose ... -p spider-e2e restart worker-1 ... worker-8`.
- **The base worker image carries the executor binary.** After merging new upstream Spider (e.g. the
  #409 init-hook executor), rebuild the base images **and** re-derive `spider-e2e-worker-clp:latest`.
- **Init hook (#409):** the CLP `.so` initializes its runtime/config/tracing in
  `__spider_tdl_package_init`; if the executor doesn't call it (old build), the task accessors panic.
- **WSL2 / host networking:** the host can't reach `172.40.0.x`; use the published `127.0.0.1` ports.
- **`staging_directory` / `tmp_directory` are CLP_HOME-relative** and must be bind-mounted writable and
  owned by uid 1000.
- **`.so` filename:** stage it as `clp/libclp.so` (package name), not `libclp_tdl_package.so`.

---

## Reference implementation

A working, machine-specific implementation of all of the above (bring-up/teardown/stage/build-image
scripts plus the concrete compose + config files) lives under **`claude/cc-dev/spider-harness/`** in
this repo (untracked). Note it hard-codes local absolute paths and contains local secrets, so treat it
as a reference, not a template to copy verbatim.
