//! Binary glTF, as a 3D-buildings tile arrives in it.
//!
//! Every rule under test comes out of a published specification: glTF 2.0 and both of the
//! extensions a buildings tile requires — `KHR_mesh_quantization` and `EXT_meshopt_compression` —
//! are Khronos documents. What the fixtures below carry is the *shape* a real store's files
//! have, which was measured across 141 of them: `ATTRIBUTES` and `TRIANGLES` modes, `EXPONENTIAL`,
//! `OCTAHEDRAL` and `NONE` filters, and `POSITION`, `NORMAL`, `TEXCOORD_0` and
//! `_FEATURE_ID_RGBA4444` attributes.

#![cfg(feature = "gltf")]

use tessella_source::gltf::{self, ComponentType, ElementType, Filter, GltfError, Mode};

/// Wraps a JSON document and a binary chunk into a GLB.
fn glb(json: &str, binary: &[u8]) -> Vec<u8> {
    let mut json_chunk = json.as_bytes().to_vec();
    while !json_chunk.len().is_multiple_of(4) {
        json_chunk.push(b' ');
    }
    let mut bin_chunk = binary.to_vec();
    while !bin_chunk.len().is_multiple_of(4) {
        bin_chunk.push(0);
    }

    let total = 12
        + 8
        + json_chunk.len()
        + if bin_chunk.is_empty() {
            0
        } else {
            8 + bin_chunk.len()
        };
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_chunk.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json_chunk);
    if !bin_chunk.is_empty() {
        out.extend_from_slice(&(bin_chunk.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin_chunk);
    }
    out
}

/// A buildings tile's shape: two buffers, compressed views, a placed node.
fn buildings() -> Vec<u8> {
    glb(
        r#"{
          "asset": {"version": "2.0", "generator": "gltfpack 0.20",
                    "extras": {"MAPBOX_mesh_features": true}},
          "extensionsUsed": ["KHR_mesh_quantization", "EXT_meshopt_compression",
                             "KHR_texture_transform"],
          "extensionsRequired": ["KHR_mesh_quantization", "EXT_meshopt_compression"],
          "buffers": [{"byteLength": 64},
                      {"byteLength": 4096,
                       "extensions": {"EXT_meshopt_compression": {"fallback": true}}}],
          "bufferViews": [
            {"buffer": 1, "byteOffset": 0, "byteLength": 1200, "byteStride": 12,
             "extensions": {"EXT_meshopt_compression": {
                "buffer": 0, "byteOffset": 0, "byteLength": 32, "byteStride": 12,
                "mode": "ATTRIBUTES", "filter": "EXPONENTIAL", "count": 100}}},
            {"buffer": 1, "byteOffset": 1200, "byteLength": 600,
             "extensions": {"EXT_meshopt_compression": {
                "buffer": 0, "byteOffset": 32, "byteLength": 24, "byteStride": 2,
                "mode": "TRIANGLES", "count": 300}}}],
          "accessors": [
            {"bufferView": 0, "byteOffset": 0, "componentType": 5126, "count": 100,
             "type": "VEC3", "min": [-96.375, -157.53125, 0], "max": [96.375, 157.5625, 12]},
            {"bufferView": 1, "byteOffset": 0, "componentType": 5123, "count": 300,
             "type": "SCALAR"},
            {"bufferView": 0, "byteOffset": 0, "componentType": 5120, "count": 100,
             "type": "VEC3", "normalized": true}],
          "meshes": [{"primitives": [
            {"attributes": {"POSITION": 0, "NORMAL": 2}, "indices": 1, "material": 0}]}],
          "nodes": [{"matrix": [3.6, 0, 0, 0, 0, -3.6, 0, 0, 0, 0, 1, 0, 3553.5, 7593.25, 0, 1],
                     "mesh": 0,
                     "extras": {"mapbox:footprint:id": "147850320",
                                "mapbox:footprint:version": "1.0.0"}}],
          "materials": [{"pbrMetallicRoughness": {"baseColorFactor": [0, 0, 0, 1]}}]
        }"#,
        &[7u8; 64],
    )
}

