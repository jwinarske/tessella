#!/usr/bin/env python3
"""Generates a synthetic glyph range for the vertical-writing fixtures.

`tests/glyph-fixtures/TestFont/0-255.pbf` is a real font subset and covers Latin only, which is
enough for every capture until vertical writing: mbgl only shapes a label vertically when some
character in it has an upright vertical orientation, and every such character is a CJK one.

Vendoring a CJK font to get three ideographs would put a third party's outlines in this
repository for a test that never looks at them. What the captures compare is *positions*, and a
position comes from the metrics — the advance, the bearings and the bitmap's size — not from what
the bitmap contains. So the glyphs here are synthetic: real metrics for a full-width ideograph,
and a distance field that is a plausible gradient rather than a letter. Both mbgl and the
frontend read the same file, so a shape that is not really a character is not a difference
between them.

The encoding is `glyphs.proto`: a `glyphs` holding one `fontstack` of `name`, `range` and its
`glyph`s, each with an id, an alpha bitmap, its width and height, its left and top bearings, and
its advance. The bitmap is three pixels larger than the glyph on every side, which is the border
the SDF needs and the one the atlas reserves.

Usage: synthetic.py [output path]
"""

import sys
from pathlib import Path

# A full-width ideograph at the 24-pixel size the ecosystem's glyphs are encoded at. The
# bearings put it in its em box the way the real ones do: `top` is measured from the baseline
# upwards, and the 3-pixel border is *not* in the width and height.
GLYPH_WIDTH = 20
GLYPH_HEIGHT = 20
GLYPH_LEFT = 2
GLYPH_TOP = 19
GLYPH_ADVANCE = 24
BORDER = 3

# 一, 三 and 二. Three strokes, three ideographs, and all three in the same 256-codepoint range,
# so one file covers the label.
CODEPOINTS = (0x4E00, 0x4E09, 0x4E8C)

STACK_NAME = "TestFont"


def varint(value):
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        out.append(byte | (0x80 if value else 0))
        if not value:
            return bytes(out)


def zigzag(value):
    return (value << 1) ^ (value >> 31)


def tag(field, wire):
    return varint(field << 3 | wire)


def uint(field, value):
    return tag(field, 0) + varint(value)


def sint(field, value):
    return tag(field, 0) + varint(zigzag(value))


def delimited(field, payload):
    return tag(field, 2) + varint(len(payload)) + payload


def bitmap(codepoint):
    """A distance field that is not a character but is shaped like one.

    A real SDF holds the signed distance to the nearest edge, scaled so that 192 is on the
    outline. This makes a filled rounded box: inside the glyph the value saturates, and it falls
    off through the border. Nothing reads it — the atlas packs by size and the captures compare
    positions — but a field that decays outward keeps the fixture honest to look at.
    """
    width = GLYPH_WIDTH + 2 * BORDER
    height = GLYPH_HEIGHT + 2 * BORDER
    out = bytearray(width * height)
    for row in range(height):
        for column in range(width):
            inset = min(row, column, height - 1 - row, width - 1 - column)
            # 192 is the on-curve value; step away from it by 8 a pixel, as a 3-pixel border
            # carrying a usable gradient does.
            out[row * width + column] = max(0, min(255, 192 + (inset - BORDER) * 8))
    # One codepoint's field must not equal another's, or an atlas that deduplicated identical
    # bitmaps would pack three glyphs as one and the capture would disagree for a reason that
    # has nothing to do with writing mode.
    out[0] = codepoint & 0xFF
    return bytes(out)


def main():
    glyphs = b""
    for codepoint in CODEPOINTS:
        body = (
            uint(1, codepoint)
            + delimited(2, bitmap(codepoint))
            + uint(3, GLYPH_WIDTH)
            + uint(4, GLYPH_HEIGHT)
            + sint(5, GLYPH_LEFT)
            + sint(6, GLYPH_TOP)
            + uint(7, GLYPH_ADVANCE)
        )
        glyphs += delimited(3, body)

    first = min(CODEPOINTS) // 256 * 256
    label = f"{first}-{first + 255}"
    stack = delimited(1, STACK_NAME.encode()) + delimited(2, label.encode()) + glyphs
    out = delimited(1, stack)

    destination = (
        Path(sys.argv[1])
        if len(sys.argv) > 1
        else Path(f"tests/glyph-fixtures/TestFont/{label}.pbf")
    )
    destination.write_bytes(out)
    print(f"wrote {destination}: {len(CODEPOINTS)} glyphs, range {label}, {len(out)} bytes")


if __name__ == "__main__":
    main()
