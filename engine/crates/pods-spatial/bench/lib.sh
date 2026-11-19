# Private PipeWire for the pods-spatial bench. Source this file, never run it.
# Adapted from airpods-study/spatial-bench/lib.sh.
#
# Everything lives in $RUN (a short private directory: unix socket paths are
# limited to 108 bytes): runtime dir, HOME, configs, logs. The daemon loads NO
# alsa / bluez / pulse / jack module and the private WirePlumber has every
# hardware monitor disabled, so nothing here can see or touch a sound card or
# a headset. Every pw-* call goes through `pwp`, which refuses to run unless
# the daemon it reaches is ours and shows no hardware node.

B="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
: "${RUN:?RUN must be set by bench.sh}"
CORE_NAME="pods-spatial-bench"
PHOME="$RUN/home"
mkdir -p "$PHOME/.config/wireplumber/wireplumber.conf.d" "$PHOME/.local/state" "$PHOME/.local/share"
cp "$B/conf/wireplumber-no-hardware.conf" "$PHOME/.config/wireplumber/wireplumber.conf.d/no-hardware.conf"

# The environment of every private process: nothing inherited that could
# point at the session (PIPEWIRE_REMOTE, D-Bus, Pulse, Wayland...).
PRIV_ENV=(-u PIPEWIRE_REMOTE -u PIPEWIRE_CONFIG_DIR -u PULSE_SERVER -u DBUS_SESSION_BUS_ADDRESS
    -u WAYLAND_DISPLAY -u DISPLAY -u JACK_DEFAULT_SERVER -u PIPEWIRE_DEBUG
    HOME="$PHOME" XDG_CONFIG_HOME="$PHOME/.config" XDG_STATE_HOME="$PHOME/.local/state"
    XDG_DATA_HOME="$PHOME/.local/share"
    XDG_RUNTIME_DIR="$RUN" PIPEWIRE_RUNTIME_DIR="$RUN" PIPEWIRE_REMOTE="$CORE_NAME"
    PODS_SPATIAL_BENCH_CORE="$CORE_NAME"
    GIO_USE_VFS=local GSETTINGS_BACKEND=memory)
private_env() { env "${PRIV_ENV[@]}" "$@"; }

# make_conf <out> <quantum>
make_conf() {
    sed -e "s|@QUANTUM@|$2|g" "$B/conf/bench-daemon.conf.in" > "$1"
}

PW_PID=""
start_pw() {   # start_pw <conf> <log>
    rm -f "$RUN/$CORE_NAME" "$RUN/$CORE_NAME.lock"
    env "${PRIV_ENV[@]}" pipewire -c "$1" > "$2" 2>&1 &
    PW_PID=$!
    for _ in $(seq 50); do [ -S "$RUN/$CORE_NAME" ] && break; sleep 0.1; done
    sleep 0.5
    check_private || { stop_pw; return 1; }
}

WP_PID=""
start_wp() {   # start_wp <log>   (needs the private bus set up by bench.sh)
    check_private || return 1
    if [ -z "${BENCH_PRIVATE_DBUS:-}" ] || [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] || \
       [[ "$DBUS_SESSION_BUS_ADDRESS" == *"/run/user/"* ]]; then
        echo "SAFETY: start_wp needs the private bus of bench.sh" >&2
        return 1
    fi
    env "${PRIV_ENV[@]}" DBUS_SESSION_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS" wireplumber > "$1" 2>&1 &
    WP_PID=$!
    sleep 1.5
    check_private || { stop_all; return 1; }
}

stop_all() {
    [ -n "$WP_PID" ] && kill "$WP_PID" 2>/dev/null; WP_PID=""
    stop_pw
}

