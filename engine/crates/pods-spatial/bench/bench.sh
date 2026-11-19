#!/usr/bin/env bash
# pods-spatial bench, entirely in a PRIVATE PipeWire + WirePlumber + D-Bus.
# It never talks to the session's PipeWire: see lib.sh for the guards.
#
#   cargo build --release -p pods-spatial --examples
#   crates/pods-spatial/bench/bench.sh [smoke] [rt] [idle] [clicks] [latency] [tone] [azimuths] [scenario] [sinks]   (default: all)
#   python3 -I crates/pods-spatial/bench/analyze.py crates/pods-spatial/bench/results
#
# Results in bench/results/ (or $BENCH_RESULTS): WAV recordings, logs,
# cpu.jsonl, scenario.txt.
set -u
B="$(cd "$(dirname "$0")" && pwd)"
ENGINE="$(cd "$B/../../.." && pwd)"

if [ -z "${BENCH_PRIVATE_DBUS:-}" ]; then
    # First pass: make a short private runtime dir (socket paths must stay
    # under 108 bytes) and re-run inside a private D-Bus session.
    BENCH_BIN="${BENCH_BIN:-$ENGINE/target/release/examples/spatial_bench}"
    [ -x "$BENCH_BIN" ] || { echo "build first: cargo build --release -p pods-spatial --examples" >&2; exit 1; }
    RUN="$(mktemp -d "${TMPDIR:-/tmp}/pods-sb.XXXXXX")"
    mkdir -p "$RUN/home"
    env -u DBUS_SESSION_BUS_ADDRESS -u WAYLAND_DISPLAY -u DISPLAY -u PIPEWIRE_REMOTE \
        -u PIPEWIRE_RUNTIME_DIR -u PULSE_SERVER \
        HOME="$RUN/home" XDG_RUNTIME_DIR="$RUN" XDG_CONFIG_HOME="$RUN/home/.config" \
        XDG_DATA_HOME="$RUN/home/.local/share" XDG_STATE_HOME="$RUN/home/.local/state" \
        GIO_USE_VFS=local GSETTINGS_BACKEND=memory BENCH_PRIVATE_DBUS=1 \
        RUN="$RUN" BENCH_BIN="$BENCH_BIN" dbus-run-session -- "$0" "$@"
    rc=$?
    rm -rf "$RUN"
    exit $rc
fi

cd "$B" || exit 1
source ./lib.sh
SB_PID=""
cleanup() { [ -n "$SB_PID" ] && kill "$SB_PID" 2>/dev/null; stop_all; }
trap cleanup EXIT

RES="${BENCH_RESULTS:-$B/results}"
SIG="$RES/signals"
WIN=15
mkdir -p "$RES"
[ -f "$SIG/pinkc.wav" ] || python3 -I "$B/gen_signals.py" "$SIG"

wait_log() {   # wait_log <file> <regex> <timeout s>
    for _ in $(seq $(( $3 * 10 ))); do grep -qE "$2" "$1" 2>/dev/null && return 0; sleep 0.1; done
    echo "timeout waiting for /$2/ in $1" >&2
    return 1
}

start_daemon() {   # start_daemon <quantum> <log dir>
    make_conf "$RUN/bench-q$1.conf" "$1"
    start_pw "$RUN/bench-q$1.conf" "$2/pipewire.log" || exit 1
    start_wp "$2/wireplumber.log" || exit 1
    sleep 0.5
    set_default fake_speakers
    sleep 0.3
    echo "== private pipewire pid $PW_PID (quantum $1), wireplumber pid $WP_PID; default: $(graph default)"
}

start_spatial() {  # start_spatial <fixed|tracked> <name> [driver args...]   (waits for Active)
    local mode="$1" name="$2"; shift 2
    rm -f "$RUN/stop"
    bench_bg run --mode "$mode" --target fake_airpods --seconds 900 --stop-file "$RUN/stop" \
        --stats "$OUT/$name.stats.json" "$@" > "$OUT/$name.log" 2>&1 &
    SB_PID=$!
    wait_log "$OUT/$name.log" "STATUS .* Active" 15 || { cat "$OUT/$name.log" >&2; exit 1; }
}

