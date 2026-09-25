#!/usr/bin/env bash
# SPDX-License-Identifier: BSD-2-Clause
#
# Pixel parity for the style fixtures the golden dumps are captured from.
#
#   fixtures.sh [name...]     every fixture when none is named
#
# # What this covers that nothing else does
#
# `verify_goldens.sh` compares those fixtures' *structure* -- drawables, counts, shader ids, UBO
# bytes -- and never looks at a pixel. The sweep looks at pixels and never isolates a family: it
# draws Berlin, where a dash or a gradient is a few pixels among a million and a regression in one
# would sit well under the noise of everything else in the frame.
#
# So a change to a shader or a material can pass both. `dash_style` has no `line-dasharray` pixel
# anywhere else in this tree; neither has `gradient_style` for `line-gradient`, nor `evenodd_style`
# for the even-odd union. This closes that: one camera per fixture, the family alone on the frame.
#
# Hermetic. Every fixture named here carries its geometry inline, so this needs the two renderers
# and nothing else -- no tile server, no asset server, no snapshot.
#
# # The numbers
#
# Recorded rather than asserted, on the same argument as `tests/golden/README.md`: a number that
# fails the run the day an unrelated floor moves is a number people learn to skip. What the exit
# status is for is a fixture that stops rendering altogether, which is what the budget below
# catches.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
source "$here/env.sh"
styles=$TESSELLA_DIR/crates/tessella-style/tests

# Fixture, camera, and what it read on 2026-09-24. The budget is generous by design: it is there to
# notice a fixture that went blank or a family that stopped drawing, not to police a pixel.
#
#   name  lat  lon  zoom  pitch  measured  budget
scenes=(
    "composite  51.505 -0.11 13 0   27   200"
    "joins      51.505 -0.11 13 0    0   200"
    "fill       51.505 -0.11 13 0    2   200"
    "gradient   51.505 -0.11 13 0    0   200"
    "dash       51.505 -0.11 13 0    0   200"
    "evenodd    51.505 -0.11 13 0    2   200"
    "selfcross  51.505 -0.11 13 0    1   200"
    "extrusion  51.505 -0.11 13 0    0   200"
    "heatmap    51.505 -0.11 13 0    0   200"
    # Pitched, because `circle-pitch-alignment` and `circle-pitch-scale` coincide at zero. Its
    # number is the circle-edge floor the golden README describes -- one antialiased pixel per
    # circle, on a frame with a great many circles -- so its budget is the one that is not tight.
    "circle     51.505 -0.11 13 60 1074 1600"
)

# `relief_style` and `pattern_style` are not here. Both name a local file -- a DEM, a sprite sheet --
# and this side's probe draws an empty frame for them where the capture probe reads them fine: 38
# colors against one for the relief, 2,748 against twelve for the pattern. That is the probe's
# resource loading rather than the renderer, since the same families draw correctly from the tile
# server in `hill_p`, `relief_p` and the pattern scenes of the sweep. Left out rather than
# allowlisted, so the omission is a sentence and not a passing row.

status=0
want=("$@")

for row in "${scenes[@]}"; do
    read -r name lat lon zoom pitch measured budget <<<"$row"
    if [ ${#want[@]} -gt 0 ]; then
        found=0
        for one in "${want[@]}"; do [ "$one" = "$name" ] && found=1; done
        [ "$found" = 1 ] || continue
    fi

    style=$styles/${name}_style.json
    [ -f "$style" ] || {
        echo "no fixture at $style" >&2
        status=1
        continue
    }

    line=$(bash "$here/parity.sh" "$style" "$lat" "$lon" "$zoom" 1024 768 "$pitch") || {
        echo "FAILED $name" >&2
        status=1
        continue
    }
    gross=$(awk '{ for (i = 1; i <= NF; i++) if ($i == "gross") print $(i + 1) }' <<<"$line")
    printf '%-24s gross %-6s (was %s, budget %s)' "${name}_style" "$gross" "$measured" "$budget"
    if [ "$gross" -gt "$budget" ]; then
        printf '  OVER BUDGET\n'
        status=1
    else
        printf '\n'
    fi
done

exit "$status"
