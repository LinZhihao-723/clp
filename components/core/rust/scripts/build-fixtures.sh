#!/usr/bin/env bash
# Regenerates the script-generated fixture inputs and every fixture archive under
# tests/fixtures/archives/ from tests/fixtures/fixtures.toml, using the C++ clp-s.
source "$(dirname "${BASH_SOURCE[0]}")/_env.sh"
"${PY[@]}" "$SCRIPTS_DIR/gen_inputs.py" "$PROTOTYPE_DIR/tests/fixtures/inputs"
"${PY[@]}" "$SCRIPTS_DIR/corpus.py" build-fixtures
