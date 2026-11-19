// SPDX-License-Identifier: GPL-2.0-or-later
// AirPods Settings, Spatial Audio page (Spatial.dc.html): a live top view of
// the head and the two virtual speakers, the mode (Off, Fixed, and Head
// Tracked on AirPods with motion sensors), Follow Screen, Recenter, the head
// profile and a test sound.
//
// The head moves with the engine's HeadPose signal. The page asks for it
// (WatchHeadPose(true)) only while it is on screen and the AirPods have the
// sensors, and gives it up (WatchHeadPose(false)) as soon as it is not: another
// page, the window closed or hidden.

import Adw from 'gi://Adw';
import Gdk from 'gi://Gdk';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Gtk from 'gi://Gtk';

import {gettext as _} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

import {errorMessage} from './engine.js';
import {HeadView} from './headView.js';
import {headAngle, liveStatus, spatialHelp, spatialLabel, spatialModes} from './model.js';
import * as TestSound from './testSound.js';

// Poses older than this: the sensors stopped.
const LIVE_MS = 700;
// How long "Recentered" stays under the view.
const RECENTERED_MS = 1500;

const DOT_COLORS = {
    live: ['#57e389', '#26a269'],
    recentered: ['#f6d32d', '#c88800'],
};

// The light of the status line under the view.
const StatusDot = GObject.registerClass(
class PodsStatusDot extends Gtk.DrawingArea {
    constructor() {
        super({content_width: 8, content_height: 8, valign: Gtk.Align.CENTER});
        this._state = 'off';
        this.set_draw_func((_area, cr, width, height) => {
            const colors = DOT_COLORS[this._state];
            if (colors) {
                const rgba = new Gdk.RGBA();
                rgba.parse(colors[Adw.StyleManager.get_default().dark ? 0 : 1]);
                cr.setSourceRGBA(rgba.red, rgba.green, rgba.blue, 1);
            } else {
                const fg = this.get_color();
                cr.setSourceRGBA(fg.red, fg.green, fg.blue, this._state === 'waiting' ? 0.6 : 0.35);
            }
            cr.arc(width / 2, height / 2, Math.min(width, height) / 2, 0, 2 * Math.PI);
            cr.fill();
            cr.$dispose();
        });
    }

    set state(state) {
        if (state !== this._state) {
            this._state = state;
            this.queue_draw();
        }
    }
});

