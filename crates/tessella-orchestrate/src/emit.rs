//! Turning buckets into envelopes: slabs, attribute descriptors, and the ring.
//!
//! # Slabs, and why the ring does not own geometry
//!
//! §2.1 kills rev 1's aliasing model. Vertex and index bytes live in refcounted slabs, and the
//! envelope carries a handle plus an offset and length rather than the bytes themselves. The
//! consumer holds the slab alive until the driver's copy completes — for Filament, until the
//! `BufferDescriptor` release callback fires (§11.3) — so geometry is touched exactly once
//! after layout, by the upload.
//!
//! That is also why geometry does not ride inline in the ring the way metadata does. Copying a
//! tile's vertices into the ring would be the copy §11.3 exists to avoid, and a ring sized to
//! hold them would be sized by tile turnover rather than by envelope count.
//!
//! # What an attribute descriptor says
//!
//! One descriptor per bound attribute, carrying where the bytes are and how to read them. The
//! position attribute is `Short2` at stride 4 with no offset, which is what the oracle emits
//! and what §12.4 asks for: i16 tile-local coordinates.
//!
//! Data-driven attributes are **not** emitted yet, and the reason is DR-6 rather than effort.
//! An attribute's id and binding slot come from the per-permutation attribute tables generated
//! from `shaders/*.hpp`, and the shader `permutationKey` that selects among them comes from the
//! same place. Those tables do not exist yet. Inventing ids here would produce a stream that
//! looks right and binds the wrong slots — the exact failure DR-6 makes the tables generated to
//! prevent — so a layer whose paint is data-driven emits its geometry and its position
//! attribute, and its per-feature attributes wait for the tables.

use alloc::sync::Arc;
use alloc::vec::Vec;

use tessella_capture_abi::envelope::{
    AddReason, AttributeDesc, GeometryAdd, GeometryId, Segment as AbiSegment, SlabRef, Span,
    WireRecord,
};
use tessella_capture_abi::ring::{Full, Producer};
use tessella_capture_abi::{AttributeDataType, BuiltIn, EnvelopeKind};
use tessella_layout::fill::FillBucket;

/// Shader-side id of the position attribute.
///
/// Observed in the oracle's dump as `id=0 bind=0 dt=9 ddt=9 off=0 voff=0 stride=4`. It is the
/// one attribute whose id is stable across every permutation, which is why it can be named here
/// while the rest wait for the generated tables.
pub const POSITION_ATTRIBUTE: u32 = 0;

/// Bytes per position: two i16.
const POSITION_STRIDE: u32 = 4;

/// Default slab size. Large enough that a tile's geometry rarely spans two, small enough that a
/// mostly-empty one is not worth worrying about.
const SLAB_BYTES: usize = 64 * 1024;

/// A refcounted block of geometry bytes.
#[derive(Debug)]
pub struct Slab {
    /// Handle the envelope carries.
    pub id: u32,
    bytes: Vec<u8>,
}

impl Slab {
    /// The bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Allocates geometry bytes into refcounted slabs.
///
/// Slabs are append-only while they are being filled and immutable once sealed, which is what
/// makes the §11.3 promise sound: the consumer can read a slab for as long as it holds a
/// reference, because nothing will rewrite it. A bucket that does not fit the current slab
/// starts a new one rather than being split, so a single attribute's bytes are always
/// contiguous.
#[derive(Debug, Default)]
pub struct SlabArena {
    sealed: Vec<Arc<Slab>>,
    open: Option<Slab>,
    next_id: u32,
}

impl SlabArena {
    /// An empty arena.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Copies `bytes` into a slab and returns a reference to them.
    ///
    /// An empty input still yields a valid reference, with zero length, so a caller does not
    /// have to special-case a bucket with no indices.
    pub fn alloc(&mut self, bytes: &[u8]) -> SlabRef {
        if bytes.is_empty() {
            return SlabRef {
                slab: self.open.as_ref().map_or(0, |slab| slab.id),
                offset: 0,
                length: 0,
            };
        }

        // A bucket larger than the default gets a slab of its own rather than being split:
        // an attribute's bytes have to be contiguous for a single offset and stride to
        // describe them.
        let needs_new = match &self.open {
            None => true,
            Some(slab) => slab.bytes.len() + bytes.len() > slab.bytes.capacity(),
        };
        if needs_new {
            self.seal();
            let capacity = SLAB_BYTES.max(bytes.len());
            self.open = Some(Slab {
                id: self.next_id,
                bytes: Vec::with_capacity(capacity),
            });
            self.next_id += 1;
        }

        let slab = self.open.as_mut().expect("just opened");
        #[allow(clippy::cast_possible_truncation)]
        let offset = slab.bytes.len() as u32;
        slab.bytes.extend_from_slice(bytes);
        #[allow(clippy::cast_possible_truncation)]
        SlabRef {
            slab: slab.id,
            offset,
            length: bytes.len() as u32,
        }
    }

    /// Seals the open slab, making it immutable and shareable.
    pub fn seal(&mut self) {
        if let Some(slab) = self.open.take() {
            self.sealed.push(Arc::new(slab));
        }
    }

