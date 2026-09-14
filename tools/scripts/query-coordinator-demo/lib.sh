# shellcheck shell=bash
# Shared orchestration for spider-search.sh and celery-search.sh. Source it; don't execute it.

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
readonly DEMO_DIR
REPO_DIR="$(cd "$DEMO_DIR/../../.." &>/dev/null && pwd)"
readonly REPO_DIR
readonly PACKAGE_DIR="${CLP_PACKAGE_DIR:-$REPO_DIR/build/clp-package}"
readonly HARNESS="$REPO_DIR/tools/scripts/query-harness/query_harness.py"
readonly HELPER="$DEMO_DIR/demo_helper.py"
readonly STATE_DIR="$DEMO_DIR/.demo-state"
readonly LOG_DIR="$STATE_DIR/logs"
readonly RUNS_DIR="$STATE_DIR/runs"
readonly LOCK_FILE="$STATE_DIR/.lock"
readonly CONFIG_BACKUP="$STATE_DIR/.config.orig.yaml"
readonly NUM_WORKERS=16
readonly DEFAULT_LIMIT=1000
readonly POLL_INTERVAL_SECS=0.01
readonly JOB_TIMEOUT_SECS="${DEMO_JOB_TIMEOUT_SECS:-300}"
readonly STARTUP_TIMEOUT_SECS=300
readonly WORKER_READY_TIMEOUT_SECS=60
readonly CELERY_BIN="/opt/clp/lib/python3/site-packages/bin/celery"
readonly EXIT_UNFINISHED_JOBS=3

SETUP_NOTE=""

_log_setup() {
    printf '[setup] %s\n' "$*" >&2
}

_die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

_now() {
    date +%s.%N
}

_elapsed_since() {
    awk -v start="$1" -v end="$(_now)" 'BEGIN { printf "%.3f", end - start }'
}

_helper() {
    uv run -q --script "$HELPER" "$@"
}

_harness() {
    local command="$1"
    shift
    uv run -q --script "$HARNESS" "$command" --package-dir "$PACKAGE_DIR" "$@"
}

_init_state_dir() {
    mkdir -p "$LOG_DIR" "$RUNS_DIR"
    [[ -f "$STATE_DIR/.gitignore" ]] || printf '*\n' >"$STATE_DIR/.gitignore"
}

_project_name() {
    local instance_id_file="$PACKAGE_DIR/var/log/instance-id"
    [[ -f "$instance_id_file" ]] || return 1
    printf 'clp-package-%s' "$(<"$instance_id_file")"
}

# Prints the compose service name of every running container in the package's project.
_running_services() {
    local project
    project="$(_project_name)" || return 0
    docker ps --filter "label=com.docker.compose.project=$project" \
        --format '{{.Label "com.docker.compose.service"}}'
}

_count() {
    local services="$1"
    local service="$2"
    grep -cx -- "$service" <<<"$services"
}

_describe_state() {
    local services="$1"
    if [[ -z "$services" ]]; then
        echo "not running"
    elif (($(_count "$services" query-coordinator) > 0)); then
        echo "in spider mode with $(_count "$services" spider-worker) spider-worker(s)"
    elif (($(_count "$services" query-scheduler) > 0)); then
        echo "in celery mode"
    else
        echo "partially running"
    fi
}

_is_engine_up() {
    local engine="$1"
    local services="$2"
    (($(_count "$services" database) == 1 && $(_count "$services" results-cache) == 1)) \
        || return 1
    case "$engine" in
        spider)
            (($(_count "$services" query-coordinator) == 1 \
                && $(_count "$services" spider-scheduler) == 1 \
                && $(_count "$services" spider-worker) == NUM_WORKERS \
                && $(_count "$services" query-scheduler) == 0))
            ;;
        celery)
            (($(_count "$services" query-scheduler) == 1 \
                && $(_count "$services" query-worker) == 1 \
                && $(_count "$services" query-coordinator) == 0))
            ;;
        *)
            return 1
            ;;
    esac
}

