// SPDX-License-Identifier: GPL-2.0-or-later
// Installs the pods engine for the current user, from the settings window,
// when the user clicks Install Engine; removes it on Uninstall Engine. Nothing
// happens without that click, nothing needs an administrator password, and no
// program is run: files are downloaded with libsoup, checked, unpacked with
// libgnome-autoar, and the engine is started by asking systemd over D-Bus.
//
//   1. download the release archive for this machine (x86_64 or aarch64),
//      whose URL, size and SHA-256 come with the extension (engine-release.json);
//   2. check its SHA-256 against that value, and stop at the first difference;
//   3. unpack it in ~/.cache/pods;
//   4. copy podsd (and podsctl) to ~/.local/lib/pods, the systemd user unit to
//      ~/.config/systemd/user and the D-Bus activation file to
//      ~/.local/share/dbus-1/services;
//   5. systemd daemon-reload, the bus's ReloadConfig, then StartUnit.
//
// The same paths as engine/scripts/install-local.sh: either removes what the
// other installed. The engine's settings (~/.config/pods) are never touched.
//
// No Shell or GTK import: prefs.js uses it, and so does tools/test-install.js.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Soup from 'gi://Soup?version=3.0';

import {BUS_NAME} from './engine.js';

export const UNIT = 'pods-engine.service';
const BUS_FILE = `${BUS_NAME}.service`;
const BINARIES = ['podsd', 'podsctl'];
const SYSTEMD = 'org.freedesktop.systemd1';
const SYSTEMD_PATH = '/org/freedesktop/systemd1';
const SYSTEMD_MANAGER = 'org.freedesktop.systemd1.Manager';
// How long the engine may take to own its name once started.
const START_TIMEOUT_MS = 15000;
const STOP_TIMEOUT_MS = 5000;
const CHUNK = 64 * 1024;

// ELF e_machine of the two architectures the engine is built for.
const MACHINES = {62: 'x86_64', 183: 'aarch64'};

export class InstallError extends Error {
    // code: 'unsupported', 'download', 'checksum', 'archive', 'files',
    // 'start', 'cancelled'
    constructor(code, message) {
        super(message);
        this.code = code;
    }
}

export function paths() {
    return {
        lib: GLib.build_filenamev([GLib.get_home_dir(), '.local', 'lib', 'pods']),
        units: GLib.build_filenamev([GLib.get_user_config_dir(), 'systemd', 'user']),
        services: GLib.build_filenamev([GLib.get_user_data_dir(), 'dbus-1', 'services']),
        cache: GLib.build_filenamev([GLib.get_user_cache_dir(), 'pods']),
    };
}

// The architecture of this computer's programs: that of the process itself,
// read from its ELF header ('x86_64', 'aarch64' or null).
export function machine() {
    try {
        const stream = Gio.File.new_for_path('/proc/self/exe').read(null);
        const header = stream.read_bytes(20, null).toArray();
        stream.close(null);
        if (header.length === 20 && header[0] === 0x7f && header[1] === 0x45 &&
            header[2] === 0x4c && header[3] === 0x46)
            return MACHINES[header[18] | header[19] << 8] ?? null;
    } catch {
        // Fall back on the kernel's own word.
    }
    try {
        const [, bytes] = GLib.file_get_contents('/proc/sys/kernel/arch');
        const arch = new TextDecoder().decode(bytes).trim();
        return Object.values(MACHINES).includes(arch) ? arch : null;
    } catch {
        return null;
    }
}

// engine-release.json: {version, assets: {arch: {url, size, sha256}}}.
export function readRelease(dir) {
    const [, bytes] = dir.get_child('engine-release.json').load_contents(null);
    return JSON.parse(new TextDecoder().decode(bytes));
}

// The archive for this machine, or null when there is none (yet).
export function releaseAsset(release, arch = machine()) {
    const asset = arch ? release?.assets?.[arch] : null;
    if (!asset?.url || !/^[0-9a-f]{64}$/.test(asset.sha256 ?? '') || !(asset.size > 0))
        return null;
    return {version: release.version, arch, ...asset};
}

