#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Runs inside the private D-Bus session opened by run.sh.
set -u
S="$(cd "$(dirname "$0")" && pwd)"; OUT="$PROBE_DIR"; RT="$XDG_RUNTIME_DIR"
failed=0
W=$((1280 * SCALE)); H=$((800 * SCALE))

probe() {   # probe <command...>: hands one command to probe@test and waits
    echo "$*" > "$OUT/cmd.tmp"; mv "$OUT/cmd.tmp" "$OUT/cmd"   # whole or not at all
    for _ in $(seq 100); do [ -e "$OUT/cmd" ] || return 0; sleep 0.05; done
    echo "FAIL the probe did not take: $*"; failed=1
}
field() {   # field <state> <python expression on d>: takes a state, prints the value
    probe state "$1"
    python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(json.dumps($2, ensure_ascii=False))" \
        "$OUT/$1.json" 2>&1 | tail -1
}
expect() {  # expect <label> <got> <wanted>
    if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1"; echo "       got:    $2"; echo "       wanted: $3"; failed=1; fi
}
shot() {    # shot <name> [qs]: screenshot; with qs, also the Quick Settings corner
    probe shot "$1"
    for _ in $(seq 60); do tail -c 12 "$OUT/$1.png" 2>/dev/null | grep -q IEND && break; sleep 0.05; done
    if [ "${2:-}" = qs ]; then
        magick "$OUT/$1.png" -crop "$((470 * SCALE))x${H}+$((W - 470 * SCALE))+0" +repage "PNG8:$OUT/$1-qs.png"
    fi
    magick "$OUT/$1.png" -colors 255 "PNG8:$OUT/$1.png"
}
engine() { echo "$*" >&3; sleep "${ENGINE_WAIT:-0.8}"; }
start_engine() {
    rm -f "$RT/engine.fifo"; mkfifo "$RT/engine.fifo"
    python3 "$EXT/tools/fake-engine.py" "$@" < "$RT/engine.fifo" >> "$OUT/engine.log" 2>&1 &
    ENGINE=$!
    exec 3> "$RT/engine.fifo"
    for _ in $(seq 50); do grep -q READY "$OUT/engine.log" 2>/dev/null && break; sleep 0.1; done
    sleep 0.8
}
stop_engine() { exec 3>&-; kill "$ENGINE" 2>/dev/null; wait "$ENGINE" 2>/dev/null; sleep 1.2; }
called() { grep -c "^CALL $1" "$OUT/engine.log"; }
rules() {   # rules <python expression on r>: the Shell's D-Bus match rules about the engine
    local shell
    shell=$(gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus \
        -m org.freedesktop.DBus.GetNameOwner org.gnome.Shell | sed -n "s/.*'\(:[0-9.]*\)'.*/\1/p")
    gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus \
        -m org.freedesktop.DBus.Debug.Stats.GetAllMatchRules > "$OUT/rules.txt"
    python3 -c "
import ast, json, re, sys
text = re.sub(r'@as ', '', open(sys.argv[1]).read())
r = [x for x in ast.literal_eval(text)[0].get(sys.argv[2], []) if 'io.github.d101c.Pods' in x]
print(json.dumps($1))" "$OUT/rules.txt" "$shell" 2>&1 | tail -1
}
A11Y() { python3 "$S/a11y.py" "$@"; }   # the settings window, by role and name
until_true() {  # until_true <seconds> <command...>: until it prints true
    local end=$((SECONDS + $1)); shift
    while [ $SECONDS -lt $end ]; do [ "$("$@")" = true ] && return 0; sleep 0.25; done
    return 1
}
owned() {   # owned <bus name>: true or false
    gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus \
        -m org.freedesktop.DBus.NameHasOwner "$1" | grep -q true && echo true || echo false
}
activatable() {   # the engine can be started by the bus: 1 or 0
    gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus \
        -m org.freedesktop.DBus.ListActivatableNames | grep -c "'io.github.d101c.Pods'"
}
cleanup() { kill "${SH:-}" "${ENGINE:-}" "${REG:-}" "${HTTP:-}" "${SYSTEMD:-}" 2>/dev/null; }
trap cleanup EXIT

# Safety: this must be a private bus where nothing provides the engine.
case "${DBUS_SESSION_BUS_ADDRESS:-}" in
    ""|"unix:path=/run/user/$(id -u)/bus"*) echo "FAIL not on a private session bus"; exit 1;;
esac
if gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus \
        -m org.freedesktop.DBus.ListActivatableNames | grep -q "io.github.d101c.Pods"; then
    echo "FAIL a real engine could be activated on this bus; stopping"; exit 1
fi

gsettings set org.gnome.shell disable-user-extensions false
gsettings set org.gnome.desktop.interface color-scheme "$SCHEME"
gsettings set org.gnome.desktop.notifications show-banners true
[ "$SCALE" = 1 ] || gsettings set org.gnome.desktop.interface scaling-factor "$SCALE"
gnome-shell --headless --wayland --no-x11 --virtual-monitor "${W}x${H}" \
    --wayland-display=wayland-pods > "$OUT/shell.log" 2>&1 &
SH=$!
# Up when the Shell answers for its extensions, then a moment to settle.
for _ in $(seq 60); do
    gdbus call --session -d org.gnome.Shell -o /org/gnome/Shell \
        -m org.gnome.Shell.Extensions.ListExtensions > /dev/null 2>&1 && break
    sleep 0.5
