// SPDX-License-Identifier: GPL-2.0-or-later
// The symbolic icons shipped in icons/ (their names end in -symbolic, so the
// Shell recolours them like its own) and the colour drawings of art/.

import Gio from 'gi://Gio';

export class Icons {
    constructor(dir) {
        this._dir = dir;
        this._cache = new Map();
    }

    get(name) {
        return this._load('icons', name);
    }

    art(name) {
        return this._load('art', name);
    }

    _load(folder, name) {
        const key = `${folder}/${name}`;
        let icon = this._cache.get(key);
        if (!icon) {
            icon = new Gio.FileIcon({file: this._dir.get_child(folder).get_child(`${name}.svg`)});
            this._cache.set(key, icon);
        }
        return icon;
    }
}