_wait_for_engine() {
    local engine="$1"
    local deadline=$((SECONDS + STARTUP_TIMEOUT_SECS))
    until _is_engine_up "$engine" "$(_running_services)"; do
        if ((SECONDS >= deadline)); then
            _die "$engine services didn't come up within ${STARTUP_TIMEOUT_SECS} s (package is" \
                "$(_describe_state "$(_running_services)"))"
        fi
        sleep 1
    done
}

_refuse_if_unfinished_jobs() {
    local output
    local rc
    output="$(_helper unfinished-jobs --package-dir "$PACKAGE_DIR")"
    rc=$?
    if ((EXIT_UNFINISHED_JOBS == rc)); then
        printf 'error: refusing to switch modes; the other engine would claim these unfinished' >&2
        printf ' query_jobs:\n%s\n' "$output" >&2
        printf 'Wait for them (query_harness.py wait <id>) or remove them' >&2
        printf ' (query_harness.py cleanup --job-id <id> --delete-jobs --force).\n' >&2
        exit 1
    fi
    ((0 == rc)) || _die "couldn't check query_jobs before switching modes: $output"
}

_switch_engine() {
    local engine="$1"
    local services="$2"
    local stamp="$3"
    local changes

    if (($(_count "$services" database) == 1)); then
        _refuse_if_unfinished_jobs
    fi
    if [[ -n "$services" ]]; then
        (cd "$PACKAGE_DIR" && ./sbin/stop-clp.sh) >"$LOG_DIR/stop-$stamp.log" 2>&1 \
            || _die "stop-clp.sh failed; see $LOG_DIR/stop-$stamp.log"
    fi
    changes="$(_helper config-apply "$engine" --package-dir "$PACKAGE_DIR" \
        --backup "$CONFIG_BACKUP")" \
        || _die "couldn't update $PACKAGE_DIR/etc/clp-config.yaml"
    printf '%s %s: %s\n' "$stamp" "$engine" "$changes" >>"$LOG_DIR/config-changes.log"
    _log_setup "config: $changes"
    (cd "$PACKAGE_DIR" && ./sbin/start-clp.sh) >"$LOG_DIR/start-$engine-$stamp.log" 2>&1 \
        || _die "start-clp.sh failed; see $LOG_DIR/start-$engine-$stamp.log"
    _wait_for_engine "$engine"
}

_query_worker_container() {
    docker ps --filter "label=com.docker.compose.project=$(_project_name)" \
        --filter "label=com.docker.compose.service=query-worker" --format '{{.ID}}' | head -n 1
}

_query_worker_concurrency() {
    local container="$1"
    docker inspect -f '{{json .Config.Cmd}}' "$container" \
        | grep -o '"--concurrency","[0-9]*"' | grep -o '[0-9][0-9]*'
}

_wait_for_celery_worker() {
    local container="$1"
    local node
    local deadline=$((SECONDS + WORKER_READY_TIMEOUT_SECS))
    node="query-worker@$(docker inspect -f '{{.Config.Hostname}}' "$container")"
    until docker exec "$container" python3 "$CELERY_BIN" -A job_orchestration.executor.query \
        inspect ping --destination "$node" --timeout 1 >/dev/null 2>&1; do
        if ((SECONDS >= deadline)); then
            _die "query-worker didn't answer a Celery ping within ${WORKER_READY_TIMEOUT_SECS} s"
        fi
        sleep 1
    done
}