# Refuse to go on unless we talk to OUR daemon and it has no hardware.
check_private() {
    [ -S "$RUN/$CORE_NAME" ] || { echo "SAFETY: no private socket" >&2; return 1; }
    private_env timeout 10 pw-dump -r "$CORE_NAME" 2>/dev/null | python3 -I -c '
import json, re, sys
want_name, want_pid = sys.argv[1], sys.argv[2]
objs = json.load(sys.stdin)
core = [o for o in objs if o.get("type") == "PipeWire:Interface:Core"]
if not core or core[0]["info"]["name"] != want_name:
    sys.exit("SAFETY: core is not " + want_name)
if str(core[0]["info"]["props"].get("application.process.id")) != want_pid:
    sys.exit("SAFETY: core pid is not our pipewire " + want_pid)
bad = re.compile(r"alsa|bluez|v4l2|libcamera|jack", re.I)
for o in objs:
    props = (o.get("info") or {}).get("props") or {}
    for k in ("node.name", "device.api", "factory.name", "device.name", "api.alsa.path", "module.name", "media.class"):
        v = str(props.get(k, ""))
        if bad.search(v):
            sys.exit("SAFETY: hardware object in private daemon: %s=%s" % (k, v))
' "$CORE_NAME" "${PW_PID:-x}" || return 1
    return 0
}

pwp() {        # pwp <pw-tool> args... : a pw tool against the private daemon only
    check_private || { echo "SAFETY: refusing '$*'" >&2; return 1; }
    local tool="$1" ropt=-r; shift
    case "$tool" in pw-play|pw-record|pw-cat) ropt=-R ;; esac   # pw-cat: -r means --record
    private_env "$tool" "$ropt" "$CORE_NAME" "$@"
}

# `pwp_bg tool ... &`: execs, so $! is the tool itself. Never in the foreground.
pwp_bg() {
    check_private || { echo "SAFETY: refusing '$*'" >&2; exit 1; }
    local tool="$1" ropt=-r; shift
    case "$tool" in pw-play|pw-record|pw-cat) ropt=-R ;; esac
    exec env "${PRIV_ENV[@]}" "$tool" "$ropt" "$CORE_NAME" "$@"
}

# `bench_bg args... &`: the pods-spatial bench driver, private env only.
bench_bg() {
    check_private || { echo "SAFETY: refusing spatial_bench $*" >&2; exit 1; }
    exec env "${PRIV_ENV[@]}" "$BENCH_BIN" "$@"
}

stop_pw() {
    [ -n "$PW_PID" ] && kill "$PW_PID" 2>/dev/null
    # Anything started from this bench (its runtime dir or core name is in the command line).
    pkill -f -- "^$BENCH_BIN " 2>/dev/null
    pkill -f -- "^pipewire -c $RUN/" 2>/dev/null
    pkill -f -- "^pw-(play|record|cli|metadata) -[rR] $CORE_NAME" 2>/dev/null
    for _ in $(seq 30); do
        pgrep -f -- "^($BENCH_BIN |pipewire -c $RUN/|pw-(play|record|cli|metadata) -[rR] $CORE_NAME)" >/dev/null || break
        sleep 0.1
    done
    pkill -9 -f -- "^($BENCH_BIN |pipewire -c $RUN/|pw-(play|record|cli|metadata) -[rR] $CORE_NAME)" 2>/dev/null
    PW_PID=""
    rm -f "$RUN/$CORE_NAME" "$RUN/$CORE_NAME.lock" "$RUN/$CORE_NAME-manager" "$RUN/$CORE_NAME-manager.lock"
}

graph() {      # graph <query> [arg]   (see graph.py)
    pwp pw-dump | python3 -I "$B/graph.py" "$@"
}

set_default() {   # set_default <node.name>
    pwp pw-metadata -n default 0 default.configured.audio.sink "{ \"name\": \"$1\" }" Spa:String:JSON >/dev/null
}

# Total on-CPU time (ns) of a pid, all threads.
sched_ns() { cat /proc/"$1"/task/*/schedstat 2>/dev/null | awk '{s+=$1} END {print s+0}'; }
