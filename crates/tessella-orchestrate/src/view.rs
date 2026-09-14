//! Binding process-scoped geometry into a view's draw order (§5.3, DR-18).
//!
//! # Two namespaces, and why
//!
//! §5.3 splits rev 1's `DrawableAdd`. Geometry is process-scoped and refcounted; a `ViewUse`
//! binds it into one view's draw order. One `GeometryAdd` plus N uses replaces N copies of the
//! whole record, which is what makes upload bandwidth scale with unique tiles rather than with
//! view count — the mbgl mistake §5 exists to escape.
//!
//! # Declare before use, enforced rather than documented
//!
//! DR-18 moved camera mode onto `ViewDeclare` because it is per view, not per use, and made a
//! `ViewUse` naming an undeclared view a protocol fault. A fault a producer can commit silently
//! is a fault that reaches a consumer, so [`ViewSession`] tracks what has been declared and
//! refuses the use rather than writing it.
//!
//! The consumer cannot check this cheaply — it would have to hold a set of live views and test
//! every use against it, on the tick thread, for a condition only a broken producer creates. It
//! is the producer's invariant to keep.
//!
//! # Pass and draw state, measured
//!
//! From the golden dump, per layer:
//!
//! ```text
//! background   pass=3  flags: depth, color
//! fill         pass=2  flags: stencil, depth, color   sublayer 1
//! fill outline pass=2  flags: stencil, depth, color   sublayer 2
//! ```
//!
//! Pass 3 is `Opaque | Translucent`: a background is opaque where its color is, and mbgl marks
//! it for both rather than deciding per frame. Fills are translucent only. Stencil is on for
//! fills and off for the background, which follows from what they draw — a background covers
//! the viewport and needs no tile clipping, while a fill is per tile and does.

use alloc::collections::BTreeSet;

use tessella_capture_abi::envelope::{
    DrawFlags, GeometryId, TileId, ViewDeclare, ViewId, ViewRelease, ViewTarget, ViewUndeclare,
    ViewUse, WireRecord,
};
use tessella_capture_abi::ring::{Full, Producer};
use tessella_capture_abi::{
    CameraMode, EnvelopeKind, RenderPass, TextureChannelDataType, TexturePixelType,
};

/// Draw state for a layer that covers the viewport rather than a tile.
///
/// Depth and color, no stencil: there is no tile to clip to.
#[must_use]
pub fn background_flags() -> DrawFlags {
    DrawFlags::ENABLE_DEPTH | DrawFlags::ENABLE_COLOR
}

/// Draw state for a layer drawn per tile.
///
/// Stencil as well, because overlapping tiles at different zooms must not double-draw and the
/// consumer resolves that with the clip masks `StencilTiles` describes (§2.2).
#[must_use]
pub fn tiled_flags() -> DrawFlags {
    DrawFlags::ENABLE_STENCIL | DrawFlags::ENABLE_DEPTH | DrawFlags::ENABLE_COLOR
}

/// The pass a background draws in.
///
/// `Opaque | Translucent`, which is what the oracle emits. A background is opaque where its
/// color is opaque, and marking both leaves the choice to the consumer's own opaque-pass
/// cutoff rather than committing per frame.
#[must_use]
pub fn background_pass() -> RenderPass {
    RenderPass::OPAQUE | RenderPass::TRANSLUCENT
}

/// Draw state for a circle.
///
/// Depth and color but *no stencil*: a circle layer is not clipped to the tile mask. The
/// oracle's circle drawable carries `flags=0011` where every fill and line carries `0111`, and
/// the stencil section names three layers rather than four. A circle is drawn from a point
/// whose quad may legitimately overhang the tile it belongs to, and the layout already dropped
/// the points that belong to a neighbor — so the mask would only clip the overhang off.
#[must_use]
pub fn circle_flags() -> DrawFlags {
    DrawFlags::ENABLE_DEPTH | DrawFlags::ENABLE_COLOR
}

