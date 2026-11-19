#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Installs the engine for the current user, without root:
#   ~/.local/lib/pods/podsd, ~/.local/lib/pods/podsctl
#   ~/.config/systemd/user/pods-engine.service
#   ~/.local/share/dbus-1/services/io.github.d101c.Pods.service
# The engine then starts on the first D-Bus call (the extension's).
#
#   scripts/install-local.sh            build in release mode and install
#   scripts/install-local.sh --no-build install what target/release holds
#   scripts/install-local.sh --uninstall
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
libdir="$HOME/.local/lib/pods"
unitdir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
busdir="${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services"

if [ "${1:-}" = "--uninstall" ]; then
    systemctl --user disable --now pods-engine.service 2>/dev/null || true
    rm -f "$unitdir/pods-engine.service" "$busdir/io.github.d101c.Pods.service"
    rm -f "$libdir/podsd" "$libdir/podsctl"
    rmdir "$libdir" 2>/dev/null || true
    systemctl --user daemon-reload
    echo "pods engine removed (settings kept in ~/.config/pods)"
    exit 0
fi

if [ "${1:-}" != "--no-build" ]; then
    (cd "$here" && cargo build --release --locked)
fi

install -d -m 0755 "$libdir" "$unitdir" "$busdir"
install -m 0755 "$here/target/release/podsd" "$libdir/podsd"
install -m 0755 "$here/target/release/podsctl" "$libdir/podsctl"
install -m 0644 "$here/data/pods-engine.service" "$unitdir/pods-engine.service"
sed "s|@PODSD@|$libdir/podsd|" "$here/data/io.github.d101c.Pods.service" \
    > "$busdir/io.github.d101c.Pods.service"
chmod 0644 "$busdir/io.github.d101c.Pods.service"

systemctl --user daemon-reload
# A running engine keeps the old binary until restarted.
systemctl --user try-restart pods-engine.service || true
echo "pods engine installed in $libdir"
