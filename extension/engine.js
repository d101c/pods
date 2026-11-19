// SPDX-License-Identifier: GPL-2.0-or-later
// Client of the AirPods engine (podsd) on the session bus, following the
// contract the engine is written against (contract.js). No Shell or GTK
// import: prefs.js uses it as well.
//
// Nothing is polled. The engine's name is watched; while it has an owner, the
// manager object is followed for ActiveDevice and the active device for its
// properties. Of the signals, only those a caller asked for are subscribed to
// (the Shell: NoiseModeChanged, LowBattery, CaseOpened and CaseClosed; the
// settings window: HeadPose, and only while it shows the head, see
// setHeadPoseWatch()).
//
// When the name has no owner, the bus is asked once whether it could start
// the engine: if not, the engine is missing (not installed) and the caller
// may offer to install it.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

import CONTRACT from './contract.js';

export const BUS_NAME = 'io.github.d101c.Pods';
export const MANAGER_PATH = '/io/github/d101c/Pods';
export const MANAGER_IFACE = 'io.github.d101c.Pods1.Manager';
export const DEVICE_IFACE = 'io.github.d101c.Pods1.Device';

// Activation is left to the name watcher; signals are subscribed one by one.
const PROXY_FLAGS = Gio.DBusProxyFlags.DO_NOT_AUTO_START |
    Gio.DBusProxyFlags.DO_NOT_CONNECT_SIGNALS |
    Gio.DBusProxyFlags.GET_INVALIDATED_PROPERTIES;

function proxyWrapper(name) {
    // makeProxyWrapper() takes the first interface of a node: give it one.
    const start = CONTRACT.indexOf(`<interface name="${name}">`);
    const end = CONTRACT.indexOf('</interface>', start);
    if (start < 0 || end < 0)
        throw new Error(`${name} is missing from the D-Bus contract`);
    return Gio.DBusProxy.makeProxyWrapper(
        `<node>${CONTRACT.slice(start, end + '</interface>'.length)}</node>`);
}

// The message of a D-Bus error without its "GDBus.Error:…" prefix.
export function errorMessage(error) {
    if (error instanceof GLib.Error && Gio.DBusError.is_remote_error(error))
        Gio.DBusError.strip_remote_error(error);
    return error.message;
}

export class Engine {
    // onChanged(): presence, active device or one of its properties changed.
    // onNoiseModeChanged(mode, source), onLowBattery(part, percent): the
    // active device's signals; onCaseOpened(path, snapshot), onCaseClosed(path):
    // the manager's. Each is subscribed to only when given.
    // onHeadPose(yaw, pitch): the device's HeadPose signal, while
    // setHeadPoseWatch(true) is in force.
    constructor({
        onChanged, onNoiseModeChanged = null, onLowBattery = null,
        onCaseOpened = null, onCaseClosed = null, onHeadPose = null,
    }) {
        this._wrappers = {Manager: proxyWrapper(MANAGER_IFACE), Device: proxyWrapper(DEVICE_IFACE)};
        this._onChanged = onChanged;
        this._onNoiseModeChanged = onNoiseModeChanged;
        this._onLowBattery = onLowBattery;
        this._onCaseOpened = onCaseOpened;
        this._onCaseClosed = onCaseClosed;
        this._onHeadPose = onHeadPose;

        this._bus = null;
        this._watchId = 0;
        this._manager = null;
        this._managerCancellable = null;
        this._managerChangedId = 0;
        this._devicePath = '/';
        this._device = null;
        this._deviceCancellable = null;
        this._deviceChangedId = 0;
        this._subscriptions = [];
        this._managerSubscriptions = [];
        // null while unknown, then whether the bus can start the engine.
        this._activatable = null;
        this._activatableCancellable = null;
        // HeadPose: what the caller wants, and the device proxy that was told.
        this._poseWanted = false;
        this._poseProxy = null;
        this._poseSubscription = 0;
    }

