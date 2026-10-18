#!/usr/bin/env bash
# Runs the results-cache tests that need a MongoDB server (ignored by default): the sink's own
# tests and the `clp-s-rs search --output results-cache` comparisons with the C++ oracles.
#
# Usage: crates/clp-s-results-cache/scripts/run-mongo-tests.sh [CARGO_TEST_ARGS]...
#
# Starts a throwaway `mongo:8.0.21` container (`proto-mongo-<suffix>`, port `$PROTO_MONGO_PORT`,
# default 27918), points the tests at it through `CLP_S_TEST_MONGODB_URI`, and removes the
# container afterwards, also on failure. CARGO_TARGET_DIR defaults to the workspace's `target/`.
set -euo pipefail

PROTOTYPE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
MONGO_IMAGE="${MONGO_IMAGE:-mongo:8.0.21}"
MONGO_PORT="${PROTO_MONGO_PORT:-27918}"
CONTAINER="proto-mongo-${PROTO_MONGO_SUFFIX:-$$}"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PROTOTYPE_DIR/target}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"

cleanup() {
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run -d --rm --name "$CONTAINER" -p "127.0.0.1:${MONGO_PORT}:27017" "$MONGO_IMAGE" >/dev/null
for _ in $(seq 1 60); do
    if docker exec "$CONTAINER" mongosh --quiet --eval 'db.runCommand({ping: 1}).ok' 2>/dev/null \
        | grep -q 1; then
        break
    fi
    sleep 1
done

export CLP_S_TEST_MONGODB_URI="mongodb://127.0.0.1:${MONGO_PORT}/proto"
cd "$PROTOTYPE_DIR"
cargo test -p clp-s-results-cache "$@" -- --ignored
cargo test -p clp-s-cli --test outputs "$@" -- --ignored
