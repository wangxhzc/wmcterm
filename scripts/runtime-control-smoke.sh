#!/usr/bin/env bash
set -euo pipefail

for tool in xvfb-run xdotool rg python3; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing runtime smoke dependency: $tool" >&2
        exit 1
    fi
done

if [ "$#" -gt 0 ]; then
    binary="$1"
else
    cargo build --quiet --bin wmcterm
    binary="$PWD/target/debug/wmcterm"
fi
binary="$(realpath "$binary")"
test_tmp="$(mktemp -d)"
trap 'rm -rf "$test_tmp"' EXIT
mkdir -p "$test_tmp/runtime" "$test_tmp/config" "$test_tmp/data" "$test_tmp/workspace"
chmod 700 "$test_tmp/runtime"

xvfb-run -a --server-args="-screen 0 1280x800x24" sh -eu -c '
    test_root="$1"
    binary="$2"
    export WGPU_BACKEND=gl
    export XDG_RUNTIME_DIR="$test_root/runtime"
    export XDG_CONFIG_HOME="$test_root/config"
    export XDG_DATA_HOME="$test_root/data"
    unset WMCTERM_CONTROL_SOCKET WMCTERM_SOCKET
    unset WMCTERM_WORKSPACE_ID WMCTERM_PANE_ID WMCTERM_SURFACE_ID
    cd "$test_root/workspace"

    "$binary" gui >"$test_root/gui.log" 2>&1 &
    app_pid=$!
    trap "kill $app_pid 2>/dev/null || true; wait $app_pid 2>/dev/null || true" EXIT

    window_id=""
    attempt=0
    while [ "$attempt" -lt 100 ]; do
        window_id=$(xdotool search --name "^wmcterm$" 2>/dev/null | head -n 1 || true)
        [ -n "$window_id" ] && break
        attempt=$((attempt + 1))
        sleep 0.1
    done
    [ -n "$window_id" ] || { cat "$test_root/gui.log" >&2; exit 1; }
    xdotool windowfocus --sync "$window_id"
    sleep 0.8

    # Send space as a NamedKey-style physical key between separate text runs.
    xdotool type --clearmodifiers "printf"
    xdotool key --clearmodifiers space
    xdotool type --clearmodifiers "SPACE_OK"
    xdotool key --clearmodifiers space
    xdotool type --clearmodifiers ">$test_root/space-result"
    xdotool key --clearmodifiers Return
    attempt=0
    while [ ! -f "$test_root/space-result" ] && [ "$attempt" -lt 50 ]; do
        attempt=$((attempt + 1))
        sleep 0.1
    done
    [ "$(sed -n "1p" "$test_root/space-result" 2>/dev/null || true)" = SPACE_OK ] || {
        cat "$test_root/gui.log" >&2
        exit 1
    }

    "$binary" status >"$test_root/status.json"
    workspace_id=$(python3 -c \
        "import json, sys; print(json.load(open(sys.argv[1], encoding=\"utf-8\"))[\"active_workspace\"])" \
        "$test_root/status.json")
    WMCTERM_WORKSPACE_ID="$workspace_id" \
        "$binary" pane split --right --no-focus \
        --command "env >$test_root/child-env; sleep 2" >"$test_root/split.json"
    attempt=0
    while [ ! -f "$test_root/child-env" ] && [ "$attempt" -lt 50 ]; do
        attempt=$((attempt + 1))
        sleep 0.1
    done
    for name in WMCTERM WMCTERM_WORKSPACE_ID WMCTERM_PANE_ID WMCTERM_SURFACE_ID WMCTERM_CONTROL_SOCKET; do
        rg -q "^${name}=" "$test_root/child-env"
    done
    for field in workspace_id pane_id surface_id terminal_id; do
        rg -q "\"${field}\"" "$test_root/split.json"
    done

    echo "runtime input and control smoke tests passed"
    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
    trap - EXIT
' sh "$test_tmp" "$binary"