    /// Every sealed slab.
    #[must_use]
    pub fn slabs(&self) -> &[Arc<Slab>] {
        &self.sealed
    }

    /// A sealed slab by id.
    #[must_use]
    pub fn slab(&self, id: u32) -> Option<&Arc<Slab>> {
        self.sealed.iter().find(|slab| slab.id == id)
    }

    /// Resolves a reference against the sealed slabs.
    ///
    /// `None` when the slab is not sealed yet, or the range does not fit it. The range check is
    /// not paranoia: a `SlabRef` read back off the ring is untrusted for the same reasons a
    /// span is.
    #[must_use]
    pub fn resolve(&self, reference: SlabRef) -> Option<&[u8]> {
        let slab = self.slab(reference.slab)?;
        let start = reference.offset as usize;
        let end = start.checked_add(reference.length as usize)?;
        slab.bytes.get(start..end)
    }
}

/// A geometry envelope and the payload bytes that follow it.
#[derive(Debug, Clone, PartialEq)]
pub struct Encoded {
    /// The fixed record.
    pub record: GeometryAdd,
    /// Attribute descriptors, segments and texture refs, in the layout the spans address.
    pub payload: Vec<u8>,
}

/// Encodes a fill bucket into a geometry envelope, allocating its bytes into `arena`.
///
/// The envelope carries no view: it is process-scoped and refcounted, and a `ViewUse` binds it
/// into a view's draw order (§5.3).
pub fn encode_fill(arena: &mut SlabArena, geometry: GeometryId, bucket: &FillBucket) -> Encoded {
    let vertex_bytes = as_bytes_i16(&bucket.vertices);
    let index_bytes = as_bytes_u16(&bucket.indices);

    let vertices = arena.alloc(&vertex_bytes);
    let indexes = arena.alloc(&index_bytes);

    let position = AttributeDesc {
        attr_id: POSITION_ATTRIBUTE,
        binding: 0,
        source: vertices,
        offset: 0,
        vertex_offset: 0,
        stride: POSITION_STRIDE,
        // Position is not zoom-interpolated, so the supplied and declared types agree. They
        // diverge for data-driven properties, where the shader declares the packed min/max
        // width and the binder supplies half of it (§2.2).
        data_type: AttributeDataType::Short2 as u8,
        declared_data_type: AttributeDataType::Short2 as u8,
        _pad: [0; 2],
    };

    let mut payload = Vec::new();
    let attrs = push_span(&mut payload, &[position]);
    let segments = push_span(
        &mut payload,
        &bucket
            .segments
            .iter()
            .map(|segment| AbiSegment {
                vertex_offset: segment.vertex_offset,
                index_offset: segment.index_offset,
                vertex_length: segment.vertex_length,
                index_length: segment.index_length,
            })
            .collect::<Vec<_>>(),
    );

    #[allow(clippy::cast_possible_truncation)]
    let record = GeometryAdd {
        geometry,
        // Zero until DR-6's generated permutation tables exist. A made-up key would select a
        // shader variant at the consumer, so it is left at the "no permutation" value rather
        // than guessed.
        permutation_key: 0,
        indexes,
        vertex_count: bucket.vertices.len() as u32,
        attrs,
        instance_attrs: Span::default(),
        segments,
        texture_refs: Span::default(),
        builtin_shader: BuiltIn::FillShader as i32,
        vertex_type: AttributeDataType::Short2 as u8,
        reason: AddReason::Created as u8,
        _pad: [0; 2],
    };

    Encoded { record, payload }
}

/// Writes an encoded envelope to the ring.
///
/// # Errors
///
/// [`Full`] when the ring cannot take it. Geometry is lossless, so the caller retries rather
/// than dropping (§4).
pub fn write(producer: &mut Producer, encoded: &Encoded) -> Result<(), Full> {
    producer.write(
        EnvelopeKind::GeometryAdd,
        encoded.record.as_bytes(),
        &encoded.payload,
    )
}

/// Appends `items` to the payload and returns the span addressing them.
fn push_span<T: WireRecord>(payload: &mut Vec<u8>, items: &[T]) -> Span {
    // Align to the payload region's requirement so an element is readable in place.
    while !payload
        .len()
        .is_multiple_of(tessella_capture_abi::envelope::PAYLOAD_ALIGN)
    {
        payload.push(0);
    }
    #[allow(clippy::cast_possible_truncation)]
    let offset = payload.len() as u32;
    for item in items {
        payload.extend_from_slice(item.as_bytes());
    }
    #[allow(clippy::cast_possible_truncation)]
    Span {
        offset,
        count: items.len() as u32,
    }
}

fn as_bytes_i16(values: &[[i16; 2]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value[0].to_le_bytes());
        out.extend_from_slice(&value[1].to_le_bytes());
    }
    out
}

