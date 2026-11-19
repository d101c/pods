// SPDX-License-Identifier: GPL-2.0-or-later
// The card that rises at the bottom of the screen, where the Shell shows its
// on-screen displays, when the case of paired AirPods opens nearby: the open
// case floating gently, the name, the batteries, and a Connect button when the
// AirPods are not connected to this computer.
//
// It follows the engine's CaseOpened and CaseClosed signals. It goes away 6 s
// after "Connected", when the lid closes, or with its close button, and comes
// back only at the next opening of the lid. It never shows over a fullscreen
// window, while the screen is shared or recorded, or when the setting "Show
// Card When Case Opens" is off. With animations off (reduced motion), it
// appears and goes at once and the case does not float.

import Atk from 'gi://Atk';
import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Graphene from 'gi://Graphene';
import Pango from 'gi://Pango';
import St from 'gi://St';

import {gettext as _} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Layout from 'resource:///org/gnome/shell/ui/layout.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Spinner} from 'resource:///org/gnome/shell/ui/animation.js';

import {errorMessage} from './engine.js';
import {caseLight, cardBatteries, cardStatus, snapshotBattery} from './model.js';
import {Ring} from './rings.js';

export const SETTING = 'show-case-card';
const AUTO_HIDE_SECONDS = 6;
// The engine asks BlueZ; past this, the attempt counts as failed.
const CONNECT_TIMEOUT_SECONDS = 30;

// The motion of the mockup: the card rises from 340 px lower with a slight
// overshoot, while it fades in.
const RISE = 340;
const SHOW_TIME = 550;
const FADE_IN_TIME = 300;
const HIDE_TIME = 350;
const FADE_OUT_TIME = 250;
const OVERSHOOT = [new Graphene.Point({x: 0.2, y: 0.9}), new Graphene.Point({x: 0.25, y: 1.08})];
// The case floats up 6 px and back, 2.6 s each way.
const FLOAT_HEIGHT = 6;
const FLOAT_TIME = 2600;

// The indicators the Shell shows in the top bar while the screen is shared
// or recorded.
const SHARING_INDICATORS = ['screenSharing', 'screenRecording'];

