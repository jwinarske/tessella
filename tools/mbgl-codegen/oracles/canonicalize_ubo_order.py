"""Sorts the `ubo` section, whose line order is an iteration order and not fixed.

The `ubos` block is a set of uniform blocks, and the probe emits them in whatever order it walked
its owners in. That order moves between captures of the same scene. Measured on 24 consecutive
`relief_style` captures, two shapes accounted for twenty of them and differed **only** in the
position of two `ubo owner:` lines -- same identity, same slot, same bytes, different place:

    11x  ... o14 ... slot=8 ... / slot=9 ...   then the rest
     9x  the rest                              then those two

So the position is not information. It takes the same treatment the drawable blocks get in
`canonicalize_drawable_index.py`, and for the same reason: sorting keeps every block and discards
only the order they were visited in.

# Why not sort the whole file, which is what this replaces

`verify_goldens.sh` used to compare a blanked-and-globally-sorted copy. That absorbed this, but it
also absorbed a line moving from one *section* to another, and it had to blank `tex=` to work at
all. Sorting one section is narrower: the section boundaries still have to match, every other
section keeps its order, and `tex=` is compared rather than discarded (tessella#285).

# What is left afterwards

The remaining four of those 24 captures differ structurally -- a z8 DEM appearing overscaled to a
different level, which moves drawables and prepare targets. That is tessella#278's race and the
verifier's `ATTEMPTS` retry is what absorbs it. With the order canonical the settled shape lands
about five times in six, so four attempts fail about once in twelve hundred runs.

    python3 tools/mbgl-codegen/oracles/canonicalize_ubo_order.py tests/golden/<dump>
"""

import sys


def canonicalize(path: str) -> int:
    with open(path, encoding="utf-8") as handle:
        lines = handle.readlines()

    out: list[str] = []
    run: list[str] = []
    moved = 0

    def flush() -> None:
        nonlocal moved
        if not run:
            return
        ordered = sorted(run)
        moved += sum(1 for before, after in zip(run, ordered) if before != after)
        out.extend(ordered)
        run.clear()

    for line in lines:
        # The `ubos N` header is not one of them and keeps its place.
        if line.startswith("ubo "):
            run.append(line)
            continue
        flush()
        out.append(line)
    flush()

    with open(path, "w", encoding="utf-8") as handle:
        handle.writelines(out)
    return moved


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: canonicalize_ubo_order.py <dump>")
    print(f"reordered {canonicalize(sys.argv[1])} ubo lines in {sys.argv[1]}")
