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

# Build first so the session only runs the test.
cargo test --locked --no-run

exec dbus-run-session -- sh "$root/ci/kde-wayland/session.sh"
