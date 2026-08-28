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
//! Painter order is pass, then depth slot, then sublayer, then sort key, then tile — and only
//! the last two are what intuition suggests.
//!
//! Pass groups before everything because mbgl runs the opaque and translucent passes as separate
//! traversals of the layer list, so a drawable marked with both is genuinely drawn twice. The
//! depth slot runs *opposite* the style index: mbgl walks the list reversed in the opaque pass
//! counting up and forwards in the translucent pass counting down, so a layer lands on the same
//! slot in both, and ordering by that slot draws the topmost layer first within a pass. Sublayer
//! is what separates a fill's triangles from its outline — 1 and 2 — and it sorts above the sort
//! key so that a sort key cannot paint an outline before the fill it outlines.
//!
//! The dump carries both a `drawable` listing and an `order` section, and only the second is a
//! draw order. The listing is sorted by structural key to keep the golden file stable, which
//! makes it look like painter order while differing on all three points above. `tests/
//! draw_order.rs` compares against `order` and pins the listing as a listing, because comparing
//! against the listing is the mistake that is easy to make and passes.

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

    /// The key painter order sorts on, for one pass.
    ///
    /// Pass first. The oracle's draw order is grouped by pass before anything else — opaque
    /// entries precede translucent ones regardless of layer — because the two passes are
    /// separate traversals of the layer list, not two kinds of entry interleaved in one.
    ///
    /// Then the depth slot, which runs opposite the style index. mbgl walks the layer list
    /// reversed in the opaque pass counting up and forwards in the translucent pass counting
    /// down, precisely so a layer lands on the same slot in both; ordering by it is what puts
    /// the topmost layer first within a pass.
    ///
    /// # Topmost first is not a mistake, and a consumer has to know it
    ///
    /// It looks like one, because painting a translucent layer over the layer above it is what
    /// a back-to-front rasterizer would need. mbgl does not paint that way: it draws
    /// front-to-back against a depth buffer, with each layer at its own slot, so a lower layer
    /// is rejected where a higher one already covered. The golden capture is unambiguous about
    /// it — style layer 4 draws at slot 1, layer 3 at slot 2, layer 2 at slot 3.
    ///
    /// A consumer with no depth buffer therefore cannot paint the order as given; it has to
    /// reverse each translucent run itself. That is the consumer's business, and putting the
    /// reversal here would have made the stream disagree with the oracle it is measured against.
    ///
    /// Then sublayer, then sort key, then tile. Sublayer above the sort key because a sort key
    /// that reordered within a layer would separate a fill's outline from the triangles it
    /// outlines, and the outline has to follow the fill.
    fn sort_key(&self, pass: RenderPass, layer_count: u32) -> (u8, u32, i32, i64, u8, u32, u32) {
        let tile = self.binding.tile.unwrap_or_default();
        (
            pass.bits(),
            depth_slot(self.binding.layer_index, layer_count),
            self.binding.sub_layer_index,
            self.draw_priority,
            tile.z,
            tile.x,
            tile.y,
        )
    }
}

/// The depth slot a style layer occupies, which runs opposite the style index.
///
/// mbgl calls this `currentLayer` and uses it for the depth value, not for painter order
/// directly — but ordering by it is what produces painter order, because it is assigned by
/// walking the layer list in the direction each pass draws. The absolute base is a consumer-side
/// depth mapping and mbgl's own count includes groups this frontend does not model, so what is
/// reproduced here is the ordering, not mbgl's particular integers.
#[must_use]
pub fn depth_slot(layer_index: i32, layer_count: u32) -> u32 {
    #[allow(clippy::cast_sign_loss)]
    let index = layer_index.max(0) as u32;
    layer_count
        .saturating_sub(1)
        .saturating_sub(index.min(layer_count.saturating_sub(1)))
}

/// A view's draw order, and the epoch naming the last one emitted.
#[derive(Debug, Default)]
pub struct DrawOrder {
    entries: Vec<Placed>,
    layer_count: u32,
    epoch: u64,
    emitted: Option<Vec<OrderEntry>>,
}

