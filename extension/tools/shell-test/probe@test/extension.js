// SPDX-License-Identifier: GPL-2.0-or-later
// Test driver of extension/tools/shell-test. It polls $PROBE_DIR/cmd, runs the
// one command written there, then deletes the file:
//
//   open | close        the Quick Settings menu
//   click               the AirPods tile (its left part)
//   menu | menuclose    the tile's menu, through its arrow
//   segment <mode>      a button of the noise control
//   spatial <mode>      a button of the spatial audio selector
//   recenter            the Recenter button under it
//   notifaction <label> the action of that name of an AirPods notification
//   settings            the "AirPods Settings" item
//   conversation        the Conversation Awareness switch of the menu
//   cardconnect         the Connect button of the case card
//   cardclose           its close button
//   cardhover on|off    the pointer on the card, or not
//   pointer X Y         move the pointer there
//   sharing on|off      the Shell's screen sharing indicator, as if the
//                       screen were shared
//   hideosd             hide the on-screen display at once
//   closewindows        close every window
//   gc                  run the JS garbage collector
//   state <name>        what is on screen, to <name>.json
//   shot <name>         screenshot of the whole screen, to <name>.png
//
// The extension under test carries nothing for the tests: the probe finds its
// widgets in the Shell's own trees.
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import St from 'gi://St';
import System from 'system';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';

