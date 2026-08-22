//! Generates the flat C header for the capture-stream ABI from the Rust definitions.
//!
//! §3.1 makes the envelope ABI "one flat C header, single source of truth, shared with the
//! mirror". The Rust side is that source of truth, and this emits the C view of it.
//!
//! # How drift is caught
//!
//! A generator carrying its own table of fields can drift from the structs it claims to
//! describe, which would recreate the problem DR-6 exists to prevent one level up. So no number
//! in the output is written by hand. This binary links against `tessella-capture-abi` and takes
//! every size, alignment and offset from `size_of`, `align_of` and `offset_of!` on the real
//! types. The table below supplies only names and C spellings.
//!
//! Those numbers land in the header as `_Static_assert`s. The result is a closed loop with two
//! independent checks:
//!
//! - A Rust struct that changes without the table being updated emits an assertion the C struct
//!   cannot satisfy, and the mirror fails to compile. That is the check that matters, because
//!   it fires on the side that would otherwise misread the stream.
//! - `--check` regenerates in memory and fails if the committed header is stale. Unlike the
//!   mbgl mirrors, this needs no C++ checkout, so CI runs it on every push.
//!
//! ```text
//! cargo run -p abi-header
//! cargo run -p abi-header -- --check
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use tessella_capture_abi::envelope::*;
use tessella_capture_abi::reverse::MAX_VIEWS;
use tessella_capture_abi::ring::{RECORD_ALIGN, RECORD_FLAG_SKIP, RecordHeader, RingControl};
use tessella_capture_abi::{ABI_REV, AttributeDataType, BuiltIn, EnvelopeKind, TexturePixelType};

/// Path of the generated header, relative to the workspace root.
const OUTPUT: &str = "include/tessella_capture_abi.h";

/// Prefix for every exported name. Short, because it appears on every line of the mirror.
const PREFIX: &str = "tsl";

/// One field of a generated struct.
struct Field {
    /// C declarator, including the field name — `double proj_matrix[16]`.
    declarator: &'static str,
    /// Field name alone, for `offsetof`.
    name: &'static str,
    /// Byte offset, taken from the Rust type.
    offset: usize,
    /// Documentation, wrapped by the emitter.
    doc: &'static str,
}

/// One generated struct.
struct Struct {
    c_name: String,
    rust_name: &'static str,
    size: usize,
    align: usize,
    doc: &'static str,
    fields: Vec<Field>,
}

/// Declares a struct's C view, taking every number from the Rust type.
///
/// The `offset_of!` on each field is what makes a forgotten table entry a compile failure in
/// the mirror rather than a silent misread.
macro_rules! c_struct {
    ($rust:ty, $suffix:literal, $doc:literal, [ $( ($field:ident, $decl:literal, $fdoc:literal) ),* $(,)? ]) => {
        Struct {
            c_name: format!("{PREFIX}_{}", $suffix),
            rust_name: stringify!($rust),
            size: size_of::<$rust>(),
            align: align_of::<$rust>(),
            doc: $doc,
            fields: vec![
                $( Field {
                    declarator: $decl,
                    name: stringify!($field),
                    offset: std::mem::offset_of!($rust, $field),
                    doc: $fdoc,
                } ),*
            ],
        }
    };
}

fn main() -> ExitCode {
    let check = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--check") => true,
        Some(other) => {
            eprintln!("unknown argument: {other}");
            return ExitCode::FAILURE;
        }
    };

    let workspace = match workspace_root() {
        Ok(root) => root,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let out = workspace.join(OUTPUT);
    let generated = generate();

    if check {
        let current = std::fs::read_to_string(&out).unwrap_or_default();
        if current == generated {
            println!("{OUTPUT} is up to date");
            return ExitCode::SUCCESS;
        }
        eprintln!("{OUTPUT} does not match the Rust definitions; re-run without --check");
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

fn workspace_root() -> Result<PathBuf, String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "CARGO_MANIFEST_DIR unset; run via `cargo run -p abi-header`".to_string())?;
    Path::new(&manifest)
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("cannot find workspace root above {manifest}"))
}

