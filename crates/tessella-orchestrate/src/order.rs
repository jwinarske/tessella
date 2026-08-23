//! Per-view draw order, and the epoch that names it (§6.3, DR-4, R-5).
//!
//! # Why order is its own envelope
//!
//! Rev 1 carried a `FrameOrder`; DR-4 split it into [`OrderUpdate`] and a camera. The reason is
//! that the two change at different rates. A camera changes every frame of a pan; the draw order
//! changes only when the set of drawables does — a tile arriving, a layer appearing, a style
//! edit. Emitting the order at camera rate would put a list proportional to the whole scene on
//! the ring sixty times a second to say nothing had changed.
//!
//! So the order carries an epoch and is emitted only when the list differs from the last one
//! sent. The camera then names the epoch it was computed against, and §4's hold-camera-until-
//! order rule makes the pair consistent: a consumer that has not yet seen the epoch a camera
//! names holds the camera rather than drawing the new camera against the old order. R-5 is that
//! this goes wrong, and the symptom is a one-frame flicker under churn.
//!
//! # The order is per view, and so is `ubo_index`
//!
//! Geometry is process-scoped and shared (§5.3), but the *order* it is drawn in is not: two
//! views over one cover draw the same tiles with different cameras and, under R-2, different
//! screen-space sizes. `ubo_index` is a slot in the view's own consolidated buffer, assigned
//! from the view's own draw order, which is why it rides on [`OrderEntry`] rather than on
//! `GeometryAdd`. Putting it on the geometry would make two views fight over one slot.
//!
//! # The sort is the oracle's, checked against it
//!
//! # Cost
//!
//! `emit` sorts every frame and writes only on change. The sort is inherent to the rebuild
//! model — a frame that clears and re-binds cannot know it produced the same list without
//! producing it — and the thing DR-8 bounds is ring bytes, which stay at zero. For the four
//! views and low hundreds of drawables §13.3 budgets against, an O(n log n) sort per view per
//! frame is far below the tessellation it follows.
//!
//! separates a fill's triangles from its outline — 1 and 2 respectively — and it is why a fill
//! layer is two entries rather than one. The golden dump is a draw order, and
//! `tests/draw_order.rs` reproduces it exactly rather than asserting the rule and hoping.

use alloc::vec::Vec;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{
    GeometryId, OrderEntry, OrderEpoch, OrderUpdate, Span, TileId, ViewId, WireRecord,
};
use tessella_capture_abi::generated::mbgl_enums::RenderPass;
use tessella_capture_abi::ring::{Full, Producer};

use crate::tile::{Content, LayerBucket};
use crate::view::{self, GeometryBinding};

/// One drawable's place in a view's draw order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    /// What the view bound.
    pub binding: GeometryBinding,
    /// Sort key within the layer.
    ///
    /// Zero for every R0 layer: `line-sort-key` and `symbol-sort-key` are the properties that
    /// make it non-zero, and neither is R0. It is carried rather than assumed because R-9 turns
    /// on it — a non-zero sort key is what makes draw order non-contiguous within a layer and
    /// blocks renderable collapse.
    pub draw_priority: i64,
}

impl Placed {
    /// A binding at the default priority.
    #[must_use]
    pub const fn new(binding: GeometryBinding) -> Self {
        Self {
            binding,
            draw_priority: 0,
        }
    }

    /// The key painter order sorts on.
    ///
    /// Layer first, because that is what the style document says and what a consumer keys
    /// uniforms by. Then sort key, then sublayer, then tile — sublayer below sort key because a
    /// sort key that reordered within a layer would reorder a fill's outline away from its
    /// triangles, and the outline has to follow the fill it outlines.
    fn sort_key(&self) -> (i32, i64, i32, u8, u32, u32) {
        let tile = self.binding.tile.unwrap_or_default();
        (
            self.binding.layer_index,
            self.draw_priority,
            self.binding.sub_layer_index,
            tile.z,
            tile.x,
            tile.y,
        )
    }
}

/// A view's draw order, and the epoch naming the last one emitted.
#[derive(Debug, Default)]
pub struct DrawOrder {
    entries: Vec<Placed>,
    epoch: u64,
    emitted: Option<Vec<OrderEntry>>,
}

impl DrawOrder {
    /// An empty order at epoch zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a drawable.
    pub fn push(&mut self, placed: Placed) {
        self.entries.push(placed);
    }

