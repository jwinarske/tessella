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
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

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

/// Path of the generated enum mirrors, relative to the workspace root.
const OUTPUT: &str = "crates/tessella-capture-abi/src/generated/mbgl_enums.rs";

/// Path of the generated shader attribute tables.
const SHADER_OUTPUT: &str = "crates/tessella-capture-abi/src/generated/shader_attributes.rs";

/// Path of the generated shader texture tables.
const TEXTURE_OUTPUT: &str = "crates/tessella-capture-abi/src/generated/texture_slots.rs";

/// Path of the generated UBO layouts.
const UBO_OUTPUT: &str = "crates/tessella-capture-abi/src/generated/ubo_layouts.rs";

/// Path of the generated UBO slot ids.
const SLOT_OUTPUT: &str = "crates/tessella-capture-abi/src/generated/ubo_slots.rs";

/// Path of the generated expression operator registry.
const OPERATOR_OUTPUT: &str = "crates/tessella-style/src/generated/operators.rs";

/// Where the Unicode block table lands.
const BLOCK_OUTPUT: &str = "crates/tessella-glyph/src/generated/blocks.rs";

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

    let shaders = match generate_shader_attributes(&mbgl).map(|text| rustfmt(&text)) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let textures = match generate_texture_slots(&mbgl).map(|text| rustfmt(&text)) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let ubos = match generate_ubo_layouts(&mbgl).map(|text| rustfmt(&text)) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let slots = match generate_ubo_slots(&mbgl).map(|text| rustfmt(&text)) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let operators = match generate_operators(&mbgl).map(|text| rustfmt(&text)) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let blocks = match generate_unicode_blocks(&mbgl).map(|text| rustfmt(&text)) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let out = workspace.join(OUTPUT);
    let shader_out = workspace.join(SHADER_OUTPUT);
    let texture_out = workspace.join(TEXTURE_OUTPUT);
    let ubo_out = workspace.join(UBO_OUTPUT);
    let slot_out = workspace.join(SLOT_OUTPUT);
    let operator_out = workspace.join(OPERATOR_OUTPUT);
    let block_out = workspace.join(BLOCK_OUTPUT);
    if check {
        let mut stale = false;
        for (path, name, want) in [
            (&out, OUTPUT, &generated),
            (&shader_out, SHADER_OUTPUT, &shaders),
            (&texture_out, TEXTURE_OUTPUT, &textures),
            (&ubo_out, UBO_OUTPUT, &ubos),
            (&slot_out, SLOT_OUTPUT, &slots),
            (&operator_out, OPERATOR_OUTPUT, &operators),
            (&block_out, BLOCK_OUTPUT, &blocks),
        ] {
            let current = std::fs::read_to_string(path).unwrap_or_default();
            if current == *want {
                println!("{name} is up to date");
            } else {
                eprintln!("{name} differs from the pinned mbgl tree; re-run without --check");
                stale = true;
            }
        }
        return if stale {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }

    for path in [&out, &operator_out, &block_out] {
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            eprintln!("creating {}: {err}", parent.display());
            return ExitCode::FAILURE;
        }
    }
    for (path, name, text) in [
        (&out, OUTPUT, &generated),
        (&shader_out, SHADER_OUTPUT, &shaders),
        (&texture_out, TEXTURE_OUTPUT, &textures),
        (&ubo_out, UBO_OUTPUT, &ubos),
        (&slot_out, SLOT_OUTPUT, &slots),
        (&operator_out, OPERATOR_OUTPUT, &operators),
        (&block_out, BLOCK_OUTPUT, &blocks),
    ] {
        if let Err(err) = std::fs::write(path, text) {
            eprintln!("writing {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
        println!("wrote {name}");
    }
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

/// Generates the per-shader vertex attribute tables (DR-6).
///
/// Two sources feed this. `shader_defines.hpp` declares the attribute ids as anonymous enums,
/// one block per shader family, whose values are their positions. `src/mbgl/shaders/vulkan/*.cpp`
/// declares, for each shader, the attributes it actually binds: a binding slot, the type the
/// shader *declares*, and the id.
///
/// The declared type is the point of the whole exercise. A shader declares the zoom-interpolated
/// width of a data-driven property — fill's color is `Float4`, a packed min/max pair — while the
/// binder supplies only as much as the property needs, `Float2` when it varies per feature but
/// not with zoom. §2.2 says to bind the declared type with the supplied offset and stride, and
/// this table is where "declared" comes from. Without it a producer has nothing to put in
/// `declaredDataType` but a guess.
///
/// The ids also decide what gets dropped. An attribute a drawable supplies that its shader does
/// not declare binds at `-1`, and the consumer drops it — `fill-outline-color` on the plain fill
/// shader is exactly that case, and it is visible in the golden dump as `bind=-1 ddt=255`.
fn generate_shader_attributes(mbgl: &Path) -> Result<String, String> {
    let revision = tree_revision(mbgl);

    let defines_path = mbgl.join("include/mbgl/shaders/shader_defines.hpp");
    let defines = std::fs::read_to_string(&defines_path)
        .map_err(|err| format!("reading {}: {err}", defines_path.display()))?;
    let ids = parse_attribute_ids(&defines);

    let dir = mbgl.join("src/mbgl/shaders/vulkan");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|err| format!("reading {}: {err}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "cpp"))
        .collect();
    // Directory order is filesystem order, which is not stable across machines.
    files.sort();

    let mut shaders: Vec<ShaderTable> = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .map_err(|err| format!("reading {}: {err}", file.display()))?;
        shaders.extend(parse_shader_attributes(&text));
    }
    shaders.sort_by(|a, b| a.0.cmp(&b.0));

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
        "// Attribute ids from include/mbgl/shaders/shader_defines.hpp; declared types and"
    )
    .unwrap();
    writeln!(out, "// binding slots from src/mbgl/shaders/vulkan/*.cpp.").unwrap();
    writeln!(out).unwrap();
    for line in wrap(
        "Per-shader vertex attribute tables (DR-6). What a shader declares, as data. A producer \
         reads the declared type from here rather than guessing it, which is what makes \
         `declaredDataType` on the wire mean anything; and an attribute absent from a shader's \
         table binds at -1 and is dropped by the consumer.",
        94,
    ) {
        if line.is_empty() {
            writeln!(out, "//!").unwrap();
        } else {
            writeln!(out, "//! {line}").unwrap();
        }
    }
    writeln!(out).unwrap();
    writeln!(
        out,
        "use super::mbgl_enums::{{AttributeDataType, BuiltIn}};"
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "/// One attribute a shader declares.").unwrap();
    writeln!(out, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]").unwrap();
    writeln!(out, "pub struct ShaderAttribute {{").unwrap();
    writeln!(out, "    /// Binding slot the shader declares for it.").unwrap();
    writeln!(out, "    pub binding: i32,").unwrap();
    writeln!(
        out,
        "    /// Type the shader declares. Bind this, with the supplied offset and stride."
    )
    .unwrap();
    writeln!(out, "    pub declared: AttributeDataType,").unwrap();
    writeln!(out, "    /// Shader-side attribute id.").unwrap();
    writeln!(out, "    pub attr_id: u32,").unwrap();
    writeln!(
        out,
        "    /// Name of the id in `shader_defines.hpp`, for diagnostics."
    )
    .unwrap();
    writeln!(out, "    pub name: &'static str,").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    for (shader, attributes) in &shaders {
        writeln!(out, "/// Attributes declared by `{shader}`.").unwrap();
        writeln!(
            out,
            "pub const {}: [ShaderAttribute; {}] = [",
            screaming(shader),
            attributes.len()
        )
        .unwrap();
        for (binding, data_type, id_name) in attributes {
            let id = ids.get(id_name).copied().unwrap_or(u32::MAX);
            writeln!(out, "    ShaderAttribute {{").unwrap();
            writeln!(out, "        binding: {binding},").unwrap();
            writeln!(out, "        declared: AttributeDataType::{data_type},").unwrap();
            writeln!(out, "        attr_id: {id},").unwrap();
            writeln!(out, "        name: \"{id_name}\",").unwrap();
            writeln!(out, "    }},").unwrap();
        }
        writeln!(out, "];").unwrap();
        writeln!(out).unwrap();
    }

    writeln!(
        out,
        "/// The attributes a shader declares, or an empty slice for one with no table."
    )
    .unwrap();
    writeln!(out, "///").unwrap();
    writeln!(
        out,
        "/// An empty table is not the same as a shader that binds nothing: it means this"
    )
    .unwrap();
    writeln!(
        out,
        "/// build has no data for it, and a producer should treat that as a fault rather than"
    )
    .unwrap();
    writeln!(out, "/// as permission to bind nothing.").unwrap();
    writeln!(out, "#[must_use]").unwrap();
    writeln!(
        out,
        "pub fn attributes(shader: BuiltIn) -> &'static [ShaderAttribute] {{"
    )
    .unwrap();
    writeln!(out, "    match shader {{").unwrap();
    for (shader, _) in &shaders {
        writeln!(out, "        BuiltIn::{shader} => &{},", screaming(shader)).unwrap();
    }
    writeln!(out, "        _ => &[],").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "/// Looks up what a shader declares for an attribute id."
    )
    .unwrap();
    writeln!(out, "///").unwrap();
    writeln!(
        out,
        "/// `None` when the shader does not declare it. That is not an error: a drawable may"
    )
    .unwrap();
    writeln!(
        out,
        "/// supply an override its shader has no slot for, and the rule is to bind it at -1 so"
    )
    .unwrap();
    writeln!(
        out,
        "/// the consumer drops it (§2.2). `fill-outline-color` on the plain fill shader is"
    )
    .unwrap();
    writeln!(
        out,
        "/// exactly that, and the golden dump shows it as `bind=-1 ddt=255`."
    )
    .unwrap();
    writeln!(out, "#[must_use]").unwrap();
    writeln!(
        out,
        "pub fn declared_for(shader: BuiltIn, attr_id: u32) -> Option<ShaderAttribute> {{"
    )
    .unwrap();
    writeln!(out, "    attributes(shader)").unwrap();
    writeln!(out, "        .iter()").unwrap();
    writeln!(
        out,
        "        .find(|attribute| attribute.attr_id == attr_id)"
    )
    .unwrap();
    writeln!(out, "        .copied()").unwrap();
    writeln!(out, "}}").unwrap();

    Ok(out)
}

