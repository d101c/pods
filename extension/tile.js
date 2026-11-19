// SPDX-License-Identifier: GPL-2.0-or-later
// The AirPods tile of Quick Settings and its menu. The tile is shown only
// while the engine reports connected AirPods; nothing goes in the top bar.
//
// Tile: blue while a noise mode is on, grey in Off. A click does what a press
// and hold on the stem does (CycleNoiseMode); the arrow opens the menu:
// header, battery rings, noise control, Conversation Awareness (models that
// have it), Spatial Audio (Off, Fixed, and Head Tracked on models with motion
// sensors, with Recenter), AirPods Settings.

import Atk from 'gi://Atk';
import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Pango from 'gi://Pango';
import St from 'gi://St';

import {gettext as _} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import * as QuickSettings from 'resource:///org/gnome/shell/ui/quickSettings.js';

import {errorMessage} from './engine.js';
import {
    PARTS, batteryItems, isNoiseActive, lowEarbud, noiseIconName, noiseLabel,
    shortModelName, spatialHint, spatialIconName, spatialLabel, spatialModes, statusLine,
    tileSubtitle,
} from './model.js';
import {showRecenterOsd} from './osd.js';
import {BatteryRing} from './rings.js';

const TILE_ICON = 'pods-earbuds-symbolic';
// How long "Done" and "Centered on your screen" stay after a recenter.
const RECENTERED_MS = 1600;

