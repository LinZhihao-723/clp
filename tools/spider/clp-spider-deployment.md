# Running the CLP package against an external Spider cluster

Reproduction guide for a CLP package deployment whose compression is driven by the
`compression-coordinator` talking to a Huntsman (Rust) Spider cluster, with the two stacks managed
independently.

Everything below was executed and verified end-to-end on a WSL2 + Docker Desktop host. Sections
marked **GOTCHA** are failures that actually occurred; each cost real debugging time.

---

## 1. Topology

Two independently-managed docker compose stacks sharing one bridge network:

| Stack | Compose file | Contains |
| --- | --- | --- |
| CLP package | `build/clp-package/docker-compose.yaml` | `database`, `queue`, `redis`, `results-cache`, query scheduler/workers, `webui`, `api-server`, `log-ingestor`, **`compression-coordinator`** |
| Spider (Huntsman) | `~/dev/spider-e2e/tools/docker/docker-compose.e2e.yml` | `mariadb` (Spider's own DB), `storage` (gRPC), `scheduler`, `worker-1..8` |

Cross-stack traffic goes **both** ways:

```
compression-coordinator ──> storage    172.40.0.30:50051   submit + poll jobs
spider workers          ──> database   (DNS alias on spider_net)  archive/column metadata writes
```

**Why the compression scheduler/worker are absent:** configuring `compression_coordinator` makes
`controller.py` emit `CLP_COMPRESSION_SCHEDULER_ENABLED=0` and `CLP_COMPRESSION_WORKER_ENABLED=0`,
scaling both to zero replicas. The coordinator replaces them.

**Not to be confused with** `docker-compose-spider.yaml` in the package: that starts CLP's *older
C++ Spider* (`spider_scheduler`, JDBC storage, port 6000). The coordinator targets Huntsman, which
speaks gRPC on 50051. Different systems.

---

## 2. Prerequisites

- Docker with compose v2.
- CLP repo at `~/dev/clp`, Spider fork at `~/dev/spider-e2e`.
- `~/credentials.yaml` with `database`, `queue`, `redis` sections (schema:
  `build/clp-package/etc/credentials.template.yaml`).
- A `clp-config.yaml` containing **both** `compression_coordinator` and `spider` sections
  (`ClpConfig` rejects the former without the latter), `package.storage_engine: "clp-s"` (the
  coordinator asserts this at startup), and AWS credentials under `archive_output`.
- The `spider-e2e-worker-clp:latest` image, which bakes `clp-s`, `indexer`, and their runtime libs.
  See `tools/spider/README.md` §4 for building it.

---

## 3. Networking: why a shared bridge

### 3a. What is actually required

The two needs are **not** the same, and this distinction is the whole design:

| Direction | Needs | Satisfied by |
| --- | --- | --- |
| coordinator → storage | IP reachability of `172.40.0.30` | works across separate bridges on this host — no shared network needed |
| worker → CLP DB | **DNS resolution of the name `database`** | requires a shared network + alias |

Docker routes between separate bridge networks here: the coordinator connected to
`172.40.0.30:50051` *before* any network attachment existed. But cross-bridge routing gives you
**addresses, not names**, and the workers must resolve the literal string `database` (see §3c). That
is what the shared network buys.

### 3b. Rejected: published ports

Publishing CLP's DB and dialling `host.docker.internal` works at the packet level but forces
`database.host` to hold two incompatible values at once:

- `controller.py` turns it into `CLP_DB_HOST`, the address the port *binds* to → needs `0.0.0.0`
- Spider workers read the same field to decide where to *connect* → needs `host.docker.internal`

so it requires maintaining a second executor config. It also needs a `0.0.0.0` bind, exposing the DB
on every host interface (here including LAN and Tailscale). Both costs are avoidable.

Host-addressing facts measured on this host, if you ever need them:

| Address | Reachable from a container? |
| --- | --- |
| `172.29.28.84` (WSL2 `eth0`) | **no** — times out |
| `host.docker.internal` | yes (resolves to `10.0.0.95`, which is **not bindable** — `EADDRNOTAVAIL`) |

### 3c. Chosen: shared bridge with a DNS alias

Attach CLP's `database` container to the Spider network **with the alias `database`**:

- Spider workers resolve `database:3306` on `spider_net`
- CLP's own services resolve `database` on the CLP network — which is exactly what
  `ClpConfig.transform_for_container` writes into the generated config
- The coordinator reaches storage at `172.40.0.30:50051`, already the configured value

Result: **one config file, unmodified, serves every consumer.**

> **GOTCHA — the network is compose-prefixed.** It is `spider-e2e_spider_net`, not `spider_net`.
> `docker network connect spider_net ...` fails with "network not found".

> **GOTCHA — attachments are per-container.** Recreating a container drops them. Re-run §6.

---

## 4. Build and stage

```bash
cd ~/dev/clp
task                                   # -> build/clp-package (slow; builds C++ deps, Rust, image)

cp claude/cc-dev/clp-config.yaml build/clp-package/etc/clp-config.yaml
cp ~/credentials.yaml             build/clp-package/etc/credentials.yaml

# TDL package. REBUILD AND RESTAGE ON EVERY TDL CHANGE -- a stale .so runs silently.
cargo build --release -p clp-tdl-package
install -D -m0644 target/release/libclp_tdl_package.so \
    ~/dev/spider-e2e/build/tdl_packages/clp/libclp.so

# Sanity: all four FFI symbols, including the #409 init hook.
nm -D --defined-only target/release/libclp_tdl_package.so | grep -o "__spider_tdl_package_[a-z_]*"
#  -> __spider_tdl_package_{execute,get_name,get_version,init}

# Writable dirs the workers bind-mount. MUST exist and be owned by uid 1000 before bring-up (§5b).
mkdir -p build/clp-package/var/data/staged-archives \
         build/clp-package/var/tmp \
         build/clp-package/var/log/em-logs

# Per-service Spider configs (idempotent).
cd ~/dev/spider-e2e
uv run --script tools/scripts/stack/generate.py \
    --config tools/scripts/stack/spider-compose.yaml \
    --output-dir build/spider-compose
```

Validate the config before starting anything — far cheaper than debugging a failed boot:

```bash
cd ~/dev/clp
uv run --project components/clp-py-utils python -c "
import yaml; from clp_py_utils.clp_config import ClpConfig
ClpConfig.model_validate(yaml.safe_load(open('build/clp-package/etc/clp-config.yaml')))
print('VALID')"
```

---

## 5. The worker overlay

`claude/cc-dev/spider-harness/docker-compose.package.yml` roots the CLP-side environment at the
built package. Two details in it are non-obvious and both caused failures.

### 5a. Workers mount the GENERATED config, not `etc/clp-config.yaml`

```
build/clp-package/var/log/.clp-config.yaml   ->  /clp/etc/clp-config.yaml  (CLP_CONFIG_PATH)
```

`.clp-config.yaml` is a **dotfile in the logs directory** (constant `CLP_SHARED_CONFIG_FILENAME`,
`clp_config.py:76`), written by `dump_shared_container_config` (`general.py:416`) on every
`start-clp.sh`. It is the config *after* `transform_for_container()`, so `database.host` is the
service name `database` — which is what makes §3c work. The raw `etc/clp-config.yaml` still says
`localhost` and would be wrong in a worker.

Consequence: **CLP must be started at least once before Spider**, or the file will not exist / will
be stale. See §6.

### 5b. Staging and tmp mounts must use ABSOLUTE paths

> **GOTCHA.** Mounting these at `$CLP_HOME/var/...` fails with:
> `clp-s exited with exit status: 1: Failed to create archives directory /var/data/staged-archives/default`

`transform_for_container` rewrites the paths to absolute for CLP's own containers:

| | raw `etc/clp-config.yaml` | generated `.clp-config.yaml` |
| --- | --- | --- |
| `staging_directory` | `var/data/staged-archives` | `/var/data/staged-archives` |
| `tmp_directory` | `var/tmp` | `/var/tmp` |

and `make_config_path_absolute` (`config.rs:552`) **returns absolute paths unchanged**, so
`CLP_HOME` is never joined:

```rust
fn make_config_path_absolute(root: &Path, path: &str) -> PathBuf {
    if Path::new(path).is_absolute() { PathBuf::from(path) } else { root.join(path) }
}
```

Used via `abs_tmp_directory` (`config.rs:112`) and `abs_archive_output_staging` (`config.rs:123`),
called at `s3_compression.rs:57` and `:71`. Two tests pin this behaviour
(`abs_*_leaves_absolute_*_unchanged`), so it is intended — the mounts must simply match.

The overlay therefore mounts:

```
build/clp-package/var/data/staged-archives -> /var/data/staged-archives
build/clp-package/var/tmp                  -> /var/tmp
build/clp-package/var/log/em-logs          -> /home/spider-user/spider-run/em-logs
```

`CLP_HOME=/clp` still matters, but **only** for locating `bin/clp-s` and `bin/indexer`.

> **GOTCHA — bind-mount source dirs must pre-exist, owned by uid 1000.** Docker creates a missing
> source directory as `root:root`. The worker runs as `spider-user` (uid 1000), so the EM then fails
> at `File::create` in its log dir and dies before spawning an executor:
> `ExecutorCreationFailure(Os { code: 13, PermissionDenied })`.
>
> The base compose deliberately mounts **no** volume at the run dir for exactly this reason; our
> overlay overrides that to get host-visible task logs, so it inherits the hazard.

> **GOTCHA — after fixing a bind-mount source dir, `stop`/`start` is not enough.** The container
> stays bound to the old (deleted) inode. Use `docker compose up -d --force-recreate`.

---

## 6. Bring-up (order matters)

```bash
cd ~/dev/clp

# Credentials into the environment for compose substitution (never hard-code them in the overlay).
eval "$(python3 -c "
import yaml
c = yaml.safe_load(open('build/clp-package/etc/credentials.yaml'))
g = yaml.safe_load(open('build/clp-package/etc/clp-config.yaml'))
aws = g['archive_output']['storage']['s3_config']['aws_authentication']['credentials']
print(f'export CLP_DB_USER={c[\"database\"][\"username\"]!r}')
print(f'export CLP_DB_PASS={c[\"database\"][\"password\"]!r}')
print(f'export AWS_ACCESS_KEY_ID={aws[\"access_key_id\"]!r}')
print(f'export AWS_SECRET_ACCESS_KEY={aws[\"secret_access_key\"]!r}')")"

# 1) CLP FIRST -- this generates var/log/.clp-config.yaml, which the workers mount (§5a).
#    The coordinator will fail to reach Spider and exit here. That is expected.
build/clp-package/sbin/start-clp.sh

# 2) Spider, mounting the config CLP just generated.
docker compose \
    -f ~/dev/spider-e2e/tools/docker/docker-compose.e2e.yml \
    -f claude/cc-dev/spider-harness/docker-compose.package.yml \
    -p spider-e2e up -d --wait

# 3) Join the networks (note the compose-prefixed network name).
NET=spider-e2e_spider_net
DB=$(docker ps -a --filter "name=clp-package-.*-database-1" --format '{{.Names}}' | head -1)
CO=$(docker ps -a --filter "name=clp-package-.*-compression-coordinator-1" --format '{{.Names}}' | head -1)
docker network connect --alias database "$NET" "$DB"
docker network connect "$NET" "$CO"

# 4) Restart the coordinator, which exited in step 1.
docker restart "$CO"
```

The ordering is circular by nature — the workers need a config only CLP produces, and the
coordinator needs a Spider that must start after CLP. Starting CLP first and restarting the
coordinator last resolves it.

> The coordinator has **no `restart` policy**, so its step-1 exit is permanent until step 4. If
> Spider is ever slow to come up you will need the same manual nudge. Adding
> `restart: unless-stopped` to that service would make this self-healing.

---

## 7. Verify

```bash
docker ps --format '{{.Names}}\t{{.Status}}' | grep spider-e2e   # 3 infra + 8 workers
docker ps -a --format '{{.Names}}\t{{.Status}}' | grep clp-package

# Workers must show this, NOT a PermissionDenied restart loop:
docker logs spider-e2e-worker-1-1 2>&1 | tail -3
#  -> "Executor spawned." / "Execution manager started."

# Both cross-stack directions.
docker exec spider-e2e-worker-1-1 bash -c \
    'exec 3<>/dev/tcp/database/3306 && head -c 24 <&3 | tr -c "[:print:]" "."'   # MariaDB greeting
docker exec "$CO" bash -c 'exec 3<>/dev/tcp/172.40.0.30/50051 && echo reachable'
```

Note `/dev/tcp` is a **bash** feature; the workers' default `sh` is dash and will report
"Directory nonexistent".

Coordinator start-up is proven by the resource group it creates:

```bash
docker exec -e MYSQL_PWD="$CLP_DB_PASS" "$DB" \
    mariadb -u"$CLP_DB_USER" -N -B \
    -e "SELECT service_name, spider_rg_id FROM \`clp-db\`.spider_resource_groups;"
#  -> compression-coordinator   1
```

That single row exercises the config load, the `clp-s` assertion, the DB pool, and the Spider
client. During a job, expect:

```
Sending job <id> for scheduling.
Partitioned job <id> into compression tasks.   num_partitions=N
Submitted job <id> to Spider.                  spider_job_id=... num_tasks=N
Received the result of job <id>.               result=Ok(Succeeded)
```

```sql
SELECT id, status, num_tasks, spider_id, start_time, duration FROM compression_jobs;  -- 2 = SUCCEEDED
SELECT name, archive_storage_directory FROM clp_datasets;
SELECT COUNT(*) FROM clp_default_archives;
```

The commit task — not the coordinator — writes the terminal status, sizes, and duration.

---

## 8. Teardown

```bash
build/clp-package/sbin/stop-clp.sh
docker compose \
    -f ~/dev/spider-e2e/tools/docker/docker-compose.e2e.yml \
    -f claude/cc-dev/spider-harness/docker-compose.package.yml \
    -p spider-e2e down -v
```

Keep the credential env vars exported: `down` still needs the overlay's variable substitutions to
resolve.

---

## 9. Re-running a failed job

The coordinator tracks a **`last_polled_job_id` watermark held in memory**, so a job it has already
seen is never re-fetched. To re-drive job 1, reset the row *and* restart the coordinator (which
clears the watermark):

```sql
UPDATE compression_jobs
   SET status = 0, status_msg = '', spider_id = NULL, num_tasks = 0,
       start_time = NULL, duration = NULL
 WHERE id = 1;
```
```bash
docker restart "$CO"
```

---

## 10. Gotchas summary

- **Stale TDL `.so`** — rebuild and restage on every change; the cluster silently runs old code.
- **Network name is `spider-e2e_spider_net`**, not `spider_net`.
- **Network attachments are per-container** — lost on recreate.
- **Bind-mount source dirs must pre-exist as uid 1000**, or the EM dies with `PermissionDenied`.
- **After changing a bind-mount source dir, `--force-recreate`** — `stop`/`start` keeps the old inode.
- **Workers mount `.clp-config.yaml`** (generated), never `etc/clp-config.yaml`.
- **Staging/tmp mounts are absolute paths**, not `$CLP_HOME`-relative.
- **Container IPs shift on recreate.** Only `172.40.0.30` (storage) is statically pinned and
  referenced by config; do not rely on any other address.
- **Resource-group drift** — the coordinator records its Spider resource group in
  `spider_resource_groups` keyed by `service_name`. Resetting the CLP DB while Spider keeps the group
  makes start-up fail on a duplicate external id, with no lookup-by-external-id to recover. Reset
  both together.
- **In-flight jobs are not drained on shutdown.** `schedule_job` runs detached and awaits the whole
  job, but nothing joins those tasks, so `termination_timeout_seconds` bounds an already-instant
  join. Jobs in flight are abandoned with their rows left `RUNNING`.
- **Credentials** — `etc/clp-config.yaml` holds AWS keys inline and `etc/credentials.yaml` holds
  DB/queue/redis passwords. Both live under `build/` and must never be committed.