/// The container reads, and the two-buffer arrangement resolves the way it must.
///
/// A meshopt file declares two buffers: buffer 0 is the binary chunk with the compressed bytes,
/// buffer 1 is larger, empty, and marked `fallback` — the destination sized for the decompressed
/// result. Every view points at the fallback for its own span and carries an extension saying
/// where its real bytes are. A reader taking the views at face value reads a buffer that does not
/// exist; one ignoring the extension reads compressed bytes as vertices.
#[test]
fn a_buildings_tile_reads_through_its_fallback_buffer() {
    let model = gltf::parse(&buildings()).expect("parses");

    assert_eq!(model.buffers, vec![64, 4096]);
    assert_eq!(
        model.fallback_buffers,
        vec![false, true],
        "the larger buffer is the destination and holds nothing"
    );
    assert!(model.mapbox_mesh_features);

    // The view's own span addresses the fallback; its compressed span addresses the binary.
    let view = &model.views[0];
    assert_eq!(view.buffer, 1);
    assert_eq!(view.byte_length, 1200);
    let compressed = view.compressed.as_ref().expect("compressed");
    assert_eq!(compressed.buffer, 0);
    assert_eq!(compressed.byte_length, 32);
    assert_eq!(compressed.mode, Mode::Attributes);
    assert_eq!(compressed.filter, Filter::Exponential);
    assert_eq!(compressed.count, 100);

    // And the compressed bytes resolve out of the binary chunk.
    assert_eq!(model.compressed_bytes(0).expect("bytes").len(), 32);
    assert_eq!(model.compressed_bytes(1).expect("bytes").len(), 24);
}

/// Accessors carry the quantization the extension exists for.
///
/// `KHR_mesh_quantization` permits integer component types on attributes that glTF would
/// otherwise require to be floats. A normal stored as three signed bytes is `normalized`, and a
/// reader ignoring that flag gets values in the hundreds where it wanted values around one.
#[test]
fn a_quantized_normal_is_marked_normalized() {
    let model = gltf::parse(&buildings()).expect("parses");

    let position = &model.accessors[0];
    assert_eq!(position.component_type, ComponentType::Float);
    assert_eq!(position.element_type, ElementType::Vec3);
    assert!(!position.normalized);
    assert_eq!(position.element_size(), 12);
    assert_eq!(position.max, vec![96.375, 157.5625, 12.0]);

    let normal = &model.accessors[2];
    assert_eq!(normal.component_type, ComponentType::Byte);
    assert!(normal.normalized, "a byte normal read raw is a hundredfold");
    assert_eq!(normal.element_size(), 3);
}

