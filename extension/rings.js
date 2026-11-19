// SPDX-License-Identifier: GPL-2.0-or-later
// The three battery rings of the menu: left earbud, right earbud, case.
// Sizes and colours come from the stylesheet, so they follow the scale factor
// and the light or dark style; only charging green and low-battery coral are
// colours of our own.

import Cairo from 'cairo';
import Clutter from 'gi://Clutter';
import GObject from 'gi://GObject';
import St from 'gi://St';

const TAU = 2 * Math.PI;
// The empty part of a ring: the text colour, faint.
const TRACK_ALPHA = 0.14;
// A ring whose level is unknown is drawn dotted and dimmed.
const UNKNOWN_OPACITY = 140;
// Size of the ring the case glyph was drawn for (the mockups: 52 px).
const DESIGN_SIZE = 52;

function withAlpha(cr, color, alpha) {
    cr.setSourceRGBA(color.red / 255, color.green / 255, color.blue / 255,
        color.alpha / 255 * alpha);
}

// The ring alone. Its size and line width come from the style class: the menu
// uses pods-ring, the case card pods-card-ring.
export const Ring = GObject.registerClass(
class PodsRing extends St.DrawingArea {
    constructor(isCase, styleClass = 'pods-ring') {
        super({
            style_class: styleClass,
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._isCase = isCase;
        this._percent = null;
        this._charging = false;
        this._low = false;
    }

    update(percent, charging, low) {
        this._percent = percent;
        this._charging = charging;
        this._low = low;
        this.queue_repaint();
    }

    vfunc_repaint() {
        const cr = this.get_context();
        const node = this.get_theme_node();
        const [width, height] = this.get_surface_size();
        const lineWidth = node.get_length('-pods-ring-width');
        const fg = node.get_foreground_color();
        const charging = node.get_color('-pods-charging-color');
        const low = node.get_color('-pods-low-color');

        // Before the style is known (the actor has no size yet), nothing.
        if (lineWidth <= 0 || width <= lineWidth || height <= lineWidth) {
            cr.$dispose();
            return;
        }

        const cx = width / 2;
        const cy = height / 2;
        const radius = Math.min(width, height) / 2 - lineWidth / 2;

        cr.setLineWidth(lineWidth);
        withAlpha(cr, fg, TRACK_ALPHA);
        if (this._percent === null)
            cr.setDash([lineWidth * 0.75, lineWidth], 0);
        cr.arc(cx, cy, radius, 0, TAU);
        cr.stroke();
        cr.setDash([], 0);

        if (this._percent !== null && this._percent > 0) {
            const color = this._charging ? charging : this._low ? low : fg;
            cr.setSourceColor(color);
            cr.setLineCap(Cairo.LineCap.ROUND);
            const start = -TAU / 4;
            cr.arc(cx, cy, radius, start, start + TAU * Math.min(this._percent, 100) / 100);
            cr.stroke();
        }

        if (this._isCase)
            this._drawCase(cr, cx, cy, Math.min(width, height) / DESIGN_SIZE, fg, charging);

        cr.$dispose();
    }

    // The case seen from the front, its lid line and its status light (green
    // while charging), from the 22 x 18 drawing of the mockups.
    _drawCase(cr, cx, cy, scale, fg, charging) {
        cr.save();
        cr.translate(cx - 11 * scale, cy - 9 * scale);
        cr.scale(scale, scale);
        cr.setLineWidth(1.5);
        cr.setSourceColor(fg);
        const [x, y, w, h, r] = [1.5, 2, 19, 14, 5];
        cr.newSubPath();
        cr.arc(x + w - r, y + r, r, -TAU / 4, 0);
        cr.arc(x + w - r, y + h - r, r, 0, TAU / 4);
        cr.arc(x + r, y + h - r, r, TAU / 4, TAU / 2);
        cr.arc(x + r, y + r, r, TAU / 2, TAU * 3 / 4);
        cr.closePath();
        cr.stroke();
        cr.moveTo(1.8, 7);
        cr.lineTo(20.2, 7);
        cr.stroke();
        cr.setSourceColor(this._charging ? charging : fg);
        cr.arc(11, 11.5, 0.9, 0, TAU);
        cr.fill();
        cr.restore();
    }
});

// One column: the ring with L, R or the case in it, the level, and the name
// of the part.
export const BatteryRing = GObject.registerClass(
class PodsBatteryRing extends St.BoxLayout {
    constructor(part, chargingIcon) {
        super({
            style_class: 'pods-battery',
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
        });
        this.part = part;
        this.item = null;

        const ringBox = new St.Widget({
            layout_manager: new Clutter.BinLayout(),
            x_align: Clutter.ActorAlign.CENTER,
        });
        this._ring = new Ring(part === 'case');
        ringBox.add_child(this._ring);
        if (part !== 'case') {
            ringBox.add_child(new St.Label({
                style_class: 'pods-ring-glyph',
                text: part === 'left' ? 'L' : 'R',
                x_align: Clutter.ActorAlign.CENTER,
                y_align: Clutter.ActorAlign.CENTER,
            }));
        }
        this.add_child(ringBox);

        const levelBox = new St.BoxLayout({
            style_class: 'pods-battery-level',
            x_align: Clutter.ActorAlign.CENTER,
        });
        this._bolt = new St.Icon({
            style_class: 'pods-charging-icon',
            gicon: chargingIcon,
            y_align: Clutter.ActorAlign.CENTER,
            visible: false,
        });
        levelBox.add_child(this._bolt);
        this._percent = new St.Label({
            style_class: 'pods-battery-percent',
            y_align: Clutter.ActorAlign.CENTER,
        });
        levelBox.add_child(this._percent);
        this.add_child(levelBox);

        this._name = new St.Label({
            style_class: 'pods-battery-label',
            x_align: Clutter.ActorAlign.CENTER,
        });
        this.add_child(this._name);
    }

    // item: one entry of model.batteryItems().
    update(item) {
        this.item = item;
        this._ring.update(item.percent, item.charging, item.low);
        this._bolt.visible = item.charging;
        this._percent.text = item.text;
        this._name.text = item.label;
        this.opacity = item.known ? 255 : UNKNOWN_OPACITY;
        if (item.low)
            this.add_style_class_name('pods-low');
        else
            this.remove_style_class_name('pods-low');
        this.accessible_name = `${item.label} ${item.text}`;
    }
});