done
sleep 2
kill -0 "$SH" 2>/dev/null || { echo "FAIL the Shell did not start:"; tail -20 "$OUT/shell.log"; exit 1; }
# So that the preferences window, started by the bus, opens in this Shell.
dbus-update-activation-environment WAYLAND_DISPLAY=wayland-pods
# The accessibility registry, which this bus cannot start through systemd:
# a11y.py reads and drives the settings window through it.
/usr/libexec/at-spi2-registryd > /dev/null 2>&1 &
REG=$!

gnome-extensions enable probe@test
for i in $(seq 40); do
    echo "state s00" > "$OUT/cmd.tmp"; mv "$OUT/cmd.tmp" "$OUT/cmd"; sleep 0.5
    [ -e "$OUT/s00.json" ] && break
    [ "$i" = 10 ] && gnome-extensions enable probe@test
done
rm -f "$OUT/cmd"
[ -e "$OUT/s00.json" ] || { echo "FAIL the probe does not answer"; gnome-extensions info probe@test; gsettings get org.gnome.shell enabled-extensions; grep -i -A3 "probe\|JS ERROR" "$OUT/shell.log" | head; exit 1; }
ICONS="$(field s00 "d['panelIcons']")"
OVERLAY="$(field s00 "d['overlayChildren']")"
gnome-extensions enable "$UUID"
sleep 2
for _ in $(seq 20); do
    STATE="$(gnome-extensions info "$UUID" 2>/dev/null | sed -n 's/^ *State: //p')"
    [ "$STATE" = ACTIVE ] && break; sleep 0.5
done
expect "the extension is enabled" "$STATE" "ACTIVE"

# ---------------------------------------------------------------------------
echo "# No engine"
expect "no engine: no tile" "$(field s01 "[d['found'], d['visible']]")" '[true, false]'
expect "no engine: nothing added to the top bar" "$(field s01 "d['panelIcons']")" "$ICONS"
expect "no engine: one notification offers to install it" \
    "$(field s01 "[[n['title'], n['actions']] for n in d['notifications'] if n['source'] == 'AirPods']")" \
    '[["Install the AirPods engine", ["Install…"]]]'
shot 20-notification-install

# ---------------------------------------------------------------------------
echo "# Engine install"
# A release of our own: podsd is the fake engine, the unit and the D-Bus file
# are the engine's, and a megabyte and a half of padding gives the download
# some length. Served slowly on 127.0.0.1; a stand-in systemd starts it.
REL="$RT/release"; NAME="pods-engine-9.9.9-$(uname -m)"
mkdir -p "$REL/$NAME"
printf '#!/bin/sh\nexec python3 %s idle >> %s 2>&1\n' "$EXT/tools/fake-engine.py" "$OUT/engine.log" > "$REL/$NAME/podsd"
chmod 755 "$REL/$NAME/podsd"
cp "$EXT/../engine/data/pods-engine.service" "$EXT/../engine/data/io.github.d101c.Pods.service" "$REL/$NAME/"
head -c 1500000 /dev/urandom > "$REL/$NAME/padding"
tar -C "$REL" -czf "$REL/$NAME.tar.gz" "$NAME"
python3 "$S/serve.py" "$REL" "$RT/port" 400000 > "$OUT/http.log" 2>&1 &
HTTP=$!
python3 "$S/fake-systemd.py" > "$OUT/systemd.log" 2>&1 &
SYSTEMD=$!
for _ in $(seq 50); do [ -s "$RT/port" ] && grep -q READY "$OUT/systemd.log" && break; sleep 0.1; done
# The extension under test points at that release instead of GitHub's.
python3 "$S/release-json.py" "$REL/$NAME.tar.gz" "http://127.0.0.1:$(cat "$RT/port")/$NAME.tar.gz" \
    > "$XDG_DATA_HOME/gnome-shell/extensions/$UUID/engine-release.json"
LIB="$HOME/.local/lib/pods"; UNIT="$XDG_CONFIG_HOME/systemd/user/pods-engine.service"
BUSFILE="$XDG_DATA_HOME/dbus-1/services/io.github.d101c.Pods.service"

probe notifaction "Install…"
until_true 15 A11Y has button "Install Engine"
expect "Install…: AirPods Settings open, notification gone" \
    "$(field i01 "[len(d['windows']), [n for n in d['notifications'] if n['source'] == 'AirPods']]")" '[1, []]'
expect "install offered: banner with Install Engine" \
    "$(A11Y label "The AirPods engine") $(A11Y has button "Install Engine")" '"The AirPods engine is not installed" true'
expect "install offered: what, how big, from where" "$(A11Y label "Pods talks")" \
    '"Pods talks to your AirPods through the pods engine, a small background service. Install Engine downloads version 9.9.9 (1.5 MB) from 127.0.0.1, checks it against the fingerprint this extension carries, and installs it in your home folder. No administrator password is needed."'
expect "nothing downloaded before the click" "$(grep -c GET "$OUT/http.log")" 0
shot 21-prefs-install

touch "$REL/tamper"
A11Y click button "Install Engine"
until_true 20 A11Y has button "Try Again"
expect "corrupt download: refused, and said so" "$(A11Y label "Could not install")" \
    '"Could not install the AirPods engine: the download does not match the engine this extension expects"'