fn as_bytes_u16(values: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 2);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket() -> FillBucket {
        tessella_layout::fill::build(&[alloc::vec![
            [10240, 4820],
            [10240, 10240],
            [2942, 10240],
            [2942, 4820],
            [10240, 4820],
        ]])
    }

    #[test]
    fn a_slab_reference_round_trips_its_bytes() {
        let mut arena = SlabArena::new();
        let first = arena.alloc(&[1, 2, 3, 4]);
        let second = arena.alloc(&[5, 6]);
        arena.seal();

        assert_eq!(arena.resolve(first), Some(&[1, 2, 3, 4][..]));
        assert_eq!(arena.resolve(second), Some(&[5, 6][..]));
        // The second allocation follows the first in the same slab.
        assert_eq!(first.slab, second.slab);
        assert_eq!(second.offset, 4);
    }

    /// A range that does not fit resolves to nothing rather than a truncated read. A `SlabRef`
    /// off the ring is untrusted for the same reasons a span is.
    #[test]
    fn an_out_of_range_reference_resolves_to_nothing() {
        let mut arena = SlabArena::new();
        let reference = arena.alloc(&[1, 2, 3, 4]);
        arena.seal();

        let overrun = SlabRef {
            length: reference.length + 1,
            ..reference
        };
        assert_eq!(arena.resolve(overrun), None);

        let missing = SlabRef {
            slab: reference.slab + 99,
            ..reference
        };
        assert_eq!(arena.resolve(missing), None);
    }

    /// An unsealed slab is not readable, which is what makes "immutable once sealed" mean
    /// something rather than being a comment.
    #[test]
    fn an_unsealed_slab_does_not_resolve() {
        let mut arena = SlabArena::new();
        let reference = arena.alloc(&[1, 2, 3]);
        assert_eq!(arena.resolve(reference), None, "not sealed yet");
        arena.seal();
        assert_eq!(arena.resolve(reference), Some(&[1, 2, 3][..]));
    }

    /// A bucket larger than the default slab gets one of its own rather than being split: an
    /// attribute's bytes must be contiguous for one offset and stride to describe them.
    #[test]
    fn an_oversized_allocation_is_kept_contiguous() {
        let mut arena = SlabArena::new();
        let big = alloc::vec![7u8; SLAB_BYTES * 2];
        let reference = arena.alloc(&big);
        arena.seal();

        assert_eq!(reference.length as usize, big.len());
        assert_eq!(arena.resolve(reference), Some(big.as_slice()));
    }

    #[test]
    fn encoding_describes_the_bucket() {
        let mut arena = SlabArena::new();
        let bucket = bucket();
        let encoded = encode_fill(&mut arena, GeometryId(7), &bucket);
        arena.seal();

        assert_eq!(encoded.record.geometry, GeometryId(7));
        assert_eq!(encoded.record.vertex_count, 5);
        assert_eq!(encoded.record.builtin_shader(), Some(BuiltIn::FillShader));
        assert_eq!(
            encoded.record.vertex_type(),
            Some(AttributeDataType::Short2)
        );
        assert_eq!(encoded.record.reason(), Some(AddReason::Created));
        assert_eq!(encoded.record.attrs.count, 1, "position only for now");
        assert_eq!(encoded.record.segments.count, 1);
        assert_eq!(encoded.record.instance_attrs.count, 0);

        // The index bytes are two triangles of u16.
        let indexes = arena.resolve(encoded.record.indexes).expect("indexes");
        assert_eq!(indexes.len(), bucket.indices.len() * 2);
    }

    /// The vertex bytes a consumer would upload are the bucket's coordinates, little-endian
    /// i16 pairs at stride 4.
    #[test]
    fn the_position_attribute_addresses_the_vertex_bytes() {
        let mut arena = SlabArena::new();
        let bucket = bucket();
        let encoded = encode_fill(&mut arena, GeometryId(1), &bucket);
        arena.seal();

        let (start, end) = encoded
            .record
            .attrs
            .extent::<AttributeDesc>(encoded.payload.len())
            .expect("the attr span fits");
        let attr = AttributeDesc::from_bytes(&encoded.payload[start..end]).expect("a descriptor");

        assert_eq!(attr.attr_id, POSITION_ATTRIBUTE);
        assert_eq!(attr.stride, POSITION_STRIDE);
        assert_eq!(attr.data_type(), Some(AttributeDataType::Short2));
        assert_eq!(attr.declared_data_type(), Some(AttributeDataType::Short2));

        let vertices = arena.resolve(attr.source).expect("vertex bytes");
        assert_eq!(vertices.len(), bucket.vertices.len() * 4);
        assert_eq!(&vertices[0..2], &10240i16.to_le_bytes());
        assert_eq!(&vertices[2..4], &4820i16.to_le_bytes());
    }

    /// Every span must resolve within the payload it was built against, which is the check a
    /// consumer performs before dereferencing.
    #[test]
    fn every_span_fits_its_payload() {
        let mut arena = SlabArena::new();
        let encoded = encode_fill(&mut arena, GeometryId(1), &bucket());
        let len = encoded.payload.len();

        assert!(encoded.record.attrs.extent::<AttributeDesc>(len).is_some());
        assert!(encoded.record.segments.extent::<AbiSegment>(len).is_some());
        assert!(
            encoded
                .record
                .instance_attrs
                .extent::<AttributeDesc>(len)
                .is_some(),
            "an empty span still resolves"
        );
    }
}
