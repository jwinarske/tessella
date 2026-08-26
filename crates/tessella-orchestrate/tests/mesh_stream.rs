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