/// Generates the per-shader texture slot tables (DR-6).
///
/// The same two sources as the attribute tables, and the same reason for reading them rather
/// than writing the numbers down. `shader_defines.hpp` declares texture ids as anonymous enums,
/// one block per shader family; `src/mbgl/shaders/vulkan/*.cpp` declares, for each shader, which
/// of them it actually binds and at which slot.
///
/// A slot is not a property of a texture, it is a property of a *shader*. The glyph atlas is
/// `idSymbolImageTexture` at slot 0 for `SymbolSDFShader` and the sprite atlas is
/// `idSymbolImageIconTexture` at slot 1 — but only `SymbolTextAndIconShader` declares the
/// second, so a producer binding two textures to an SDF drawable binds one the shader has no
/// sampler for. The table is what says which is which.
///
/// The raster case is the one that looks like a mistake and is not: `RasterShaderSource`
/// declares *two* textures and `render_raster_layer.cpp` sets the same image to both. Slot 1 is
/// the parent tile a fading tile blends against, and with no fade in progress it is the tile
/// itself. A producer binding only slot 0 leaves the second sampler unbound.
fn generate_texture_slots(mbgl: &Path) -> Result<String, String> {
    let revision = tree_revision(mbgl);

    let defines_path = mbgl.join("include/mbgl/shaders/shader_defines.hpp");
    let defines = std::fs::read_to_string(&defines_path)
        .map_err(|err| format!("reading {}: {err}", defines_path.display()))?;
    let ids = parse_attribute_ids(&defines);

    let dir = mbgl.join("src/mbgl/shaders/vulkan");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|err| format!("reading {}: {err}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "cpp"))
        .collect();
    files.sort();

    let mut shaders: Vec<TextureTable> = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .map_err(|err| format!("reading {}: {err}", file.display()))?;
        shaders.extend(parse_shader_textures(&text));
    }
    shaders.sort_by(|a, b| a.0.cmp(&b.0));

    if shaders.is_empty() {
        return Err("no shader declared a texture table; the parse missed something".to_string());
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
        "// Texture ids from include/mbgl/shaders/shader_defines.hpp; binding slots from"
    )
    .unwrap();
    writeln!(out, "// src/mbgl/shaders/vulkan/*.cpp.").unwrap();
    writeln!(out).unwrap();
    for line in wrap(
        "Per-shader texture slot tables (DR-6). Which samplers a shader has and what goes in \
         them, as data. A slot belongs to the shader rather than to the texture: the same glyph \
         atlas is slot 0 of the SDF shader and slot 0 of the text-and-icon shader, while the \
         sprite atlas is slot 1 of the second and has no slot at all in the first. Binding by a \
         remembered number instead binds a texture the shader has no sampler for, which reads as \
         a missing picture rather than as a wrong slot.",
        94,
    ) {
        if line.is_empty() {
            writeln!(out, "//!").unwrap();
        } else {
            writeln!(out, "//! {line}").unwrap();
        }
    }
    writeln!(out).unwrap();
    writeln!(out, "use super::mbgl_enums::BuiltIn;").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "/// One texture a shader samples.").unwrap();
    writeln!(out, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]").unwrap();
    writeln!(out, "pub struct ShaderTexture {{").unwrap();
    writeln!(out, "    /// Binding slot the shader declares for it.").unwrap();
    writeln!(out, "    pub binding: u32,").unwrap();
    writeln!(out, "    /// Shader-side texture id.").unwrap();
    writeln!(out, "    pub texture_id: u32,").unwrap();
    writeln!(
        out,
        "    /// Name of the id in `shader_defines.hpp`, for diagnostics."
    )
    .unwrap();
    writeln!(out, "    pub name: &'static str,").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    for (shader, textures) in &shaders {
        writeln!(out, "/// Textures sampled by `{shader}`.").unwrap();
        writeln!(
            out,
            "pub const {}_TEXTURES: [ShaderTexture; {}] = [",
            screaming(shader),
            textures.len()
        )
        .unwrap();
        for (binding, id_name) in textures {
            let id = ids.get(id_name).copied().unwrap_or(u32::MAX);
            writeln!(out, "    ShaderTexture {{").unwrap();
            writeln!(out, "        binding: {binding},").unwrap();
            writeln!(out, "        texture_id: {id},").unwrap();
            writeln!(out, "        name: \"{id_name}\",").unwrap();
            writeln!(out, "    }},").unwrap();
        }
        writeln!(out, "];").unwrap();
        writeln!(out).unwrap();
    }

    writeln!(
        out,
        "/// The textures a shader samples, or an empty slice for one that samples none."
    )
    .unwrap();
    writeln!(out, "///").unwrap();
    writeln!(
        out,
        "/// Empty is a real answer here, unlike in the attribute tables: a fill shader genuinely"
    )
    .unwrap();
    writeln!(
        out,
        "/// has no sampler, and mbgl writes that as `std::array<TextureInfo, 0>`. A shader"
    )
    .unwrap();
    writeln!(
        out,
        "/// missing from the match falls through to the same empty slice, which is why"
    )
    .unwrap();
    writeln!(
        out,
        "/// [`texture_count`] exists — it distinguishes the two by consulting the table itself."
    )
    .unwrap();
    writeln!(out, "#[must_use]").unwrap();
    writeln!(
        out,
        "pub fn textures(shader: BuiltIn) -> &'static [ShaderTexture] {{"
    )
    .unwrap();
    writeln!(out, "    match shader {{").unwrap();
    for (shader, _) in &shaders {
        writeln!(
            out,
            "        BuiltIn::{shader} => &{}_TEXTURES,",
            screaming(shader)
        )
        .unwrap();
    }
    writeln!(out, "        _ => &[],").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "/// How many samplers a shader has, or `None` for a shader this build has no table for."
    )
    .unwrap();
    writeln!(out, "///").unwrap();
    writeln!(
        out,
        "/// The distinction an empty slice cannot make. A producer that binds nothing to a"
    )
    .unwrap();
    writeln!(
        out,
        "/// shader with no samplers is correct; one that binds nothing because the table was"
    )
    .unwrap();
    writeln!(
        out,
        "/// never generated is emitting a drawable that cannot draw."
    )
    .unwrap();
    writeln!(out, "#[must_use]").unwrap();
    writeln!(
        out,
        "pub fn texture_count(shader: BuiltIn) -> Option<usize> {{"
    )
    .unwrap();
    writeln!(out, "    match shader {{").unwrap();
    for (shader, textures) in &shaders {
        writeln!(
            out,
            "        BuiltIn::{shader} => Some({}),",
            textures.len()
        )
        .unwrap();
    }
    writeln!(out, "        _ => None,").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "/// Every shader with a table, for exhaustive checks.").unwrap();
    writeln!(out, "pub const TABLED: [BuiltIn; {}] = [", shaders.len()).unwrap();
    for (shader, _) in &shaders {
        writeln!(out, "    BuiltIn::{shader},").unwrap();
    }
    writeln!(out, "];").unwrap();

    Ok(out)
}

