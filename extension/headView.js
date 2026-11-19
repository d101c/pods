// SPDX-License-Identifier: GPL-2.0-or-later
// The top view of the Spatial Audio page (Spatial.dc.html): the screen at the
// top, the listener's head in the middle, turning with HeadPose, and the two
// virtual speakers. In Head Tracked they stay with the screen; in Fixed they
// turn with the head; in Off there are none, the sound is inside the head.
//
// Colours come from the theme (text colour, accent colour), so the view
// follows the light and dark styles and the accent the user picked.

import Adw from 'gi://Adw';
import GObject from 'gi://GObject';
import Gtk from 'gi://Gtk';
import Pango from 'gi://Pango';
import PangoCairo from 'gi://PangoCairo';

import {gettext as _} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

import {speakerAngles} from './model.js';

const HEIGHT = 270;
// Centre of the head, from the top (the mockup: 178 of 270).
const HEAD_Y = 178;
// Radius of the circle the speakers stand on, at most.
const RADIUS = 128;
// The head eases towards each new pose (~30 a second) with this time constant.
const EASE_S = 0.06;
// Under this many degrees from the pose, the head stops moving.
const SETTLED_DEG = 0.05;
const DEG = Math.PI / 180;

function source(cr, color, alpha = 1) {
    cr.setSourceRGBA(color.red, color.green, color.blue, color.alpha * alpha);
}

function ellipse(cr, x, y, rx, ry) {
    // A path of its own: no line from wherever the last one ended.
    cr.newSubPath();
    cr.save();
    cr.translate(x, y);
    cr.scale(rx, ry);
    cr.arc(0, 0, 1, 0, 2 * Math.PI);
    cr.restore();
}

function roundedRect(cr, x, y, width, height, radius) {
    cr.newSubPath();
    cr.arc(x + width - radius, y + radius, radius, -Math.PI / 2, 0);
    cr.arc(x + width - radius, y + height - radius, radius, 0, Math.PI / 2);
    cr.arc(x + radius, y + height - radius, radius, Math.PI / 2, Math.PI);
    cr.arc(x + radius, y + radius, radius, Math.PI, 3 * Math.PI / 2);
    cr.closePath();
}

