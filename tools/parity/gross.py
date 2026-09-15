# SPDX-License-Identifier: BSD-2-Clause
"""Gross pixels: per-channel difference over 48, the §9.1 metric.

Not a percentage of a percentage and not a mean: a mean hides a hundred wrong pixels in a
million right ones, and the thing worth knowing is how many pixels a person would call
different. 48 is the threshold that metric is defined at; 12 is the second lens, for when
a change should move nothing at all.
"""

import sys

from PIL import Image


def over_black(path: str) -> bytes:
    """An image as the probe would show it: composited over its opaque black clear.

    `mbgl-render` writes an RGBA PNG and un-premultiplies it on the way out, so a frame with no
    background keeps its color at full strength however transparent the pixel is. `convert("RGB")`
    drops that alpha rather than applying it, and a hillshade -- whose shade is mostly alpha --
    then read as twice as bright as the same picture on the probe's black. A frame with no alpha
    is unchanged.
    """
    image = Image.open(path)
    if "A" not in image.getbands():
        return image.convert("RGB").tobytes()
    rgba = image.convert("RGBA")
    black = Image.new("RGBA", rgba.size, (0, 0, 0, 255))
    return Image.alpha_composite(black, rgba).convert("RGB").tobytes()


threshold = int(sys.argv[3]) if len(sys.argv) > 3 else 48
a = over_black(sys.argv[1])
b = over_black(sys.argv[2])
if len(a) != len(b):
    sys.exit(f"different sizes: {len(a) // 3} against {len(b) // 3} pixels")
n = sum(
    1
    for i in range(0, len(a), 3)
    if max(abs(a[i] - b[i]), abs(a[i + 1] - b[i + 1]), abs(a[i + 2] - b[i + 2])) > threshold
)
total = len(a) // 3
print(f"gross {n} of {total}  ({100.0 * n / total:.3f}%)")