stop_spatial() {
    touch "$RUN/stop"
    wait "$SB_PID" 2>/dev/null
    SB_PID=""
}

# run_case <name> <fixed|tracked|none> <signal> <traj> [driver args...]
#   tracked: the yaw follows <traj> at 50 Hz through set_yaw (in-process)
run_case() {
    local name="$1" mode="$2" sig="$3" traj="$4"; shift 4
    local target=spatial_airpods
    case "$mode" in
        tracked) start_spatial tracked "$name" --traj "$traj" --rate 50 "$@" ;;
        fixed) start_spatial fixed "$name" "$@" ;;
        none) target=fake_airpods ;;
    esac
    pwp_bg pw-record --target fake_airpods -P '{ node.name=bench_rec stream.capture.sink=true }' \
        --rate 48000 --channels 2 --format f32 "$OUT/$name.wav" >/dev/null 2>&1 &
    local REC=$!
    sleep 0.3
    pwp_bg pw-play --target "$target" -P '{ node.name=bench_play }' "$SIG/$sig.wav" >/dev/null 2>&1 &
    local PLAY=$!
    sleep 1.5
    check_private || exit 1
    local links; links="$(graph links-from spatial_airpods.output)"
    local t0 p0 w0 s0 t1 p1 w1 s1
    t0=$(date +%s.%N); p0=$(sched_ns "$PW_PID"); w0=$(sched_ns "$WP_PID"); s0=$(sched_ns "${SB_PID:-0}")
    sleep "$WIN"
    t1=$(date +%s.%N); p1=$(sched_ns "$PW_PID"); w1=$(sched_ns "$WP_PID"); s1=$(sched_ns "${SB_PID:-0}")
    kill -TERM "$PLAY" "$REC" 2>/dev/null; wait "$REC" 2>/dev/null; wait "$PLAY" 2>/dev/null
    [ "$mode" != none ] && stop_spatial
    python3 -I -c '
import json, sys
a = sys.argv[1:]
name, q, mode, traj, links, extra = a[:6]
t0, t1, p0, p1, w0, w1, s0, s1 = map(float, a[6:])
dt = t1 - t0
pct = lambda x0, x1: round((x1 - x0) / 1e9 / dt * 100, 3)
print(json.dumps(dict(case=name, quantum=int(q), mode=mode, traj=traj, args=extra,
    seconds=round(dt, 2), output_linked_to=links,
    daemon_cpu_pct=pct(p0, p1), wireplumber_cpu_pct=pct(w0, w1), engine_cpu_pct=pct(s0, s1))))' \
        "$name" "$Q" "$mode" "$traj" "$links" "$*" "$t0" "$t1" "$p0" "$p1" "$w0" "$w1" "$s0" "$s1" \
        | tee -a "$OUT/cpu.jsonl"
}