expect "corrupt download: nothing installed, systemd not asked" \
    "$(ls -A "$LIB" "$UNIT" "$BUSFILE" 2>/dev/null | wc -l) $(grep -cE '^SYSTEMD (Reload|StartUnit|RestartUnit|StopUnit)' "$OUT/systemd.log")" '0 0'
expect "corrupt download: nothing left in the cache" "$(ls -A "$XDG_CACHE_HOME/pods" | wc -l)" 0
shot 22-prefs-install-failed
rm "$REL/tamper"

A11Y click button "Try Again"
sleep 2
expect "downloading: progress in the banner" "$(A11Y label "Downloading" | sed 's/[0-9]*%/N%/')" \
    '"Downloading the AirPods engine… N%"'
shot 23-prefs-installing
until_true 20 owned io.github.d101c.Pods
expect "installed: the engine runs" "$(owned io.github.d101c.Pods)" true
expect "installed: podsd in ~/.local/lib/pods, executable" \
    "$(stat -c '%a' "$LIB/podsd") $(cmp -s "$LIB/podsd" "$REL/$NAME/podsd" && echo same)" '755 same'
expect "installed: the engine's own unit" "$(cmp -s "$UNIT" "$EXT/../engine/data/pods-engine.service" && echo same)" same
expect "installed: D-Bus activation runs podsd" "$(grep -E '^(Exec|SystemdService)=' "$BUSFILE" | tr '\n' ' ')" \
    "Exec=$LIB/podsd SystemdService=pods-engine.service "
expect "installed: systemd reloaded first, then started the unit" \
    "$(grep -E '^SYSTEMD (Reload|StartUnit|RestartUnit|StopUnit)( |$)' "$OUT/systemd.log" | cut -d' ' -f2- | tr '\n' ',')" \
    'Reload,RestartUnit pods-engine.service replace,'
expect "installed: the bus can start it from now on" "$(activatable)" 1
expect "installed: download cleaned up" "$(ls -A "$XDG_CACHE_HOME/pods" | wc -l)" 0
until_true 10 A11Y has button "Uninstall Engine…"
expect "installed: banner gone, the engine in the settings" \
    "$(A11Y has button "Install Engine") $(A11Y label "Version")" 'false "Version 0.0.0-fake · Running · ~/.local/lib/pods"'
expect "installed: no AirPods yet" "$(A11Y label "No AirPods")" '"No AirPods Connected"'
expect "installed: still no tile, no notification" \
    "$(field i02 "[d['visible'], [n for n in d['notifications'] if n['source'] == 'AirPods']]")" '[false, []]'
shot 24-prefs-installed

mkdir -p "$XDG_CONFIG_HOME/pods"; echo "low_battery_alerts = true" > "$XDG_CONFIG_HOME/pods/config.toml"
A11Y click button "Uninstall Engine…"
until_true 10 A11Y has button "Uninstall"
shot 25-prefs-uninstall
A11Y click button "Uninstall"
until_true 15 A11Y has button "Install Engine"
expect "uninstalled: engine stopped through systemd" \
    "$(owned io.github.d101c.Pods) $(grep -c '^SYSTEMD StopUnit pods-engine.service' "$OUT/systemd.log")" 'false 1'
expect "uninstalled: its files gone, settings kept" \
    "$(ls -A "$LIB" "$UNIT" "$BUSFILE" 2>/dev/null | wc -l) $(ls "$XDG_CONFIG_HOME/pods")" '0 config.toml'
expect "uninstalled: the bus cannot start it any more" "$(activatable)" 0
expect "uninstalled: Install Engine offered again, no new notification" \
    "$(A11Y label "The AirPods engine") $(field i03 "[n for n in d['notifications'] if n['source'] == 'AirPods']")" \
    '"The AirPods engine is not installed" []'
probe closewindows; sleep 1
kill "$HTTP" "$SYSTEMD" 2>/dev/null; wait "$HTTP" "$SYSTEMD" 2>/dev/null

echo "# Engine without AirPods"
start_engine idle
expect "engine, no AirPods: no tile" "$(field s02 "d['visible']")" 'false'

echo "# AirPods Pro (1st generation) connected"
engine pro1
probe open; sleep 1
expect "tile shown" "$(field s03 "[d['visible'], d['mapped']]")" '[true, true]'
expect "title, subtitle, blue" "$(field s03 "[d['title'], d['subtitle'], d['checked']]")" \
    '["AirPods Pro", "Noise Cancelling", true]'
