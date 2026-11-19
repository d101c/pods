// SPDX-License-Identifier: GPL-2.0-or-later
// The Shell's own on-screen display, the one of the volume keys, when the
// noise mode changes from the stem or from the tile, and after a recenter.

import {gettext as _} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

import {noiseIconName, osdLabel, spatialIconName} from './model.js';

// Changes that someone just made by hand. "other" (the settings window,
// another program, the engine putting things back) shows nothing.
const SHOWN_SOURCES = new Set(['stem', 'tile']);

export function showNoiseOsd(icons, mode, source) {
    if (!SHOWN_SOURCES.has(source) || !mode)
        return false;
    Main.osdWindowManager.showAll(icons.get(noiseIconName(mode)), osdLabel(mode));
    return true;
}

// After Recenter in the menu: the sound is in front of the screen again.
export function showRecenterOsd(icons) {
    Main.osdWindowManager.showAll(icons.get(spatialIconName('head')), _('Recentered'));
}
