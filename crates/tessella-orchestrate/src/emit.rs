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
    AddReason, AttributeDesc, GeometryAdd, GeometryId, GeometryRemove, MeshAdd, MeshFormat,
    Segment as AbiSegment, SlabEntry, SlabRef, SlabRegion, Span, TextureId, TextureRef, WireRecord,
};
use tessella_capture_abi::generated::texture_slots;
use tessella_capture_abi::ring::{Full, Producer};
use tessella_capture_abi::{AttributeDataType, BuiltIn, EnvelopeKind};
use tessella_layout::circle::CircleBucket;
use tessella_layout::fill::FillBucket;
use tessella_layout::fill::Segment;
use tessella_layout::fill_extrusion::FillExtrusionBucket;
use tessella_layout::line::LineBucket;
use tessella_layout::raster::{RasterBucket, RasterVertex};
use tessella_layout::symbol_bucket::{SymbolBuffers, SymbolVertex};

use crate::binder::VertexLayout;

/// Shader-side id of the position attribute.
///
/// Observed in the oracle's dump as `id=0 bind=0 dt=9 ddt=9 off=0 voff=0 stride=4`. It is the
/// one attribute whose id is stable across every permutation, which is why it can be named here
/// while the rest wait for the generated tables.
pub const POSITION_ATTRIBUTE: u32 = 0;

/// Bytes per position: two i16.
const POSITION_STRIDE: u32 = 4;

/// A symbol's interleaved layout vertex: three attributes of four shorts.
const SYMBOL_STRIDE: u32 = 24;

/// A raster vertex: a tile position and a texture position, two shorts each.
const RASTER_STRIDE: u32 = 8;

/// A slab's starting capacity, so a frame of a few small buckets does not rebuild its buffer on
/// every allocation. It is not a ceiling — see [`SlabArena::alloc`].
const SLAB_BYTES: usize = 64 * 1024;

