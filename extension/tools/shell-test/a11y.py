#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Drives the AirPods Settings window of the headless test through the
accessibility bus, as a screen reader would: by role and name, never by
position. Only windows of programs other than the Shell are searched.

    a11y.py click ROLE NAME      wait (10 s) for that widget, on screen, and press it
    a11y.py has ROLE NAME        prints true when it is on screen, false otherwise
    a11y.py state ROLE NAME      prints [checked, sensitive] of it (null if absent)
    a11y.py label PREFIX         prints the first label on screen starting with PREFIX
    a11y.py names ROLE           prints the names of those on screen, in order, once each

A row and the widget in it often carry the same name: the innermost one is
the one pressed and the one whose checked state is read; it counts as
sensitive only when all of them are.
    a11y.py dump                 the whole tree, to find roles and names

ROLE is an AT-SPI role name ("push button", "toggle button", "switch", "label"…).
"""
import json
import sys
import time

import gi
gi.require_version('Atspi', '2.0')
from gi.repository import Atspi  # noqa: E402

SHOWING = Atspi.StateType.SHOWING


def apps():
    desktop = Atspi.get_desktop(0)
    for i in range(desktop.get_child_count()):
        app = desktop.get_child_at_index(i)
        if app and app.get_name() != 'gnome-shell':
            yield app


def walk(node, depth=0):
    yield node, depth
    for i in range(node.get_child_count()):
        child = node.get_child_at_index(i)
        if child:
            yield from walk(child, depth + 1)


def showing(node):
    return node.get_state_set().contains(SHOWING)


def nodes(role=None):
    for app in apps():
        for node, _depth in walk(app):
            try:
                if (role is None or node.get_role_name() == role) and showing(node):
                    yield node
            except gi.repository.GLib.Error:
                continue


def find_all(role, name, wait=0.0):
    """Every match on screen, innermost last (a row and the switch in it
    both carry the row's name)."""
    deadline = time.monotonic() + wait
    while True:
        found = [node for node in nodes(role) if node.get_name() == name]
        if found or time.monotonic() >= deadline:
            return found
        time.sleep(0.2)


def find(role, name, wait=0.0):
    found = find_all(role, name, wait)
    return found[-1] if found else None


def actions(node):
    action = node.get_action_iface()
    names = []
    for i in range(action.get_n_actions() if action else 0):
        try:
            names.append(action.get_action_name(i))
        except gi.repository.GLib.Error:
            pass
    return names


def press(found):
    """Presses the innermost match that can be pressed."""
    for node in reversed(found):
        if actions(node):
            node.get_action_iface().do_action(0)
            return
    raise SystemExit(f'nothing to press among {len(found)} "{found[0].get_name()}"')


def main():
    cmd, *args = sys.argv[1:]
    if cmd == 'click':
        found = find_all(args[0], args[1], wait=10)
        if not found:
            sys.exit(f'no {args[0]} "{args[1]}" on screen')
        press(found)
    elif cmd == 'has':
        print(json.dumps(find(args[0], args[1]) is not None))
    elif cmd == 'state':
        found = find_all(args[0], args[1])
        if not found:
            print('null')
        else:
            # Checked: the innermost. Sensitive: only if the row around it is
            # too (GTK does not pass a row's state on to what it holds).
            states = found[-1].get_state_set()
            sensitive = all(n.get_state_set().contains(Atspi.StateType.SENSITIVE) for n in found)
            print(json.dumps([states.contains(Atspi.StateType.CHECKED) or states.contains(Atspi.StateType.PRESSED),
                              sensitive]))
    elif cmd == 'label':
        found = next((n.get_name() for n in nodes('label') if n.get_name().startswith(args[0])), None)
        print(json.dumps(found, ensure_ascii=False))
    elif cmd == 'names':
        names = []
        for node in nodes(args[0]):
            if node.get_name() not in names:
                names.append(node.get_name())
        print(json.dumps(names, ensure_ascii=False))
    elif cmd == 'dump':
        for app in apps():
            for node, depth in walk(app):
                states = node.get_state_set()
                print('  ' * depth + node.get_role_name(), repr(node.get_name()),
                      'showing' if states.contains(SHOWING) else '',
                      'insensitive' if not states.contains(Atspi.StateType.SENSITIVE) else '', actions(node))
    else:
        sys.exit(f'unknown command {cmd}')


if __name__ == '__main__':
    main()
