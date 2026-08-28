#!/usr/bin/env python3
"""Generates the DUCET collation table from the Unicode Character Database.

A style may compare two strings with a collator, and what "less than" means for text is not
codepoint order: `a` sorts before `A` and both sort before `ä`, where their codepoints run
0x41 < 0x61 < 0xE4. Getting that from a rule rather than from a table is not possible — the
order is a decision the Unicode Consortium published, not a property of the characters — so the
table is generated here for the same reason the shader tables are (DR-6): a hand-kept one is
wrong silently, and the symptom is a label list in a plausible but wrong order.

Reads `allkeys.txt`, the Default Unicode Collation Element Table. Each line maps one or more
codepoints to a sequence of collation elements, each a primary, secondary and tertiary weight:

    0061  ; [.23EC.0020.0002] # LATIN SMALL LETTER A
    00E4  ; [.23EC.0020.0002][.0000.002B.0002] # LATIN SMALL LETTER A WITH DIAERESIS

`a` and `ä` share a primary weight — they are the same letter — and differ at the secondary,
which is what makes a diacritic-insensitive comparison call them equal by ignoring that level.

The output is two tables. Most codepoints have a single element, and consecutive codepoints
very often have consecutive primaries with identical secondary and tertiary, so those are
emitted as runs. The rest are listed one by one.

Usage: collation.py <directory holding allkeys.txt> [output path]
"""

import re
import subprocess
import sys
from pathlib import Path

ELEMENT = re.compile(r"\[([.*])([0-9A-Fa-f]{4,5})\.([0-9A-Fa-f]{4,5})\.([0-9A-Fa-f]{4,5})\]")


def read_implicit(path):
    """The `@implicitweights` lines: ranges whose primaries are computed, not tabled."""
    ranges = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.startswith("@implicitweights"):
            continue
        span, _, base = line.split("#")[0].partition(";")
        first, _, last = span.split()[1].partition("..")
        ranges.append((int(first, 16), int(last, 16), int(base.strip(), 16)))
    return ranges


def read_allkeys(path):
    """Single-codepoint entries, in codepoint order, and the contractions that were skipped."""
    entries = {}
    contractions = 0
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.split("#")[0].split("%")[0].strip()
        if not line or line.startswith("@"):
            continue
        codepoints, _, weights = line.partition(";")
        codepoints = codepoints.split()
        if len(codepoints) != 1:
            # A contraction: several codepoints collating as a unit, like Danish `aa`. Skipped,
            # and counted so the omission is a number rather than a silence.
            contractions += 1
            continue
        elements = [
            (int(primary, 16), int(secondary, 16), int(tertiary, 16))
            for _, primary, secondary, tertiary in ELEMENT.findall(weights)
        ]
        if not elements:
            continue
        entries[int(codepoints[0], 16)] = elements
    return entries, contractions


def compress(entries):
    """Splits the table into runs of consecutive single-element entries and the rest."""
    runs = []
    singles = []
    for codepoint in sorted(entries):
        elements = entries[codepoint]
        if len(elements) != 1:
            singles.append((codepoint, elements))
            continue
        primary, secondary, tertiary = elements[0]
        if runs:
            start, length, first_primary, run_secondary, run_tertiary = runs[-1]
            if (
                codepoint == start + length
                and primary == first_primary + length
                and secondary == run_secondary
                and tertiary == run_tertiary
            ):
                runs[-1] = (start, length + 1, first_primary, run_secondary, run_tertiary)
                continue
        runs.append((codepoint, 1, primary, secondary, tertiary))
    return runs, singles