/// Draw state for a heatmap's kernels.
///
/// Color and nothing else -- the oracle's `flags=0001`, against a circle's `0011`. The builder
/// calls `setEnableDepth(false)`, and there is no depth buffer on the offscreen target to test
/// against in any case. No stencil either: the kernels are meant to overlap and accumulate
/// across tile boundaries, and a tile clip would leave a seam down every edge where two tiles'
/// kernels should have summed.
#[must_use]
pub fn heatmap_flags() -> DrawFlags {
    DrawFlags::ENABLE_COLOR
}

/// Draw state for a raster tile.
///
/// No stencil, and `RenderRasterLayer` is the evidence: it never calls `setEnableStencil` nor
/// `setStencilTiles`, so it takes the default of false. The clip would be a no-op in any case --
/// a raster drawable is a quad covering exactly its own tile, so there is nothing outside the
/// tile square to cut.
///
/// It is not a no-op here, and that is the point. A raster source is looked up at *its* zoom, so
/// a style with one puts z16 tiles in the frame beside the vector layers' z15. Asking for a
/// stencil put those tiles into the mask buffer, where they overwrote the z15 masks covering the
/// same screen area, and every z15 drawable that tested against one was rejected: the water and
/// the pattern vanished outright while the frame went on issuing all of their drawables. What
/// that looks like is the imagery painting over everything beneath it, which is how it was
/// described and why it was hunted in painter order for so long.
#[must_use]
pub fn raster_flags() -> DrawFlags {
    DrawFlags::ENABLE_DEPTH | DrawFlags::ENABLE_COLOR
}

/// Draw state for a symbol.
///
/// Depth and color but *no stencil*, which is the same answer as [`circle_flags`] and for the
/// same reason: a label is drawn from an anchor and its glyphs legitimately overhang the tile
/// that owns that anchor. Clipping them to the tile square cuts a road name in half at every
/// tile edge it crosses -- a horizontal slice through the letters where the edge runs across
/// the label, and the leading glyphs simply missing where it runs down through them.
///
/// mbgl agrees twice over. `RenderSymbolLayer` never calls `setEnableStencil`, whose default is
/// false; and every symbol drawable in the captures -- `sh0033` in `scaled_style.dump`, `sh0034`
/// in `image_text_style.dump` -- carries `flags=0011` where every fill and line carries `0111`.
#[must_use]
pub fn symbol_flags() -> DrawFlags {
    DrawFlags::ENABLE_DEPTH | DrawFlags::ENABLE_COLOR
}

/// The depth-only pass of a fill extrusion.
///
/// A translucent extrusion is drawn twice: once writing depth and no color, then once writing
/// color. Without the first pass the walls of one building blend against the walls of the
/// building behind it — every surface alpha-blended against every other surface in front of it —
/// which reads as a city made of glass. The depth pass settles what is visible first so the
/// color pass blends only against the ground.
///
/// `IS_3D`, which the ABI has carried since R0 and nothing has set until now: an extrusion is
/// the first geometry in this build that leaves the map plane.
#[must_use]
pub fn extrusion_depth_flags() -> DrawFlags {
    DrawFlags::IS_3D | DrawFlags::ENABLE_DEPTH
}

/// The color pass of a fill extrusion.
///
/// # The stencil follows the depth pass
///
/// mbgl writes `colorBuilder->setEnableStencil(doDepthPass)`, and this used to set no stencil at
/// all — with a comment asserting that mbgl "sets no stencil mode on either extrusion builder"
/// because a building's walls legitimately overhang the tile that owns its footprint. That
/// reasoning is sound and the fact was wrong: the capture's color-pass drawable carries
/// `flags=1111`, and the layer appears in the stencil section with a mask per tile.
///
/// Why it is conditional rather than always on: without a depth pass there is nothing that has
/// already written the tile's stencil for this layer, so testing against it would clip the
/// walls to the tile square and slice every building on a boundary in half — which is what the
/// old comment was describing. With one, the prepass has laid down what the color pass tests
/// against, and skipping the test double-draws wherever two tiles overlap.
#[must_use]
pub fn extrusion_color_flags(depth_pass: bool) -> DrawFlags {
    let base = DrawFlags::IS_3D | DrawFlags::ENABLE_DEPTH | DrawFlags::ENABLE_COLOR;
    if depth_pass {
        base | DrawFlags::ENABLE_STENCIL
    } else {
        base
    }
}

