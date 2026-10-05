// SPDX-License-Identifier: BSD-2-Clause
//! No source file in the workspace claims a license other than the workspace's.
//!
//! Ten claimed Apache-2.0, and none of them was a deliberate choice. `tessella_fluorite` and
//! `ivi-homescreen` are Apache-2.0 and sit beside this tree, so a file begun in one of those and
//! finished in this one arrives with their header. Nothing caught it: the workspace declares
//! `BSD-2-Clause` in `Cargo.toml` and `LICENSE`, and nothing read the files back.
//!
//! `include/tessella.h` is why it matters rather than merely being untidy. A consumer copies that
//! header into its own tree, so its SPDX line is the license that consumer believes it has.
//!
//! # What this does not decide
//!
//! **Whether a file needs a header at all.** 346 source files here carry no SPDX line, against 64
//! that do, so a per-file header is the exception in this workspace rather than the rule. Whether
//! that should change is a policy question, and a test that answered it by failing would be
//! deciding it. So a file with no SPDX line passes: this catches a *wrong* license, which is the
//! defect it was written for, and says nothing about a missing one.
//!
//! **Whose terms a derived file carries.** A file that genuinely derives from another project
//! keeps that project's terms, and this would be the wrong place to argue otherwise. `vendor/` is
//! excluded for that reason -- `earcutr` has its own `LICENSE` and upstream's header. If a file in
//! the workspace proper ever needs different terms, the honest form is a `NOTICE` entry saying
//! which file and from where, and an exception here pointing at it.

use std::path::{Path, PathBuf};

/// The only license a file in the workspace proper may declare.
const ALLOWED: &str = "BSD-2-Clause";

/// Directories that are not the workspace's to license.
///
/// `vendor` is third-party and keeps upstream's terms. `target` and `.git` are not source.
const SKIPPED: &[&str] = &["vendor", "target", ".git", ".github"];

/// Extensions this holds to the rule: the languages the workspace is written in.
const SOURCES: &[&str] = &["rs", "h", "c", "cpp", "cc"];

/// Files with different terms, each with a `NOTICE` entry saying why.
///
/// Empty, and an addition needs the `NOTICE` entry in the same change.
const EXCEPTIONS: &[&str] = &[];

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
            .is_some_and(|ext| SOURCES.contains(&ext))
        {
            out.push(path);
        }
    }
}

#[test]
// Miri runs this crate's tests for its unsafe, and its isolation has no filesystem, so the walk
// below is an unsupported operation there rather than a failure of any header.
#[cfg_attr(
    miri,
    ignore = "walks the workspace; Miri's isolation has no filesystem"
)]
fn no_source_file_claims_another_license() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(
        files.len() > 40,
        "walked {} source files, which is too few to be the workspace -- the walk is broken, \
         not the headers",
        files.len()
    );

    let mut wrong = Vec::new();
    for path in &files {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if EXCEPTIONS.contains(&relative.as_str()) {
            continue;
        }
        // The first few lines, because a license line further down is one a reader of the top of
        // the file will not see.
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Some(line) = text
            .lines()
            .take(3)
            .find(|line| line.contains("SPDX-License-Identifier:"))
        else {
            // No claim, so nothing to contradict. See the module docs.
            continue;
        };
        if !line.contains(ALLOWED) {
            wrong.push(format!("{relative}  ({})", line.trim()));
        }
    }

    assert!(
        wrong.is_empty(),
        "these declare a license the workspace does not use:\n  {}\n\nThe workspace is \
         BSD-2-Clause (Cargo.toml, LICENSE). A file that genuinely needs other terms goes in \
         EXCEPTIONS with a NOTICE entry saying from where.",
        wrong.join("\n  ")
    );
}
