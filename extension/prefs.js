// SPDX-License-Identifier: GPL-2.0-or-later
// AirPods Settings. Two pages:
// - General: the settings that live on the AirPods (press and hold, ear
//   detection, name), read from and written to the engine; the one of the
//   extension itself (the case card), in GSettings; and the engine (version,
//   Uninstall Engine).
// - Spatial Audio (spatialPage.js).
//
// When the engine is not installed, a banner on each page offers to install
// it: one click on Install Engine (installer.js), and nothing before it.

import Adw from 'gi://Adw';
import Gdk from 'gi://Gdk';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Gtk from 'gi://Gtk';

import {ExtensionPreferences, gettext as _} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

import {Engine, errorMessage} from './engine.js';
import * as Installer from './installer.js';
import {batterySummary, fmt, formatSize, noiseLabel, setGettext, shortModelName} from './model.js';
import {SpatialPage} from './spatialPage.js';

// The stem cycles through at least two modes.
const MIN_PRESS_HOLD_MODES = 2;

export default class PodsPreferences extends ExtensionPreferences {
    fillPreferencesWindow(window) {
        setGettext(_);
        const display = Gdk.Display.get_default();
        if (display)
            Gtk.IconTheme.get_for_display(display).add_search_path(this.dir.get_child('icons').get_path());

        window.title = _('AirPods Settings');
        window.set_default_size(680, 760);
        window.search_enabled = false;

        const pages = [];
        let spatial = null;
        let setup = null;
        const engine = new Engine({
            onChanged: () => {
                setup?.syncBanners();
                pages.forEach(page => page.sync());
            },
            onHeadPose: (yaw, pitch) => spatial?.headPose(yaw, pitch),
        });
        setup = new EngineSetup(this.dir, engine, window, () => pages.forEach(page => page.sync()));

        const general = new GeneralPage(window, this.getSettings(), engine, setup);
        spatial = new SpatialPage(window, engine, setup.banner(), new Placeholder(setup));
        pages.push(general, spatial);
        window.add(general);
        window.add(spatial);
        engine.start();
        pages.forEach(page => page.sync());

        window.connect('close-request', () => {
            pages.forEach(page => page.stop());
            setup.stop();
            engine.stop();
            return false;
        });
    }
}

// The install and uninstall of the engine, shared by the pages: what the
// banners and the placeholders say comes from here.
class EngineSetup {
    constructor(dir, engine, window, changed) {
        this._engine = engine;
        this._window = window;
        this._changed = changed;
        this._banners = [];
        this._installer = null;
        // 'idle', 'busy' or 'failed'
        this.state = 'idle';
        this.step = '';
        this.fraction = 0;
        this.error = '';
        this.asset = null;
        try {
            this.asset = Installer.releaseAsset(Installer.readRelease(dir));
        } catch (e) {
            console.warn(`pods: engine-release.json: ${e.message}`);
        }
    }

    stop() {
        this._installer?.cancel();
        this._installer = null;
        this._changed = () => {};
    }

    // Offered when the engine is missing, and while installing or after a
    // failure (the engine is still missing then, but say what happens).
    get offered() {
        return this._engine.missing || this.state !== 'idle';
    }

    get host() {
        if (!this.asset)
            return '';
        return GLib.Uri.parse(this.asset.url, GLib.UriFlags.NONE).get_host();
    }

    // A group holding this page's banner.
    banner() {
        const banner = new Adw.Banner();
        banner.connect('button-clicked', () => {
            if (this.state === 'busy')
                this._installer?.cancel();
            else
                this.install();
        });
        this._banners.push(banner);
        const group = new Adw.PreferencesGroup();
        group.add(banner);
        this.syncBanner(banner);
        return group;
    }

    syncBanners() {
        this._banners.forEach(banner => this.syncBanner(banner));
    }

    syncBanner(banner) {
        banner.revealed = this.offered;
        banner.button_style = this.state === 'busy'
            ? Adw.BannerButtonStyle.DEFAULT : Adw.BannerButtonStyle.SUGGESTED;
        if (this.state === 'busy') {
            banner.title = this.progressText();
            banner.button_label = this.step === 'download' ? _('Cancel') : null;
        } else if (this.state === 'failed') {
            banner.title = fmt(_('Could not install the AirPods engine: %s'), this.error);
            banner.button_label = _('Try Again');
        } else if (this.asset) {
            banner.title = _('The AirPods engine is not installed');
            banner.button_label = _('Install Engine');
        } else {
            banner.title = _('The AirPods engine is not installed, and there is none for this computer yet');
            banner.button_label = null;
        }
    }

