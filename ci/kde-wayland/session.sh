#!/bin/sh
# Inside dbus-run-session: start PipeWire, KWin, AT-SPI and the portals, then
# the test app and the test. See run.sh.
set -eu

pids=""
cleanup() {
    for pid in $pids; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT

wait_for() {
    tries=0
    until sh -c "$1" >/dev/null 2>&1; do
        tries=$((tries + 1))
        if [ "$tries" -gt 100 ]; then
            echo "timed out waiting for: $1" >&2
            exit 1
        fi
        sleep 0.2
    done
}

pipewire & pids="$pids $!"
wireplumber & pids="$pids $!"

export WAYLAND_DISPLAY=wayland-todex
# Two outputs; per-output scales need kscreen, which is not running here.
kwin_wayland --virtual --no-lockscreen --no-global-shortcuts \
    --width 1920 --height 1080 --output-count 2 \
    --socket "$WAYLAND_DISPLAY" & pids="$pids $!"
wait_for "test -S '$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY'"
wait_for "busctl --user status org.kde.KWin"

# Accessibility on, as Request access would do.
wait_for "busctl --user call org.a11y.Bus /org/a11y/bus org.a11y.Bus GetAddress"
busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true

/usr/lib/xdg-desktop-portal-kde & pids="$pids $!"
/usr/lib/xdg-desktop-portal --replace & pids="$pids $!"
wait_for "busctl --user status org.freedesktop.portal.Desktop"

# Pre-authorize remote control for TodeX's app id in this private bus only,
# so KDE's consent dialog does not wait for a person.
busctl --user call org.freedesktop.impl.portal.PermissionStore \
    /org/freedesktop/impl/portal/PermissionStore \
    org.freedesktop.impl.portal.PermissionStore SetPermission \
    sbssas kde-authorized true remote-desktop com.unbaked0692.todex.agentd 1 yes

python3 ci/kde-wayland/test_app.py & pids="$pids $!"

cargo test --locked kde_wayland -- --ignored --nocapture --test-threads=1