/// One shader's textures: binding slot and texture id name.
type TextureTable = (String, Vec<(u32, String)>);

/// Extracts `(shader, [(binding, id name)])` from a shader source file.
///
/// The zero case has to be recognised as well as the populated one. mbgl writes a shader with no
/// samplers as `std::array<TextureInfo, 0> XSource::textures = {};` — one line, no block — and a
/// parser that only looked for an opening brace would leave those shaders absent from the table
/// rather than present with nothing in them. The two mean different things.
fn parse_shader_textures(text: &str) -> Vec<TextureTable> {
    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("using ")
            && let Some((alias, tail)) = rest.split_once(" = ShaderSource<BuiltIn::")
            && let Some((shader, _)) = tail.split_once(',')
        {
            aliases.insert(alias.trim().to_string(), shader.trim().to_string());
        }
    }

    let shader_of = |line: &str| -> Option<String> {
        let alias = line.split("> ").nth(1)?.split("::").next()?.trim();
        aliases.get(alias).cloned()
    };

    let mut out = Vec::new();
    let mut current: Option<TextureTable> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.contains("::textures = {") {
            // `= {};` on one line is an empty table, not the start of a block.
            if line.ends_with("{};")
                && let Some(shader) = shader_of(line)
            {
                out.push((shader, Vec::new()));
                continue;
            }
            if let Some(shader) = shader_of(line) {
                current = Some((shader, Vec::new()));
            }
            continue;
        }
        if let Some((_, textures)) = current.as_mut() {
            if line.starts_with("};") {
                if let Some(entry) = current.take() {
                    out.push(entry);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("TextureInfo{") {
                let body = rest.trim_end_matches("},").trim_end_matches('}');
                let parts: Vec<&str> = body.split(',').map(str::trim).collect();
                if parts.len() == 2
                    && let Ok(binding) = parts[0].parse::<u32>()
                {
                    textures.push((binding, parts[1].to_string()));
                }
            }
        }
    }
    out
}

/// One shader's attributes: binding slot, declared type name, attribute id name.
type ShaderTable = (String, Vec<(i32, String, String)>);

/// Maps attribute id names to their values.
///
/// The ids are anonymous enums whose values are positions, one block per shader family, and each
/// block ends with a `...Count` member. Preprocessor conditionals are followed by taking the
/// first branch, which is recorded in the output rather than hidden: only fill-extrusion has
/// one, and R0 does not use it.
fn parse_attribute_ids(header: &str) -> BTreeMap<String, u32> {
    let mut ids = BTreeMap::new();
    let mut value = 0u32;
    let mut in_enum = false;
    let mut skipping = false;

    for line in header.lines() {
        let line = line.trim();
        if line.starts_with("enum {") || line == "enum {" {
            in_enum = true;
            value = 0;
            continue;
        }
        if !in_enum {
            continue;
        }
        // Take the first arm of a conditional and skip the alternative.
        if line.starts_with("#if") {
            continue;
        }
        if line.starts_with("#else") {
            skipping = true;
            continue;
        }
        if line.starts_with("#endif") {
            skipping = false;
            continue;
        }
        if skipping || line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with("};") {
            in_enum = false;
            continue;
        }
        let name = line.trim_end_matches(',').trim();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        // The trailing count member is not an attribute.
        if !name.ends_with("Count") {
            ids.insert(name.to_string(), value);
        }
        value += 1;
    }
    ids
}

/// Extracts `(shader, [(binding, declared type, id name)])` from a shader source file.
fn parse_shader_attributes(text: &str) -> Vec<ShaderTable> {
    // `using XSource = ShaderSource<BuiltIn::Name, ...>;` binds an alias to a shader.
    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("using ")
            && let Some((alias, tail)) = rest.split_once(" = ShaderSource<BuiltIn::")
            && let Some((shader, _)) = tail.split_once(',')
        {
            aliases.insert(alias.trim().to_string(), shader.trim().to_string());
        }
    }

    let mut out = Vec::new();
    let mut current: Option<ShaderTable> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.contains("::attributes = {")
            && let Some(alias) = line.split("> ").nth(1).and_then(|s| s.split("::").next())
            && let Some(shader) = aliases.get(alias.trim())
        {
            current = Some((shader.clone(), Vec::new()));
            continue;
        }
        if let Some((_, attributes)) = current.as_mut() {
            if line.starts_with("};") {
                if let Some(entry) = current.take() {
                    out.push(entry);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("AttributeInfo{") {
                let body = rest.trim_end_matches("},").trim_end_matches('}');
                let parts: Vec<&str> = body.split(',').map(str::trim).collect();
                if parts.len() == 3
                    && let Ok(binding) = parts[0].parse::<i32>()
                {
                    let data_type = parts[1].rsplit("::").next().unwrap_or(parts[1]).to_string();
                    attributes.push((binding, data_type, parts[2].to_string()));
                }
            }
        }
    }
    out
}

/// Formats generated source with rustfmt, falling back to the input if that is not possible.
///
/// The output is committed and CI runs `cargo fmt --check` over it, so a generator emitting
/// almost-formatted code makes every regeneration dirty the tree. Replicating rustfmt's
/// heuristics — it collapses a single-element array onto one line, for instance — is a losing
/// game, so the real thing is used instead and stability is by construction rather than by
/// matching its rules.
///
/// A missing rustfmt is not fatal: the output is still correct, `cargo fmt` will tidy it, and
/// failing the whole generation over formatting would be worse than emitting it unformatted.
fn rustfmt(text: &str) -> String {
    let Ok(mut child) = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    else {
        eprintln!("note: rustfmt not found; emitting unformatted");
        return text.to_string();
    };

    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
    }
    // Dropping stdin closes the pipe, which rustfmt needs before it will produce output.
    drop(child.stdin.take());

    match child.wait_with_output() {
        Ok(output) if output.status.success() => {
            String::from_utf8(output.stdout).unwrap_or_else(|_| text.to_string())
        }
        _ => {
            eprintln!("note: rustfmt failed; emitting unformatted");
            text.to_string()
        }
    }
}

/// A field of a UBO, as the header declares it.
struct UboField {
    offset: u32,
    /// Kind name as the generated enum spells it. The size lives on the kind, not here.
    kind: &'static str,
    name: String,
}