// Only HTTPS, or plain HTTP to this very computer (the test bench).
function allowedUrl(url) {
    const uri = GLib.Uri.parse(url, GLib.UriFlags.NONE);
    const scheme = uri.get_scheme();
    return scheme === 'https' ||
        (scheme === 'http' && ['127.0.0.1', 'localhost', '[::1]', '::1'].includes(uri.get_host()));
}

function exists(path) {
    return GLib.file_test(path, GLib.FileTest.EXISTS);
}

// The engine's program is where this installer (or install-local.sh) puts it.
export function isInstalled() {
    return exists(GLib.build_filenamev([paths().lib, 'podsd']));
}

function promise(object, start, finish, ...args) {
    return new Promise((resolve, reject) => {
        object[start](...args, (source, result) => {
            try {
                resolve(source[finish](result));
            } catch (e) {
                reject(e);
            }
        });
    });
}

function busCall(name, path, iface, method, params, replyType, cancellable = null) {
    return promise(Gio.DBus.session, 'call', 'call_finish', name, path, iface, method, params,
        replyType ? new GLib.VariantType(replyType) : null, Gio.DBusCallFlags.NONE, -1, cancellable)
        .then(reply => reply?.deepUnpack());
}

function busDaemon(method, params = null, replyType = null) {
    return busCall('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
        method, params, replyType);
}

async function hasOwner(name) {
    const [owned] = await busDaemon('NameHasOwner', new GLib.Variant('(s)', [name]), '(b)');
    return owned;
}

// Resolves once `name` has an owner (appear) or none (vanish), false after
// timeoutMs.
function waitForName(name, appear, timeoutMs) {
    return new Promise(resolve => {
        let watchId = 0;
        let timeoutId = 0;
        const done = result => {
            if (!watchId)
                return;
            Gio.bus_unwatch_name(watchId);
            watchId = 0;
            if (timeoutId)
                GLib.source_remove(timeoutId);
            timeoutId = 0;
            resolve(result);
        };
        // The watcher answers from the main loop, never from inside this call.
        watchId = Gio.bus_watch_name_on_connection(Gio.DBus.session, name,
            Gio.BusNameWatcherFlags.NONE,
            () => appear && done(true),
            () => !appear && done(true));
        timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, timeoutMs, () => {
            timeoutId = 0;
            done(false);
            return GLib.SOURCE_REMOVE;
        });
    });
}

function systemd(method, params = null, replyType = null) {
    return busCall(SYSTEMD, SYSTEMD_PATH, SYSTEMD_MANAGER, method, params, replyType);
}

