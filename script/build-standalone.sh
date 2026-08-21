#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
profile=${HARNESS_PROFILE:-release-fast}
# The standalone dependency cut keeps the graph small enough to compile in
# parallel again. Four workers balance this 20-thread CPU against the laptop's
# 16 GiB RAM; callers can still override the value for a colder/roomier box.
jobs=${HARNESS_BUILD_JOBS:-4}

case "$profile" in
    dev)
        # Keep assertions, overflow checks, line tables, and incremental
        # compilation while optimizing only Harness's interactive hot path.
        profile_arguments="--profile harness-dev"
        ;;
    release-fast)
        profile_arguments="--profile release-fast"
        ;;
    *)
        echo "HARNESS_PROFILE must be dev or release-fast" >&2
        exit 2
        ;;
esac

available_kib=$(awk '/MemAvailable:/ { print $2 }' /proc/meminfo)
minimum_kib=$((4 * 1024 * 1024))
if [ "${available_kib:-0}" -lt "$minimum_kib" ]; then
    echo "Refusing to compile with less than 4 GiB of available memory." >&2
    echo "Close memory-heavy applications or wait for the current build to finish." >&2
    exit 1
fi

# A failed build is preferable to the kernel choosing an interactive app as an
# OOM victim. Keep a per-process ceiling even though the bounded worker pool is
# parallel by default.
ulimit -v $((8 * 1024 * 1024))
CARGO_BUILD_JOBS="$jobs"
export CARGO_BUILD_JOBS

cd "$project_dir"
# shellcheck disable=SC2086
exec cargo build -p harness_app --bin harness $profile_arguments
