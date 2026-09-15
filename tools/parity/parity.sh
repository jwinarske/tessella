#!/usr/bin/env bash
# SPDX-License-Identifier: BSD-2-Clause
#
# One camera, both renderers, gross pixels.
#
#   parity.sh <scene> <lat> <lon> <zoom> <width> <height> <pitch> [bearing]
#
# <scene> is a name under scenes/, or a path to a style -- which is how examples.sh hands over
# the style it composed.
#
# The oracle is rendered here rather than reused from disk. A stored oracle whose camera nobody
# wrote down is what turned a 2,029 into a 41,355 once: the image was right and the camera behind
# it was not, and nothing in the file said so.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

scene=$1
lat=$2
lon=$3
z=$4
W=$5
H=$6
pitch=$7
bearing=${8:-0}
# A path by its shape rather than by whether a file exists: a bare scene name that happened to
# match a file in the working directory would otherwise be read as a style.
case "$scene" in
  */* | *.json)
    style=$scene
    scene=$(basename "${scene%.json}")
    ;;
  *) style="$PARITY_DIR/scenes/$scene.json" ;;
esac
tag="${scene}_z${z}_p${pitch}"
# Only when set, so the sweep's frames keep the names they have always had.
[ "$bearing" = 0 ] || tag+="_b${bearing}"
[ -f "$style" ] || {
  echo "no scene at $style" >&2
  exit 1
}
[ -x "$MBGL_RENDER" ] || {
  echo "no oracle at $MBGL_RENDER (set MBGL_RENDER)" >&2
  exit 1
}

# A scene with a sibling .geojson is an annotation scene. Annotations are not a style layer --
# nothing in the stylesheet can produce one -- so the oracle takes them on the command line, and
# every PNG beside the scene is offered as an icon. `default_marker` is what an annotation with no
# icon asks for, and it aliases marker.png rather than needing a file of its own.
annot=()
if [ -f "$PARITY_DIR/scenes/$scene.geojson" ]; then
  annot+=(--annotations "$PARITY_DIR/scenes/$scene.geojson")
  for png in "$PARITY_DIR"/scenes/*.png; do
    [ -f "$png" ] || continue
    annot+=(--annotation-image "$(basename "${png%.png}")=$png")
  done
  [ -f "$PARITY_DIR/scenes/marker.png" ] && annot+=(--annotation-image "default_marker=$PARITY_DIR/scenes/marker.png")
fi

"$MBGL_RENDER" --style "$style" --output "$PARITY_WORK/o_$tag.png" \
  --lat "$lat" --lon "$lon" --zoom "$z" --width "$W" --height "$H" \
  --pitch "$pitch" --bearing "$bearing" "${annot[@]}" >/dev/null 2>&1 ||
  {
    echo "ORACLE FAILED $tag" >&2
    exit 1
  }

# The same two files to this side, through the environment rather than the command line: the
# probe's arguments are positional and a camera is what they are for.
if [ ${#annot[@]} -gt 0 ]; then
  export TSF_ANNOTATIONS="$PARITY_DIR/scenes/$scene.geojson"
  images=""
  for png in "$PARITY_DIR"/scenes/*.png; do
    [ -f "$png" ] || continue
    images="${images:+$images,}$(basename "${png%.png}")=$png"
  done
  [ -f "$PARITY_DIR/scenes/marker.png" ] && images="$images,default_marker=$PARITY_DIR/scenes/marker.png"
  export TSF_ANNOTATION_IMAGES="$images"
fi

# TSF_NO_FADES: a fade is time-dependent and the two renderers are not started at the same
# instant, so comparing mid-fade measures the clock rather than the geometry.
out=$(TSF_NO_FADES=1 "$PARITY_WORK/render_probe" "$style" "$PARITY_WORK/mat" \
  "$PARITY_WORK/t_$tag.ppm" "$lat" "$lon" "$z" "$W" "$H" "$pitch" "$bearing" 2>&1)
grep -q "materials_loaded 16" <<<"$out" || {
  echo "MATERIALS NOT LOADED $tag" >&2
  exit 1
}

printf "%-20s z%-4s p%-3s " "$scene" "$z" "$pitch"
python3 "$PARITY_DIR/gross.py" "$PARITY_WORK/t_$tag.ppm" "$PARITY_WORK/o_$tag.png"