    /// Adds a binding at the default priority.
    pub fn bind(&mut self, binding: GeometryBinding) {
        self.push(Placed::new(binding));
    }

    /// Drops the accumulated bindings, keeping the emitted order and epoch.
    ///
    /// This is what makes the frame loop work: a frame rebuilds the order from whatever is
    /// currently bound and emits, and the comparison against the last emitted list is what keeps
    /// an unchanged scene silent. Keeping the emitted list across a clear is the whole point —
    /// clearing it too would make every frame look like a change and put the order on the ring
    /// at camera rate, which is the traffic DR-8 forbids.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// How many drawables are in the order.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The epoch of the last emitted order.
    #[must_use]
    pub fn epoch(&self) -> OrderEpoch {
        OrderEpoch(self.epoch)
    }

    /// The order, sorted, with `ubo_index` assigned.
    ///
    /// `sort_by_key` rather than `sort_unstable_by_key`: two drawables with an identical key are
    /// genuinely interchangeable in painter order, but keeping their insertion order makes the
    /// output a function of the input rather than of the sort implementation, and a stream that
    /// reorders equal entries between runs would emit an `OrderUpdate` every frame to say
    /// nothing had changed.
    #[must_use]
    pub fn resolve(&self) -> Vec<OrderEntry> {
        let mut sorted = self.entries.clone();
        sorted.sort_by_key(Placed::sort_key);

        // Per pass, from this view's own draw order. A drawable in both passes takes a slot in
        // each, because it is written to both consolidated buffers.
        let mut next: [u32; 8] = [0; 8];
        sorted
            .iter()
            .map(|placed| {
                let pass = placed.binding.pass;
                let slot = &mut next[usize::from(pass.bits() & 0x7)];
                let ubo_index = *slot;
                *slot += 1;
                #[allow(clippy::cast_sign_loss)]
                OrderEntry {
                    geometry: placed.binding.geometry,
                    draw_priority: placed.draw_priority,
                    // `ViewUse` carries the layer index signed and `OrderEntry` carries it
                    // unsigned — an asymmetry inherited from mbgl, where the signed form is a
                    // slot number that can be absent. A bare cast would turn a negative into
                    // four billion and sort it last at the consumer, inverting painter order
                    // for the whole view. Clamped, because a negative layer index has no
                    // painter-order meaning and the sort above has already used the signed
                    // value.
                    layer_index: placed.binding.layer_index.max(0) as u32,
                    sub_layer_index: placed.binding.sub_layer_index,
                    ubo_index,
                    pass,
                    _pad: [0; 3],
                }
            })
            .collect()
    }

    /// The index at which the opaque pass ends, for [`CameraUpdate::opaque_pass_cutoff`].
    ///
    /// mbgl draws opaque layers front-to-back with depth writes and translucent back-to-front,
    /// and the cutoff is where the consumer switches. Computed from the resolved order rather
    /// than tracked alongside it, because a cutoff that disagreed with the order it indexes is
    /// worse than no cutoff at all.
    ///
    /// [`CameraUpdate::opaque_pass_cutoff`]: tessella_capture_abi::envelope::CameraUpdate
    #[must_use]
    pub fn opaque_cutoff(resolved: &[OrderEntry]) -> u32 {
        #[allow(clippy::cast_possible_truncation)]
        {
            resolved
                .iter()
                .position(|e| e.pass.bits() & RenderPass::OPAQUE.bits() == 0)
                .unwrap_or(resolved.len()) as u32
        }
    }

