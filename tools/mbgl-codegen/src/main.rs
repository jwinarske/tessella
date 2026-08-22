//! Generates tessella's mirrors of mbgl C++ types from the pinned maplibre-native tree.
//!
//! DR-6: the attribute tables, UBO layouts and the scalar enums they are expressed in are
//! *data*, generated from the C++ headers and committed, never hand-maintained. The Rust
//! frontend has no shader registry to derive them from at runtime, and a mirror that drifts
//! from the headers is a wrong-pixels bug that no test in this workspace would catch — it
//! would agree with itself perfectly.
//!
//! This binary is run by hand when the pinned mbgl tree moves, not by a build script: CI has
//! no C++ checkout, and the plan's phrasing is "generated once and committed". Committing the
//! output is also what makes the diff reviewable when upstream renumbers something.
//!
//! ```text
//! cargo run -p mbgl-codegen -- --mbgl /path/to/maplibre-native
//! cargo run -p mbgl-codegen -- --mbgl /path/to/maplibre-native --check
//! ```
//!
//! `--check` regenerates in memory and exits non-zero if the committed file differs, which is
//! how a developer with the tree confirms the mirror is current.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// How a C++ enum should be mirrored on the Rust side.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Distinct values; one variant per enumerator, with a validating `from_repr`.
    Discriminant,
    /// A bitmask whose enumerators are `None` plus powers of two. Mirrored as a transparent
    /// newtype with associated constants, because a Rust enum holding an OR of two variants
    /// would be an invalid value, and mbgl very much does OR these together.
    Flags,
}

/// One enum to mirror: where it lives, what it is called, and how it behaves.
struct Source {
    /// Path under the mbgl tree root.
    header: &'static str,
    /// C++ enum name, also the Rust name.
    name: &'static str,
    /// Rust representation. Where the C++ declares no underlying type the enum is `int`, so
    /// the mirror is `i32`.
    repr: &'static str,
    shape: Shape,
    /// Prose for the generated type's doc comment, beyond the provenance line.
    doc: &'static str,
}

const SOURCES: &[Source] = &[
    Source {
        header: "include/mbgl/gfx/gfx_types.hpp",
        name: "AttributeDataType",
        repr: "u8",
        shape: Shape::Discriminant,
        doc: "The type of a vertex attribute.\n\nTwo of these travel with every attribute \
              descriptor and they are not interchangeable: the buffer's own type, and the \
              type the shader declares for the slot. Bind the declared one with the supplied \
              offset and stride. See the note on `declaredDataType` in plan.md §2.2.",
    },
    Source {
        header: "include/mbgl/gfx/types.hpp",
        name: "TexturePixelType",
        repr: "u8",
        shape: Shape::Discriminant,
        doc: "Pixel format of a texture crossing the capture stream.\n\nNote that §12.4 wants \
              glyph and SDF atlases single-channel rather than RGBA — 4x on the largest \
              persistent texture — so `Alpha` carries more traffic here than it does upstream.",
    },
    Source {
        header: "src/mbgl/renderer/render_pass.hpp",
        name: "RenderPass",
        repr: "u8",
        shape: Shape::Flags,
        doc: "Which pass or passes a drawable participates in.\n\nA bitmask, not a choice: \
              mbgl ORs these together. Consumers must honor the opaque/translucent split \
              together with `opaquePassCutoff` — opaque layers front-to-back with depth \
              writes — or tile-based parts eat full-screen overdraw per layer (§11.7).",
    },
    Source {
        header: "include/mbgl/shaders/shader_source.hpp",
        name: "BuiltIn",
        repr: "i32",
        shape: Shape::Discriminant,
        doc: "Identifies a shader family.\n\nCarried with a `permutationKey` that distinguishes \
              the data-driven-attribute variants within the family. tessella has no shader \
              registry, so this pair plus the generated attribute tables is the whole of \
              shader identity on the wire (§2.2).",
    },
];

/// Path of the generated file, relative to the workspace root.
const OUTPUT: &str = "crates/tessella-capture-abi/src/generated/mbgl_enums.rs";

