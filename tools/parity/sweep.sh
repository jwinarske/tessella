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
# The numbers to hold, as of 2026-09-15: 24 / 45 / 5 / 50 / 30, then 3 / 2.
set -euo pipefail
P="$(dirname "${BASH_SOURCE[0]}")"
for args in "14 1024 768 0" "14 1024 768 60" "16 1024 768 0" "16 1024 768 60"; do
  # shellcheck disable=SC2086 # four words by construction: zoom, width, height, pitch
  bash "$P/parity.sh" families_p 52.52 13.405 $args
done
bash "$P/parity.sh" families_p 52.52 13.405 9 2400 900 0
for args in "14 1024 768 0" "16 1024 768 0"; do
  # shellcheck disable=SC2086 # four words by construction: zoom, width, height, pitch
  bash "$P/parity.sh" annot_p 52.52 13.405 $args
done