impl DrawOrder {
    /// An empty order over a style of `layer_count` layers, at epoch zero.
    ///
    /// The layer count is needed because the depth slot is measured from the top of the style,
    /// so a layer's place in the order depends on how many layers there are.
    #[must_use]
    pub fn new(layer_count: u32) -> Self {
        Self {
            layer_count,
            ..Self::default()
        }
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

    /// The style's layer count, which the depth slot is measured against.
    ///
    /// Exposed so a caller holding an order across frames can tell a style change from a camera
    /// one: a different layer count means every depth slot moved, and rebuilding is the only
    /// answer.
    #[must_use]
    pub fn layer_count(&self) -> u32 {
        self.layer_count
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
    /// A drawable whose pass is a mask of several bits becomes one entry per pass. That is not a
    /// convenience: mbgl runs the opaque and translucent passes as separate traversals, and a
    /// background marked `Opaque | Translucent` is genuinely drawn in both — the oracle's order
    /// has forty-three entries for thirty-seven drawables, and the six extra are the background
    /// appearing a second time. Emitting the mask once would leave the consumer to invent the
    /// second entry and to guess where it goes.
    ///
    /// `sort_by_key` rather than `sort_unstable_by_key`: two drawables with an identical key are
    /// genuinely interchangeable in painter order, but keeping their insertion order makes the
    /// output a function of the input rather than of the sort implementation, and a stream that
    /// reordered equal entries between runs would emit an `OrderUpdate` every frame to say
    /// nothing had changed.
    #[must_use]
    pub fn resolve(&self) -> Vec<OrderEntry> {
        let mut expanded: Vec<(Placed, RenderPass)> = Vec::new();
        for placed in &self.entries {
            for pass in [
                RenderPass::OPAQUE,
                RenderPass::TRANSLUCENT,
                RenderPass::PASS3D,
            ] {
                if placed.binding.pass.bits() & pass.bits() != 0 {
                    expanded.push((*placed, pass));
                }
            }
        }
        expanded.sort_by_key(|(placed, pass)| placed.sort_key(*pass, self.layer_count));

        // Per *layer*, from this view's own draw order. mbgl's tweakers are handed a layer
        // group and size their vector to `layerGroup.getDrawableCount()`, incrementing `i` once
        // per drawable they visit — so the index addresses that layer's consolidated buffer and
        // resets at each layer.
        //
        // Numbering per pass across the whole view was the earlier rule, and it is wrong in a
        // way nothing arithmetic could see: the buffers are written per `(view, layer, slot)`,
        // so a second layer's drawables index past the end of their own buffer and pick up
        // whatever the first layer left at that offset. The picture is a layer drawn with
        // another layer's matrices — every tile in the wrong place — and every value that went
        // into it is correct.
        let mut next: alloc::collections::BTreeMap<i32, u32> = alloc::collections::BTreeMap::new();
        expanded
            .iter()
            .map(|(placed, pass)| {
                let slot = next.entry(placed.binding.layer_index).or_default();
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
                    //
                    // The *style* index travels here, not the depth slot the sort used. The
                    // slot is derivable from this and the layer count, while the style index is
                    // what UBO updates are keyed by (§2.2) and cannot be recovered from a slot
                    // whose base depends on how many layer groups mbgl happened to create.
                    layer_index: placed.binding.layer_index.max(0) as u32,
                    sub_layer_index: placed.binding.sub_layer_index,
                    ubo_index,
                    pass: *pass,
                    _pad: [0; 3],
                }
            })
            .collect()
    }

    /// The depth slot at which the opaque pass ends.
    ///
    /// Not an index into the draw order, which is what it looks like and what this returned
    /// before the oracle was consulted. mbgl compares it against the layer's own slot —
    /// `if (currentLayer < opaquePassCutoff)` — so it is a threshold on the layer list, and a
    /// draw-order position substituted for it would cut the passes in the wrong place as soon
    /// as a layer contributed more than one drawable.
    ///
    /// How mbgl *derives* the value is not ported: it comes from the render tree, and the
    /// oracle's own frame reports zero. What is modeled here is the comparison it takes part in,
    /// so a consumer reading this field reads the quantity mbgl's shaders expect.
    #[must_use]
    pub fn opaque_cutoff(&self) -> u32 {
        self.entries
            .iter()
            .filter(|placed| placed.binding.pass.bits() & RenderPass::OPAQUE.bits() != 0)
            .map(|placed| depth_slot(placed.binding.layer_index, self.layer_count))
            .max()
            .map_or(0, |slot| slot + 1)
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
        // A bucket that drew nothing produces no drawable, the way mbgl's `hasData` gates one.
        if !bucket.content.has_data() {
            continue;
        }
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
            // Sublayer 0, not 1: a fill's triangles and outline occupy 1 and 2 so that the
            // outline sorts above the fill it belongs to, and a line has no such pair.
            Content::Line(_) => {
                emit(0, view::fill_pass(), view::tiled_flags());
            }
            // Translucent like the others, and unstencilled unlike them — see `circle_flags`.
            Content::Circle(_) => {
                emit(0, view::fill_pass(), view::circle_flags());
            }
            // Translucent, whatever `raster-opacity` is: mbgl draws a raster layer in the
            // translucent pass and drops it from the frame entirely at an opacity of zero,
            // rather than promoting an opaque one to the opaque pass. An image with an alpha
            // channel is not opaque because its layer is.
            Content::Raster(_) => {
                emit(0, view::fill_pass(), view::tiled_flags());
            }
            // Two drawables, and the order between them is load-bearing. mbgl builds a
            // depth-only pass at sub-layer 0 and a colour pass at 1 whenever the layer is not
            // opaque — `doDepthPass = (!opaque || hasPattern)`, with `opaque` meaning an opacity
            // of one. Without the depth pass every wall alpha-blends against every wall behind
            // it, which reads as a city made of glass rather than as buildings.
            //
            // An opaque extrusion needs only the colour pass, and still takes sub-layer 1 —
            // dropping it to zero would reorder it against a translucent extrusion in the same
            // layer group.
            Content::Fill3d(ref extrusion) => {
                if !extrusion.opaque {
                    emit(0, view::fill_pass(), view::extrusion_depth_flags());
                }
                emit(1, view::fill_pass(), view::extrusion_color_flags());
            }
            // Sublayer 0 and stencilled, which is what `symbol_style.dump` shows: its symbol
            // drawable carries the same flags as the fill above it. Symbols overhang tile edges,
            // so leaving the stencil off would be the defensible guess — the oracle says
            // otherwise, and the oracle is what this is measured against.
            Content::Symbol(_) => {
                emit(0, view::fill_pass(), view::tiled_flags());
            }
        }
    }
    bindings
}

