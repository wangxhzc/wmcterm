#!/usr/bin/env bash
set -euo pipefail

for tool in xvfb-run xdotool xprop import identify compare; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing UI test dependency: $tool" >&2
        exit 1
    fi
done

cargo build --quiet --bin wmcterm
ui_tmp="$(mktemp -d)"
trap 'rm -rf "$ui_tmp"' EXIT
mkdir -p "$ui_tmp/runtime" "$ui_tmp/config" "$ui_tmp/data" "$ui_tmp/workspace"
chmod 700 "$ui_tmp/runtime"

for language in en zh-CN; do
    screenshot="$ui_tmp/wmcterm-$language.png"
    palette_screenshot="$ui_tmp/wmcterm-$language-palette.png"
    sidebar_hover_screenshot="$ui_tmp/wmcterm-$language-sidebar-hover.png"
    workspace_menu_screenshot="$ui_tmp/wmcterm-$language-workspace-menu.png"
    tab_menu_screenshot="$ui_tmp/wmcterm-$language-tab-menu.png"
    split_screenshot="$ui_tmp/wmcterm-$language-split.png"
    sidebar_hidden_screenshot="$ui_tmp/wmcterm-$language-sidebar-hidden.png"
    notifications_screenshot="$ui_tmp/wmcterm-$language-notifications.png"
    empty_workspace_screenshot="$ui_tmp/wmcterm-$language-empty-workspace.png"
    empty_pane_screenshot="$ui_tmp/wmcterm-$language-empty-pane.png"
    terminal_menu_screenshot="$ui_tmp/wmcterm-$language-terminal-menu.png"
    sidebar_resize_screenshot="$ui_tmp/wmcterm-$language-sidebar-resize.png"
    prompt_screenshot="$ui_tmp/wmcterm-$language-prompt.png"
    full_access_prompt_screenshot="$ui_tmp/wmcterm-$language-full-access-prompt.png"
    xvfb-run -a --server-args="-screen 0 1280x800x24" sh -eu -c '
        export WMCTERM_UI_LANGUAGE="$1"
        export WGPU_BACKEND=gl
        export XDG_RUNTIME_DIR="$2/runtime"
        export XDG_CONFIG_HOME="$2/config"
        export XDG_DATA_HOME="$2/data"
        unset WMCTERM_CONTROL_SOCKET WMCTERM_SOCKET
        unset WMCTERM_WORKSPACE_ID WMCTERM_PANE_ID WMCTERM_SURFACE_ID
        cd "$2/workspace"
        "$3" gui >"$2/$1.log" 2>&1 &
        app_pid=$!
        trap "kill $app_pid 2>/dev/null || true" EXIT
        window_id=""
        attempt=0
        while [ "$attempt" -lt 80 ]; do
            window_id=$(xdotool search --name "^wmcterm$" 2>/dev/null | head -n 1 || true)
            [ -n "$window_id" ] && break
            attempt=$((attempt + 1))
            sleep 0.1
        done
        [ -n "$window_id" ] || { cat "$2/$1.log" >&2; exit 1; }
        xprop -id "$window_id" _NET_WM_ICON | grep -q "_NET_WM_ICON(CARDINAL)"
        xdotool windowsize "$window_id" 1000 650
        # Allow the resize redraw and glyph-atlas upload (notably CJK fallback)
        # to settle before capturing the assertion artifact.
        sleep 1.2
        screenshot_attempt=0
        while ! import -window "$window_id" "$4" 2>/dev/null; do
            screenshot_attempt=$((screenshot_attempt + 1))
            [ "$screenshot_attempt" -lt 20 ] || { cat "$2/$1.log" >&2; exit 1; }
            sleep 0.1
        done
        identify "$4" >/dev/null
        xdotool mousemove --window "$window_id" 260 300
        xdotool mousedown 1
        xdotool mousemove --window "$window_id" 320 300
        sleep 0.2
        import -window "$window_id" "${15}"
        identify "${15}" >/dev/null
        xdotool mouseup 1
        xdotool mousemove --window "$window_id" 320 300
        xdotool mousedown 1
        xdotool mousemove --window "$window_id" 260 300
        xdotool mouseup 1
        sleep 0.2
        xdotool windowfocus --sync "$window_id"
        xdotool mousemove --window "$window_id" 20 20
        xdotool click 1
        sleep 0.2
        import -window "$window_id" "${10}"
        identify "${10}" >/dev/null
        xdotool click 1
        xdotool mousemove --window "$window_id" 860 20
        xdotool click 1
        sleep 0.2
        import -window "$window_id" "${11}"
        identify "${11}" >/dev/null
        xdotool click 1
        xdotool mousemove --window "$window_id" 235 64
        sleep 0.2
        import -window "$window_id" "$6"
        identify "$6" >/dev/null
        xdotool mousemove --window "$window_id" 80 105
        xdotool click 3
        sleep 0.2
        # Rename is the fourth 30px row after the workspace menu opens at y=105.
        xdotool mousemove --window "$window_id" 100 210
        xdotool click 1
        sleep 0.2
        import -window "$window_id" "${16}"
        identify "${16}" >/dev/null
        xdotool key Escape
        xdotool mousemove --window "$window_id" 80 105
        xdotool click 3
        sleep 0.2
        import -window "$window_id" "$7"
        identify "$7" >/dev/null
        # Close is the seventh 30px row in the workspace menu.
        xdotool mousemove --window "$window_id" 100 300
        xdotool click 1
        sleep 0.2
        import -window "$window_id" "${12}"
        identify "${12}" >/dev/null
        xdotool mousemove --window "$window_id" 630 347
        xdotool click 1
        sleep 0.3
        xdotool mousemove --window "$window_id" 330 52
        xdotool click 3
        sleep 0.2
        import -window "$window_id" "$9"
        identify "$9" >/dev/null
        xdotool mousemove --window "$window_id" 350 68
        xdotool click 1
        sleep 0.2
        import -window "$window_id" "${13}"
        identify "${13}" >/dev/null
        xdotool mousemove --window "$window_id" 630 347
        xdotool click 1
        sleep 0.3
        xdotool mousemove --window "$window_id" 600 220
        xdotool click 3
        sleep 0.2
        import -window "$window_id" "${14}"
        identify "${14}" >/dev/null
        xdotool key Escape
        xdotool key ctrl+shift+d
        sleep 0.4
        import -window "$window_id" "$8"
        identify "$8" >/dev/null
        xdotool mousemove --window "$window_id" 500 20
        xdotool click 1
        sleep 0.3
        import -window "$window_id" "$5"
        identify "$5" >/dev/null
        xdotool key Escape
        xdotool key ctrl+shift+p
        xdotool type --delay 10 "agent.codex.full_access"
        xdotool key Return
        sleep 0.3
        import -window "$window_id" "${17}"
        identify "${17}" >/dev/null
        xdotool key Escape
        kill "$app_pid" 2>/dev/null || true
        wait "$app_pid" 2>/dev/null || true
        trap - EXIT
    ' sh "$language" "$ui_tmp" "$PWD/target/debug/wmcterm" "$screenshot" "$palette_screenshot" "$sidebar_hover_screenshot" "$workspace_menu_screenshot" "$split_screenshot" "$tab_menu_screenshot" "$sidebar_hidden_screenshot" "$notifications_screenshot" "$empty_workspace_screenshot" "$empty_pane_screenshot" "$terminal_menu_screenshot" "$sidebar_resize_screenshot" "$prompt_screenshot" "$full_access_prompt_screenshot"

    dimensions="$(identify -format '%wx%h' "$screenshot")"
    [ "$dimensions" = "1000x650" ] || {
        echo "unexpected $language screenshot size: $dimensions" >&2
        exit 1
    }
    entropy="$(identify -format '%[entropy]' "$screenshot")"
    awk -v entropy="$entropy" 'BEGIN { exit !(entropy > 0.02) }' || {
        echo "$language UI screenshot appears blank (entropy=$entropy)" >&2
        exit 1
    }
    cp "$screenshot" "target/ui-smoke-$language.png"
    hover_difference="$(compare -metric AE "$screenshot" \
        "$sidebar_hover_screenshot" null: 2>&1 || true)"
    awk -v pixels="$hover_difference" 'BEGIN { exit !(pixels > 100) }' || {
        echo "$language sidebar hover did not visibly highlight a workspace" >&2
        exit 1
    }
    cp "$sidebar_hover_screenshot" "target/ui-smoke-$language-sidebar-hover.png"
    sidebar_resize_difference="$(compare -metric AE "$screenshot" \
        "$sidebar_resize_screenshot" null: 2>&1 || true)"
    awk -v pixels="$sidebar_resize_difference" 'BEGIN { exit !(pixels > 10000) }' || {
        echo "$language sidebar resize did not update the layout in real time" >&2
        exit 1
    }
    cp "$sidebar_resize_screenshot" "target/ui-smoke-$language-sidebar-resize.png"
    sidebar_hidden_difference="$(compare -metric AE "$screenshot" \
        "$sidebar_hidden_screenshot" null: 2>&1 || true)"
    awk -v pixels="$sidebar_hidden_difference" 'BEGIN { exit !(pixels > 10000) }' || {
        echo "$language sidebar navigation button did not hide the sidebar" >&2
        exit 1
    }
    cp "$sidebar_hidden_screenshot" "target/ui-smoke-$language-sidebar-hidden.png"
    notifications_difference="$(compare -metric AE "$screenshot" \
        "$notifications_screenshot" null: 2>&1 || true)"
    awk -v pixels="$notifications_difference" 'BEGIN { exit !(pixels > 10000) }' || {
        echo "$language notifications navigation button did not open its panel" >&2
        exit 1
    }
    cp "$notifications_screenshot" "target/ui-smoke-$language-notifications.png"
    workspace_menu_difference="$(compare -metric AE "$sidebar_hover_screenshot" \
        "$workspace_menu_screenshot" null: 2>&1 || true)"
    awk -v pixels="$workspace_menu_difference" 'BEGIN { exit !(pixels > 1000) }' || {
        echo "$language workspace context menu did not visibly open" >&2
        exit 1
    }
    cp "$workspace_menu_screenshot" "target/ui-smoke-$language-workspace-menu.png"
    prompt_difference="$(compare -metric AE "$screenshot" \
        "$prompt_screenshot" null: 2>&1 || true)"
    awk -v pixels="$prompt_difference" 'BEGIN { exit !(pixels > 10000) }' || {
        echo "$language workspace rename prompt did not visibly open" >&2
        exit 1
    }
    cp "$prompt_screenshot" "target/ui-smoke-$language-prompt.png"
    full_access_prompt_difference="$(compare -metric AE "$screenshot" \
        "$full_access_prompt_screenshot" null: 2>&1 || true)"
    awk -v pixels="$full_access_prompt_difference" 'BEGIN { exit !(pixels > 10000) }' || {
        echo "$language full access confirmation prompt did not visibly open" >&2
        exit 1
    }
    cp "$full_access_prompt_screenshot" "target/ui-smoke-$language-full-access-prompt.png"
    empty_workspace_difference="$(compare -metric AE "$screenshot" \
        "$empty_workspace_screenshot" null: 2>&1 || true)"
    awk -v pixels="$empty_workspace_difference" 'BEGIN { exit !(pixels > 10000) }' || {
        echo "$language final workspace did not close to an empty state" >&2
        exit 1
    }
    cp "$empty_workspace_screenshot" "target/ui-smoke-$language-empty-workspace.png"
    tab_menu_difference="$(compare -metric AE "$screenshot" \
        "$tab_menu_screenshot" null: 2>&1 || true)"
    awk -v pixels="$tab_menu_difference" 'BEGIN { exit !(pixels > 1000) }' || {
        echo "$language terminal tab context menu did not visibly open" >&2
        exit 1
    }
    cp "$tab_menu_screenshot" "target/ui-smoke-$language-tab-menu.png"
    terminal_menu_difference="$(compare -metric AE "$screenshot" \
        "$terminal_menu_screenshot" null: 2>&1 || true)"
    awk -v pixels="$terminal_menu_difference" 'BEGIN { exit !(pixels > 1000) }' || {
        echo "$language terminal content context menu did not visibly open" >&2
        exit 1
    }
    cp "$terminal_menu_screenshot" "target/ui-smoke-$language-terminal-menu.png"
    empty_pane_difference="$(compare -metric AE "$screenshot" \
        "$empty_pane_screenshot" null: 2>&1 || true)"
    awk -v pixels="$empty_pane_difference" 'BEGIN { exit !(pixels > 1000) }' || {
        echo "$language final terminal tab did not close to an empty pane" >&2
        exit 1
    }
    cp "$empty_pane_screenshot" "target/ui-smoke-$language-empty-pane.png"
    split_difference="$(compare -metric AE "$screenshot" \
        "$split_screenshot" null: 2>&1 || true)"
    awk -v pixels="$split_difference" 'BEGIN { exit !(pixels > 1000) }' || {
        echo "$language split pane did not visibly render" >&2
        exit 1
    }
    cp "$split_screenshot" "target/ui-smoke-$language-split.png"
    palette_dimensions="$(identify -format '%wx%h' "$palette_screenshot")"
    [ "$palette_dimensions" = "1000x650" ] || {
        echo "unexpected $language palette screenshot size: $palette_dimensions" >&2
        exit 1
    }
    palette_difference="$(compare -metric AE "$screenshot" \
        "$palette_screenshot" null: 2>&1 || true)"
    awk -v pixels="$palette_difference" 'BEGIN { exit !(pixels > 1000) }' || {
        echo "$language command palette did not visibly open" >&2
        exit 1
    }
    cp "$palette_screenshot" "target/ui-smoke-$language-palette.png"
done

different_pixels="$(compare -metric AE target/ui-smoke-en.png \
    target/ui-smoke-zh-CN.png null: 2>&1 || true)"
awk -v pixels="$different_pixels" 'BEGIN { exit !(pixels > 100) }' || {
    echo "English and Chinese UI screenshots are unexpectedly identical" >&2
    exit 1
}

echo "UI smoke tests passed; base and palette screenshots are in target/ui-smoke-*.png"