/// One parsed C++ enumerator.
struct Enumerator {
    name: String,
    value: i64,
    /// Trailing `///<` documentation, carried across verbatim.
    doc: Option<String>,
}

fn main() -> ExitCode {
    let mut mbgl: Option<PathBuf> = None;
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mbgl" => mbgl = args.next().map(PathBuf::from),
            "--check" => check = true,
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(mbgl) = mbgl else {
        eprintln!("usage: mbgl-codegen --mbgl <maplibre-native tree> [--check]");
        return ExitCode::FAILURE;
    };

    let workspace = match workspace_root() {
        Ok(root) => root,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let generated = match generate(&mbgl) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let out = workspace.join(OUTPUT);
    if check {
        let current = std::fs::read_to_string(&out).unwrap_or_default();
        if current == generated {
            println!("{OUTPUT} is up to date");
            return ExitCode::SUCCESS;
        }
        eprintln!("{OUTPUT} differs from the pinned mbgl tree; re-run without --check");
        return ExitCode::FAILURE;
    }

    if let Some(parent) = out.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("creating {}: {err}", parent.display());
        return ExitCode::FAILURE;
    }
    if let Err(err) = std::fs::write(&out, &generated) {
        eprintln!("writing {}: {err}", out.display());
        return ExitCode::FAILURE;
    }
    println!("wrote {OUTPUT}");
    ExitCode::SUCCESS
}

/// The workspace root, taken from cargo rather than guessed from the current directory.
fn workspace_root() -> Result<PathBuf, String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "CARGO_MANIFEST_DIR unset; run via `cargo run -p mbgl-codegen`".to_string())?;
    // tools/mbgl-codegen -> tools -> workspace root
    Path::new(&manifest)
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("cannot find workspace root above {manifest}"))
}

/// Reads the pinned tree's HEAD so the generated file records exactly what it came from.
/// Falls back to a marker rather than failing: a missing `.git` makes the provenance weaker,
/// not the mirror wrong.
fn tree_revision(mbgl: &Path) -> String {
    let head = std::fs::read_to_string(mbgl.join(".git/HEAD")).unwrap_or_default();
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        std::fs::read_to_string(mbgl.join(".git").join(reference))
            .map(|sha| sha.trim().chars().take(12).collect())
            .unwrap_or_else(|_| "unknown".to_string())
    } else if head.len() >= 12 {
        head.chars().take(12).collect()
    } else {
        "unknown".to_string()
    }
}

fn generate(mbgl: &Path) -> Result<String, String> {
    let revision = tree_revision(mbgl);

    // Headers are read once each even when they carry several of the enums we want.
    let mut headers: BTreeMap<&str, String> = BTreeMap::new();
    for source in SOURCES {
        if !headers.contains_key(source.header) {
            let path = mbgl.join(source.header);
            let text = std::fs::read_to_string(&path)
                .map_err(|err| format!("reading {}: {err}", path.display()))?;
            headers.insert(source.header, text);
        }
    }

    let mut out = String::new();
    writeln!(
        out,
        "// @generated by `cargo run -p mbgl-codegen`. Do not edit by hand."
    )
    .unwrap();
    writeln!(out, "//").unwrap();
    writeln!(
        out,
        "// Source: maplibre-native @ {revision}, branch capture-backend-phase0."
    )
    .unwrap();
    writeln!(
        out,
        "// Regenerate when that pin moves; DR-6 makes drift a review-visible diff rather"
    )
    .unwrap();
    writeln!(out, "// than a silent wrong-pixels bug.").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "//! Mirrors of mbgl scalar enums that cross the capture stream."
    )
    .unwrap();
    writeln!(out, "//!").unwrap();
    writeln!(
        out,
        "//! Every value here is untrusted on ingress — see the crate-level note. The"
    )
    .unwrap();
    writeln!(
        out,
        "//! `from_repr` and `from_bits` constructors are the only supported way in."
    )
    .unwrap();

    for source in SOURCES {
        let header = &headers[source.header];
        let enumerators = parse_enum(header, source.name)
            .ok_or_else(|| format!("enum {} not found in {}", source.name, source.header))?;
        emit(&mut out, source, &enumerators);
    }

    Ok(out)
}

