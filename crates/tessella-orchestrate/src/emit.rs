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
    AddReason, AttributeDesc, GeometryAdd, GeometryId, GeometryRemove, Segment as AbiSegment,
    SlabRef, Span, WireRecord,
};
use tessella_capture_abi::ring::{Full, Producer};
use tessella_capture_abi::{AttributeDataType, BuiltIn, EnvelopeKind};
use tessella_layout::fill::FillBucket;

use crate::binder::VertexLayout;

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
///
/// `layout` and `attributes` come from the binder. Every data-driven attribute references the
/// *same* interleaved buffer at a different offset, which is what the oracle does — its three
/// data-driven descriptors share one source hash and differ only in `off`.
pub fn encode_fill(
    arena: &mut SlabArena,
    geometry: GeometryId,
    bucket: &FillBucket,
    layout: &VertexLayout,
    attributes: &[u8],
) -> Encoded {
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

    // One allocation for the whole interleaved buffer, shared by every data-driven attribute.
    // Allocating per attribute would give each its own slab and lose the interleaving that the
    // stride describes.
    let interleaved = arena.alloc(attributes);

    let mut descriptors = alloc::vec![position];
    for attribute in &layout.attributes {
        descriptors.push(AttributeDesc {
            attr_id: attribute.attr_id,
            // -1 when the shader declares no slot; the consumer drops it but the bytes stay,
            // because another shader reading this bucket may declare it (§2.2).
            binding: attribute.binding,
            source: interleaved,
            offset: attribute.offset,
            vertex_offset: 0,
            stride: layout.stride,
            data_type: attribute.supplied as u8,
            declared_data_type: attribute.declared as u8,
            _pad: [0; 2],
        });
    }

    let mut payload = Vec::new();
    let attrs = push_span(&mut payload, &descriptors);
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

/// Drops a shared geometry.
///
/// Emitted when the last view releases it, not when one does: geometry is process-scoped and
/// refcounted (§5.3), so a remove sent on the first release would pull a tile out from under
/// every other view still drawing it. The caller owns the refcount; this is only the envelope.
///
/// # Errors
///
/// [`Full`] when the ring cannot take it. Geometry is lossless, so the caller retries rather
/// than dropping (§4) — and dropping a remove in particular would leak the geometry at the
/// consumer for as long as the stream lives.
pub fn remove(producer: &mut Producer, geometry: GeometryId) -> Result<(), Full> {
    let record = GeometryRemove { geometry };
    producer.write(EnvelopeKind::GeometryRemove, record.as_bytes(), &[])
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
        let encoded = encode_fill(
            &mut arena,
            GeometryId(7),
            &bucket,
            &VertexLayout::default(),
            &[],
        );
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
        let encoded = encode_fill(
            &mut arena,
            GeometryId(1),
            &bucket,
            &VertexLayout::default(),
            &[],
        );
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
        let encoded = encode_fill(
            &mut arena,
            GeometryId(1),
            &bucket(),
            &VertexLayout::default(),
            &[],
        );
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

#[cfg(test)]
mod descriptor_tests {
    use super::*;
    use crate::binder::{FeatureVertices, layout, pack_attributes};
    use tessella_capture_abi::envelope::WireRecord;
    use tessella_capture_abi::{AttributeDataType, declared_for};
    use tessella_source::geojson;
    use tessella_style::{Source, Style};

    const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");

    /// Everything the oracle's data-driven fill drawable declares, reproduced end to end.
    ///
    /// ```text
    /// id=0 bind=0  dt=9  ddt=9   off=0  stride=4
    /// id=1 bind=1  dt=26 ddt=28  off=0  stride=20
    /// id=2 bind=2  dt=25 ddt=26  off=8  stride=20
    /// id=3 bind=-1 dt=26 ddt=255 off=12 stride=20
    /// ```
    #[test]
    fn the_descriptors_match_the_oracles() {
        let style = Style::parse(HERMETIC).expect("style parses");
        let layer = style.layer("fill-datadriven").expect("the layer");
        let paint = tessella_style::property::resolve_paint(layer).expect("resolves");

        let ids = [
            ("idFillColorVertexAttribute", 1u32),
            ("idFillOpacityVertexAttribute", 2),
            ("idFillOutlineColorVertexAttribute", 3),
        ]
        .into_iter()
        .map(|(name, id)| (alloc::string::String::from(name), id))
        .collect();

        // The declared types come from the generated table, not from the test.
        let vertex_layout = layout(&paint, &ids, |attr_id| {
            declared_for(BuiltIn::FillShader, attr_id)
                .map(|attribute| (attribute.binding, attribute.declared))
        });

        let Some(Source::Geojson(source)) = style.source("probe") else {
            panic!("a geojson source");
        };
        let features: Vec<_> = geojson::read(&source.data)
            .expect("features")
            .into_iter()
            .filter(|f| f.geometry.type_name() == "Polygon")
            .collect();

        let bucket = tessella_layout::fill::build(&[alloc::vec![
            [10240, 4820],
            [10240, 10240],
            [2942, 10240],
            [2942, 4820],
            [10240, 4820],
        ]]);
        let packed = pack_attributes(
            &vertex_layout,
            &paint,
            &[FeatureVertices {
                feature: &features[0],
                vertices: bucket.vertices.len(),
            }],
            None,
        )
        .expect("packs");

        let mut arena = SlabArena::new();
        let encoded = encode_fill(&mut arena, GeometryId(1), &bucket, &vertex_layout, &packed);
        arena.seal();

        assert_eq!(encoded.record.attrs.count, 4, "position plus three");

        let (start, end) = encoded
            .record
            .attrs
            .extent::<AttributeDesc>(encoded.payload.len())
            .expect("the span fits");
        let bytes = &encoded.payload[start..end];
        let descriptors: Vec<AttributeDesc> = (0..4)
            .map(|i| {
                AttributeDesc::from_bytes(&bytes[i * size_of::<AttributeDesc>()..])
                    .expect("a descriptor")
            })
            .collect();

        let expected = [
            // (id, binding, supplied, declared, offset, stride)
            (
                0,
                0,
                AttributeDataType::Short2,
                AttributeDataType::Short2,
                0,
                4,
            ),
            (
                1,
                1,
                AttributeDataType::Float2,
                AttributeDataType::Float4,
                0,
                20,
            ),
            (
                2,
                2,
                AttributeDataType::Float,
                AttributeDataType::Float2,
                8,
                20,
            ),
            (
                3,
                -1,
                AttributeDataType::Float2,
                AttributeDataType::Invalid,
                12,
                20,
            ),
        ];
        for (descriptor, (id, binding, supplied, declared, offset, stride)) in
            descriptors.iter().zip(expected)
        {
            assert_eq!(descriptor.attr_id, id, "id");
            assert_eq!(descriptor.binding, binding, "id {id} binding");
            assert_eq!(descriptor.data_type(), Some(supplied), "id {id} supplied");
            assert_eq!(
                descriptor.declared_data_type(),
                Some(declared),
                "id {id} declared"
            );
            assert_eq!(descriptor.offset, offset, "id {id} offset");
            assert_eq!(descriptor.stride, stride, "id {id} stride");
        }
    }

    /// The three data-driven attributes share one buffer, differing only in offset. Allocating
    /// per attribute would give each its own slab and lose the interleaving the stride describes.
    #[test]
    fn the_data_driven_attributes_share_one_buffer() {
        let style = Style::parse(HERMETIC).expect("style parses");
        let layer = style.layer("fill-datadriven").expect("the layer");
        let paint = tessella_style::property::resolve_paint(layer).expect("resolves");
        let ids = [
            ("idFillColorVertexAttribute", 1u32),
            ("idFillOpacityVertexAttribute", 2),
            ("idFillOutlineColorVertexAttribute", 3),
        ]
        .into_iter()
        .map(|(name, id)| (alloc::string::String::from(name), id))
        .collect();
        let vertex_layout = layout(&paint, &ids, |attr_id| {
            declared_for(BuiltIn::FillShader, attr_id)
                .map(|attribute| (attribute.binding, attribute.declared))
        });

        let mut arena = SlabArena::new();
        let bucket =
            tessella_layout::fill::build(&[alloc::vec![[0, 0], [10, 0], [10, 10], [0, 0]]]);
        let packed = alloc::vec![0u8; vertex_layout.stride as usize * bucket.vertices.len()];
        let encoded = encode_fill(&mut arena, GeometryId(1), &bucket, &vertex_layout, &packed);

        let (start, end) = encoded
            .record
            .attrs
            .extent::<AttributeDesc>(encoded.payload.len())
            .expect("the span fits");
        let bytes = &encoded.payload[start..end];
        let source_of = |i: usize| {
            AttributeDesc::from_bytes(&bytes[i * size_of::<AttributeDesc>()..])
                .expect("a descriptor")
                .source
        };

        // Position has its own buffer; the three data-driven ones share another.
        assert_ne!(
            source_of(0).slab_and_offset(),
            source_of(1).slab_and_offset()
        );
        assert_eq!(
            source_of(1).slab_and_offset(),
            source_of(2).slab_and_offset()
        );
        assert_eq!(
            source_of(2).slab_and_offset(),
            source_of(3).slab_and_offset()
        );
    }
}