/// A UBO layout, verified against its own header.
struct UboLayout {
    name: String,
    header: String,
    align: u32,
    /// Where the fields end. mbgl's closing comment.
    size: u32,
    /// `sizeof`, which pads the field extent up to the alignment. mbgl asserts this itself.
    stride: u32,
    fields: Vec<UboField>,
}

/// A union of drawable blocks, whose size is the stride of a consolidated buffer.
struct UboUnion {
    name: String,
    header: String,
    members: Vec<String>,
    stride: u32,
}

/// A struct the parser could not verify, and why.
struct UnparsedUbo {
    name: String,
    header: String,
    reason: String,
}

/// Generates the UBO layouts (DR-6).
///
/// mbgl declares each uniform block as a `struct alignas(16)` whose every field carries its byte
/// offset in a comment and whose closing comment is the struct's size. Those comments are the
/// authority: mbgl's own `static_assert`s check the C++ struct against them, so a layout derived
/// from them is derived from the same source the shaders are.
///
/// # Verified, not merely transcribed
///
/// Each struct is accepted only if every field's declared offset equals the running total of the
/// fields before it *and* the total equals the declared size. A header whose comments had drifted
/// from its fields would be rejected rather than turned into a table that silently mispacks every
/// frame — which is the failure mode that makes a hand-maintained table unacceptable in the first
/// place, and it does not improve by being generated carelessly.
///
/// # What it cannot parse, it names
///
/// Four of the fifty-one structs do not verify: two in the line layer, where one block has no size
/// comment and another uses a bitmask type this does not model, and two in symbols, where offsets
/// jump over fields declared in a form this does not read. All four are outside R0 — line is R1
/// and symbols are R2 — so they are listed in `UNPARSED` rather than guessed at. A consumer that
/// needs one gets a compile error against a missing constant, not a wrong layout.
fn generate_ubo_layouts(mbgl: &Path) -> Result<String, String> {
    let revision = tree_revision(mbgl);
    let dir = mbgl.join("include/mbgl/shaders");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|err| format!("reading {}: {err}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "hpp"))
        .collect();
    // Filesystem order is not stable across machines, and the output is committed.
    files.sort();

    let mut layouts: Vec<UboLayout> = Vec::new();
    let mut unparsed: Vec<UnparsedUbo> = Vec::new();
    let mut union_sources: Vec<(String, String)> = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .map_err(|err| format!("reading {}: {err}", file.display()))?;
        let header = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let asserted = parse_ubo_size_asserts(&text);
        parse_ubo_layouts(&text, &header, &asserted, &mut layouts, &mut unparsed);
        union_sources.push((header, text));
    }
    layouts.sort_by(|a, b| a.name.cmp(&b.name));

    // Unions are resolved after every struct is known, because a union in one header may name a
    // block declared in another.
    let known: std::collections::BTreeMap<&str, u32> = layouts
        .iter()
        .map(|layout| (layout.name.as_str(), layout.stride))
        .collect();
    let mut unions: Vec<UboUnion> = Vec::new();
    for (header, text) in &union_sources {
        parse_ubo_unions(text, header, &known, &mut unions, &mut unparsed);
    }
    unions.sort_by(|a, b| a.name.cmp(&b.name));
    unparsed.sort_by(|a, b| a.name.cmp(&b.name));

    if layouts.is_empty() {
        return Err("no UBO layouts parsed; the header format has changed".to_string());
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
    writeln!(out, "// Layouts from include/mbgl/shaders/*_ubo.hpp.").unwrap();
    writeln!(out).unwrap();
    for line in wrap(
        "Uniform block layouts (DR-6). mbgl declares each block as a struct whose fields carry \
         their byte offsets in comments and whose closing comment is the size, checked there by \
         `static_assert`. Every layout here was accepted only after each field's declared offset \
         matched the running total and the total matched the declared size, so a header whose \
         comments had drifted would be rejected rather than mispack silently.",
        94,
    ) {
        if line.is_empty() {
            writeln!(out, "//!").unwrap();
        } else {
            writeln!(out, "//! {line}").unwrap();
        }
    }
    writeln!(out).unwrap();

    writeln!(out, "/// What a UBO field holds.").unwrap();
    writeln!(
        out,
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]"
    )
    .unwrap();
    writeln!(out, "pub enum UboFieldKind {{").unwrap();
    for (name, doc) in [
        ("F32", "A single `float`."),
        ("I32", "A signed 32-bit integer."),
        ("U32", "An unsigned 32-bit integer."),
        ("Vec2", "`std::array<float, 2>`."),
        ("Vec3", "`std::array<float, 3>`."),
        ("Vec4", "`std::array<float, 4>`."),
        (
            "Color",
            "`Color`, four floats. Premultiplied RGBA, as the style resolves it.",
        ),
        ("Mat4", "`std::array<float, 16>`, column-major."),
    ] {
        writeln!(out, "    /// {doc}").unwrap();
        writeln!(out, "    {name},").unwrap();
    }
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "impl UboFieldKind {{").unwrap();
    writeln!(out, "    /// Size in bytes.").unwrap();
    writeln!(out, "    #[must_use]").unwrap();
    writeln!(out, "    pub const fn size(self) -> u32 {{").unwrap();
    writeln!(out, "        match self {{").unwrap();
    writeln!(out, "            Self::F32 | Self::I32 | Self::U32 => 4,").unwrap();
    writeln!(out, "            Self::Vec2 => 8,").unwrap();
    writeln!(out, "            Self::Vec3 => 12,").unwrap();
    writeln!(out, "            Self::Vec4 | Self::Color => 16,").unwrap();
    writeln!(out, "            Self::Mat4 => 64,").unwrap();
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "/// One field of a uniform block.").unwrap();
    writeln!(out, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]").unwrap();
    writeln!(out, "pub struct UboField {{").unwrap();
    writeln!(out, "    /// Field name, as the header spells it.").unwrap();
    writeln!(out, "    pub name: &'static str,").unwrap();
    writeln!(out, "    /// Byte offset from the start of the block.").unwrap();
    writeln!(out, "    pub offset: u32,").unwrap();
    writeln!(out, "    /// What it holds.").unwrap();
    writeln!(out, "    pub kind: UboFieldKind,").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "/// A uniform block's layout.").unwrap();
    writeln!(out, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]").unwrap();
    writeln!(out, "pub struct UboLayout {{").unwrap();
    writeln!(out, "    /// Struct name, as the header spells it.").unwrap();
    writeln!(out, "    pub name: &'static str,").unwrap();
    writeln!(out, "    /// Header it came from.").unwrap();
    writeln!(out, "    pub header: &'static str,").unwrap();
    writeln!(out, "    /// Alignment the struct declares.").unwrap();
    writeln!(out, "    pub align: u32,").unwrap();
    writeln!(
        out,
        "    /// Where the fields end, which is mbgl's closing comment."
    )
    .unwrap();
    writeln!(out, "    pub size: u32,").unwrap();
    writeln!(
        out,
        "    /// `sizeof`: the field extent padded up to the alignment, and the stride between"
    )
    .unwrap();
    writeln!(
        out,
        "    /// consecutive blocks in a consolidated buffer. Equal to `size` unless the fields"
    )
    .unwrap();
    writeln!(out, "    /// end mid-alignment.").unwrap();
    writeln!(out, "    pub stride: u32,").unwrap();
    writeln!(out, "    /// Fields, in declaration order.").unwrap();
    writeln!(out, "    pub fields: &'static [UboField],").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    for layout in &layouts {
        let konst = screaming_snake(&layout.name);
        writeln!(
            out,
            "/// `{}` from `include/mbgl/shaders/{}`.",
            layout.name, layout.header
        )
        .unwrap();
        writeln!(out, "pub const {konst}: UboLayout = UboLayout {{").unwrap();
        writeln!(out, "    name: \"{}\",", layout.name).unwrap();
        writeln!(out, "    header: \"{}\",", layout.header).unwrap();
        writeln!(out, "    align: {},", layout.align).unwrap();
        writeln!(out, "    size: {},", layout.size).unwrap();
        writeln!(out, "    stride: {},", layout.stride).unwrap();
        writeln!(out, "    fields: &[").unwrap();
        for field in &layout.fields {
            writeln!(
                out,
                "        UboField {{ name: \"{}\", offset: {}, kind: UboFieldKind::{} }},",
                field.name, field.offset, field.kind
            )
            .unwrap();
        }
        writeln!(out, "    ],").unwrap();
        writeln!(out, "}};").unwrap();
        writeln!(out).unwrap();
    }

    writeln!(out, "/// Every layout, by name, for a lookup that does not").unwrap();
    writeln!(out, "/// hard-code which blocks exist.").unwrap();
    writeln!(out, "pub const LAYOUTS: [UboLayout; {}] = [", layouts.len()).unwrap();
    for layout in &layouts {
        writeln!(out, "    {},", screaming_snake(&layout.name)).unwrap();
    }
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "/// A union of drawable blocks.").unwrap();
    writeln!(out, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]").unwrap();
    writeln!(out, "pub struct UboUnion {{").unwrap();
    writeln!(out, "    /// Union name, as the header spells it.").unwrap();
    writeln!(out, "    pub name: &'static str,").unwrap();
    writeln!(out, "    /// Header it came from.").unwrap();
    writeln!(out, "    pub header: &'static str,").unwrap();
    writeln!(out, "    /// The blocks it can hold.").unwrap();
    writeln!(out, "    pub members: &'static [&'static str],").unwrap();
    writeln!(
        out,
        "    /// The largest member's stride, which is the stride of a consolidated buffer."
    )
    .unwrap();
    writeln!(out, "    pub stride: u32,").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    for item in &unions {
        let konst = screaming_snake(&item.name);
        writeln!(
            out,
            "/// `{}` from `include/mbgl/shaders/{}`.",
            item.name, item.header
        )
        .unwrap();
        writeln!(out, "pub const {konst}: UboUnion = UboUnion {{").unwrap();
        writeln!(out, "    name: \"{}\",", item.name).unwrap();
        writeln!(out, "    header: \"{}\",", item.header).unwrap();
        writeln!(out, "    members: &[").unwrap();
        for member in &item.members {
            writeln!(out, "        \"{member}\",").unwrap();
        }
        writeln!(out, "    ],").unwrap();
        writeln!(out, "    stride: {},", item.stride).unwrap();
        writeln!(out, "}};").unwrap();
        writeln!(out).unwrap();
    }

    writeln!(
        out,
        "/// Every union, for a lookup that does not hard-code which exist."
    )
    .unwrap();
    writeln!(out, "pub const UNIONS: [UboUnion; {}] = [", unions.len()).unwrap();
    for item in &unions {
        writeln!(out, "    {},", screaming_snake(&item.name)).unwrap();
    }
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();

    for line in wrap(
        "Blocks whose headers this generator will not vouch for, with the reason. Listed rather \
         than omitted: a block that is missing because it could not be parsed and one that is \
         missing because mbgl does not have it are different situations, and a caller that needs \
         one of these should find out from this list rather than from a wrong layout.",
        94,
    ) {
        writeln!(out, "/// {line}").unwrap();
    }
    writeln!(
        out,
        "pub const UNPARSED: [(&str, &str, &str); {}] = [",
        unparsed.len()
    )
    .unwrap();
    for item in &unparsed {
        writeln!(
            out,
            "    (\"{}\", \"{}\", \"{}\"),",
            item.name, item.header, item.reason
        )
        .unwrap();
    }
    writeln!(out, "];").unwrap();

    Ok(out)
}

