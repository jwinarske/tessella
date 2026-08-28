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
//! Pass 3 is `Opaque | Translucent`: a background is opaque where its colour is, and mbgl marks
//! it for both rather than deciding per frame. Fills are translucent only. Stencil is on for
//! fills and off for the background, which follows from what they draw — a background covers
//! the viewport and needs no tile clipping, while a fill is per tile and does.

use alloc::collections::BTreeSet;

use tessella_capture_abi::envelope::{
    DrawFlags, GeometryId, TileId, ViewDeclare, ViewId, ViewRelease, ViewUndeclare, ViewUse,
    WireRecord,
};
use tessella_capture_abi::ring::{Full, Producer};
use tessella_capture_abi::{CameraMode, EnvelopeKind, RenderPass};

/// Draw state for a layer that covers the viewport rather than a tile.
///
/// Depth and colour, no stencil: there is no tile to clip to.
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
/// colour is opaque, and marking both leaves the choice to the consumer's own opaque-pass
/// cutoff rather than committing per frame.
#[must_use]
pub fn background_pass() -> RenderPass {
    RenderPass::OPAQUE | RenderPass::TRANSLUCENT
}

/// Draw state for a circle.
///
/// Depth and colour but *no stencil*: a circle layer is not clipped to the tile mask. The
/// oracle's circle drawable carries `flags=0011` where every fill and line carries `0111`, and
/// the stencil section names three layers rather than four. A circle is drawn from a point
/// whose quad may legitimately overhang the tile it belongs to, and the layout already dropped
/// the points that belong to a neighbour — so the mask would only clip the overhang off.
#[must_use]
pub fn circle_flags() -> DrawFlags {
    DrawFlags::ENABLE_DEPTH | DrawFlags::ENABLE_COLOR
}

/// The depth-only pass of a fill extrusion.
///
/// A translucent extrusion is drawn twice: once writing depth and no colour, then once writing
/// colour. Without the first pass the walls of one building blend against the walls of the
/// building behind it — every surface alpha-blended against every other surface in front of it —
/// which reads as a city made of glass. The depth pass settles what is visible first so the
/// colour pass blends only against the ground.
///
/// `IS_3D`, which the ABI has carried since R0 and nothing has set until now: an extrusion is
/// the first geometry in this build that leaves the map plane.
#[must_use]
pub fn extrusion_depth_flags() -> DrawFlags {
    DrawFlags::IS_3D | DrawFlags::ENABLE_DEPTH
}

/// The colour pass of a fill extrusion.
#[must_use]
pub fn extrusion_color_flags() -> DrawFlags {
    DrawFlags::IS_3D | DrawFlags::ENABLE_DEPTH | DrawFlags::ENABLE_COLOR
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
}

impl From<Full> for ViewError {
    fn from(_: Full) -> Self {
        Self::Full
    }
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
        let record = ViewDeclare {
            view,
            camera_mode: mode as u8,
            _reserved: [0; 3],
        };
        producer.write(EnvelopeKind::ViewDeclare, record.as_bytes(), &[])?;
        self.declared.insert(view.0);
        Ok(())
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
            _pad: 0,
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
        // Background: Opaque | Translucent, depth and colour, no stencil.
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
        assert_eq!(used._pad, 0);
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
