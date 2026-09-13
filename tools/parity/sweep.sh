#!/usr/bin/env bash
# The visual-parity sweep: the gate a change has to hold.
#
# Berlin at street zoom, north-up and pitched, at two levels -- plus the wide low camera that a
# 1024x768 street-zoom set cannot see. Needs the tile server and asset server the scenes name:
# `serve.sh` on 8080 and `assets.py` on 8081 from maplibre-frontend/tileserver.
#
# The numbers to hold, as of 2026-09-13: 24 / 45 / 5 / 52 / 30.
set -euo pipefail
P="$(dirname "${BASH_SOURCE[0]}")"
for args in "14 1024 768 0" "14 1024 768 60" "16 1024 768 0" "16 1024 768 60"; do
  bash "$P/parity.sh" families_p 52.52 13.405 $args
done
bash "$P/parity.sh" families_p 52.52 13.405 9 2400 900 0
