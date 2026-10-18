#!/usr/bin/env bash
# Regenerates the C++ oracles: fixture decompression oracles, fixture query oracles (stdout,
# msgpack file output, results cache via a throwaway mongo container), the dataset archives and
# their decompression and query oracles.
#
# Usage: gen-oracles.sh [fixtures|queries|datasets|dataset-queries|all]   (default: all)
source "$(dirname "${BASH_SOURCE[0]}")/_env.sh"
step="${1:-all}"
case "$step" in
    fixtures) "${PY[@]}" "$SCRIPTS_DIR/corpus.py" fixture-oracles ;;
    queries) "${PY[@]}" "$SCRIPTS_DIR/corpus.py" query-oracles --only-fixtures ;;
    datasets) "${PY[@]}" "$SCRIPTS_DIR/corpus.py" datasets ;;
    dataset-queries) "${PY[@]}" "$SCRIPTS_DIR/corpus.py" query-oracles --only-datasets ;;
    all)
        "${PY[@]}" "$SCRIPTS_DIR/corpus.py" fixture-oracles
        "${PY[@]}" "$SCRIPTS_DIR/corpus.py" datasets
        "${PY[@]}" "$SCRIPTS_DIR/corpus.py" query-oracles
        ;;
    *) echo "unknown step: $step" >&2; exit 2 ;;
esac