export const SpatialPage = GObject.registerClass(
class PodsSpatialPage extends Adw.PreferencesPage {
    // banner: the "engine not installed" banner of this page; placeholder:
    // the group shown instead of the settings without engine or AirPods.
    constructor(window, engine, banner, placeholder) {
        super({
            title: _('Spatial Audio'),
            icon_name: 'pods-spatial-fixed-symbolic',
            name: 'spatial',
        });
        this._window = window;
        this._engine = engine;
        this._syncing = false;
        this._modesKey = '';
        this._pending = null;
        this._lastMode = null;
        this._yaw = 0;
        this._live = false;
        this._liveId = 0;
        this._recenteredId = 0;
        this._sound = null;

        this.add(banner);
        this._placeholder = placeholder;
        this.add(placeholder);
        this._buildView();
        this._buildSpatial();
        this._buildSound();
        this._footer = new Adw.PreferencesGroup({
            description: _('Head tracking uses the motion sensors in your AirPods. Their data stays on this computer.'),
        });
        this.add(this._footer);

        // On screen or not: another page, the window closed or minimised.
        this.connect('map', () => this._syncWatch());
        this.connect('unmap', () => this._syncWatch());
    }

    stop() {
        for (const id of [this._liveId, this._recenteredId]) {
            if (id)
                GLib.source_remove(id);
        }
        this._liveId = this._recenteredId = 0;
        this._sound?.stop();
        this._engine = null;
    }

    _buildView() {
        this._viewGroup = new Adw.PreferencesGroup();
        const card = new Gtk.Box({
            orientation: Gtk.Orientation.VERTICAL,
            css_classes: ['card'],
            overflow: Gtk.Overflow.HIDDEN,
        });
        this._view = new HeadView();
        card.append(this._view);
        card.append(new Gtk.Separator());
        const status = new Gtk.Box({
            spacing: 8,
            margin_top: 12,
            margin_bottom: 12,
            margin_start: 18,
            margin_end: 18,
        });
        this._dot = new StatusDot();
        status.append(this._dot);
        this._statusLabel = new Gtk.Label({
            xalign: 0,
            css_classes: ['heading', 'numeric'],
        });
        status.append(this._statusLabel);
        card.append(status);
        this._viewGroup.add(card);
        this.add(this._viewGroup);
    }

    _buildSpatial() {
        this._spatial = new Adw.PreferencesGroup({title: _('Spatial Audio')});

        this._modeRow = new Adw.ActionRow({title: _('Mode')});
        this._modes = new Adw.ToggleGroup({valign: Gtk.Align.CENTER});
        this._modes.connect('notify::active-name', () => {
            if (!this._syncing && this._modes.active_name)
                this._pick(this._modes.active_name);
        });
        this._modeRow.add_suffix(this._modes);
        this._spatial.add(this._modeRow);

        this._followRow = new Adw.SwitchRow({
            title: _('Follow Screen'),
            subtitle: _('Slowly recenters when you stay turned away'),
        });
        this._followRow.connect('notify::active', () => {
            if (!this._syncing)
                this._run(this._engine.setFollowScreen(this._followRow.active));
        });
        this._spatial.add(this._followRow);

        this._recenterRow = new Adw.ActionRow({
            title: _('Recenter'),
            subtitle: _('Face your screen, then press'),
        });
        this._recenterButton = new Gtk.Button({label: _('Recenter'), valign: Gtk.Align.CENTER});
        this._recenterButton.connect('clicked', () => this._recenter());
        this._recenterRow.add_suffix(this._recenterButton);
        this._recenterRow.activatable_widget = this._recenterButton;
        this._spatial.add(this._recenterRow);
        this.add(this._spatial);
    }

    _buildSound() {
        this._soundGroup = new Adw.PreferencesGroup({title: _('Sound')});
        // One profile for now: the engine renders with the KEMAR dummy head.
        const profile = new Adw.ActionRow({
            title: _('Head Profile'),
            subtitle: _('How sound reaches your ears'),
        });
        profile.add_suffix(new Gtk.Label({label: _('Generic (KEMAR)'), css_classes: ['dim-label']}));
        this._soundGroup.add(profile);

        this._testRow = new Adw.ActionRow({
            title: _('Test Sound'),
            subtitle: _('Plays a short sound from each virtual speaker'),
            visible: false,
        });
        this._playButton = new Gtk.Button({label: _('Play'), valign: Gtk.Align.CENTER});
        this._playButton.connect('clicked', () => this._play());
        this._testRow.add_suffix(this._playButton);
        this._testRow.activatable_widget = this._playButton;
        this._soundGroup.add(this._testRow);
        TestSound.available().then(ok => {
            this._testRow.visible = ok;
        });
        this.add(this._soundGroup);
    }

    // The engine's state changed.
    sync() {
        const engine = this._engine;
        if (!engine)
            return;
        const device = engine.device;
        const connected = engine.present && device?.connected;
        this._placeholder.sync(engine);
        this._placeholder.visible = !connected;
        for (const group of [this._viewGroup, this._spatial, this._soundGroup, this._footer])
            group.visible = !!connected;
        this._syncWatch();
        if (!connected)
            return;

        if (device.spatialMode !== this._lastMode) {
            this._lastMode = device.spatialMode;
            this._pending = null;
        }
        const mode = this._pending ?? device.spatialMode;
        const tracked = device.capabilities.includes('spatial.head');

        this._syncing = true;
        const modes = spatialModes(device);
        const key = modes.join(',');
        if (key !== this._modesKey) {
            this._modesKey = key;
            this._modes.remove_all();
            for (const name of modes)
                this._modes.add(new Adw.Toggle({name, label: spatialLabel(name)}));
        }
        this._modes.active_name = modes.includes(mode) ? mode : 'off';
        this._modeRow.subtitle = spatialHelp(mode);
        this._followRow.visible = this._recenterRow.visible = this._footer.visible = tracked;
        this._setTracked(mode === 'head');
        this._followRow.active = device.followScreen;
        this._syncing = false;

        this._view.setMode(mode);
        if (!this._watching() || (!this._live && device.headTracking !== 'active'))
            this._yaw = 0;
        this._view.setAngle(headAngle(this._yaw));
        this._syncStatus(device);
    }

    // A HeadPose signal: degrees, yaw positive to the left.
    headPose(yaw, _pitch) {
        this._yaw = yaw;
        this._live = true;
        if (this._liveId)
            GLib.source_remove(this._liveId);
        this._liveId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, LIVE_MS, () => {
            this._liveId = 0;
            this._live = false;
            this.sync();
            return GLib.SOURCE_REMOVE;
        });
        this._view.setAngle(headAngle(yaw));
        const device = this._engine?.device;
        if (device)
            this._syncStatus(device);
    }

    _syncStatus(device) {
        const {state, text} = liveStatus(device, {
            live: this._live,
            yaw: this._yaw,
            recentered: this._recenteredId !== 0,
        });
        this._dot.state = state;
        if (this._statusLabel.label !== text)
            this._statusLabel.label = text;
    }

    _watching() {
        const device = this._engine?.device;
        return this.get_mapped() && !!this._engine?.present && !!device?.connected &&
            device.capabilities.includes('spatial.head');
    }

    _syncWatch() {
        const watching = this._watching();
        this._engine?.setHeadPoseWatch(watching);
        if (!watching && this._live) {
            this._live = false;
            this._yaw = 0;
        }
    }

    // Follow Screen and Recenter mean something only when the head is tracked.
    // The button too, not only its row: screen readers look at the button.
    _setTracked(tracked) {
        for (const widget of [this._followRow, this._recenterRow, this._recenterButton])
            widget.sensitive = tracked;
    }

    _pick(mode) {
        this._pending = mode;
        this._modeRow.subtitle = spatialHelp(mode);
        this._view.setMode(mode);
        this._setTracked(mode === 'head');
        this._engine.setSpatialMode(mode).catch(e => {
            this._pending = null;
            this._toast(e);
            this.sync();
        });
    }

    _recenter() {
        this._engine?.recenter().then(() => {
            if (!this._engine)
                return;
            if (this._recenteredId)
                GLib.source_remove(this._recenteredId);
            this._recenteredId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, RECENTERED_MS, () => {
                this._recenteredId = 0;
                this.sync();
                return GLib.SOURCE_REMOVE;
            });
            this.sync();
        }).catch(e => this._toast(e));
    }

    _play() {
        this._sound ??= new TestSound.TestSound();
        this._playButton.sensitive = false;
        this._playButton.label = _('Playing…');
        this._sound.play().catch(e => {
            this._window.add_toast(new Adw.Toast({
                title: _('Could not play the test sound: %s').replace('%s', e.message),
            }));
        }).finally(() => {
            this._playButton.sensitive = true;
            this._playButton.label = _('Play');
        });
    }

    _run(promise) {
        promise.catch(e => {
            this._toast(e);
            this.sync();
        });
    }

    _toast(error) {
        this._window.add_toast(new Adw.Toast({
            title: _('The AirPods did not take the change: %s').replace('%s', errorMessage(error)),
        }));
    }
});