// One level: a ring with the earbuds, L, R or the case in it, the level and
// the name of the part.
const CardBattery = GObject.registerClass(
class PodsCardBattery extends St.BoxLayout {
    constructor(kind, icons) {
        super({style_class: 'pods-card-battery'});
        this.kind = kind;
        this.item = null;

        const ringBox = new St.Widget({
            layout_manager: new Clutter.BinLayout(),
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._ring = new Ring(kind === 'case', 'pods-card-ring');
        ringBox.add_child(this._ring);
        if (kind === 'buds') {
            ringBox.add_child(new St.Icon({
                style_class: 'pods-card-ring-icon',
                gicon: icons.get('pods-earbuds-symbolic'),
                x_align: Clutter.ActorAlign.CENTER,
                y_align: Clutter.ActorAlign.CENTER,
            }));
        } else if (kind !== 'case') {
            ringBox.add_child(new St.Label({
                style_class: 'pods-card-ring-glyph',
                text: kind === 'left' ? 'L' : 'R',
                x_align: Clutter.ActorAlign.CENTER,
                y_align: Clutter.ActorAlign.CENTER,
            }));
        }
        this.add_child(ringBox);

        const texts = new St.BoxLayout({
            orientation: Clutter.Orientation.VERTICAL,
            y_align: Clutter.ActorAlign.CENTER,
        });
        const level = new St.BoxLayout({style_class: 'pods-card-level'});
        this._bolt = new St.Icon({
            style_class: 'pods-card-bolt',
            gicon: icons.get('pods-charging-symbolic'),
            y_align: Clutter.ActorAlign.CENTER,
            visible: false,
        });
        level.add_child(this._bolt);
        this._percent = new St.Label({
            style_class: 'pods-card-percent',
            y_align: Clutter.ActorAlign.CENTER,
        });
        level.add_child(this._percent);
        texts.add_child(level);
        this._label = new St.Label({style_class: 'pods-card-label'});
        texts.add_child(this._label);
        this.add_child(texts);
    }

    // item: one entry of model.cardBatteries().
    update(item) {
        this.item = item;
        this._ring.update(item.percent, item.charging, item.low);
        this._bolt.visible = item.charging;
        this._percent.text = item.text;
        this._label.text = item.label;
        if (item.low)
            this.add_style_class_name('pods-low');
        else
            this.remove_style_class_name('pods-low');
        this.accessible_name = `${item.label} ${item.text}`;
    }
});

// The card itself, without any logic: CaseCard below fills it.
const CardView = GObject.registerClass(
class PodsCaseCardView extends St.Widget {
    constructor(icons) {
        super({
            style_class: 'pods-case-card',
            accessible_role: Atk.Role.NOTIFICATION,
            layout_manager: new Clutter.BinLayout(),
            reactive: true,
            track_hover: true,
        });
        this._icons = icons;

        const content = new St.BoxLayout({
            style_class: 'pods-card-content',
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
        });
        this.add_child(content);

        this.art = new St.Icon({
            style_class: 'pods-card-art',
            x_align: Clutter.ActorAlign.CENTER,
        });
        content.add_child(this.art);

        const heading = new St.BoxLayout({
            style_class: 'pods-card-heading',
            orientation: Clutter.Orientation.VERTICAL,
            x_align: Clutter.ActorAlign.CENTER,
        });
        this.title = new St.Label({
            style_class: 'pods-card-title',
            x_align: Clutter.ActorAlign.CENTER,
        });
        this.title.clutter_text.ellipsize = Pango.EllipsizeMode.END;
        heading.add_child(this.title);
        this.statusBox = new St.BoxLayout({
            style_class: 'pods-card-status',
            x_align: Clutter.ActorAlign.CENTER,
        });
        this.check = new St.Icon({
            style_class: 'pods-card-check',
            gicon: icons.get('pods-check-symbolic'),
            y_align: Clutter.ActorAlign.CENTER,
        });
        this.statusBox.add_child(this.check);
        this.status = new St.Label({y_align: Clutter.ActorAlign.CENTER});
        this.statusBox.add_child(this.status);
        heading.add_child(this.statusBox);
        content.add_child(heading);

        this.batteries = new St.BoxLayout({
            style_class: 'pods-card-batteries',
            x_align: Clutter.ActorAlign.CENTER,
        });
        content.add_child(this.batteries);

        this.connectButton = new St.Button({
            style_class: 'button pods-card-connect',
            can_focus: true,
            x_expand: true,
            child: new St.Label({
                text: _('Connect'),
                x_align: Clutter.ActorAlign.CENTER,
                y_align: Clutter.ActorAlign.CENTER,
            }),
        });
        content.add_child(this.connectButton);

        this.connecting = new St.Bin({style_class: 'pods-card-connecting', x_expand: true});
        const connectingBox = new St.BoxLayout({
            style_class: 'pods-card-connecting-box',
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this.spinner = new Spinner(16);
        connectingBox.add_child(this.spinner);
        connectingBox.add_child(new St.Label({
            text: _('Connecting…'),
            y_align: Clutter.ActorAlign.CENTER,
        }));
        this.connecting.child = connectingBox;
        content.add_child(this.connecting);

        // The close button sits in the top right corner, over the content: a
        // layer of boxes that are not reactive, so clicks elsewhere go through.
        const corner = new St.BoxLayout({
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
            y_expand: true,
        });
        const row = new St.BoxLayout({x_expand: true});
        row.add_child(new St.Widget({x_expand: true}));
        this.closeButton = new St.Button({
            style_class: 'pods-card-close',
            can_focus: true,
            accessible_name: _('Dismiss'),
            child: new St.Icon({gicon: icons.get('pods-close-symbolic')}),
        });
        row.add_child(this.closeButton);
        corner.add_child(row);
        this.add_child(corner);

        this._batteryKinds = '';
        this._batteryItems = [];
    }

    setBatteries(items) {
        const kinds = items.map(i => i.kind).join(',');
        if (kinds !== this._batteryKinds) {
            this._batteryKinds = kinds;
            this.batteries.destroy_all_children();
            this._batteryItems = items.map(i => {
                const widget = new CardBattery(i.kind, this._icons);
                this.batteries.add_child(widget);
                return widget;
            });
        }
        items.forEach((item, i) => this._batteryItems[i].update(item));
        this.batteries.visible = items.length > 0;
    }

    get batteryWidgets() {
        return this._batteryItems;
    }
});

export class CaseCard {
    // engine: engine.js; settings: the extension's GSettings; icons: icons.js.
    constructor(engine, settings, icons) {
        this._engine = engine;
        this._settings = settings;
        this._icons = icons;

        // The devices whose lid is open.
        this._lids = new Set();
        this._path = null;
        this._snapshot = null;
        this._state = 'disconnected';
        this._shown = false;
        this._autoHideId = 0;
        this._connectTimeoutId = 0;

        // Like the Shell's OSD: a layer the size of the primary monitor with
        // the card at the bottom, in the middle.
        this._layer = new Clutter.Actor({
            name: 'pods-case-card-layer',
            x_expand: true,
            y_expand: true,
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.END,
            visible: false,
        });
        this._layer.add_constraint(new Layout.MonitorConstraint({primary: true}));
        this.view = new CardView(icons);
        this._layer.add_child(this.view);
        Main.layoutManager.addTopChrome(this._layer);

        this.view.connectButton.connect('clicked', () => this._connect());
        this.view.closeButton.connect('clicked', () => this.hide());
        // The card stays while the pointer is on it.
        this.view.connect('notify::hover', () => {
            if (this._shown)
                this._scheduleAutoHide();
        });

        this._settings.connectObject(`changed::${SETTING}`, () => {
            if (!this._settings.get_boolean(SETTING))
                this.hide();
        }, this);
        global.display.connectObject('in-fullscreen-changed', () => {
            if (this._blocked())
                this.hide();
        }, this);
        for (const name of SHARING_INDICATORS) {
            Main.panel.statusArea[name]?.connectObject('notify::visible', () => {
                if (this._blocked())
                    this.hide();
            }, this);
        }
        St.Settings.get().connectObject('notify::enable-animations',
            () => this._syncFloat(), this);
    }

    destroy() {
        this._stopTimers();
        this._settings.disconnectObject(this);
        global.display.disconnectObject(this);
        for (const name of SHARING_INDICATORS)
            Main.panel.statusArea[name]?.disconnectObject(this);
        St.Settings.get().disconnectObject(this);
        Main.layoutManager.removeChrome(this._layer);
        this._layer.destroy();
        this._layer = null;
        this.view = null;
    }

    // The engine's CaseOpened: the first one of an opening shows the card,
    // the next ones (every 2 s at most while the lid stays open) update it.
    caseOpened(path, snapshot) {
        const opening = !this._lids.has(path);
        this._lids.add(path);
        if (this._shown && this._path === path) {
            this._snapshot = snapshot;
            this._update();
            return;
        }
        // Closed by hand, gone by itself, or never shown for this opening:
        // it waits for the next one.
        if (!opening || !this._settings.get_boolean(SETTING) || this._blocked())
            return;

        this._path = path;
        this._snapshot = snapshot;
        this._state = 'disconnected';
        this._stopTimers();
        this._update();
        this._show();
    }

    caseClosed(path) {
        this._lids.delete(path);
        if (this._path === path)
            this.hide();
    }

    // The engine came or went, or a property of the active device changed.
    sync() {
        if (!this._engine.present) {
            this._lids.clear();
            this.hide();
            return;
        }
        if (this._shown)
            this._update();
    }

    hide() {
        this._stopTimers();
        if (!this._shown)
            return;
        this._shown = false;
        this._path = null;
        const view = this.view;
        view.remove_transition('translation-y');
        view.remove_transition('opacity');
        view.ease({
            opacity: 0,
            duration: FADE_OUT_TIME,
            mode: Clutter.AnimationMode.EASE_IN_QUAD,
        });
        view.ease({
            translation_y: RISE * this._scale(),
            duration: HIDE_TIME,
            mode: Clutter.AnimationMode.EASE_IN_CUBIC,
            onStopped: finished => {
                if (!finished || this._shown || !this._layer)
                    return;
                this._layer.hide();
                this._syncFloat();
                view.spinner.stop();
            },
        });
    }

    _scale() {
        return St.ThemeContext.get_for_stage(global.stage).scale_factor;
    }

    // Fullscreen on the card's monitor, or the screen shared or recorded.
    _blocked() {
        if (Main.layoutManager.primaryMonitor?.inFullscreen)
            return true;
        return SHARING_INDICATORS.some(name => Main.panel.statusArea[name]?.visible);
    }

    _show() {
        const view = this.view;
        const appearing = !this._layer.visible;
        this._shown = true;
        this._layer.show();
        Main.layoutManager.uiGroup.set_child_above_sibling(this._layer, null);
        view.remove_transition('translation-y');
        view.remove_transition('opacity');
        if (appearing) {
            view.opacity = 0;
            view.translation_y = RISE * this._scale();
        }
        view.ease({
            opacity: 255,
            duration: FADE_IN_TIME,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
        });
        view.ease({
            translation_y: 0,
            duration: SHOW_TIME,
            mode: Clutter.AnimationMode.EASE_OUT_CUBIC,
        });
        view.get_transition('translation-y')?.set_cubic_bezier_progress?.(...OVERSHOOT);
        this._syncFloat();
    }

    // The case floats while the card is up, unless animations are off.
    _syncFloat() {
        const art = this.view?.art;
        if (!art)
            return;
        const floating = this._shown && St.Settings.get().enable_animations;
        if (floating && !art.get_transition('translation-y')) {
            art.translation_y = 0;
            art.ease({
                translation_y: -FLOAT_HEIGHT * this._scale(),
                duration: FLOAT_TIME,
                mode: Clutter.AnimationMode.EASE_IN_OUT_SINE,
                repeatCount: -1,
                autoReverse: true,
            });
        } else if (!floating) {
            art.remove_transition('translation-y');
            art.translation_y = 0;
        }
    }

    // What the card shows: the snapshot of the lid, with the live values of
    // the engine once these AirPods are the connected ones.
    _update() {
        const snapshot = this._snapshot ?? {};
        const device = this._engine.device;
        const live = device && device.path === this._path && device.connected ? device : null;
        const connected = !!(live || snapshot.connected);

        if (connected && this._state !== 'connected') {
            this._state = 'connected';
            this._stopConnectTimeout();
            this._scheduleAutoHide();
        } else if (!connected && this._state === 'connected') {
            this._state = 'disconnected';
            this._stopAutoHide();
        }

        const battery = {...snapshotBattery(snapshot), ...live?.battery};
        const view = this.view;
        view.art.gicon = this._icons.art(caseLight(battery) === 'amber'
            ? 'pods-case-open-amber' : 'pods-case-open');
        const name = live?.name || snapshot.name || 'AirPods';
        view.title.text = name;
        view.accessible_name = name;
        view.status.text = cardStatus(this._state);
        view.check.visible = this._state === 'connected';
        if (this._state === 'connected')
            view.statusBox.add_style_class_name('pods-connected');
        else
            view.statusBox.remove_style_class_name('pods-connected');
        view.setBatteries(cardBatteries(battery));

        const connecting = this._state === 'connecting';
        view.connectButton.visible = this._state === 'disconnected' || this._state === 'failed';
        view.connecting.visible = connecting;
        if (connecting)
            view.spinner.play();
        else
            view.spinner.stop();
    }

    _connect() {
        if (!this._path || this._state === 'connecting' || this._state === 'connected')
            return;
        const path = this._path;
        this._state = 'connecting';
        this._update();
        this._connectTimeoutId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT,
            CONNECT_TIMEOUT_SECONDS, () => {
                this._connectTimeoutId = 0;
                this._failed();
                return GLib.SOURCE_REMOVE;
            });
        this._engine.connectDevice(path).catch(e => {
            console.warn(`pods: Connect failed: ${errorMessage(e)}`);
            if (this._path === path)
                this._failed();
        });
    }

    _failed() {
        if (this._state !== 'connecting')
            return;
        this._stopConnectTimeout();
        this._state = 'failed';
        if (this._shown)
            this._update();
    }

    // 6 s after "Connected", not counting the time the pointer rests on the
    // card.
    _scheduleAutoHide() {
        this._stopAutoHide();
        if (this._state !== 'connected' || this.view.hover)
            return;
        this._autoHideId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, AUTO_HIDE_SECONDS, () => {
            this._autoHideId = 0;
            this.hide();
            return GLib.SOURCE_REMOVE;
        });
    }

    _stopAutoHide() {
        if (this._autoHideId) {
            GLib.source_remove(this._autoHideId);
            this._autoHideId = 0;
        }
    }

    _stopConnectTimeout() {
        if (this._connectTimeoutId) {
            GLib.source_remove(this._connectTimeoutId);
            this._connectTimeoutId = 0;
        }
    }

    _stopTimers() {
        this._stopAutoHide();
        this._stopConnectTimeout();
    }
}
