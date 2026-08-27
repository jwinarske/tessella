//! What a consumer reconstructs from the stream.
//!
//! Nothing here reaches back into the producer's types. A geometry is whatever its descriptors
//! say it is, read out of a slab at the stride the descriptor names — which is the point: a
//! descriptor that names the wrong buffer produces geometry made of noise, and a test that asks
//! the producer what it meant cannot see that.

use std::collections::BTreeMap;

use tessella_capture_abi::envelope::WireRecord;
use tessella_capture_abi::envelope::{
    AttributeDesc, GeometryAdd, GeometryId, OrderEntry, OrderUpdate, Segment, UboUpdate, ViewUse,
};
use tessella_capture_abi::ring::Consumer;
use tessella_capture_abi::{BuiltIn, EnvelopeKind};
use tessella_orchestrate::SlabArena;

/// One announced geometry, with its bytes already resolved.
pub(crate) struct Geometry {
    /// Which shader family reads it.
    pub(crate) shader: BuiltIn,
    /// Attribute descriptors, in the order the record carried them.
    pub(crate) attributes: Vec<AttributeDesc>,
    /// Index buffer, as `u16`.
    pub(crate) indices: Vec<u16>,
    /// Draw segments.
    pub(crate) segments: Vec<Segment>,
    /// Vertex count.
    pub(crate) vertex_count: u32,
}

impl Geometry {
    /// The descriptor for one attribute id.
    pub(crate) fn attribute(&self, attr_id: u32) -> Option<&AttributeDesc> {
        self.attributes.iter().find(|a| a.attr_id == attr_id)
    }
}

/// One drawable: a geometry bound into a view's order.
pub(crate) struct Drawable {
    /// Which geometry.
    pub(crate) geometry: GeometryId,
    /// Layer group, which keys the uniform buffers.
    pub(crate) layer_index: u32,
    /// Slot in the layer's consolidated drawable buffer.
    pub(crate) ubo_index: u32,
}

/// The frame, as a consumer sees it.
#[derive(Default)]
pub(crate) struct Scene {
    /// Geometries by id.
    pub(crate) geometries: BTreeMap<u64, Geometry>,
    /// Uniform buffers by `(layer_index, slot)`.
    pub(crate) ubos: BTreeMap<(i32, u32), Vec<u8>>,
    /// The draw order, last one wins.
    pub(crate) order: Vec<Drawable>,
    /// Which render pass each entry of [`Self::order`] belongs to, in step with it.
    pub(crate) passes: Vec<u8>,
    /// Uses seen, for the ones the order does not name.
    pub(crate) uses: Vec<Drawable>,
}

impl Scene {
    /// Drains a ring into a scene, resolving slabs against the arena that filled them.
    ///
    /// The arena is the producer's, which is what "in process" means here: a `SlabRef` is a
    /// handle into a region the two sides share rather than bytes on the wire, so a consumer in
    /// another process would map the region instead. Nothing else about the reconstruction
    /// changes, which is why this is a fair test of the descriptors.
    pub(crate) fn drain(consumer: &mut Consumer, arena: &SlabArena) -> Self {
        let mut scene = Self::default();
        while let Some(record) = consumer.peek() {
            match record.kind {
                EnvelopeKind::GeometryAdd => {
                    if let Some(add) = GeometryAdd::from_bytes(record.record) {
                        scene.add_geometry(&add, record.payload, arena);
                    }
                }
                EnvelopeKind::ViewUse => {
                    if let Some(use_) = ViewUse::from_bytes(record.record) {
                        scene.uses.push(Drawable {
                            geometry: use_.geometry,
                            #[allow(clippy::cast_sign_loss)]
                            layer_index: use_.layer_index as u32,
                            ubo_index: 0,
                        });
                    }
                }
                EnvelopeKind::UboUpdate => {
                    if let Some(update) = UboUpdate::from_bytes(record.record) {
                        let start = update.data.offset as usize;
                        let end = start + update.data.count as usize;
                        if let Some(bytes) = record.payload.get(start..end) {
                            scene
                                .ubos
                                .insert((update.layer_index, update.slot), bytes.to_vec());
                        }
                    }
                }
                EnvelopeKind::OrderUpdate => {
                    if let Some(update) = OrderUpdate::from_bytes(record.record) {
                        scene.order = read_span::<OrderEntry>(record.payload, update.entries)
                            .into_iter()
                            .map(|entry| Drawable {
                                geometry: entry.geometry,
                                layer_index: entry.layer_index,
                                ubo_index: entry.ubo_index,
                            })
                            .collect();
                        scene.passes = read_span::<OrderEntry>(record.payload, update.entries)
                            .into_iter()
                            .map(|entry| entry.pass.bits())
                            .collect();
                    }
                }
                _ => {}
            }
            let consumed = record.consumed();
            consumer.advance(consumed);
        }
        scene
    }

    fn add_geometry(&mut self, add: &GeometryAdd, payload: &[u8], arena: &SlabArena) {
        let Some(shader) = BuiltIn::from_repr(add.builtin_shader) else {
            return;
        };
        let attributes = read_span::<AttributeDesc>(payload, add.attrs);
        let segments = read_span::<Segment>(payload, add.segments);
        let indices = arena
            .resolve(add.indexes)
            .map(|bytes| {
                bytes
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect()
            })
            .unwrap_or_default();

        self.geometries.insert(
            add.geometry.0,
            Geometry {
                shader,
                attributes,
                indices,
                segments,
                vertex_count: add.vertex_count,
            },
        );
    }

    /// A uniform buffer's bytes.
    pub(crate) fn ubo(&self, layer_index: u32, slot: u32) -> Option<&[u8]> {
        #[allow(clippy::cast_possible_wrap)]
        self.ubos
            .get(&(layer_index as i32, slot))
            .map(Vec::as_slice)
    }
}

fn read_span<T: WireRecord>(payload: &[u8], span: tessella_capture_abi::envelope::Span) -> Vec<T> {
    let size = core::mem::size_of::<T>();
    let start = span.offset as usize;
    (0..span.count as usize)
        .filter_map(|index| payload.get(start + index * size..).and_then(T::from_bytes))
        .collect()
}
