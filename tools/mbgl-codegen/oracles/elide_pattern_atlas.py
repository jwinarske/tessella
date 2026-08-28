"""Elides the lines a pattern capture cannot reproduce.

mbgl builds an *image atlas per tile* holding only the sprites that tile needs, and packs it in
the order the images arrive. That order is not deterministic, exactly as the glyph atlas's is
not -- see `elide_symbol_atlas.py`, which this is the pattern counterpart of. Over three
consecutive captures of the identical style, four or five lines of two hundred and fifty-two
moved and nothing else did.

What moves is the atlas texture's hash and, following from it, the pattern rectangle each
pattern UBO carries: a rectangle names where in the atlas an image was packed, so a different
packing is a different rectangle for the same sprite. The rectangle's *size* does not move --
`sand_noise` is fifty by fifty in every capture -- only its origin.

The texture *count* moves too. Some runs emit an extra 1x1 placeholder of a second format and
some do not, so the `textures N` header varies between three and four.

The 1x1 placeholder textures are dropped for the same reason: some runs emit one and some two.

What does not move, and is what the golden is for: all thirty-six drawables and their
identities, the pattern shaders each layer resolved to (background 4, fill 13, fill outline 14,
line 27), the texture slot each binds, every attribute descriptor, every segment, the index
buffers, the painter order, the camera and the tile matrices.

    python3 tools/mbgl-codegen/oracles/elide_pattern_atlas.py tests/golden/pattern_style.dump
"""

import re
import sys

MARK = "---------------- (atlas order varies)"

# The pattern-position buffers, per layer. A fill's slot 4 and a line's slot 3 are the ones
# carrying the atlas rectangles; every other slot of the same layers was byte-identical across
# the captures and stays in the golden.
#
# The rule is what was *observed* to move, not what could. The background's slot 5 carries
# rectangles too and has held still across every capture taken, so it stays — eliding a
# comparison that holds gives away a check for nothing. The data-driven layer's per-vertex
# buffers were in that category until a capture showed them moving, which is what the rule is
# for: they are elided now and the background is not, on evidence rather than on symmetry.
PATTERN_SLOTS = {
    ("layer:1", "slot=4"),
    ("layer:2", "slot=4"),
    ("layer:3", "slot=3"),
    # The extrusion layer's, which carries rectangles like a fill's and moves like one.
    ("layer:5", "slot=4"),
    # And the data-driven line layer's, which is a line's slot 3.
    ("layer:6", "slot=3"),
}


def elide(path: str) -> int:
    with open(path, encoding="utf-8") as handle:
        lines = handle.readlines()

    changed = 0
    out = []
    for line in lines:
        stripped = line.rstrip("\n")

        # The image atlas itself. The 1x1 placeholders and the 64x64 sheet are stable; only the
        # 512x512 atlas is packed per capture.
        if stripped.startswith("texture 512x512"):
            out.append(re.sub(r"hash=[0-9a-f]+", f"hash={MARK}", stripped) + "\n")
            changed += 1
            continue

        # The count varies with the placeholders below, so the number is elided and the line
        # kept -- a capture that emitted no textures at all should still fail.
        if stripped.startswith("textures "):
            out.append(f"textures {MARK}\n")
            changed += 1
            continue

        # A 1x1 placeholder is a texture slot filled so a sampler has something to read, and
        # how many of them a capture emits varies: some runs carry one and some two, differing
        # in format. They say nothing about the pattern, so they are dropped rather than
        # elided -- eliding would leave a line whose presence still varied.
        if stripped.startswith("texture 1x1 "):
            changed += 1
            continue

        # A data-driven pattern's rectangles travel per *vertex* as well as in a uniform, and
        # they follow the packing exactly as the uniforms do. Ids 4 and 5 are a fill's
        # `idFillPatternFrom/ToVertexAttribute`; 9 and 10 are a line's, which sit at different
        # slots because the line shader has already spent its low bindings. Everything else about
        # the descriptor — the binding, the type, the stride, the length — is a property of the
        # shader and stays.
        #
        # The line pair was missed when the data-driven line layer was added, and the golden
        # committed with a hash that varies between *processes*: three captures in one session
        # agreed, and a session later the value had moved. A committed dump that disagrees with
        # its own regeneration is the thing this whole script exists to prevent.
        #
        # Indented, unlike every other line this touches, so the comparison is against the
        # trimmed text and the rewrite keeps the indentation the dump gave it.
        body = stripped.lstrip()
        if body.startswith("attr ") and any(
            f" id={which} " in body for which in (4, 5, 9, 10)
        ):
            # Both hashes, unlike the symbol case where only one attribute of the three follows
            # the atlas. Here the attribute *is* the atlas rectangles, so its own bytes move
            # exactly as the buffer's do and keeping `fld=` would keep the nondeterminism under
            # a different name.
            line = re.sub(r"src=(\d+):[0-9a-f]+", rf"src=\1:{MARK}", stripped)
            out.append(re.sub(r"fld=[0-9a-f]+", f"fld={MARK}", line) + "\n")
            changed += 1
            continue

        if stripped.startswith("ubo "):
            fields = stripped.split()
            if len(fields) > 2 and (fields[1], fields[2]) in PATTERN_SLOTS:
                # The size stays: it is a property of the shader's block, not of the packing,
                # and a wrong one is a real difference.
                size = next((f for f in fields if f.startswith("size=")), "size=?")
                out.append(f"ubo {fields[1]} {fields[2]} {size} bytes={MARK}\n")
                changed += 1
                continue

        out.append(line)

    with open(path, "w", encoding="utf-8") as handle:
        handle.writelines(out)
    return changed


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: elide_pattern_atlas.py <dump>")
    count = elide(sys.argv[1])
    print(f"elided {count} lines in {sys.argv[1]}")