/// Extracts the enumerators of `enum class <name>` from a header.
///
/// A regex-free scan: this parses the handful of shapes mbgl actually writes, and returns
/// `None` rather than guessing at anything else. A missing enum is a hard error upstream, so
/// a renamed enum stops the generator instead of silently emitting a shorter mirror.
fn parse_enum(header: &str, name: &str) -> Option<Vec<Enumerator>> {
    let decl = header.find(&format!("enum class {name}"))?;
    let open = header[decl..].find('{')? + decl;
    let close = header[open..].find("};")? + open;
    let body = &header[open + 1..close];

    let mut enumerators = Vec::new();
    let mut next_value = 0i64;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        // Split the trailing `///< doc` off before touching the declaration.
        let (decl, doc) = match line.find("///<") {
            Some(at) => (&line[..at], Some(line[at + 4..].trim().to_string())),
            None => (line, None),
        };
        let decl = decl.trim().trim_end_matches(',').trim();
        if decl.is_empty() {
            continue;
        }
        let (ident, value) = match decl.split_once('=') {
            Some((ident, value)) => {
                let value = value.trim();
                let parsed = if let Some((lhs, rhs)) = value.split_once("<<") {
                    // mbgl writes flag values as `1 << n`.
                    let lhs: i64 = lhs.trim().parse().ok()?;
                    let rhs: u32 = rhs.trim().parse().ok()?;
                    lhs.checked_shl(rhs)?
                } else {
                    value.parse().ok()?
                };
                (ident.trim(), parsed)
            }
            None => (decl, next_value),
        };
        if !ident.chars().all(|c| c.is_alphanumeric() || c == '_') || ident.is_empty() {
            return None;
        }
        next_value = value.checked_add(1)?;
        enumerators.push(Enumerator {
            name: ident.to_string(),
            value,
            doc,
        });
    }
    (!enumerators.is_empty()).then_some(enumerators)
}

fn emit(out: &mut String, source: &Source, enumerators: &[Enumerator]) {
    writeln!(out).unwrap();
    for line in wrap(source.doc, 92) {
        if line.is_empty() {
            writeln!(out, "///").unwrap();
        } else {
            writeln!(out, "/// {line}").unwrap();
        }
    }
    writeln!(out, "///").unwrap();
    writeln!(
        out,
        "/// Mirrors `mln::{}` from `{}`.",
        source.name, source.header
    )
    .unwrap();

    match source.shape {
        Shape::Discriminant => emit_discriminant(out, source, enumerators),
        Shape::Flags => emit_flags(out, source, enumerators),
    }
}

fn emit_discriminant(out: &mut String, source: &Source, enumerators: &[Enumerator]) {
    let name = source.name;
    let repr = source.repr;
    writeln!(out, "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]").unwrap();
    writeln!(out, "#[repr({repr})]").unwrap();
    writeln!(out, "pub enum {name} {{").unwrap();
    for e in enumerators {
        // Upstream documents some enumerators and not others. Where it does not, name the
        // C++ enumerator rather than leaving the variant bare: the mirror's whole job is to
        // be traceable back to a line of mbgl.
        match &e.doc {
            Some(doc) => writeln!(out, "    /// {doc}").unwrap(),
            None => writeln!(out, "    /// `mln::{}::{}`.", name, e.name).unwrap(),
        }
        writeln!(out, "    {} = {},", e.name, e.value).unwrap();
    }
    writeln!(out, "}}").unwrap();

    writeln!(out).unwrap();
    writeln!(out, "impl {name} {{").unwrap();
    writeln!(out, "    /// Every mirrored value, in declaration order.").unwrap();
    writeln!(out, "    pub const ALL: [Self; {}] = [", enumerators.len()).unwrap();
    for e in enumerators {
        writeln!(out, "        Self::{},", e.name).unwrap();
    }
    writeln!(out, "    ];").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "    /// Converts a wire value into a `{name}`, rejecting anything unrecognized."
    )
    .unwrap();
    writeln!(out, "    #[must_use]").unwrap();
    writeln!(
        out,
        "    pub const fn from_repr(value: {repr}) -> Option<Self> {{"
    )
    .unwrap();
    writeln!(out, "        match value {{").unwrap();
    for e in enumerators {
        writeln!(out, "            {} => Some(Self::{}),", e.value, e.name).unwrap();
    }
    writeln!(out, "            _ => None,").unwrap();
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
}

