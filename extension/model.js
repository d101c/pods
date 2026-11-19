// SPDX-License-Identifier: GPL-2.0-or-later
// What the tile, its menu and the settings window say about the AirPods,
// worked out from a device snapshot (engine.js). No Shell or GTK import: the
// Shell, prefs.js and tools/test-model.js all use it.

let _ = s => s;

// The Shell and the preferences each have their own gettext.
export function setGettext(gettext) {
    _ = gettext;
}

// "%s at %d%%" with its arguments, without String.prototype.format (which the
// Shell adds but plain gjs does not have).
export function fmt(template, ...args) {
    let i = 0;
    return template.replace(/%[sd%]/g, m => (m === '%%' ? '%' : String(args[i++])));
}

// Ring in coral under this level (the mockups: "< 20").
export const LOW_RING_PERCENT = 20;
// The case card shows one level for both earbuds when they are this close.
export const COMBINE_EARBUDS_POINTS = 10;
// The tile subtitle warns at or under this level, the first threshold of the
// engine's LowBattery signal.
export const LOW_WARNING_PERCENT = 10;

export const PARTS = ['left', 'right', 'case'];

export function noiseLabel(mode) {
    switch (mode) {
    case 'off':
        return _('Off');
    case 'transparency':
        return _('Transparency');
    case 'adaptive':
        return _('Adaptive');
    case 'anc':
        return _('Noise Cancellation');
    default:
        return mode;
    }
}

// The tile's subtitle has room for about thirteen characters next to the menu
// arrow: "Noise Cancellation" would end in an ellipsis there.
export function noiseShortLabel(mode) {
    return mode === 'anc' ? _('Noise Cancelling') : noiseLabel(mode);
}

// The on-screen display spells Off out: "Off" alone says nothing there.
export function osdLabel(mode) {
    return mode === 'off' ? _('Noise Control Off') : noiseLabel(mode);
}

export function noiseIconName(mode) {
    return `pods-noise-${mode}-symbolic`;
}

export function partLabel(part) {
    switch (part) {
    case 'left':
        return _('Left');
    case 'right':
        return _('Right');
    default:
        return _('Case');
    }
}

// "AirPods Pro (1st generation)" -> "AirPods Pro": a tile with a menu arrow
// has room for about eleven characters.
export function shortModelName(device) {
    const model = (device.modelName ?? '').replace(/\s*\([^)]*\)\s*$/, '').trim();
    return model || device.name || 'AirPods';
}

export function isNoiseActive(device) {
    return !!device.noiseMode && device.noiseMode !== 'off';
}

// One entry per ring: left, right, case. A part the engine does not know (an
// earbud in the closed case, say) has known = false and percent = null.
export function batteryItems(device) {
    return PARTS.map(part => {
        const level = device.battery[part];
        const known = level !== undefined;
        const percent = known ? level.percent : null;
        const charging = known && level.charging;
        return {
            part,
            label: partLabel(part),
            glyph: part === 'left' ? 'L' : part === 'right' ? 'R' : '',
            known,
            percent,
            charging,
            low: known && !charging && percent < LOW_RING_PERCENT,
            text: known ? fmt(_('%d%%'), percent) : '—',
        };
    });
}

// The line under the name in the menu header.
export function statusLine(device) {
    const [left, right] = device.wearing;
    if (device.mediaPaused && !(left === 'case' && right === 'case')) {
        // The engine holds what was playing since a bud came out.
        if (left === 'ear' && right !== 'ear')
            return _('Left in ear · Paused');
        if (right === 'ear' && left !== 'ear')
            return _('Right in ear · Paused');
        if (left !== 'ear' && right !== 'ear')
            return _('Not in ears · Paused');
    }
    if (left === 'ear' && right === 'ear')
        return _('Connected · In both ears');
    if (left === 'ear')
        return _('Connected · Left in ear');
    if (right === 'ear')
        return _('Connected · Right in ear');
    if (left === 'case' && right === 'case') {
        const charging = device.battery.left?.charging || device.battery.right?.charging;
        return charging ? _('In case · Charging') : _('In case');
    }
    if (left === 'unknown' && right === 'unknown')
        return _('Connected');
    return _('Connected · Not in ears');
}

// The earbud that runs low, if any: the lowest of those not charging.
export function lowEarbud(device) {
    return ['left', 'right']
        .map(part => ({part, level: device.battery[part]}))
        .filter(({level}) => level && !level.charging && level.percent <= LOW_WARNING_PERCENT)
        .sort((a, b) => a.level.percent - b.level.percent)[0] ?? null;
}

// The tile's subtitle: the noise mode, unless something matters more.
export function tileSubtitle(device) {
    const low = lowEarbud(device);
    if (low)
        return fmt(_('%s %d%% · Low Battery'), partLabel(low.part), low.level.percent);

    if (device.spatialStatus === 'paused')
        return _('Spatial Off · Sharing');

    const mode = noiseShortLabel(device.noiseMode);
    const [left, right] = device.wearing;
    if (left === 'ear' && right !== 'ear')
        return fmt(_('Left Only · %s'), mode);
    if (right === 'ear' && left !== 'ear')
        return fmt(_('Right Only · %s'), mode);
    return mode;
}

// "Left 82%   Right 79%   Case 64%" for the settings window.
export function batterySummary(device) {
    return batteryItems(device)
        .filter(item => item.known)
        .map(item => `${item.label} ${item.text}`)
        .join('    ');
}

