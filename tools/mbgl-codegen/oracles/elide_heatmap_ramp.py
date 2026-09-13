"""Elides the one id a heatmap capture cannot reproduce: its color-ramp texture.

`RenderHeatmapLayer::update` rebuilds the texture-pass layer group on every frame, and each
rebuild does

    std::shared_ptr<gfx::Texture2D> texture = context.createTexture2D();
    texture->setImage(colorRamp);

so the ramp is a brand-new texture object with a new id, 43,000 times over a settle. The id a
drawable ends up binding is therefore a frame counter, and it moved by hundreds between three
consecutive captures.

Nothing else moves. `slot=0`, the offscreen render target, is created once per layer and keeps
its id; the ramp's *content* hash is identical every run and stays in the golden; the
`rendertargets` section is stable. Two lines of a hundred and three.

The churn itself is not elided anywhere -- the probe's `textures` section counts distinct
content rather than distinct ids, which is what keeps this dump at a hundred lines instead of
eighty-six thousand, and tests/golden/README.md records why.

    python3 tools/mbgl-codegen/oracles/elide_heatmap_ramp.py tests/golden/heatmap_style.dump
"""

import re
import sys

MARK = "------ (ramp recreated per frame)"

# The heatmap texture pass binds two: slot 0 is the render target, slot 1 the ramp. Only the
# second is recreated, so only the second goes.
RAMP_SLOT = "slot=1 "


def elide(text: str) -> tuple[str, int]:
    out, count = [], 0
    for line in text.split("\n"):
        if line.startswith("  tex ") and "sh0021" in line and RAMP_SLOT in line:
            line, n = re.subn(r"tex=\d+", f"tex={MARK}", line)
            count += n
        out.append(line)
    return "\n".join(out), count


if __name__ == "__main__":
    path = sys.argv[1]
    text = open(path).read()
    elided, count = elide(text)
    open(path, "w").write(elided)
    print(f"elided {count} lines in {path}")
