"""Elides the two hashes a symbol capture cannot reproduce.

mbgl packs the glyph atlas in the order glyphs arrive, and that order is not deterministic:
over ten consecutive captures of the same style, the symbol vertex hashes and the atlas texture
hash each took four or five distinct values, one dominating. Nothing else in the dump moved --
seven lines of eighty-seven.

The vertex hashes follow the atlas because the `data` attribute carries each glyph's texture
coordinates, so a different packing is different vertex bytes for identical geometry.

Committing a dump with those lines in it would mean every regeneration produced a spurious diff,
which is exactly what tests/golden/README.md says a diff must never mean. They are elided the
same way `symbol_fade_change` already is.

    python3 tools/mbgl-codegen/oracles/elide_symbol_atlas.py tests/golden/symbol_style.dump
"""

import re
import sys

MARK = "---------------- (atlas order varies)"


# Only the interleaved layout buffer -- attributes 0, 1 and 2 share it, and it is the one
# carrying each glyph's texture coordinates. Attributes 3 and 4 are the dynamic position and
# fade opacity, in buffers of their own; both were byte-identical across all ten captures, so
# they stay in the golden.
INTERLEAVED = ("id=0 ", "id=1 ", "id=2 ")


def elide(text: str) -> tuple[str, int]:
    out, count = [], 0
    for line in text.split("\n"):
        if (
            line.startswith("  attr ")
            and "sh0033" in line
            and "src=" in line
            and any(attr in line for attr in INTERLEAVED)
        ):
            line, n = re.subn(r"src=(\d+):[0-9a-f]{16}", rf"src=\1:{MARK}", line)
            count += n
        elif line.startswith("texture 512x512"):
            line, n = re.subn(r"hash=[0-9a-f]{16}", f"hash={MARK}", line)
            count += n
        out.append(line)
    return "\n".join(out), count


if __name__ == "__main__":
    path = sys.argv[1]
    text = open(path).read()
    elided, count = elide(text)
    open(path, "w").write(elided)
    print(f"elided {count} lines in {path}")