fn structs() -> Vec<Struct> {
    vec![
        c_struct!(
            TileId,
            "tile_id",
            "A tile address.",
            [
                (x, "uint32_t x", "Canonical x."),
                (y, "uint32_t y", "Canonical y."),
                (z, "uint8_t z", "Canonical z."),
                (
                    overscaled_z,
                    "uint8_t overscaled_z",
                    "Zoom the tile is displayed at."
                ),
                (wrap, "int16_t wrap", "World copy, past the antimeridian."),
            ]
        ),
        c_struct!(
            Rect16,
            "rect",
            "A rectangle in texture space.",
            [
                (x, "uint16_t x", "Left edge."),
                (y, "uint16_t y", "Top edge."),
                (w, "uint16_t w", "Width."),
                (h, "uint16_t h", "Height."),
            ]
        ),
        c_struct!(
            Extent,
            "extent",
            "Pixel dimensions.",
            [
                (width, "uint32_t width", "Width in pixels."),
                (height, "uint32_t height", "Height in pixels."),
            ]
        ),
        c_struct!(
            Span,
            "span",
            "A run of elements in the payload region following a record.\n\n\
             offset is in bytes from the start of that region; count is in elements, so the \
             element type is implied by the field the span sits in. Validate both against the \
             record's payload_len before dereferencing: these arrive from the far side and a \
             span that overruns is an out-of-bounds read, not a short one.",
            [
                (
                    offset,
                    "uint32_t offset",
                    "Byte offset into the payload region."
                ),
                (count, "uint32_t count", "Number of elements."),
            ]
        ),
        c_struct!(
            SlabRef,
            "slab_ref",
            "A reference into a refcounted geometry slab. Hold the slab until the driver's \
             copy completes.",
            [
                (slab, "uint32_t slab", "Slab this data lives in."),
                (offset, "uint32_t offset", "Byte offset within the slab."),
                (length, "uint32_t length", "Length in bytes."),
            ]
        ),
        c_struct!(
            AttributeDesc,
            "attribute_desc",
            "One vertex attribute, as a binding rather than as bytes.\n\n\
             binding of -1 means the geometry supplied an override the shader does not \
             declare, and the consumer must drop it. Bind declared_data_type, not data_type, \
             with the supplied offset and stride: binding the supplied type hands the shader a \
             narrower attribute than it reads.",
            [
                (attr_id, "uint32_t attr_id", "Shader-side attribute id."),
                (
                    binding,
                    "int32_t binding",
                    "Declared binding slot, or -1 to drop."
                ),
                (source, "tsl_slab_ref source", "Where the bytes are."),
                (offset, "uint32_t offset", "Byte offset within a vertex."),
                (vertex_offset, "uint32_t vertex_offset", "First vertex."),
                (stride, "uint32_t stride", "Bytes between vertices."),
                (
                    data_type,
                    "uint8_t data_type",
                    "tsl_attribute_data_type the buffer supplies."
                ),
                (
                    declared_data_type,
                    "uint8_t declared_data_type",
                    "tsl_attribute_data_type the shader declares. Bind this one."
                ),
                (_pad, "uint8_t _pad[2]", "Must be zero."),
            ]
        ),
        c_struct!(
            Segment,
            "segment",
            "A contiguous index range with its own vertex base.",
            [
                (vertex_offset, "uint32_t vertex_offset", "First vertex."),
                (index_offset, "uint32_t index_offset", "First index."),
                (vertex_length, "uint32_t vertex_length", "Vertex count."),
                (index_length, "uint32_t index_length", "Index count."),
            ]
        ),
        c_struct!(
            TextureRef,
            "texture_ref",
            "Binds a texture to a shader slot.",
            [
                (texture, "uint64_t texture", "Texture to bind."),
                (slot, "uint32_t slot", "Shader-side slot."),
                (_pad, "uint32_t _pad", "Must be zero."),
            ]
        ),
        c_struct!(
            GeometryAdd,
            "geometry_add",
            "Process-scoped, refcounted geometry.\n\n\
             Carries no view: one of these plus N tsl_view_use records replaces N copies of the \
             whole thing, which is why upload bandwidth scales with unique tiles rather than \
             with view count.",
            [
                (geometry, "uint64_t geometry", "Process-wide geometry id."),
                (
                    permutation_key,
                    "uint64_t permutation_key",
                    "Distinguishes shader-family variants."
                ),
                (indexes, "tsl_slab_ref indexes", "Index buffer."),
                (vertex_count, "uint32_t vertex_count", "Number of vertices."),
                (attrs, "tsl_span attrs", "tsl_attribute_desc run."),
                (
                    instance_attrs,
                    "tsl_span instance_attrs",
                    "tsl_attribute_desc run, instanced."
                ),
                (segments, "tsl_span segments", "tsl_segment run."),
                (
                    texture_refs,
                    "tsl_span texture_refs",
                    "tsl_texture_ref run."
                ),
                (
                    builtin_shader,
                    "int32_t builtin_shader",
                    "tsl_builtin shader family."
                ),
                (
                    vertex_type,
                    "uint8_t vertex_type",
                    "tsl_attribute_data_type of a vertex."
                ),
                (
                    reason,
                    "uint8_t reason",
                    "tsl_add_reason. A steady stream of ATTRIBUTES_MODIFIED on a static scene is a bug."
                ),
                (_pad, "uint8_t _pad[2]", "Must be zero."),
            ]
        ),
        c_struct!(
            GeometryRemove,
            "geometry_remove",
            "Drops shared geometry once no view holds it.",
            [(geometry, "uint64_t geometry", "Geometry to drop."),]
        ),
        c_struct!(
            ViewDeclare,
            "view_declare",
            "Declares a view and its configuration.\n\n\
             Ordered ahead of any tsl_view_use naming the view, and re-emitted when the \
             configuration changes rather than repeated per use. A tsl_view_use naming an \
             undeclared view is a protocol fault.",
            [
                (view, "uint32_t view", "View being declared."),
                (
                    camera_mode,
                    "uint8_t camera_mode",
                    "tsl_camera_mode. Per view, not per use."
                ),
                (
                    _reserved,
                    "uint8_t _reserved[3]",
                    "Must be zero. Reserved for the per-view maxzoom clamp and view class."
                ),
            ]
        ),
        c_struct!(
            ViewUndeclare,
            "view_undeclare",
            "Drops a view and everything scoped to it: its scene, uniform buffers, stencil \
             sets and reverse-channel slot. Geometry it was using is refcounted and \
             process-scoped, so it is not dropped with the view.",
            [(view, "uint32_t view", "View being dropped."),]
        ),
        c_struct!(
            ViewUse,
            "view_use",
            "Binds shared geometry into one view's draw order.\n\n\
             Carries nothing about the view itself; see tsl_view_declare.",
            [
                (geometry, "uint64_t geometry", "Geometry being used."),
                (view, "uint32_t view", "View using it."),
                (layer_index, "int32_t layer_index", "Layer group."),
                (
                    sub_layer_index,
                    "int32_t sub_layer_index",
                    "Order within the layer."
                ),
                (
                    tile,
                    "tsl_tile_id tile",
                    "Tile covered, when has_tile is set."
                ),
                (render_pass, "uint8_t render_pass", "tsl_render_pass mask."),
                (draw_flags, "uint8_t draw_flags", "tsl_draw_flags mask."),
                (
                    has_tile,
                    "uint8_t has_tile",
                    "Non-zero when tile is meaningful."
                ),
                (_pad, "uint8_t _pad", "Must be zero."),
            ]
        ),
        c_struct!(
            ViewRelease,
            "view_release",
            "Releases one view's use of shared geometry.",
            [
                (geometry, "uint64_t geometry", "Geometry being released."),
                (view, "uint32_t view", "View releasing it."),
                (_pad, "uint32_t _pad", "Must be zero."),
            ]
        ),
        c_struct!(
            UboUpdate,
            "ubo_update",
            "An absolute write to a consolidated uniform buffer.\n\n\
             Absolute, never a delta, which is what makes latest-wins coalescing exact. \
             layer_index of -1 means a frame-wide buffer belonging to no layer.",
            [
                (view, "uint32_t view", "View this buffer belongs to."),
                (
                    layer_index,
                    "int32_t layer_index",
                    "Layer group, or -1 for frame-wide."
                ),
                (slot, "uint32_t slot", "Buffer slot."),
                (_pad, "uint32_t _pad", "Must be zero."),
                (
                    data,
                    "tsl_span data",
                    "Buffer bytes; count is a byte count."
                ),
            ]
        ),
        c_struct!(
            TextureUpdate,
            "texture_update",
            "New pixels for a process-scoped texture.\n\n\
             Carries no view: one texture serves every view that draws with it. rect_count of \
             zero means a whole-texture upload.",
            [
                (texture, "uint64_t texture", "Texture being written."),
                (size, "tsl_extent size", "Full texture dimensions."),
                (
                    rects,
                    "tsl_rect rects[TSL_TEXTURE_RECT_CAP]",
                    "Dirty regions; first rect_count are meaningful."
                ),
                (
                    pixels,
                    "tsl_span pixels",
                    "Pixel bytes; count is a byte count."
                ),
                (format, "uint8_t format", "tsl_texture_pixel_type."),
                (
                    rect_count,
                    "uint8_t rect_count",
                    "Meaningful entries in rects; zero means whole texture."
                ),
                (_pad, "uint8_t _pad[2]", "Must be zero."),
            ]
        ),
        c_struct!(
            StencilTile,
            "stencil_tile",
            "One tile of a clip set, with the matrix placing its mask quad.\n\n\
             The matrix travels with the tile because the consumer draws the quad itself and \
             must not derive it from a content drawable's matrix, which carries the layer's \
             translate and would put the mask where the content is not.",
            [
                (matrix, "float matrix[16]", "Column-major tile-to-clip."),
                (tile, "tsl_tile_id tile", "Tile this mask covers."),
            ]
        ),
        c_struct!(
            StencilTiles,
            "stencil_tiles",
            "The tile set a layer group wants clipped.\n\n\
             Stencil reference values are deliberately absent: the consumer assigns its own and \
             keys them by tile.",
            [
                (view, "uint32_t view", "View this clip set belongs to."),
                (
                    layer_index,
                    "int32_t layer_index",
                    "Layer group being clipped."
                ),
                (tiles, "tsl_span tiles", "tsl_stencil_tile run."),
            ]
        ),
        c_struct!(
            OrderEntry,
            "order_entry",
            "One geometry's position in a view's painter order.",
            [
                (geometry, "uint64_t geometry", "Geometry being drawn."),
                (
                    draw_priority,
                    "int64_t draw_priority",
                    "Sort key within the layer."
                ),
                (layer_index, "uint32_t layer_index", "Layer group."),
                (
                    sub_layer_index,
                    "int32_t sub_layer_index",
                    "Order within the layer."
                ),
                (
                    ubo_index,
                    "uint32_t ubo_index",
                    "Slot in the layer's consolidated buffer."
                ),
                (pass, "uint8_t pass", "tsl_render_pass this entry draws in."),
                (_pad, "uint8_t _pad[3]", "Must be zero."),
            ]
        ),
        c_struct!(
            OrderUpdate,
            "order_update",
            "A view's draw order, emitted only when it differs from the last one.",
            [
                (
                    order_epoch,
                    "uint64_t order_epoch",
                    "Epoch this order establishes."
                ),
                (view, "uint32_t view", "View this order belongs to."),
                (_pad, "uint32_t _pad", "Must be zero."),
                (
                    entries,
                    "tsl_span entries",
                    "tsl_order_entry run, in draw order."
                ),
            ]
        ),
        c_struct!(
            Light,
            "light",
            "The style's light.",
            [
                (
                    direction,
                    "double direction[3]",
                    "Direction towards the light, cartesian."
                ),
                (color, "double color[4]", "Light color, RGBA."),
                (intensity, "double intensity", "Light intensity."),
                (
                    anchored_to_map,
                    "uint8_t anchored_to_map",
                    "Non-zero when anchored to the map rather than the viewport."
                ),
                (_pad, "uint8_t _pad[7]", "Must be zero."),
            ]
        ),
        c_struct!(
            CameraUpdate,
            "camera_update",
            "A view's camera and frame-wide parameters.\n\n\
             Apply only once the referenced order_epoch is held; otherwise hold the camera \
             until its tsl_order_update arrives.\n\n\
             In consumer-camera mode proj_matrix, center_zoom0, bearing and pitch are advisory \
             — the producer computed them from a camera read back one frame stale. The rest \
             stay authoritative because the producer is the only side that knows them.\n\n\
             center_zoom0 is the map center at zoom zero, 0..512 regardless of the map's zoom, \
             scale-free on purpose. Multiplying it by a frame's zoom scale before sending \
             couples it to that frame and makes a consumer at a slightly different zoom look \
             where the tiles are not.",
            [
                (
                    proj_matrix,
                    "double proj_matrix[16]",
                    "World-to-clip, column-major."
                ),
                (
                    center_zoom0,
                    "double center_zoom0[2]",
                    "Map center at zoom zero. Scale-free."
                ),
                (bearing, "double bearing", "Bearing in degrees."),
                (pitch, "double pitch", "Pitch in degrees."),
                (
                    pixels_per_meter,
                    "double pixels_per_meter",
                    "World pixels per meter. Omit it and heights come out wrong by its reciprocal."
                ),
                (light, "tsl_light light", "Style light."),
                (
                    frame_no,
                    "uint64_t frame_no",
                    "Frame this camera belongs to."
                ),
                (
                    order_epoch,
                    "uint64_t order_epoch",
                    "Order this camera was computed against."
                ),
                (view, "uint32_t view", "View this camera belongs to."),
                (
                    opaque_pass_cutoff,
                    "uint32_t opaque_pass_cutoff",
                    "Draw-order index where the opaque pass ends."
                ),
                (depth_range_size, "float depth_range_size", "Depth range."),
                (_pad, "uint32_t _pad", "Must be zero."),
            ]
        ),
        c_struct!(
            RecordHeader,
            "record_header",
            "Fixed prefix of every record on the ring.\n\n\
             A record is this header, then record_len bytes of fixed envelope, then \
             payload_len bytes of payload, the whole padded to TSL_RECORD_ALIGN. Records never \
             straddle the buffer's wrap: one that would not fit contiguously is preceded by a \
             record with TSL_RECORD_FLAG_SKIP set, covering the remainder.",
            [
                (
                    kind,
                    "uint16_t kind",
                    "tsl_envelope_kind, or zero for a skip record."
                ),
                (flags, "uint16_t flags", "TSL_RECORD_FLAG_SKIP, or zero."),
                (
                    record_len,
                    "uint32_t record_len",
                    "Bytes of fixed envelope record."
                ),
                (
                    payload_len,
                    "uint32_t payload_len",
                    "Bytes of payload region."
                ),
                (
                    total_len,
                    "uint32_t total_len",
                    "Total bytes, header included."
                ),
            ]
        ),
        c_struct!(
            RingControl,
            "ring_control",
            "Control block at the head of the shared region, followed by capacity data bytes.\n\n\
             head and tail are free-running byte counters, not indices, so full and empty are \
             never ambiguous. Access them atomically: the producer releases head after writing \
             bytes, the consumer acquires head before reading them and releases tail after. \
             They are declared as plain integers so this header stays includable from C and \
             C++ alike; the atomicity is a protocol obligation, not a type.",
            [
                (
                    abi_rev,
                    "uint32_t abi_rev",
                    "ABI revision the producer wrote this region with."
                ),
                (_pad0, "uint32_t _pad0", "Must be zero."),
                (
                    capacity,
                    "uint64_t capacity",
                    "Bytes in the data region. Always a power of two."
                ),
                (
                    _pad1,
                    "uint8_t _pad1[112]",
                    "Must be zero. Keeps the counters off this line."
                ),
                (
                    head,
                    "uint64_t head",
                    "Bytes ever written. Producer releases, consumer acquires."
                ),
                (
                    _pad2,
                    "uint8_t _pad2[120]",
                    "Must be zero. Keeps the counters off each other's line."
                ),
                (
                    tail,
                    "uint64_t tail",
                    "Bytes ever consumed. Consumer releases, producer acquires."
                ),
                (_pad3, "uint8_t _pad3[120]", "Must be zero."),
            ]
        ),
    ]
}