/// The pass a fill draws in.
#[must_use]
pub fn fill_pass() -> RenderPass {
    RenderPass::TRANSLUCENT
}

/// A `ViewUse` for a view that was never declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ViewError {
    /// The view has not been declared, or has been undeclared.
    #[error("view {0} is not declared")]
    NotDeclared(u32),
    /// The ring could not take the record.
    #[error("the ring is full")]
    Full,
    /// A caller declared a view in the offscreen half of the id space.
    ///
    /// [`offscreen_view`] owns everything with the top bit set, so a caller that declares one
    /// would have its view silently aliased by a layer's render target. Refused rather than
    /// left to collide: the collision would show up as a heatmap drawing into a pane.
    #[error("view {0} is in the offscreen id space")]
    Reserved(u32),
    /// A target named a parent that is itself an offscreen view.
    ///
    /// A target is sized against its parent and drawn before it, and a chain of them would have
    /// to be ordered rather than merely paired. Nothing in the style language asks for one.
    #[error("view {0} cannot parent a render target")]
    NestedTarget(u32),
}

impl From<Full> for ViewError {
    fn from(_: Full) -> Self {
        Self::Full
    }
}

/// What a layer's offscreen target is, apart from which view owns it.
///
/// A struct rather than five arguments because the five are one decision — the target a layer
/// needs — and because [`ViewSession::declare_target`] is the only thing that takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetSpec {
    /// The layer whose pass this is, which also derives the view's id.
    pub layer_index: u32,
    /// The id the target's output is bound by, in [`TextureUpdate`]'s space.
    ///
    /// [`TextureUpdate`]: tessella_capture_abi::envelope::TextureUpdate
    pub texture: tessella_capture_abi::envelope::TextureId,
    /// Size against the parent, as numerator and denominator. `(1, 2)` is mbgl's heatmap.
    pub scale: (u16, u16),
    /// Channel layout.
    pub format: TexturePixelType,
    /// Component type. `HalfFloat` for a heatmap, because the kernel sum runs past one.
    pub channel_type: TextureChannelDataType,
}

/// The bit that marks a view as a layer's offscreen target rather than a caller's pane.
///
/// A caller's views are its own — Fluorite numbers the quad's panes 0 through 3 — and an
/// offscreen pass needs a view too (DR-25). Rather than have the producer allocate from a pool
/// and hope the caller does not collide, the top half of the id space is reserved and an
/// offscreen view's id is *derived* from what it is for. Two consequences, both wanted: the
/// same layer of the same view gets the same id in every frame, so re-declaring is idempotent
/// and a consumer can key its render target by it; and a collision is impossible by
/// construction rather than by bookkeeping.
const OFFSCREEN_BIT: u32 = 1 << 31;

/// The offscreen view that draws layer `layer_index` of `parent` into a texture.
///
/// `None` when either input is too large to encode — fifteen bits of parent and sixteen of
/// layer — which is a refusal rather than a truncation, because a truncated id is a valid id
/// that names the wrong pass.
#[must_use]
pub fn offscreen_view(parent: ViewId, layer_index: u32) -> Option<ViewId> {
    if parent.0 >= (1 << 15) || layer_index >= (1 << 16) {
        return None;
    }
    Some(ViewId(OFFSCREEN_BIT | (parent.0 << 16) | layer_index))
}

/// Whether a view is a layer's offscreen target.
#[must_use]
pub fn is_offscreen(view: ViewId) -> bool {
    view.0 & OFFSCREEN_BIT != 0
}

/// Tracks which views have been declared, so a use cannot precede its declaration.
#[derive(Debug, Default)]
pub struct ViewSession {
    declared: BTreeSet<u32>,
}

