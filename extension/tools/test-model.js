// SPDX-License-Identifier: GPL-2.0-or-later
// What the tile and the menu say, for each state of the mockups; which engine
// release the installer takes; the test sound.
//
//     gjs -m extension/tools/test-model.js
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

import {EngineInstaller, machine, readRelease, releaseAsset} from '../installer.js';
import {chimeSamples} from '../testSound.js';
import {
    batteryItems, batterySummary, cardBatteries, cardStatus, caseLight, formatSize, liveStatus,
    lowBatteryTexts, noiseLabel, osdLabel, shortModelName, snapshotBattery, spatialHelp, spatialHint,
    spatialIconName, spatialLabel, spatialModes, speakerAngles, statusLine, tileSubtitle, yawText,
} from '../model.js';

let failed = 0;
function expect(label, got, wanted) {
    const [g, w] = [JSON.stringify(got), JSON.stringify(wanted)];
    if (g === w) {
        print(`ok   ${label}`);
    } else {
        print(`FAIL ${label}\n       got:    ${g}\n       wanted: ${w}`);
        failed = 1;
    }
}

function device(changes = {}) {
    return {
        name: 'AirPods Pro de Camille',
        modelName: 'AirPods Pro (1st generation)',
        battery: {
            left: {percent: 82, charging: false},
            right: {percent: 79, charging: false},
            case: {percent: 64, charging: true},
        },
        wearing: ['ear', 'ear'],
        noiseMode: 'anc',
        ...changes,
    };
}

expect('short model name', shortModelName(device()), 'AirPods Pro');
expect('short model name without a model', shortModelName(device({modelName: ''})), 'AirPods Pro de Camille');
expect('short model name without anything', shortModelName(device({modelName: '', name: ''})), 'AirPods');

expect('subtitle: the mode, short', tileSubtitle(device()), 'Noise Cancelling');
expect('selector and settings keep the full name', noiseLabel('anc'), 'Noise Cancellation');
expect('subtitle: Off', tileSubtitle(device({noiseMode: 'off'})), 'Off');
expect('subtitle: spatial paused by sharing', tileSubtitle(device({spatialStatus: 'paused'})), 'Spatial Off · Sharing');
expect('hint: paused while shared', spatialHint('head', false, 'paused'), 'Paused while the audio is shared');
expect('subtitle: one earbud', tileSubtitle(device({noiseMode: 'transparency', wearing: ['case', 'ear']})),
    'Right Only · Transparency');
expect('subtitle: low battery first',
    tileSubtitle(device({battery: {left: {percent: 9, charging: false}, right: {percent: 7, charging: false}}})),
    'Right 7% · Low Battery');
expect('subtitle: low but charging is not low',
    tileSubtitle(device({battery: {left: {percent: 9, charging: true}}})), 'Noise Cancelling');
expect('subtitle: 11 % is not low yet',
    tileSubtitle(device({battery: {left: {percent: 11, charging: false}}})), 'Noise Cancelling');

expect('status: both ears', statusLine(device()), 'Connected · In both ears');
expect('status: left only', statusLine(device({wearing: ['ear', 'out']})), 'Connected · Left in ear');
expect('status: in the case, charging',
    statusLine(device({wearing: ['case', 'case'], battery: {left: {percent: 40, charging: true}}})),
    'In case · Charging');
expect('status: out of the ears', statusLine(device({wearing: ['out', 'out']})), 'Connected · Not in ears');
expect('status: unknown', statusLine(device({wearing: ['unknown', 'unknown']})), 'Connected');
expect('status: paused', statusLine(device({wearing: ['out', 'out'], mediaPaused: true})),
    'Not in ears · Paused');
expect('status: paused, one bud still in', statusLine(device({wearing: ['ear', 'out'], mediaPaused: true})),
    'Left in ear · Paused');
expect('status: paused but both back in the case',
    statusLine(device({wearing: ['case', 'case'], mediaPaused: true})), 'In case');
expect('status: not paused', statusLine(device({wearing: ['out', 'out'], mediaPaused: false})),
    'Connected · Not in ears');