# record_io <name> <fixed|tracked|none> <signal> <seconds> [driver args...]
#   One 4-channel recorder captures, in the same cycles, what goes into the
#   spatial sink (FL, FR) and what reaches fake_airpods (RL, RR): the
#   transfer matrix of the whole chain. `none`: plain stereo output, the
#   player goes straight to fake_airpods (input = output).
record_io() {
    local name="$1" mode="$2" sig="$3" secs="$4"; shift 4
    local target=spatial_airpods src=spatial_airpods
    case "$mode" in
        none) target=fake_airpods; src=fake_airpods ;;
        *) start_spatial "$mode" "$name" "$@" ;;
    esac
    pwp_bg pw-record -P '{ node.name=io_rec node.autoconnect=false }' --rate 48000 --channels 4 \
        --channel-map FL,FR,RL,RR --format f32 "$OUT/$name.wav" >/dev/null 2>&1 &
    local REC=$!
    sleep 0.5
    pwp pw-link "$src:monitor_FL" io_rec:input_FL
    pwp pw-link "$src:monitor_FR" io_rec:input_FR
    pwp pw-link fake_airpods:monitor_FL io_rec:input_RL
    pwp pw-link fake_airpods:monitor_FR io_rec:input_RR
    pwp_bg pw-play --target "$target" -P '{ node.name=bench_play }' "$SIG/$sig.wav" >/dev/null 2>&1 &
    local PLAY=$!
    sleep "$secs"
    kill -TERM "$PLAY" "$REC" 2>/dev/null; wait "$REC" 2>/dev/null; wait "$PLAY" 2>/dev/null
    [ "$mode" != none ] && stop_spatial
}

SEL="${*:-smoke rt idle clicks latency tone azimuths scenario sinks}"
sel() { [[ " $SEL " == *" $1 "* ]]; }

if sel smoke; then
    Q=1024; OUT="$RES/smoke"; rm -rf "$OUT"; mkdir -p "$OUT"
    start_daemon 1024 "$OUT"
    for mode in tracked fixed; do
        start_spatial "$mode" "smoke-$mode" --traj sine --rate 50
        echo "-- $mode: default = $(graph default); output -> $(graph links-from spatial_airpods.output)"
        stop_spatial
        echo "-- $mode stopped: default = $(graph default); $(tail -n 2 "$OUT/smoke-$mode.log" | tr '\n' ' ')"
    done
    stop_all; sleep 0.5
fi

if sel rt; then
    # Scheduling class of every thread of the engine and of the Fixed client.
    Q=1024; OUT="$RES/rt"; rm -rf "$OUT"; mkdir -p "$OUT"
    start_daemon 1024 "$OUT"
    for mode in tracked fixed; do
        start_spatial "$mode" "rt-$mode"
        pwp_bg pw-play --target spatial_airpods -P '{ node.name=bench_play }' "$SIG/pink.wav" >/dev/null 2>&1 &
        PLAY=$!
        sleep 1.5
        echo "-- $mode engine:"; ps -L -o tid,cls,rtprio,ni,comm -p "$SB_PID"
        echo "-- daemon:"; ps -L -o tid,cls,rtprio,ni,comm -p "$PW_PID"
        kill -TERM "$PLAY"; wait "$PLAY" 2>/dev/null
        stop_spatial
    done
    stop_all; sleep 0.5
fi

if sel idle; then
    # Node states and link flags while nothing plays (the AirPods must be
    # allowed to suspend).
    Q=1024; OUT="$RES/idle"; rm -rf "$OUT"; mkdir -p "$OUT"
    start_daemon 1024 "$OUT"
    for mode in tracked fixed; do
        start_spatial "$mode" "idle-$mode"
        sleep 7
        echo "-- $mode, idle for 7 s:"; graph states | grep -E "spatial|fake_airpods"
        pwp_bg pw-play --target spatial_airpods -P '{ node.name=bench_play }' "$SIG/pink.wav" >/dev/null 2>&1 &
        PLAY=$!
        sleep 1.5
        echo "-- $mode, playing (pw-top: drivers first, followers indented):"
        pwp pw-top -b -n 2 2>/dev/null | tail -n 8
        kill -TERM "$PLAY"; wait "$PLAY" 2>/dev/null
        stop_spatial
        echo "   stats: $(cat "$OUT/idle-$mode.stats.json")"
    done
    stop_all; sleep 0.5
fi

