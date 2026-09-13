# Where the pieces of a parity run live, and what to set when they live elsewhere.
#
# Sourced by the scripts here. Every path is an override, so a checkout laid out differently
# only has to say which parts moved.
#
#   MBGL_RENDER        the oracle. maplibre-native's own renderer, built with `-DMLN_WITH_...`;
#                      the comparison is meaningless without it and the scripts refuse to run.
#   TESSELLA_FLUORITE  the Filament consumer, which holds render_probe and the materials.
#   FILAMENT_STAGING   the Filament build the consumer links and whose matc compiles materials.
#   PARITY_WORK        scratch for probes, materials and rendered frames. Not in the tree: these
#                      are build outputs and one run's frames are megabytes.
: "${MBGL_RENDER:=/mnt/dev/maplibre-frontend/maplibre-native/build-capture/bin/mbgl-render}"
: "${TESSELLA_FLUORITE:=/mnt/dev/tessella_fluorite}"
: "${FILAMENT_STAGING:=/mnt/dev/maplibre-frontend/filament/build/release/staging}"
: "${PARITY_WORK:=${TMPDIR:-/tmp}/tessella-parity}"

PARITY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TESSELLA_DIR="$(cd "$PARITY_DIR/../.." && pwd)"
export MBGL_RENDER TESSELLA_FLUORITE FILAMENT_STAGING PARITY_WORK PARITY_DIR TESSELLA_DIR
mkdir -p "$PARITY_WORK"
