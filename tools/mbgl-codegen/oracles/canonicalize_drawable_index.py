"""Renumbers the `#NN` in a drawable identity by what the drawable *is*.

mbgl numbers a layer's drawables in the order it happens to visit them, and that order is not
fixed. A translucent fill-extrusion emits four per tile — two shaders by two render-state sets —
and which of them is `#00` swaps between captures. Within one capture the pairing is not even
consistent across the two shaders: `sh0018` gets flags 1111 at `#00` and `sh0019` gets them at
`#01`.

So the number is not an identity. It is a position in an arbitrary iteration, which is what
`tests/golden/README.md` already says of `uboIndex` — "what it points at is compared; which slot
it happens to occupy is not" — and it takes the same treatment rather than an elision. Eliding
would throw away the drawable; renumbering keeps every one of them and discards only the order
they were visited in.

Drawables sharing an identity but for the number are sorted by what distinguishes them — the
render pass, the flags, the vertex type, the index buffer — and renumbered from zero. Every line
that names an identity is rewritten, so `attr`, `seg`, `tex` and `draw` follow their drawable.

    python3 tools/mbgl-codegen/oracles/canonicalize_drawable_index.py tests/golden/<dump>
"""

import re
import sys
from collections import defaultdict

# `L00001.S00000.t13_00004092_00002723_o13_w+000.sh0018.pk0001.v00000005#00`
IDENTITY = re.compile(r"(L\d+\.S\d+\.t\S+?\.sh\d+\.pk\d+\.v\d+)#(\d+)")


def canonicalize(path: str) -> int:
    with open(path, encoding="utf-8") as handle:
        lines = handle.readlines()

    # What each drawable is, keyed by its full identity. The `drawable` line carries everything
    # that distinguishes one from another; the rest of its lines follow it.
    described: dict[tuple[str, str], str] = {}
    for line in lines:
        if not line.startswith("drawable "):
            continue
        match = IDENTITY.search(line)
        if match:
            # Everything after the identity: pass, vtype, flags, idx, segs.
            described[(match.group(1), match.group(2))] = line[match.end() :].strip()

    # Within a group, order by description rather than by arrival.
    groups: dict[str, list[str]] = defaultdict(list)
    for base, index in described:
        groups[base].append(index)

    renumbered: dict[tuple[str, str], str] = {}
    changed = 0
    for base, indices in groups.items():
        if len(indices) < 2:
            continue
        ordered = sorted(indices, key=lambda index: (described[(base, index)], index))
        for position, index in enumerate(ordered):
            new = f"{position:02d}"
            renumbered[(base, index)] = new
            if new != index:
                changed += 1

    def rewrite(match: "re.Match[str]") -> str:
        base, index = match.group(1), match.group(2)
        return f"{base}#{renumbered.get((base, index), index)}"

    lines = [IDENTITY.sub(rewrite, line) for line in lines]

    # The blocks move as well as their labels: mbgl lists drawables in the order it visited
    # them, so the same set comes out in a different sequence. Sorting them by identity is the
    # same judgement as renumbering — the list is a set of drawables, and painter order is the
    # `draw` lines, which are compared exactly and left alone.
    out: list[str] = []
    block: list[str] | None = None
    blocks: list[list[str]] = []

    def flush() -> None:
        nonlocal block
        if block is not None:
            blocks.append(block)
            block = None

    def emit() -> None:
        out.extend(line for sorted_block in sorted(blocks) for line in sorted_block)
        blocks.clear()

    for line in lines:
        if line.startswith("drawable "):
            flush()
            block = [line]
            continue
        if block is not None:
            # A drawable's own lines are indented; anything else ends the run of them.
            if line.startswith("  "):
                block.append(line)
                continue
            flush()
            emit()
        out.append(line)
    flush()
    emit()

    with open(path, "w", encoding="utf-8") as handle:
        handle.writelines(out)
    return changed


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: canonicalize_drawable_index.py <dump>")
    print(f"renumbered {canonicalize(sys.argv[1])} identities in {sys.argv[1]}")