    /// Emits the order if it differs from the last one emitted, returning the epoch in force.
    ///
    /// §6.3 says "emitted only when the list differs from the last one emitted", and this is
    /// where that is decided. An unconditional emit would put a list proportional to the scene
    /// on the ring at camera rate; an emit that compared only lengths would miss a restack.
    ///
    /// The epoch advances only on a real change, so a camera naming epoch N stays valid across
    /// every frame that did not restack.
    ///
    /// # Errors
    ///
    /// [`Full`] when the ring cannot take the record.
    pub fn emit(&mut self, producer: &mut Producer, view: ViewId) -> Result<Emitted, Full> {
        let resolved = self.resolve();
        if self.emitted.as_deref() == Some(resolved.as_slice()) {
            return Ok(Emitted {
                epoch: OrderEpoch(self.epoch),
                changed: false,
                entries: resolved.len(),
            });
        }

        self.epoch += 1;
        let mut payload = Vec::new();
        for entry in &resolved {
            payload.extend_from_slice(entry.as_bytes());
        }

        #[allow(clippy::cast_possible_truncation)]
        let record = OrderUpdate {
            order_epoch: OrderEpoch(self.epoch),
            view,
            _pad: 0,
            entries: Span {
                offset: 0,
                count: resolved.len() as u32,
            },
        };
        producer.write(EnvelopeKind::OrderUpdate, record.as_bytes(), &payload)?;

        let count = resolved.len();
        self.emitted = Some(resolved);
        Ok(Emitted {
            epoch: OrderEpoch(self.epoch),
            changed: true,
            entries: count,
        })
    }
}

/// What an [`DrawOrder::emit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emitted {
    /// The epoch now in force, which a camera must name.
    pub epoch: OrderEpoch,
    /// Whether anything was written. False is the steady state and the point of the check.
    pub changed: bool,
    /// How many entries the order holds.
    pub entries: usize,
}

/// The bindings a tile's buckets become, in one place because the count is a rule.
///
/// A fill is two drawables and a background is one, and the sublayer indices — 1 for a fill's
/// triangles, 2 for its outline, 0 for a background — are painter order within the layer. The
/// oracle emits exactly this: `L00001.S00001` and `L00001.S00002` for one fill layer, and
/// `L00000.S00000` for the background. Deriving them in one function is what keeps the order
/// path and the emit path from disagreeing about how many drawables a layer has.
///
/// Geometry ids come from `next_id`, advanced per drawable. In the real pipeline they come from
/// the shared geometry registry; here they only have to be distinct and stable.
#[must_use]
pub fn bindings_for(
    view: ViewId,
    tile: TileId,
    buckets: &[LayerBucket],
    next_id: &mut u64,
) -> Vec<GeometryBinding> {
    let mut bindings = Vec::new();
    for bucket in buckets {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let layer_index = bucket.layer_index as i32;
        let mut emit = |sub_layer_index, pass, flags| {
            let binding = GeometryBinding {
                geometry: GeometryId(*next_id),
                view,
                layer_index,
                sub_layer_index,
                tile: Some(tile),
                pass,
                flags,
            };
            *next_id += 1;
            bindings.push(binding);
        };

        match bucket.content {
            Content::Background => {
                emit(0, view::background_pass(), view::background_flags());
            }
            Content::Fill(_) => {
                emit(1, view::fill_pass(), view::tiled_flags());
                emit(2, view::fill_pass(), view::tiled_flags());
            }
        }
    }
    bindings
}