if sel clicks; then
    for Q in 1024 256; do
        OUT="$RES/clicks-q$Q"; rm -rf "$OUT"; mkdir -p "$OUT"
        start_daemon "$Q" "$OUT"
        [ "$Q" = 1024 ] && run_case pink_direct none pink static
        run_case sine_fixed_static      fixed   sine500 static
        run_case sine_tracked_static    tracked sine500 static
        run_case sine_tracked_sine50    tracked sine500 sine
        run_case sine_tracked_jump      tracked sine500 jump
        run_case sine_raw_tracked_sine50 tracked sine500 sine --tone raw
        run_case pink_fixed_static      fixed   pink    static
        run_case pink_tracked_static    tracked pink    static
        run_case pink_tracked_sine50    tracked pink    sine
        run_case pink_raw_tracked_sine50 tracked pink   sine --tone raw
        stop_all; sleep 0.5
    done
fi

if sel latency; then
    Q=1024; OUT="$RES/latency"; rm -rf "$OUT"; mkdir -p "$OUT"
    start_daemon 1024 "$OUT"
    for mode in tracked fixed; do
        start_spatial "$mode" "lat-$mode"
        pwp_bg pw-record -P '{ node.name=lat_rec node.autoconnect=false }' --rate 48000 --channels 4 \
            --channel-map FL,FR,RL,RR --format f32 "$OUT/in_out_$mode.wav" >/dev/null 2>&1 &
        REC=$!
        pwp_bg pw-play --target spatial_airpods -P '{ node.name=bench_play }' "$SIG/impulse.wav" >/dev/null 2>&1 &
        PLAY=$!
        sleep 1.5
        pwp pw-link spatial_airpods:monitor_FL lat_rec:input_FL
        pwp pw-link spatial_airpods:monitor_FR lat_rec:input_FR
        pwp pw-link fake_airpods:monitor_FL lat_rec:input_RL
        pwp pw-link fake_airpods:monitor_FR lat_rec:input_RR
        sleep 6
        kill -TERM "$PLAY" "$REC" 2>/dev/null; wait "$REC" 2>/dev/null; wait "$PLAY" 2>/dev/null
        stop_spatial
    done
    stop_all; sleep 0.5
fi

if sel tone; then
    # Frequency response and loudness, plain stereo vs raw vs equalised.
    Q=1024; OUT="$RES/tone"; rm -rf "$OUT"; mkdir -p "$OUT"
    start_daemon 1024 "$OUT"
    for sig in pink pinkc; do
        record_io "off-$sig"         none    "$sig" 10
        record_io "fixed-raw-$sig"   fixed   "$sig" 10 --tone raw
        record_io "fixed-eq-$sig"    fixed   "$sig" 10
        record_io "tracked-eq-$sig"  tracked "$sig" 10 --traj hold --yaw 0
    done
    stop_all; sleep 0.5
fi

if sel azimuths; then
    # The same, the head turned to 12 yaws: both virtual speakers visit 24
    # azimuths of the horizontal plane.
    Q=1024; OUT="$RES/azimuths"; rm -rf "$OUT"; mkdir -p "$OUT"
    start_daemon 1024 "$OUT"
    for tone in raw eq; do
        for yaw in 0 30 60 90 120 150 180 210 240 270 300 330; do
            record_io "$tone-yaw$yaw" tracked pink 6 --tone "$tone" --traj hold --yaw "$yaw"
        done
    done
    stop_all; sleep 0.5
fi