impl ViewSession {
    /// A session with no views declared.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True when a view has been declared and not undeclared.
    #[must_use]
    pub fn is_declared(&self, view: ViewId) -> bool {
        self.declared.contains(&view.0)
    }

    /// How many views are live.
    #[must_use]
    pub fn len(&self) -> usize {
        self.declared.len()
    }

    /// True when no view is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.declared.is_empty()
    }

    /// As [`Self::declare`], writing the record only when `write` is true.
    ///
    /// The view is marked declared either way. A stream that has told the consumer about this
    /// view once need not repeat it — DR-18 re-emits a declaration when the *configuration*
    /// changes, not every frame — but this session still has to know the view is legitimate, or
    /// the very next `use_geometry` is refused as undeclared.
    ///
    /// # Errors
    ///
    /// As [`Self::declare`].
    pub fn declare_if(
        &mut self,
        producer: &mut Producer,
        view: ViewId,
        mode: CameraMode,
        write: bool,
    ) -> Result<(), ViewError> {
        if write {
            return self.declare(producer, view, mode);
        }
        self.declared.insert(view.0);
        Ok(())
    }

    /// Declares a view and its camera mode.
    ///
    /// Re-declaring is how a mode changes, so it is not an error — but it is the only way, and
    /// every subsequent use inherits the new mode.
    ///
    /// # Errors
    ///
    /// [`ViewError::Full`] when the ring cannot take the record.
    pub fn declare(
        &mut self,
        producer: &mut Producer,
        view: ViewId,
        mode: CameraMode,
    ) -> Result<(), ViewError> {
        if is_offscreen(view) {
            return Err(ViewError::Reserved(view.0));
        }
        self.declare_any(producer, view, mode)
    }

    /// Declares a view without the offscreen-space guard, for [`offscreen_view`]'s own ids.
    fn declare_any(
        &mut self,
        producer: &mut Producer,
        view: ViewId,
        mode: CameraMode,
    ) -> Result<(), ViewError> {
        let record = ViewDeclare {
            view,
            camera_mode: mode as u8,
            _reserved: [0; 3],
        };
        producer.write(EnvelopeKind::ViewDeclare, record.as_bytes(), &[])?;
        self.declared.insert(view.0);
        Ok(())
    }

    /// Declares the offscreen view for one layer of `parent`, and the texture it draws into.
    ///
    /// Two records: the `ViewDeclare` the view needs like any other, then the `ViewTarget` that
    /// makes it draw to a texture. In that order, because a target naming an undeclared view is
    /// the same protocol fault a use would be.
    ///
    /// The camera mode is the parent's. An offscreen pass renders the same scene through the
    /// same camera at a different resolution — that is the whole of what it is — so a mode of
    /// its own would be a second camera nothing drives.
    ///
    /// # Errors
    ///
    /// [`ViewError::NotDeclared`] when `parent` has not been declared, [`ViewError::Reserved`]
    /// when the layer index will not encode, [`ViewError::NestedTarget`] when `parent` is
    /// itself offscreen, and [`ViewError::Full`] when the ring cannot take the records.
    pub fn declare_target(
        &mut self,
        producer: &mut Producer,
        parent: ViewId,
        mode: CameraMode,
        spec: TargetSpec,
    ) -> Result<ViewId, ViewError> {
        if is_offscreen(parent) {
            return Err(ViewError::NestedTarget(parent.0));
        }
        if !self.declared.contains(&parent.0) {
            return Err(ViewError::NotDeclared(parent.0));
        }
        let view = offscreen_view(parent, spec.layer_index).ok_or(ViewError::Reserved(parent.0))?;

        self.declare_any(producer, view, mode)?;
        let record = ViewTarget {
            view,
            parent,
            texture: spec.texture,
            scale_num: spec.scale.0,
            scale_den: spec.scale.1,
            format: spec.format as u8,
            channel_type: spec.channel_type as u8,
            _pad: [0; 2],
        };
        producer.write(EnvelopeKind::ViewTarget, record.as_bytes(), &[])?;
        Ok(view)
    }

    /// Drops a view and everything scoped to it.
    ///
    /// Geometry the view was using is not dropped with it: that is refcounted and
    /// process-scoped, and other views may still hold it.
    ///
    /// # Errors
    ///
    /// [`ViewError::NotDeclared`] when the view was never declared, and [`ViewError::Full`]
    /// when the ring cannot take the record.
    pub fn undeclare(&mut self, producer: &mut Producer, view: ViewId) -> Result<(), ViewError> {
        if !self.declared.contains(&view.0) {
            return Err(ViewError::NotDeclared(view.0));
        }
        let record = ViewUndeclare { view };
        producer.write(EnvelopeKind::ViewUndeclare, record.as_bytes(), &[])?;
        self.declared.remove(&view.0);
        Ok(())
    }

    /// Binds geometry into a view's draw order.
    ///
    /// # Errors
    ///
    /// [`ViewError::NotDeclared`] when the view has not been declared — the DR-18 protocol
    /// fault, caught here rather than shipped — and [`ViewError::Full`] when the ring cannot
    /// take the record.
    pub fn use_geometry(
        &mut self,
        producer: &mut Producer,
        binding: GeometryBinding,
    ) -> Result<(), ViewError> {
        if !self.declared.contains(&binding.view.0) {
            return Err(ViewError::NotDeclared(binding.view.0));
        }
        let record = ViewUse {
            geometry: binding.geometry,
            view: binding.view,
            layer_index: binding.layer_index,
            sub_layer_index: binding.sub_layer_index,
            tile: binding.tile.unwrap_or_default(),
            render_pass: binding.pass,
            draw_flags: binding.flags,
            has_tile: u8::from(binding.tile.is_some()),
            _pad: [0; 5],
        };
        producer.write(EnvelopeKind::ViewUse, record.as_bytes(), &[])?;
        Ok(())
    }

    /// Releases one view's use of geometry.
    ///
    /// # Errors
    ///
    /// As [`ViewSession::use_geometry`].
    pub fn release_geometry(
        &mut self,
        producer: &mut Producer,
        view: ViewId,
        geometry: GeometryId,
    ) -> Result<(), ViewError> {
        if !self.declared.contains(&view.0) {
            return Err(ViewError::NotDeclared(view.0));
        }
        let record = ViewRelease {
            geometry,
            view,
            _pad: 0,
        };
        producer.write(EnvelopeKind::ViewRelease, record.as_bytes(), &[])?;
        Ok(())
    }
}