/// Every allocation starts eight-aligned within its slab.
///
/// A slab holds many buckets' attributes back to back, and a `SlabRef` is where one of them
/// begins. Appending without padding puts the next one wherever the previous ended — a fill's
/// index buffer is six bytes per triangle, so a bucket with an odd triangle count leaves the
/// following vertex buffer on a two-byte boundary. The consumer binds that as a vertex buffer of
/// `i16` pairs or of floats, which is an unaligned load: tolerated on x86, a fault on some of the
/// targets §16 cross-compiles for. §16's own rule for the packed region is eight, so this is the
/// same rule applied one level down.
const SLAB_ALIGN: usize = 8;

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

    /// Copies `bytes` into the open slab and returns a reference to them.
    ///
    /// An empty input still yields a valid reference, with zero length, so a caller does not
    /// have to special-case a bucket with no indices.
    ///
    /// # A slab ends where the caller says, not at a byte count
    ///
    /// This used to open a new slab whenever the current one would overflow a fixed size, which
    /// made the slab an accident of how much geometry happened to precede it. That is the wrong
    /// boundary, because a slab is what a consumer binds as one vertex buffer and *one draw call
    /// reads one vertex buffer*: two buckets in different slabs cannot be drawn together however
    /// alike they are. With the fixed size a layer's forty-two tiles landed in forty-two slabs
    /// and every tile was its own draw.
    ///
    /// So the split is the caller's: [`seal`](Self::seal) closes a slab, and DR-16 says what the
    /// caller should close it on — "consolidated buffer per (view, layer)". Between seals this
    /// grows, and the only limit it keeps is the one the ABI imposes: a `SlabRef` addresses its
    /// slab with a `u32`, so an allocation that would push the end past that opens a new slab
    /// whatever the caller intended.
    pub fn alloc(&mut self, bytes: &[u8]) -> SlabRef {
        if bytes.is_empty() {
            return SlabRef {
                slab: self.open.as_ref().map_or(0, |slab| slab.id),
                offset: 0,
                length: 0,
            };
        }

        // Pad first, so the *reference* is aligned rather than merely the slab.
        if let Some(slab) = self.open.as_mut() {
            let padding = slab.bytes.len().next_multiple_of(SLAB_ALIGN) - slab.bytes.len();
            slab.bytes.resize(slab.bytes.len() + padding, 0);
        }

        let needs_new = match &self.open {
            None => true,
            // Not a tuning threshold: past this the offset no longer fits the field that carries
            // it, and the reference would name the wrong bytes.
            Some(slab) => slab.bytes.len() + bytes.len() > u32::MAX as usize,
        };
        if needs_new {
            self.seal();
            self.open = Some(Slab {
                id: self.next_id,
                bytes: Vec::with_capacity(SLAB_BYTES.max(bytes.len())),
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

    /// Packs every sealed slab into one region a consumer can map.
    ///
    /// # Why an arena is not already that
    ///
    /// In process, a slab is an `Arc` and a handle is an index into a `Vec` of them — which is
    /// all a Rust consumer needs, and §3.6's elision is exactly that: the geometry "copy"
    /// degenerates to a refcount bump. A consumer across a mapping has neither the `Vec` nor the
    /// `Arc`, so a handle names nothing until the slabs are laid out contiguously with a table
    /// saying where each one begins.
    ///
    /// §3.5 says the ABI precludes nothing here because "slab handles are offsets". That is true
    /// of the *handle* and was undefined for the thing it offsets into — a C consumer could read
    /// every envelope and reach not one vertex. Found the way that kind of gap is found, by
    /// writing the consumer.
    ///
    /// The layout is [`SlabRegion`], then `count` [`SlabEntry`], then the bytes. Each slab starts
    /// on an eight-byte boundary so a consumer can read its contents at their natural alignment
    /// rather than byte-wise.
    ///
    /// Sealed slabs only. The open one is not in `slabs()` either, and it is the slab the
    /// producer is still writing.
    #[must_use]
    pub fn pack(&self) -> Vec<u8> {
        let header = core::mem::size_of::<SlabRegion>();
        let table = core::mem::size_of::<SlabEntry>() * self.sealed.len();

        let mut entries = Vec::with_capacity(self.sealed.len());
        let mut offset = (header + table).next_multiple_of(8);
        for slab in &self.sealed {
            entries.push(SlabEntry {
                offset: offset as u64,
                length: slab.bytes.len() as u64,
            });
            offset += slab.bytes.len().next_multiple_of(8);
        }

        #[allow(clippy::cast_possible_truncation)]
        let region = SlabRegion {
            abi_rev: tessella_capture_abi::ABI_REV,
            count: self.sealed.len() as u32,
            total_len: offset as u64,
        };

        let mut out = Vec::with_capacity(offset);
        out.extend_from_slice(region.as_bytes());
        for entry in &entries {
            out.extend_from_slice(entry.as_bytes());
        }
        for (slab, entry) in self.sealed.iter().zip(&entries) {
            out.resize(entry.offset as usize, 0);
            out.extend_from_slice(&slab.bytes);
        }
        out.resize(offset, 0);
        out
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

impl Encoded {
    /// The attribute descriptors, decoded.
    ///
    /// The spans in the record address the payload, and reading them by hand is easy to get
    /// subtly wrong — three tests were doing exactly that before this existed.
    #[must_use]
    pub fn attributes(&self) -> Vec<AttributeDesc> {
        let size = core::mem::size_of::<AttributeDesc>();
        let start = self.record.attrs.offset as usize;
        (0..self.record.attrs.count as usize)
            .filter_map(|index| AttributeDesc::from_bytes(&self.payload[start + index * size..]))
            .collect()
    }

    /// The segments, decoded.
    #[must_use]
    pub fn segments(&self) -> Vec<AbiSegment> {
        let size = core::mem::size_of::<AbiSegment>();
        let start = self.record.segments.offset as usize;
        (0..self.record.segments.count as usize)
            .filter_map(|index| AbiSegment::from_bytes(&self.payload[start + index * size..]))
            .collect()
    }
}

/// The texture bindings for a drawable, paired with the slots its shader declares.
///
/// # Why the slot comes from a table rather than from the caller
///
/// A slot belongs to the *shader*, not to the texture. The glyph atlas is slot 0 of
/// `SymbolSDFShader` and slot 0 of `SymbolTextAndIconShader`; the sprite atlas is slot 1 of the
/// second and has no slot at all in the first. A producer that remembered "the icon atlas is
/// slot 1" would bind, on an SDF drawable, a texture that shader has no sampler for — which
/// draws a label with no glyphs rather than reporting anything. DR-6's generated table is where
/// the slots come from, for the same reason attribute bindings come from one.
///
/// # Supplying the wrong number is a fault, not a shorter list
///
/// A shader's samplers are all of them or none: a raster shader declares two and reads both, and
/// what a shader reads from an *unbound* sampler is the backend's business rather than a defined
/// black. So a caller supplying fewer textures than the shader declares is emitting a drawable
/// that cannot draw, and this says so rather than binding a prefix.
///
/// # Panics
///
/// When `bound` is not exactly as long as the shader's table, or when the shader has no
/// generated table at all. Both are producer bugs rather than data faults — the shader is chosen
/// a few lines above every call site — and a drawable with the wrong samplers is worse on the
/// wire than a panic in a test.
#[must_use]
pub fn texture_refs(shader: BuiltIn, bound: &[TextureId]) -> Vec<TextureRef> {
    let declared = texture_slots::texture_count(shader)
        .unwrap_or_else(|| panic!("{shader:?} has no generated texture table"));
    assert_eq!(
        bound.len(),
        declared,
        "{shader:?} declares {declared} samplers and {} were supplied",
        bound.len()
    );

    texture_slots::textures(shader)
        .iter()
        .zip(bound)
        .map(|(slot, texture)| TextureRef {
            texture: *texture,
            slot: slot.binding,
            _pad: 0,
        })
        .collect()
}

/// Encodes a fill bucket into a geometry envelope, allocating its bytes into `arena`.
///
/// The envelope carries no view: it is process-scoped and refcounted, and a `ViewUse` binds it
/// into a view's draw order (§5.3).
///
/// `layout`, `attributes` and `permutation_key` all come from the binder. Every data-driven
/// attribute references the *same* interleaved buffer at a different offset, which is what the
/// oracle does — its three data-driven descriptors share one source hash and differ only in
/// `off`. The permutation key says which of the shader's declared attributes this variant
/// actually supplies; see [`crate::binder::permutation_key`] for why it is a mask.
pub fn encode_fill(
    arena: &mut SlabArena,
    geometry: GeometryId,
    bucket: &FillBucket,
    layout: &VertexLayout,
    attributes: &[u8],
    permutation_key: u64,
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
        permutation_key,
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

/// The attribute ids of the fixed, non-data-driven parts of a vertex.
///
/// Position is always attribute zero; a line and an extrusion each carry a second fixed
/// attribute beside it, because the shader needs more per vertex than a point. They are fixed in
/// the sense that matters here: the layout generator does not produce them and the paint binder
/// never supplies them, so they are written by whichever encoder knows the bucket's own struct.
const LINE_DATA_ATTRIBUTE: u32 = 1;

/// `decimals` and the edge distance, packed together.
const EXTRUSION_DECIMALS_ATTRIBUTE: u32 = 1;

/// A line vertex: two shorts and four bytes.
const LINE_STRIDE: u32 = 8;

/// An extrusion vertex: two shorts, then two unsigned shorts.
const EXTRUSION_STRIDE: u32 = 8;

/// Builds the descriptor run shared by every encoder: the fixed attributes this bucket's struct
/// supplies, then whatever the paint binder made data-driven.
///
/// The split is not cosmetic. The fixed ones come from the bucket's own vertex struct and their
/// offsets are properties of that struct; the data-driven ones come from a separate interleaved
/// buffer whose stride the binder decides, and pointing them at the vertex buffer — or the
/// reverse — reads one buffer with the other's stride and produces geometry made of noise.
fn descriptors(
    fixed: &[(u32, i32, u32, AttributeDataType)],
    vertices: SlabRef,
    vertex_stride: u32,
    layout: &VertexLayout,
    interleaved: SlabRef,
) -> Vec<AttributeDesc> {
    let mut out = Vec::with_capacity(fixed.len() + layout.attributes.len());
    for &(attr_id, binding, offset, data_type) in fixed {
        out.push(AttributeDesc {
            attr_id,
            binding,
            source: vertices,
            offset,
            vertex_offset: 0,
            stride: vertex_stride,
            // Neither supplied nor declared is interpolated: these are the geometry itself.
            data_type: data_type as u8,
            declared_data_type: data_type as u8,
            _pad: [0; 2],
        });
    }
    for attribute in &layout.attributes {
        out.push(AttributeDesc {
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
    out
}

/// Packs the descriptor and segment runs into a payload, and builds the record around them.
fn geometry_add(
    geometry: GeometryId,
    permutation_key: u64,
    indexes: SlabRef,
    vertex_count: usize,
    descriptors: &[AttributeDesc],
    segments: &[Segment],
    shader: BuiltIn,
) -> Encoded {
    let mut payload = Vec::new();
    let attrs = push_span(&mut payload, descriptors);
    let segments = push_span(
        &mut payload,
        &segments
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
        permutation_key,
        indexes,
        vertex_count: vertex_count as u32,
        attrs,
        instance_attrs: Span::default(),
        segments,
        texture_refs: Span::default(),
        builtin_shader: shader as i32,
        vertex_type: AttributeDataType::Short2 as u8,
        reason: AddReason::Created as u8,
        _pad: [0; 2],
    };

    Encoded { record, payload }
}

/// Encodes a line layer's geometry.
///
/// Two fixed attributes rather than one, and the second is what makes a line a line. A
/// `LineBucket` holds the *centreline*, doubled: `pos_normal` is the point times two with the
/// cap and side flags in the low bits, and `data` carries the extrusion as two biased bytes
/// beside the distance along the line. The shader reads both and widens the centreline into a
/// quad at draw time, in screen space, which is why a line's width is a uniform rather than
/// geometry and why zooming does not rebuild the bucket.
///
/// A consumer that binds only the position therefore draws nothing visible: every vertex of a
/// segment sits on the centreline and its triangles are degenerate. That is not a defect in the
/// encoding, it is what the second attribute is for.
pub fn encode_line(
    arena: &mut SlabArena,
    geometry: GeometryId,
    bucket: &LineBucket,
    layout: &VertexLayout,
    attributes: &[u8],
    permutation_key: u64,
) -> Encoded {
    let mut vertex_bytes = Vec::with_capacity(bucket.vertices.len() * LINE_STRIDE as usize);
    for vertex in &bucket.vertices {
        vertex_bytes.extend_from_slice(&vertex.pos_normal[0].to_le_bytes());
        vertex_bytes.extend_from_slice(&vertex.pos_normal[1].to_le_bytes());
        vertex_bytes.extend_from_slice(&vertex.data);
    }

    let vertices = arena.alloc(&vertex_bytes);
    let indexes = arena.alloc(&as_bytes_u16(&bucket.indices));
    let interleaved = arena.alloc(attributes);

    let fixed = [
        (POSITION_ATTRIBUTE, 0, 0, AttributeDataType::Short2),
        (LINE_DATA_ATTRIBUTE, 1, 4, AttributeDataType::UByte4),
    ];
    let descriptors = descriptors(&fixed, vertices, LINE_STRIDE, layout, interleaved);
    geometry_add(
        geometry,
        permutation_key,
        indexes,
        bucket.vertices.len(),
        &descriptors,
        &bucket.segments,
        BuiltIn::LineShader,
    )
}

/// Encodes a circle layer's geometry.
///
/// The same vertex as a fill's — two shorts — and for the same reason a line's is not: a circle
/// is a quad per point with the disc drawn inside it by the shader, so the geometry is the
/// centre doubled with a corner bit in the low bits and nothing else. The radius is a uniform.
pub fn encode_circle(
    arena: &mut SlabArena,
    geometry: GeometryId,
    bucket: &CircleBucket,
    layout: &VertexLayout,
    attributes: &[u8],
    permutation_key: u64,
) -> Encoded {
    let vertices = arena.alloc(&as_bytes_i16(&bucket.vertices));
    let indexes = arena.alloc(&as_bytes_u16(&bucket.indices));
    let interleaved = arena.alloc(attributes);

    let fixed = [(POSITION_ATTRIBUTE, 0, 0, AttributeDataType::Short2)];
    let descriptors = descriptors(&fixed, vertices, POSITION_STRIDE, layout, interleaved);
    geometry_add(
        geometry,
        permutation_key,
        indexes,
        bucket.vertices.len(),
        &descriptors,
        &bucket.segments,
        BuiltIn::CircleShader,
    )
}

/// Encodes a fill-extrusion layer's geometry.
///
/// The second attribute is three things at once, which is why it cannot be dropped as padding:
/// `decimals` holds the fractional part of both axes packed with a discard flag, and the edge
/// distance rides beside it. The fraction is what keeps a wall's foot on its own outline rather
/// than half a tile unit away from it, and the discard flag is what stops a ring's closing point
/// raising a wall it has no edge for.
///
/// The bucket is the *instanced* branch's, so what is here is the ground outline and the earcut
/// roof. The walls are instances the shader raises over the same buffer; a consumer drawing only
/// these vertices gets roofs and outlines, which is a flat city rather than an empty one.
pub fn encode_extrusion(
    arena: &mut SlabArena,
    geometry: GeometryId,
    bucket: &FillExtrusionBucket,
    layout: &VertexLayout,
    attributes: &[u8],
    permutation_key: u64,
) -> Encoded {
    let mut vertex_bytes = Vec::with_capacity(bucket.vertices.len() * EXTRUSION_STRIDE as usize);
    for vertex in &bucket.vertices {
        vertex_bytes.extend_from_slice(&vertex.position[0].to_le_bytes());
        vertex_bytes.extend_from_slice(&vertex.position[1].to_le_bytes());
        vertex_bytes.extend_from_slice(&vertex.decimals.to_le_bytes());
        vertex_bytes.extend_from_slice(&vertex.edge_distance.to_le_bytes());
    }

    let vertices = arena.alloc(&vertex_bytes);
    let indexes = arena.alloc(&as_bytes_u16(&bucket.indices));
    let interleaved = arena.alloc(attributes);

    let fixed = [
        (POSITION_ATTRIBUTE, 0, 0, AttributeDataType::Short2),
        (
            EXTRUSION_DECIMALS_ATTRIBUTE,
            1,
            4,
            AttributeDataType::UShort2,
        ),
    ];
    let descriptors = descriptors(&fixed, vertices, EXTRUSION_STRIDE, layout, interleaved);
    geometry_add(
        geometry,
        permutation_key,
        indexes,
        bucket.vertices.len(),
        &descriptors,
        &bucket.segments,
        BuiltIn::FillExtrusionShader,
    )
}

/// Encodes a symbol layer's geometry.
///
/// Three attributes interleaved at a stride of 24, then two more in buffers of their own — the
/// layout the golden capture measured rather than the one mbgl's source suggested. The two are
/// separate because they change at different rates: the interleaved buffer is a function of the
/// tile and the glyphs, and the other two are rewritten every frame placement runs.
///
/// `is_sdf` picks the shader. Text is always SDF; an icon may be either, and the flag is already
/// packed into each vertex's size field, so this only decides which shader is named.
///
/// `atlas` is the texture this drawable samples, and it is *one* texture whichever kind of
/// symbol this is. mbgl's `DrawableAtlasesTweaker` is explicit about it: a shader declaring a
/// separate icon sampler gets both atlases, and a shader that does not gets the glyph atlas for
/// a text drawable and the *icon* atlas for an icon drawable — at the same slot 0 either way.
/// Neither shader named here declares the second sampler, so the caller passes whichever atlas
/// this drawable's symbols came out of. The golden's single `tex ... slot=0` line per symbol
/// drawable is that rule seen from outside.
pub fn encode_symbol(
    arena: &mut SlabArena,
    geometry: GeometryId,
    buffers: &SymbolBuffers,
    permutation_key: u64,
    is_sdf: bool,
    atlas: TextureId,
) -> Encoded {
    let vertex_bytes = as_symbol_bytes(&buffers.vertices);
    let index_bytes = as_bytes_u16(&buffers.indices);
    let dynamic_bytes = as_bytes_f32_3(&buffers.dynamic);
    let opacity_bytes = as_bytes_f32(&buffers.opacity);

    let interleaved = arena.alloc(&vertex_bytes);
    let indexes = arena.alloc(&index_bytes);
    let dynamic = arena.alloc(&dynamic_bytes);
    let opacity = arena.alloc(&opacity_bytes);

    // The five attributes the capture shows, in the order it shows them. Offsets 0, 8 and 16
    // share the interleaved buffer at stride 24; the last two are tightly packed buffers of
    // their own, which is why their offset is zero and their stride is their own width.
    let descriptors = alloc::vec![
        AttributeDesc {
            attr_id: 0,
            binding: 0,
            source: interleaved,
            offset: 0,
            vertex_offset: 0,
            stride: SYMBOL_STRIDE,
            data_type: AttributeDataType::Short4 as u8,
            declared_data_type: AttributeDataType::Short4 as u8,
            _pad: [0; 2],
        },
        AttributeDesc {
            attr_id: 1,
            binding: 1,
            source: interleaved,
            offset: 8,
            vertex_offset: 0,
            stride: SYMBOL_STRIDE,
            data_type: AttributeDataType::UShort4 as u8,
            declared_data_type: AttributeDataType::UShort4 as u8,
            _pad: [0; 2],
        },
        AttributeDesc {
            attr_id: 2,
            binding: 2,
            source: interleaved,
            offset: 16,
            vertex_offset: 0,
            stride: SYMBOL_STRIDE,
            data_type: AttributeDataType::Short4 as u8,
            declared_data_type: AttributeDataType::Short4 as u8,
            _pad: [0; 2],
        },
        AttributeDesc {
            attr_id: 3,
            binding: 3,
            source: dynamic,
            offset: 0,
            vertex_offset: 0,
            stride: 12,
            data_type: AttributeDataType::Float3 as u8,
            declared_data_type: AttributeDataType::Float3 as u8,
            _pad: [0; 2],
        },
        AttributeDesc {
            attr_id: 4,
            binding: 4,
            source: opacity,
            offset: 0,
            vertex_offset: 0,
            stride: 4,
            data_type: AttributeDataType::Float as u8,
            declared_data_type: AttributeDataType::Float as u8,
            _pad: [0; 2],
        },
    ];

    let mut payload = Vec::new();
    let attrs = push_span(&mut payload, &descriptors);
    // One segment: the capture shows `segs=1` for both its symbol drawables, and a layer's
    // labels share one buffer. A second segment appears only past what a u16 index reaches,
    // which `SymbolBuffers::add_quad` refuses rather than wrapping into.
    #[allow(clippy::cast_possible_truncation)]
    let segments = push_span(
        &mut payload,
        &[AbiSegment {
            vertex_offset: 0,
            index_offset: 0,
            vertex_length: buffers.vertices.len() as u32,
            index_length: buffers.indices.len() as u32,
        }],
    );

    let shader = if is_sdf {
        BuiltIn::SymbolSDFShader
    } else {
        BuiltIn::SymbolIconShader
    };
    let texture_refs = push_span(&mut payload, &texture_refs(shader, &[atlas]));

    #[allow(clippy::cast_possible_truncation)]
    let record = GeometryAdd {
        geometry,
        permutation_key,
        indexes,
        vertex_count: buffers.vertices.len() as u32,
        attrs,
        instance_attrs: Span::default(),
        segments,
        texture_refs,
        builtin_shader: shader as i32,
        vertex_type: AttributeDataType::Short4 as u8,
        reason: AddReason::Created as u8,
        _pad: [0; 2],
    };

    Encoded { record, payload }
}

/// Encodes a raster layer's geometry.
///
/// Two attributes, both `Short2` and both out of the same interleaved buffer: where the vertex
/// sits in the tile and where it samples the image. mbgl declares the second as `UShort2` in the
/// bucket and binds `idRasterTexturePosVertexAttribute` as `Short2`, which is not a
/// contradiction — the values are all non-negative and under the extent, so the two readings
/// agree over the range a tile occupies. The shader's declaration is what the wire carries.
///
/// `image` is bound to *both* of the shader's samplers. `render_raster_layer.cpp` does the same
/// thing, and it is not a mistake: slot 1 is the parent tile a fading tile blends against, and
/// with no fade in progress it is this tile's own picture. Binding only slot 0 would leave the
/// second sampler unbound, and what a shader reads from an unbound sampler is the backend's
/// business rather than a defined black.
pub fn encode_raster(
    arena: &mut SlabArena,
    geometry: GeometryId,
    bucket: &RasterBucket,
    image: TextureId,
) -> Encoded {
    let vertex_bytes = as_raster_bytes(&bucket.vertices);
    let index_bytes = as_bytes_u16(&bucket.indices);

    let interleaved = arena.alloc(&vertex_bytes);
    let indexes = arena.alloc(&index_bytes);

    let descriptors = alloc::vec![
        AttributeDesc {
            attr_id: 0,
            binding: 0,
            source: interleaved,
            offset: 0,
            vertex_offset: 0,
            stride: RASTER_STRIDE,
            data_type: AttributeDataType::Short2 as u8,
            declared_data_type: AttributeDataType::Short2 as u8,
            _pad: [0; 2],
        },
        AttributeDesc {
            attr_id: 1,
            binding: 1,
            source: interleaved,
            offset: 4,
            vertex_offset: 0,
            stride: RASTER_STRIDE,
            data_type: AttributeDataType::Short2 as u8,
            declared_data_type: AttributeDataType::Short2 as u8,
            _pad: [0; 2],
        },
    ];

    let mut payload = Vec::new();
    let attrs = push_span(&mut payload, &descriptors);
    // One segment however many quads the mask produced. They share a buffer, and a raster
    // tile's four indices per quad cannot approach what a u16 reaches.
    #[allow(clippy::cast_possible_truncation)]
    let segments = push_span(
        &mut payload,
        &[AbiSegment {
            vertex_offset: 0,
            index_offset: 0,
            vertex_length: bucket.vertices.len() as u32,
            index_length: bucket.indices.len() as u32,
        }],
    );
    let texture_refs = push_span(
        &mut payload,
        &texture_refs(BuiltIn::RasterShader, &[image, image]),
    );

    #[allow(clippy::cast_possible_truncation)]
    let record = GeometryAdd {
        geometry,
        // No data-driven attributes: a raster tile has no features for a property to vary over,
        // so there is one permutation and its key is zero.
        permutation_key: 0,
        indexes,
        vertex_count: bucket.vertices.len() as u32,
        attrs,
        instance_attrs: Span::default(),
        segments,
        texture_refs,
        builtin_shader: BuiltIn::RasterShader as i32,
        vertex_type: AttributeDataType::Short2 as u8,
        reason: AddReason::Created as u8,
        _pad: [0; 2],
    };

    Encoded { record, payload }
}

/// Puts an authored model into a slab and announces it.
///
/// # What this deliberately does not do
///
/// It does not decode the glTF. Both consumers this targets already have a loader that does —
/// Filament's `gltfio` links meshoptimizer and takes a byte pointer, flutter_scene's importer
/// recognises the same extensions and takes a `Uint8List` — so decoding here would discard work
/// the consumer already links and roughly triple what crosses the seam, since a meshopt-packed
/// tile is several times smaller than its vertices.
///
/// What the producer decides is what it always decides: whether the tile is in the cover, and
/// where it goes. The bytes are an asset, not a computation.
///
/// # The slab is the zero-copy hand-off
///
/// The GLB is copied into a slab **once**, and everything after that is a reference: `gltfio`
/// parses from the slab's memory, and a `Uint8List` view can be taken over the same bytes. The
/// consumer holds the slab alive until its loader is done, which is the same §11.3 contract
/// geometry already uses — and exactly the lifetime an asynchronous load needs.
///
/// That one copy is the arena's, and it is not free; a caller that already has the bytes in a
/// slab should build the record itself rather than hand them here to be copied again.
pub fn encode_mesh(arena: &mut SlabArena, mesh: GeometryId, glb: &[u8]) -> MeshEncoded {
    let bytes = arena.alloc(glb);
    MeshEncoded {
        record: MeshAdd {
            mesh,
            bytes,
            format: MeshFormat::Glb as u8,
            // A mesh is announced once. It is never modified in place: a changed tile is a
            // different tile with an id of its own, which is what makes the refcount enough.
            reason: AddReason::Created as u8,
            _pad: [0; 2],
        }
        .as_bytes()
        .to_vec(),
        payload: Vec::new(),
    }
}

/// Writes a mesh announcement to the ring.
///
/// # Errors
///
/// [`Full`] when the ring cannot take it. Lossless like geometry: a dropped announcement leaves
/// a later `ViewUse` naming an id the consumer never saw.
pub fn write_mesh(producer: &mut Producer, mesh: &MeshEncoded) -> Result<(), Full> {
    producer.write(EnvelopeKind::MeshAdd, &mesh.record, &[])
}

/// A mesh announcement, ready for the ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshEncoded {
    /// The record as it will be written.
    pub record: Vec<u8>,
    /// Nothing: a mesh's bytes live in a slab, not in the payload region.
    pub payload: Vec<u8>,
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

/// A symbol's interleaved vertices, in the order the three attributes are declared.
///
/// Written out rather than reinterpreted from the struct's memory: the layout the consumer reads
/// is a property of the wire format, not of how Rust happens to lay a struct out, and a
/// `repr(Rust)` reordering would be silent.
fn as_symbol_bytes(values: &[SymbolVertex]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * SYMBOL_STRIDE as usize);
    for vertex in values {
        for value in vertex.pos_offset {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in vertex.data {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in vertex.pixel_offset {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out
}

/// A raster vertex's bytes: position then texture coordinate, interleaved.
///
/// The texture coordinate is written as its own two little-endian `u16`, which is the same four
/// bytes an `i16` pair would produce for any value a tile holds. It is spelled as the type the
/// bucket stores rather than converted, so a value that ever did exceed `i16::MAX` would be
/// wrong here in an obvious way instead of silently negative.
fn as_raster_bytes(values: &[RasterVertex]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * RASTER_STRIDE as usize);
    for vertex in values {
        for value in vertex.position {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in vertex.texture {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out
}

fn as_bytes_f32_3(values: &[[f32; 3]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 12);
    for value in values {
        for component in value {
            out.extend_from_slice(&component.to_le_bytes());
        }
    }
    out
}

fn as_bytes_f32(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
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
        // The second allocation follows the first in the same slab, at the next eight-byte
        // boundary rather than immediately after — four bytes of geometry are padded to eight so
        // that whatever the consumer binds at `second` is naturally aligned.
        assert_eq!(first.slab, second.slab);
        assert_eq!(second.offset, 8);
    }

    /// Every reference is eight-aligned, whatever lengths precede it.
    ///
    /// Lengths chosen to be coprime with eight, so each one would leave the next unaligned if
    /// nothing padded it. This matters more than it used to: a slab now holds a whole layer's
    /// buckets rather than as many as fit in sixty-four kilobytes, so there are many more
    /// interior references and every one of them is a buffer binding.
    #[test]
    fn every_reference_is_aligned() {
        let mut arena = SlabArena::new();
        let refs: Vec<SlabRef> = [1usize, 3, 7, 13, 31, 5]
            .iter()
            .map(|length| arena.alloc(&alloc::vec![0xa5; *length]))
            .collect();
        arena.seal();

        for (reference, length) in refs.iter().zip([1usize, 3, 7, 13, 31, 5]) {
            assert_eq!(
                reference.offset % 8,
                0,
                "an allocation of {length} bytes landed at {}",
                reference.offset
            );
            assert_eq!(
                arena.resolve(*reference),
                Some(&alloc::vec![0xa5; length][..])
            );
        }
    }

    /// A caller that never seals gets one slab, however much it allocates.
    ///
    /// The old arena split at a fixed size, which is what put a layer's tiles in separate
    /// buffers and made each one its own draw call. The boundary is the caller's now.
    #[test]
    fn nothing_splits_a_slab_but_the_caller() {
        let mut arena = SlabArena::new();
        let refs: Vec<SlabRef> = (0..8)
            .map(|_| arena.alloc(&alloc::vec![9u8; SLAB_BYTES]))
            .collect();
        arena.seal();

        assert_eq!(
            arena.slabs().len(),
            1,
            "one slab, because seal was called once"
        );
        assert!(
            refs.windows(2).all(|pair| pair[0].slab == pair[1].slab),
            "every reference names it"
        );
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
            0,
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
            0,
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
            0,
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
    use crate::binder::{FILL_FAMILY, attribute_ids, layout, permutation_key};
    use tessella_capture_abi::envelope::WireRecord;
    use tessella_capture_abi::{AttributeDataType, declared_for};
    use tessella_layout::paint::PaintBinder;
    use tessella_source::geojson;
    use tessella_style::property::paint_specs;
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

        // Ids come from the generated table rather than being written out here, so the test
        // cannot disagree with `shader_defines.hpp` about what an attribute is called.
        let ids = attribute_ids(FILL_FAMILY);
        let key = permutation_key(&paint, &ids);

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

        let mut binder = PaintBinder::new(paint_specs(&layer.kind).unwrap_or(&[]), &paint, 13.0);
        binder
            .push(bucket.vertices.len(), &paint, &features[0])
            .expect("binds");
        let packed = binder.data().to_vec();

        // The declared types come from the generated table; the offsets and stride come from
        // the binder that wrote the bytes, so the descriptors cannot describe a layout the
        // buffer does not have.
        let vertex_layout = layout(&binder, &ids, |attr_id| {
            declared_for(BuiltIn::FillShader, attr_id)
                .map(|attribute| (attribute.binding, attribute.declared))
        });

        let mut arena = SlabArena::new();
        let encoded = encode_fill(
            &mut arena,
            GeometryId(1),
            &bucket,
            &vertex_layout,
            &packed,
            key,
        );
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
        let ids = attribute_ids(FILL_FAMILY);
        let key = permutation_key(&paint, &ids);
        let binder = PaintBinder::new(paint_specs(&layer.kind).unwrap_or(&[]), &paint, 13.0);
        let vertex_layout = layout(&binder, &ids, |attr_id| {
            declared_for(BuiltIn::FillShader, attr_id)
                .map(|attribute| (attribute.binding, attribute.declared))
        });

        let mut arena = SlabArena::new();
        let bucket =
            tessella_layout::fill::build(&[alloc::vec![[0, 0], [10, 0], [10, 10], [0, 0]]]);
        let packed = alloc::vec![0u8; vertex_layout.stride as usize * bucket.vertices.len()];
        let encoded = encode_fill(
            &mut arena,
            GeometryId(1),
            &bucket,
            &vertex_layout,
            &packed,
            key,
        );

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