if sel scenario; then
    # Default-output policy in a real graph (both modes). Each line: ok/FAIL.
    Q=1024; OUT="$RES/scenario"; rm -rf "$OUT"; mkdir -p "$OUT"
    REPORT="$OUT/scenario.txt"; : > "$REPORT"
    check() {   # check <label> <command...>
        local label="$1"; shift
        if "$@"; then echo "ok    $label" | tee -a "$REPORT"; else echo "FAIL  $label" | tee -a "$REPORT"; fi
    }
    default_is() { [ "$(graph default | cut -d' ' -f1)" = "$1" ]; }
    effective_is() { [ "$(graph default | cut -d' ' -f2)" = "$1" ]; }
    no_spatial_nodes() { ! graph has-node spatial_airpods && ! graph has-node spatial_airpods.output; }
    level_db() {  # level_db <wav> : RMS in dBFS of the second half
        python3 -I -c '
import sys; sys.path.insert(0, sys.argv[2])
import numpy as np; from wavio import read_wav
r, x = read_wav(sys.argv[1]); x = x[len(x)//2:]
print("%.1f" % (10*np.log10(np.mean(x**2)+1e-20)))' "$1" "$B"
    }
    for mode in tracked fixed; do
        start_daemon 1024 "$OUT"
        echo "== $mode" | tee -a "$REPORT"
        if [ "$mode" = tracked ]; then
            # Nothing plays: the passive links must let the graph (and the
            # AirPods) sleep, so the renderer should hardly run.
            start_spatial tracked tracked-idle
            sleep 3
            stop_spatial
            idle_cycles="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["cycles"])' "$OUT/tracked-idle.stats.json")"
            check "tracked: idle for 3 s -> renderer ran $idle_cycles cycles (< 30)" [ "$idle_cycles" -lt 30 ]
            check "tracked: idle stop restores the default" default_is fake_speakers
        fi
        start_spatial "$mode" "$mode-1"
        sleep 0.5
        check "$mode: spatial sink is the configured default" default_is spatial_airpods
        check "$mode: and the effective default" effective_is spatial_airpods
        check "$mode: output linked to fake_airpods only" [ "$(graph links-from spatial_airpods.output)" = fake_airpods ]
        # An application playing to the default output reaches the AirPods only.
        pwp_bg pw-record --target fake_airpods -P '{ node.name=rec_a stream.capture.sink=true }' --rate 48000 --channels 2 --format f32 "$OUT/$mode-airpods.wav" >/dev/null 2>&1 & RA=$!
        pwp_bg pw-record --target fake_speakers -P '{ node.name=rec_s stream.capture.sink=true }' --rate 48000 --channels 2 --format f32 "$OUT/$mode-speakers.wav" >/dev/null 2>&1 & RS=$!
        sleep 0.3
        pwp_bg pw-play -P '{ node.name=app_play }' "$SIG/pink.wav" >/dev/null 2>&1 & PL=$!
        sleep 3
        check "$mode: app on the default output goes to spatial_airpods" [ "$(graph links-from app_play)" = spatial_airpods ]
        kill -TERM "$PL" "$RA" "$RS" 2>/dev/null; wait "$RA" "$RS" "$PL" 2>/dev/null
        la="$(level_db "$OUT/$mode-airpods.wav")"; ls_="$(level_db "$OUT/$mode-speakers.wav")"
        check "$mode: sound on fake_airpods ($la dB), silence on fake_speakers ($ls_ dB)" \
            python3 -I -c 'import sys; sys.exit(not (float(sys.argv[1]) > -40 and float(sys.argv[2]) < -100))' "$la" "$ls_"
        # Listen Together starts: its combine sink appears and becomes the default.
        pwp pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=listen_together media.class=Audio/Sink audio.position=[FL FR] object.linger=true }' >/dev/null
        sleep 0.3; set_default listen_together
        check "$mode: Listen Together active -> Paused(Sharing)" wait_log "$OUT/$mode-1.log" "Paused\(Sharing\)" 5
        check "$mode: spatial sink kept while sharing" graph has-node spatial_airpods
        check "$mode: default left to Listen Together" default_is listen_together
        # Listen Together stops: it hands the default back, then its node goes.
        set_default spatial_airpods
        pwp pw-cli destroy "$(graph node-id listen_together)" >/dev/null
        sleep 1
        check "$mode: sharing over -> Active again" bash -c "tail -n 1 '$OUT/$mode-1.log' | grep -q 'Active'"
        stop_spatial
        sleep 0.3
        check "$mode: stop() restores the previous default (fake_speakers)" default_is fake_speakers
        check "$mode: stop() removes the spatial nodes" no_spatial_nodes
        # The user picks another output: stop without touching the default.
        start_spatial "$mode" "$mode-2"
        sleep 0.5
        set_default fake_speakers
        check "$mode: user switch -> Stopped(UserSwitched)" wait_log "$OUT/$mode-2.log" "UserSwitched" 5
        wait "$SB_PID" 2>/dev/null; SB_PID=""
        sleep 0.3
        check "$mode: user switch leaves the user's choice" default_is fake_speakers
        check "$mode: user switch removes the spatial nodes" no_spatial_nodes
        # The AirPods disappear: no fallback to the speakers, default restored.
        start_spatial "$mode" "$mode-3"
        sleep 0.5
        pwp pw-cli destroy "$(graph node-id fake_airpods)" >/dev/null
        check "$mode: AirPods gone -> Stopped(TargetGone)" wait_log "$OUT/$mode-3.log" "TargetGone" 5
        wait "$SB_PID" 2>/dev/null; SB_PID=""
        sleep 0.3
        check "$mode: AirPods gone restores the default (fake_speakers)" default_is fake_speakers
        check "$mode: AirPods gone removes the spatial nodes" no_spatial_nodes
        stop_all; sleep 0.5
    done
    echo "== $(grep -c '^ok' "$REPORT") ok, $(grep -c '^FAIL' "$REPORT") failed" | tee -a "$REPORT"
