#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
profile=${HARNESS_PROFILE:-dev}

case "$profile" in
    dev)
        binary="$project_dir/target/harness-dev/harness"
        ;;
    release-fast)
        binary="$project_dir/target/release-fast/harness"
        ;;
    *)
        echo "HARNESS_PROFILE must be dev or release-fast" >&2
        exit 2
        ;;
esac

if [ ! -x "$binary" ]; then
    echo "Harness has not been built for the $profile profile." >&2
    echo "Run: $script_dir/build-standalone.sh" >&2
    exit 1
fi

exec "$binary" "$@"
