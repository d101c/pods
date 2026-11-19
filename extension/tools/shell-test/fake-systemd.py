#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""A stand-in for the systemd user manager on the test's private session bus:
org.freedesktop.systemd1, with the few Manager methods the extension's engine
installer calls (Reload, StartUnit, RestartUnit, StopUnit).

Units are read from $XDG_CONFIG_HOME/systemd/user at Reload only, as systemd
does after a daemon-reload: a unit written without Reload is unknown. StartUnit
runs the unit's ExecStart= (%h is $HOME); StopUnit terminates it.

Every call is printed as "SYSTEMD <Method> <arguments>". Refuses the real
session bus, like fake-engine.py.
"""
import os
import pathlib
import shlex
import signal
import subprocess
import sys

from gi.repository import Gio, GLib

XML = '''<node>
  <interface name="org.freedesktop.systemd1.Manager">
    <method name="Reload"/>
    <method name="StartUnit"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="o" direction="out"/></method>
    <method name="RestartUnit"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="o" direction="out"/></method>
    <method name="StopUnit"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="o" direction="out"/></method>
  </interface>
</node>'''

UNITS = pathlib.Path(os.environ['XDG_CONFIG_HOME']) / 'systemd' / 'user'
loaded = {}
running = {}
jobs = 0


def log(*parts):
    print('SYSTEMD', *parts, flush=True)


def parse(path):
    unit = {}
    for line in path.read_text().splitlines():
        if '=' in line and not line.lstrip().startswith(('#', ';', '[')):
            key, value = line.split('=', 1)
            unit[key.strip()] = value.strip()
    return unit


def start(name):
    if name in running and running[name].poll() is None:
        return
    command = loaded[name]['ExecStart'].replace('%h', os.environ['HOME'])
    running[name] = subprocess.Popen(shlex.split(command), stdin=subprocess.DEVNULL)
    log('STARTED', name, command)


def stop(name):
    process = running.pop(name, None)
    if process and process.poll() is None:
        process.send_signal(signal.SIGTERM)
        process.wait(5)
        log('STOPPED', name)


def call(_conn, _sender, _path, _iface, method, params, invocation):
    global jobs, loaded
    args = params.unpack()
    log(method, *args)
    try:
        if method == 'Reload':
            loaded = {p.name: parse(p) for p in UNITS.glob('*.service')} if UNITS.is_dir() else {}
            log('UNITS', *sorted(loaded))
            invocation.return_value(None)
            return
        name = args[0]
        if method != 'StopUnit' and name not in loaded:
            invocation.return_dbus_error('org.freedesktop.systemd1.NoSuchUnit', f'Unit {name} not found.')
            return
        if method in ('StopUnit', 'RestartUnit'):
            stop(name)
        if method in ('StartUnit', 'RestartUnit'):
            start(name)
        jobs += 1
        invocation.return_value(GLib.Variant('(o)', (f'/org/freedesktop/systemd1/job/{jobs}',)))
    except Exception as e:  # noqa: BLE001 - say it on the bus, keep serving
        invocation.return_dbus_error('org.freedesktop.systemd1.Error', str(e))


def main():
    address = os.environ.get('DBUS_SESSION_BUS_ADDRESS', '')
    if not address or address.startswith(f'unix:path=/run/user/{os.getuid()}/bus'):
        sys.exit('fake-systemd: this looks like the real session bus')
    loop = GLib.MainLoop()
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    info = Gio.DBusNodeInfo.new_for_xml(XML).interfaces[0]
    bus.register_object('/org/freedesktop/systemd1', info, call, None, None)
    Gio.bus_own_name_on_connection(bus, 'org.freedesktop.systemd1', Gio.BusNameOwnerFlags.NONE,
                                   lambda *_: log('READY'), lambda *_: loop.quit())
    for signum in (signal.SIGINT, signal.SIGTERM):
        GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signum, lambda: loop.quit() or False)
    loop.run()
    for name in list(running):
        stop(name)


if __name__ == '__main__':
    main()