    progressText() {
        switch (this.step) {
        case 'download':
            return fmt(_('Downloading the AirPods engine… %d%%'), Math.floor(this.fraction * 100));
        case 'verify':
            return _('Checking the download…');
        case 'extract':
        case 'install':
            return _('Installing the AirPods engine…');
        default:
            return _('Starting the AirPods engine…');
        }
    }

    async install() {
        if (this.state === 'busy' || !this.asset)
            return;
        this.state = 'busy';
        this.step = 'download';
        this.fraction = 0;
        this._update();
        const installer = new Installer.EngineInstaller(this.asset, ({step, done, total}) => {
            this.step = step;
            if (step === 'download')
                this.fraction = total ? Math.min(1, done / total) : 0;
            this._update();
        });
        this._installer = installer;
        try {
            await installer.install();
            this.state = 'idle';
            this._window.add_toast(new Adw.Toast({title: _('AirPods engine installed')}));
        } catch (e) {
            if (e.code === 'cancelled') {
                this.state = 'idle';
            } else {
                this.state = 'failed';
                this.error = e.code === 'checksum'
                    ? _('the download does not match the engine this extension expects')
                    : e.message;
                console.warn(`pods: engine install failed: ${e.message}`);
            }
        } finally {
            if (this._installer === installer)
                this._installer = null;
            this._engine.recheck();
            this._update();
        }
    }

    async uninstall() {
        if (this.state === 'busy')
            return;
        this.state = 'busy';
        this.step = 'uninstall';
        this._update();
        let stopped = true;
        try {
            stopped = await new Installer.EngineInstaller(null).uninstall();
            this._window.add_toast(new Adw.Toast({
                title: stopped ? _('AirPods engine uninstalled')
                    : _('AirPods engine uninstalled. It stops when you log out.'),
            }));
        } catch (e) {
            this._window.add_toast(new Adw.Toast({
                title: fmt(_('Could not uninstall the AirPods engine: %s'), e.message),
            }));
        } finally {
            this.state = 'idle';
            this._engine.recheck();
            this._update();
        }
    }

    _update() {
        this.syncBanners();
        this._changed();
    }
}

// What a page shows instead of its settings: no engine (with what Install
// Engine does), engine not running, or no AirPods.
const Placeholder = GObject.registerClass(
class PodsPlaceholder extends Adw.PreferencesGroup {
    constructor(setup) {
        super();
        this._setup = setup;
        this._status = new Adw.StatusPage({
            icon_name: 'pods-earbuds-symbolic',
            vexpand: true,
        });
        this._progress = new Gtk.ProgressBar({
            visible: false,
            width_request: 280,
            halign: Gtk.Align.CENTER,
        });
        this._status.child = this._progress;
        this.add(this._status);
    }

    sync(engine) {
        const setup = this._setup;
        this._progress.visible = setup.state === 'busy';
        this._progress.fraction = setup.step === 'download' ? setup.fraction : 1;
        if (setup.offered) {
            this._status.title = _('Install the AirPods Engine');
            this._status.description = setup.asset
                ? fmt(_('Pods talks to your AirPods through the pods engine, a small background service. Install Engine downloads version %s (%s) from %s, checks it against the fingerprint this extension carries, and installs it in your home folder. No administrator password is needed.'),
                    setup.asset.version, formatSize(setup.asset.size), setup.host)
                : _('Pods talks to your AirPods through the pods engine, a small background service. There is no release of it for this computer yet: see github.com/d101c/pods to build it.');
        } else if (engine.present) {
            this._status.title = _('No AirPods Connected');
            this._status.description = _('Connect your AirPods in the Bluetooth settings.');
        } else {
            this._status.title = _('AirPods Engine Not Running');
            this._status.description = _('The pods engine talks to your AirPods. It starts with your session; if it does not, see its log with “journalctl --user -u pods-engine”.');
        }
    }
});

