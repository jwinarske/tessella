"""Turns `fill-antialias` off on every fill layer of a composed style, in place.

# Why the suite measures with it off

mbgl draws a fill outline two ways and they do not agree with each other.
`MLN_TRIANGULATE_FILL_OUTLINES` in `src/mbgl/renderer/buckets/fill_bucket.hpp` is
`(MLN_RENDER_BACKEND_METAL || MLN_RENDER_BACKEND_WEBGPU)`:

| | how | coverage outside the fill |
|---|---|---|
| mbgl GL / Vulkan | `gfx::Lines(lineWidth)`, `lineWidth = 2.0f` | ~1 px at full alpha, aliased |
| mbgl Metal / WebGPU | a triangulated quad | ramps 1 to 0 over 1 px |
| tessella + Filament | the same triangulated quad | the same ramp |

The parity oracle is a Vulkan build, so it takes the hardware-line path, and this side is
bit-faithful to the other one -- the shader math was checked against
`fill_outline_triangulated.{vertex,fragment}.glsl` line by line. Filament exposes no line width in
any public header, so this side cannot take the oracle's path even to imitate it.

So the difference is permanent and it is not a defect. Measured over all 59 example cameras on
2026-09-22: **26,225 gross with outlines, 6,886 without**. Roughly three quarters of what the suite
reports was one difference nothing is going to fix, which is enough to hide a new defect of any
size below it -- the point of a gross number is to notice something changing, and a number whose
bulk cannot change is not doing that.

Turning the property off removes the outline from *both* renderers, because `examples.sh` patches
the composed style once and `parity.sh` hands the same file to each. That is what makes this a
change to the metric rather than a change to what is being compared.

# What it gives up

An outline *defect* stops being visible in the number. That was already true -- tessella#243 fixed
a triangulated outline missing its layer's `fill-translate` and the gross count barely moved -- and
the guidance for that class is to judge it by the overlap of the two outline masks and the best-fit
shift between them, not by gross. `PARITY_FILL_OUTLINES=1` restores the old measurement when that
is what is wanted.

    python3 tools/parity/no_fill_antialias.py <composed style>
"""

import json
import sys


def suppress(path: str) -> int:
    with open(path, encoding="utf-8") as handle:
        style = json.load(handle)

    patched = 0
    for layer in style.get("layers", []):
        # `fill-antialias` is a fill-layer property. A fill-extrusion has no outline to suppress
        # and a line layer is not an outline, so neither is touched.
        if layer.get("type") != "fill":
            continue
        paint = layer.setdefault("paint", {})
        if paint.get("fill-antialias") is not False:
            paint["fill-antialias"] = False
            patched += 1

    if patched:
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(style, handle)
    return patched


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: no_fill_antialias.py <style>")
    print(f"suppressed {suppress(sys.argv[1])} fill outlines")