/// A node carries where the building stands, and which footprint it is.
///
/// The matrix is not decoration: a buildings tile puts the whole placement there — a scale into
/// tile units and a translation to the footprint's corner. The footprint id is Mapbox's own, and
/// is carried rather than interpreted because nothing else in the file identifies a building.
#[test]
fn a_node_carries_its_placement_and_its_footprint() {
    let model = gltf::parse(&buildings()).expect("parses");
    let node = &model.nodes[0];

    assert_eq!(node.mesh, Some(0));
    assert_eq!(node.matrix[0], 3.6, "scale x");
    assert_eq!(node.matrix[5], -3.6, "scale y, flipped");
    assert_eq!(node.matrix[12], 3553.5, "translate x");
    assert_eq!(node.matrix[13], 7593.25, "translate y");
    assert_eq!(node.footprint_id.as_deref(), Some("147850320"));

    // A node without a matrix is the identity rather than zeros, which would collapse the mesh
    // to a point.
    let plain =
        gltf::parse(&glb(r#"{"asset":{"version":"2.0"},"nodes":[{}]}"#, &[])).expect("parses");
    assert_eq!(plain.nodes[0].matrix[0], 1.0);
    assert_eq!(plain.nodes[0].matrix[15], 1.0);
    assert_eq!(plain.nodes[0].matrix[1], 0.0);
}

/// Primitive attributes come back in a stable order.
///
/// A JSON object has none, so two readings of one file would otherwise differ in a way that is
/// not in the file — which a caller comparing two tiles would see as a change.
#[test]
fn attributes_are_ordered() {
    let model = gltf::parse(&buildings()).expect("parses");
    let primitive = &model.meshes[0][0];
    let names: Vec<&str> = primitive
        .attributes
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(names, vec!["NORMAL", "POSITION"]);
    assert_eq!(primitive.attribute("POSITION"), Some(0));
    assert_eq!(primitive.attribute("TEXCOORD_0"), None);
    assert_eq!(primitive.indices, Some(1));
    assert_eq!(primitive.material, Some(0));
}

/// A required extension this build does not implement is refused by name.
///
/// glTF's own conformance rule: `extensionsRequired` is the file saying it cannot be read
/// correctly without it. Draco-compressed geometry read as though it were uncompressed is not a
/// degraded mesh, it is noise — so the refusal names the extension rather than producing one.
#[test]
fn an_unimplemented_required_extension_is_refused() {
    let bytes = glb(
        r#"{"asset":{"version":"2.0"},
            "extensionsRequired":["KHR_draco_mesh_compression"]}"#,
        &[],
    );
    match gltf::parse(&bytes) {
        Err(GltfError::UnsupportedExtension(name)) => {
            assert_eq!(name, "KHR_draco_mesh_compression");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // A merely *used* extension is not a refusal. `KHR_texture_transform` is used by a buildings
    // tile and not required: a viewer ignoring it gets the texture at the wrong scale rather
    // than a broken mesh, so the file does not insist.
    gltf::parse(&buildings()).expect("a used-but-not-required extension is fine");
}

/// A reference outside what it names is refused rather than followed.
///
/// Every index in a glTF comes from a JSON document that arrived over a network. They are
/// checked at parse rather than at use, so a malformed file is one error rather than a read that
/// happens to land inside another mesh's vertices.
#[test]
fn a_reference_out_of_range_is_refused() {
    for (what, json) in [
        (
            "buffer",
            r#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":8}],
                "bufferViews":[{"buffer":9,"byteOffset":0,"byteLength":4}]}"#,
        ),
        (
            "accessor",
            r#"{"asset":{"version":"2.0"},
                "meshes":[{"primitives":[{"attributes":{"POSITION":7}}]}]}"#,
        ),
        (
            "mesh",
            r#"{"asset":{"version":"2.0"},"nodes":[{"mesh":3}]}"#,
        ),
    ] {
        let result = gltf::parse(&glb(json, &[0; 8]));
        assert!(
            matches!(result, Err(GltfError::BadReference { .. })),
            "{what}: {result:?}"
        );
    }
}

/// A span past the end of its buffer is refused.
#[test]
fn a_span_past_its_buffer_is_refused() {
    let bytes = glb(
        r#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":8}],
            "bufferViews":[{"buffer":0,"byteOffset":4,"byteLength":900}]}"#,
        &[0; 8],
    );
    assert!(matches!(
        gltf::parse(&bytes),
        Err(GltfError::OutOfBounds { .. })
    ));

    // And a compressed span past the binary chunk, which is the one that would read another
    // mesh's bytes as this one's.
    let compressed = glb(
        r#"{"asset":{"version":"2.0"},
            "buffers":[{"byteLength":8},{"byteLength":99,
                        "extensions":{"EXT_meshopt_compression":{"fallback":true}}}],
            "bufferViews":[{"buffer":1,"byteOffset":0,"byteLength":99,
              "extensions":{"EXT_meshopt_compression":{"buffer":0,"byteOffset":4,
                "byteLength":900,"byteStride":4,"mode":"ATTRIBUTES","count":9}}}]}"#,
        &[0; 8],
    );
    assert!(matches!(
        gltf::parse(&compressed),
        Err(GltfError::OutOfBounds { .. })
    ));
}

/// glTF 1.0 is refused by number rather than attempted.
///
/// A different format rather than an earlier dialect: different material model, different buffer
/// layout. Reading one as 2.0 produces a mesh, and it is the wrong mesh.
#[test]
fn version_one_is_refused() {
    let mut bytes = buildings();
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(gltf::parse(&bytes), Err(GltfError::Version(1)));
}

/// Bytes that are not a GLB, and a GLB with nothing in it.
#[test]
fn a_malformed_container_is_refused() {
    assert_eq!(gltf::parse(b"not a gltf"), Err(GltfError::NotGlb));
    assert_eq!(gltf::parse(&[]), Err(GltfError::NotGlb));

    // A valid header with no JSON chunk.
    let mut header = b"glTF".to_vec();
    header.extend_from_slice(&2u32.to_le_bytes());
    header.extend_from_slice(&12u32.to_le_bytes());
    assert!(matches!(gltf::parse(&header), Err(GltfError::Json(_))));
}

/// An unknown chunk is skipped, which glTF requires.
///
/// The format is extended by adding chunks, and a reader refusing them could not read a later
/// file that is otherwise entirely readable.
#[test]
fn an_unknown_chunk_is_skipped() {
    let json = br#"{"asset":{"version":"2.0"},"nodes":[{}]}"#;
    let mut out = b"glTF".to_vec();
    out.extend_from_slice(&2u32.to_le_bytes());
    let total = 12 + 8 + json.len() + 8 + 4;
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(json);
    // A chunk from a future version of the format.
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(b"FUTR");
    out.extend_from_slice(&[1, 2, 3, 4]);

    let model = gltf::parse(&out).expect("skips what it does not know");
    assert_eq!(model.nodes.len(), 1);
}
