// SPDX-License-Identifier: GPL-2.0-or-later
// A fullscreen window for extension/tools/shell-test: the case card must not
// show over it. inner.sh runs it in the headless Shell and closes it with the
// probe's "closewindows".
import Gtk from 'gi://Gtk?version=4.0';

const app = new Gtk.Application({application_id: 'io.github.d101c.Pods.TestFullscreen'});
app.connect('activate', () => {
    const window = new Gtk.ApplicationWindow({application: app, title: 'Fullscreen'});
    window.fullscreen();
    window.present();
});
app.run([]);