expect('rings', batteryItems(device()).map(i => [i.part, i.text, i.charging, i.low, i.known]), [
    ['left', '82%', false, false, true],
    ['right', '79%', false, false, true],
    ['case', '64%', true, false, true],
]);
expect('rings: unknown and low',
    batteryItems(device({battery: {right: {percent: 12, charging: false}}})).map(i => [i.text, i.low, i.known]),
    [['—', false, false], ['12%', true, true], ['—', false, false]]);
expect('rings: 19 % is coral, 20 % is not',
    [19, 20].map(p => batteryItems(device({battery: {left: {percent: p, charging: false}}}))[0].low),
    [true, false]);

expect('OSD labels', ['off', 'transparency', 'anc', 'adaptive'].map(osdLabel),
    ['Noise Control Off', 'Transparency', 'Noise Cancellation', 'Adaptive']);
expect('notification: earbud', lowBatteryTexts('left', 10),
    {title: 'Left AirPod at 10%', body: 'Put it back in the case to charge it.'});
expect('notification: case', lowBatteryTexts('case', 5).title, 'AirPods case at 5%');
expect('settings summary', batterySummary(device({battery: {left: {percent: 82, charging: false}}})), 'Left 82%');

const snap = {left: 82, right: 79, case: 64, caseCharging: true, leftCharging: false, name: 'x'};
expect('card: snapshot levels', snapshotBattery(snap), {
    left: {percent: 82, charging: false},
    right: {percent: 79, charging: false},
    case: {percent: 64, charging: true},
});
expect('card: snapshot without levels', snapshotBattery({connected: false}), {});
const card = battery => cardBatteries(snapshotBattery(battery)).map(i => [i.kind, i.label, i.text, i.charging, i.low]);
expect('card: earbuds within 10 points are one (the lower level)', card(snap),
    [['buds', 'AirPods', '79%', false, false], ['case', 'Case', '64%', true, false]]);
expect('card: 10 points apart, each earbud', card({left: 90, right: 80, case: 50}),
    [['left', 'Left', '90%', false, false], ['right', 'Right', '80%', false, false],
        ['case', 'Case', '50%', false, false]]);
expect('card: charging in the case', card({left: 40, right: 45, leftCharging: true, rightCharging: true}),
    [['buds', 'AirPods', '40%', true, false]]);
expect('card: one earbud and a low case', card({right: 71, case: 12}),
    [['right', 'Right', '71%', false, false], ['case', 'Case', '12%', false, true]]);
expect('card: nothing known', card({}), []);
expect('card: status lines', ['disconnected', 'connecting', 'connected', 'failed'].map(cardStatus),
    ['Not connected to this computer', 'Connecting to this computer', 'Connected', 'Could not connect']);
expect('card: case light', [
    caseLight({case: {percent: 64, charging: false}}),
    caseLight({case: {percent: 12, charging: false}}),
    caseLight({case: {percent: 12, charging: true}}),
    caseLight({}),
], ['green', 'amber', 'green', 'green']);

// Spatial audio
const pro1 = device({capabilities: ['noise.off', 'noise.anc']});
const pro2 = device({capabilities: ['noise.off', 'noise.anc', 'spatial.head']});
expect('spatial: Off and Fixed without motion sensors', spatialModes(pro1), ['off', 'fixed']);
expect('spatial: Head Tracked with them', spatialModes(pro2), ['off', 'fixed', 'head']);
expect('spatial: labels', ['off', 'fixed', 'head'].map(spatialLabel), ['Off', 'Fixed', 'Head Tracked']);
expect('spatial: icons', spatialIconName('head'), 'pods-spatial-head-symbolic');
expect('spatial: menu hints', ['off', 'fixed', 'head'].map(m => spatialHint(m)),
    ['', 'Music sits in front of you', 'Sound stays with your screen']);
expect('spatial: hint after a recenter', spatialHint('head', true), 'Centered on your screen');
expect('spatial: help in the settings', ['off', 'fixed', 'head'].map(spatialHelp),
    ['Regular stereo', 'Music in front of you, it turns with your head', 'Music stays with your screen as you turn']);
