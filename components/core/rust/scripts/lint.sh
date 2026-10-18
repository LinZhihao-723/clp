#!/usr/bin/env bash
# Runs the three gates of the prototype workspace: rustfmt (check only), clippy (warnings denied,
# with the repository's pedantic/nursery rustflags from `.cargo/config.toml`), and the tests.
#
# Usage: scripts/lint.sh [-p CRATE]...
#   Without `-p`, every gate runs on the whole workspace; with `-p`, only on the named crates
#   (useful while other crates are being edited).
#
# CARGO_TARGET_DIR defaults to the workspace's `target/`; CARGO_BUILD_JOBS defaults to 6.
set -euo pipefail

PROTOTYPE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROTOTYPE_DIR"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PROTOTYPE_DIR/target}"

packages=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        -p | --package)
            [[ $# -ge 2 ]] || { echo "lint.sh: $1 needs a crate name" >&2; exit 2; }
            packages+=("-p" "$2")
            shift 2
            ;;
        *)
            echo "lint.sh: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

if [[ ${#packages[@]} -eq 0 ]]; then
    fmt_scope=(--all)
    cargo_scope=(--workspace)
else
    fmt_scope=("${packages[@]}")
    cargo_scope=("${packages[@]}")
fi

echo "==> cargo +nightly fmt ${fmt_scope[*]} -- --check"
cargo +nightly fmt "${fmt_scope[@]}" -- --check

echo "==> cargo +nightly clippy ${cargo_scope[*]} --all-targets --all-features -- -D warnings"
cargo +nightly clippy "${cargo_scope[@]}" --all-targets --all-features -- -D warnings

echo "==> cargo test ${cargo_scope[*]}"
cargo test "${cargo_scope[@]}"

echo "==> all gates passed"
