// SPDX-License-Identifier: GPL-2.0-or-later
// The extension's notifications, from one "AirPods" source:
// - low battery: the engine sends LowBattery once per threshold (10 %, then
//   5 %); a newer one for the same part replaces the older;
// - "Install the AirPods engine", once, when the engine is not installed.

import Gio from 'gi://Gio';

import {gettext as _} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as MessageTray from 'resource:///org/gnome/shell/ui/messageTray.js';

import {lowBatteryTexts} from './model.js';

const CRITICAL_PERCENT = 5;

export class Notifier {
    constructor(sourceIcon) {
        this._sourceIcon = sourceIcon;
        this._source = null;
        this._notifications = new Map();
        this._install = null;
    }

    // The engine is missing. A click (or Install…) opens the settings, where
    // Install Engine does the rest; nothing is downloaded from here.
    offerInstall(openSettings) {
        if (this._install)
            return;
        const source = this._ensureSource();
        const notification = new MessageTray.Notification({
            source,
            title: _('Install the AirPods engine'),
            body: _('Pods needs its engine, a small background service, to talk to your AirPods. Install it from AirPods Settings in one click, without an administrator password.'),
            gicon: this._sourceIcon,
            urgency: MessageTray.Urgency.NORMAL,
            resident: false,
        });
        notification.addAction(_('Install…'), () => openSettings());
        notification.connect('activated', () => openSettings());
        notification.connect('destroy', () => {
            if (this._install === notification)
                this._install = null;
        });
        this._install = notification;
        source.addNotification(notification);
    }

    // The engine is there now: the offer has nothing left to do.
    withdrawInstall() {
        this._install?.destroy(MessageTray.NotificationDestroyedReason.SOURCE_CLOSED);
        this._install = null;
    }

    lowBattery(part, percent) {
        const {title, body} = lowBatteryTexts(part, percent);
        // First: a source left without notifications destroys itself.
        this._notifications.get(part)?.destroy(MessageTray.NotificationDestroyedReason.REPLACED);
        const source = this._ensureSource();

        const notification = new MessageTray.Notification({
            source,
            title,
            body,
            gicon: new Gio.ThemedIcon({
                name: percent <= CRITICAL_PERCENT ? 'battery-caution-symbolic' : 'battery-level-10-symbolic',
            }),
            urgency: percent <= CRITICAL_PERCENT
                ? MessageTray.Urgency.HIGH : MessageTray.Urgency.NORMAL,
        });
        notification.connect('destroy', () => {
            if (this._notifications.get(part) === notification)
                this._notifications.delete(part);
        });
        this._notifications.set(part, notification);
        source.addNotification(notification);
    }

    destroy() {
        // Takes its notifications with it.
        this._source?.destroy();
        this._source = null;
        this._notifications.clear();
        this._install = null;
    }

    _ensureSource() {
        if (!this._source) {
            this._source = new MessageTray.Source({
                title: _('AirPods'),
                icon: this._sourceIcon,
            });
            this._source.connect('destroy', () => {
                this._source = null;
                this._notifications.clear();
                this._install = null;
            });
            Main.messageTray.add(this._source);
        }
        return this._source;
    }
}