/// Parses every `union NameUnionUBO { ... };` in a header.
///
/// A consolidated buffer is an array of these, not of the block a drawable happens to use: mbgl
/// sizes it `sizeof(union) * drawableCount` so every drawable's entry sits at a fixed stride
/// whatever variant it is. Packing at the individual block's size instead puts every entry after
/// the first at the wrong offset, and the symptom is a layer whose tiles are drawn with each
/// other's matrices.
///
/// A union naming a block this generator could not verify is itself unverifiable — its stride
/// depends on that block's size — so it is reported rather than sized from the members that
/// happen to be known.
fn parse_ubo_unions(
    text: &str,
    header: &str,
    known: &std::collections::BTreeMap<&str, u32>,
    unions: &mut Vec<UboUnion>,
    unparsed: &mut Vec<UnparsedUbo>,
) {
    let mut rest = text;
    while let Some(start) = rest.find("union ") {
        let after = &rest[start + "union ".len()..];
        let Some(brace) = after.find('{') else { break };
        let name = after[..brace].trim().to_string();
        let Some(end) = after[brace..].find("\n};") else {
            rest = &after[brace..];
            continue;
        };
        let body = &after[brace + 1..brace + end];
        rest = &after[brace + end..];

        if !name.ends_with("UBO") {
            continue;
        }

        let mut members = Vec::new();
        let mut missing = Vec::new();
        let mut stride = 0;
        for line in body.lines() {
            let line = line.trim();
            let Some(semicolon) = line.find(';') else {
                continue;
            };
            let declaration = &line[..semicolon];
            let Some(split) = declaration.rfind(char::is_whitespace) else {
                continue;
            };
            let member = declaration[..split].trim().to_string();
            match known.get(member.as_str()) {
                Some(&size) => stride = stride.max(size),
                None => missing.push(member.clone()),
            }
            members.push(member);
        }

        if members.is_empty() {
            continue;
        }
        if !missing.is_empty() {
            unparsed.push(UnparsedUbo {
                name,
                header: header.to_string(),
                reason: format!("names unverified blocks: {}", missing.join(", ")),
            });
            continue;
        }
        unions.push(UboUnion {
            name,
            header: header.to_string(),
            members,
            stride,
        });
    }
}

/// Reads `static_assert(sizeof(X) == N * 16);` into a map from name to `sizeof`.
///
/// mbgl writes these beside each block, so the header states its own answer for what the struct
/// occupies. Checking against it is what turns the offset comments from documentation into
/// something two independent statements agree on.
fn parse_ubo_size_asserts(text: &str) -> std::collections::BTreeMap<String, u32> {
    let mut sizes = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("static_assert(sizeof(") else {
            continue;
        };
        let Some(close) = rest.find(')') else {
            continue;
        };
        let name = rest[..close].trim().to_string();
        let Some(equals) = rest[close..].find("==") else {
            continue;
        };
        let expression = rest[close + equals + 2..]
            .trim_end_matches(|c: char| c == ')' || c == ';' || c.is_whitespace());
        // The form is either `N` or `N * 16`.
        let value: Option<u32> = match expression.split_once('*') {
            Some((left, right)) => left
                .trim()
                .parse::<u32>()
                .ok()
                .zip(right.trim().parse::<u32>().ok())
                .map(|(a, b)| a * b),
            None => expression.trim().parse::<u32>().ok(),
        };
        if let Some(value) = value {
            sizes.insert(name, value);
        }
    }
    sizes
}

/// Turns `FillDrawableUBO` into `FILL_DRAWABLE_UBO`.
fn screaming_snake(name: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if ch.is_uppercase() && index > 0 {
            let previous_lower = chars[index - 1].is_lowercase() || chars[index - 1].is_numeric();
            let next_lower = chars.get(index + 1).is_some_and(|c| c.is_lowercase());
            if previous_lower || (chars[index - 1].is_uppercase() && next_lower) {
                out.push('_');
            }
        }
        out.push(ch.to_ascii_uppercase());
    }
    out
}