fi
if sel sinks; then
    # SinkWatch, the engine's view of the AirPods' node: present, running, gone.
    Q=1024; OUT="$RES/sinks"; rm -rf "$OUT"; mkdir -p "$OUT"
    REPORT="$OUT/sinks.txt"; : > "$REPORT"
    check() {   # check <label> <command...>
        local label="$1"; shift
        if "$@"; then echo "ok    $label" | tee -a "$REPORT"; else echo "FAIL  $label" | tee -a "$REPORT"; fi
    }
    count_is() { [ "$(grep -c "^NODE fake_airpods $1\$" "$OUT/watch.log")" -ge "$2" ]; }
    wait_count() {  # wait_count <state> <n> <timeout s>
        for _ in $(seq $(( $3 * 10 ))); do count_is "$1" "$2" && return 0; sleep 0.1; done
        return 1
    }
    start_daemon 1024 "$OUT"
    bench_bg watch-sinks --prefix fake_airpods --seconds 60 > "$OUT/watch.log" 2>&1 &
    SB_PID=$!
    check "sinks: fake_airpods seen, idle" wait_count Stopped 1 5
    pwp_bg pw-play --target fake_airpods -P '{ node.name=bench_play }' "$SIG/sine500.wav" >/dev/null 2>&1 & PL=$!
    check "sinks: a stream plays to it -> Running" wait_count Running 1 5
    kill -TERM "$PL" 2>/dev/null; wait "$PL" 2>/dev/null
    check "sinks: the stream ends -> not running" wait_count Stopped 2 15
    pwp_bg pw-play --target fake_airpods -P '{ node.name=bench_play }' "$SIG/sine500.wav" >/dev/null 2>&1 & PL=$!
    check "sinks: playing again -> Running" wait_count Running 2 5
    kill -TERM "$PL" 2>/dev/null; wait "$PL" 2>/dev/null
    pwp pw-cli destroy "$(graph node-id fake_airpods)" >/dev/null
    check "sinks: node destroyed -> Gone" wait_count Gone 1 5
    check "sinks: no other node reported" bash -c "! grep '^NODE' '$OUT/watch.log' | grep -v '^NODE fake_airpods '"
    kill -TERM "$SB_PID" 2>/dev/null; wait "$SB_PID" 2>/dev/null; SB_PID=""
    stop_all; sleep 0.5
    echo "== $(grep -c '^ok' "$REPORT") ok, $(grep -c '^FAIL' "$REPORT") failed" | tee -a "$REPORT"
fi
echo "== done"
