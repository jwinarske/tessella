#!/usr/bin/env bash
# SPDX-License-Identifier: BSD-2-Clause
#
# One camera, this renderer only, and how much of the frame is a hole.
#
#   coverage.sh <scene> <lat> <lon> <zoom> <width> <height> <pitch> <hex>
#
# For a terrain above zero exaggeration, which maplibre-native cannot draw and so cannot be
# compared against. What can be held is that the ground is covered: the ground takes the
# background's color, so a scene whose background is a color nothing else uses shows every hole
# in it as that color, and the count of those pixels is the measure.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

scene=$1
lat=$2
lon=$3
z=$4
W=$5
H=$6
pitch=$7
hex=$8
style="$PARITY_DIR/scenes/$scene.json"
tag="${scene}_z${z}_p${pitch}"

out=$(TSF_NO_FADES=1 "$PARITY_WORK/render_probe" "$style" "$PARITY_WORK/mat" \
  "$PARITY_WORK/t_$tag.ppm" "$lat" "$lon" "$z" "$W" "$H" "$pitch" 0 2>&1)
grep -qE "^materials_loaded [1-9][0-9]* rejected 0$" <<<"$out" || {
  echo "MATERIALS NOT LOADED $tag" >&2
  exit 1
}
# A hole is the background's color, and black is not that color -- so a frame the renderer never
# lit at all has no holes in it and scores a clean zero. That is not a hypothetical: a change that
# blanked every viewport past about a megapixel passed this gate at five cameras, because the
# count it reports only ever looked for magenta.
#
# `lit_pixels` is the probe's own count of what the renderer put down. Every camera here is ground
# to the horizon and lights the whole frame, so anything short of all of them is the measure
# reporting on an image that was never drawn. A camera that means to show sky would need to say so.
lit=$(sed -nE 's/^lit_pixels ([0-9]+) of ([0-9]+)$/\1 \2/p' <<<"$out")
[ -n "$lit" ] || {
  echo "NO lit_pixels REPORTED $tag" >&2
  exit 1
}
[ "${lit% *}" = "${lit#* }" ] || {
  echo "FRAME NOT DRAWN $tag: lit_pixels $lit" >&2
  exit 1
}

printf "%-20s z%-4s p%-3s " "$scene" "$z" "$pitch"
python3 - "$PARITY_WORK/t_$tag.ppm" "$hex" <<'PY'
import sys

path, hex_color = sys.argv[1], sys.argv[2].lstrip("#")
want = bytes(int(hex_color[i : i + 2], 16) for i in (0, 2, 4))
with open(path, "rb") as f:
    data = f.read()
# The header render_probe writes, exactly: magic, dimensions and depth on three lines. Split on
# those three newlines and no further -- whitespace-valued bytes are ordinary pixel values.
magic, dimensions, depth, pixels = data.split(b"\n", 3)
assert magic == b"P6" and depth == b"255", (magic, depth)
width, height = (int(value) for value in dimensions.split())
holes = sum(1 for i in range(0, width * height * 3, 3) if pixels[i : i + 3] == want)
print(f"holes {holes} of {width * height}  ({100 * holes / (width * height):.3f}%)")
PY
