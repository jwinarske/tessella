//! Authored models on the wire.
//!
//! A model tile is the one thing this build draws that it did not compute. What travels is the
//! asset itself, and what the producer contributes is the decision to draw it and the transform
//! that places it — which is the same division as every other layer, seen from the far side.

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{
    AddReason, GeometryId, MeshAdd, MeshFormat, ViewId, WireRecord,
};
use tessella_capture_abi::ring::Ring;
use tessella_orchestrate::emit::{self, SlabArena};

/// A GLB with nothing much in it, which is enough: the envelope carries bytes, not meaning.
fn glb() -> Vec<u8> {
    let json = br#"{"asset":{"version":"2.0"},"nodes":[{}]}"#;
    let mut out = b"glTF".to_vec();
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&((12 + 8 + json.len()) as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(json);
    out
}

/// The bytes go in a slab and the record points at them.
///
/// A slab reference rather than an inline payload: a real model tile is hundreds of kilobytes and
/// the ring is sized by envelope count, not by tile turnover. It is also what makes the hand-off
/// zero-copy — a glTF loader parses straight from the slab's memory, and the consumer holds the
/// slab alive until it is done, which is exactly the lifetime an asynchronous load needs.
#[test]
fn a_mesh_travels_as_a_slab_reference() {
    let bytes = glb();
    let mut arena = SlabArena::default();
    let encoded = emit::encode_mesh(&mut arena, GeometryId(7), &bytes);

    assert!(
        encoded.payload.is_empty(),
        "the ring carries no model bytes"
    );

    let record = MeshAdd::from_bytes(&encoded.record).expect("a record");
    assert_eq!(record.mesh, GeometryId(7));
    assert_eq!(record.bytes.length as usize, bytes.len());
    assert_eq!(record.format, MeshFormat::Glb as u8);
    assert_eq!(record.reason, AddReason::Created as u8);
    assert_eq!(record._pad, [0; 2]);

    // And the slab actually holds them, at the offset the record names. `resolve` is what the
    // consumer's side of the §11.3 contract looks like from here.
    arena.seal();
    assert_eq!(
        arena.resolve(record.bytes).expect("the slab resolves"),
        &bytes[..]
    );
}

/// A mesh shares the geometry id space, so the records that bind and drop it are the existing ones.
///
/// One new envelope kind rather than four. The consumer needs no second table either: it looks an
/// id up and finds whichever kind of thing it added. What follows from that is worth stating —
/// a consumer that skips `MeshAdd` and then meets a `ViewUse` naming the id has a protocol fault,
/// which is the truth about a style with a model layer rather than a gap in the design.
#[test]
fn a_mesh_is_bound_and_dropped_by_the_geometry_records() {
    use tessella_capture_abi::envelope::{GeometryRemove, ViewRelease};

    // These compile against a mesh's id because the id space is one. That is the whole claim.
    let id = GeometryId(7);
    let release = ViewRelease {
        geometry: id,
        view: ViewId(1),
        _pad: 0,
    };
    let remove = GeometryRemove { geometry: id };
    assert_eq!(release.geometry, remove.geometry);
}

/// The announcement reaches the ring, and is lossless.
///
/// Lossless for the reason `GeometryAdd` is: it announces something that exists rather than a
/// state that can be superseded, so a dropped one leaves a later `ViewUse` naming an id the
/// consumer never saw.
#[test]
fn a_mesh_announcement_reaches_the_ring() {
    use tessella_capture_abi::CoalescePolicy;

    let mut ring = Ring::new(1 << 16);
    let (producer, consumer) = ring.split();

    let mut arena = SlabArena::default();
    let encoded = emit::encode_mesh(&mut arena, GeometryId(3), &glb());
    emit::write_mesh(producer, &encoded).expect("writes");

    let record = consumer.peek().expect("a record");
    assert_eq!(record.kind, EnvelopeKind::MeshAdd);
    assert_eq!(
        EnvelopeKind::MeshAdd.coalesce_policy(),
        CoalescePolicy::Lossless
    );
}

/// An unknown format is refused rather than guessed at.
#[test]
fn an_unknown_mesh_format_is_refused() {
    assert_eq!(MeshFormat::from_repr(1), Some(MeshFormat::Glb));
    assert_eq!(MeshFormat::from_repr(0), None);
    assert_eq!(MeshFormat::from_repr(2), None);
    assert_eq!(MeshFormat::from_repr(255), None);
}

/// Placement: where a model tile goes.
///
/// `height_factor` is still pinned here, because it is still mbgl's and still a real field —
/// of the *fill-extrusion* drawable block, where it walks a pattern up a wall. What changed is
/// that it is not a mesh's business and never was a conversion.
mod placement {
    use tessella_orchestrate::ubo::{
        MESH_DRAWABLE_UBO, MESH_DRAWABLE_UBO_SIZE, MeshPlacement, height_factor,
        pack_mesh_drawable_buffer,
    };

    /// A Mapbox buildings mesh is tile units in x and y and **metres** in z.
    ///
    /// Measured rather than assumed, across 972 nodes of a real store: node translations span
    /// 60 to 8189, which is the tile extent, while node z-scale is exactly 1.0 and mesh heights
    /// run to 330 with a 95th percentile of 136. Those are building heights in metres, not tile
    /// units — half the nodes are flat because a buildings tile carries a footprint mesh beside
    /// each extruded one.
    ///
    /// The *convention* is the same one `fill-extrusion` uses. The conversion is not
    /// `heightFactor`, which was the original claim here and was wrong: mbgl's position shader
    /// passes the height straight into the matrix, `gl_Position = matrix * vec4(pos, z, 1.0)`,
    /// because `getWorldToCamera` has already scaled its third column by `pixelsPerMeter`.
    /// `heightFactor` walks a pattern up a wall and appears nowhere else.
    #[test]
    fn the_height_factor_is_mbgls() {
        // `-numTiles / tileSize_D / 8.0`, with tileSize_D 512.
        assert_eq!(height_factor(0), -(1.0 / 4096.0));
        assert_eq!(height_factor(14), -4.0);
        assert_eq!(height_factor(16), -16.0);

        // Negative throughout: the sign is the shader's vertical convention, not a scale.
        for zoom in 0..=22u8 {
            assert!(height_factor(zoom) < 0.0, "z{zoom}");
        }

        // It doubles per zoom level, because a tile covers half the ground each time while its
        // unit count stays the same.
        for zoom in 0..22u8 {
            let ratio = height_factor(zoom + 1) / height_factor(zoom);
            assert!((ratio - 2.0).abs() < 1e-6, "z{zoom} -> {ratio}");
        }
    }

    /// A mesh's placement is the matrix and nothing else.
    ///
    /// It carried `height_factor` beside the matrix, as what a metre multiplies by, and that
    /// told a consumer to scale a building by four thousand at z14. The matrix already converts:
    /// its third column carries `pixelsPerMeter`, which is why mbgl's own shader passes a height
    /// in metres straight into it.
    #[test]
    fn a_placement_carries_no_conversion_of_its_own() {
        assert_eq!(
            MESH_DRAWABLE_UBO_SIZE,
            core::mem::size_of::<[f32; 16]>(),
            "a placement is a mat4; anything more is a conversion the matrix already did"
        );
    }

    /// The placement buffer is one entry per mesh, at the layer's stride.
    ///
    /// A consolidated buffer, like every other layer's: the consumer reads entry `i` for the
    /// `i`-th mesh named by that layer's `ViewUse` records.
    #[test]
    fn each_mesh_gets_its_own_entry() {
        let mut first = [0.0f32; 16];
        first[0] = 3.0;
        let mut second = [0.0f32; 16];
        second[15] = 9.0;

        let placements = [
            MeshPlacement { matrix: first },
            MeshPlacement { matrix: second },
        ];
        // A stride wider than the block is what an alignment requirement produces.
        let packed = pack_mesh_drawable_buffer(&placements, 96);
        assert_eq!(packed.len(), 192);

        let at = |offset: usize| {
            f32::from_le_bytes(packed[offset..offset + 4].try_into().expect("four bytes"))
        };
        assert_eq!(at(0), 3.0, "the first matrix");
        assert_eq!(at(60), 0.0, "and the rest of it");
        assert_eq!(at(96 + 60), 9.0, "the second matrix, one stride on");
        assert!(
            packed[MESH_DRAWABLE_UBO_SIZE..96]
                .iter()
                .all(|byte| *byte == 0),
            "the gap between blocks is not zeroed"
        );
    }

    /// The mesh slot sits clear of every slot mbgl generates.
    ///
    /// The first slot this build chooses rather than transcribes, because mbgl has no mesh layer
    /// to read one from. It is separated by a gap rather than placed adjacent, and the gap is
    /// asserted at compile time in the module — this is the readable statement of the same
    /// thing. Taking nine, one past mbgl's `MAX_UBO_COUNT_PER_SHADER`, would collide the moment
    /// mbgl added a shader's worth of buffer.
    #[test]
    fn the_mesh_slot_is_clear_of_mbgls() {
        use tessella_capture_abi::generated::ubo_slots::{MAX_UBO_COUNT_PER_SHADER, SLOTS};

        // The gap itself is asserted at compile time in the module; both of these are
        // constants, so the readable statement of it lives in a `const` block here too.
        const _: () = assert!(MESH_DRAWABLE_UBO > MAX_UBO_COUNT_PER_SHADER);
        const _: () = assert!(
            MESH_DRAWABLE_UBO - MAX_UBO_COUNT_PER_SHADER >= 4,
            "the gap is too small to be a gap"
        );

        // This one is a real search rather than a constant comparison: nothing anywhere in the
        // generated chain may already carry the number, whatever its name.
        assert!(
            SLOTS.iter().all(|(_, slot)| *slot != MESH_DRAWABLE_UBO),
            "a generated slot already uses this number"
        );
    }
}

/// A mesh's matrix is the drawable matrix, not a second one computed beside it.
///
/// A model tile sits in the same tile space as every other layer and takes the same layer and
/// sublayer depth bias. Computing it separately would leave two implementations of mbgl's bias
/// arithmetic to keep in step, and they would agree right up until the day they did not.
#[test]
fn a_mesh_uses_the_same_matrix_as_every_other_drawable() {
    use tessella_orchestrate::ubo::{DrawableEntry, MeshPlacement};
    use tessella_tile::camera;
    use tessella_tile::cover::ViewTransform;

    let view = camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 14.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    });

    let entry = DrawableEntry::for_tile(&view, 14, 8189, 5447, 0, 3, 1).expect("an entry");
    let placement = MeshPlacement::for_tile(&view, 14, 8189, 5447, 0, 3, 1).expect("a placement");

    assert_eq!(placement.matrix, entry.matrix);

    // And the bias is genuinely in there: a different sublayer is a different matrix.
    let deeper = MeshPlacement::for_tile(&view, 14, 8189, 5447, 0, 3, 2).expect("a placement");
    assert_ne!(
        deeper.matrix, placement.matrix,
        "the sublayer bias is not applied"
    );
}
