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

    let out = workspace.join(OUTPUT);
    let shader_out = workspace.join(SHADER_OUTPUT);
    if check {
        let mut stale = false;
        for (path, name, want) in [
            (&out, OUTPUT, &generated),
            (&shader_out, SHADER_OUTPUT, &shaders),
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

    if let Some(parent) = out.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("creating {}: {err}", parent.display());
        return ExitCode::FAILURE;
    }
    for (path, name, text) in [
        (&out, OUTPUT, &generated),
        (&shader_out, SHADER_OUTPUT, &shaders),
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