/// A tile ordered the way painter order orders tiles.
#[must_use]
pub fn tile_of(z: u8, x: u32, y: u32) -> TileId {
    TileId {
        x,
        y,
        z,
        overscaled_z: z,
        wrap: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{background_flags, background_pass, fill_pass, tiled_flags};
    use tessella_capture_abi::envelope::DrawFlags;
    use tessella_capture_abi::ring::Ring;

    fn binding(
        layer: i32,
        sub: i32,
        x: u32,
        pass: RenderPass,
        flags: DrawFlags,
    ) -> GeometryBinding {
        GeometryBinding {
            geometry: GeometryId(u64::from(x) * 100 + u64::from(layer as u32) * 10 + sub as u64),
            view: ViewId(0),
            layer_index: layer,
            sub_layer_index: sub,
            tile: Some(tile_of(13, x, 2723)),
            pass,
            flags,
        }
    }

    fn fill(layer: i32, sub: i32, x: u32) -> GeometryBinding {
        binding(layer, sub, x, fill_pass(), tiled_flags())
    }

    fn background(x: u32) -> GeometryBinding {
        binding(0, 0, x, background_pass(), background_flags())
    }

    /// Layer first, then sublayer, then tile — and the input order does not survive.
    #[test]
    fn painter_order_is_layer_then_sublayer_then_tile() {
        let mut order = DrawOrder::new();
        // Deliberately backwards on every axis.
        order.bind(fill(2, 2, 4094));
        order.bind(fill(1, 1, 4092));
        order.bind(fill(2, 1, 4092));
        order.bind(background(4093));

        let resolved = order.resolve();
        let keys: Vec<(u32, i32)> = resolved
            .iter()
            .map(|e| (e.layer_index, e.sub_layer_index))
            .collect();
        assert_eq!(keys, [(0, 0), (1, 1), (2, 1), (2, 2)]);
    }

    /// A sort key does not separate a fill's outline from its triangles.
    ///
    /// Sublayer sorts below `draw_priority`, so a layer with a non-zero sort key reorders as a
    /// unit. Sorting sublayer above it would let a sort key paint an outline before the fill it
    /// outlines, which is the R-9 failure in miniature.
    #[test]
    fn a_sort_key_does_not_split_a_fill_from_its_outline() {
        let mut order = DrawOrder::new();
        order.push(Placed {
            binding: fill(1, 1, 4092),
            draw_priority: 5,
        });
        order.push(Placed {
            binding: fill(1, 2, 4092),
            draw_priority: 5,
        });
        order.push(Placed {
            binding: fill(1, 1, 4093),
            draw_priority: 1,
        });
        order.push(Placed {
            binding: fill(1, 2, 4093),
            draw_priority: 1,
        });

        let resolved = order.resolve();
        let pairs: Vec<(i64, i32)> = resolved
            .iter()
            .map(|e| (e.draw_priority, e.sub_layer_index))
            .collect();
        assert_eq!(pairs, [(1, 1), (1, 2), (5, 1), (5, 2)]);
    }

    /// `ubo_index` is dense from zero within each pass, not across the whole order.
    ///
    /// It is a slot in the pass's consolidated buffer, so numbering it globally would leave
    /// holes in both buffers and index past the end of the shorter one.
    #[test]
    fn ubo_indices_are_dense_within_each_pass() {
        let mut order = DrawOrder::new();
        order.bind(background(4092));
        order.bind(fill(1, 1, 4092));
        order.bind(background(4093));
        order.bind(fill(1, 1, 4093));

        let resolved = order.resolve();
        let mut by_pass: alloc::collections::BTreeMap<u8, Vec<u32>> = Default::default();
        for entry in &resolved {
            by_pass
                .entry(entry.pass.bits())
                .or_default()
                .push(entry.ubo_index);
        }
        for (pass, slots) in by_pass {
            let expected: Vec<u32> = (0..slots.len() as u32).collect();
            assert_eq!(slots, expected, "pass {pass}");
        }
    }

    /// The cutoff is where the opaque pass ends, computed from the order it indexes.
    #[test]
    fn the_opaque_cutoff_is_the_first_translucent_entry() {
        let mut order = DrawOrder::new();
        order.bind(background(4092));
        order.bind(background(4093));
        order.bind(fill(1, 1, 4092));

        let resolved = order.resolve();
        assert_eq!(DrawOrder::opaque_cutoff(&resolved), 2, "two backgrounds");
    }

    /// An order with no opaque entries cuts off at zero, not at the end.
    #[test]
    fn an_all_translucent_order_cuts_off_at_zero() {
        let mut order = DrawOrder::new();
        order.bind(fill(1, 1, 4092));
        assert_eq!(DrawOrder::opaque_cutoff(&order.resolve()), 0);
    }

    /// §6.3: emitted only when the list differs. An unchanged order writes nothing at all, which
    /// is DR-8's zero-bytes guarantee applied to the order channel.
    #[test]
    fn an_unchanged_order_writes_no_bytes() {
        let mut ring = Ring::new(4096);
        let (producer, _consumer) = ring.split();
        let mut order = DrawOrder::new();
        order.bind(background(4092));
        order.bind(fill(1, 1, 4092));

        let first = order.emit(producer, ViewId(0)).expect("emits");
        assert!(first.changed);
        assert_eq!(first.epoch, OrderEpoch(1));
        let after_first = producer.head();

        for _frame in 0..1000 {
            let again = order.emit(producer, ViewId(0)).expect("emits");
            assert!(!again.changed);
            assert_eq!(again.epoch, OrderEpoch(1), "the epoch does not advance");
        }
        assert_eq!(
            producer.head(),
            after_first,
            "a thousand unchanged frames wrote nothing"
        );
    }

    /// A real change does advance the epoch, so a camera naming the old one is known to be
    /// stale rather than silently drawn against the new order (R-5).
    #[test]
    fn a_changed_order_advances_the_epoch() {
        let mut ring = Ring::new(4096);
        let (producer, _consumer) = ring.split();
        let mut order = DrawOrder::new();
        order.bind(background(4092));
        assert_eq!(
            order.emit(producer, ViewId(0)).expect("emits").epoch,
            OrderEpoch(1)
        );

        order.bind(fill(1, 1, 4092));
        let second = order.emit(producer, ViewId(0)).expect("emits");
        assert!(second.changed);
        assert_eq!(second.epoch, OrderEpoch(2));
        assert_eq!(second.entries, 2);
    }

    /// A restack with the same entry count is still a change. Comparing lengths would miss it,
    /// and the symptom would be a consumer drawing a new order against an old epoch (R-5).
    #[test]
    fn a_restack_of_equal_length_is_a_change() {
        let mut ring = Ring::new(4096);
        let (producer, _consumer) = ring.split();
        let mut order = DrawOrder::new();

        order.bind(fill(1, 1, 4092));
        order.bind(fill(2, 1, 4092));
        let first = order.emit(producer, ViewId(0)).expect("emits");

        // The same two drawables with their layers exchanged: same length, different painter
        // order, and the geometry each layer draws has changed places.
        order.clear();
        let mut a = fill(1, 1, 4092);
        let mut b = fill(2, 1, 4092);
        core::mem::swap(&mut a.layer_index, &mut b.layer_index);
        order.bind(a);
        order.bind(b);

        let second = order.emit(producer, ViewId(0)).expect("emits");
        assert_eq!(
            second.entries, first.entries,
            "the same number of drawables"
        );
        assert!(second.changed, "but not the same order");
        assert_eq!(second.epoch, OrderEpoch(2));
    }

    /// The frame loop: rebuild the order every frame from what is bound, emit, stay silent.
    ///
    /// This is how `clear` is actually used, and it is the case DR-8 is about. A producer that
    /// rebuilt the order each frame and emitted unconditionally would write a list proportional
    /// to the scene sixty times a second to say nothing had changed.
    #[test]
    fn rebuilding_an_identical_order_each_frame_stays_silent() {
        let mut ring = Ring::new(4096);
        let (producer, _consumer) = ring.split();
        let mut order = DrawOrder::new();

        let bind_scene = |order: &mut DrawOrder| {
            order.clear();
            order.bind(background(4092));
            order.bind(fill(1, 1, 4092));
            order.bind(fill(1, 2, 4092));
        };

        bind_scene(&mut order);
        order.emit(producer, ViewId(0)).expect("emits");
        let after_first = producer.head();

        for _frame in 0..1000 {
            bind_scene(&mut order);
            assert!(!order.emit(producer, ViewId(0)).expect("emits").changed);
        }
        assert_eq!(
            producer.head(),
            after_first,
            "a thousand rebuilt-but-identical frames wrote nothing"
        );
    }

    /// Equal keys keep their insertion order, so the resolved list is a function of the input.
    /// An unstable sort could permute them between runs and emit an `OrderUpdate` every frame
    /// to say nothing had changed.
    #[test]
    fn equal_keys_keep_their_insertion_order() {
        let mut order = DrawOrder::new();
        for id in 0..16 {
            let mut b = fill(1, 1, 4092);
            b.geometry = GeometryId(id);
            order.bind(b);
        }
        let ids: Vec<u64> = order.resolve().iter().map(|e| e.geometry.0).collect();
        assert_eq!(ids, (0..16).collect::<Vec<u64>>());
    }

    /// A negative layer index does not become four billion.
    ///
    /// `ViewUse` carries the index signed and `OrderEntry` unsigned, so a bare cast would sort a
    /// negative last at the consumer and invert painter order for the whole view.
    #[test]
    fn a_negative_layer_index_does_not_wrap() {
        let mut order = DrawOrder::new();
        let mut odd = fill(1, 1, 4092);
        odd.layer_index = -1;
        order.bind(odd);
        order.bind(fill(1, 1, 4093));

        let resolved = order.resolve();
        assert_eq!(resolved[0].layer_index, 0, "clamped, not wrapped");
        assert!(
            resolved.iter().all(|e| e.layer_index < 1000),
            "and nothing sorted to the end of the world"
        );
    }
}