export default class Probe {
    enable() {
        this._dir = GLib.getenv('PROBE_DIR');
        this._id = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 50, () => this._tick());
    }

    disable() {
        GLib.source_remove(this._id);
    }

    _toggle() {
        const grid = Main.panel.statusArea.quickSettings.menu._grid;
        return grid.get_children().find(c => c.constructor.name === 'PodsToggle') ?? null;
    }

    _card() {
        const layer = Main.layoutManager.uiGroup.get_children()
            .find(c => c.name === 'pods-case-card-layer');
        return layer?.get_first_child() ?? null;
    }

    _conversation(toggle) {
        return this._menuItems(toggle).find(i => i.constructor.name === 'PodsSwitchItem') ?? null;
    }

    _tick() {
        const path = `${this._dir}/cmd`;
        if (!GLib.file_test(path, GLib.FileTest.EXISTS))
            return GLib.SOURCE_CONTINUE;
        const [, bytes] = GLib.file_get_contents(path);
        const [cmd, ...args] = new TextDecoder().decode(bytes).trim().split(/\s+/);
        try {
            this._run(cmd, args);
        } catch (e) {
            logError(e, 'probe');
            const old = GLib.file_test(`${this._dir}/error`, GLib.FileTest.EXISTS)
                ? new TextDecoder().decode(GLib.file_get_contents(`${this._dir}/error`)[1]) : '';
            GLib.file_set_contents(`${this._dir}/error`, `${old}${cmd} ${args.join(' ')}: ${e.message}\n`);
        }
        GLib.unlink(path);
        return GLib.SOURCE_CONTINUE;
    }

    _run(cmd, args) {
        const qs = Main.panel.statusArea.quickSettings;
        const toggle = this._toggle();
        switch (cmd) {
        case 'open':
            qs.menu.open();
            break;
        case 'close':
            qs.menu.close();
            break;
        case 'click':
            // The left part of a QuickMenuToggle is a QuickToggle of its own.
            toggle._box.get_first_child().emit('clicked', Clutter.BUTTON_PRIMARY);
            break;
        case 'menu':
            toggle._menuButton.emit('clicked', Clutter.BUTTON_PRIMARY);
            break;
        case 'menuclose':
            toggle.menu.close();
            break;
        case 'segment':
            this._segments(toggle).find(b => b.mode === args[0]).emit('clicked', Clutter.BUTTON_PRIMARY);
            break;
        case 'spatial':
            toggle._spatialBox.get_children().find(b => b.mode === args[0]).emit('clicked', Clutter.BUTTON_PRIMARY);
            break;
        case 'recenter':
            toggle._recenterButton.emit('clicked', Clutter.BUTTON_PRIMARY);
            break;
        case 'notifaction': {
            const label = args.join(' ');
            const notification = Main.messageTray.getSources().filter(s => s.title === 'AirPods')
                .flatMap(s => s.notifications).find(n => n.actions.some(a => a.label === label));
            // As a click on its button: it runs, then the notification goes.
            notification.actions.find(a => a.label === label).activate();
            break;
        }
        case 'settings':
            this._menuItems(toggle).find(i => i.label?.text === 'AirPods Settings').activate(null);
            break;
        case 'conversation':
            this._conversation(toggle).activate(null);
            break;
        case 'cardconnect':
            this._card().connectButton.emit('clicked', Clutter.BUTTON_PRIMARY);
            break;
        case 'cardclose':
            this._card().closeButton.emit('clicked', Clutter.BUTTON_PRIMARY);
            break;
        case 'cardhover':
            this._card().hover = args[0] === 'on';
            break;
        case 'pointer': {
            const seat = global.stage.context.get_backend().get_default_seat();
            this._pointer ??= seat.create_virtual_device(Clutter.InputDeviceType.POINTER_DEVICE);
            this._pointer.notify_absolute_motion(GLib.get_monotonic_time(), Number(args[0]), Number(args[1]));
            break;
        }
        case 'sharing':
            Main.panel.statusArea.screenSharing.visible = args[0] === 'on';
            break;
        case 'hideosd':
            Main.osdWindowManager.hideAll();
            break;
        case 'gc':
            System.gc();
            break;
        case 'closewindows':
            for (const actor of global.get_window_actors())
                actor.meta_window.delete(global.get_current_time());
            break;
        case 'state':
            GLib.file_set_contents(`${this._dir}/${args[0]}.json`,
                `${JSON.stringify(this._state(toggle), null, 1)}\n`);
            break;
        case 'shot': {
            const file = Gio.File.new_for_path(`${this._dir}/${args[0]}.png`);
            const stream = file.replace(null, false, Gio.FileCreateFlags.NONE, null);
            new Shell.Screenshot().screenshot(false, stream, (o, res) => {
                try {
                    o.screenshot_finish(res);
                } catch (e) {
                    logError(e, 'probe');
                }
                stream.close(null);
            });
            break;
        }
        default:
            throw new Error(`unknown command ${cmd}`);
        }
    }

    _cardState(card, monitor) {
        if (!card)
            return {found: false, shown: false};
        const [x, y] = card.get_transformed_position();
        const [width, height] = card.get_transformed_size();
        return {
            found: true,
            shown: card.get_parent().visible,
            opacity: card.opacity,
            hover: card.hover,
            translationY: Math.round(card.translation_y),
            title: card.title.text,
            status: card.status.text,
            check: card.check.visible,
            connectedStyle: card.statusBox.has_style_class_name('pods-connected'),
            batteries: card.batteryWidgets.map(b => ({
                kind: b.kind,
                text: b.item?.text,
                label: b.item?.label,
                charging: b.item?.charging,
                low: b.item?.low,
            })),
            connect: card.connectButton.visible,
            connecting: card.connecting.visible,
            art: card.art.gicon?.to_string().replace(/.*\//, '') ?? null,
            floating: card.art.get_transition('translation-y') !== null,
            width: Math.round(width),
            centreOffset: Math.round(x + width / 2 - (monitor.x + monitor.width / 2)),
            bottomGap: Math.round(monitor.y + monitor.height - (y + height)),
        };
    }

    _segments(toggle) {
        return toggle?._noiseBox.get_children() ?? [];
    }

    _menuItems(toggle) {
        return toggle?.menu._getMenuItems() ?? [];
    }

    _state(toggle) {
        const qs = Main.panel.statusArea.quickSettings;
        const osd = Main.osdWindowManager._osdWindows[0];
        const menu = toggle?.menu;
        const subtitle = toggle?._box.get_first_child()._subtitle ?? null;
        const conversation = this._conversation(toggle);
        const card = this._card();
        const monitor = Main.layoutManager.primaryMonitor;
        return {
            found: toggle !== null,
            visible: toggle?.visible ?? false,
            mapped: toggle?.mapped ?? false,
            checked: toggle?.checked ?? false,
            title: toggle?.title ?? null,
            subtitle: toggle?.subtitle ?? null,
            icon: toggle?.gicon?.to_string().replace(/.*\//, '') ?? null,
            lowBatteryStyle: toggle?.has_style_class_name('pods-low-battery') ?? false,
            subtitleCut: subtitle?.clutter_text.get_layout().is_ellipsized() ?? null,
            conversation: conversation ? {
                visible: conversation.visible,
                state: conversation.state,
                title: conversation.label.text,
                subtitle: conversation.label.get_parent().get_last_child().text,
            } : null,
            card: this._cardState(card, monitor),
            fullscreen: monitor?.inFullscreen ?? false,
            animations: St.Settings.get().enable_animations,
            qsOpen: qs.menu.isOpen,
            menuOpen: menu?.isOpen ?? false,
            header: menu ? {
                title: menu._headerTitle.text,
                subtitle: menu._headerSubtitle.text,
                iconActive: menu._headerIcon.has_style_class_name('active'),
            } : null,
            rings: (toggle?._rings ?? []).map(r => ({
                part: r.part,
                text: r.item?.text,
                label: r.item?.label,
                known: r.item?.known,
                charging: r.item?.charging,
                low: r.item?.low,
                width: r._ring.width,
                height: r._ring.height,
            })),
            noiseVisible: toggle?._noiseBox.visible ?? false,
            spatial: toggle ? {
                visible: toggle._spatialBox.visible && toggle._spatialTitle.visible,
                buttons: toggle._spatialBox.get_children().map(b => ({
                    mode: b.mode,
                    label: b.accessible_name,
                    checked: b.checked,
                    icon: b.get_child().get_first_child().gicon?.to_string().replace(/.*\//, '') ?? null,
                    width: Math.round(b.width),
                })),
                hint: toggle._spatialHintBox.visible ? toggle._spatialHint.text : null,
                recenter: toggle._recenterButton.visible ? toggle._recenterButton.label : null,
            } : null,
            noise: this._segments(toggle).map(b => ({
                mode: b.mode,
                label: b.accessible_name,
                checked: b.checked,
                width: Math.round(b.width),
            })),
            menuItems: this._menuItems(toggle).map(i => i.label?.text ?? i.constructor.name),
            osd: {
                visible: osd?.visible ?? false,
                label: osd?._label.visible ? osd._label.text : null,
                icon: osd?._icon.gicon?.to_string().replace(/.*\//, '') ?? null,
            },
            notifications: Main.messageTray.getSources().flatMap(s => s.notifications.map(n => ({
                source: s.title,
                title: n.title,
                body: n.body,
                urgency: n.urgency,
                actions: n.actions.map(a => a.label),
            }))),
            panelIcons: qs._indicators.get_children().filter(c => c.visible).length,
            overlayChildren: qs.menu._overlay.get_n_children(),
            windows: global.get_window_actors().map(a => a.meta_window.get_title()),
        };
    }
}