/// Parses every `struct alignas(N) NameUBO { ... };` in a header.
fn parse_ubo_layouts(
    text: &str,
    header: &str,
    asserted: &std::collections::BTreeMap<String, u32>,
    layouts: &mut Vec<UboLayout>,
    unparsed: &mut Vec<UnparsedUbo>,
) {
    let mut rest = text;
    while let Some(start) = rest.find("struct alignas(") {
        let after = &rest[start + "struct alignas(".len()..];
        let Some(paren) = after.find(')') else { break };
        let align: u32 = after[..paren].trim().parse().unwrap_or(16);
        let after = &after[paren + 1..];
        let Some(brace) = after.find('{') else { break };
        let name = after[..brace].trim().to_string();
        let body_start = brace + 1;
        let Some(end) = after[body_start..].find("\n};") else {
            rest = &after[body_start..];
            continue;
        };
        let body = &after[body_start..body_start + end];
        rest = &after[body_start + end..];

        if !name.ends_with("UBO") {
            continue;
        }
        match parse_ubo_body(body) {
            Ok((fields, size)) => {
                // `sizeof` pads the field extent up to the alignment. mbgl asserts the padded
                // value in the same header, so where that assert exists it is a second,
                // independent check on the same struct — and a disagreement means the comments
                // and the assert have drifted from each other, which is worse than either
                // drifting alone.
                let stride = size.div_ceil(align) * align;
                if let Some(&declared) = asserted.get(&name)
                    && declared != stride
                {
                    unparsed.push(UnparsedUbo {
                        name,
                        header: header.to_string(),
                        reason: format!(
                            "fields end at {size} padding to {stride}, but the header asserts sizeof {declared}"
                        ),
                    });
                    continue;
                }
                layouts.push(UboLayout {
                    name,
                    header: header.to_string(),
                    align,
                    size,
                    stride,
                    fields,
                });
            }
            Err(reason) => unparsed.push(UnparsedUbo {
                name,
                header: header.to_string(),
                reason,
            }),
        }
    }
}

/// Parses a block body, verifying every offset against the running total.
fn parse_ubo_body(body: &str) -> Result<(Vec<UboField>, u32), String> {
    let mut fields = Vec::new();
    let mut running: u32 = 0;
    let mut declared_size: Option<u32> = None;

    for line in body.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("/*") else {
            continue;
        };
        let Some(close) = rest.find("*/") else {
            continue;
        };
        let Ok(offset) = rest[..close].trim().parse::<u32>() else {
            continue;
        };
        let tail = rest[close + 2..].trim();
        if tail.is_empty() {
            // The closing comment: the struct's size.
            declared_size = Some(offset);
            continue;
        }
        let Some(semicolon) = tail.find(';') else {
            return Err(format!("field `{tail}` has no terminator"));
        };
        let declaration = &tail[..semicolon];
        let Some(split) = declaration.rfind(char::is_whitespace) else {
            return Err(format!("cannot split `{declaration}`"));
        };
        let (kind_text, field_name) = declaration.split_at(split);
        let kind = ubo_field_kind(kind_text.trim())
            .ok_or_else(|| format!("unmodelled type `{}`", kind_text.trim()))?;

        if offset != running {
            return Err(format!(
                "field `{}` declares offset {offset} where the fields before it end at {running}",
                field_name.trim()
            ));
        }
        running += kind.1;
        fields.push(UboField {
            offset,
            kind: kind.0,
            name: field_name.trim().to_string(),
        });
    }

    let size = declared_size.ok_or_else(|| "no closing size comment".to_string())?;
    if size != running {
        return Err(format!(
            "declares size {size} where its fields end at {running}"
        ));
    }
    if fields.is_empty() {
        return Err("no fields".to_string());
    }
    Ok((fields, size))
}

/// Maps a C++ declaration to a field kind and its size.
///
/// Inline comments are stripped first. mbgl annotates some integer fields as `/*bool*/ int`,
/// which is a note about intent rather than a different type: the field occupies four bytes
/// either way, and refusing to read it would drop two symbol blocks over a comment.
fn ubo_field_kind(text: &str) -> Option<(&'static str, u32)> {
    let mut stripped = String::new();
    let mut rest = text;
    while let Some(open) = rest.find("/*") {
        stripped.push_str(&rest[..open]);
        let close = rest[open..].find("*/")?;
        rest = &rest[open + close + 2..];
    }
    stripped.push_str(rest);

    let text = stripped.as_str();
    let normalized: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    match normalized.as_str() {
        "float" => Some(("F32", 4)),
        "int32_t" | "int" => Some(("I32", 4)),
        "uint32_t" => Some(("U32", 4)),
        "std::array<float,2>" => Some(("Vec2", 8)),
        "std::array<float,3>" => Some(("Vec3", 12)),
        "std::array<float,4>" => Some(("Vec4", 16)),
        "Color" => Some(("Color", 16)),
        "std::array<float,16>" | "std::array<float,4*4>" => Some(("Mat4", 64)),
        _ => None,
    }
}