    start() {
        if (this._watchId)
            return;
        this._bus = Gio.DBus.session;
        // AUTO_START: the engine is D-Bus activatable (systemd user unit).
        this._watchId = Gio.bus_watch_name_on_connection(this._bus, BUS_NAME,
            Gio.BusNameWatcherFlags.AUTO_START,
            () => this._appeared(),
            () => this._vanished());
    }

    // Stops watching and drops every proxy and subscription. Nothing is
    // called back afterwards.
    stop() {
        if (this._watchId) {
            Gio.bus_unwatch_name(this._watchId);
            this._watchId = 0;
        }
        this._poseWanted = false;
        this._activatableCancellable?.cancel();
        this._activatableCancellable = null;
        // The engine is still there: it hears that the window stops watching.
        this._dropManager(false);
        this._bus = null;
    }

    // The engine answers on the bus.
    get present() {
        return this._manager !== null;
    }

    // Not running, and the bus has no way to start it: not installed.
    get missing() {
        return this._manager === null && this._activatable === false;
    }

    get version() {
        return this._manager?.EngineVersion ?? '';
    }

    // Asks the bus again whether it can start the engine (after the engine
    // was installed or removed).
    recheck() {
        if (this._watchId && !this._manager)
            this._checkActivatable();
    }

    // A plain snapshot of the active device, or null when there is none.
    get device() {
        const p = this._device;
        if (!p || !this._manager)
            return null;
        const battery = {};
        for (const [part, [percent, charging]] of Object.entries(p.Battery ?? {}))
            battery[part] = {percent, charging};
        return {
            path: p.g_object_path,
            address: p.Address ?? '',
            name: p.Name ?? '',
            model: p.Model ?? '',
            modelName: p.ModelName ?? '',
            firmware: p.Firmware ?? '',
            connected: p.Connected ?? false,
            capabilities: p.Capabilities ?? [],
            battery,
            wearing: p.Wearing ?? ['unknown', 'unknown'],
            noiseMode: p.NoiseMode ?? '',
            noiseModes: p.NoiseModes ?? [],
            pressHoldModes: p.PressHoldModes ?? [],
            earDetection: p.EarDetection ?? false,
            pauseOnRemove: p.PauseOnRemove ?? false,
            resumeOnReturn: p.ResumeOnReturn ?? false,
            mediaPaused: p.MediaPaused ?? false,
            conversationAwareness: p.ConversationAwareness ?? false,
            spatialMode: p.SpatialMode || 'off',
            spatialStatus: p.SpatialStatus || 'off',
            followScreen: p.FollowScreen ?? false,
            headTracking: p.HeadTracking || 'unavailable',
        };
    }

    cycleNoiseMode() {
        return this._call('CycleNoiseMode');
    }

    setNoiseMode(mode) {
        return this._call('SetNoiseMode', mode);
    }

    setPressHoldModes(modes) {
        return this._call('SetPressHoldModes', modes);
    }

    rename(name) {
        return this._call('Rename', name);
    }

    setEarDetection(enabled) {
        return this._call('SetEarDetection', enabled);
    }

    setPauseOnRemove(enabled) {
        return this._call('SetPauseOnRemove', enabled);
    }

    setResumeOnReturn(enabled) {
        return this._call('SetResumeOnReturn', enabled);
    }

    setConversationAwareness(enabled) {
        return this._call('SetConversationAwareness', enabled);
    }

    setSpatialMode(mode) {
        return this._call('SetSpatialMode', mode);
    }

    setFollowScreen(enabled) {
        return this._call('SetFollowScreen', enabled);
    }

    recenter() {
        return this._call('Recenter');
    }

    // The settings window wants the head pose (true) or no longer (false).
    // While wanted, every active device is told WatchHeadPose(true) and its
    // HeadPose signal is subscribed to; when no longer wanted, or when the
    // device goes, WatchHeadPose(false) and the subscription goes. Nothing is
    // sent twice.
    setHeadPoseWatch(wanted) {
        this._poseWanted = !!wanted;
        this._syncHeadPose();
    }