/// What a `ViewUse` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeometryBinding {
    /// Geometry being bound.
    pub geometry: GeometryId,
    /// View binding it.
    pub view: ViewId,
    /// Layer group, which is the style document's order.
    pub layer_index: i32,
    /// Order within the layer: 1 for a fill's triangles, 2 for its outline.
    pub sub_layer_index: i32,
    /// Tile this geometry covers, or `None` for something that covers the viewport.
    pub tile: Option<TileId>,
    /// Pass or passes it draws in.
    pub pass: RenderPass,
    /// Render state.
    pub flags: DrawFlags,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessella_capture_abi::ring::Ring;

    const VIEW: ViewId = ViewId(0);

    fn binding() -> GeometryBinding {
        GeometryBinding {
            geometry: GeometryId(1),
            view: VIEW,
            layer_index: 1,
            sub_layer_index: 1,
            tile: Some(TileId {
                x: 4093,
                y: 2723,
                z: 13,
                overscaled_z: 13,
                wrap: 0,
            }),
            pass: fill_pass(),
            flags: tiled_flags(),
        }
    }

    /// The DR-18 fault, caught rather than shipped. A consumer would have to hold a set of live
    /// views and test every use against it, on the tick thread, for a condition only a broken
    /// producer creates.
    #[test]
    fn using_an_undeclared_view_is_refused() {
        let mut ring = Ring::new(4096);
        let (producer, consumer) = ring.split();
        let mut session = ViewSession::new();

        assert_eq!(
            session.use_geometry(producer, binding()),
            Err(ViewError::NotDeclared(0))
        );
        assert!(consumer.peek().is_none(), "and nothing was written");

        session
            .declare(producer, VIEW, CameraMode::Consumer)
            .expect("declares");
        assert!(session.use_geometry(producer, binding()).is_ok());
    }

    /// An offscreen view's id is derived, so the same layer of the same view is the same view
    /// every frame and no two layers can collide.
    #[test]
    fn an_offscreen_view_id_is_derived_and_distinct() {
        let first = offscreen_view(ViewId(0), 3).expect("encodes");
        assert_eq!(first, offscreen_view(ViewId(0), 3).expect("encodes"));
        assert_ne!(first, offscreen_view(ViewId(0), 4).expect("encodes"));
        assert_ne!(first, offscreen_view(ViewId(1), 3).expect("encodes"));
        assert!(is_offscreen(first));
        assert!(!is_offscreen(ViewId(0)));
        assert!(
            !is_offscreen(ViewId(u32::MAX >> 1)),
            "the top bit and only it"
        );
    }

    /// Too large to encode is a refusal, not a truncation: a truncated id is a valid id naming
    /// the wrong pass.
    #[test]
    fn an_unencodable_offscreen_view_is_refused() {
        assert_eq!(offscreen_view(ViewId(1 << 15), 0), None);
        assert_eq!(offscreen_view(ViewId(0), 1 << 16), None);
        assert!(offscreen_view(ViewId((1 << 15) - 1), (1 << 16) - 1).is_some());
    }

    /// A caller cannot declare into the offscreen half. The collision it would cause reads as a
    /// heatmap drawing into a pane, which is not a diagnosis anyone reaches from the symptom.
    #[test]
    fn a_caller_cannot_declare_an_offscreen_view() {
        let mut ring = Ring::new(4096);
        let (producer, consumer) = ring.split();
        let mut session = ViewSession::new();

        let reserved = offscreen_view(ViewId(0), 1).expect("encodes");
        assert_eq!(
            session.declare(producer, reserved, CameraMode::Consumer),
            Err(ViewError::Reserved(reserved.0))
        );
        assert!(consumer.peek().is_none(), "and nothing was written");
    }

    /// A target writes its declaration first, then the target itself — the order a consumer
    /// needs, since a target naming an undeclared view is the fault a use would be.
    #[test]
    fn a_target_declares_its_view_before_binding_it() {
        let mut ring = Ring::new(4096);
        let (producer, consumer) = ring.split();
        let mut session = ViewSession::new();

        session
            .declare(producer, VIEW, CameraMode::Consumer)
            .expect("declares the parent");
        let offscreen = session
            .declare_target(
                producer,
                VIEW,
                CameraMode::Consumer,
                TargetSpec {
                    layer_index: 2,
                    texture: tessella_capture_abi::envelope::TextureId(9),
                    scale: (1, 2),
                    format: TexturePixelType::RGBA,
                    channel_type: TextureChannelDataType::HalfFloat,
                },
            )
            .expect("declares the target");
        assert_eq!(offscreen, offscreen_view(VIEW, 2).expect("encodes"));

        let mut kinds = alloc::vec::Vec::new();
        while let Some(record) = consumer.peek() {
            let (kind, consumed) = (record.kind, record.consumed());
            consumer.advance(consumed);
            kinds.push(kind);
        }
        assert_eq!(
            kinds,
            [
                EnvelopeKind::ViewDeclare,
                EnvelopeKind::ViewDeclare,
                EnvelopeKind::ViewTarget
            ]
        );
    }

    /// The target's own fields, which are the renderer's choices rather than the style's.
    #[test]
    fn a_target_carries_its_scale_and_both_type_fields() {
        let mut ring = Ring::new(4096);
        let (producer, consumer) = ring.split();
        let mut session = ViewSession::new();

        session
            .declare(producer, VIEW, CameraMode::Consumer)
            .expect("declares the parent");
        session
            .declare_target(
                producer,
                VIEW,
                CameraMode::Consumer,
                TargetSpec {
                    layer_index: 2,
                    texture: tessella_capture_abi::envelope::TextureId(9),
                    scale: (1, 2),
                    format: TexturePixelType::RGBA,
                    channel_type: TextureChannelDataType::HalfFloat,
                },
            )
            .expect("declares the target");

        let mut target = None;
        while let Some(record) = consumer.peek() {
            let consumed = record.consumed();
            if record.kind == EnvelopeKind::ViewTarget {
                target = ViewTarget::from_bytes(record.record);
            }
            consumer.advance(consumed);
        }
        let target = target.expect("a target reached the ring");
        assert_eq!(target.parent, VIEW);
        assert_eq!(target.texture.0, 9);
        assert_eq!((target.scale_num, target.scale_den), (1, 2));
        assert_eq!(target.format(), Some(TexturePixelType::RGBA));
        assert_eq!(
            target.channel_type(),
            Some(TextureChannelDataType::HalfFloat)
        );
        // mbgl's own numbers, from the heatmap golden.
        assert_eq!(
            target.size(1024, 768),
            Some(tessella_capture_abi::envelope::Extent {
                width: 512,
                height: 384
            })
        );
    }

    /// A target needs its parent declared, and a parent may not itself be offscreen.
    #[test]
    fn a_target_refuses_an_undeclared_or_offscreen_parent() {
        let mut ring = Ring::new(4096);
        let producer = ring.producer();
        let mut session = ViewSession::new();

        let args = |session: &mut ViewSession, producer: &mut Producer, parent: ViewId| {
            session.declare_target(
                producer,
                parent,
                CameraMode::Consumer,
                TargetSpec {
                    layer_index: 0,
                    texture: tessella_capture_abi::envelope::TextureId(1),
                    scale: (1, 2),
                    format: TexturePixelType::RGBA,
                    channel_type: TextureChannelDataType::HalfFloat,
                },
            )
        };

        assert_eq!(
            args(&mut session, producer, VIEW),
            Err(ViewError::NotDeclared(0))
        );

        session
            .declare(producer, VIEW, CameraMode::Consumer)
            .expect("declares");
        let offscreen = args(&mut session, producer, VIEW).expect("declares the target");
        assert_eq!(
            args(&mut session, producer, offscreen),
            Err(ViewError::NestedTarget(offscreen.0)),
            "a target cannot parent a target"
        );
    }

    /// An undeclared view stops accepting uses, because the consumer has dropped everything
    /// scoped to it.
    #[test]
    fn an_undeclared_view_stops_accepting_uses() {
        let mut ring = Ring::new(4096);
        let producer = ring.producer();
        let mut session = ViewSession::new();

        session
            .declare(producer, VIEW, CameraMode::Producer)
            .expect("declares");
        assert!(session.use_geometry(producer, binding()).is_ok());

        session.undeclare(producer, VIEW).expect("undeclares");
        assert!(session.is_empty());
        assert_eq!(
            session.use_geometry(producer, binding()),
            Err(ViewError::NotDeclared(0))
        );
        assert_eq!(
            session.undeclare(producer, VIEW),
            Err(ViewError::NotDeclared(0)),
            "and it cannot be undeclared twice"
        );
    }

    /// The declaration reaches the ring ahead of the use, which is the ordering the protocol
    /// requires and the reason lossless envelopes are in-order.
    #[test]
    fn the_declaration_precedes_the_use_on_the_ring() {
        let mut ring = Ring::new(4096);
        let (producer, consumer) = ring.split();
        let mut session = ViewSession::new();

        session
            .declare(producer, VIEW, CameraMode::Consumer)
            .expect("declares");
        session.use_geometry(producer, binding()).expect("uses");

        let mut kinds = alloc::vec::Vec::new();
        while let Some(record) = consumer.peek() {
            let (kind, consumed) = (record.kind, record.consumed());
            consumer.advance(consumed);
            kinds.push(kind);
        }
        assert_eq!(kinds, [EnvelopeKind::ViewDeclare, EnvelopeKind::ViewUse]);
    }

    /// Camera mode rides on the declaration, once per view, which is what DR-18 moved it there
    /// for. Reading it back proves the raw discriminant round-trips through its accessor.
    #[test]
    fn the_camera_mode_rides_on_the_declaration() {
        let mut ring = Ring::new(4096);
        let (producer, consumer) = ring.split();
        let mut session = ViewSession::new();

        session
            .declare(producer, ViewId(3), CameraMode::Consumer)
            .expect("declares");

        let record = consumer.peek().expect("a record");
        assert_eq!(record.kind, EnvelopeKind::ViewDeclare);
        let declared = ViewDeclare::from_bytes(record.record).expect("a declaration");
        assert_eq!(declared.view, ViewId(3));
        assert_eq!(declared.camera_mode(), Some(CameraMode::Consumer));
        assert_eq!(declared._reserved, [0; 3], "reserved bytes are zero");
    }

    /// The pass and draw state the oracle emits, per layer kind.
    #[test]
    fn the_pass_and_flags_match_the_oracle() {
        // Background: Opaque | Translucent, depth and color, no stencil.
        assert_eq!(background_pass().bits(), 3);
        assert!(background_pass().contains(RenderPass::OPAQUE));
        assert!(background_pass().contains(RenderPass::TRANSLUCENT));
        assert!(!background_flags().contains(DrawFlags::ENABLE_STENCIL));
        assert!(background_flags().contains(DrawFlags::ENABLE_DEPTH));
        assert!(background_flags().contains(DrawFlags::ENABLE_COLOR));

        // Fill: Translucent only, with stencil, because it is drawn per tile and overlapping
        // tiles must not double-draw.
        assert_eq!(fill_pass().bits(), 2);
        assert!(!fill_pass().contains(RenderPass::OPAQUE));
        assert!(tiled_flags().contains(DrawFlags::ENABLE_STENCIL));

        assert!(!background_flags().contains(DrawFlags::IS_3D));
        assert!(!tiled_flags().contains(DrawFlags::IS_3D));
    }

    /// A viewport-covering layer carries no tile, and `has_tile` says so rather than a sentinel
    /// tile id doing it.
    #[test]
    fn a_layer_without_a_tile_says_so() {
        let mut ring = Ring::new(4096);
        let (producer, consumer) = ring.split();
        let mut session = ViewSession::new();
        session
            .declare(producer, VIEW, CameraMode::Producer)
            .expect("declares");

        session
            .use_geometry(
                producer,
                GeometryBinding {
                    tile: None,
                    layer_index: 0,
                    sub_layer_index: 0,
                    pass: background_pass(),
                    flags: background_flags(),
                    ..binding()
                },
            )
            .expect("uses");

        // Skip the declaration.
        let consumed = consumer.peek().expect("declaration").consumed();
        consumer.advance(consumed);

        let record = consumer.peek().expect("a use");
        let used = ViewUse::from_bytes(record.record).expect("a use record");
        assert_eq!(used.has_tile, 0);
        assert_eq!(used.tile, TileId::default());
        assert_eq!(used.layer_index, 0);
        assert_eq!(used._pad, [0; 5]);
    }

    /// Several views bind the same geometry, which is the whole point of the split: one
    /// `GeometryAdd` and N uses, rather than N copies.
    #[test]
    fn many_views_bind_one_geometry() {
        let mut ring = Ring::new(8192);
        let producer = ring.producer();
        let mut session = ViewSession::new();

        for view in 0..4 {
            session
                .declare(producer, ViewId(view), CameraMode::Consumer)
                .expect("declares");
        }
        assert_eq!(session.len(), 4);

        for view in 0..4 {
            session
                .use_geometry(
                    producer,
                    GeometryBinding {
                        view: ViewId(view),
                        ..binding()
                    },
                )
                .expect("uses");
        }

        // Releasing from one view leaves the others holding it.
        session
            .release_geometry(producer, ViewId(0), GeometryId(1))
            .expect("releases");
        assert_eq!(session.len(), 4, "releasing geometry is not undeclaring");
    }
}
