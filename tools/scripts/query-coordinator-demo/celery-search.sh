#!/usr/bin/env bash
# Runs one search on the live CLP package through Celery with 16 query-worker processes. See
# README.md.

set -o nounset
set -o pipefail

# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

demo_main celery "$@"
