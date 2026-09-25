# SPDX-License-Identifier: BSD-2-Clause
"""Which way a residual leans: the signature that separates a rasterization floor from a defect.

`gross.py` says how many pixels differ. It cannot say whether that is worth chasing, and most of
what is left in this suite is not: two of mbgl's render paths are chosen by a compile-time backend
macro and disagree with each other, so a fill outline and a circle edge each have a floor this side
cannot reach. Counting is the same either way.

What tells them apart is **which way the difference leans**. Take the pixels that differ and count
how many are brighter here than in the oracle:

| brighter | reading |
|---|---|
| near 50% | symmetric jitter -- the antialiasing class, and finished |
| near 0% or 100% | systematic, and worth a look |

An antialiased edge rounds each way about equally often. A surface drawn twice, or once too few, is
wrong in one direction at every pixel. tessella#246's dropped oneway arrows and #250's round joins
both read 100%; tessella#297 -- a translucent fill-extrusion composited once per tile that carries
it -- read 0.0% beside an `animate-a-point` that read 44.5% and was left alone.

**One-sided does not mean defect.** The circle-edge floor is one-sided too, by construction: the
oracle's edge pixel is a little more covered at every circle, so `filter-within-a-layer` reads 92.8%
and is finished. What the tool separates is systematic from symmetric; deciding which systematic
thing it is still means reading the golden README's list of floors. A 50% answer is the one that
closes a question outright.

    python3 tools/parity/lean.py <tag>...          # names under $PARITY_WORK
    python3 tools/parity/lean.py --pair <a> <b>    # two images directly

A tag is what `parity.sh` writes its pair as -- `families_p_z16_p60`, say -- and every `o_<tag>.png`
it matches is compared against the `t_<tag>.ppm` beside it. Matching by tag rather than by globbing
the slug is deliberate: an example with variants leaves `o_<slug>__noaa_...` next to `o_<slug>_...`,
and pairing those across each other reported 140,994 gross where the suite had said 377.
"""

import os
import sys

from gross import over_black

THRESHOLD = 48


def lean(oracle: str, ours: str) -> tuple[int, float]:
    """How many pixels differ, and what fraction of them are brighter in `ours`."""
    a = over_black(oracle)
    b = over_black(ours)
    if len(a) != len(b):
        raise SystemExit(f"different sizes: {len(a) // 3} against {len(b) // 3} pixels")

    differing = brighter = 0
    for i in range(0, len(a), 3):
        if (
            max(abs(a[i] - b[i]), abs(a[i + 1] - b[i + 1]), abs(a[i + 2] - b[i + 2]))
            > THRESHOLD
        ):
            differing += 1
            if b[i] + b[i + 1] + b[i + 2] > a[i] + a[i + 1] + a[i + 2]:
                brighter += 1
    return differing, (100.0 * brighter / differing if differing else 0.0)


# Below this many differing pixels the fraction is noise: a handful of pixels is one-sided about
# as often as a coin lands heads twice, and reading a verdict off it invents a defect. The floors
# this tool exists to dismiss are hundreds of pixels; so are the defects it has found.
READABLE = 20


def verdict(differing: int, percent: float) -> str:
    """The reading, so the number does not have to be interpreted from memory each time."""
    if differing < READABLE:
        return f"too few to read -- under {READABLE} pixels the lean means nothing"
    if percent <= 15.0 or percent >= 85.0:
        return "one-sided -- systematic; check the known floors before opening"
    if 35.0 <= percent <= 65.0:
        return "jitter -- the antialiasing class"
    return "mixed"


def report(oracle: str, ours: str, label: str) -> None:
    differing, percent = lean(oracle, ours)
    if differing == 0:
        print(f"{label:52s} gross=    0")
        return
    print(f"{label:52s} gross={differing:5d}  ours brighter {percent:5.1f}%  {verdict(differing, percent)}")


def main() -> None:
    args = sys.argv[1:]
    if not args:
        raise SystemExit("usage: lean.py <tag>... | --pair <oracle> <ours>")

    if args[0] == "--pair":
        if len(args) != 3:
            raise SystemExit("usage: lean.py --pair <oracle> <ours>")
        report(args[1], args[2], os.path.basename(args[1]))
        return

    work = os.environ.get("PARITY_WORK")
    if not work:
        raise SystemExit("PARITY_WORK is not set; it is where parity.sh writes its pairs")
    for tag in args:
        oracle = os.path.join(work, f"o_{tag}.png")
        ours = os.path.join(work, f"t_{tag}.ppm")
        if not os.path.exists(oracle) or not os.path.exists(ours):
            print(f"{tag:52s} no pair in {work}")
            continue
        report(oracle, ours, tag)


if __name__ == "__main__":
    main()