    // Connect() on any device object, connected or not: the case card offers
    // it for AirPods that are not the active device.
    connectDevice(path) {
        if (!this._manager)
            return Promise.reject(new Error('The AirPods engine is not running'));
        const bus = this._bus;
        return new Promise((resolve, reject) => {
            bus.call(this._manager.g_name_owner ?? BUS_NAME, path, DEVICE_IFACE, 'Connect',
                null, null, Gio.DBusCallFlags.NO_AUTO_START, -1, null, (_bus, result) => {
                    try {
                        bus.call_finish(result);
                        resolve();
                    } catch (e) {
                        reject(e);
                    }
                });
        });
    }

    _call(method, ...args) {
        if (!this._device)
            return Promise.reject(new Error('No AirPods connected'));
        return this._device[`${method}Async`](...args);
    }

    _changed() {
        if (this._watchId)
            this._onChanged?.();
    }

    _appeared() {
        this._activatableCancellable?.cancel();
        this._activatableCancellable = null;
        this._activatable = true;
        this._dropManager(true);
        const cancellable = new Gio.Cancellable();
        this._managerCancellable = cancellable;
        // The wrapper is a constructor: GNOME 51 requires `new`.
        // eslint-disable-next-line no-new
        new this._wrappers.Manager(this._bus, BUS_NAME, MANAGER_PATH, (proxy, error) => {
            if (cancellable.is_cancelled())
                return;
            this._managerCancellable = null;
            if (error) {
                console.warn(`pods: the engine does not answer: ${errorMessage(error)}`);
                return;
            }
            this._manager = proxy;
            this._managerChangedId = proxy.connect('g-properties-changed',
                () => this._followActiveDevice());
            this._subscribeManager(proxy);
            this._followActiveDevice();
            this._changed();
        }, cancellable, PROXY_FLAGS);
    }

    _vanished() {
        const wasPresent = this._manager !== null;
        this._dropManager(true);
        if (wasPresent)
            this._changed();
        this._checkActivatable();
    }

    _checkActivatable() {
        this._activatableCancellable?.cancel();
        const cancellable = new Gio.Cancellable();
        this._activatableCancellable = cancellable;
        const bus = this._bus;
        bus.call('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
            'ListActivatableNames', null, new GLib.VariantType('(as)'),
            Gio.DBusCallFlags.NONE, -1, cancellable, (_bus, result) => {
                if (cancellable.is_cancelled())
                    return;
                this._activatableCancellable = null;
                let activatable;
                try {
                    activatable = bus.call_finish(result).deepUnpack()[0].includes(BUS_NAME);
                } catch (e) {
                    console.warn(`pods: cannot ask the bus about the engine: ${errorMessage(e)}`);
                    return;
                }
                if (activatable !== this._activatable) {
                    this._activatable = activatable;
                    if (!this._manager)
                        this._changed();
                }
            });
    }

    // engineGone: the name lost its owner, nobody is left to tell anything.
    _dropManager(engineGone) {
        this._managerCancellable?.cancel();
        this._managerCancellable = null;
        this._dropDevice(!engineGone);
        for (const id of this._managerSubscriptions)
            this._bus?.signal_unsubscribe(id);
        this._managerSubscriptions = [];
        if (this._manager) {
            this._manager.disconnect(this._managerChangedId);
            this._managerChangedId = 0;
            this._manager = null;
        }
    }

    _followActiveDevice() {
        const path = this._manager?.ActiveDevice ?? '/';
        if (path === this._devicePath)
            return;
        this._dropDevice();
        this._devicePath = path;
        this._changed();
        if (path === '/')
            return;

        const cancellable = new Gio.Cancellable();
        this._deviceCancellable = cancellable;
        // eslint-disable-next-line no-new
        new this._wrappers.Device(this._bus, BUS_NAME, path, (proxy, error) => {
            if (cancellable.is_cancelled())
                return;
            this._deviceCancellable = null;
            if (error) {
                console.warn(`pods: cannot follow ${path}: ${errorMessage(error)}`);
                return;
            }
            this._device = proxy;
            this._deviceChangedId = proxy.connect('g-properties-changed', () => {
                this._syncHeadPose();
                this._changed();
            });
            this._subscribe(proxy);
            this._syncHeadPose();
            this._changed();
        }, cancellable, PROXY_FLAGS);
    }

