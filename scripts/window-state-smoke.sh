#!/usr/bin/env bash
set -euo pipefail

for tool in xvfb-run xdotool; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing window-state test dependency: $tool" >&2
        exit 1
    fi
done

cargo build --quiet --bin wmcterm
state_tmp="$(mktemp -d)"
trap 'rm -rf "$state_tmp"' EXIT
mkdir -p "$state_tmp/runtime" "$state_tmp/config" "$state_tmp/data" "$state_tmp/workspace"
chmod 700 "$state_tmp/runtime"

xvfb-run -a --server-args="-screen 0 1280x800x24" sh -eu -c '
    export WGPU_BACKEND=gl
    export XDG_RUNTIME_DIR="$1/runtime"
    export XDG_CONFIG_HOME="$1/config"
    export XDG_DATA_HOME="$1/data"
    unset WMCTERM_CONTROL_SOCKET WMCTERM_SOCKET
    unset WMCTERM_WORKSPACE_ID WMCTERM_PANE_ID WMCTERM_SURFACE_ID
    binary="$2"
    cd "$1/workspace"

    start_window() {
        excluded_window_id="${2:-}"
        "$binary" gui >"$1/window.log" 2>&1 &
        app_pid=$!
        window_id=""
        attempt=0
        while [ "$attempt" -lt 80 ]; do
            for candidate in $(xdotool search --name "^wmcterm$" 2>/dev/null || true); do
                if [ "$candidate" != "$excluded_window_id" ]; then
                    window_id="$candidate"
                fi
            done
            [ -n "$window_id" ] && break
            attempt=$((attempt + 1))
            sleep 0.1
        done
        [ -n "$window_id" ] || { cat "$1/window.log" >&2; exit 1; }
    }

    start_window "$1"
    xdotool windowsize "$window_id" 900 600
    sleep 0.3
    closing_window_id=$window_id
    xdotool mousemove --window "$window_id" 880 20 click 1
    closing_pid=$app_pid

    start_window "$1" "$closing_window_id"
    wait "$closing_pid"
    [ -S "$XDG_RUNTIME_DIR/wmcterm.sock" ] || {
        echo "replacement control socket was removed by the previous instance" >&2
        exit 1
    }
    attempt=0
    while ! "$binary" status >/dev/null 2>&1; do
        attempt=$((attempt + 1))
        [ "$attempt" -lt 50 ] || {
            cat "$1/window.log" >&2
            echo "replacement control socket did not become ready" >&2
            exit 1
        }
        sleep 0.1
    done
    sleep 0.4
    geometry=$(xdotool getwindowgeometry --shell "$window_id")
    eval "$geometry"
    [ "$WIDTH" -eq 900 ] && [ "$HEIGHT" -eq 600 ] || {
        echo "window size was not restored: ${WIDTH}x${HEIGHT}" >&2
        exit 1
    }
    [ "$X" -ge 188 ] && [ "$X" -le 192 ] && [ "$Y" -ge 98 ] && [ "$Y" -le 102 ] || {
        echo "restored window is not centered: ${X},${Y}" >&2
        exit 1
    }
    xdotool mousemove --window "$window_id" 880 20 click 1
    wait "$app_pid"
' sh "$state_tmp" "$PWD/target/debug/wmcterm"

echo "Window size restore and desktop centering smoke test passed"
