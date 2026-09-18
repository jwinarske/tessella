#!/usr/bin/env bash
# SPDX-License-Identifier: BSD-2-Clause
#
# The visual-parity sweep: the gate a change has to hold.
#
# Berlin at street zoom, north-up and pitched, at two levels -- plus the wide low camera that a
# 1024x768 street-zoom set cannot see. Needs the tile server and asset server the scenes name:
# `serve.sh` on 8080 and `assets.py` on 8081 from maplibre-frontend/tileserver.
#
# The annotation scene joins it north-up only, at both zooms. Not at a pitched camera, and the
# reason is the oracle rather than this side: mbgl's fill outline is a GL line whose fragment
# measures its distance from a *screen-space* varying, and a screen-space varying interpolated
# perspective-correctly -- which is all GLSL ES can do -- drifts from the truth toward the near
# end of anything running away from the camera. A polygon's vertical edge does exactly that, so
# the oracle draws the outline at the far end of it and nothing below. Held to a pitched camera
# this scene would gate a defect, and the number could only get worse by fixing something.
#
# The location indicator joins it north-up too, at two zooms and a pitched camera -- its accuracy
# circle has no outline to catch the defect above. Not with a bearing, and again the reason is the
# oracle: `prepare` puts the map bearing into the render parameters in degrees and `updateRadius`
# then calls `rad2deg` on it a second time, so a bearing of 38 turns the oracle's ring by 17.2.
# The picture barely differs -- the ring is a 72-gon and turning one is nearly itself, 17 gross
# pixels at z14 -- but it differs for a reason nothing on this side can fix.
#
# Then the terrain, which maplibre-native does not draw: a style carrying one renders there exactly
# as the same style without it. That is an oracle for exaggeration zero, where every raised path
# -- the variants, the raised clip masks, the grid, the rebuilds -- has to come out flat and
# identical, so the DEM scenes and the families scene are held against it with a terrain of no
# height. Above zero nothing can be compared, and what is held instead is that the ground is
# covered; see coverage.sh. The hillshade and color relief scenes join here too, since they read
# the same generated elevation.
#
# Those need `scenes/dem.py`. Started here when nothing is listening on its port, and stopped by
# the process this started -- never by name, because another run on the machine may be using one.
#
# Two scenes are not stable run to run. `hill_p` at z11 gave 190, 141 and 190 from one binary, and
# `terrain_flat_p` at z14 p60 has read 26, 27 and 28. The numbers below are the ones they settle on
# most often, and a run that reads one of the others is that scene rather than a change.
#
# The numbers to hold, as of 2026-09-18: 4 / 16 / 0 / 2 / 30, then 3 / 2, then 0 / 0 / 0; then
# 0 / 190 and 0 / 0 for the hillshade and relief; 0 / 28 / 0 for the flat terrain; 4 / 16 / 0 / 2
# / 30 for the families on a flat terrain, the same as without one.
#
# Then the raised cover: 0, 0, 0, 0 and 28 holes. These were 62, 34140, 622, 180367 and 618072
# when the row was first written -- see the README for the three things that were wrong and the
# order they came out in.
set -euo pipefail
P="$(dirname "${BASH_SOURCE[0]}")"
source "$P/env.sh"
for args in "14 1024 768 0" "14 1024 768 60" "16 1024 768 0" "16 1024 768 60"; do
  # shellcheck disable=SC2086 # four words by construction: zoom, width, height, pitch
  bash "$P/parity.sh" families_p 52.52 13.405 $args
done
bash "$P/parity.sh" families_p 52.52 13.405 9 2400 900 0
for args in "14 1024 768 0" "16 1024 768 0"; do
  # shellcheck disable=SC2086 # four words by construction: zoom, width, height, pitch
  bash "$P/parity.sh" annot_p 52.52 13.405 $args
done
for args in "14 1024 768 0" "16 1024 768 0" "14 1024 768 60"; do
  # shellcheck disable=SC2086 # four words by construction: zoom, width, height, pitch
  bash "$P/parity.sh" puck_p 52.52 13.405 $args
done

if ! curl -s -o /dev/null --max-time 1 "http://127.0.0.1:$PARITY_DEM_PORT/"; then
  python3 "$P/scenes/dem.py" >/dev/null 2>&1 &
  dem=$!
  trap 'kill "$dem" 2>/dev/null' EXIT
  for _ in $(seq 1 50); do
    curl -s -o /dev/null --max-time 1 "http://127.0.0.1:$PARITY_DEM_PORT/" && break
    python3 -c "import time; time.sleep(0.1)"
  done
fi

for z in 14 11; do
  bash "$P/parity.sh" hill_p 52.52 13.405 "$z" 1024 768 0
  bash "$P/parity.sh" relief_p 52.52 13.405 "$z" 1024 768 0
done
for args in "14 1024 768 0" "14 1024 768 60" "16 1024 768 60"; do
  # shellcheck disable=SC2086 # four words by construction: zoom, width, height, pitch
  bash "$P/parity.sh" terrain_flat_p 52.52 13.405 $args
done
for args in "14 1024 768 0" "14 1024 768 60" "16 1024 768 0" "16 1024 768 60"; do
  # shellcheck disable=SC2086 # four words by construction: zoom, width, height, pitch
  bash "$P/parity.sh" terrain_families_p 52.52 13.405 $args
done
bash "$P/parity.sh" terrain_families_p 52.52 13.405 9 2400 900 0
# Five cameras, not two. The first two were the gate for a while and they are the two kindest in
# the whole space: every other pitch and every zoom past the DEM's own is far worse, and holding
# only these two said the ground was covered when most of it was not. See the README.
for args in "14 0" "14 30" "14 45" "14 60" "16 45"; do
  bash "$P/coverage.sh" terrain_cover_p 52.52 13.405 "${args% *}" 1024 768 "${args#* }" ff00ff
done