export const HeadView = GObject.registerClass(
class PodsHeadView extends Gtk.DrawingArea {
    constructor() {
        super({
            content_height: HEIGHT,
            hexpand: true,
            accessible_role: Gtk.AccessibleRole.IMG,
        });
        this.update_property([Gtk.AccessibleProperty.LABEL],
            [_('Top view of your head and the two virtual speakers')]);
        this._mode = 'off';
        // Degrees clockwise seen from above: the pose, and what is drawn.
        this._target = 0;
        this._shown = 0;
        this._tickId = 0;
        this._lastFrame = 0;
        this.set_draw_func((_area, cr, width, height) => {
            try {
                this._draw(cr, width, height);
            } finally {
                cr.$dispose();
            }
        });

        const style = Adw.StyleManager.get_default();
        this._styleIds = [
            style.connect('notify::dark', () => this.queue_draw()),
            style.connect('notify::accent-color-rgba', () => this.queue_draw()),
        ];
        this.connect('destroy', () => {
            this._styleIds.forEach(id => style.disconnect(id));
            this._styleIds = [];
            this._stopTicking();
        });
        this.connect('unmap', () => {
            // Nothing to animate while nobody looks.
            this._stopTicking();
            this._shown = this._target;
        });
    }

    get angle() {
        return this._shown;
    }

    setMode(mode) {
        if (mode === this._mode)
            return;
        this._mode = mode;
        this.queue_draw();
    }

    // angle: model.headAngle(yaw).
    setAngle(angle) {
        this._target = angle;
        const animate = this.get_mapped() && Gtk.Settings.get_default().gtk_enable_animations;
        if (!animate) {
            this._stopTicking();
            this._shown = angle;
            this.queue_draw();
            return;
        }
        if (!this._tickId) {
            this._lastFrame = 0;
            this._tickId = this.add_tick_callback((_widget, clock) => this._tick(clock));
        }
    }

    _tick(clock) {
        const now = clock.get_frame_time();
        const dt = this._lastFrame ? (now - this._lastFrame) / 1e6 : 1 / 60;
        this._lastFrame = now;
        const gap = this._target - this._shown;
        if (Math.abs(gap) < SETTLED_DEG) {
            this._shown = this._target;
            this._tickId = 0;
            this.queue_draw();
            return false;
        }
        this._shown += gap * (1 - Math.exp(-dt / EASE_S));
        this.queue_draw();
        return true;
    }

    _stopTicking() {
        if (this._tickId)
            this.remove_tick_callback(this._tickId);
        this._tickId = 0;
    }

    _label(cr, text, x, baseline, {scale = 0.8, bold = false, color, alpha}) {
        const layout = this.create_pango_layout(text);
        const font = this.get_pango_context().get_font_description().copy();
        font.set_size(Math.round(font.get_size() * scale));
        if (bold)
            font.set_weight(Pango.Weight.SEMIBOLD);
        layout.set_font_description(font);
        const [, logical] = layout.get_pixel_extents();
        const ascent = layout.get_baseline() / Pango.SCALE;
        source(cr, color, alpha);
        cr.moveTo(x - logical.width / 2, baseline - ascent);
        PangoCairo.show_layout(cr, layout);
        cr.newPath();
    }

    _draw(cr, width, height) {
        const style = Adw.StyleManager.get_default();
        const dark = style.dark;
        const fg = this.get_color();
        const accent = style.get_accent_color_rgba();
        const cx = width / 2;
        const cy = Math.min(HEAD_Y, height - 92);
        const radius = Math.max(60, Math.min(RADIUS, cx - 48));

        // A shade darker than the card it sits in.
        cr.setSourceRGBA(0, 0, 0, dark ? 0.12 : 0.03);
        cr.paint();

        // Two rings around the listener.
        cr.setLineWidth(1);
        source(cr, fg, 0.06);
        cr.newPath();
        cr.arc(cx, cy, radius, 0, 2 * Math.PI);
        cr.stroke();
        source(cr, fg, 0.05);
        cr.arc(cx, cy, radius / 2, 0, 2 * Math.PI);
        cr.stroke();

        // The screen.
        source(cr, fg, 0.4);
        roundedRect(cr, cx - 75, 18, 150, 9, 4.5);
        cr.fill();
        this._label(cr, _('Screen'), cx, 44, {color: fg, alpha: 0.6});

        // The virtual speakers, a dotted line from each to the head.
        for (const {name, angle} of speakerAngles(this._mode, this._shown)) {
            const sin = Math.sin(angle * DEG);
            const cos = Math.cos(angle * DEG);
            const x = cx + radius * sin;
            const y = cy - radius * cos;
            source(cr, accent, 0.35);
            cr.setLineWidth(2);
            cr.setDash([3, 6], 0);
            cr.newPath();
            cr.moveTo(x, y);
            cr.lineTo(cx + 44 * sin, cy - 44 * cos);
            cr.stroke();
            cr.setDash([], 0);

            cr.save();
            cr.translate(x, y);
            cr.rotate(angle * DEG);
            source(cr, accent);
            roundedRect(cr, -15, -15, 30, 30, 8);
            cr.fill();
            cr.setSourceRGBA(1, 1, 1, 1);
            cr.setLineWidth(2);
            cr.newSubPath();
            cr.arc(0, 0, 6.5, 0, 2 * Math.PI);
            cr.stroke();
            cr.newSubPath();
            cr.arc(0, 0, 1.8, 0, 2 * Math.PI);
            cr.fill();
            cr.restore();

            this._label(cr, name, cx + (radius + 30) * sin, cy - (radius + 30) * cos + 4,
                {bold: true, color: fg, alpha: 0.75});
        }

        // The head, nose towards the screen, ears white like the AirPods.
        cr.newPath();
        cr.save();
        cr.translate(cx, cy);
        cr.rotate(this._shown * DEG);
        cr.setLineWidth(1.5);
        cr.setLineJoin(1 /* ROUND */);
        // Opaque, so that the nose covers the outline of the head under it.
        const skin = dark ? [0x45, 0x45, 0x4c] : [0xe4, 0xe4, 0xe9];
        ellipse(cr, 0, 0, 33, 38);
        cr.setSourceRGB(...skin.map(c => c / 255));
        cr.fillPreserve();
        source(cr, fg, 0.35);
        cr.stroke();
        cr.moveTo(-9, -35);
        cr.lineTo(0, -50);
        cr.lineTo(9, -35);
        cr.setSourceRGB(...skin.map(c => c / 255));
        cr.fillPreserve();
        source(cr, fg, 0.35);
        cr.stroke();
        for (const x of [-34, 34]) {
            ellipse(cr, x, 2, 5, 8);
            cr.setSourceRGBA(1, 1, 1, 1);
            if (dark) {
                cr.fill();
            } else {
                cr.fillPreserve();
                source(cr, fg, 0.35);
                cr.setLineWidth(1);
                cr.stroke();
            }
        }
        cr.restore();

        if (this._mode === 'off')
            this._label(cr, _('Plain stereo, inside your head'), cx, height - 20, {scale: 0.86, color: fg, alpha: 0.7});
    }
});
