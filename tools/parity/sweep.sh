#!/usr/bin/env bash
# SPDX-License-Identifier: BSD-2-Clause
#
# The visual-parity sweep: the gate a change has to hold.
#
# Berlin at street zoom, north-up and pitched, at two levels -- plus the wide low camera that a
# 1024x768 street-zoom set cannot see. Needs the tile server and asset server the scenes name:
# `serve.sh` on 8080 and `assets.py` on 8081 from maplibre-frontend/tileserver.
#
# The numbers to hold, as of 2026-09-15: 24 / 45 / 5 / 50 / 30.
set -euo pipefail
P="$(dirname "${BASH_SOURCE[0]}")"
for args in "14 1024 768 0" "14 1024 768 60" "16 1024 768 0" "16 1024 768 60"; do
  # shellcheck disable=SC2086 # four words by construction: zoom, width, height, pitch
  bash "$P/parity.sh" families_p 52.52 13.405 $args
done
bash "$P/parity.sh" families_p 52.52 13.405 9 2400 900 0