# Recreates only the query-worker with `CLP_QUERY_WORKER_CONCURRENCY` set in the environment, which
# takes precedence over the controller-written `.env`. Returns 1 if it was already configured.
_ensure_celery_concurrency() {
    local stamp="$1"
    local container
    local concurrency
    local log="$LOG_DIR/query-worker-override-$stamp.log"

    container="$(_query_worker_container)"
    [[ -n "$container" ]] || _die "the query-worker isn't running"
    concurrency="$(_query_worker_concurrency "$container")"
    [[ "$concurrency" != "$NUM_WORKERS" ]] || return 1

    _log_setup "query-worker runs --concurrency ${concurrency:-?}; recreating only that service" \
        "with --concurrency $NUM_WORKERS..."
    (cd "$PACKAGE_DIR" && CLP_QUERY_WORKER_CONCURRENCY="$NUM_WORKERS" docker compose \
        --project-name "$(_project_name)" --file docker-compose.yaml \
        up --detach --no-deps --force-recreate query-worker) >"$log" 2>&1 \
        || _die "couldn't recreate the query-worker; see $log"
    container="$(_query_worker_container)"
    [[ -n "$container" ]] || _die "the query-worker isn't running after recreation; see $log"
    concurrency="$(_query_worker_concurrency "$container")"
    if [[ "$concurrency" != "$NUM_WORKERS" ]]; then
        _die "the query-worker runs --concurrency ${concurrency:-?} after recreation; see $log"
    fi
    _wait_for_celery_worker "$container"
}