// Deletes a file or a directory with everything in it; nothing if absent.
function removeTree(path) {
    const file = Gio.File.new_for_path(path);
    const type = file.query_file_type(Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
    if (type === Gio.FileType.UNKNOWN)
        return;
    if (type === Gio.FileType.DIRECTORY) {
        const children = file.enumerate_children('standard::name',
            Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
        for (let info; (info = children.next_file(null));)
            removeTree(GLib.build_filenamev([path, info.get_name()]));
        children.close(null);
    }
    file.delete(null);
}

function removeFile(path) {
    try {
        Gio.File.new_for_path(path).delete(null);
    } catch (e) {
        if (!e.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.NOT_FOUND))
            throw e;
    }
}

// Writes `contents` next to `path`, then renames it over `path`: a running
// engine keeps its old program until it restarts.
function replaceFile(path, contents, mode) {
    const tmp = `${path}.new`;
    GLib.file_set_contents(tmp, contents);
    GLib.chmod(tmp, mode);
    Gio.File.new_for_path(tmp).move(Gio.File.new_for_path(path), Gio.FileCopyFlags.OVERWRITE, null, null);
}

function readFile(path) {
    return GLib.file_get_contents(path)[1];
}

export class EngineInstaller {
    // asset: releaseAsset(); progress({step, done, total}) with step one of
    // 'download', 'verify', 'extract', 'install', 'start'.
    constructor(asset, progress = () => {}) {
        this._asset = asset;
        this._progress = progress;
        this._cancellable = new Gio.Cancellable();
        this._session = null;
    }

    cancel() {
        this._cancellable.cancel();
    }

    async install() {
        const asset = this._asset;
        if (!asset)
            throw new InstallError('unsupported', 'No engine release for this computer');
        if (!allowedUrl(asset.url))
            throw new InstallError('download', `Not a secure address: ${asset.url}`);
        const p = paths();
        GLib.mkdir_with_parents(p.cache, 0o700);
        const archive = GLib.build_filenamev([p.cache, GLib.path_get_basename(GLib.Uri.parse(asset.url, GLib.UriFlags.NONE).get_path())]);
        const staging = GLib.build_filenamev([p.cache, `engine-${asset.arch}`]);
        try {
            const sha256 = await this._download(asset, archive);
            this._progress({step: 'verify', done: asset.size, total: asset.size});
            if (sha256 !== asset.sha256)
                throw new InstallError('checksum', `The download does not match: SHA-256 ${sha256}`);

            this._progress({step: 'extract'});
            removeTree(staging);
            await this._extract(archive, staging);
            const files = this._findFiles(staging);

            this._progress({step: 'install'});
            this._installFiles(files, p);

            this._progress({step: 'start'});
            await this._start();
        } catch (e) {
            if (this._cancellable.is_cancelled())
                throw new InstallError('cancelled', 'Cancelled');
            throw e;
        } finally {
            try {
                removeFile(archive);
                removeTree(staging);
            } catch (e) {
                console.warn(`pods: cannot clean ${p.cache}: ${e.message}`);
            }
        }
    }

    // Stops the engine, then removes what install() added. Resolves to
    // whether the engine stopped (false: it runs until the session ends).
    async uninstall() {
        const p = paths();
        let stopped = true;
        if (await hasOwner(SYSTEMD).catch(() => false)) {
            await systemd('StopUnit', new GLib.Variant('(ss)', [UNIT, 'replace']), '(o)')
                .catch(e => console.warn(`pods: StopUnit: ${e.message}`));
        }
        if (await hasOwner(BUS_NAME).catch(() => false))
            stopped = await waitForName(BUS_NAME, false, STOP_TIMEOUT_MS);

        for (const name of BINARIES)
            removeFile(GLib.build_filenamev([p.lib, name]));
        try {
            Gio.File.new_for_path(p.lib).delete(null);
        } catch {
            // Not empty (files of someone else) or already gone: leave it.
        }
        removeFile(GLib.build_filenamev([p.units, UNIT]));
        removeFile(GLib.build_filenamev([p.services, BUS_FILE]));
        await this._reload();
        return stopped;
    }

    async _download(asset, dest) {
        this._session ??= new Soup.Session({user_agent: 'pods-gnome-extension', timeout: 60});
        const message = Soup.Message.new('GET', asset.url);
        let input;
        try {
            input = await promise(this._session, 'send_async', 'send_finish', message,
                GLib.PRIORITY_DEFAULT, this._cancellable);
        } catch (e) {
            throw new InstallError('download', e.message);
        }
        if (message.get_status() !== Soup.Status.OK) {
            input.close(null);
            throw new InstallError('download', `HTTP ${message.get_status()} ${message.get_reason_phrase() ?? ''}`.trim());
        }
        const output = Gio.File.new_for_path(dest).replace(null, false,
            Gio.FileCreateFlags.PRIVATE | Gio.FileCreateFlags.REPLACE_DESTINATION, this._cancellable);
        const checksum = new GLib.Checksum(GLib.ChecksumType.SHA256);
        let done = 0;
        this._progress({step: 'download', done, total: asset.size});
        try {
            for (;;) {
                // eslint-disable-next-line no-await-in-loop
                const bytes = await promise(input, 'read_bytes_async', 'read_bytes_finish',
                    CHUNK, GLib.PRIORITY_DEFAULT, this._cancellable);
                const size = bytes.get_size();
                if (size === 0)
                    break;
                done += size;
                // Never more than announced: a longer file is not ours.
                if (done > asset.size)
                    throw new InstallError('checksum', `The download is larger than ${asset.size} bytes`);
                checksum.update(bytes.toArray());
                output.write_bytes(bytes, this._cancellable);
                this._progress({step: 'download', done, total: asset.size});
            }
        } catch (e) {
            if (e instanceof InstallError || this._cancellable.is_cancelled())
                throw e;
            throw new InstallError('download', e.message);
        } finally {
            output.close(null);
            input.close(null);
        }
        return checksum.get_string();
    }

    async _extract(archive, dir) {
        let Autoar;
        try {
            ({default: Autoar} = await import('gi://GnomeAutoar?version=0.1'));
        } catch {
            throw new InstallError('archive', 'libgnome-autoar is missing');
        }
        GLib.mkdir_with_parents(dir, 0o700);
        await new Promise((resolve, reject) => {
            const extractor = new Autoar.Extractor({
                source_file: Gio.File.new_for_path(archive),
                output_file: Gio.File.new_for_path(dir),
                output_is_dest: true,
                delete_after_extraction: false,
            });
            extractor.connect('error', (_e, error) => reject(new InstallError('archive', error.message)));
            extractor.connect('cancelled', () => reject(new InstallError('cancelled', 'Cancelled')));
            extractor.connect('completed', () => resolve());
            extractor.start_async(this._cancellable);
        });
    }

    // The engine's files in the unpacked archive: at its root, or in its one
    // folder (pods-engine-<version>-<arch>/).
    _findFiles(staging) {
        let root = staging;
        if (!exists(GLib.build_filenamev([root, 'podsd']))) {
            const children = Gio.File.new_for_path(staging).enumerate_children('standard::name,standard::type',
                Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
            const dirs = [];
            for (let info; (info = children.next_file(null));) {
                if (info.get_file_type() === Gio.FileType.DIRECTORY)
                    dirs.push(info.get_name());
            }
            children.close(null);
            if (dirs.length === 1)
                root = GLib.build_filenamev([staging, dirs[0]]);
        }
        const file = name => GLib.build_filenamev([root, name]);
        const regular = path => Gio.File.new_for_path(path)
            .query_file_type(Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null) === Gio.FileType.REGULAR;
        for (const name of ['podsd', UNIT, BUS_FILE]) {
            if (!regular(file(name)))
                throw new InstallError('archive', `${name} is missing from the archive`);
        }
        return {
            binaries: BINARIES.map(name => [name, file(name)]).filter(([, path]) => regular(path)),
            unit: file(UNIT),
            service: file(BUS_FILE),
        };
    }

    _installFiles(files, p) {
        try {
            for (const dir of [p.lib, p.units, p.services])
                GLib.mkdir_with_parents(dir, 0o755);
            for (const [name, path] of files.binaries)
                replaceFile(GLib.build_filenamev([p.lib, name]), readFile(path), 0o755);
            replaceFile(GLib.build_filenamev([p.units, UNIT]), readFile(files.unit), 0o644);
            const podsd = GLib.build_filenamev([p.lib, 'podsd']);
            const service = new TextDecoder().decode(readFile(files.service)).replaceAll('@PODSD@', podsd);
            replaceFile(GLib.build_filenamev([p.services, BUS_FILE]), service, 0o644);
        } catch (e) {
            throw new InstallError('files', e.message);
        }
    }

    // systemd and the bus read their files again.
    async _reload() {
        if (await hasOwner(SYSTEMD).catch(() => false))
            await systemd('Reload').catch(e => console.warn(`pods: systemd Reload: ${e.message}`));
        await busDaemon('ReloadConfig').catch(e => console.warn(`pods: ReloadConfig: ${e.message}`));
    }

    async _start() {
        await this._reload();
        const appeared = waitForName(BUS_NAME, true, START_TIMEOUT_MS);
        try {
            if (await hasOwner(SYSTEMD).catch(() => false)) {
                // A running engine (installed before) restarts on the new program.
                await systemd('RestartUnit', new GLib.Variant('(ss)', [UNIT, 'replace']), '(o)');
            } else {
                // No systemd user manager: the bus starts the engine itself.
                await busDaemon('StartServiceByName', new GLib.Variant('(su)', [BUS_NAME, 0]), '(u)');
            }
        } catch (e) {
            throw new InstallError('start', e.message);
        }
        if (!await appeared)
            throw new InstallError('start', 'The engine did not start');
    }
}
