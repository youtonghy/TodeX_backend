#!/bin/sh
# KDE Plasma Wayland Computer Use test (src/computer/kde_wayland_e2e.rs) in a
# private D-Bus session and a headless KWin. Run from the repository root in
# a disposable machine or container (the kde-wayland GitHub workflow): it
# pre-authorizes TodeX for remote control in that private session's
# permission store, which must never be done on a real desktop.
set -eu

root=$(pwd)
scratch=$(mktemp -d)
export XDG_RUNTIME_DIR="$scratch/runtime"
export XDG_DATA_HOME="$scratch/data"
export XDG_STATE_HOME="$scratch/state"
export XDG_CONFIG_HOME="$scratch/config"
export XDG_CACHE_HOME="$scratch/cache"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME"
chmod 700 "$XDG_RUNTIME_DIR"
export XDG_CURRENT_DESKTOP=KDE
export XDG_SESSION_TYPE=wayland
export KDE_FULL_SESSION=true
export QT_QPA_PLATFORM=wayland
export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
export TODEX_KDE_WAYLAND_E2E=1
# KWin captures screens only with OpenGL on a GPU render node; hosted CI
# runners have none, so the test skips screenshots and portal input there.
if ! ls /dev/dri/renderD* >/dev/null 2>&1; then
    echo "no render node: testing observation and window focus only" >&2
    export TODEX_KDE_WAYLAND_E2E_NO_GPU=1
fi

# Build first so the session only runs the test.
cargo test --locked --no-run

exec dbus-run-session -- sh "$root/ci/kde-wayland/session.sh"
