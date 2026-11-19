#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Fake pods engine: serves docs/io.github.d101c.Pods1.xml on the session bus
with made-up AirPods, to try the extension without podsd, Bluetooth or
AirPods.

It refuses the user's real session bus: run it inside dbus-run-session (the
headless test does). --allow-real-bus overrides that, for a nested Shell the
developer starts by hand.

    fake-engine.py [scenario ...]        scenarios played in order at start

then one scenario per line on standard input:

    idle                 engine up, no AirPods (ActiveDevice "/")
    pro1                 AirPods Pro (1st generation) connected: Off,
                         Transparency, Noise Cancellation; both in the ears;
                         82 % / 79 % / case 64 % charging
    pro2                 AirPods Pro (2nd generation), with Adaptive
    low [part [pct]]     battery of part (default left) at pct (default 9) and
                         the LowBattery signal
    one-bud              left earbud in the closed case (level unknown), right
                         in the ear at 71 %
    charging             both earbuds in the case, everything charging
    unknown              no battery level known, wearing unknown
    disconnect           the AirPods go away (Connected false, then "/")
    reconnect            they come back
    paired               AirPods Pro (1st generation) paired but not connected
                         (no active device): what the case card offers to
                         connect
    lid-open [L R C]     the lid of their case opens: CaseOpened with a
                         snapshot of the device (levels L, R, C, not
                         charging, if given)
    lid-close            CaseClosed
    connect-delay SECONDS  how long Connect() takes to bring them (1)
    connect-fail on|off  Connect() fails with Failed
    pause                both buds out, the engine holds the media it paused
    resume               both back in the ears, media playing again
    stem                 a press and hold on the stem: next mode, source "stem"
    mode MODE [SOURCE]   NoiseMode set by someone else (source "other")
    battery PART PCT [charging]
    wear LEFT RIGHT      ear, out, case or unknown
    delay SECONDS        how long the AirPods take to switch modes (0.2)
    spatial MODE         SpatialMode set by someone else: off, fixed or head
    sound on|off         whether sound plays to the AirPods (on): with Head
                         Tracked, HeadTracking goes active, else idle
    spatial-status S     SpatialStatus forced: off, starting, active, paused,
                         failed (paused = audio shared, e.g. Listen Together)
    tracking STATE       HeadTracking forced: unavailable, idle, starting,
                         active
    follow on|off        FollowScreen set by someone else
    pose YAW [PITCH]     the head, held still (degrees from the screen, yaw
                         positive to the left), for HeadPose; forgets Recenter
    pose-sim on|off      the head turning from side to side (55° each way)
    spatial-support on|off  off: an engine before M3, whose spatial methods
                         fail with NotSupported
    sleep SECONDS        wait before the next scenario
    quit