def emit(runs, singles, implicit, contractions, version):
    out = []
    out.append("//! DUCET collation weights, generated from `allkeys.txt`.")
    out.append("//!")
    out.append("//! Do not edit. Regenerate with `tools/unicode-codegen/collation.py`.")
    out.append("//!")
    out.append(f"//! Unicode Collation Algorithm data version {version}.")
    out.append("//!")
    out.append("//! # What is here and what is not")
    out.append("//!")
    out.append("//! Every single-codepoint entry, as weights at three levels: primary is the")
    out.append("//! letter, secondary the accent, tertiary the case. Comparing at fewer levels is")
    out.append("//! what a case- or diacritic-insensitive collator does.")
    out.append("//!")
    out.append(
        f"//! Not here: the {contractions} *contractions*, sequences of several codepoints that"
    )
    out.append("//! collate as one unit — Danish `aa` sorting as `å` is the usual example. They")
    out.append("//! need a longest-match scan over the input rather than a per-codepoint lookup,")
    out.append("//! and the comparison here does not do one, so those sequences compare as their")
    out.append("//! parts. Counted rather than silently dropped.")
    out.append("")
    out.append("/// One codepoint's weights: primary, secondary, tertiary.")
    out.append("pub type Weights = (u16, u16, u16);")
    out.append("")
    out.append("/// Consecutive codepoints whose primaries are also consecutive.")
    out.append("///")
    out.append("/// `(first codepoint, how many, first primary, secondary, tertiary)`. The nth")
    out.append("/// codepoint of a run has primary `first primary + n`, which is how the table")
    out.append(
        f"/// fits {len(runs)} entries where the file has one line each for tens of thousands."
    )
    out.append(f"pub static RUNS: [(u32, u16, u16, u16, u16); {len(runs)}] = [")
    for start, length, primary, secondary, tertiary in runs:
        out.append(f"    ({start}, {length}, {primary}, {secondary}, {tertiary}),")
    out.append("];")
    out.append("")
    out.append("/// Codepoints whose weights are more than one element, in codepoint order.")
    out.append("///")
    out.append("/// A precomposed letter is the usual case: `ä` is the primary of `a` followed by")
    out.append("/// an element carrying only the diaeresis at the secondary level.")
    out.append(f"pub static MULTI: [(u32, &[Weights]); {len(singles)}] = [")
    for codepoint, elements in singles:
        packed = ", ".join(f"({p}, {s}, {t})" for p, s, t in elements)
        out.append(f"    ({codepoint}, &[{packed}]),")
    out.append("];")
    out.append("")
    out.append("/// Ranges whose primaries the algorithm computes rather than looks up.")
    out.append("///")
    out.append("/// `(first, last, base)`, from `allkeys.txt`'s own `@implicitweights` lines. A")
    out.append("/// codepoint in one of these has primary `base + (codepoint >> 15)` followed by a")
    out.append("/// second element carrying the low bits — a script too large to table, given an")
    out.append("/// order by construction instead.")
    out.append(f"pub static IMPLICIT: [(u32, u32, u16); {len(implicit)}] = [")
    for first, last, base in implicit:
        out.append(f"    ({first}, {last}, {base}),")
    out.append("];")
    out.append("")
    return "\n".join(out)


def formatted(text):
    """Runs the generated source through rustfmt, as `tools/mbgl-codegen` does.

    CI runs `cargo fmt --check` over the committed output, so a generator that emits
    almost-formatted code dirties the tree on every regeneration: `cargo fmt` rewrites the file,
    the next run of this script writes it back, and the diff never settles. Matching rustfmt's
    heuristics by hand is a losing game, so the real thing is used.

    A missing or failing rustfmt is not fatal — the table is still correct, and `cargo fmt` will
    tidy it.
    """
    try:
        done = subprocess.run(
            ["rustfmt", "--edition", "2024", "--emit", "stdout", "--quiet"],
            input=text,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        print("note: rustfmt not found; emitting unformatted", file=sys.stderr)
        return text
    if done.returncode != 0:
        print("note: rustfmt failed; emitting unformatted", file=sys.stderr)
        return text
    return done.stdout


def main():
    if not 2 <= len(sys.argv) <= 3:
        sys.exit(f"usage: {sys.argv[0]} <directory holding allkeys.txt> [output]")
    ucd = Path(sys.argv[1])
    path = ucd / "allkeys.txt"
    version = "unknown"
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith("@version"):
            version = line.split()[1]
            break

    entries, contractions = read_allkeys(path)
    implicit = read_implicit(path)
    runs, singles = compress(entries)
    text = emit(runs, singles, implicit, contractions, version)

    destination = (
        Path(sys.argv[2])
        if len(sys.argv) == 3
        else Path("crates/tessella-style/src/generated/collation.rs")
    )
    destination.write_text(formatted(text), encoding="utf-8")
    print(
        f"wrote {destination}: {len(runs)} runs, {len(singles)} multi-element entries, "
        f"{contractions} contractions skipped"
    )


if __name__ == "__main__":
    main()
