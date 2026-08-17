#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
profile=${HARNESS_PROFILE:-dev}
jobs=${HARNESS_BUILD_JOBS:-1}
bundled_cmake="$project_dir/.tools/cmake-4.3.4/bin"

case "$profile" in
    dev)
        profile_arguments=""
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

if [ -x "$bundled_cmake/cmake" ]; then
    PATH="$bundled_cmake:$PATH"
elif ! command -v cmake >/dev/null 2>&1; then
    echo "CMake is required. Install it or place CMake 4.3.4 under .tools/." >&2
    exit 1
fi

# A failed build is preferable to the kernel choosing an interactive app as an
# OOM victim. Codegen stays serialized unless the caller explicitly opts in.
ulimit -v $((8 * 1024 * 1024))
CARGO_BUILD_JOBS="$jobs"
export PATH CARGO_BUILD_JOBS

cd "$project_dir"
# shellcheck disable=SC2086
exec cargo build -p harness_app --bin harness $profile_arguments