expect('yaw: left is negative on screen (the contract: positive)', [yawText(24), yawText(-24.4), yawText(0.3)],
    ['−24°', '+24°', '0°']);
const live = (changes, opts) => liveStatus(device({spatialMode: 'head', headTracking: 'idle', ...changes}), opts);
expect('live: poses arriving', live({headTracking: 'active'}, {yaw: 24}), {state: 'live', text: 'Live · −24°'});
expect('live: recentered first', live({headTracking: 'active'}, {recentered: true}),
    {state: 'recentered', text: 'Recentered'});
expect('live: head tracked, no sound', live({}), {state: 'waiting', text: 'Starts when sound plays'});
expect('live: starting', live({headTracking: 'starting'}).text, 'Starting…');
expect('live: fixed, sensors idle', live({spatialMode: 'fixed'}).text, 'Not tracking');
expect('live: no sensors', live({headTracking: 'unavailable'}).text, 'Not tracking');
expect('speakers: stay with the screen when tracked', speakerAngles('head', 40).map(s => s.angle), [-30, 30]);
expect('speakers: turn with the head when fixed', speakerAngles('fixed', 40).map(s => s.angle), [10, 70]);
expect('speakers: none in Off', speakerAngles('off', 0), []);
expect('sizes', [formatSize(2_345_678), formatSize(812_300), formatSize(12)], ['2.3 MB', '812 kB', '1 kB']);

// The engine release the installer picks, and the test sound.
const sha = 'ab'.repeat(32);
const release = {
    version: '0.1.0',
    assets: {
        x86_64: {url: 'https://example.org/x.tar.gz', size: 10, sha256: sha},
        aarch64: {url: 'https://example.org/a.tar.gz', size: 0, sha256: ''},
    },
};
expect('release: this machine is one the engine is built for', ['x86_64', 'aarch64'].includes(machine()), true);
expect('release: the archive of the architecture', releaseAsset(release, 'x86_64'),
    {version: '0.1.0', arch: 'x86_64', url: 'https://example.org/x.tar.gz', size: 10, sha256: sha});
expect('release: none without a SHA-256 (not published yet)', releaseAsset(release, 'aarch64'), null);
expect('release: none for another architecture', releaseAsset(release, 'riscv64'), null);
const shipped = releaseAsset(readRelease(Gio.File.new_for_path(GLib.build_filenamev([GLib.path_get_dirname(
    GLib.filename_from_uri(import.meta.url)[0]), '..']))), 'x86_64');
expect('release: the one shipped is complete for x86_64',
    [/^[0-9a-f]{64}$/.test(shipped?.sha256 ?? ''), shipped?.size > 0,
        shipped?.url?.startsWith('https://github.com/d101c/pods/releases/download/engine-')], [true, true, true]);
const refusal = await new EngineInstaller({...releaseAsset(release, 'x86_64'), url: 'http://example.org/x.tar.gz'})
    .install().then(() => null, e => [e.code, e.message]);
expect('install: plain HTTP only to this computer', refusal, ['download', 'Not a secure address: http://example.org/x.tar.gz']);

const samples = chimeSamples(8000);
const half = samples.length / 2;
const energy = (from, to, channel) => {
    let sum = 0;
    for (let i = from; i < to; i++)
        sum += samples[2 * i + channel] ** 2;
    return sum;
};
expect('test sound: stereo, 1.06 s', samples.length, 2 * Math.round(0.42 * 8000) * 2 + 2 * Math.round(0.22 * 8000));
expect('test sound: left first, then right, never both',
    [energy(0, half / 2, 0) > 1, energy(0, half / 2, 1), energy(half / 2 + 1000, half, 0), energy(half / 2 + 1000, half, 1) > 1],
    [true, 0, 0, true]);
expect('test sound: quiet, and silent at both ends',
    [Math.max(...samples.map(Math.abs)) < 0.35, samples[0], samples[samples.length - 1]], [true, 0, 0]);

if (failed)
    imports.system.exit(1);