    // Tells the active device what setHeadPoseWatch() asked for, once. Only a
    // connected device can watch: a device that connects later is told then.
    _syncHeadPose() {
        const device = this._device;
        const wanted = this._poseWanted && this._onHeadPose !== null &&
            device !== null && (device.Connected ?? false);
        if (wanted && this._poseProxy === device)
            return;
        if (!wanted && this._poseProxy === null)
            return;

        if (this._poseProxy) {
            const old = this._poseProxy;
            this._poseProxy = null;
            this._bus?.signal_unsubscribe(this._poseSubscription);
            this._poseSubscription = 0;
            // The engine stops the sensors when nobody watches. A device that
            // went away with the engine needs no word.
            if (this._manager && old === this._device)
                old.WatchHeadPoseAsync(false).catch(e => this._poseFailed(e));
        }
        if (!wanted)
            return;

        this._poseProxy = device;
        this._poseSubscription = this._bus.signal_subscribe(
            device.g_name_owner ?? BUS_NAME, DEVICE_IFACE, 'HeadPose', device.g_object_path, null,
            Gio.DBusSignalFlags.NONE,
            (_connection, _sender, _path, _iface, _signal, params) => {
                if (this._poseProxy === device)
                    this._onHeadPose(...params.deepUnpack());
            });
        device.WatchHeadPoseAsync(true).catch(e => this._poseFailed(e));
    }

    _poseFailed(error) {
        // An engine without head tracking (before M4) says NotSupported.
        if (!(error instanceof GLib.Error && Gio.DBusError.get_remote_error(error)?.endsWith('.NotSupported')))
            console.warn(`pods: WatchHeadPose failed: ${errorMessage(error)}`);
    }

    _subscribeManager(proxy) {
        const listen = (signal, handler) => {
            this._managerSubscriptions.push(this._bus.signal_subscribe(
                proxy.g_name_owner ?? BUS_NAME, MANAGER_IFACE, signal, MANAGER_PATH, null,
                Gio.DBusSignalFlags.NONE,
                (_connection, _sender, _path, _iface, _signal, params) => {
                    if (this._manager === proxy)
                        handler(...params.recursiveUnpack());
                }));
        };
        if (this._onCaseOpened)
            listen('CaseOpened', (path, snapshot) => this._onCaseOpened(path, snapshot));
        if (this._onCaseClosed)
            listen('CaseClosed', path => this._onCaseClosed(path));
    }

    _subscribe(proxy) {
        const listen = (signal, handler) => {
            this._subscriptions.push(this._bus.signal_subscribe(
                proxy.g_name_owner ?? BUS_NAME, DEVICE_IFACE, signal, proxy.g_object_path, null,
                Gio.DBusSignalFlags.NONE,
                (_connection, _sender, _path, _iface, _signal, params) => {
                    if (this._device === proxy)
                        handler(...params.deepUnpack());
                }));
        };
        if (this._onNoiseModeChanged)
            listen('NoiseModeChanged', (mode, source) => this._onNoiseModeChanged(mode, source));
        if (this._onLowBattery)
            listen('LowBattery', (part, percent) => this._onLowBattery(part, percent));
    }

    // tell: the device that watched the head pose hears that it stops (not
    // when the engine went away: there is nobody to tell).
    _dropDevice(tell = true) {
        this._deviceCancellable?.cancel();
        this._deviceCancellable = null;
        if (this._poseProxy) {
            const old = this._poseProxy;
            this._poseProxy = null;
            this._bus?.signal_unsubscribe(this._poseSubscription);
            this._poseSubscription = 0;
            if (tell && this._manager)
                old.WatchHeadPoseAsync(false).catch(e => this._poseFailed(e));
        }
        for (const id of this._subscriptions)
            this._bus?.signal_unsubscribe(id);
        this._subscriptions = [];
        if (this._device) {
            this._device.disconnect(this._deviceChangedId);
            this._deviceChangedId = 0;
            this._device = null;
        }
        this._devicePath = '/';
    }
}