fn emit_flags(out: &mut String, source: &Source, enumerators: &[Enumerator]) {
    let name = source.name;
    let repr = source.repr;
    let mask: i64 = enumerators.iter().fold(0, |acc, e| acc | e.value);

    writeln!(
        out,
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]"
    )
    .unwrap();
    writeln!(out, "#[repr(transparent)]").unwrap();
    writeln!(out, "pub struct {name}(pub(crate) {repr});").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "impl {name} {{").unwrap();
    for e in enumerators {
        if let Some(doc) = &e.doc {
            writeln!(out, "    /// {doc}").unwrap();
        } else {
            writeln!(out, "    /// `mln::{}::{}`.", name, e.name).unwrap();
        }
        writeln!(
            out,
            "    pub const {}: Self = Self({});",
            screaming(&e.name),
            e.value
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    writeln!(out, "    /// Every bit any mirrored value defines.").unwrap();
    writeln!(out, "    pub const VALID_BITS: {repr} = {mask};").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "    /// Converts a wire value into a `{name}`, rejecting undefined bits."
    )
    .unwrap();
    writeln!(out, "    ///").unwrap();
    writeln!(
        out,
        "    /// Unlike a discriminant enum any combination of defined bits is legal, so this"
    )
    .unwrap();
    writeln!(
        out,
        "    /// checks the mask rather than a value list. An undefined bit means the far side"
    )
    .unwrap();
    writeln!(
        out,
        "    /// knows a pass this build does not, which is a fault to report, not to mask off."
    )
    .unwrap();
    writeln!(out, "    #[must_use]").unwrap();
    writeln!(
        out,
        "    pub const fn from_bits(value: {repr}) -> Option<Self> {{"
    )
    .unwrap();
    writeln!(out, "        if value & !Self::VALID_BITS == 0 {{").unwrap();
    writeln!(out, "            Some(Self(value))").unwrap();
    writeln!(out, "        }} else {{").unwrap();
    writeln!(out, "            None").unwrap();
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "    /// The raw bits.").unwrap();
    writeln!(out, "    #[must_use]").unwrap();
    writeln!(out, "    pub const fn bits(self) -> {repr} {{").unwrap();
    writeln!(out, "        self.0").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "    /// True when every bit of `other` is set.").unwrap();
    writeln!(out, "    #[must_use]").unwrap();
    writeln!(
        out,
        "    pub const fn contains(self, other: Self) -> bool {{"
    )
    .unwrap();
    writeln!(out, "        self.0 & other.0 == other.0").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "impl core::ops::BitOr for {name} {{").unwrap();
    writeln!(out, "    type Output = Self;").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "    fn bitor(self, rhs: Self) -> Self {{").unwrap();
    writeln!(out, "        Self(self.0 | rhs.0)").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
}

/// `Pass3D` -> `PASS3_D` is wrong; mbgl's flag names are short and better mapped by hand-free
/// uppercasing with underscores inserted only between a lowercase and an uppercase run.
fn screaming(name: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() && chars[i - 1].is_lowercase() {
            out.push('_');
        }
        out.extend(c.to_uppercase());
    }
    out
}

/// Wraps doc prose to `width` columns, preserving blank lines as paragraph breaks.
///
/// rustfmt leaves comments alone, so an unwrapped sentence stays a 300-column line in the
/// committed output forever. Wrapping here is the only place it can happen.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if !current.is_empty() && current.len() + 1 + word.len() > width {
                lines.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}
