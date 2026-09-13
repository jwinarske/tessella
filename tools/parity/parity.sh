#!/usr/bin/env bash
# One camera, both renderers, gross pixels.
#
#   parity.sh <scene> <lat> <lon> <zoom> <width> <height> <pitch> [bearing]
#
# The oracle is rendered here rather than reused from disk. A stored oracle whose camera nobody
# wrote down is what turned a 2,029 into a 41,355 once: the image was right and the camera behind
# it was not, and nothing in the file said so.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

scene=$1; lat=$2; lon=$3; z=$4; W=$5; H=$6; pitch=$7; bearing=${8:-0}
tag="${scene}_z${z}_p${pitch}"
style="$PARITY_DIR/scenes/$scene.json"
[ -f "$style" ] || { echo "no scene at $style" >&2; exit 1; }
[ -x "$MBGL_RENDER" ] || { echo "no oracle at $MBGL_RENDER (set MBGL_RENDER)" >&2; exit 1; }

"$MBGL_RENDER" --style "$style" --output "$PARITY_WORK/o_$tag.png" \
    --lat "$lat" --lon "$lon" --zoom "$z" --width "$W" --height "$H" \
    --pitch "$pitch" --bearing "$bearing" >/dev/null 2>&1 \
    || { echo "ORACLE FAILED $tag" >&2; exit 1; }

# TSF_NO_FADES: a fade is time-dependent and the two renderers are not started at the same
# instant, so comparing mid-fade measures the clock rather than the geometry.
out=$(TSF_NO_FADES=1 "$PARITY_WORK/render_probe" "$style" "$PARITY_WORK/mat" \
      "$PARITY_WORK/t_$tag.ppm" "$lat" "$lon" "$z" "$W" "$H" "$pitch" "$bearing" 2>&1)
grep -q "materials_loaded 14" <<<"$out" || { echo "MATERIALS NOT LOADED $tag" >&2; exit 1; }

printf "%-20s z%-4s p%-3s " "$scene" "$z" "$pitch"
python3 "$PARITY_DIR/gross.py" "$PARITY_WORK/t_$tag.ppm" "$PARITY_WORK/o_$tag.png"
