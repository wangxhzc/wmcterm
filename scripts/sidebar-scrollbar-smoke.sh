#!/usr/bin/env bash
set -euo pipefail

for tool in xvfb-run xdotool import python3; do
    command -v "$tool" >/dev/null || { echo "missing dependency: $tool" >&2; exit 1; }
done
python3 -c 'from PIL import Image'
if [ "$#" -gt 0 ]; then
    binary=$(realpath "$1")
else
    cargo build --quiet --bin wmcterm
    binary="$PWD/target/debug/wmcterm"
fi
test_root=$(mktemp -d)
trap 'rm -rf "$test_root"' EXIT
artifacts=${2:-$test_root/screenshots}
mkdir -p "$test_root"/{runtime,config,data,workspace} "$artifacts"
artifacts=$(realpath "$artifacts")
chmod 700 "$test_root/runtime"

xvfb-run -a --server-args='-screen 0 1280x800x24' bash -eu -c '
    test_root=$1
    binary=$2
    artifacts=$3
    export WMCTERM_UI_LANGUAGE=zh-CN WGPU_BACKEND=gl
    export XDG_RUNTIME_DIR="$test_root/runtime" XDG_CONFIG_HOME="$test_root/config" XDG_DATA_HOME="$test_root/data"
    unset WMCTERM_CONTROL_SOCKET WMCTERM_SOCKET WMCTERM_WORKSPACE_ID WMCTERM_PANE_ID WMCTERM_SURFACE_ID
    cd "$test_root/workspace"
    "$binary" gui > "$test_root/gui.log" 2>&1 &
    app_pid=$!
    trap '\''kill "$app_pid" 2>/dev/null || true; wait "$app_pid" 2>/dev/null || true'\'' EXIT
    window_id=""
    for attempt in $(seq 1 100); do
        window_id=$(xdotool search --name "^wmcterm$" 2>/dev/null | head -n1 || true)
        [ -n "$window_id" ] && break
        sleep .1
    done
    [ -n "$window_id" ]
    xdotool windowsize "$window_id" 1000 650
    xdotool windowfocus --sync "$window_id"
    sleep 1
    capture() { sleep .3; import -window "$window_id" "$artifacts/$1.png"; }
    capture fits
    for index in $(seq -w 1 18); do
        "$binary" workspace new --name "Workspace-$index" > /dev/null
    done
    "$binary" workspace list > "$artifacts/before.json"
    xdotool mousemove --window "$window_id" 500 350
    capture top
    xdotool mousemove --window "$window_id" 247 100 mousedown 1
    xdotool mousemove --window "$window_id" 247 610
    xdotool mouseup 1
    xdotool mousemove --window "$window_id" 500 350
    capture bottom
    xdotool mousemove --window "$window_id" 247 90 click 1
    xdotool mousemove --window "$window_id" 500 350
    capture track-top
    # Losing focus must cancel a captured drag, including after refocusing.
    xdotool mousemove --window "$window_id" 247 100 mousedown 1
    xdotool windowfocus 0
    xdotool mouseup 1
    xdotool windowfocus --sync "$window_id"
    xdotool mousemove --window "$window_id" 247 610
    xdotool mousemove --window "$window_id" 500 350
    capture focus-cancel
    # Wheel input still scrolls, without activating or reordering a workspace.
    xdotool mousemove --window "$window_id" 100 300
    xdotool click --repeat 30 --delay 15 5
    xdotool mousemove --window "$window_id" 500 350
    capture wheel-bottom
    "$binary" workspace list > "$artifacts/after.json"
    cmp "$artifacts/before.json" "$artifacts/after.json"
    # Search filters away overflow and releases the reserved rail space.
    xdotool mousemove --window "$window_id" 100 64 click 1
    xdotool type --clearmodifiers Workspace-18
    capture filtered
    xdotool key ctrl+a BackSpace
    xdotool mousemove --window "$window_id" 500 350
    capture edge-idle
    xdotool mousemove --window "$window_id" 260 350
    sleep .6
    capture edge-hover
    xdotool mousedown 1
    capture edge-drag
    xdotool mouseup 1
    xdotool mousemove --window "$window_id" 500 350
    capture edge-leave
    xdotool mousemove --window "$window_id" 260 350 mousedown 1
    xdotool mousemove --window "$window_id" 320 350 mouseup 1
    xdotool mousemove --window "$window_id" 500 350
    capture resized
    xdotool mousemove --window "$window_id" 500 350 click 1
    xdotool key ctrl+shift+d
    sleep .4
    xdotool key ctrl+shift+e
    sleep .4
    xdotool mousemove --window "$window_id" 660 350
    capture split-dividers
    xdotool mousemove --window "$window_id" 500 350
    capture split-idle
' bash "$test_root" "$binary" "$artifacts"

python3 - "$artifacts" <<'PY'
from pathlib import Path
import sys
from PIL import Image
root = Path(sys.argv[1])
def pixel(name, x, y):
    with Image.open(root / f'{name}.png') as image:
        return sum(image.convert('RGB').getpixel((x, y)))
for name in ['top', 'track-top', 'focus-cancel']:
    assert pixel(name, 246, 100) > pixel(name, 246, 600), name
for name in ['bottom', 'wheel-bottom']:
    assert pixel(name, 246, 640) > pixel(name, 246, 100), name
for name in ['fits', 'filtered']:
    assert pixel(name, 246, 300) == pixel(name, 100, 300), name
assert pixel('resized', 306, 100) > pixel('resized', 306, 600)
with Image.open(root / 'edge-idle.png') as idle:
    assert idle.getpixel((260, 100)) == idle.getpixel((260, 648))
    for name in ['edge-hover', 'edge-drag']:
        with Image.open(root / f'{name}.png') as active:
            # Exactly three continuous pixels: center, one left and one right.
            color = active.getpixel((260, 100))
            for x in [259, 260, 261]:
                for y in [100, 300, 648, 649]:
                    assert active.getpixel((x, y)) == color, (name, x, y)
            for x in [258, 262]:
                assert active.getpixel((x, 648)) == idle.getpixel((x, 648)), (name, x)
            assert active.getpixel((260, 100)) != idle.getpixel((260, 100)), name
with Image.open(root / 'edge-leave.png') as leave, Image.open(root / 'edge-idle.png') as idle:
    assert leave.getpixel((259, 648)) == idle.getpixel((259, 648))
with Image.open(root / 'split-dividers.png') as hover, Image.open(root / 'split-idle.png') as idle:
    assert hover.getpixel((660, 500)) != idle.getpixel((660, 500))
print('sidebar scrollbar smoke passed: drag, track, wheel, focus, filter, resize, edge rendering')
PY
