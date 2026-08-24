#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
profile=${HARNESS_PROFILE:-release-fast}
foreground=${HARNESS_FOREGROUND:-0}
skip_build=${HARNESS_SKIP_BUILD:-0}

case "$skip_build" in
    0|false|FALSE|no|NO|'')
        # A launch command should never silently exercise an old executable.
        # Cargo's incremental freshness check is sub-second when nothing
        # changed, while any real source change is rebuilt before the GUI
        # detaches from the caller's terminal.
        "$script_dir/build-standalone.sh"
        ;;
    1|true|TRUE|yes|YES)
        ;;
    *)
        echo "HARNESS_SKIP_BUILD must be 0 or 1" >&2
        exit 2
        ;;
esac

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
    echo "Run without HARNESS_SKIP_BUILD or invoke: $script_dir/build-standalone.sh" >&2
    exit 1
fi

case "$foreground" in
    1|true|TRUE|yes|YES)
        exec "$binary" "$@"
        ;;
    0|false|FALSE|no|NO|'')
        ;;
    *)
        echo "HARNESS_FOREGROUND must be 0 or 1" >&2
        exit 2
        ;;
esac

state_root=${XDG_STATE_HOME:-}
if [ -z "$state_root" ]; then
    if [ -z "${HOME:-}" ]; then
        echo "Neither XDG_STATE_HOME nor HOME is available for Harness logs." >&2
        exit 1
    fi
    state_root="$HOME/.local/state"
fi
log_dir="$state_root/harness/logs"
log_file="$log_dir/harness.log"
umask 077
mkdir -p "$log_dir"

if command -v setsid >/dev/null 2>&1; then
    nohup setsid "$binary" "$@" </dev/null >>"$log_file" 2>&1 &
else
    nohup "$binary" "$@" </dev/null >>"$log_file" 2>&1 &
fi
pid=$!

# Surface immediate launch failures without keeping a GUI application attached
# to the caller's terminal for its entire lifetime.
sleep 0.1
if ! kill -0 "$pid" 2>/dev/null; then
    echo "Harness exited during startup. Recent log output:" >&2
    tail -n 20 "$log_file" >&2 || true
    exit 1
fi

echo "Harness started (PID $pid). Log: $log_file"