Every method call is printed as "CALL <Method> <arguments>". HeadPose is sent
at 30 Hz while someone watches (WatchHeadPose) and HeadTracking is active;
"POSES ON" and "POSES OFF <count>" mark each stream, "WATCHERS <n>" each
change of watchers (a watcher that leaves the bus stops watching).
"""
import math
import os
import time
import pathlib
import sys

from gi.repository import Gio, GLib

BUS_NAME = 'io.github.d101c.Pods'
MANAGER_PATH = '/io/github/d101c/Pods'
MANAGER_IFACE = 'io.github.d101c.Pods1.Manager'
DEVICE_IFACE = 'io.github.d101c.Pods1.Device'
CONTRACT = pathlib.Path(__file__).resolve().parents[2] / 'docs' / 'io.github.d101c.Pods1.xml'
ERROR = 'io.github.d101c.Pods1.Error.'

PRO1_ADDRESS = 'FA:KE:00:00:00:01'
PRO2_ADDRESS = 'FA:KE:00:00:00:02'

SPATIAL_METHODS = {'SetSpatialMode', 'SetFollowScreen', 'Recenter', 'WatchHeadPose'}
SPATIAL_MODES = ('off', 'fixed', 'head')
POSE_HZ = 30

loop = GLib.MainLoop()
bus = None
node = Gio.DBusNodeInfo.new_for_xml(CONTRACT.read_text())
manager_info = node.lookup_interface(MANAGER_IFACE)
device_info = node.lookup_interface(DEVICE_IFACE)
switch_delay = 0.2
connect_delay = 1.0
connect_fails = False
# Devices whose case lid is open.
lids = set()
spatial_supported = True
sound_playing = True
# The head: held at pose_yaw, or turning (pose_sim); recenter_yaw is the
# direction Recenter made the front.
pose_yaw = 0.0
pose_pitch = 0.0
pose_sim = False
recenter_yaw = 0.0
started = time.monotonic()


def log(*parts):
    print(*parts, flush=True)


def device_path(address):
    return f'{MANAGER_PATH}/devices/{address.replace(":", "_")}'


class DBusError(Exception):
    def __init__(self, name, message):
        super().__init__(message)
        self.name = name


class Exported:
    """One object on the bus with the properties of one interface."""

    def __init__(self, path, info, props):
        self.path = path
        self.info = info
        self.props = props
        self.types = {p.name: p.signature for p in info.properties}
        assert set(props) == set(self.types), set(props) ^ set(self.types)
        self.reg_id = bus.register_object(path, info, self._call, self._get, None)

    def unregister(self):
        bus.unregister_object(self.reg_id)

    def variant(self, name):
        return GLib.Variant(self.types[name], self.props[name])

    def set(self, **changes):
        changed = {}
        for name, value in changes.items():
            if self.props[name] != value:
                self.props[name] = value
                changed[name] = self.variant(name)
        if changed:
            bus.emit_signal(None, self.path, 'org.freedesktop.DBus.Properties', 'PropertiesChanged',
                            GLib.Variant('(sa{sv}as)', (self.info.name, changed, [])))

    def emit(self, signal, signature, *args):
        bus.emit_signal(None, self.path, self.info.name, signal, GLib.Variant(f'({signature})', args))

    def _get(self, _conn, _sender, _path, _iface, name):
        return self.variant(name)

    def _call(self, _conn, sender, _path, _iface, method, params, invocation):
        args = params.unpack()
        log('CALL', method, *[repr(a) for a in args])
        self.sender = sender
        try:
            result = self.handle(method, *args)
        except DBusError as e:
            invocation.return_dbus_error(ERROR + e.name, str(e))
            return
        invocation.return_value(result)

    def handle(self, method, *args):
        raise DBusError('NotSupported', method)


class Manager(Exported):
    def __init__(self):
        super().__init__(MANAGER_PATH, manager_info, {
            'Devices': [],
            'ActiveDevice': '/',
            'EngineVersion': '0.0.0-fake',
            'Conflict': '',
        })


class Device(Exported):
    def __init__(self, address, **props):
        base = {
            'Address': address,
            'Name': 'AirPods Pro',
            'Model': '0x200E',
            'ModelName': 'AirPods Pro (1st generation)',
            'Firmware': '6F21',
            'Color': '',
            'Connected': True,
            'Capabilities': ['noise.off', 'noise.transparency', 'noise.anc', 'pressHold',
                             'rename', 'earDetection'],
            'Battery': {'left': (82, False), 'right': (79, False), 'case': (64, True)},
            'Wearing': ('ear', 'ear'),
            'NoiseMode': 'anc',
            'NoiseModes': ['off', 'transparency', 'anc'],
            'PressHoldModes': ['transparency', 'anc'],
            'EarDetection': True,
            'PauseOnRemove': True,
            'ResumeOnReturn': True,
            'MediaPaused': False,
            'ConversationAwareness': False,
            'SpatialMode': 'off',
            'SpatialStatus': 'off',
            'FollowScreen': False,
            'HeadTracking': 'unavailable',
        }
        base.update(props)
        super().__init__(device_path(address), device_info, base)
        # Bus names watching HeadPose, with their name watches.
        self.watchers = {}
        self.pose_id = 0
        self.poses = 0

    def has_sensors(self):
        return 'spatial.head' in self.props['Capabilities']

    def follow_sound(self):
        """HeadTracking as the engine sets it: sensors run in Head Tracked
        while sound plays."""
        if not self.has_sensors():
            self.set(HeadTracking='unavailable')
        elif self.props['SpatialMode'] == 'head' and sound_playing and self.props['Connected']:
            self.set(HeadTracking='starting')
            GLib.timeout_add(300, lambda: self.props['HeadTracking'] == 'starting'
                             and self.set(HeadTracking='active') or self.stream() and False)
        else:
            self.set(HeadTracking='idle')
        self.stream()

    def set(self, **changes):
        super().set(**changes)
        if 'HeadTracking' in changes or 'Connected' in changes:
            self.stream()

    def watch(self, sender, on):
        if on and sender not in self.watchers:
            self.watchers[sender] = Gio.bus_watch_name_on_connection(
                bus, sender, Gio.BusNameWatcherFlags.NONE, None,
                lambda *_: self.watch(sender, False))
        elif not on and sender in self.watchers:
            Gio.bus_unwatch_name(self.watchers.pop(sender))
        else:
            return
        log('WATCHERS', len(self.watchers))
        self.stream()

    def stream(self):
        """HeadPose at 30 Hz while watched and active, nothing otherwise."""
        wanted = bool(self.watchers) and self.props['HeadTracking'] == 'active' and self.props['Connected']
        if wanted and not self.pose_id:
            self.poses = 0
            self.pose_id = GLib.timeout_add(1000 // POSE_HZ, self.send_pose)
            log('POSES ON')
        elif not wanted and self.pose_id:
            GLib.source_remove(self.pose_id)
            self.pose_id = 0
            log('POSES OFF', self.poses)
        return True

    def raw_yaw(self):
        if pose_sim:
            return 55 * math.sin((time.monotonic() - started) * 1.1)
        return pose_yaw

    def send_pose(self):
        yaw = self.raw_yaw() - recenter_yaw
        yaw = (yaw + 180) % 360 - 180
        self.emit('HeadPose', 'dd', yaw, pose_pitch)
        self.poses += 1
        return GLib.SOURCE_CONTINUE

    def switch(self, mode, source):
        def apply():
            self.set(NoiseMode=mode)
            self.emit('NoiseModeChanged', 'ss', mode, source)
            return GLib.SOURCE_REMOVE
        GLib.timeout_add(int(switch_delay * 1000), apply)

    def next_mode(self):
        cycle = self.props['PressHoldModes'] or self.props['NoiseModes']
        current = self.props['NoiseMode']
        return cycle[(cycle.index(current) + 1) % len(cycle)] if current in cycle else cycle[0]

    def handle(self, method, *args):
        global recenter_yaw
        if method in SPATIAL_METHODS and not spatial_supported:
            raise DBusError('NotSupported', f'{method} comes after M2')
        if method == 'Connect':
            if connect_fails:
                raise DBusError('Failed', 'Page timeout')
            GLib.timeout_add(int(connect_delay * 1000), lambda: connected(self) and False)
            return None
        if not self.props['Connected'] and method != 'Disconnect':
            raise DBusError('NotConnected', 'The AirPods are not connected')
        if method == 'CycleNoiseMode':
            self.switch(self.next_mode(), 'tile')
        elif method == 'SetNoiseMode':
            (mode,) = args
            if mode not in self.props['NoiseModes']:
                raise DBusError('InvalidArgument', f'{mode} is not a mode of these AirPods')
            self.switch(mode, 'tile')
        elif method == 'SetPressHoldModes':
            (modes,) = args
            if len(modes) < 2 or not set(modes) <= set(self.props['NoiseModes']):
                raise DBusError('InvalidArgument', 'Pick at least two modes of these AirPods')
            self.set(PressHoldModes=list(modes))
        elif method == 'Rename':
            (name,) = args
            if not name.strip():
                raise DBusError('InvalidArgument', 'Empty name')
            self.set(Name=name)
        elif method == 'SetEarDetection':
            self.set(EarDetection=args[0])
        elif method == 'SetPauseOnRemove':
            self.set(PauseOnRemove=args[0])
        elif method == 'SetResumeOnReturn':
            self.set(ResumeOnReturn=args[0])
        elif method == 'SetConversationAwareness':
            if 'conversation' not in self.props['Capabilities']:
                raise DBusError('NotSupported', 'No Conversation Awareness on these AirPods')
            self.set(ConversationAwareness=args[0])
        elif method == 'Disconnect':
            disconnect()
        elif method == 'SetSpatialMode':
            (mode,) = args
            if mode not in SPATIAL_MODES:
                raise DBusError('InvalidArgument', f'{mode} is not a spatial mode')
            if mode == 'head' and not self.has_sensors():
                raise DBusError('NotSupported', 'No motion sensors on these AirPods')
            self.set(SpatialMode=mode, SpatialStatus='off' if mode == 'off' else 'active')
            self.follow_sound()
        elif method == 'SetFollowScreen':
            if not self.has_sensors():
                raise DBusError('NotSupported', 'No motion sensors on these AirPods')
            self.set(FollowScreen=args[0])
        elif method == 'Recenter':
            if self.props['SpatialMode'] != 'head':
                raise DBusError('Failed', 'Recenter needs Head Tracked')
            recenter_yaw = self.raw_yaw()
        elif method == 'WatchHeadPose':
            if not self.has_sensors():
                raise DBusError('NotSupported', 'No motion sensors on these AirPods')
            self.watch(self.sender, args[0])
        else:
            raise DBusError('NotSupported', method)
        return None


manager = None
devices = {}
active = None


def ensure_device(address, **props):
    if address not in devices:
        devices[address] = Device(address, **props)
        manager.set(Devices=[d.path for d in devices.values()])
    return devices[address]


def activate(device):
    global active
    active = device
    device.set(Connected=True)
    manager.set(ActiveDevice=device.path)


def disconnect():
    global active
    if active:
        active.set(Connected=False)
        manager.set(ActiveDevice='/')
        active = None


def snapshot(dev, levels=None):
    snap = {
        'name': GLib.Variant('s', dev.props['Name']),
        'model': GLib.Variant('s', dev.props['Model']),
        'color': GLib.Variant('s', dev.props['Color']),
        'connected': GLib.Variant('b', dev.props['Connected']),
        'budsInCase': GLib.Variant('y', 2),
    }
    battery = dict(dev.props['Battery'])
    for part, pct in zip(('left', 'right', 'case'), levels or []):
        battery[part] = (int(pct), False)
    for part, (pct, charging) in battery.items():
        snap[part] = GLib.Variant('y', pct)
        snap[f'{part}Charging'] = GLib.Variant('b', charging)
    return snap


def case_opened(dev, levels=None):
    manager.emit('CaseOpened', 'oa{sv}', dev.path, snapshot(dev, levels))


def connected(dev):
    """Connect() went through: the AirPods are the active device, and the open
    lid says so with a new snapshot."""
    if active is not dev:
        disconnect()
    activate(dev)
    log('CONNECTED', dev.path, f'{time.time():.3f}')
    if dev.props['Address'] in lids:
        case_opened(dev)


def lid_device():
    return active or devices.get(PRO1_ADDRESS) or next(iter(devices.values()))


def need_active():
    if not active:
        raise ValueError('no AirPods connected')
    return active


def scenario(line):
    global switch_delay, connect_delay, connect_fails, active
    global spatial_supported, sound_playing, pose_yaw, pose_pitch, pose_sim, recenter_yaw
    words = line.split()
    if not words:
        return
    cmd, args = words[0], words[1:]
    log('SCENARIO', line)
    if cmd == 'idle':
        disconnect()
    elif cmd == 'pro1':
        disconnect()
        activate(ensure_device(PRO1_ADDRESS, Name='AirPods Pro de Camille'))
    elif cmd == 'pro2':
        disconnect()
        activate(ensure_device(
            PRO2_ADDRESS, Name='AirPods Pro de Léo', Model='0x2014',
            ModelName='AirPods Pro (2nd generation)',
            Capabilities=['noise.off', 'noise.transparency', 'noise.adaptive', 'noise.anc',
                          'conversation', 'pressHold', 'rename', 'earDetection', 'spatial.head'],
            NoiseMode='adaptive', NoiseModes=['off', 'transparency', 'adaptive', 'anc'],
            PressHoldModes=['transparency', 'adaptive', 'anc'],
            Battery={'left': (100, False), 'right': (96, False), 'case': (71, False)},
            HeadTracking='idle'))
    elif cmd == 'low':
        part = args[0] if args else 'left'
        pct = int(args[1]) if len(args) > 1 else 9
        dev = need_active()
        battery = dict(dev.props['Battery'])
        battery[part] = (pct, False)
        dev.set(Battery=battery)
        dev.emit('LowBattery', 'sy', part, pct)
    elif cmd == 'one-bud':
        need_active().set(Wearing=('case', 'ear'), Battery={'right': (71, False)})
    elif cmd == 'charging':
        need_active().set(Wearing=('case', 'case'),
                          Battery={'left': (46, True), 'right': (51, True), 'case': (88, True)})
    elif cmd == 'unknown':
        need_active().set(Wearing=('unknown', 'unknown'), Battery={})
    elif cmd == 'disconnect':
        disconnect()
    elif cmd == 'reconnect':
        dev = devices.get(PRO1_ADDRESS) or next(iter(devices.values()))
        activate(dev)
    elif cmd == 'paired':
        disconnect()
        dev = ensure_device(PRO1_ADDRESS, Name='AirPods Pro de Camille')
        dev.set(Connected=False)
    elif cmd == 'lid-open':
        dev = lid_device()
        lids.add(dev.props['Address'])
        case_opened(dev, args)
    elif cmd == 'lid-close':
        dev = lid_device()
        lids.discard(dev.props['Address'])
        manager.emit('CaseClosed', 'o', dev.path)
    elif cmd == 'connect-delay':
        connect_delay = float(args[0])
    elif cmd == 'connect-fail':
        connect_fails = args[0] == 'on'
    elif cmd == 'pause':
        need_active().set(Wearing=('out', 'out'), MediaPaused=True)
    elif cmd == 'resume':
        need_active().set(Wearing=('ear', 'ear'), MediaPaused=False)
    elif cmd == 'stem':
        dev = need_active()
        dev.switch(dev.next_mode(), 'stem')
    elif cmd == 'mode':
        dev = need_active()
        dev.switch(args[0], args[1] if len(args) > 1 else 'other')
    elif cmd == 'battery':
        dev = need_active()
        battery = dict(dev.props['Battery'])
        battery[args[0]] = (int(args[1]), len(args) > 2 and args[2] == 'charging')
        dev.set(Battery=battery)
    elif cmd == 'wear':
        need_active().set(Wearing=(args[0], args[1]))
    elif cmd == 'delay':
        switch_delay = float(args[0])
    elif cmd == 'spatial':
        dev = need_active()
        dev.set(SpatialMode=args[0])
        dev.follow_sound()
    elif cmd == 'sound':
        sound_playing = args[0] == 'on'
        need_active().follow_sound()
    elif cmd == 'tracking':
        need_active().set(HeadTracking=args[0])
    elif cmd == 'spatial-status':
        need_active().set(SpatialStatus=args[0])
    elif cmd == 'follow':
        need_active().set(FollowScreen=args[0] == 'on')
    elif cmd == 'pose':
        recenter_yaw = 0.0
        pose_sim = False
        pose_yaw = float(args[0])
        pose_pitch = float(args[1]) if len(args) > 1 else 0.0
    elif cmd == 'pose-sim':
        pose_sim = args[0] == 'on'
    elif cmd == 'spatial-support':
        spatial_supported = args[0] == 'on'
    elif cmd == 'quit':
        loop.quit()
    else:
        raise ValueError(f'unknown scenario {cmd}')


def run(line):
    try:
        scenario(line.strip())
    except Exception as e:  # a typo in a scenario must not stop the engine
        log('ERROR', line.strip(), e)


def play(lines):
    """Plays lines in order; 'sleep N' waits without blocking the bus."""
    lines = list(lines)
    while lines:
        line = lines.pop(0)
        words = line.split()
        if words and words[0] == 'sleep':
            GLib.timeout_add(int(float(words[1]) * 1000), lambda: play(lines) and False)
            return False
        run(line)
    return False


def on_stdin(channel, condition):
    if condition & GLib.IO_IN:
        line = channel.readline()
        if line:
            play([line])
            return True
    # End of input: keep serving until 'quit' or a signal.
    return False


def main():
    global bus, manager
    argv = sys.argv[1:]
    allow_real = '--allow-real-bus' in argv
    argv = [a for a in argv if a != '--allow-real-bus']
    address = os.environ.get('DBUS_SESSION_BUS_ADDRESS', '')
    real = f'unix:path=/run/user/{os.getuid()}/bus'
    if (not address or address.startswith(real)) and not allow_real:
        sys.exit('fake-engine: this looks like the real session bus; run it inside '
                 'dbus-run-session (or pass --allow-real-bus)')

    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    manager = Manager()

    def acquired(*_):
        log('READY', BUS_NAME)
        GLib.idle_add(lambda: play(argv))

    def lost(*_):
        log('LOST', BUS_NAME)
        loop.quit()

    Gio.bus_own_name_on_connection(bus, BUS_NAME, Gio.BusNameOwnerFlags.NONE, acquired, lost)
    for signum in (2, 15):  # SIGINT, SIGTERM
        GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signum, lambda: loop.quit() or False)
    channel = GLib.IOChannel.unix_new(sys.stdin.fileno())
    GLib.io_add_watch(channel, GLib.PRIORITY_DEFAULT, GLib.IO_IN | GLib.IO_HUP, on_stdin)
    loop.run()


if __name__ == '__main__':
    main()
