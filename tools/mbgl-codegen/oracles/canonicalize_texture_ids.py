"""Renumbers the `tex=` in a texture binding by which bindings share the texture.

A `tex=` id is the probe's allocation counter over its whole run, so it moves between captures of
the same scene. Three consecutive `relief_style` captures:

    r1: tex=34 tex=49 tex=5726 tex=25 tex=26 tex=47
    r2: tex=48 tex=34 tex=5708 tex=24 tex=25 tex=46
    r3: tex=49 tex=33 tex=5777 tex=24 tex=25 tex=47

Nothing else in those captures differs. The id is not an identity either -- a `texture` line
carries a size, a format and a hash but no id, so the dump never says which `texture` a `tex=`
points at. The only comparable thing in the field is **which bindings share a texture**, and that
is what this keeps: a first-class renumber preserves the partition and discards the allocator's
history. Eliding would throw the partition away with it, which is why `dash_atlases.rs` can assert
that three line widths bind one atlas and a fourth layer binds its own.

`verify_goldens.sh` used to blank the field and sort before comparing. That is weaker than this:
blanking hides a real change to a `tex=` line, and the sort it needs hides lines having moved.
With the ids canonical both go away and the comparison is byte equality (tessella#285).

# The numbering does not depend on line order

Not first appearance, which would inherit whatever order mbgl visited drawables in. Each distinct
id is keyed by the sorted set of `(identity, slot)` bindings that name it, and the ids are numbered
by that key -- so the same partition produces the same numbering however the lines are arranged.

Run *after* `canonicalize_drawable_index.py` where both apply: that one rewrites and reorders the
identities this keys on.

    python3 tools/mbgl-codegen/oracles/canonicalize_texture_ids.py tests/golden/<dump>
"""

import re
import sys
from collections import defaultdict

# `  tex L00002.S00000.t13_...#00 slot=0 tex=34`
BINDING = re.compile(r"^\s*tex\s+(\S+)\s+slot=(\d+)\s+tex=(\d+)\s*$")
FIELD = re.compile(r"tex=(\d+)")


def canonicalize(path: str) -> int:
    with open(path, encoding="utf-8") as handle:
        lines = handle.readlines()

    # Every binding each raw id is named by, which is the whole of what the field says.
    contexts: dict[str, set[tuple[str, str]]] = defaultdict(set)
    for line in lines:
        match = BINDING.match(line)
        if match:
            identity, slot, raw = match.groups()
            contexts[raw].add((identity, slot))

    if not contexts:
        return 0

    # Ordered by what names them rather than by their value or their position.
    ordered = sorted(contexts, key=lambda raw: sorted(contexts[raw]))
    renumbered = {raw: str(position) for position, raw in enumerate(ordered)}

    changed = sum(1 for raw, new in renumbered.items() if raw != new)

    out = []
    for line in lines:
        if BINDING.match(line):
            line = FIELD.sub(lambda m: f"tex={renumbered[m.group(1)]}", line)
        out.append(line)

    with open(path, "w", encoding="utf-8") as handle:
        handle.writelines(out)
    return changed


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: canonicalize_texture_ids.py <dump>")
    print(f"renumbered {canonicalize(sys.argv[1])} texture ids in {sys.argv[1]}")