/// Generates the UBO slot ids (DR-6).
///
/// # Why this needs an evaluator rather than a parser
///
/// The slots are not constants in the header. They are a chain of anonymous C enums whose values
/// come from each other — `idFillDrawableUBO = idDrawableReservedVertexOnlyUBO`, which is
/// `layerSSBOStartId`, which is `globalUBOCount`, which is the length of another enum — with a
/// `std::max` over fifteen of those lengths in the middle and a macro at the end. Reading a
/// number out of it means evaluating it.
///
/// # The backend is not a parameter
///
/// Every step of the chain is `#if MLN_RENDER_BACKEND_VULKAN`-gated, and the gate changes the
/// answers: `getEnumValue(packed, unpacked)` takes the second argument under Vulkan and the
/// first everywhere else, which moves every layer's evaluated-props slot. DR-16 settled that
/// question — SSBO-only, Vulkan-first, no fallback path exists — so this evaluates the Vulkan
/// branch and says so, rather than parameterizing over backends the design has ruled out. A
/// build for some other backend would need a different table and would be a different decision
/// than a code-generation one.
fn generate_ubo_slots(mbgl: &Path) -> Result<String, String> {
    let revision = tree_revision(mbgl);
    let mut symbols: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    let mut ordered: Vec<(String, u32)> = Vec::new();

    // `shader_defines.hpp` includes `layer_ubo.hpp`, so the global enum has to be evaluated
    // first; the chain begins with its length.
    for relative in [
        "include/mbgl/shaders/layer_ubo.hpp",
        "include/mbgl/shaders/shader_defines.hpp",
    ] {
        let path = mbgl.join(relative);
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("reading {}: {err}", path.display()))?;
        evaluate_slot_source(&select_vulkan(&text), &mut symbols, &mut ordered)?;
    }

    for required in [
        "globalUBOCount",
        "idFillDrawableUBO",
        "idFillTilePropsUBO",
        "idFillEvaluatedPropsUBO",
        "idBackgroundDrawableUBO",
        "idBackgroundPropsUBO",
        "idGlobalPaintParamsUBO",
    ] {
        if !symbols.contains_key(required) {
            return Err(format!(
                "slot evaluation produced no value for `{required}`; the header shape has changed"
            ));
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
        "// Evaluated from include/mbgl/shaders/{{layer_ubo,shader_defines}}.hpp with"
    )
    .unwrap();
    writeln!(out, "// MLN_RENDER_BACKEND_VULKAN selected (DR-16).").unwrap();
    writeln!(out).unwrap();
    for line in wrap(
        "Uniform buffer slot ids (DR-6). Not constants in the header but a chain of anonymous \
         enums that take their values from each other, so these are evaluated rather than read. \
         Every step is gated on the render backend and the gate changes the answers; DR-16 \
         settled that as Vulkan-only, so this is the Vulkan chain and nothing else.",
        94,
    ) {
        if line.is_empty() {
            writeln!(out, "//!").unwrap();
        } else {
            writeln!(out, "//! {line}").unwrap();
        }
    }
    writeln!(out).unwrap();

    for (name, value) in &ordered {
        writeln!(out, "/// `{name}`").unwrap();
        writeln!(
            out,
            "pub const {}: u32 = {value};",
            screaming_snake_ident(name)
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    writeln!(
        out,
        "/// Every evaluated symbol, for a lookup that does not hard-code which exist."
    )
    .unwrap();
    writeln!(out, "pub const SLOTS: [(&str, u32); {}] = [", ordered.len()).unwrap();
    for (name, value) in &ordered {
        writeln!(out, "    (\"{name}\", {value}),").unwrap();
    }
    writeln!(out, "];").unwrap();

    Ok(out)
}

/// Keeps the `MLN_RENDER_BACKEND_VULKAN` branch of every conditional and drops the rest.
///
/// Deliberately narrow: it understands only the backend conditionals these two headers use, and
/// treats anything else as unconditional. A general preprocessor would be the wrong tool — the
/// point is not to compile C++ but to answer one question the same way the Vulkan build does.
fn select_vulkan(text: &str) -> String {
    let mut out = String::new();
    // `None` outside any conditional; `Some(true)` inside a branch that is taken.
    let mut stack: Vec<bool> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(condition) = trimmed.strip_prefix("#if ") {
            stack.push(condition.contains("MLN_RENDER_BACKEND_VULKAN"));
            continue;
        }
        if let Some(condition) = trimmed.strip_prefix("#elif ") {
            if let Some(last) = stack.last_mut() {
                *last = condition.contains("MLN_RENDER_BACKEND_VULKAN");
            }
            continue;
        }
        if trimmed == "#else" {
            if let Some(last) = stack.last_mut() {
                *last = !*last;
            }
            continue;
        }
        if trimmed == "#endif" {
            stack.pop();
            continue;
        }
        if stack.iter().all(|taken| *taken) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Walks a preprocessed header, evaluating `static constexpr` scalars and anonymous enums.
fn evaluate_slot_source(
    text: &str,
    symbols: &mut std::collections::BTreeMap<String, u32>,
    ordered: &mut Vec<(String, u32)>,
) -> Result<(), String> {
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("static constexpr uint32_t ") {
            let Some((name, expression)) = rest.split_once('=') else {
                continue;
            };
            // The initializer may run over several lines, as `layerUBOStartId`'s `std::max` does.
            let mut text = expression.to_string();
            while !text.contains(';') {
                let Some(next) = lines.next() else { break };
                text.push(' ');
                text.push_str(next.trim());
            }
            let value = evaluate_slot_expression(text.trim_end_matches(';').trim(), symbols)?;
            let name = name.trim().to_string();
            symbols.insert(name.clone(), value);
            ordered.push((name, value));
            continue;
        }

        if trimmed != "enum {" {
            continue;
        }

        // An anonymous enum: each name takes the previous value plus one unless it says otherwise.
        let mut next_value: u32 = 0;
        for body in lines.by_ref() {
            let body = body.trim();
            if body.starts_with('}') {
                break;
            }
            let body = body.split("//").next().unwrap_or(body).trim();
            let body = body.trim_end_matches(',').trim();
            if body.is_empty() {
                continue;
            }
            let (name, value) = match body.split_once('=') {
                Some((name, expression)) => (
                    name.trim().to_string(),
                    evaluate_slot_expression(expression.trim(), symbols)?,
                ),
                None => (body.to_string(), next_value),
            };
            next_value = value + 1;
            symbols.insert(name.clone(), value);
            ordered.push((name, value));
        }
    }
    Ok(())
}

/// Evaluates one enumerator or scalar initializer.
fn evaluate_slot_expression(
    expression: &str,
    symbols: &std::collections::BTreeMap<String, u32>,
) -> Result<u32, String> {
    let expression = expression.trim();

    if let Some(rest) = expression.strip_prefix("getEnumValue(") {
        // Under Vulkan the macro expands to its second argument. Everywhere else it is the
        // first, which is why the backend is not a free choice here (DR-16).
        let inner = rest.trim_end_matches(')');
        let Some((_packed, unpacked)) = inner.split_once(',') else {
            return Err(format!("cannot read `{expression}`"));
        };
        return evaluate_slot_expression(unpacked.trim(), symbols);
    }

    if let Some(rest) = expression.strip_prefix("std::max(") {
        let inner = rest
            .trim_end_matches(')')
            .trim()
            .trim_start_matches('{')
            .trim_end_matches('}');
        let mut largest = 0;
        for part in inner.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            largest = largest.max(evaluate_slot_expression(part, symbols)?);
        }
        return Ok(largest);
    }

    if let Some(rest) = expression.strip_prefix("static_cast<uint32_t>(") {
        return evaluate_slot_expression(rest.trim_end_matches(')'), symbols);
    }

    // `idCollisionCircleUBO = idCollisionDrawableUBO + 1` and friends. Split on the last
    // operator so the left side keeps any nesting, which is what makes `a + b + c` work without
    // a real expression parser.
    if let Some((left, right)) = expression.rsplit_once('+') {
        return Ok(evaluate_slot_expression(left.trim(), symbols)?
            + evaluate_slot_expression(right.trim(), symbols)?);
    }

    // Subtraction appears as a count of slots between two bases. Checked rather than wrapped:
    // a negative here would mean the chain had been reordered, and a slot id of four billion
    // would index a buffer that does not exist instead of saying so.
    if let Some((left, right)) = expression.rsplit_once('-') {
        let (left, right) = (
            evaluate_slot_expression(left.trim(), symbols)?,
            evaluate_slot_expression(right.trim(), symbols)?,
        );
        return left
            .checked_sub(right)
            .ok_or_else(|| format!("`{expression}` is negative: {left} - {right}"));
    }

    if let Ok(literal) = expression.parse::<u32>() {
        return Ok(literal);
    }

    symbols
        .get(expression)
        .copied()
        .ok_or_else(|| format!("unknown symbol `{expression}`"))
}

/// Turns `idFillDrawableUBO` into `ID_FILL_DRAWABLE_UBO`.
fn screaming_snake_ident(name: &str) -> String {
    screaming_snake(name)
}

/// Generates the expression operator registry (DR-6, DR-11).
///
/// # Why this has to be generated
///
/// The style spec says an array is an expression when its first element names a *registered
/// operator*, and a value otherwise. That rule is the only thing separating
/// `["Noto Sans Regular"]` — a font stack, and the overwhelmingly common spelling of
/// `text-font` — from a call to an operator of that name. Get it wrong in the permissive
/// direction and every font stack in every style is read as an expression, which loses the
/// glyphs; get it wrong in the strict direction and a real expression is read as a literal
/// array of strings.
///
/// A hand-maintained list is wrong the moment mbgl gains an operator, and wrong silently: the
/// symptom is a style that renders slightly differently, not a build failure. So the list comes
/// from mbgl's two registries and `--check` fails when they drift.
///
/// # The two registries
///
/// `parsing_context.cpp` holds the special forms — the ones with their own parse functions
/// because their arguments are not all expressions (`let`'s bindings, `match`'s labels,
/// `literal`'s payload). `compound_expression.cpp` holds everything else: the arithmetic, the
/// lookups, the string and colour functions, each with one or more typed signatures.
///
/// Names beginning `filter-` are excluded. They are mbgl's internal spelling for the legacy
/// filter syntax, generated when a legacy filter is converted, and never appear in a style
/// document — including them would let `["filter-in", ...]` parse as an expression when the
/// spec says it is not one.
fn generate_operators(mbgl: &Path) -> Result<String, String> {
    let revision = tree_revision(mbgl);

    let special_path = mbgl.join("src/mbgl/style/expression/parsing_context.cpp");
    let special_text = std::fs::read_to_string(&special_path)
        .map_err(|err| format!("reading {}: {err}", special_path.display()))?;
    let special = parse_registry(&special_text, "expressionRegistry");

    let compound_path = mbgl.join("src/mbgl/style/expression/compound_expression.cpp");
    let compound_text = std::fs::read_to_string(&compound_path)
        .map_err(|err| format!("reading {}: {err}", compound_path.display()))?;
    let compound = parse_registry(&compound_text, "compoundExpressionRegistry");

    if special.is_empty() {
        return Err(format!(
            "found no operators in {}: has expressionRegistry been renamed?",
            special_path.display()
        ));
    }
    if compound.is_empty() {
        return Err(format!(
            "found no operators in {}: has compoundExpressionRegistry been renamed?",
            compound_path.display()
        ));
    }

    let mut names: Vec<String> = special
        .into_iter()
        .chain(compound)
        // mbgl's spelling for converted legacy filters, which never appear in a style document.
        .filter(|name| !name.starts_with("filter-"))
        .collect();
    names.sort();
    names.dedup();

    let mut out = String::new();
    out.push_str("//! Expression operator names, generated from maplibre-native.\n");
    out.push_str("//!\n");
    out.push_str(&format!("//! Source revision: {revision}\n"));
    out.push_str("//!\n");
    out.push_str(
        "//! Do not edit: regenerate with `cargo run -p mbgl-codegen -- --mbgl <tree>`.\n",
    );
    out.push_str("//!\n");
    for line in wrap(
        "Taken from `expressionRegistry` in `parsing_context.cpp` (the special forms, whose \
         arguments are not all expressions) and `compoundExpressionRegistry` in \
         `compound_expression.cpp` (everything else). Names beginning `filter-` are excluded: \
         they are mbgl's internal spelling for converted legacy filters and never appear in a \
         style document.",
        96,
    ) {
        out.push_str(&format!("//! {line}\n"));
    }
    out.push('\n');

    out.push_str("/// Every name that heads an expression call.\n");
    out.push_str("///\n");
    for line in wrap(
        "Sorted, so `is_operator` can binary-search it and so a diff of this file is a diff of \
         what mbgl supports rather than of the order two registries happened to be written in.",
        96,
    ) {
        out.push_str(&format!("/// {line}\n"));
    }
    out.push_str(&format!(
        "pub const OPERATORS: [&str; {}] = [\n",
        names.len()
    ));
    for name in &names {
        out.push_str(&format!("    {name:?},\n"));
    }
    out.push_str("];\n");

    Ok(out)
}

/// Extracts the quoted keys of a `mapbox::eternal` map literal named `name`.
///
/// Regex-free, and deliberately narrow: it takes the text from the map's name to the closing
/// `});` and reads the first string of each `{"key", value}` pair. A renamed or restructured
/// registry yields nothing, which the caller turns into a hard error rather than a short table.
fn parse_registry(text: &str, name: &str) -> Vec<String> {
    let Some(start) = text.find(name) else {
        return Vec::new();
    };
    let rest = &text[start..];
    let end = rest.find("});").map_or(rest.len(), |at| at + 3);
    let body = &rest[..end];

    let mut names = Vec::new();
    let bytes = body.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        // Only a string that opens a pair counts, so the second element of `{"x", Foo::parse}`
        // and any stray quoted text in a comment are not mistaken for operator names.
        if bytes[at] == b'{'
            && let Some(quote) = body[at + 1..]
                .find('"')
                .filter(|offset| body[at + 1..at + 1 + offset].trim().is_empty())
        {
            let from = at + 1 + quote + 1;
            if let Some(len) = body[from..].find('"') {
                names.push(body[from..from + len].to_string());
                at = from + len + 1;
                continue;
            }
        }
        at += 1;
    }
    names
}