# Brings the package into `engine` mode with `NUM_WORKERS` workers and sets `SETUP_NOTE`.
_ensure_engine() {
    local engine="$1"
    local stamp="$2"
    local services
    local config_diff
    local check_running=()
    local notes=()
    local switch_start

    services="$(_running_services)"
    [[ -z "$services" ]] || check_running=(--check-running)
    config_diff="$(_helper config-diff "$engine" --package-dir "$PACKAGE_DIR" \
        "${check_running[@]}")" \
        || _die "couldn't read $PACKAGE_DIR/etc/clp-config.yaml"

    if [[ -n "$config_diff" ]] || ! _is_engine_up "$engine" "$services"; then
        local state
        local action
        state="$(_describe_state "$services")"
        printf '%s %s: package %s\n%s\n' "$stamp" "$engine" "$state" "$config_diff" \
            >>"$LOG_DIR/switch-reasons.log"
        if _is_engine_up "$engine" "$services"; then
            action="restarting it with the demo's $engine config"
            notes+=("package restart to apply the demo's $engine config")
        else
            action="switching to $engine mode with $NUM_WORKERS workers"
            notes+=("mode switch to $engine incl. package restart")
        fi
        _log_setup "package is $state; $action (stop -> reconfigure -> start, ~1-2 min; logs" \
            "in $LOG_DIR)..."
        switch_start=$SECONDS
        _switch_engine "$engine" "$services" "$stamp"
        _log_setup "$engine mode is up after $((SECONDS - switch_start)) s"
    fi

    if [[ "celery" == "$engine" ]] && _ensure_celery_concurrency "$stamp"; then
        notes+=("query-worker recreated with --concurrency $NUM_WORKERS")
    fi

    if ((0 == ${#notes[@]})); then
        SETUP_NOTE="fast path: already in $engine mode with $NUM_WORKERS workers"
    else
        local IFS=";"
        SETUP_NOTE="${notes[*]}"
        SETUP_NOTE="${SETUP_NOTE//;/; }"
    fi
}

_current_workers() {
    local engine="$1"
    if [[ "spider" == "$engine" ]]; then
        _count "$(_running_services)" spider-worker
    else
        _query_worker_concurrency "$(_query_worker_container)"
    fi
}

_usage() {
    local engine="$1"
    cat <<EOF
Usage: $(basename "$0") [-n N] [-i] [-h] '<query>'

Runs <query> on a running CLP package (dataset "default") through ${engine^} with $NUM_WORKERS
workers, first switching the package to $engine mode if needed (setup time isn't query time).

  -n N   Print at most N results, newest first (default: $DEFAULT_LIMIT). All results are saved
         to $RUNS_DIR/$engine-<job_id>.jsonl.
  -i     Case-insensitive search.
  -h     Show this help.

Environment:
  CLP_PACKAGE_DIR        Package to use (default: <repo>/build/clp-package).
  DEMO_RESOURCE_GROUP    Spider mode only: set query_coordinator.resource_group.name to this
                         value (default: leave it unchanged).
  DEMO_JOB_TIMEOUT_SECS  How long to wait for the job (default: 300).
EOF
}

demo_main() {
    local engine="$1"
    shift
    local limit="$DEFAULT_LIMIT"
    local ignore_case_args=()
    local query=""
    local have_query=false

    while (($# > 0)); do
        case "$1" in
            -h | --help)
                _usage "$engine"
                exit 0
                ;;
            -i)
                ignore_case_args=(--ignore-case)
                shift
                ;;
            -n)
                (($# >= 2)) || _die "-n needs a value"
                limit="$2"
                shift 2
                ;;
            -n*)
                limit="${1#-n}"
                shift
                ;;
            --)
                shift
                (($# == 1)) || _die "expected exactly one query after --"
                query="$1"
                have_query=true
                shift
                ;;
            -*)
                _die "unknown option '$1' (see -h)"
                ;;
            *)
                "$have_query" && _die "expected exactly one query; quote it (see -h)"
                query="$1"
                have_query=true
                shift
                ;;
        esac
    done
    "$have_query" || { _usage "$engine" >&2; exit 2; }
    [[ "$limit" =~ ^[0-9]+$ ]] || _die "-n expects a non-negative integer, got '$limit'"
    command -v uv >/dev/null || _die "uv isn't installed"
    command -v docker >/dev/null || _die "docker isn't installed"
    [[ -d "$PACKAGE_DIR" ]] || _die "package directory '$PACKAGE_DIR' doesn't exist (set CLP_PACKAGE_DIR)"
    [[ -f "$HARNESS" ]] || _die "query harness '$HARNESS' doesn't exist"

    _init_state_dir
    exec 9>"$LOCK_FILE"
    if ! flock -n 9; then
        _log_setup "waiting for another demo run to finish..."
        flock 9
    fi

    local stamp
    local setup_start
    local workers
    local setup_secs
    stamp="$(date +%Y%m%dT%H%M%S)"
    setup_start="$(_now)"
    _ensure_engine "$engine" "$stamp"
    workers="$(_current_workers "$engine")"
    setup_secs="$(_elapsed_since "$setup_start")"

    local pending_record="$RUNS_DIR/.pending-$$.record.json"
    local pending_dump="$RUNS_DIR/.pending-$$.jsonl"
    local pending_log="$LOG_DIR/.pending-$$.harness.log"
    local job_id
    rm -f "$pending_dump"
    _harness submit --poll-interval "$POLL_INTERVAL_SECS" --timeout "$JOB_TIMEOUT_SECS" \
        --dump-results "$pending_dump" "${ignore_case_args[@]}" -- "$query" \
        >"$pending_record" 2>"$pending_log"
    job_id="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1])).get("job_id") or "")' \
        "$pending_record" 2>/dev/null)"
    if [[ -z "$job_id" ]]; then
        cat "$pending_log" >&2
        _die "the harness didn't submit the job; see $pending_log"
    fi

    local record="$RUNS_DIR/$engine-$job_id.record.json"
    local dump="$RUNS_DIR/$engine-$job_id.jsonl"
    local harness_log="$LOG_DIR/harness-$engine-$job_id.log"
    mv "$pending_record" "$record"
    mv "$pending_log" "$harness_log"
    if [[ -f "$pending_dump" ]]; then
        mv "$pending_dump" "$dump"
    fi

    _harness cleanup --job-id "$job_id" >>"$LOG_DIR/cleanup.log" 2>&1 \
        || printf 'warning: couldn'\''t drop the result collection of job %s; see %s\n' \
            "$job_id" "$LOG_DIR/cleanup.log" >&2

    _helper render --engine "$engine" --workers "${workers:-?}" --query="$query" \
        "${ignore_case_args[@]}" --record "$record" --dump "$dump" --limit "$limit" \
        --setup-secs "$setup_secs" --setup-note="$SETUP_NOTE"
}
