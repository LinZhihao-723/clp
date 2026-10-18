#!/usr/bin/env bash
# Runs the S3 end-to-end tests that need a real S3 server (ignored by default): the fixture and
# dataset archives uploaded to the server are extracted and searched by `clp-s-rs --auth s3`, and
# compared with the local-file results and with C++ `clp-s --auth s3` over the same endpoint.
#
# Usage: crates/clp-s-object-store/scripts/run-s3-tests.sh [CARGO_TEST_ARGS]...
#
# Starts a throwaway S3-compatible server (`proto-s3-<suffix>`, port `$PROTO_S3_PORT`, default
# 18333) from `$S3_IMAGE` (default `chrislusf/seaweedfs`: the MinIO images and binaries are no
# longer published), points the tests at it through `CLP_S_TEST_S3_ENDPOINT` and the
# `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` it accepts, and removes the container afterwards,
# also on failure. The tests run in release mode, since they extract the dataset archives.
# CARGO_TARGET_DIR defaults to the workspace's `target/`.
set -euo pipefail

PROTOTYPE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
S3_IMAGE="${S3_IMAGE:-chrislusf/seaweedfs}"
S3_PORT="${PROTO_S3_PORT:-18333}"
CONTAINER="proto-s3-${PROTO_S3_SUFFIX:-$$}"
ACCESS_KEY="protoaccess"
SECRET_KEY="protosecret"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PROTOTYPE_DIR/target}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"

CONFIG_DIR="$(mktemp -d)"
cleanup() {
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    rm -rf "$CONFIG_DIR"
}
trap cleanup EXIT

cat >"$CONFIG_DIR/s3.json" <<JSON
{"identities": [{"name": "proto",
  "credentials": [{"accessKey": "$ACCESS_KEY", "secretKey": "$SECRET_KEY"}],
  "actions": ["Admin", "Read", "Write", "List", "Tagging"]}]}
JSON
chmod a+r "$CONFIG_DIR/s3.json"

docker run -d --rm --name "$CONTAINER" -p "127.0.0.1:${S3_PORT}:8333" \
    -v "$CONFIG_DIR/s3.json:/etc/seaweedfs/s3.json:ro" \
    "$S3_IMAGE" server -s3 -s3.config=/etc/seaweedfs/s3.json -dir=/data >/dev/null
# The server answers anonymous requests with 403 once it is up.
for _ in $(seq 1 60); do
    if [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${S3_PORT}/")" == "403" ]]; then
        break
    fi
    sleep 1
done

export CLP_S_TEST_S3_ENDPOINT="http://127.0.0.1:${S3_PORT}"
export AWS_ACCESS_KEY_ID="$ACCESS_KEY"
export AWS_SECRET_ACCESS_KEY="$SECRET_KEY"
cd "$PROTOTYPE_DIR"
cargo test --release -p clp-s-cli --test s3 "$@" -- --ignored
