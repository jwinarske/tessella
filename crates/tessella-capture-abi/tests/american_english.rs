// SPDX-License-Identifier: BSD-2-Clause
//! Everything in the workspace a reader or a caller sees is written in American English.
//!
//! A stated convention, and one that drifts silently: nothing about "colour" beside "color" fails
//! to compile, and a doc comment is as much a part of the interface as the name it belongs to.
//! Sixty-one of these had accumulated before anything read them back -- in comments, in two inner
//! function names, and in one line of `include/tessella_capture_abi.h`, which is the text a
//! consumer copies into its own tree.
//!
//! Beside `license_headers.rs` because that is where the workspace-wide checks live: this crate is
//! the ABI's own, but neither test is about the ABI, and a reader looking for either finds both.
//!
//! # How the words are chosen
//!
//! By spelling rather than by a dictionary, because the list of words this workspace actually uses
//! is short and a dictionary would need a list anyway. The entries are the *families* -- -our, -ise
//! and -isation, -re, the doubled -lled, -ogue, the -ce nouns, and the one-offs -- rather than the
//! words this tree happened to get wrong. A word earns its place by belonging to a family, which is
//! what makes the check hold for prose nobody has written yet; a list grown one correction at a
//! time is green until the next word outside it arrives.
//!
//! The search is unanchored, which is what [`ISE`]'s three endings and the two omissions below are
//! for: "recolors" has to be found as surely as "colors", so an entry is a substring and any
//! American word containing it is a false failure.
//!
//! Two words belong to a family and still cannot be searched for. "kerb" is inside
//! **checkerboard**, and "programme" is inside **programmer** and **programmed**; both American
//! words are in this tree. A substring search has no way to tell them from the British spelling, so
//! the words are left out rather than exempted file by file. "grey" has the same shape -- it is
//! inside "greyhound" -- and stays, because the color appears in this tree and the dog does not.
//!
//! # What this does not read
//!
//! **`vendor/`.** Third-party source, which keeps upstream's prose along with upstream's terms.
//!
//! **Fixtures.** `.json`, `.txt`, `.mvt` and `.dump` are inputs and expected outputs, not prose. A
//! style fixture copied from MapLibre's examples names a `Theatre` in four files, and that is a
//! place name in the data rather than a spelling this workspace chose.
//!
//! **`LICENSE`.** Not ours to edit.

use std::path::{Path, PathBuf};

/// British verbs in -ise, as stems, each searched for with the three endings the British verb takes.
///
/// `-ise` (which covers `-ised` and `-ises` too), `-ising`, and `-isation`. Not the bare stem,
/// because the bare stem is inside the American `-ism` and `-ist` nouns: "criticism", "optimism",
/// "organism", "realism", "modernism" and "apologist" are spelled that way on both sides of the
/// Atlantic. `plan.md` says "criticism", and a bare stem list calls it British.
///
/// "analys" is here for the same reason read from the other end -- it is inside "analysis" -- and
/// its three endings give "analyse" and "analysing" without it.
const ISE: [&str; 31] = [
    "recognis",
    "normalis",
    "organis",
    "serialis",
    "visualis",
    "initialis",
    "prioritis",
    "summaris",
    "quantis",
    "analys",
    "realis",
    "minimis",
    "maximis",
    "specialis",
    "standardis",
    "utilis",
    "apologis",
    "criticis",
    "authoris",
    "synchronis",
    "customis",
    "finalis",
    "generalis",
    "modernis",
    "emphasis",
    "characteris",
    "parameteris",
    "rasteris",
    "digitis",
    "optimis",
    "practis",
];