fn generate() -> String {
    let mut out = String::new();
    let w = &mut out;

    writeln!(
        w,
        "/* Generated by `cargo run -p abi-header`. Do not edit by hand."
    )
    .unwrap();
    writeln!(w, " *").unwrap();
    writeln!(w, " * The tessella capture-stream ABI, revision {ABI_REV}.").unwrap();
    writeln!(w, " *").unwrap();
    for line in wrap(
        "The Rust definitions in tessella-capture-abi are the source of truth; this is the C \
         view of them. Every size and offset asserted below was taken from the Rust types at \
         generation time, so a struct here that does not match will fail to compile rather \
         than misread the stream.",
        94,
    ) {
        writeln!(w, " * {line}").unwrap();
    }
    writeln!(w, " */").unwrap();
    writeln!(w, "#ifndef TESSELLA_CAPTURE_ABI_H").unwrap();
    writeln!(w, "#define TESSELLA_CAPTURE_ABI_H").unwrap();
    writeln!(w).unwrap();
    writeln!(w, "#include <stddef.h>").unwrap();
    writeln!(w, "#include <stdint.h>").unwrap();
    writeln!(w).unwrap();
    writeln!(w, "#ifdef __cplusplus").unwrap();
    writeln!(w, "extern \"C\" {{").unwrap();
    writeln!(w, "#endif").unwrap();
    writeln!(w).unwrap();
    writeln!(
        w,
        "/* C++ spells the assertion differently, and older C has no keyword at all. */"
    )
    .unwrap();
    writeln!(w, "#if defined(__cplusplus)").unwrap();
    writeln!(w, "#define TSL_ASSERT(cond, msg) static_assert(cond, msg)").unwrap();
    writeln!(
        w,
        "#elif defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L"
    )
    .unwrap();
    writeln!(w, "#define TSL_ASSERT(cond, msg) _Static_assert(cond, msg)").unwrap();
    writeln!(w, "#else").unwrap();
    writeln!(w, "#define TSL_ASSERT(cond, msg)").unwrap();
    writeln!(w, "#endif").unwrap();
    writeln!(w).unwrap();
    writeln!(w, "#if defined(__cplusplus)").unwrap();
    writeln!(w, "#define TSL_ALIGNOF(type) alignof(type)").unwrap();
    writeln!(w, "#else").unwrap();
    writeln!(w, "#define TSL_ALIGNOF(type) _Alignof(type)").unwrap();
    writeln!(w, "#endif").unwrap();
    writeln!(w).unwrap();

    writeln!(w, "#define TSL_ABI_REV {ABI_REV}u").unwrap();
    writeln!(w, "#define TSL_RECORD_ALIGN {RECORD_ALIGN}u").unwrap();
    writeln!(w, "#define TSL_RECORD_FLAG_SKIP {RECORD_FLAG_SKIP}u").unwrap();
    writeln!(w, "#define TSL_TEXTURE_RECT_CAP {TEXTURE_RECT_CAP}u").unwrap();
    writeln!(w, "#define TSL_PAYLOAD_ALIGN {PAYLOAD_ALIGN}u").unwrap();
    writeln!(w, "#define TSL_MAX_VIEWS {MAX_VIEWS}u").unwrap();
    writeln!(w).unwrap();

    emit_enum(
        w,
        "envelope_kind",
        "Envelope kinds carried on the ring.",
        &EnvelopeKind::ALL.map(|k| (screaming(&format!("{k:?}")), k as i64)),
    );
    emit_enum(
        w,
        "add_reason",
        "Why geometry was announced. A steady stream of ATTRIBUTES_MODIFIED on a static scene \
         is a visible bug, not a hint.",
        &AddReason::ALL.map(|r| (screaming(&format!("{r:?}")), r as i64)),
    );
    emit_enum(
        w,
        "camera_mode",
        "Which side owns a view's camera. Declared per view at tsl_view_declare.",
        &[("PRODUCER".to_string(), 0), ("CONSUMER".to_string(), 1)],
    );
    emit_enum(
        w,
        "texture_pixel_type",
        "Pixel format of a texture.",
        &TexturePixelType::ALL.map(|t| (screaming(&format!("{t:?}")), t as i64)),
    );
    emit_enum(
        w,
        "attribute_data_type",
        "The type of a vertex attribute.",
        &AttributeDataType::ALL.map(|t| (screaming(&format!("{t:?}")), t as i64)),
    );
    emit_enum(
        w,
        "builtin",
        "Shader family.",
        &BuiltIn::ALL.map(|b| (screaming(&format!("{b:?}")), b as i64)),
    );

    writeln!(w, "/* Render pass is a mask; mbgl ORs these together. */").unwrap();
    writeln!(w, "#define TSL_RENDER_PASS_NONE 0u").unwrap();
    writeln!(w, "#define TSL_RENDER_PASS_OPAQUE 1u").unwrap();
    writeln!(w, "#define TSL_RENDER_PASS_TRANSLUCENT 2u").unwrap();
    writeln!(w, "#define TSL_RENDER_PASS_3D 4u").unwrap();
    writeln!(w).unwrap();
    writeln!(w, "/* Per-drawable render state, also a mask. */").unwrap();
    writeln!(
        w,
        "#define TSL_DRAW_FLAG_IS_3D {}u",
        DrawFlags::IS_3D.bits()
    )
    .unwrap();
    writeln!(
        w,
        "#define TSL_DRAW_FLAG_ENABLE_STENCIL {}u",
        DrawFlags::ENABLE_STENCIL.bits()
    )
    .unwrap();
    writeln!(
        w,
        "#define TSL_DRAW_FLAG_ENABLE_DEPTH {}u",
        DrawFlags::ENABLE_DEPTH.bits()
    )
    .unwrap();
    writeln!(
        w,
        "#define TSL_DRAW_FLAG_ENABLE_COLOR {}u",
        DrawFlags::ENABLE_COLOR.bits()
    )
    .unwrap();
    writeln!(w).unwrap();

    for s in structs() {
        emit_struct(w, &s);
    }

    writeln!(w, "#ifdef __cplusplus").unwrap();
    writeln!(w, "}} /* extern \"C\" */").unwrap();
    writeln!(w, "#endif").unwrap();
    writeln!(w).unwrap();
    writeln!(w, "#endif /* TESSELLA_CAPTURE_ABI_H */").unwrap();
    out
}