/// The Unicode blocks mbgl's `allowsIdeographicBreaking` consults, and their bounds.
///
/// Taken from `i18n.cpp`'s `DEFINE_IS_IN_UNICODE_BLOCK` table rather than from Blocks.txt, so
/// that this agrees with the engine rather than with the standard — mbgl comments out the
/// blocks it does not use, and a table built from the standard would silently include them.
///
/// Twenty ranges of four hex digits each is precisely the material that transcribes wrongly:
/// a bound one out is a line break that appears in the wrong place for one script, in one
/// language, and nowhere else.
fn generate_unicode_blocks(mbgl: &Path) -> Result<String, String> {
    let revision = tree_revision(mbgl);
    let path = mbgl.join("src/mbgl/util/i18n.cpp");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("reading {}: {err}", path.display()))?;

    // The blocks named by `allowsIdeographicBreaking`, which is the predicate that decides
    // where a line may break without a space.
    const IDEOGRAPHIC: &[&str] = &[
        "Bopomofo",
        "BopomofoExtended",
        "CJKCompatibility",
        "CJKCompatibilityForms",
        "CJKCompatibilityIdeographs",
        "CJKRadicalsSupplement",
        "CJKStrokes",
        "CJKSymbolsandPunctuation",
        "CJKUnifiedIdeographs",
        "CJKUnifiedIdeographsExtensionA",
        "EnclosedCJKLettersandMonths",
        "HalfwidthandFullwidthForms",
        "Hiragana",
        "IdeographicDescriptionCharacters",
        "KangxiRadicals",
        "Katakana",
        "KatakanaPhoneticExtensions",
        "VerticalForms",
        "YiRadicals",
        "YiSyllables",
    ];

    let mut found: Vec<(String, u32, u32)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // Commented-out blocks are ones mbgl does not use; skip them exactly as it does.
        if line.starts_with("//") {
            continue;
        }
        let Some(rest) = line.strip_prefix("DEFINE_IS_IN_UNICODE_BLOCK(") else {
            continue;
        };
        let Some(rest) = rest.split(')').next() else {
            continue;
        };
        let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
        if parts.len() != 3 {
            continue;
        }
        let name = parts[0].to_string();
        if !IDEOGRAPHIC.contains(&name.as_str()) {
            continue;
        }
        let parse = |value: &str| {
            u32::from_str_radix(value.trim_start_matches("0x").trim_start_matches("0X"), 16)
        };
        match (parse(parts[1]), parse(parts[2])) {
            (Ok(first), Ok(last)) => found.push((name, first, last)),
            _ => return Err(format!("{}: could not read {rest}", path.display())),
        }
    }

    let missing: Vec<&str> = IDEOGRAPHIC
        .iter()
        .copied()
        .filter(|name| !found.iter().any(|(found, _, _)| found == name))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{}: no DEFINE_IS_IN_UNICODE_BLOCK for {missing:?} — has i18n.cpp changed shape, or \
             has mbgl commented these out?",
            path.display()
        ));
    }

    found.sort_by_key(|(_, first, _)| *first);

    let mut out = String::new();
    out.push_str("//! Unicode blocks that permit a line break without a space, generated from\n");
    out.push_str("//! maplibre-native.\n");
    out.push_str("//!\n");
    out.push_str(&format!("//! Source revision: {revision}\n"));
    out.push_str("//!\n");
    out.push_str(
        "//! Taken from `i18n.cpp`'s own table rather than from Unicode's Blocks.txt: mbgl\n",
    );
    out.push_str(
        "//! comments out the blocks it does not consult, and a table built from the standard\n",
    );
    out.push_str("//! would include them and break lines where mbgl does not.\n");
    out.push_str("//!\n");
    out.push_str("//! Generated by `cargo run -p mbgl-codegen`. Do not edit.\n\n");
    out.push_str("/// A Unicode block, inclusive at both ends.\n");
    out.push_str("pub type Block = (u32, u32);\n\n");
    out.push_str("/// Every block `allowsIdeographicBreaking` consults, in codepoint order.\n");
    out.push_str(&format!(
        "pub const IDEOGRAPHIC_BLOCKS: [Block; {}] = [\n",
        found.len()
    ));
    for (name, first, last) in &found {
        out.push_str(&format!("    // {name}\n"));
        out.push_str(&format!("    (0x{first:04X}, 0x{last:04X}),\n"));
    }
    out.push_str("];\n");
    Ok(out)
}