// A switch with a line of explanation under its name. Unlike the Shell's own
// switch items, it leaves the menu open: it is one setting among others.
const SwitchItem = GObject.registerClass(
class PodsSwitchItem extends PopupMenu.PopupSwitchMenuItem {
    constructor(title, subtitle) {
        super(title, false, {style_class: 'pods-switch-item'});
        const texts = new St.BoxLayout({
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        const index = this.get_children().indexOf(this.label);
        this.remove_child(this.label);
        texts.add_child(this.label);
        texts.add_child(new St.Label({style_class: 'pods-switch-subtitle', text: subtitle}));
        this.insert_child_at_index(texts, index);
    }

    activate(_event) {
        this.toggle();
    }
});

// One segment of the noise control or of the spatial audio selector: icon
// above, name below.
const SegmentButton = GObject.registerClass(
class PodsSegmentButton extends St.Button {
    constructor(mode, label, icon) {
        super({
            style_class: 'button pods-noise-button',
            can_focus: true,
            x_expand: true,
            accessible_role: Atk.Role.TOGGLE_BUTTON,
            accessible_name: label,
        });
        this.mode = mode;

        const box = new St.BoxLayout({
            style_class: 'pods-noise-button-box',
            orientation: Clutter.Orientation.VERTICAL,
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.CENTER,
        });
        box.add_child(new St.Icon({
            style_class: 'pods-noise-icon',
            gicon: icon,
            x_align: Clutter.ActorAlign.CENTER,
        }));
        const text = new St.Label({
            style_class: 'pods-noise-label',
            text: label,
            x_align: Clutter.ActorAlign.CENTER,
        });
        text.clutter_text.set({
            line_wrap: true,
            line_wrap_mode: Pango.WrapMode.WORD,
            ellipsize: Pango.EllipsizeMode.NONE,
            line_alignment: Pango.Alignment.CENTER,
        });
        box.add_child(text);
        this.set_child(box);
    }
});

export const PodsToggle = GObject.registerClass(
class PodsToggle extends QuickSettings.QuickMenuToggle {
    constructor(extension, engine, icons) {
        super({
            title: 'AirPods',
            gicon: icons.get(TILE_ICON),
            toggleMode: false,
            visible: false,
            menuButtonAccessibleName: _('Battery and noise control'),
        });
        this._extension = extension;
        this._engine = engine;
        this._icons = icons;
        this._noiseModes = '';
        this._pendingMode = null;
        this._lastNoiseMode = null;
        this._spatialModes = '';
        this._pendingSpatial = null;
        this._lastSpatial = null;
        this._recenteredId = 0;
        this._syncing = false;
        this._destroyed = false;

        this.menu.setHeader(icons.get(TILE_ICON), 'AirPods', '');

        this._batteries = new St.BoxLayout({style_class: 'pods-batteries', x_expand: true});
        this._batteries.layout_manager.homogeneous = true;
        this._rings = PARTS.map(part => new BatteryRing(part, icons.get('pods-charging-symbolic')));
        this._rings.forEach(ring => this._batteries.add_child(ring));
        this.menu.box.add_child(this._batteries);

        this._noiseTitle = new St.Label({
            style_class: 'pods-section-title',
            text: _('Noise Control'),
        });
        this.menu.box.add_child(this._noiseTitle);
        this._noiseBox = new St.BoxLayout({
            style_class: 'pods-noise-control',
            x_expand: true,
            accessible_role: Atk.Role.PANEL,
            accessible_name: _('Noise Control'),
        });
        this._noiseBox.layout_manager.homogeneous = true;
        this._noiseButtons = [];
        global.focus_manager.add_group(this._noiseBox);
        this.menu.box.add_child(this._noiseBox);

        this._conversation = new SwitchItem(_('Conversation Awareness'),
            _('Lowers media when you speak'));
        this._conversation.connect('toggled', (_item, state) => {
            if (this._syncing)
                return;
            this._engine.setConversationAwareness(state).catch(e => {
                this._failed('SetConversationAwareness', e);
                this.sync();
            });
        });
        this.menu.addMenuItem(this._conversation);

        this._buildSpatial();

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._settingsItem = this.menu.addAction(_('AirPods Settings'), () => {
            Main.overview.hide();
            Main.panel.closeQuickSettings();
            this._extension.openPreferences();
        });

        this.connect('clicked', () => {
            this._engine.cycleNoiseMode().catch(e => this._failed('CycleNoiseMode', e));
        });
        // Quick Settings puts the menu in an overlay of its own, which does
        // not go away with the tile.
        this.connect('destroy', () => {
            this._destroyed = true;
            if (this._recenteredId)
                GLib.source_remove(this._recenteredId);
            this._recenteredId = 0;
            this.menu.destroy();
        });

        this.sync();
    }

    // Spatial Audio: the selector, then a line of help with Recenter when the
    // head is tracked.
    _buildSpatial() {
        this._spatialTitle = new St.Label({
            style_class: 'pods-section-title pods-spatial-title',
            text: _('Spatial Audio'),
        });
        this.menu.box.add_child(this._spatialTitle);
        this._spatialBox = new St.BoxLayout({
            style_class: 'pods-noise-control pods-spatial-control',
            x_expand: true,
            accessible_role: Atk.Role.PANEL,
            accessible_name: _('Spatial Audio'),
        });
        this._spatialBox.layout_manager.homogeneous = true;
        this._spatialButtons = [];
        global.focus_manager.add_group(this._spatialBox);
        this.menu.box.add_child(this._spatialBox);

        this._spatialHintBox = new St.BoxLayout({style_class: 'pods-spatial-hint', x_expand: true});
        this._spatialHint = new St.Label({
            style_class: 'pods-spatial-hint-label',
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._spatialHintBox.add_child(this._spatialHint);
        this._recenterButton = new St.Button({
            style_class: 'button pods-recenter-button',
            label: _('Recenter'),
            can_focus: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._recenterButton.connect('clicked', () => this._recenter());
        this._spatialHintBox.add_child(this._recenterButton);
        this.menu.box.add_child(this._spatialHintBox);
    }

    sync() {
        // A call may still answer after disable().
        if (this._destroyed)
            return;
        const device = this._engine.device;
        const shown = this._engine.present && device !== null && device.connected;
        this.visible = shown;
        if (!shown) {
            this.menu.close();
            return;
        }

        // A choice in the menu shows at once; any answer of the engine ends it.
        if (device.noiseMode !== this._lastNoiseMode) {
            this._lastNoiseMode = device.noiseMode;
            this._pendingMode = null;
        }

        const title = shortModelName(device);
        this.title = title;
        this.subtitle = tileSubtitle(device);
        this.checked = isNoiseActive(device);
        if (lowEarbud(device))
            this.add_style_class_name('pods-low-battery');
        else
            this.remove_style_class_name('pods-low-battery');
        this.menu.setHeader(this._icons.get(TILE_ICON), device.name || title, statusLine(device));

        const items = batteryItems(device);
        this._rings.forEach((ring, i) => ring.update(items[i]));

        this._syncNoise(device);

        this._conversation.visible = device.capabilities.includes('conversation');
        this._syncing = true;
        this._conversation.state = device.conversationAwareness;
        this._syncing = false;

        this._syncSpatial(device);
    }

    _syncSpatial(device) {
        if (device.spatialMode !== this._lastSpatial) {
            this._lastSpatial = device.spatialMode;
            this._pendingSpatial = null;
        }
        const modes = spatialModes(device);
        const key = modes.join(',');
        if (key !== this._spatialModes) {
            this._spatialModes = key;
            this._spatialButtons.forEach(b => b.destroy());
            this._spatialButtons = modes.map(mode => {
                const button = new SegmentButton(mode, spatialLabel(mode),
                    this._icons.get(spatialIconName(mode)));
                button.connect('clicked', () => this._pickSpatial(mode));
                this._spatialBox.add_child(button);
                return button;
            });
        }
        this._spatialStatus = device.spatialStatus;
        this._showSpatial(this._pendingSpatial ?? device.spatialMode);
    }

    _showSpatial(mode) {
        for (const button of this._spatialButtons)
            button.checked = button.mode === mode;
        const recentered = this._recenteredId !== 0;
        this._spatialHint.text = spatialHint(mode, recentered, this._spatialStatus);
        this._spatialHintBox.visible = this._spatialHint.text !== '';
        this._recenterButton.visible = mode === 'head';
        this._recenterButton.label = recentered ? _('Done') : _('Recenter');
    }

    _pickSpatial(mode) {
        this._pendingSpatial = mode;
        this._showSpatial(mode);
        this._engine.setSpatialMode(mode).catch(e => {
            this._pendingSpatial = null;
            this._failed('SetSpatialMode', e);
            this.sync();
        });
    }

    _recenter() {
        this._engine.recenter().then(() => {
            if (this._destroyed)
                return;
            showRecenterOsd(this._icons);
            if (this._recenteredId)
                GLib.source_remove(this._recenteredId);
            this._recenteredId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, RECENTERED_MS, () => {
                this._recenteredId = 0;
                this.sync();
                return GLib.SOURCE_REMOVE;
            });
            this.sync();
        }).catch(e => this._failed('Recenter', e));
    }

    _syncNoise(device) {
        const modes = device.noiseModes;
        const key = modes.join(',');
        if (key !== this._noiseModes) {
            this._noiseModes = key;
            this._noiseButtons.forEach(b => b.destroy());
            this._noiseButtons = modes.map(mode => {
                const button = new SegmentButton(mode, noiseLabel(mode),
                    this._icons.get(noiseIconName(mode)));
                button.connect('clicked', () => this._pick(mode));
                this._noiseBox.add_child(button);
                return button;
            });
        }
        this._noiseTitle.visible = this._noiseBox.visible = modes.length > 0;
        if (modes.length > 3)
            this._noiseBox.add_style_class_name('pods-noise-compact');
        else
            this._noiseBox.remove_style_class_name('pods-noise-compact');

        const current = this._pendingMode ?? device.noiseMode;
        for (const button of this._noiseButtons)
            button.checked = button.mode === current;
    }

    // Shown at once; the engine's NoiseMode confirms it a moment later, as
    // the AirPods answer in about half a second.
    _pick(mode) {
        this._pendingMode = mode;
        for (const button of this._noiseButtons)
            button.checked = button.mode === mode;
        this._engine.setNoiseMode(mode).catch(e => {
            this._pendingMode = null;
            this._failed('SetNoiseMode', e);
            this.sync();
        });
    }

    _failed(what, error) {
        console.warn(`pods: ${what} failed: ${errorMessage(error)}`);
    }
});

export const PodsIndicator = GObject.registerClass(
class PodsIndicator extends QuickSettings.SystemIndicator {
    constructor(extension, engine, icons) {
        super();
        // No _addIndicator(): the top bar stays as it is.
        this.toggle = new PodsToggle(extension, engine, icons);
        this.quickSettingsItems.push(this.toggle);
    }

    destroy() {
        this.quickSettingsItems.forEach(item => item.destroy());
        this.quickSettingsItems = [];
        super.destroy();
    }
});