/// A tile ordered the way painter order orders tiles.
#[must_use]
pub fn tile_of(z: u8, x: u32, y: u32) -> TileId {
    wrapped_tile_of(z, x, y, 0)
}

/// As [`tile_of`], for a tile in a world copy other than the first.
///
/// # Why the wrap has to travel
///
/// A cover walks three world copies on each side, so at low zooms the same `z/x/y` appears
/// several times and only the wrap tells them apart. Writing zero for all of them made two
/// copies of one tile indistinguishable on the wire — a consumer given two `ViewUse` records
/// for `0/0/0` cannot know they are different ground, and anything keyed on the tile collapses
/// them into one.
///
/// It stayed invisible because geometry ids were sequential: two copies got different ids from
/// the counter regardless, so nothing downstream had to tell them apart. Keying an id on the
/// tile is what made them collide, and the wire has carried a `wrap` field for exactly this
/// since rev 1.
#[must_use]
pub fn wrapped_tile_of(z: u8, x: u32, y: u32, wrap: i32) -> TileId {
    #[allow(clippy::cast_possible_truncation)]
    TileId {
        x,
        y,
        z,
        overscaled_z: z,
        wrap: wrap as i16,
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

    /// Pass first, then depth slot, then sublayer — and the input order does not survive.
    ///
    /// The background is `Opaque | Translucent`, so it opens the order in the opaque pass and
    /// closes it in the translucent one, with the fills between. Sorting by style index would
    /// have put it first in both.
    #[test]
    fn painter_order_is_pass_then_depth_slot_then_sublayer() {
        let mut order = DrawOrder::new(5);
        // Deliberately backwards on every axis.
        order.bind(fill(2, 2, 4094));
        order.bind(fill(1, 1, 4092));
        order.bind(fill(2, 1, 4092));
        order.bind(background(4093));

        let keys: Vec<(u8, u32, i32)> = order
            .resolve()
            .iter()
            .map(|e| (e.pass.bits(), e.layer_index, e.sub_layer_index))
            .collect();
        assert_eq!(
            keys,
            [
                (1, 0, 0), // background, opaque pass
                (2, 2, 1), // then translucent, topmost layer first
                (2, 2, 2),
                (2, 1, 1),
                (2, 0, 0), // background again, drawn last
            ]
        );
    }

    /// A sort key does not separate a fill's outline from its triangles.
    ///
    /// Sublayer sorts below `draw_priority`, so a layer with a non-zero sort key reorders as a
    /// unit. Sorting sublayer above it would let a sort key paint an outline before the fill it
    /// outlines, which is the R-9 failure in miniature.
    #[test]
    fn a_sort_key_does_not_split_a_fill_from_its_outline() {
        let mut order = DrawOrder::new(5);
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
        let pairs: Vec<(i32, i64)> = resolved
            .iter()
            .map(|e| (e.sub_layer_index, e.draw_priority))
            .collect();
        assert_eq!(
            pairs,
            [(1, 1), (1, 5), (2, 1), (2, 5)],
            "sublayer groups above the sort key, so no outline precedes its fill"
        );
    }

    /// `ubo_index` is dense from zero within each *layer*, not across the view.
    ///
    /// It addresses that layer's consolidated buffer, which is written per
    /// `(view, layer, slot)`. mbgl assigns it the same way: its tweakers take a layer group,
    /// size their vector to `layerGroup.getDrawableCount()`, and increment once per drawable
    /// visited.
    ///
    /// Numbering across the view instead is the failure that has no arithmetic symptom. Every
    /// matrix is right and every buffer is right; the second layer's drawables simply read
    /// past the end of their own buffer, and the map comes out with one layer's tiles wearing
    /// another layer's matrices.
    #[test]
    fn ubo_indices_are_dense_within_each_layer() {
        let mut order = DrawOrder::new(5);
        order.bind(background(4092));
        order.bind(fill(1, 1, 4092));
        order.bind(background(4093));
        order.bind(fill(1, 1, 4093));

        let resolved = order.resolve();
        let mut by_layer: alloc::collections::BTreeMap<u32, Vec<u32>> = Default::default();
        for entry in &resolved {
            by_layer
                .entry(entry.layer_index)
                .or_default()
                .push(entry.ubo_index);
        }
        assert_eq!(by_layer.len(), 2, "two layers");
        for (layer, slots) in by_layer {
            let expected: Vec<u32> = (0..slots.len() as u32).collect();
            assert_eq!(slots, expected, "layer {layer}");
        }
    }

    /// The cutoff is a threshold on the depth slot, not a position in the draw order.
    ///
    /// mbgl compares it against the layer's own slot, so a draw-order position substituted for
    /// it would cut the passes in the wrong place the moment a layer contributed more than one
    /// drawable — which every tiled layer does.
    #[test]
    fn the_opaque_cutoff_is_a_layer_slot_not_a_draw_index() {
        let mut order = DrawOrder::new(5);
        // Six background drawables, all in one layer at one slot.
        for x in 4092..4098 {
            order.bind(background(x));
        }
        order.bind(fill(1, 1, 4092));

        assert_eq!(
            order.opaque_cutoff(),
            5,
            "the background's slot plus one, not its six drawables"
        );
    }

    /// An order with no opaque entries cuts off at zero.
    #[test]
    fn an_all_translucent_order_cuts_off_at_zero() {
        let mut order = DrawOrder::new(5);
        order.bind(fill(1, 1, 4092));
        assert_eq!(order.opaque_cutoff(), 0);
    }

    /// The depth slot runs opposite the style index, so the topmost layer sorts first.
    #[test]
    fn the_depth_slot_runs_opposite_the_style_index() {
        assert_eq!(depth_slot(0, 5), 4, "the bottom layer is deepest");
        assert_eq!(depth_slot(4, 5), 0, "the top layer is shallowest");
        assert!(depth_slot(1, 5) > depth_slot(2, 5));
    }

    /// A layer index past the end of the style clamps rather than underflowing the slot.
    #[test]
    fn a_layer_index_past_the_style_does_not_underflow() {
        assert_eq!(depth_slot(99, 5), 0);
        assert_eq!(depth_slot(-1, 5), 4);
        assert_eq!(
            depth_slot(0, 0),
            0,
            "an empty style has one slot, not a wrap"
        );
    }

    /// §6.3: emitted only when the list differs. An unchanged order writes nothing at all, which
    /// is DR-8's zero-bytes guarantee applied to the order channel.
    #[test]
    fn an_unchanged_order_writes_no_bytes() {
        let mut ring = Ring::new(4096);
        let (producer, _consumer) = ring.split();
        let mut order = DrawOrder::new(5);
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
        let mut order = DrawOrder::new(5);
        order.bind(background(4092));
        assert_eq!(
            order.emit(producer, ViewId(0)).expect("emits").epoch,
            OrderEpoch(1)
        );

        order.bind(fill(1, 1, 4092));
        let second = order.emit(producer, ViewId(0)).expect("emits");
        assert!(second.changed);
        assert_eq!(second.epoch, OrderEpoch(2));
        assert_eq!(
            second.entries, 3,
            "the background is two entries for its two passes, plus the fill"
        );
    }

    /// A restack with the same entry count is still a change. Comparing lengths would miss it,
    /// and the symptom would be a consumer drawing a new order against an old epoch (R-5).
    #[test]
    fn a_restack_of_equal_length_is_a_change() {
        let mut ring = Ring::new(4096);
        let (producer, _consumer) = ring.split();
        let mut order = DrawOrder::new(5);

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
        let mut order = DrawOrder::new(5);

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
        let mut order = DrawOrder::new(5);
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
        let mut order = DrawOrder::new(5);
        let mut odd = fill(1, 1, 4092);
        odd.layer_index = -1;
        order.bind(odd);
        order.bind(fill(1, 1, 4093));

        let resolved = order.resolve();
        assert!(
            resolved.iter().all(|e| e.layer_index < 1000),
            "clamped, not wrapped: {:?}",
            resolved.iter().map(|e| e.layer_index).collect::<Vec<_>>()
        );
        assert_eq!(
            resolved.last().expect("an entry").layer_index,
            0,
            "and a clamped index sorts deepest, not first"
        );
    }
}
