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
# # Why it retries
#
# The probe captures whatever frame it holds when it stops, and a frame may not have settled: a z8
# DEM tile appears overscaled to two levels in most captures of `relief_style.json` and to three in
# some, which moves its drawables, their draw order and its prepare targets. Measured at 25 of 200
# captures, in four distinct shapes; `pattern`, `image_text` and `symbol_lines` do it too at lower
# rates, since glyph and sprite atlases arrive on their own schedule (tessella#278).
#
# That is a race rather than drift, and what this script asks is whether the recipe *can* still
# produce the committed file. So a mismatch is re-captured up to `ATTEMPTS` times and passes if any
# attempt matches; one that never matches is drift and fails. Tolerating the race instead -- an
# allowlist of the affected captures -- would have gutted the check, since four of the seventeen
# are affected.
#
# Exit status is zero when every golden reproduces, byte for byte or differing *only* in `tex=`
# texture ids, which is the one field known to move with the probe's own allocation history and
# which no test reads.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
tessella=$(cd "$here/../../.." && pwd)
probe=${MBGL_PROBE:-}
if [[ -z $probe || ! -x $probe ]]; then
    echo "MBGL_PROBE must name an executable mbgl-capture-probe" >&2
    exit 2
fi

# Enough to clear a race that lands the settled frame seven times in eight, and few enough that a
# genuine drift still fails quickly.
ATTEMPTS=${ATTEMPTS:-4}

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
    # though its first line is intact. Truncation is what a killed capture leaves behind, and it is
    # the shape that reads as drift rather than as failure.
    local declared actual
    declared=$(awk '$1 == "drawables" { print $2; exit }' "$dump")
    actual=$(grep -c '^drawable ' "$dump" || true)
    if [[ -z $declared || $declared != "$actual" ]]; then
        echo "  capture is short: $dump declares ${declared:-no} drawables and holds $actual" >&2
        return 1
    fi
}

# A `tex=` id is an allocation counter that moves between runs, and some sections order their lines
# by it, so one changed id shows up as unrelated lines having moved. Neither is comparable and no
# test reads the field -- see the golden README. So the honest comparison blanks the ids and sorts,
# and only what survives that is drift.
blank() { sed 's/ tex=[0-9]\{1,\}/ tex=*/g' "$1" | sort; }

# Captures one golden and compares it, retrying a mismatch.
#
# `$1` is the dump's name, `$2` a snippet that writes `$work/$1`, and the rest the post-processing
# the recipe applies to it.
verify_one() {
    local name=$1 make=$2
    shift 2
    local post=("$@")
    local committed=$golden/$name
    if [[ ! -f $committed ]]; then
        printf '  %-28s no committed file\n' "$name"
        return 1
    fi

    local attempt lines beyond step
    for ((attempt = 1; attempt <= ATTEMPTS; attempt++)); do
        eval "$make" || return 1
        for step in "${post[@]}"; do
            eval "$step" >/dev/null
        done
        if cmp -s "$work/$name" "$committed"; then
            if ((attempt == 1)); then
                printf '  %-28s identical\n' "$name"
            else
                printf '  %-28s identical on attempt %s\n' "$name" "$attempt"
            fi
            return 0
        fi
        if diff -q <(blank "$work/$name") <(blank "$committed") >/dev/null; then
            lines=$(diff "$work/$name" "$committed" | grep -c '^[<>]' || true)
            printf '  %-28s tex= ids only (%s lines)\n' "$name" "$lines"
            return 0
        fi
    done

    lines=$(diff "$work/$name" "$committed" | grep -c '^[<>]' || true)
    beyond=$(diff <(blank "$work/$name") <(blank "$committed") | grep -c '^[<>]' || true)
    printf '  %-28s DRIFTED in %s attempts (%s lines, %s beyond tex=)\n' \
        "$name" "$ATTEMPTS" "$lines" "$beyond"
    return 1
}

status=0

# The captures whose styles name a font, a sprite sheet or a DEM by path, which the fixture spells
# as TESSELLA so the tree is relocatable.
for name in symbol scaled spaced vertical image_text; do
    sed "s|TESSELLA|$tessella|" "$styles/${name}_style.json" > "$work/$name.json"
    verify_one "${name}_style.dump" \
        "capture \"file://$work/$name.json\" \"--dump=$work/${name}_style.dump\"" \
        "python3 '$here/elide_symbol_atlas.py' '$work/${name}_style.dump'" || status=1
done

sed "s|TESSELLA|$tessella|" "$styles/pattern_style.json" > "$work/pattern.json"
verify_one pattern_style.dump \
    "capture \"file://$work/pattern.json\" \"--dump=$work/pattern_style.dump\"" \
    "python3 '$here/canonicalize_drawable_index.py' '$work/pattern_style.dump'" \
    "python3 '$here/elide_pattern_atlas.py' '$work/pattern_style.dump'" || status=1

for name in symbol_lines relief; do
    sed "s|TESSELLA|$tessella|" "$styles/${name}_style.json" > "$work/$name.json"
    verify_one "${name}_style.dump" \
        "capture \"file://$work/$name.json\" \"--dump=$work/${name}_style.dump\"" || status=1
done

# The probe's own built-in style, which reaches no network at all.
verify_one hermetic_style.dump "capture \"--dump=$work/hermetic_style.dump\"" || status=1

# Inline-GeoJSON fixtures, captured as they are.
for name in composite joins fill gradient; do
    verify_one "${name}_style.dump" \
        "capture \"file://$styles/${name}_style.json\" \"--dump=$work/${name}_style.dump\"" \
        || status=1
done

verify_one composite_style_z13_5.dump \
    "capture \"file://$styles/composite_style.json\" --zoom=13.5 \
        \"--dump=$work/composite_style_z13_5.dump\"" || status=1

verify_one circle_style.dump \
    "capture \"file://$styles/circle_style.json\" --pitch=60 \"--dump=$work/circle_style.dump\"" \
    || status=1

verify_one extrusion_style.dump \
    "capture \"file://$styles/extrusion_style.json\" \"--dump=$work/extrusion_style.dump\"" \
    "python3 '$here/canonicalize_drawable_index.py' '$work/extrusion_style.dump'" || status=1

verify_one heatmap_style.dump \
    "capture \"file://$styles/heatmap_style.json\" \"--dump=$work/heatmap_style.dump\"" \
    "python3 '$here/elide_heatmap_ramp.py' '$work/heatmap_style.dump'" || status=1

# Goldens this cannot reach, so that a clean run is not read as covering them.
for name in live_protomaps_z5.dump; do
    [[ -f $golden/$name ]] && printf '  %-28s skipped, needs the tile server\n' "$name"
done

exit "$status"