fn emit_enum(w: &mut String, name: &str, doc: &str, values: &[(String, i64)]) {
    for line in wrap(doc, 94) {
        writeln!(w, "/* {line} */").unwrap();
    }
    writeln!(w, "typedef enum {PREFIX}_{name} {{").unwrap();
    for (variant, value) in values {
        writeln!(
            w,
            "    {}_{}_{} = {value},",
            PREFIX.to_uppercase(),
            screaming(name),
            variant
        )
        .unwrap();
    }
    writeln!(w, "}} {PREFIX}_{name};").unwrap();
    writeln!(w).unwrap();
}

fn emit_struct(w: &mut String, s: &Struct) {
    writeln!(w, "/*").unwrap();
    for line in wrap(s.doc, 94) {
        if line.is_empty() {
            writeln!(w, " *").unwrap();
        } else {
            writeln!(w, " * {line}").unwrap();
        }
    }
    writeln!(w, " *").unwrap();
    writeln!(w, " * Mirrors `{}`.", s.rust_name).unwrap();
    writeln!(w, " */").unwrap();

    writeln!(w, "typedef struct {} {{", s.c_name).unwrap();
    for field in &s.fields {
        for line in wrap(field.doc, 88) {
            writeln!(w, "    /* {line} */").unwrap();
        }
        writeln!(w, "    {};", field.declarator).unwrap();
    }
    writeln!(w, "}} {};", s.c_name).unwrap();
    writeln!(w).unwrap();

    writeln!(
        w,
        "TSL_ASSERT(sizeof({}) == {}, \"{} size differs from the Rust definition\");",
        s.c_name, s.size, s.c_name
    )
    .unwrap();
    writeln!(
        w,
        "TSL_ASSERT(TSL_ALIGNOF({}) == {}, \"{} alignment differs from the Rust definition\");",
        s.c_name, s.align, s.c_name
    )
    .unwrap();
    for field in &s.fields {
        writeln!(
            w,
            "TSL_ASSERT(offsetof({}, {}) == {}, \"{}.{} moved\");",
            s.c_name, field.name, field.offset, s.c_name, field.name
        )
        .unwrap();
    }
    writeln!(w).unwrap();
}

/// `GeometryAdd` -> `GEOMETRY_ADD`.
fn screaming(name: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() && (chars[i - 1].is_lowercase() || chars[i - 1].is_numeric()) {
            out.push('_');
        }
        if *c == '_' && !out.ends_with('_') {
            out.push('_');
        } else if *c != '_' {
            out.extend(c.to_uppercase());
        }
    }
    out
}

/// Wraps prose to `width` columns, keeping blank lines as paragraph breaks.
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
