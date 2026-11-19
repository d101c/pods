#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# The extension in a real GNOME Shell, headless, against the fake engine.
#
#   extension/tools/shell-test/run.sh               all the checks, screenshots in out/
#   SCHEME=prefer-light extension/tools/shell-test/run.sh     light style
#   SCALE=2 extension/tools/shell-test/run.sh       a HiDPI screen
#
# Everything is private to the run: HOME, XDG dirs, runtime dir, the session
# bus (dbus-run-session) and even the system bus (an empty bus of our own, so
# the Shell sees no BlueZ, no UPower, no NetworkManager). The extension is the
# one pack.sh builds for extensions.gnome.org, unpacked; the user's own
# extensions, settings, sound and Bluetooth are never touched.
#
# Outputs in extension/tools/shell-test/out: *.json (what was on screen at each
# step), *.png (screenshots), shell.log, engine.log.
set -u
S="$(cd "$(dirname "$0")" && pwd)"
EXT="$(cd "$S/../.." && pwd)"
UUID="pods@d101c.github.io"
OUT="${OUT:-$S/out}"

rm -rf "$OUT"; mkdir -p "$OUT"
echo "# model.js"
GIO_USE_VFS=local gjs -m "$EXT/tools/test-model.js" > "$OUT/model.txt" 2>&1 \
    || { cat "$OUT/model.txt"; echo "FAIL model.js"; exit 1; }
echo "ok   model.js: $(grep -c '^ok' "$OUT/model.txt") checks"
RT=$(mktemp -d /tmp/pods-test.XXXX); chmod 700 "$RT"
T="$RT/home"
mkdir -p "$T/config" "$T/data/gnome-shell/extensions/$UUID" "$T/cache"
"$EXT/scripts/pack.sh" "$RT/dist" > /dev/null || { echo "FAIL pack.sh"; exit 1; }
unzip -q "$RT/dist/$UUID.shell-extension.zip" -d "$T/data/gnome-shell/extensions/$UUID"
# What the Shell does when it installs an extension from extensions.gnome.org.
glib-compile-schemas --strict "$T/data/gnome-shell/extensions/$UUID/schemas" || { echo "FAIL schemas"; exit 1; }
cp -r "$S/probe@test" "$T/data/gnome-shell/extensions/"
# No "Screen Lock disabled" notice (there is no GDM here) over the screenshots.
touch "$T/data/gnome-shell/lock-warning-shown"

# An empty system bus of our own.
dbus-daemon --session --address="unix:path=$RT/system_bus" --fork --print-pid > "$RT/system_bus.pid"
SYSBUS_PID=$(cat "$RT/system_bus.pid")

env -u WAYLAND_DISPLAY -u DISPLAY -u DBUS_SESSION_BUS_ADDRESS -u XDG_SESSION_ID \
    -u PULSE_SERVER -u PIPEWIRE_REMOTE -u SESSION_MANAGER \
  HOME="$T" XDG_CONFIG_HOME="$T/config" XDG_DATA_HOME="$T/data" XDG_CACHE_HOME="$T/cache" \
  XDG_STATE_HOME="$T/state" XDG_RUNTIME_DIR="$RT" GSETTINGS_BACKEND=dconf GIO_USE_VFS=local \
  DBUS_SYSTEM_BUS_ADDRESS="unix:path=$RT/system_bus" \
  PROBE_DIR="$OUT" EXT="$EXT" UUID="$UUID" \
  SCALE="${SCALE:-1}" SCHEME="${SCHEME:-prefer-dark}" LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 LANGUAGE= \
  dbus-run-session -- "$S/inner.sh" 2> "$OUT/session.log"
status=$?
kill "$SYSBUS_PID" 2>/dev/null
sleep 0.5
rm -rf "$RT" 2>/dev/null
exit $status