const GeneralPage = GObject.registerClass(
class PodsGeneralPage extends Adw.PreferencesPage {
    constructor(window, settings, engine, setup) {
        super({
            title: _('General'),
            icon_name: 'pods-earbuds-symbolic',
            name: 'general',
        });
        this._window = window;
        this._settings = settings;
        this._engine = engine;
        this._setup = setup;
        this._syncing = false;
        this._modesKey = '';
        this._checks = new Map();

        this.add(setup.banner());
        this._placeholder = new Placeholder(setup);
        this.add(this._placeholder);
        this._buildHeader();
        this._buildPressHold();
        this._buildEarDetection();
        this._buildDevice();
        this._buildDesktop();
        this._buildEngine();
    }

    stop() {
        this._engine = null;
    }

    _buildHeader() {
        this._header = new Adw.PreferencesGroup();
        const box = new Gtk.Box({
            orientation: Gtk.Orientation.VERTICAL,
            spacing: 6,
            halign: Gtk.Align.CENTER,
            margin_bottom: 6,
        });
        box.append(new Gtk.Image({
            icon_name: 'pods-earbuds-symbolic',
            pixel_size: 64,
            margin_bottom: 6,
            css_classes: ['dim-label'],
        }));
        this._title = new Gtk.Label({css_classes: ['title-1'], wrap: true, justify: Gtk.Justification.CENTER});
        box.append(this._title);
        this._subtitle = new Gtk.Label({css_classes: ['dim-label']});
        box.append(this._subtitle);
        this._levels = new Gtk.Label({css_classes: ['heading', 'numeric'], margin_top: 4});
        box.append(this._levels);
        this._header.add(box);
        this.add(this._header);
    }

    _buildPressHold() {
        this._pressHold = new Adw.PreferencesGroup({
            title: _('Press and Hold'),
            description: _('Press and hold the stem of either AirPod to switch between the modes you choose. Pick at least two. Clicking the Quick Settings tile does the same.'),
        });
        this.add(this._pressHold);
    }

    _buildEarDetection() {
        this._ear = new Adw.PreferencesGroup({title: _('Ear Detection')});
        this._earRow = new Adw.SwitchRow({
            title: _('Automatic Ear Detection'),
            subtitle: _('Plays sound only when AirPods are in your ears'),
        });
        this._pauseRow = new Adw.SwitchRow({
            title: _('Pause When Removed'),
            subtitle: _('Pauses music and videos when you take one out'),
        });
        this._resumeRow = new Adw.SwitchRow({
            title: _('Resume When Put Back'),
            subtitle: _('Within a minute of taking them out'),
        });
        const bind = (row, set) => row.connect('notify::active', () => {
            if (!this._syncing)
                this._run(set(row.active));
        });
        bind(this._earRow, on => this._engine.setEarDetection(on));
        bind(this._pauseRow, on => this._engine.setPauseOnRemove(on));
        bind(this._resumeRow, on => this._engine.setResumeOnReturn(on));
        this._ear.add(this._earRow);
        this._ear.add(this._pauseRow);
        this._ear.add(this._resumeRow);
        this.add(this._ear);
    }

    _buildDevice() {
        this._deviceGroup = new Adw.PreferencesGroup({title: _('Device')});
        this._nameRow = new Adw.EntryRow({
            title: _('Name'),
            show_apply_button: true,
        });
        this._nameRow.connect('apply', () => {
            const name = this._nameRow.text.trim();
            if (name)
                this._run(this._engine.rename(name));
        });
        this._deviceGroup.add(this._nameRow);
        this.add(this._deviceGroup);
    }

    // Settings of this computer, not of the AirPods: shown even without them.
    _buildDesktop() {
        const group = new Adw.PreferencesGroup({title: _('Desktop')});
        const row = new Adw.SwitchRow({
            title: _('Show Card When Case Opens'),
            subtitle: _('Shows the batteries, or a Connect button, when you open the case nearby'),
        });
        this._settings.bind('show-case-card', row, 'active', Gio.SettingsBindFlags.DEFAULT);
        group.add(row);
        this.add(group);
    }

    // The engine itself: its version, and Uninstall Engine when it lives in
    // ~/.local/lib/pods (put there by Install Engine or install-local.sh).
    _buildEngine() {
        this._engineGroup = new Adw.PreferencesGroup({title: _('Engine')});
        this._engineRow = new Adw.ActionRow({title: _('AirPods Engine')});
        this._uninstall = new Gtk.Button({
            label: _('Uninstall Engine…'),
            valign: Gtk.Align.CENTER,
            css_classes: ['destructive-action'],
        });
        this._uninstall.connect('clicked', () => this._confirmUninstall());
        this._engineRow.add_suffix(this._uninstall);
        this._engineGroup.add(this._engineRow);
        this.add(this._engineGroup);
    }

    _confirmUninstall() {
        const dialog = new Adw.AlertDialog({
            heading: _('Uninstall the AirPods Engine?'),
            body: _('Your AirPods stop working with this computer until you install it again. Your settings are kept.'),
            default_response: 'cancel',
            close_response: 'cancel',
        });
        dialog.add_response('cancel', _('Cancel'));
        dialog.add_response('uninstall', _('Uninstall'));
        dialog.set_response_appearance('uninstall', Adw.ResponseAppearance.DESTRUCTIVE);
        dialog.connect('response', (_dialog, response) => {
            if (response === 'uninstall')
                this._setup.uninstall();
        });
        dialog.present(this._window);
    }

    sync() {
        const engine = this._engine;
        if (!engine)
            return;
        const device = engine.device;
        const connected = engine.present && device?.connected;
        this._placeholder.sync(engine);
        this._placeholder.visible = !connected;
        for (const group of [this._header, this._pressHold, this._ear, this._deviceGroup])
            group.visible = !!connected;
        this._syncEngine(engine);
        if (!connected)
            return;

        this._syncing = true;
        const caps = device.capabilities;
        const model = shortModelName(device);
        this._title.label = device.name || model;
        this._subtitle.label = `${device.modelName || model} · ${_('Connected')}`;
        this._levels.label = batterySummary(device);

        this._syncPressHold(device);
        this._pressHold.visible = caps.includes('pressHold') && device.noiseModes.length >= MIN_PRESS_HOLD_MODES;

        this._earRow.visible = caps.includes('earDetection');
        this._earRow.active = device.earDetection;
        this._pauseRow.active = device.pauseOnRemove;
        this._resumeRow.active = device.resumeOnReturn;
        const followsEars = device.earDetection || !caps.includes('earDetection');
        this._pauseRow.sensitive = followsEars;
        this._resumeRow.sensitive = followsEars;

        this._deviceGroup.visible = caps.includes('rename');
        if (!this._nameRow.has_focus && this._nameRow.text !== device.name)
            this._nameRow.text = device.name;
        this._syncing = false;
    }

    _syncEngine(engine) {
        const installed = Installer.isInstalled();
        this._engineGroup.visible = installed || engine.present;
        const where = installed ? '~/.local/lib/pods' : '';
        let state;
        if (this._setup.state === 'busy' && this._setup.step === 'uninstall')
            state = _('Uninstalling…');
        else if (engine.present)
            state = engine.version ? fmt(_('Version %s · Running'), engine.version) : _('Running');
        else
            state = _('Not running');
        this._engineRow.subtitle = where ? `${state} · ${where}` : state;
        this._uninstall.visible = installed;
        this._uninstall.sensitive = this._setup.state !== 'busy';
    }

    _syncPressHold(device) {
        const key = device.noiseModes.join(',');
        if (key !== this._modesKey) {
            this._modesKey = key;
            for (const row of this._checks.values())
                this._pressHold.remove(row);
            this._checks.clear();
            for (const mode of device.noiseModes) {
                const check = new Gtk.CheckButton({valign: Gtk.Align.CENTER});
                const row = new Adw.ActionRow({title: noiseLabel(mode), activatable_widget: check});
                row.add_prefix(check);
                row.check = check;
                check.connect('toggled', () => {
                    if (!this._syncing)
                        this._pressHoldChanged();
                });
                this._pressHold.add(row);
                this._checks.set(mode, row);
            }
        }
        const chosen = new Set(device.pressHoldModes);
        for (const [mode, row] of this._checks)
            row.check.active = chosen.has(mode);
        this._lockLastModes();
    }

    // With only two modes left, those two cannot be unticked.
    _lockLastModes() {
        const ticked = [...this._checks.values()].filter(row => row.check.active);
        for (const row of this._checks.values())
            row.check.sensitive = !(row.check.active && ticked.length <= MIN_PRESS_HOLD_MODES);
    }

    _pressHoldChanged() {
        const modes = [...this._checks].filter(([, row]) => row.check.active).map(([mode]) => mode);
        this._lockLastModes();
        if (modes.length >= MIN_PRESS_HOLD_MODES)
            this._run(this._engine.setPressHoldModes(modes));
    }

    _run(promise) {
        promise.catch(e => {
            this._window.add_toast(new Adw.Toast({
                title: _('The AirPods did not take the change: %s').replace('%s', errorMessage(e)),
            }));
            this.sync();
        });
    }
});