/// Everything else, searched as written.
///
/// Stems where the doubled consonant or the ending is the whole difference, so that every inflected
/// form is caught by one entry: "travell" finds "travelled" and "travelling", neither of which
/// contains the American spelling. Whole words where the American form is shorter and so cannot
/// contain them -- "judgement", "analogue" and "catalogue" are not inside "judgment", "analog" or
/// "catalog".
const BRITISH: [&str; 61] = [
    // -our
    "colour",
    "neighbour",
    "behaviour",
    "favour",
    "honour",
    "flavour",
    "labour",
    "rumour",
    "vapour",
    "armour",
    "harbour",
    "endeavour",
    "rigour",
    "vigour",
    "odour",
    // -re
    "centre",
    "metre",
    "litre",
    "fibre",
    "calibre",
    "theatre",
    "lustre",
    "manoeuvre",
    // -ce nouns
    "licence",
    "defence",
    "offence",
    "pretence",
    // the doubled -lled
    "modelled",
    "labelled",
    "cancelled",
    "travell",
    "signall",
    "fuelled",
    "totalled",
    "levelled",
    "channelled",
    "dialled",
    // -ogue
    "catalogue",
    "analogue",
    "dialogue",
    // one-offs
    "grey",
    "whilst",
    "amongst",
    "judgement",
    "acknowledgement",
    "learnt",
    "spelt",
    "burnt",
    "dreamt",
    "aluminium",
    "storey",
    "enquire",
    "sceptic",
    "mould",
    "plough",
    "draught",
    "tyre",
    "nought",
    "ageing",
    "cheque",
    "towards",
];

/// Directories below the workspace root that are read.
///
/// Named rather than reached by walking the root, which would descend into `.git` and `target`.
const ROOTS: &[&str] = &[
    "crates", "tools", "tests", "web", "include", "fuzz", ".github",
];

/// Files at the workspace root that are read. The walk starts below it, so these are named.
const ROOT_FILES: &[&str] = &[
    "README.md",
    "plan.md",
    "NOTICE",
    "Cargo.toml",
    "rustfmt.toml",
    "rust-toolchain.toml",
];

/// Directories skipped wherever they appear. See the module docs for `vendor`.
const SKIPPED: &[&str] = &["vendor", "target", ".git"];

/// Extensions held to the rule: the languages the workspace writes prose in.
const PROSE: &[&str] = &[
    "rs", "md", "h", "c", "py", "js", "mjs", "sh", "html", "toml", "yml",
];

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !SKIPPED.contains(&name.as_ref()) {
                walk(&path, out);
            }
        } else if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| PROSE.contains(&ext))
        {
            out.push(path);
        }
    }
}

#[test]
// Miri runs this crate's tests for its unsafe, and its isolation has no filesystem, so the walk
// below is an unsupported operation there rather than a spelling.
#[cfg_attr(
    miri,
    ignore = "walks the workspace; Miri's isolation has no filesystem"
)]
fn the_workspace_is_written_in_american_english() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files: Vec<PathBuf> = ROOT_FILES.iter().map(|name| root.join(name)).collect();
    for name in ROOTS {
        walk(&root.join(name), &mut files);
    }
    assert!(
        files.len() > 400,
        "walked {} files, which is too few to be the workspace -- the walk is broken, not the \
         spelling",
        files.len()
    );

    // Expanded once rather than per file: 154 terms against hundreds of files.
    let mut terms: Vec<String> = BRITISH.iter().map(|word| (*word).to_string()).collect();
    for stem in ISE {
        terms.push(format!("{stem}e"));
        terms.push(format!("{stem}ing"));
        terms.push(format!("{stem}ation"));
    }

    let mut found = Vec::new();
    for path in &files {
        // This file names every word it forbids, so it answers to none of them. A check that has
        // to spell out what it looks for cannot also be looked in.
        if path
            .file_name()
            .is_some_and(|name| name == "american_english.rs")
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let lower = text.to_lowercase();
        for term in &terms {
            if lower.contains(term) {
                found.push(format!("{relative}: {term}"));
            }
        }
    }
    found.sort();
    found.dedup();

    assert!(
        found.is_empty(),
        "this workspace is written in American English, and these are not:\n  {}",
        found.join("\n  ")
    );
}
