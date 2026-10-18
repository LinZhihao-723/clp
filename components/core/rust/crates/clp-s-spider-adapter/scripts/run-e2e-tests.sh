#!/usr/bin/env bash
# Runs the Spider adapter's end-to-end tests (ignored by default): in-process search tasks over
# file-system and S3 archive storage, compared with the C++ `clp-s` subprocess path through the
# results cache, plus the fallback on unsupported archives.
#
# Usage: crates/clp-s-spider-adapter/scripts/run-e2e-tests.sh [CARGO_TEST_ARGS]...
#
# Starts a throwaway `mongo:8.0.21` container (`proto-mongo-spider-<suffix>`, port
# `$PROTO_MONGO_PORT`, default 27919) and a throwaway S3-compatible server
# (`proto-s3-spider-<suffix>`, port `$PROTO_S3_PORT`, default 18334, from `$S3_IMAGE`, default
# `chrislusf/seaweedfs`), points the tests at them through `CLP_S_TEST_MONGODB_URI`,
# `CLP_S_TEST_S3_ENDPOINT` and the `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` the server accepts,
# and removes both containers afterwards, also on failure. CARGO_TARGET_DIR defaults to the
# workspace's `target/`.
set -euo pipefail

PROTOTYPE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
MONGO_IMAGE="${MONGO_IMAGE:-mongo:8.0.21}"
MONGO_PORT="${PROTO_MONGO_PORT:-27919}"
S3_IMAGE="${S3_IMAGE:-chrislusf/seaweedfs}"
S3_PORT="${PROTO_S3_PORT:-18334}"
SUFFIX="${PROTO_SUFFIX:-$$}"
MONGO_CONTAINER="proto-mongo-spider-${SUFFIX}"
S3_CONTAINER="proto-s3-spider-${SUFFIX}"
ACCESS_KEY="protoaccess"
SECRET_KEY="protosecret"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PROTOTYPE_DIR/target}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"

CONFIG_DIR="$(mktemp -d)"
cleanup() {
    docker rm -f "$MONGO_CONTAINER" "$S3_CONTAINER" >/dev/null 2>&1 || true
    rm -rf "$CONFIG_DIR"
}
trap cleanup EXIT

cat >"$CONFIG_DIR/s3.json" <<JSON
{"identities": [{"name": "proto",
  "credentials": [{"accessKey": "$ACCESS_KEY", "secretKey": "$SECRET_KEY"}],
  "actions": ["Admin", "Read", "Write", "List", "Tagging"]}]}
JSON
chmod a+r "$CONFIG_DIR/s3.json"

docker run -d --rm --name "$MONGO_CONTAINER" -p "127.0.0.1:${MONGO_PORT}:27017" "$MONGO_IMAGE" \
    >/dev/null
docker run -d --rm --name "$S3_CONTAINER" -p "127.0.0.1:${S3_PORT}:8333" \
    -v "$CONFIG_DIR/s3.json:/etc/seaweedfs/s3.json:ro" \
    "$S3_IMAGE" server -s3 -s3.config=/etc/seaweedfs/s3.json -dir=/data >/dev/null

for _ in $(seq 1 60); do
    if docker exec "$MONGO_CONTAINER" mongosh --quiet --eval 'db.runCommand({ping: 1}).ok' \
        2>/dev/null | grep -q 1; then
        break
    fi
    sleep 1
done
# The S3 server answers anonymous requests with 403 once it is up.
for _ in $(seq 1 60); do
    if [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${S3_PORT}/")" == "403" ]]; then
        break
    fi
    sleep 1
done

export CLP_S_TEST_MONGODB_URI="mongodb://127.0.0.1:${MONGO_PORT}/proto-spider"
export CLP_S_TEST_S3_ENDPOINT="http://127.0.0.1:${S3_PORT}"
export AWS_ACCESS_KEY_ID="$ACCESS_KEY"
export AWS_SECRET_ACCESS_KEY="$SECRET_KEY"
unset AWS_SESSION_TOKEN AWS_PROFILE AWS_REGION AWS_DEFAULT_REGION AWS_ENDPOINT_URL
cd "$PROTOTYPE_DIR"
cargo test -p clp-s-spider-adapter --test e2e "$@" -- --ignored --nocapture
