#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Builds the symbolic icons of extension/icons from the line drawings of the
mockups (airpods-study/maquettes: Main.dc.html, Feedback.dc.html).

GNOME recolours a symbolic icon by forcing the fill of every shape, so a
stroked drawing would come out as filled blobs. Each stroke is therefore turned
into its outline here (skia-pathops), and the outlines are merged into one
filled path.

    extension/tools/make-icons.py          rewrites extension/icons/*.svg

Needs python3-fonttools and skia-pathops (pip). Only a developer runs it; the
SVG files it writes are committed.
"""
import math
import pathlib
import re

import pathops
from fontTools.misc.transform import Transform
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.svgLib.path import parse_path

ICONS = pathlib.Path(__file__).resolve().parent.parent / 'icons'
INK = '#2e3436'


def circle(cx, cy, r):
    # Starts at 3 o'clock and runs clockwise, like the stroke of an SVG
    # <circle>, so that dashes fall where the mockup has them.
    return (f'M{cx + r} {cy}A{r} {r} 0 1 1 {cx - r} {cy}'
            f'A{r} {r} 0 1 1 {cx + r} {cy}Z')


def stroke(d, width, dash=None, join=pathops.LineJoin.MITER_JOIN):
    path = pathops.Path()
    parse_path(d, path.getPen())
    path.stroke(width, pathops.LineCap.ROUND_CAP, join, 4,
                dash_array=dash or None)
    path.convertConicsToQuads()
    path.simplify(fix_winding=True)
    return path


def union(paths):
    out = paths[0]
    for p in paths[1:]:
        out = pathops.op(out, p, pathops.PathOp.UNION, fix_winding=True)
    return out


MATERIAL = pathlib.Path(__file__).resolve().parent / 'material'


def material(name, keep=None):
    """One Material Symbols icon (tools/material/mat_<name>.svg) as a path;
    keep: the indices of the contours to keep."""
    d = re.search(r' d="([^"]+)"', (MATERIAL / f'mat_{name}.svg').read_text()).group(1)
    path = pathops.Path()
    parse_path(d, path.getPen())
    if keep is None:
        return path
    out = pathops.Path()
    for i, contour in enumerate(path.contours):
        if i in keep:
            contour.draw(out.getPen())
    return out


def place(path, height, cx, cy, rot=0, mirror=False):
    """Scales path to height, centres it on (cx, cy), turns it by rot degrees."""
    x0, y0, x1, y1 = path.bounds
    k = height / (y1 - y0)
    t = (Transform().translate(cx, cy).rotate(math.radians(rot))
         .scale(-k if mirror else k, k).translate(-(x0 + x1) / 2, -(y0 + y1) / 2))
    out = pathops.Path()
    path.draw(TransformPen(out.getPen(), t))
    return out


def to_box(path):
    """A Material path (viewBox 0 -960 960 960) in the 24 box, unchanged."""
    out = pathops.Path()
    path.draw(TransformPen(out.getPen(), Transform().scale(1 / 40, 1 / 40).translate(0, 960)))
    return out


def grow(path, width):
    """path widened by width: the gap cut around the arrow."""
    wide = pathops.Path()
    path.draw(wide.getPen())
    wide.stroke(width, pathops.LineCap.ROUND_CAP, pathops.LineJoin.ROUND_JOIN, 4)
    return union([wide, path])


def number(x):
    s = f'{x:.3f}'.rstrip('0').rstrip('.')
    return '0' if s == '-0' else s


def write(name, size, paths):
    pen = SVGPathPen(None, ntos=number)
    union(paths).draw(pen)
    svg = (f'<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" '
           f'viewBox="0 0 {size} {size}"><path d="{pen.getCommands()}" '
           f'fill="{INK}"/></svg>\n')
    (ICONS / f'{name}-symbolic.svg').write_text(svg)
    print(f'{name}-symbolic.svg')


def main():
    ICONS.mkdir(exist_ok=True)

    # The pair of earbuds of the tile and of the menu header (viewBox 20).
    left = 'M6.2 3.6a2.9 2.9 0 0 1 2.6 4.2l-0.3 0.5V16a1.1 1.1 0 0 1-2.2 0V9.6A3 3 0 0 1 6.2 3.6z'
    right = 'M13.8 3.6a2.9 2.9 0 0 0-2.6 4.2l0.3 0.5V16a1.1 1.1 0 0 0 2.2 0V9.6a3 3 0 0 0 0.1-6z'
    write('pods-earbuds', 20, [stroke(left, 1.7, join=pathops.LineJoin.ROUND_JOIN),
                               stroke(right, 1.7, join=pathops.LineJoin.ROUND_JOIN)])

    # Noise control (viewBox 24): a listener, alone for Off, inside a dotted
    # ring for Transparency, a solid ring for Noise Cancellation and half of
    # each for Adaptive.
    person = [stroke(circle(12, 9.2, 2.9), 1.6),
              stroke('M6.8 18.4c0-2.9 2.3-4.6 5.2-4.6s5.2 1.7 5.2 4.6', 1.6)]
    dots = [2.1, 2.5]
    write('pods-noise-off', 24, person)
    write('pods-noise-transparency', 24,
          person + [stroke(circle(12, 12, 10.3), 1.6, dots)])
    write('pods-noise-anc', 24, person + [stroke(circle(12, 12, 10.3), 2.3)])
    write('pods-noise-adaptive', 24, person + [
        stroke('M12 1.7a10.3 10.3 0 0 1 0 20.6', 2.3),
        stroke('M12 22.3a10.3 10.3 0 0 1 0-20.6', 1.6, dots),
    ])

    # Spatial audio (viewBox 24), composed from Material Symbols (Google,
    # Apache-2.0, tools/material): the listener of "person", the listener and
    # the arcs of "spatial_tracking", the arrow of "360". Off: the listener;
    # Fixed: arcs from the two upper corners aimed at the listener; Head
    # Tracked: the same with the turning arrow under the listener.
    person = material('person')
    listener = material('spatial_tracking', keep=[0, 1, 2, 3])
    arcs = material('spatial_tracking', keep=[4, 5, 6, 7])
    turn = material('360')
    arrow = place(turn, 5.0, 12, 21.3)
    small = place(listener, 12, 12, 11.5)
    write('pods-spatial-off', 24, [place(person, 15, 12, 13)])
    write('pods-spatial-fixed', 24, [to_box(listener), to_box(arcs)])
    write('pods-spatial-head', 24, [
        pathops.op(small, grow(arrow, 1.4), pathops.PathOp.DIFFERENCE, fix_winding=True),
        place(arcs, 8, 12, 4.2), arrow])

    # The case card (CaseCard.dc.html, viewBox 20): the tick of "Connected"
    # and the cross of its close button.
    write('pods-check', 20, [stroke('M4.5 10.5l3.5 3.5 7.5-8', 2.4, join=pathops.LineJoin.ROUND_JOIN)])
    write('pods-close', 20, [stroke('M5 5l10 10', 2.4), stroke('M15 5L5 15', 2.4)])


if __name__ == '__main__':
    main()
