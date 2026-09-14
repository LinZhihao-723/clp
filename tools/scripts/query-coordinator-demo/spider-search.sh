#!/usr/bin/env bash
# Runs one search on the live CLP package through Spider with 16 spider-workers. See README.md.

set -o nounset
set -o pipefail

# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

demo_main spider "$@"
