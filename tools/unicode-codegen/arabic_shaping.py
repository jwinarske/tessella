#!/usr/bin/env python3
"""Generates the Arabic contextual shaping table from the Unicode Character Database.

ICU's ``u_shapeArabic`` with ``U_SHAPE_LETTERS_SHAPE`` selects one of four *presentation forms*
for each Arabic letter from its joining context. Both halves of what that needs are in the UCD
and neither is in the code:

  * ``ArabicShaping.txt`` gives each character's joining type — dual, right, left, causing,
    transparent or non-joining — which decides whether a neighbour may join to it.
  * ``UnicodeData.txt`` gives the forms themselves, as ``<isolated>``/``<initial>``/
    ``<medial>``/``<final>`` decompositions of the Arabic Presentation Forms-B block.

Writing the table by hand would be forty-odd letters times four forms recalled rather than read,
which is the failure mode this whole codegen directory exists against (DR-6). Regenerate with:

    python3 tools/unicode-codegen/arabic_shaping.py <ucd-dir> > \\
        crates/tessella-glyph/src/generated/arabic.rs
"""

import re
import sys
from pathlib import Path

FORMS = ["isolated", "final", "initial", "medial"]


def read_categories(path):
    """Code point -> General_Category, from UnicodeData.txt."""
    out = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        fields = line.split(";")
        if len(fields) > 2:
            out[int(fields[0], 16)] = fields[2]
    return out


def read_joining(path):
    """Code point -> joining type letter, from ArabicShaping.txt.

    Only the *listed* characters. Unicode derives the rest, and the derivation is load-bearing
    here: every Arabic diacritic is Transparent and **none of them is in this file** — the whole
    block lists five T entries. A generator that took this file as the complete answer marks no
    mark transparent, and a diacritic then breaks the join around it, which unjoins every voweled
    word while leaving unvoweled text perfect.
    """
    out = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        fields = [field.strip() for field in line.split(";")]
        if len(fields) < 3:
            continue
        out[int(fields[0], 16)] = fields[2]
    return out


def read_forms(path):
    """Base code point -> {form name: presentation code point}, from UnicodeData.txt."""
    pattern = re.compile(r"<(isolated|final|initial|medial)>\s+([0-9A-F]{4,6})$")
    out = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        fields = line.split(";")
        if len(fields) < 6:
            continue
        match = pattern.match(fields[5].strip())
        if not match:
            continue
        form, base = match.group(1), int(match.group(2), 16)
        out.setdefault(base, {})[form] = int(fields[0], 16)
    return out