expect "subtitle: not cut at the real size" "$(field s03 "d['subtitleCut']")" 'false'
expect "tile icon" "$(field s03 "d['icon']")" '"pods-earbuds-symbolic.svg"'
expect "still nothing in the top bar" "$(field s03 "d['panelIcons']")" "$ICONS"
expect "D-Bus: the two device signals it uses, nothing more" \
    "$(rules "sorted(re.search(r\"member='(\\w+)'\", x).group(1) for x in r if 'Pods1.Device' in x and 'PropertiesChanged' not in x)")" \
    '["LowBattery", "NoiseModeChanged"]'
expect "D-Bus: the two manager signals of the case card" \
    "$(rules "sorted(re.search(r\"member='(\w+)'\", x).group(1) for x in r if 'Pods1.Manager' in x and 'PropertiesChanged' not in x)")" \
    '["CaseClosed", "CaseOpened"]'
expect "D-Bus: no subscription to a whole interface's signals" \
    "$(rules "[x for x in r if \"interface='io.github.d101c.Pods1.\" in x and 'member=' not in x]")" '[]'
shot 01-tile-anc qs

echo "# Click on the tile"
probe click; sleep 0.8
expect "click: CycleNoiseMode called once" "$(called CycleNoiseMode)" 1
expect "click: Transparency, still blue" "$(field s04 "[d['subtitle'], d['checked']]")" '["Transparency", true]'
expect "click: on-screen display" "$(field s04 "[d['osd']['visible'], d['osd']['label'], d['osd']['icon']]")" \
    '[true, "Transparency", "pods-noise-transparency-symbolic.svg"]'
shot 02-osd-transparency

echo "# Press and hold on the stem"
probe hideosd
engine stem
expect "stem: Noise Cancelling" "$(field s05 "d['subtitle']")" '"Noise Cancelling"'
expect "stem: on-screen display" "$(field s05 "[d['osd']['visible'], d['osd']['label'], d['osd']['icon']]")" \
    '[true, "Noise Cancellation", "pods-noise-anc-symbolic.svg"]'
shot 03-osd-anc

echo "# Off, changed elsewhere"
probe hideosd
engine mode off
expect "Off: grey tile" "$(field s06 "[d['subtitle'], d['checked']]")" '["Off", false]'
expect "change from elsewhere: no on-screen display" "$(field s06 "d['osd']['visible']")" 'false'
shot 04-tile-off qs
engine mode anc
probe hideosd

echo "# The menu"
probe menu; sleep 1
expect "menu open" "$(field s07 "d['menuOpen']")" 'true'
expect "header" "$(field s07 "d['header']")" \
    '{"title": "AirPods Pro de Camille", "subtitle": "Connected · In both ears", "iconActive": true}'
expect "rings: levels" "$(field s07 "[r['text'] for r in d['rings']]")" '["82%", "79%", "64%"]'
expect "rings: names" "$(field s07 "[r['label'] for r in d['rings']]")" '["Left", "Right", "Case"]'
expect "rings: case charging" "$(field s07 "[r['charging'] for r in d['rings']]")" '[false, false, true]'
expect "rings: 52 px" "$(field s07 "sorted({(r['width'], r['height']) for r in d['rings']})")" "[[$((52 * SCALE)), $((52 * SCALE))]]"
expect "noise control: Pro 1 modes" "$(field s07 "[b['label'] for b in d['noise']]")" \
    '["Off", "Transparency", "Noise Cancellation"]'
expect "noise control: current mode" "$(field s07 "[b['checked'] for b in d['noise']]")" '[false, false, true]'
expect "noise control: equal widths" "$(field s07 "max(b['width'] for b in d['noise']) - min(b['width'] for b in d['noise']) <= 1")" true
expect "settings item" "$(field s07 "d['menuItems'][-1]")" '"AirPods Settings"'
expect "Pro 1: no Conversation Awareness" "$(field s07 "d['conversation']['visible']")" 'false'
expect "Pro 1 spatial audio: Off and Fixed (no head tracking), Off chosen" \
    "$(field s07 "[[b['mode'], b['label'], b['checked']] for b in d['spatial']['buttons']]")" \
    '[["off", "Off", true], ["fixed", "Fixed", false]]'
expect "spatial audio Off: no line of help, no Recenter" "$(field s07 "[d['spatial']['hint'], d['spatial']['recenter']]")" '[null, null]'
shot 05-menu-pro1 qs

echo "# Noise control in the menu"
probe segment off; sleep 0.8
expect "segment: SetNoiseMode off" "$(grep -c "^CALL SetNoiseMode 'off'" "$OUT/engine.log")" 1
expect "segment: Off chosen, tile grey" "$(field s08 "[[b['checked'] for b in d['noise']], d['checked'], d['header']['iconActive']]")" \
    '[[true, false, false], false, false]'
expect "segment: on-screen display" "$(field s08 "[d['osd']['visible'], d['osd']['label']]")" '[true, "Noise Control Off"]'
shot 06-menu-off qs
probe segment transparency; sleep 0.8
probe hideosd

echo "# AirPods Settings"
probe open; probe menu; sleep 0.8
probe settings
for _ in $(seq 60); do [ "$(field s20 "len(d['windows'])")" = 0 ] || break; sleep 0.25; done
sleep 2.5
expect "settings: Quick Settings closed, a window opened" "$(field s21 "[d['qsOpen'], len(d['windows']) > 0]")" '[false, true]'
shot 12-preferences
probe closewindows; sleep 1

echo "# AirPods Pro (2nd generation)"
engine pro2
expect "other AirPods: tile follows the new device" "$(field s09 "[d['visible'], d['title'], d['subtitle']]")" \
    '[true, "AirPods Pro", "Adaptive"]'
probe open; probe menu; sleep 1
expect "Pro 2: four modes" "$(field s10 "[b['mode'] for b in d['noise']]")" '["off", "transparency", "adaptive", "anc"]'
expect "Pro 2: Adaptive chosen" "$(field s10 "[b['checked'] for b in d['noise']]")" '[false, false, true, false]'
expect "Pro 2: header and rings" "$(field s10 "[d['header']['title'], [r['text'] for r in d['rings']]]")" \
    '["AirPods Pro de Léo", ["100%", "96%", "71%"]]'
expect "Pro 2: Conversation Awareness, off" "$(field s10 "d['conversation']")" \
    '{"visible": true, "state": false, "title": "Conversation Awareness", "subtitle": "Lowers media when you speak"}'
probe conversation; sleep 0.5
expect "Conversation Awareness: SetConversationAwareness true" "$(grep -c "^CALL SetConversationAwareness True" "$OUT/engine.log")" 1
expect "Conversation Awareness: on, menu still open" "$(field s10b "[d['conversation']['state'], d['menuOpen']]")" '[true, true]'
shot 07-menu-pro2 qs

echo "# Spatial Audio in the menu"
spatial() { field "$1" "(lambda s: $2)(d['spatial'])"; }   # spatial <state> <expression on s>
expect "Pro 2: Off, Fixed, Head Tracked, each with its icon" \
    "$(spatial m01 "[[b['label'], b['icon'], b['checked']] for b in s['buttons']]")" \
    '[["Off", "pods-spatial-off-symbolic.svg", true], ["Fixed", "pods-spatial-fixed-symbolic.svg", false], ["Head Tracked", "pods-spatial-head-symbolic.svg", false]]'
expect "spatial audio: equal widths" "$(spatial m01 "max(b['width'] for b in s['buttons']) - min(b['width'] for b in s['buttons']) <= 1")" true
probe spatial fixed; sleep 0.5
expect "Fixed: SetSpatialMode fixed" "$(called "SetSpatialMode 'fixed'")" 1
expect "Fixed: chosen, its line, no Recenter" "$(spatial m02 "[[b['checked'] for b in s['buttons']], s['hint'], s['recenter']]")" \
    '[[false, true, false], "Music sits in front of you", null]'
probe spatial head; sleep 0.5
expect "Head Tracked: its line, Recenter" "$(spatial m03 "[[b['checked'] for b in s['buttons']], s['hint'], s['recenter']]")" \
    '[[false, false, true], "Sound stays with your screen", "Recenter"]'
expect "spatial audio: the tile stays on the noise mode" "$(field m03 "[d['subtitle'], d['osd']['visible']]")" '["Adaptive", false]'
shot 26-menu-spatial-head qs
probe recenter; sleep 0.3
expect "Recenter: called once" "$(called Recenter)" 1
expect "Recenter: Done, centered, on-screen display" \
    "$(field m04 "[d['spatial']['hint'], d['spatial']['recenter'], d['osd']['visible'], d['osd']['label'], d['osd']['icon']]")" \
    '["Centered on your screen", "Done", true, "Recentered", "pods-spatial-head-symbolic.svg"]'
shot 27-menu-recentered qs
sleep 1.6
expect "1.6 s later: Recenter again" "$(spatial m05 "[s['hint'], s['recenter']]")" '["Sound stays with your screen", "Recenter"]'
engine spatial-status paused
expect "shared audio: tile and line say spatial is paused" \
    "$(field m05b "[d['subtitle'], d['spatial']['hint']]")" '["Spatial Off · Sharing", "Paused while the audio is shared"]'
engine spatial-status active
expect "sharing over: back to the noise mode" "$(field m05c "d['subtitle']")" '"Adaptive"'
probe hideosd
engine spatial off
expect "Off set elsewhere: the menu follows" "$(spatial m06 "[[b['checked'] for b in s['buttons']], s['hint']]")" '[[true, false, false], null]'
engine spatial-support off   # an engine before M3
probe spatial fixed; sleep 0.6
expect "engine without spatial audio: back to Off" "$(spatial m07 "[b['checked'] for b in s['buttons']]")" '[true, false, false]'
engine spatial-support on
probe close

echo "# Spatial Audio settings"
engine spatial head
engine pose 24
gnome-extensions prefs "$UUID"
until_true 15 A11Y has "page tab" "Spatial Audio"
sleep 1
expect "General page first: the head is not watched" "$(called WatchHeadPose)" 0
A11Y click "page tab" "Spatial Audio"; sleep 1.2
expect "Spatial Audio page shown: WatchHeadPose(true), poses flow" \
    "$(called "WatchHeadPose True") $(grep '^POSES' "$OUT/engine.log" | tail -1)" '1 POSES ON'
expect "live: the head turned 24° to the left" "$(A11Y label "Live")" '"Live · −24°"'
expect "mode: three, Head Tracked chosen" \
    "$(A11Y names "radio button") $(A11Y state "radio button" "Head Tracked")" '["Off", "Fixed", "Head Tracked"] [true, true]'
expect "mode: its line" "$(A11Y label "Music stays")" '"Music stays with your screen as you turn"'
expect "Follow Screen off, Recenter, both available" \
    "$(A11Y state switch "Follow Screen") $(A11Y state button "Recenter")" '[false, true] [false, true]'
# A row with a button speaks for the button: "Test Sound" is the Play button.
expect "head profile and test sound" "$(A11Y label "Generic") $(A11Y has button "Test Sound")" '"Generic (KEMAR)" true'
expect "sensor data stays here: said so" "$(A11Y label "Head tracking uses")" \
    '"Head tracking uses the motion sensors in your AirPods. Their data stays on this computer."'
shot 30-spatial-head
A11Y click switch "Follow Screen"; sleep 0.5
expect "Follow Screen: SetFollowScreen true" "$(called "SetFollowScreen True")" 1
A11Y click button "Recenter"; sleep 0.3
expect "Recenter: called, Recentered" "$(called Recenter) $(A11Y label "Recentered")" '2 "Recentered"'
sleep 1.6
expect "recentered: the head faces the screen" "$(A11Y label "Live")" '"Live · 0°"'
engine pose-sim on; sleep 1
shot 31-spatial-head-turning
A11Y click "radio button" "Fixed"; sleep 1
expect "Fixed: SetSpatialMode fixed, sensors idle, its line" \
    "$(called "SetSpatialMode 'fixed'") $(A11Y label "Not tracking") $(A11Y label "Music in")" \
    '3 "Not tracking" "Music in front of you, it turns with your head"'
expect "Fixed: Follow Screen and Recenter dimmed" \
    "$(A11Y state switch "Follow Screen") $(A11Y state button "Recenter")" '[true, false] [false, false]'
shot 32-spatial-fixed
A11Y click "radio button" "Off"; sleep 0.8
expect "Off: SetSpatialMode off, its line" "$(called "SetSpatialMode 'off'") $(A11Y label "Regular")" '1 "Regular stereo"'
shot 33-spatial-off
A11Y click "radio button" "Head Tracked"; sleep 1
A11Y click "page tab" "General"; sleep 0.8
expect "General page: WatchHeadPose(false), poses stop" \
    "$(called "WatchHeadPose False") $(grep '^POSES' "$OUT/engine.log" | tail -1 | cut -d' ' -f1-2)" '1 POSES OFF'
A11Y click "page tab" "Spatial Audio"; sleep 0.8
expect "Spatial Audio page again: watched again" "$(called "WatchHeadPose True")" 2
probe closewindows; sleep 1.5
expect "window closed: WatchHeadPose(false), nobody watches" \
    "$(called "WatchHeadPose False") $(grep '^WATCHERS' "$OUT/engine.log" | tail -1)" '2 WATCHERS 0'
expect "window closed: no HeadPose subscription left on the bus" \
    "$(gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus \
        -m org.freedesktop.DBus.Debug.Stats.GetAllMatchRules | grep -c "member='HeadPose'")" 0
engine pose 24

echo "# Spatial Audio settings, light style, AirPods without head tracking"
gsettings set org.gnome.desktop.interface color-scheme prefer-light; sleep 1
gnome-extensions prefs "$UUID"
until_true 15 A11Y has "page tab" "Spatial Audio"
A11Y click "page tab" "Spatial Audio"; sleep 1.5
shot 34-spatial-light
probe closewindows; sleep 1
gsettings set org.gnome.desktop.interface color-scheme "$SCHEME"
engine pro1
WATCHES=$(called WatchHeadPose)
gnome-extensions prefs "$UUID"
until_true 15 A11Y has "page tab" "Spatial Audio"
A11Y click "page tab" "Spatial Audio"; sleep 1.2
expect "Pro 1: Off and Fixed, no Follow Screen, no Recenter" \
    "$(A11Y names "radio button") $(A11Y has switch "Follow Screen") $(A11Y has button "Recenter")" '["Off", "Fixed"] false false'
expect "Pro 1: no sensors, the head is not watched" "$(called WatchHeadPose)" "$WATCHES"
expect "Pro 1: not tracking" "$(A11Y label "Not tracking")" '"Not tracking"'
shot 35-spatial-pro1
probe closewindows; sleep 1
engine pro2
probe open; probe menu; sleep 1

echo "# Low battery"
engine low left 9
expect "low: subtitle" "$(field s11 "[d['subtitle'], d['lowBatteryStyle']]")" '["Left 9% · Low Battery", true]'
expect "low: coral ring" "$(field s11 "[[r['low'] for r in d['rings']], d['rings'][0]['text']]")" '[[true, false, false], "9%"]'
expect "low: notification" "$(field s11 "[[n['source'], n['title'], n['body']] for n in d['notifications'] if n['source'] == 'AirPods']")" \
    '[["AirPods", "Left AirPod at 9%", "Put it back in the case to charge it."]]'
shot 08-menu-low qs
probe close; sleep 0.3
engine low left 4
shot 09-notification
expect "lower again: replaces the first notification" "$(field s12 "[[n['title'], n['urgency']] for n in d['notifications'] if n['source'] == 'AirPods']")" \
    '[["Left AirPod at 4%", 2]]'
engine low case 10
expect "case: its own notification" "$(field s13 "sorted(n['title'] for n in d['notifications'] if n['source'] == 'AirPods')")" \
    '["AirPods case at 10%", "Left AirPod at 4%"]'

echo "# One earbud, then both in the case"
engine pro1
engine one-bud
probe open; probe menu; sleep 1
expect "one earbud: subtitle and status" "$(field s14 "[d['subtitle'], d['header']['subtitle']]")" \
    '["Right Only · Transparency", "Connected · Right in ear"]'
expect "one earbud: unknown levels" "$(field s14 "[[r['known'] for r in d['rings']], [r['text'] for r in d['rings']]]")" \
    '[[false, true, false], ["—", "71%", "—"]]'
shot 10-menu-one-bud qs
engine charging
expect "in the case: status" "$(field s15 "d['header']['subtitle']")" '"In case · Charging"'
expect "in the case: green rings" "$(field s15 "[[r['charging'] for r in d['rings']], [r['text'] for r in d['rings']]]")" \
    '[[true, true, true], ["46%", "51%", "88%"]]'
shot 11-menu-charging qs
engine wear ear ear
engine battery left 82
engine battery right 79
engine battery case 64 charging
engine pause
expect "media paused: status" "$(field s15b "d['header']['subtitle']")" '"Not in ears · Paused"'
shot 11b-menu-paused qs
engine resume
expect "media playing again: status" "$(field s15c "d['header']['subtitle']")" '"Connected · In both ears"'

echo "# Disconnection and engine restarts"
engine disconnect
expect "disconnected: no tile, menu closed" "$(field s16 "[d['visible'], d['menuOpen']]")" '[false, false]'
engine reconnect
expect "reconnected: tile back" "$(field s17 "d['visible']")" 'true'
stop_engine
expect "engine stopped: no tile" "$(field s18 "d['visible']")" 'false'
start_engine pro1
expect "engine back: tile back" "$(field s19 "[d['visible'], d['subtitle']]")" '[true, "Noise Cancelling"]'
probe close; sleep 0.5

# ---------------------------------------------------------------------------
GS_EXT="gsettings --schemadir $XDG_DATA_HOME/gnome-shell/extensions/$UUID/schemas"
card() { field "$1" "(lambda c: $2)(d['card'])"; }   # card <state> <expression on c, the card>

echo "# Case card: paired AirPods, not connected"
# Out of the way of the card, which stays while the pointer rests on it.
probe pointer $((200 * SCALE)) $((300 * SCALE))
engine paired
expect "paired, not connected: no tile, no card" "$(field c01 "[d['visible'], d['card']['shown']]")" '[false, false]'
engine lid-open; sleep 0.6
expect "lid open: card shown" "$(card c02 "c['shown']")" 'true'
expect "card: name and status" "$(card c02 "[c['title'], c['status'], c['check']]")" \
    '["AirPods Pro de Camille", "Not connected to this computer", false]'
expect "card: Connect offered" "$(card c02 "[c['connect'], c['connecting']]")" '[true, false]'
expect "card: earbuds within 10 points as one, then the case charging" "$(card c02 "c['batteries']")" \
    '[{"kind": "buds", "text": "79%", "label": "AirPods", "charging": false, "low": false}, {"kind": "case", "text": "64%", "label": "Case", "charging": true, "low": false}]'
expect "card: open case drawing, floating" "$(card c02 "[c['art'], c['floating']]")" '["pods-case-open.svg", true]'
expect "card: risen, opaque" "$(card c02 "[c['translationY'], c['opacity']]")" '[0, 255]'
expect "card: 400 px wide, in the middle" "$(card c02 "[c['width'], abs(c['centreOffset']) <= 1]")" "[$((400 * SCALE)), true]"
expect "card: where the on-screen display sits" "$(card c02 "c['bottomGap'] in range($((40 * SCALE)), $((80 * SCALE)))")" 'true'
shot 14-card-not-connected

echo "# Case card: Connect"
engine connect-delay 5   # the screenshot of "Connecting…" takes a few seconds
probe cardconnect; sleep 0.3
expect "Connect: called on the device" "$(called Connect)" 1
expect "Connect: Connecting…" "$(card c03 "[c['status'], c['connect'], c['connecting']]")" \
    '["Connecting to this computer", false, true]'
shot 15-card-connecting
for _ in $(seq 50); do grep -q "^CONNECTED" "$OUT/engine.log" && break; sleep 0.1; done
T0=$(sed -n "s/^CONNECTED .* \([0-9.]*\)$/\1/p" "$OUT/engine.log" | head -1)
since() { sleep "$(python3 -c "import sys, time; print(max(0, float(sys.argv[1]) + float(sys.argv[2]) - time.time()))" "$T0" "$1")"; }
since 0.5
expect "connected: status with its tick" "$(card c04 "[c['status'], c['check'], c['connectedStyle']]")" \
    '["Connected", true, true]'
expect "connected: no button left, tile shown" "$(field c04 "[d['card']['connect'], d['card']['connecting'], d['visible']]")" \
    '[false, false, true]'
shot 16-card-connected
since 5
expect "connected: still there after 5 s" "$(card c05 "c['shown']")" 'true'
since 8
expect "connected: gone 6 s after Connected" "$(card c06 "c['shown']")" 'false'
engine connect-delay 1
engine lid-open
expect "same opening: a new snapshot does not bring it back" "$(card c07 "c['shown']")" 'false'

echo "# Case card: lid closed, opened again"
engine lid-close
engine lid-open; sleep 0.6
expect "next opening, already connected: card with Connected" "$(card c08 "[c['shown'], c['status'], c['connect']]")" \
    '[true, "Connected", false]'
probe cardhover on; sleep 7
expect "pointer on the card: it stays" "$(card c09 "c['shown']")" 'true'
probe cardhover off
probe cardclose; sleep 0.6
expect "close button: card gone" "$(card c10 "c['shown']")" 'false'
engine lid-close
engine paired
engine lid-open 90 70 50; sleep 0.6
expect "earbuds 10 points apart or more: each one" "$(card c11 "[[b['kind'], b['text']] for b in c['batteries']]")" \
    '[["left", "90%"], ["right", "70%"], ["case", "50%"]]'
shot 17-card-each-earbud
engine lid-close; sleep 0.6
expect "lid closed: card gone" "$(card c12 "c['shown']")" 'false'

echo "# Case card: Connect fails"
engine paired
engine connect-fail on
engine lid-open; sleep 0.6
probe cardconnect; sleep 0.6
expect "Connect failed: said so, Connect again" "$(card c13 "[c['status'], c['connect'], c['connecting']]")" \
    '["Could not connect", true, false]'
engine connect-fail off
engine lid-close

echo "# Case card: when it does not show"
$GS_EXT set org.gnome.shell.extensions.pods show-case-card false
engine lid-open
expect "setting off: no card" "$(card c14 "c['shown']")" 'false'
engine lid-close
$GS_EXT set org.gnome.shell.extensions.pods show-case-card true
WAYLAND_DISPLAY=wayland-pods gjs -m "$S/fullscreen.js" >> "$OUT/session.log" 2>&1 &
FULLSCREEN=$!
for _ in $(seq 40); do [ "$(field c15 "d['fullscreen']")" = true ] && break; sleep 0.25; done
expect "a fullscreen window" "$(field c15 "d['fullscreen']")" 'true'
engine lid-open
expect "fullscreen: no card" "$(card c16 "c['shown']")" 'false'
engine lid-close
probe closewindows; wait "$FULLSCREEN" 2>/dev/null; sleep 1
probe sharing on
engine lid-open
expect "screen shared: no card" "$(card c17 "c['shown']")" 'false'
engine lid-close
probe sharing off
engine lid-open; sleep 0.6
expect "not shared any more: card" "$(card c18 "c['shown']")" 'true'
probe sharing on; sleep 0.6
expect "sharing starts: card gone" "$(card c19 "c['shown']")" 'false'
probe sharing off
engine lid-close

echo "# Case card: reduced motion"
gsettings set org.gnome.desktop.interface enable-animations false; sleep 0.5
ENGINE_WAIT=0.1 engine lid-open 15 18 12
expect "no animation: there at once, case still" "$(card c20 "[c['shown'], c['opacity'], c['translationY'], c['floating']]")" \
    '[true, 255, 0, false]'
expect "low case: amber light, coral levels" "$(card c20 "[c['art'], [b['low'] for b in c['batteries']]]")" \
    '["pods-case-open-amber.svg", [true, true]]'
shot 18-card-low
ENGINE_WAIT=0.1 engine lid-close
expect "no animation: gone at once" "$(card c21 "c['shown']")" 'false'
gsettings set org.gnome.desktop.interface enable-animations true
engine pro1

echo "# Light style"
gsettings set org.gnome.desktop.interface color-scheme prefer-light; sleep 2
probe open; probe menu; sleep 1
expect "light style: tile still there" "$(field s22 "d['visible']")" 'true'
shot 13-menu-light qs
probe close
engine lid-open; sleep 0.8
shot 19-card-light
engine lid-close
gsettings set org.gnome.desktop.interface color-scheme "$SCHEME"; sleep 1

echo "# disable()"
probe open; probe menu; engine low right 9; engine lid-open; sleep 0.6
expect "before disable(): a card up" "$(card s22b "c['shown']")" 'true'
gnome-extensions disable "$UUID"; sleep 1.5
expect "disabled: tile gone" "$(field s23 "d['found']")" 'false'
expect "disabled: its notifications gone" "$(field s23 "[n for n in d['notifications'] if n['source'] == 'AirPods']")" '[]'
expect "disabled: top bar as before" "$(field s23 "d['panelIcons']")" "$ICONS"
expect "disabled: its menu removed from Quick Settings" "$(field s23 "d['overlayChildren']")" "$OVERLAY"
expect "disabled: no case card" "$(field s23 "d['card']['found']")" 'false'
# A GDBusProxy drops its match rules when it is finalized: the extension keeps
# no reference, so they go with the next garbage collection.
probe gc; sleep 0.5
expect "disabled: no D-Bus subscription left about the engine" "$(rules "r")" '[]'
probe close
gnome-extensions enable "$UUID"; sleep 2
expect "enabled again: tile back with the engine's state" "$(field s24 "[d['visible'], d['subtitle']]")" '[true, "Right 9% · Low Battery"]'
gnome-extensions disable "$UUID"; sleep 1
expect "disabled again: tile gone" "$(field s25 "d['found']")" 'false'
engine stem
expect "disabled: a signal of the engine changes nothing" "$(field s26 "[d['found'], d['osd']['visible']]")" '[false, false]'

# ---------------------------------------------------------------------------
[ -e "$OUT/error" ] && { echo "FAIL probe errors:"; cat "$OUT/error"; failed=1; }
stop_engine
kill "$SH"; wait "$SH" 2>/dev/null
# The warnings the run provokes on purpose: the failed Connect(), spatial
# audio on an engine before M3 (Shell), the corrupt download (settings).
grep -n -E "pods@d101c|pods:|JS ERROR|JS WARNING|Gjs-CRITICAL|Gjs-WARNING" "$OUT/shell.log" \
    | grep -v -e "pods: Connect failed: Page timeout$" \
        -e "pods: SetSpatialMode failed: SetSpatialMode comes after M2$" > "$OUT/extension-log.txt"
grep -n -E "JS ERROR|JS WARNING|Gjs-CRITICAL|Gjs-WARNING|pods@d101c|pods:" "$OUT/session.log" \
    | grep -v -e "Activating service" -e "Falling back to slow path" \
        -e "pods: engine install failed: The download does not match: SHA-256 [0-9a-f]*$" >> "$OUT/extension-log.txt"
if [ -s "$OUT/extension-log.txt" ]; then
    echo "FAIL the logs mention the extension or a JS error:"; cat "$OUT/extension-log.txt"; failed=1
else
    echo "ok   no error or warning from the extension in the logs"
fi
grep -c "^CALL" "$OUT/engine.log" | sed 's/^/# calls received by the fake engine: /'
exit $failed
