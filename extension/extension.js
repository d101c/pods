// SPDX-License-Identifier: GPL-2.0-or-later
// AirPods in GNOME: a Quick Settings tile with the battery of each part, the
// noise control and spatial audio, an on-screen display when the mode
// changes, low battery notifications, and a card at the bottom of the screen
// when the case opens. Everything comes from the engine (podsd) over D-Bus;
// without the engine, or without connected AirPods, nothing shows, except one
// notification, once, when the engine is not installed at all.

import {Extension, gettext as _} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

import {CaseCard} from './caseCard.js';
import {Engine} from './engine.js';
import {Icons} from './icons.js';
import {setGettext} from './model.js';
import {Notifier} from './notify.js';
import {showNoiseOsd} from './osd.js';
import {PodsIndicator} from './tile.js';

export default class PodsExtension extends Extension {
    enable() {
        setGettext(_);
        this._settings = this.getSettings();
        this._icons = new Icons(this.dir);
        this._notifier = new Notifier(this._icons.get('pods-earbuds-symbolic'));
        this._engine = new Engine({
            onChanged: () => {
                this._indicator?.toggle.sync();
                this._card?.sync();
                this._syncInstallOffer();
            },
            onNoiseModeChanged: (mode, source) => showNoiseOsd(this._icons, mode, source),
            onLowBattery: (part, percent) => this._notifier.lowBattery(part, percent),
            onCaseOpened: (path, snapshot) => this._card?.caseOpened(path, snapshot),
            onCaseClosed: path => this._card?.caseClosed(path),
        });
        this._indicator = new PodsIndicator(this, this._engine, this._icons);
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);
        this._card = new CaseCard(this._engine, this._settings, this._icons);
        this._engine.start();
    }

    // One notification, the first time the engine turns out not to be
    // installed; the settings window keeps offering it afterwards.
    _syncInstallOffer() {
        if (this._engine.present) {
            this._notifier.withdrawInstall();
        } else if (this._engine.missing && !this._settings.get_boolean('engine-install-offered')) {
            this._settings.set_boolean('engine-install-offered', true);
            this._notifier.offerInstall(() => {
                Main.overview.hide();
                Main.panel.closeQuickSettings();
                this.openPreferences();
            });
        }
    }

    disable() {
        this._engine?.stop();
        this._engine = null;
        this._card?.destroy();
        this._card = null;
        this._indicator?.destroy();
        this._indicator = null;
        this._notifier?.destroy();
        this._notifier = null;
        this._icons = null;
        this._settings = null;
    }
}