def main():
    if len(sys.argv) != 2:
        sys.exit(f"usage: {sys.argv[0]} <directory holding the UCD text files>")
    ucd = Path(sys.argv[1])
    kinds = {"D": "Dual", "R": "Right", "L": "Left", "C": "Causing", "T": "Transparent", "U": "None"}
    joining = read_joining(ucd / "ArabicShaping.txt")
    forms = read_forms(ucd / "UnicodeData.txt")
    categories = read_categories(ucd / "UnicodeData.txt")

    # The derivation: unlisted characters are Non_Joining, except combining marks and format
    # characters, which are Transparent. `Mn` is every Arabic vowel and `Cf` is the zero-width
    # joiner and its kin, which exist precisely to be joined through.
    for code, category in categories.items():
        if code not in joining and category in ("Mn", "Me", "Cf"):
            joining[code] = "T"

    # Only letters that have forms *and* a joining type: the block also carries ligatures, whose
    # decompositions name two code points and which this table does not describe.
    letters = sorted(base for base in forms if base in joining)

    rows = []
    for base in letters:
        entry = forms[base]
        # A right-joining letter has no initial or medial form; the isolated and final ones stand
        # in, which is what ICU's table holds and what keeps the lookup a plain index.
        isolated = entry.get("isolated", base)
        final = entry.get("final", isolated)
        initial = entry.get("initial", isolated)
        medial = entry.get("medial", final)
        rows.append((base, joining[base], isolated, final, initial, medial))

    print("//! Arabic contextual shaping, generated from the Unicode Character Database.")
    print("//!")
    print("//! Do not edit. See `tools/unicode-codegen/arabic_shaping.py`.")
    print("//!")
    print("//! Each row is a letter, its joining type, and its four presentation forms in the")
    print("//! order isolated, final, initial, medial. A letter with no initial or medial form —")
    print("//! every right-joining one — repeats its isolated and final, which is what keeps the")
    print("//! lookup a plain index rather than a branch.")
    print()
    print("/// How a character joins to its neighbours.")
    print("///")
    print("/// Unicode's joining types. `Transparent` is the one that carries the algorithm: a")
    print("/// diacritic sits *between* two letters without breaking their join, so the context a")
    print("/// letter sees has to look past any number of them.")
    print("#[derive(Debug, Clone, Copy, PartialEq, Eq)]")
    print("pub enum Joining {")
    print("    /// Joins on both sides.")
    print("    Dual,")
    print("    /// Joins only to the letter before it.")
    print("    Right,")
    print("    /// Joins only to the letter after it.")
    print("    Left,")
    print("    /// Joins nothing itself but lets its neighbours join through it.")
    print("    Causing,")
    print("    /// Invisible to joining: a diacritic.")
    print("    Transparent,")
    print("    /// Joins nothing.")
    print("    None,")
    print("}")
    print()
    print("/// One letter's joining type and its four forms.")
    print("#[derive(Debug, Clone, Copy)]")
    print("pub struct Letter {")
    print("    /// The base code point, as text stores it.")
    print("    pub base: u32,")
    print("    /// How it joins.")
    print("    pub joining: Joining,")
    print("    /// Isolated, final, initial, medial.")
    print("    pub forms: [u32; 4],")
    print("}")
    print()
    print(f"/// Every Arabic letter with presentation forms: {len(rows)} of them.")
    print("///")
    print("/// Sorted by code point, so a lookup is a binary search.")
    print(f"pub static LETTERS: [Letter; {len(rows)}] = [")
    for base, joining_type, isolated, final, initial, medial in rows:
        kind = kinds.get(joining_type, "None")
        print(
            f"    Letter {{ base: 0x{base:04X}, joining: Joining::{kind}, "
            f"forms: [0x{isolated:04X}, 0x{final:04X}, 0x{initial:04X}, 0x{medial:04X}] }},"
        )
    print("];")

    # Lam-alef: the ligatures ICU substitutes when a lam is followed by an alef. Their
    # decompositions name two code points, so they are gathered separately from the table above.
    #
    # Bounded to Presentation Forms-B. `<isolated> 0644 ...` also matches lam-jeem, lam-hah and
    # the rest of the Forms-A ligatures, which `U_SHAPE_LETTERS_SHAPE` does *not* substitute —
    # taking them would draw ligatures ICU leaves as two letters.
    ligatures = []
    pattern = re.compile(r"<(isolated|final)>\s+0644\s+([0-9A-F]{4,6})$")
    for line in (ucd / "UnicodeData.txt").read_text(encoding="utf-8").splitlines():
        fields = line.split(";")
        if len(fields) < 6:
            continue
        match = pattern.match(fields[5].strip())
        if match:
            code = int(fields[0], 16)
            if 0xFE70 <= code <= 0xFEFF:
                ligatures.append((int(match.group(2), 16), match.group(1), code))

    pairs = {}
    for alef, form, code in ligatures:
        pairs.setdefault(alef, {})[form] = code
    # The joining types, as ranges over every code point that has one — which is not the same set
    # as the letters above. A diacritic is Transparent and has *no* presentation forms of its own:
    # `FE76 ARABIC FATHA ISOLATED FORM` decomposes to a space and a fatha, two code points, so it
    # is rightly absent from a forms table. Conflating the two questions is what leaves every mark
    # non-joining and unjoins any voweled word.
    ranges = []
    for code in sorted(joining):
        kind = kinds.get(joining[code], "None")
        if ranges and ranges[-1][1] + 1 == code and ranges[-1][2] == kind:
            ranges[-1][1] = code
        else:
            ranges.append([code, code, kind])

    print()
    print("/// Every code point with a joining type, as ranges.")
    print("///")
    print("/// A superset of the letters above: `Transparent` covers every combining mark and")
    print("/// format character, none of which has presentation forms. Unicode lists only five of")
    print("/// them explicitly and *derives* the rest from General_Category, which is why this is")
    print("/// generated from two files rather than one.")
    print("///")
    print("/// Sorted and disjoint, so a lookup is a binary search on the start.")
    print(f"pub static JOINING: [(u32, u32, Joining); {len(ranges)}] = [")
    for start, end, kind in ranges:
        print(f"    (0x{start:04X}, 0x{end:04X}, Joining::{kind}),")
    print("];")

    print()
    print("/// Lam followed by alef is drawn as one ligature, not two letters.")
    print("///")
    print("/// Each row is the alef, then the ligature's isolated and final forms. Which of the")
    print("/// two is used depends on what precedes the *lam*, so the pair is chosen the way a")
    print("/// single letter's form is.")
    print(f"pub static LAM_ALEF: [(u32, [u32; 2]); {len(pairs)}] = [")
    for alef in sorted(pairs):
        entry = pairs[alef]
        isolated = entry.get("isolated", 0)
        final = entry.get("final", isolated)
        print(f"    (0x{alef:04X}, [0x{isolated:04X}, 0x{final:04X}]),")
    print("];")


if __name__ == "__main__":
    main()