// Notification for the engine's LowBattery signal.
export function lowBatteryTexts(part, percent) {
    if (part === 'case') {
        return {
            title: fmt(_('AirPods case at %d%%'), percent),
            body: _('Connect the case to a charger.'),
        };
    }
    return {
        title: part === 'left'
            ? fmt(_('Left AirPod at %d%%'), percent)
            : fmt(_('Right AirPod at %d%%'), percent),
        body: _('Put it back in the case to charge it.'),
    };
}

// The case card ---------------------------------------------------------------

// Battery levels from a CaseOpened snapshot, in the shape of a device's
// battery (part -> {percent, charging}). A part the snapshot leaves out is
// absent.
export function snapshotBattery(snapshot) {
    const battery = {};
    for (const part of PARTS) {
        const percent = snapshot[part];
        if (typeof percent === 'number')
            battery[part] = {percent, charging: !!snapshot[`${part}Charging`]};
    }
    return battery;
}

// The levels on the card, left to right: both earbuds as one when they are
// less than ten points apart (the lower level: that bud runs out first), each
// earbud otherwise, then the case. A part with no known level is left out.
export function cardBatteries(battery) {
    const entry = (kind, label, {percent, charging}) => ({
        kind,
        label,
        percent,
        charging,
        low: !charging && percent < LOW_RING_PERCENT,
        text: fmt(_('%d%%'), percent),
    });
    const items = [];
    const {left, right} = battery;
    if (left && right && Math.abs(left.percent - right.percent) < COMBINE_EARBUDS_POINTS) {
        items.push(entry('buds', _('AirPods'), {
            percent: Math.min(left.percent, right.percent),
            charging: left.charging && right.charging,
        }));
    } else {
        if (left)
            items.push(entry('left', partLabel('left'), left));
        if (right)
            items.push(entry('right', partLabel('right'), right));
    }
    if (battery.case)
        items.push(entry('case', partLabel('case'), battery.case));
    return items;
}

// The line under the name on the card, for each step of the connection.
export function cardStatus(state) {
    switch (state) {
    case 'connected':
        return _('Connected');
    case 'connecting':
        return _('Connecting to this computer');
    case 'failed':
        return _('Could not connect');
    default:
        return _('Not connected to this computer');
    }
}

// The status light of the case drawing: amber when the case runs low.
export function caseLight(battery) {
    const level = battery.case;
    return level && !level.charging && level.percent < LOW_RING_PERCENT ? 'amber' : 'green';
}

// Spatial audio -----------------------------------------------------------------

// The modes offered: Head Tracked only on AirPods with motion sensors.
export function spatialModes(device) {
    return device.capabilities?.includes('spatial.head')
        ? ['off', 'fixed', 'head'] : ['off', 'fixed'];
}

export function spatialLabel(mode) {
    switch (mode) {
    case 'off':
        return _('Off');
    case 'fixed':
        return _('Fixed');
    case 'head':
        return _('Head Tracked');
    default:
        return mode;
    }
}

export function spatialIconName(mode) {
    return `pods-spatial-${mode}-symbolic`;
}

// The line under the selector of the menu ('' in Off: no line).
export function spatialHint(mode, recentered = false, status = 'off') {
    if (mode !== 'off' && status === 'paused')
        return _('Paused while the audio is shared');
    switch (mode) {
    case 'fixed':
        return _('Music sits in front of you');
    case 'head':
        return recentered ? _('Centered on your screen') : _('Sound stays with your screen');
    default:
        return '';
    }
}

// The line under "Mode" in the settings window.
export function spatialHelp(mode) {
    switch (mode) {
    case 'fixed':
        return _('Music in front of you, it turns with your head');
    case 'head':
        return _('Music stays with your screen as you turn');
    default:
        return _('Regular stereo');
    }
}

// The angle the head view turns the head by: degrees clockwise seen from
// above, the screen at the top. The contract counts yaw positive to the left.
export function headAngle(yaw) {
    return -yaw;
}

// "+24°" (turned right), "−24°" (left), "0°": the head view's angle.
export function yawText(yaw) {
    const degrees = Math.round(headAngle(yaw));
    if (degrees === 0)
        return '0°';
    return `${degrees > 0 ? '+' : '−'}${Math.abs(degrees)}°`;
}

// What the light under the head view says. live: head poses are arriving.
// Returns {state, text}; state is 'live', 'recentered', 'waiting' or 'off'.
export function liveStatus(device, {live = false, yaw = 0, recentered = false} = {}) {
    if (recentered)
        return {state: 'recentered', text: _('Recentered')};
    if (live || device.headTracking === 'active')
        return {state: 'live', text: fmt(_('Live · %s'), yawText(yaw))};
    if (device.spatialMode === 'head') {
        if (device.headTracking === 'starting')
            return {state: 'waiting', text: _('Starting…')};
        if (device.headTracking === 'idle')
            return {state: 'waiting', text: _('Starts when sound plays')};
    }
    return {state: 'off', text: _('Not tracking')};
}

// Where the two virtual speakers stand, in degrees clockwise from the screen:
// they stay with the screen when the head is tracked, and turn with the head
// (angle: headAngle()) otherwise. None in Off.
export function speakerAngles(mode, angle) {
    if (mode === 'off')
        return [];
    const turn = mode === 'head' ? 0 : angle;
    return [{name: _('Left'), angle: -30 + turn}, {name: _('Right'), angle: 30 + turn}];
}

// The engine install -------------------------------------------------------------

export function formatSize(bytes) {
    if (bytes >= 1e6)
        return fmt(_('%s MB'), (bytes / 1e6).toFixed(1));
    return fmt(_('%s kB'), Math.max(1, Math.round(bytes / 1e3)));
}
