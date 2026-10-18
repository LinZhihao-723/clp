#!/usr/bin/env bash
# Checks `x` on the three dataset archives (bench, spider, synthetic) against the C++ oracles
# through the Rust library and the `clp-s-rs` binary (`docs/design.md` §11.4), then times C++
# `clp-s x` and `clp-s-rs x` on each dataset (pinned with `taskset -c 8-15`, best of RUNS runs).
#
# Usage: crates/clp-s-difftest/scripts/extract-datasets.sh
#   CARGO_TARGET_DIR (default: the workspace's `target/`), PROTO_DATA_ROOT (default:
#   `target/proto-data`), CLP_S (the C++ binary, default: `<repo>/build/core/clp-s`), RUNS
#   (default 3) and CPUS (default 8-15) may be overridden.
set -euo pipefail

PROTOTYPE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$PROTOTYPE_DIR"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PROTOTYPE_DIR/target}"
PROTO_DATA_ROOT="${PROTO_DATA_ROOT:-$PROTOTYPE_DIR/target/proto-data}"
CLP_S="${CLP_S:-$(cd "$PROTOTYPE_DIR/../../.." && pwd)/build/core/clp-s}"
RUNS="${RUNS:-3}"
CPUS="${CPUS:-8-15}"

cargo build --release -p clp-s-cli -p clp-s-difftest
release_dir="$CARGO_TARGET_DIR/release"

for reader in rust-library rust-cli; do
    echo "==> clp-s-difftest datasets --reader $reader (x cases)"
    "$release_dir/clp-s-difftest" --cpp-clp-s "$CLP_S" --data-root "$PROTO_DATA_ROOT" \
        datasets --reader "$reader" --rust-cli "$release_dir/clp-s-rs" \
        | grep -E '^(PASS|ACCEPTED|FAIL) +x:|passed'
done

work_dir="$(mktemp -d "$PROTO_DATA_ROOT/work/extract-datasets.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT

# Prints the best wall time (seconds) and its peak RSS (KiB) of RUNS runs of a command whose
# output directory is `$work_dir/out`.
best_of() {
    local best_time="" best_rss=""
    for _ in $(seq "$RUNS"); do
        rm -rf "$work_dir/out"
        /usr/bin/time -o "$work_dir/time" -f '%e %M' taskset -c "$CPUS" "$@" >/dev/null 2>&1
        local time rss
        read -r time rss <"$work_dir/time"
        if [[ -z "$best_time" ]] || awk "BEGIN { exit !($time < $best_time) }"; then
            best_time="$time"
            best_rss="$rss"
        fi
    done
    echo "$best_time $best_rss"
}

echo "==> x wall time (best of $RUNS) and peak RSS"
printf '%-10s %12s %12s %12s %12s\n' dataset cpp_s cpp_rss_kib rust_s rust_rss_kib
for dataset in bench spider synthetic; do
    archive="$PROTO_DATA_ROOT/datasets/$dataset.clps"
    read -r cpp_time cpp_rss < <(best_of "$CLP_S" x "$archive" "$work_dir/out")
    read -r rust_time rust_rss < <(best_of "$release_dir/clp-s-rs" x "$archive" -o "$work_dir/out")
    printf '%-10s %12s %12s %12s %12s\n' "$dataset" "$cpp_time" "$cpp_rss" "$rust_time" "$rust_rss"
done
