# shellcheck shell=bash
# Shared environment for build-fixtures.sh and gen-oracles.sh (sourced, not executed).
set -euo pipefail
SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROTOTYPE_DIR="$(dirname "$SCRIPTS_DIR")"
REPO_DIR="$(cd "$PROTOTYPE_DIR/../../.." && pwd)"
export CLP_S="${CLP_S:-$REPO_DIR/build/core/clp-s}"
export LOG_CONVERTER="${LOG_CONVERTER:-$REPO_DIR/build/core/log-converter}"
export PROTO_DATA_ROOT="${PROTO_DATA_ROOT:-$PROTOTYPE_DIR/target/proto-data}"
VENV="$PROTO_DATA_ROOT/venv"
if [[ ! -x "$VENV/bin/python" ]]; then
    uv venv "$VENV" -p 3.10
    VIRTUAL_ENV="$VENV" uv pip install msgpack==1.1.0 pymongo==4.10.1 tomli==2.0.2
fi
# shellcheck disable=SC2034  # used by the scripts that source this file
PY=("$VENV/bin/python" -I)
