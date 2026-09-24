#!/usr/bin/env bash
# SPDX-License-Identifier: BSD-2-Clause
#
# Regenerates every committed golden dump and diffs it against the file in the tree.
#
#   MBGL_PROBE=<maplibre-native>/build-capture/mbgl-capture-probe verify_goldens.sh
#
# The point is that `tests/golden/README.md`'s recipe is prose, and prose drifts. A capture whose
# post-processing step was never run looks fine -- the tests read counts and pass either way -- and
# is only found by diffing the file against a fresh capture. That is how tessella#270 was found,
# by accident, while a determinism control fired during unrelated work. This is that check on
# purpose.
#
# Exit status is zero when every golden either reproduces byte for byte or differs *only* in
# `tex=` texture ids, which is the one field known to move with the probe's own allocation history
# and which no test reads. Anything else is a real drift and fails.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
tessella=$(cd "$here/../../.." && pwd)
probe=${MBGL_PROBE:-}
if [[ -z $probe || ! -x $probe ]]; then
    echo "MBGL_PROBE must name an executable mbgl-capture-probe" >&2
    exit 2
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

styles=$tessella/crates/tessella-style/tests
golden=$tessella/tests/golden

# A capture that failed or was cut short must say so. Without this check a probe that timed out
# leaves a short dump behind and the comparison below reports it as drift, which sends the reader
# looking for a change in the frontend that never happened.
capture() {
    local dump=${*: -1}
    dump=${dump#--dump=}
    if ! timeout 300 "$probe" "$@" >/dev/null 2>&1; then
        echo "  capture failed: $*" >&2
        return 1
    fi
    if [[ ! -s $dump ]]; then
        echo "  capture wrote nothing: $dump" >&2
        return 1
    fi
    if ! head -1 "$dump" | grep -q '^tessella-capture-dump'; then
        echo "  capture is not a dump: $dump" >&2
        return 1
    fi
    # A dump declares how many drawables it holds, so a file cut off partway is detectable even
    # though its first line is intact. Truncation is what a killed capture leaves behind, and it
    # is the shape that reads as drift rather than as failure.
    local declared actual
    declared=$(awk '$1 == "drawables" { print $2; exit }' "$dump")
    actual=$(grep -c '^drawable ' "$dump" || true)
    if [[ -z $declared || $declared != "$actual" ]]; then
        echo "  capture is short: $dump declares ${declared:-no} drawables and holds $actual" >&2
        return 1
    fi
}

# The captures whose styles name a font, a sprite sheet or a DEM by path, which the fixture spells
# as TESSELLA so the tree is relocatable.
for name in symbol scaled spaced vertical image_text pattern symbol_lines relief; do
    sed "s|TESSELLA|$tessella|" "$styles/${name}_style.json" > "$work/$name.json"
    capture "file://$work/$name.json" "--dump=$work/${name}_style.dump" || exit 1
done

# The probe's own built-in style, which reaches no network at all.
capture "--dump=$work/hermetic_style.dump" || exit 1

# Inline-GeoJSON fixtures, captured as they are.
for name in composite joins fill extrusion heatmap gradient; do
    capture "file://$styles/${name}_style.json" "--dump=$work/${name}_style.dump" || exit 1
done
capture "file://$styles/composite_style.json" --zoom=13.5 \
    "--dump=$work/composite_style_z13_5.dump" || exit 1
capture "file://$styles/circle_style.json" --pitch=60 "--dump=$work/circle_style.dump" || exit 1

# The documented post-processing. A capture that skips its step is exactly what this looks for, so
# the list has to match the README's recipe.
for name in symbol scaled spaced vertical image_text; do
    python3 "$here/elide_symbol_atlas.py" "$work/${name}_style.dump" >/dev/null
done
python3 "$here/canonicalize_drawable_index.py" "$work/pattern_style.dump" >/dev/null
python3 "$here/elide_pattern_atlas.py" "$work/pattern_style.dump" >/dev/null
python3 "$here/elide_heatmap_ramp.py" "$work/heatmap_style.dump" >/dev/null
python3 "$here/canonicalize_drawable_index.py" "$work/extrusion_style.dump" >/dev/null

status=0
for fresh in "$work"/*_style*.dump; do
    name=$(basename "$fresh")
    committed=$golden/$name
    if [[ ! -f $committed ]]; then
        printf '  %-28s no committed file\n' "$name"
        status=1
        continue
    fi
    if cmp -s "$fresh" "$committed"; then
        printf '  %-28s identical\n' "$name"
        continue
    fi
    # Id noise or real drift? A `tex=` id is an allocation counter that moves between runs, and
    # some sections order their lines by it, so one changed id shows up as unrelated lines having
    # moved. Neither is comparable and no test reads the field -- see the golden README. So the
    # honest comparison blanks the ids and sorts, and only what survives that is drift.
    lines=$(diff "$fresh" "$committed" | grep -c '^[<>]' || true)
    blank() { sed 's/ tex=[0-9]\{1,\}/ tex=*/g' "$1" | sort; }
    if diff -q <(blank "$fresh") <(blank "$committed") >/dev/null; then
        printf '  %-28s tex= ids only (%s lines)\n' "$name" "$lines"
    else
        beyond=$(diff <(blank "$fresh") <(blank "$committed") | grep -c '^[<>]' || true)
        printf '  %-28s DRIFTED (%s lines, %s beyond tex=)\n' "$name" "$lines" "$beyond"
        status=1
    fi
done

# Goldens this cannot reach, so that a clean run is not read as covering them.
for name in live_protomaps_z5.dump; do
    [[ -f $golden/$name ]] && printf '  %-28s skipped, needs the tile server\n' "$name"
done

exit $status
