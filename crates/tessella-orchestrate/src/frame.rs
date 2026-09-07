//! One settled frame, emitted onto the ring.
//!
//! # Why this is a module and not a caller's business
//!
//! Everything here is protocol order, and protocol order is the part a caller cannot be trusted
//! to rediscover. A view must be declared before anything names it (DR-18); geometry must be on
//! the wire before the order that draws it; the order must precede the camera that names its
//! epoch, or a consumer holding a camera whose epoch it has not seen stalls a frame every frame
//! (§4). None of those are arithmetic, so none of them show up as a wrong pixel in a caller that
//! gets them wrong — the picture is simply late, or absent, or drawn against stale uniforms.
//!
//! It lived in a test until now, which meant the only correct driver of this producer was one
//! nothing shipped could call. A second copy in a tool would have been a second thing to keep in
//! step with the first.
//!
//! # What a caller still decides
//!
//! The buckets and the view. This takes a cover's worth of built buckets and a camera and emits
//! them; it does not build tiles, choose a cover, or own a source. That split is §5.1's: the
//! store is process-scoped and the frame is per view.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use tessella_capture_abi::envelope::{OrderEpoch, ViewId};
use tessella_capture_abi::generated::{ubo_layouts, ubo_slots};
use tessella_capture_abi::ring::{Full, Producer};
use tessella_capture_abi::{BuiltIn, CameraMode, declared_for};
use tessella_glyph::fonts::Fonts;
use tessella_glyph::sprite::IconPosition;
use tessella_layout::symbol_bucket::SymbolBuffers;
use tessella_layout::symbol_layout::{Alignments, Placement};
use tessella_style::crossfade::ZoomHistory;
use tessella_style::light::Light;
use tessella_style::property::ResolvedProperty;
use tessella_style::{LayerKind, Style};
use tessella_tile::cover::{TileCoord, ViewTransform};
use tessella_tile::renderables::DataTileId;

use crate::binder::{
    CIRCLE_FAMILY, FILL_EXTRUSION_FAMILY, FILL_FAMILY, LINE_FAMILY, SYMBOL_FAMILY, attribute_ids,
    layout, permutation_key,
};
use crate::camera::CameraBlock;
use crate::emit::SlabArena;
use crate::order::{self, DrawOrder};
use crate::registry::{DrawableKey, Session};
use crate::tile::{Content, LayerBucket, TileId};
use crate::ubo::{self, DrawableEntry};
use crate::view::{GeometryBinding, ViewSession};
use crate::{emit, stencil, texture};

/// What went wrong emitting a frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// The ring had no room. A frame is emitted whole or not at all.
    #[error("the ring is full")]
    Full,
    /// The camera could not be resolved into a matrix.
    #[error("camera: {0}")]
    Camera(alloc::string::String),
    /// The view could not be declared.
    #[error("view: {0}")]
    View(alloc::string::String),
    /// The arena's region had no room for the frame's geometry.
    ///
    /// Distinct from [`Self::Full`], because the recourse differs. A full ring clears when the
    /// consumer drains it and the producer waits. A full region does not clear on its own: the
    /// arena bump allocates, so the space a swept slab left is only recovered once everything
    /// above it has gone too. The caller displaces what its poorly-packed slabs still hold —
    /// DR-21's compaction — sweeps, and tries again.
    #[error("the slab region is full")]
    RegionFull,
}

impl From<Full> for FrameError {
    fn from(_: Full) -> Self {
        Self::Full
    }
}

impl From<crate::view::ViewError> for FrameError {
    /// Keeps a full ring distinguishable from a view fault.
    ///
    /// These used to be flattened into `View(format!("{error}"))`, which turned backpressure
    /// into a string. A caller cannot act on that: a full ring is the ordinary consequence of a
    /// consumer that stalled for a frame and the response is to try again, where a view fault is
    /// a protocol error and retrying repeats it. The message read "view: the ring is full",
    /// which says the right words under the wrong variant.
    fn from(error: crate::view::ViewError) -> Self {
        match error {
            crate::view::ViewError::Full => Self::Full,
            other => Self::View(alloc::format!("{other}")),
        }
    }
}

/// What one frame put on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emitted {
    /// Geometries announced.
    pub geometries: usize,
    /// Per-view uses bound into the order.
    ///
    /// Every drawable in the frame, announced this time or not. A caller comparing this against
    /// `geometries` is comparing what was drawn against what had to be sent, which is the whole
    /// measure of an incremental emission.
    pub drawables: usize,
    /// `ViewUse` records actually written.
    ///
    /// Which is not [`Self::drawables`] once a registry is in play: a use is durable, so a
    /// drawable already bound is drawn again without a record. The difference between the two is
    /// what retention saves; conflating them reads a settled frame as though it had re-sent
    /// everything it drew.
    pub uses: usize,
    /// Drawables released and removed because they left the cover.
    pub removed: usize,
    /// Drawables let go so their slab could be emptied, to be announced again next frame.
    ///
    /// Not a loss: they are still drawn this frame, from the geometry the consumer already has.
    /// What moves is where their bytes will live once they are re-announced.
    pub displaced: usize,
    /// The order epoch the camera names.
    pub epoch: OrderEpoch,
}

impl Default for Emitted {
    fn default() -> Self {
        Self {
            geometries: 0,
            drawables: 0,
            uses: 0,
            removed: 0,
            displaced: 0,
            epoch: OrderEpoch(0),
        }
    }
}

/// One view's buckets, per tile of its cover.
pub struct Frame<'a> {
    /// The style the buckets were built from. Uniforms are read from it per layer.
    pub style: &'a Style,
    /// The camera.
    pub view: &'a ViewTransform,
    /// Which view this is.
    pub view_id: ViewId,
    /// The cover, for the clip masks.
    pub tiles: &'a [TileCoord],
    /// Built buckets, per tile, in cover order.
    pub buckets: &'a [(TileId, Vec<LayerBucket>)],
    /// The store's bucket list each entry of `buckets` was taken from, for the symbol layout
    /// cache to key on. `None` where the frame built the list itself and there is no identity to
    /// key on -- the sourceless background, which carries no symbols.
    pub origins: &'a [Option<alloc::sync::Arc<Vec<LayerBucket>>>],
    /// The style light, which travels in the camera block (§2.2).
    pub light: &'a Light,
    /// Glyphs, for the symbol layers.
    ///
    /// `None` means no symbol layer is drawn, and that is a legitimate frame rather than an
    /// error: a symbol layer's glyphs are a *fetch*, discovered only once `text-field` has been
    /// evaluated against the tile's own features, so a caller that has not run that round trip
    /// has nothing to pass and no way to invent it.
    pub fonts: Option<&'a Fonts>,
    /// Sprites, for the layers that carry a pattern.
    ///
    /// `None` for the same reason `fonts` may be: a pattern's sprites are a fetch, and which
    /// ones a frame needs is discovered only once each layer's pattern expression has been
    /// evaluated at the zooms a fade can reach. A caller that has not made that round trip has
    /// nothing to pass, and every pattern layer then draws as a plain fill.
    pub patterns: Option<&'a Patterns<'a>>,
}

impl crate::tile::PatternLookup for Patterns<'_> {
    /// One feature's pattern, for whichever of the four properties the layer carries.
    ///
    /// The layer's kind decides which property to read, and a layer carries at most one — so
    /// trying each in turn costs a map lookup and saves the caller having to say.
    fn resolve(
        &self,
        layer: &tessella_style::Layer,
        zoom: f64,
        feature: &dyn tessella_style::expression::Feature,
    ) -> Option<([u16; 4], [u16; 4])> {
        tessella_style::crossfade::PATTERN_PROPERTIES
            .iter()
            .find_map(|property| self.feature_placement(layer, property, zoom, feature))
    }
}

/// The sprites a frame's patterns resolve against, and where the camera has been.
///
/// # Built by the caller, like the glyph atlas
///
/// The frame emitter does not pack an atlas any more than it fetches a glyph range. It is
/// handed one, and the caller decides how long it lives — which matters because the atlas is
/// *shared across tiles*, as mbgl's is: one copy of each sprite, referenced by every tile that
/// names it, with only the position map per tile. An atlas per tile would put the same fifty by
/// fifty pixels in the stream once per tile of the cover.
pub struct Patterns<'a> {
    /// The texture the atlas was uploaded as.
    pub texture: tessella_capture_abi::envelope::TextureId,
    /// Its dimensions, which the shader needs to turn a rectangle into texture coordinates.
    pub size: [u16; 2],
    /// Where each sprite was packed, by name.
    pub positions: &'a alloc::collections::BTreeMap<alloc::string::String, IconPosition>,
    /// The atlas's pixels, RGBA, which go up before any drawable names the texture.
    pub pixels: &'a [u8],
    /// Which way the camera last crossed an integer zoom, which chooses a fade's `from`.
    pub history: ZoomHistory,
}

impl Patterns<'_> {
    /// The mix a pattern is at, and how each of its two images is scaled.
    ///
    /// No clock is threaded through: `crossfade` is given mbgl's "no time" sentinel, which
    /// leaves the time term complete and the mix driven by the zoom's fractional part alone.
    /// That is what the oracle's capture carries — a fade of one — because the probe evaluates
    /// outside a frame and passes the same sentinel. Animating a fade over its duration needs a
    /// clock the producer does not have and the caller does.
    #[must_use]
    pub fn crossfade(&self, zoom: f64) -> tessella_style::crossfade::Crossfade {
        tessella_style::crossfade::crossfade(zoom, &self.seeded(zoom), None, 0)
    }

    /// The history, seeded at `zoom` if it has never been updated.
    ///
    /// A default [`ZoomHistory`] has `last_integer_zoom` of zero, so every positive zoom looks
    /// like the camera zoomed in from the bottom of the world — a caller that forgot to update
    /// it gets `from_scale` of two where the level being left is the one above, and a pattern
    /// drawn at the wrong size with nothing reporting it. Seeding on read gives what mbgl's
    /// first `update` gives, which is the answer for a camera that has not moved yet.
    fn seeded(&self, zoom: f64) -> ZoomHistory {
        let mut history = self.history;
        if history.first {
            history.update(zoom, None);
        }
        history
    }

    /// The pair of rectangles a feature's pattern resolves to, for the composite binder.
    ///
    /// Distinct from [`Self::placement`], which answers for the *layer* at a zoom. This answers
    /// for one feature, which is the case a uniform cannot carry.
    #[must_use]
    pub fn feature_placement(
        &self,
        layer: &tessella_style::Layer,
        property: &str,
        zoom: f64,
        feature: &dyn tessella_style::expression::Feature,
    ) -> Option<([u16; 4], [u16; 4])> {
        use tessella_style::crossfade::faded;

        let value = layer.paint.get(property)?;
        let expression = value.as_expression()?;
        let parsed = tessella_style::Expression::parse(expression.value()).ok()?;
        let image = |z: f64| match parsed.evaluate(Some(z), Some(feature)) {
            Ok(tessella_style::Value::String(name)) if !name.is_empty() => Some(name),
            _ => None,
        };
        let pair = faded(image, zoom, &self.seeded(zoom));
        Some((
            ubo::atlas_rect(self.positions.get(pair.from?.as_str())?),
            ubo::atlas_rect(self.positions.get(pair.to?.as_str())?),
        ))
    }

    /// A background's block, which needs the images' display sizes as well as their rectangles.
    ///
    /// Separate from [`Self::placement`] because a background is the only kind whose block
    /// carries `pattern_size`, and getting it needs the positions themselves rather than the
    /// rectangles derived from them.
    #[must_use]
    pub fn background_placement(
        &self,
        paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
        zoom: f64,
        opacity: f32,
    ) -> Option<ubo::BackgroundPatternPlacement> {
        use tessella_style::crossfade::{PatternSource as _, faded};

        let source = paint.get("background-pattern")?;
        let pair = faded(|z| source.image_at(z), zoom, &self.seeded(zoom));
        let from = self.positions.get(pair.from?.as_str())?;
        let to = self.positions.get(pair.to?.as_str())?;
        Some(ubo::BackgroundPatternPlacement {
            placement: ubo::pattern_placement(Some(from), Some(to), self.size)?,
            display: [ubo::display_size(from), ubo::display_size(to)],
            crossfade: self.crossfade(zoom),
            opacity,
        })
    }

    /// The two rectangles a layer's pattern is between at `zoom`, if both are packed.
    ///
    /// `None` when the layer has no pattern, when its expression names nothing, or when a name
    /// it does give is missing from the atlas — see [`ubo::pattern_placement`] for why a missing
    /// image places nothing rather than falling back.
    #[must_use]
    pub fn placement(
        &self,
        paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
        property: &str,
        zoom: f64,
    ) -> Option<ubo::PatternPlacement> {
        use tessella_style::crossfade::{PatternSource as _, faded};

        let source = paint.get(property)?;
        let pair = faded(|z| source.image_at(z), zoom, &self.seeded(zoom));
        ubo::pattern_placement(
            self.positions.get(pair.from?.as_str()),
            self.positions.get(pair.to?.as_str()),
            self.size,
        )
    }
}

/// The first texture id a glyph atlas takes.
///
/// One per font stack, not one for all of them. A style names several -- liberty asks for regular,
/// italic and bold -- and each gets its own packed atlas with its own coordinates. Publishing them
/// all to one id made every stack overwrite the one before it, so the frame drew with whichever
/// landed last: the coordinates a label carried were right and the pixels under them belonged to
/// another font, which is a blank patch of atlas nine times in ten.
///
/// Which texture a *symbol* samples is a second question and unchanged: one slot whichever kind
/// of symbol it is, because mbgl's `DrawableAtlasesTweaker` gives a shader with no separate icon
/// sampler the glyph atlas for a text drawable and the icon atlas for an icon drawable.
const GLYPH_ATLAS_BASE: u64 = 2;

/// How many stacks can be published before the ids would run into the raster tiles'.
const GLYPH_ATLAS_CAP: usize = (RASTER_TEXTURE_BASE - GLYPH_ATLAS_BASE) as usize;

/// The atlas id for the `index`th font stack of a frame.
fn glyph_atlas_id(index: usize) -> tessella_capture_abi::envelope::TextureId {
    tessella_capture_abi::envelope::TextureId(GLYPH_ATLAS_BASE + index as u64)
}

/// The first texture id a raster tile's picture takes.
///
/// One per tile rather than one per layer: a raster tile *is* its picture, and two raster layers
/// over one source are two buckets sharing one image (§11.5). Numbered above the atlases so a
/// raster texture and a glyph atlas never collide.
const RASTER_TEXTURE_BASE: u64 = 16;

/// The texture id a raster tile's picture takes, derived from the tile itself.
///
/// # Not the tile's position in this frame
///
/// It was `RASTER_TEXTURE_BASE + index`, the tile's index in the frame's bucket list. That is
/// stable only while the list is, and the list grows as tiles arrive -- while the textures and
/// the drawables that name them live across frames. So a later frame handed the same id to a
/// different tile, the consumer's texture map took the new picture at that key, and every
/// drawable still holding the id started sampling it. Tiles ended up wearing each other's
/// imagery: two of them reported the same texture, and which two depended on the order the
/// network answered in, so the same frame scored anywhere from 64% to 74% of pixels exact
/// against the oracle between runs.
///
/// Packed rather than hashed, so it is injective by construction: zoom above column above row
/// above the world copy, in fields wide enough for `MAX_ZOOM`. Added to the base so it can never
/// land on a glyph atlas.
#[must_use]
fn raster_texture_id(
    z: u8,
    x: u32,
    y: u32,
    wrap: i32,
) -> tessella_capture_abi::envelope::TextureId {
    #[allow(clippy::cast_sign_loss)]
    let copy = u64::from((wrap.clamp(-7, 7) + 8) as u8);
    let packed = (u64::from(z) << 52) | (u64::from(x) << 28) | (u64::from(y) << 4) | copy;
    tessella_capture_abi::envelope::TextureId(RASTER_TEXTURE_BASE + packed)
}

/// Emits a whole frame: state, geometry, uniforms, order, camera — in that order.
///
/// # Errors
///
/// [`FrameError`] when the ring fills, the camera will not resolve, or the view will not declare.
pub fn emit(
    producer: &mut Producer,
    arena: &mut SlabArena,
    frame: &Frame<'_>,
) -> Result<Emitted, FrameError> {
    // A frame is thirty records or six hundred, and they only mean anything together: geometry
    // to register, an order saying where to draw it, a camera naming that order's epoch. Written
    // one at a time they published as they landed, so a ring that filled halfway left a consumer
    // holding geometry with no order and no camera — and the retry registered the same buckets
    // again under fresh ids. `FrameError::Full` has always claimed a frame is emitted whole or
    // not at all; this is the claim being made true.
    // A session that lives exactly as long as this call, which makes every drawable new, every
    // geometry announced and the view declared — the full emission, as the degenerate case of
    // the incremental one rather than as a second implementation of it.
    emit_into(
        producer,
        arena,
        &mut SymbolCache::default(),
        &mut PlacementState::new(),
        frame,
        Some(&mut Session::new()),
    )
}

/// As [`emit()`], sending only what the consumer does not already have.
///
/// # What changes
///
/// [`emit()`] announces every geometry every time, which is why `GeometryId`'s documentation says
/// an emission replaces the previous set entire and an id is not a cache key. Given a registry,
/// an id belongs to a drawable instead — the tile, the layer, the order within it — so a tile
/// that survives a pan keeps its id, its geometry is announced once, and only what arrived is
/// sent. What left is released and removed.
///
/// The registry and the arena both have to outlive the frame, and that is the point: the arena
/// keeps a retained geometry's bytes, and `GeometryRemove` is what says they can go. Passing a
/// fresh one of either each frame reduces this to [`emit()`] with more steps.
///
/// # Errors
///
/// As [`emit()`]. A frame that fails retires nothing and retains nothing — the registry is only
/// swept on success, so a retry sees the state the failed attempt started from.
pub fn emit_incremental(
    producer: &mut Producer,
    arena: &mut SlabArena,
    layouts: &mut SymbolCache,
    placement: &mut PlacementState,
    frame: &Frame<'_>,
    session: &mut Session,
) -> Result<Emitted, FrameError> {
    emit_into(producer, arena, layouts, placement, frame, Some(session))
}

fn emit_into(
    producer: &mut Producer,
    arena: &mut SlabArena,
    layouts: &mut SymbolCache,
    placement: &mut PlacementState,
    frame: &Frame<'_>,
    session: Option<&mut Session>,
) -> Result<Emitted, FrameError> {
    producer.begin();
    crate::watch::begin_frame();
    let mark = arena.mark();
    let mut session = session;
    let key = camera_key(frame.view);
    let camera_moved = session
        .as_deref()
        .is_none_or(|session| session.camera_differs(frame.view_id, &key));
    // A view is declared once and its constant textures sent once. DR-18 re-emits a declaration
    // only when the configuration changes, and the placeholders never change at all.
    let declare = session
        .as_deref()
        .is_none_or(|session| session.needs_declaring(frame.view_id));
    if let Some(session) = session.as_deref_mut() {
        session.registry().begin_frame(frame.view_id);
    }
    let attempt = emit_group(
        producer,
        arena,
        layouts,
        placement,
        frame,
        session.as_deref_mut(),
        camera_moved,
        declare,
    );
    // Checked here rather than at each allocation, and before the commit rather than after it.
    // An arena over a shared region reports a short allocation instead of returning a reference
    // to bytes it did not write — a `GeometryAdd` naming those would be perfectly well formed
    // and name nothing — so this is where that becomes a frame that did not happen.
    let attempt = match attempt {
        Ok(_) if arena.is_full() => Err(FrameError::RegionFull),
        other => other,
    };
    match attempt {
        Ok(emitted) => {
            // Only now: a frame that could not be written must leave the registry as it found
            // it, so the retry announces the same geometry rather than assuming the consumer
            // has what the failed attempt never sent.
            if let Some(session) = session {
                // The arena moves with the commit and not before: `retire` hands back what this
                // frame let go, staged there since it was decided.
                for reference in session.registry().retire() {
                    arena.release(reference);
                }
                // And the slabs those releases emptied. Releasing hands back the *bytes* a
                // drawable held; a slab whose last reference went is still a slot with a length
                // in the region's table until this drops it. Without the sweep nothing is ever
                // reclaimable: a region-backed arena's cursor climbs for as long as the map
                // moves and every slab looks live, which is a map that fills its region and
                // stops drawing. The removal records went out above, so a consumer is not
                // holding any of these.
                // The accounting the arena documents: a slab's live bytes should equal the
                // lengths of every reference the registry still holds into it. A divergence is a
                // release without its counterpart, and it matters because `sweep` frees a slab
                // whose count reaches zero -- so a slab over-released on one drawable's account
                // takes every other drawable's bytes with it.
                if crate::watch::watching() {
                    let mut held: alloc::collections::BTreeMap<u32, usize> =
                        alloc::collections::BTreeMap::new();
                    for (_, refs) in session.registry().live_refs() {
                        for reference in refs {
                            *held.entry(reference.slab).or_insert(0) += reference.length as usize;
                        }
                    }
                    for (slab, want) in &held {
                        let have = arena.live_bytes(*slab);
                        if have != *want {
                            crate::watch::accounting(*slab, have, *want);
                        }
                    }
                }
                arena.sweep();
                session.record_camera(frame.view_id, key);
                session.record_declared(frame.view_id);
            }
            crate::watch::frame_end(emitted.geometries, emitted.removed, emitted.uses, true);
            producer.commit();
            Ok(emitted)
        }
        Err(error) => {
            crate::watch::frame_end(0, 0, 0, false);
            producer.abort();
            // The arena as well as the ring. The discarded records were the only things that
            // would ever have named these slabs.
            arena.rewind(mark);
            // And the registry, which handed out ids during the binding pass before anything
            // was written. All three roll back together or the retry is working from a state
            // the consumer never saw.
            if let Some(session) = session {
                session.registry().rollback();
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_group(
    producer: &mut Producer,
    arena: &mut SlabArena,
    layouts: &mut SymbolCache,
    placement: &mut PlacementState,
    frame: &Frame<'_>,
    stream: Option<&mut Session>,
    camera_moved: bool,
    declare: bool,
) -> Result<Emitted, FrameError> {
    // Where the ring stood before this frame wrote anything, so the camera gate below can ask
    // whether it did.
    let opened_at = producer.head();
    let Frame {
        style,
        view,
        view_id,
        tiles,
        buckets,
        origins: _,
        light,
        fonts,
        patterns,
    } = *frame;

    let mut session = ViewSession::new();
    session
        .declare_if(producer, view_id, CameraMode::Producer, declare)
        .map_err(FrameError::from)?;

    // Frame-wide state the shaders read whatever the style says. The placeholders matter: a
    // shader samples its texture slots unconditionally, so a drawable whose layer binds none
    // still reads whatever was last at that slot.
    for upload in texture::placeholders() {
        if !declare {
            break;
        }
        texture::write(producer, &upload)?;
    }
    // The glyph atlas, before any drawable names it. A symbol geometry carries a texture
    // reference, and a reference to a texture the consumer has not been given is a drawable that
    // samples whatever was last at that slot.
    let stacks = symbol_stacks(buckets);
    if let Some(fonts) = fonts {
        for (index, stack) in stacks.iter().enumerate().take(GLYPH_ATLAS_CAP) {
            if let Some(atlas) = fonts.atlas(stack) {
                let (width, height) = atlas.size();
                let whole = [tessella_glyph::atlas::Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                }];
                if let Some(upload) = texture::glyph_atlas(glyph_atlas_id(index), atlas, &whole) {
                    texture::write(producer, &upload)?;
                }
            }
        }
    }

    // The sprite atlas, before any drawable names it — the reason the glyph atlas goes up
    // here, and the same failure if it does not: a texture reference the consumer has not been
    // given samples whatever was last at that slot.
    if let Some(patterns) = patterns
        && let Some(upload) =
            texture::pattern_atlas(patterns.texture, patterns.size, patterns.pixels)
    {
        texture::write(producer, &upload)?;
    }

    // Derived from the viewport, so it moves when the camera does and not otherwise.
    if camera_moved || declare {
        let global = ubo::GlobalPaintParams::for_view(view, [64.0, 64.0], 1.0).pack();
        ubo::write(
            producer,
            view_id,
            ubo::FRAME_WIDE,
            ubo_slots::ID_GLOBAL_PAINT_PARAMS_UBO,
            &global,
        )?;
    }

    // Held across frames when there is a stream, so an order identical to the last one it sent
    // recognises itself and stays off the ring. `DrawOrder` has always suppressed that; building
    // a fresh one every frame threw the memory away before it could.
    #[allow(clippy::cast_possible_truncation)]
    let layer_count = style.layers.len() as u32;
    // `session` below is the view session that writes `ViewDeclare` and `ViewUse`; this is the
    // stream's memory across frames.
    let mut owned_order;
    // Both at once: a frame needs the registry and the order for its whole length, and they are
    // different fields of the same stream.
    let (mut registry, draw_order): (Option<&mut _>, &mut DrawOrder) = match stream {
        Some(stream) => {
            let (registry, order) = stream.split(view_id, layer_count);
            (Some(registry), order)
        }
        None => {
            owned_order = DrawOrder::new(layer_count);
            (None, &mut owned_order)
        }
    };
    let mut next_id = 0;
    let mut by_layer: BTreeMap<i32, Vec<GeometryBinding>> = BTreeMap::new();
    let mut emitted = Emitted::default();
    // Which bucket each geometry id came from, so the packing pass below can revisit them in
    // draw order rather than in the order the tiles arrived.
    let mut source: BTreeMap<u64, (usize, usize, tessella_capture_abi::envelope::TextureId)> =
        BTreeMap::new();
    let mut bound: Vec<GeometryBinding> = Vec::new();
    // One entry per bucket that reached the arena, so a bucket's second drawable reuses the
    // bytes rather than copying them.
    let mut packed_bytes: BTreeMap<(usize, usize), alloc::vec::Vec<emit::Encoded>> =
        BTreeMap::new();
    // Which drawables this frame is announcing for the first time, and what each id names.
    // Empty without a registry, which is what makes the unregistered path emit everything.
    let mut fresh: BTreeSet<DrawableKey> = BTreeSet::new();
    // Drawables this view is not yet bound to, which is a wider set than `fresh`.
    let mut unbound: BTreeSet<DrawableKey> = BTreeSet::new();
    let mut keyed: BTreeMap<u64, DrawableKey> = BTreeMap::new();

    for (index, (tile, tile_buckets)) in buckets.iter().enumerate() {
        // The wrap comes from the cover, which is the only place that has it: a bucket's `TileId`
        // is canonical and carries no world copy. Read here rather than below because the texture
        // id is a function of the tile *and* its copy.
        let wrap = tiles.get(index).map_or(0, |coord| coord.wrap);

        // A raster tile's picture goes up before any drawable names it, for the reason the glyph
        // atlas does: a texture reference the consumer has not been given samples whatever was
        // last at that slot.
        let raster_texture = raster_texture_id(tile.z, tile.x, tile.y, wrap);
        for bucket in tile_buckets {
            if let Content::Raster(raster) = &bucket.content
                && let Some(upload) = texture::raster_tile(raster_texture, &raster.image)
            {
                texture::write(producer, &upload)?;
                break;
            }
        }

        // `Frame::buckets` is documented as being in cover order, and this is what depends on
        // that -- at low zooms the same `z/x/y` appears in several copies and only the wrap tells
        // them apart.
        let at = order::wrapped_tile_of(tile.z, tile.x, tile.y, wrap);
        let mut bindings =
            order::bindings_for(view_id, at, tile_buckets, &mut next_id, fonts.is_some());

        // With a registry the id belongs to the drawable rather than to its place in this
        // frame's cover, so `bindings_for`'s sequential numbering is replaced. It still runs:
        // it is what decides how many drawables a bucket makes and which sub-layers they take,
        // and only the number it stamped is wrong for a retained stream.
        if let Some(registry) = registry.as_deref_mut() {
            for binding in &mut bindings {
                let key = DrawableKey {
                    tile: binding.tile,
                    layer_index: binding.layer_index,
                    sub_layer_index: binding.sub_layer_index,
                };
                // Two questions, and §5.3 makes them different: whether any view has the
                // geometry, which gates the announcement, and whether *this* view uses it,
                // which gates the binding. A second view picking up a tile the first already
                // draws needs a `ViewUse` and no `GeometryAdd`.
                if registry.is_new(&key) {
                    fresh.insert(key);
                }
                if registry.is_unused_by(&key) {
                    unbound.insert(key);
                }
                binding.geometry = registry.id_for(key);
                keyed.insert(binding.geometry.0, key);
            }
        }

        // A binding per drawable, and a bucket may produce two of them — a fill's triangles and
        // its outline. Each gets its own geometry id from `bindings_for`, so each is announced
        // separately rather than one being bound to an id nothing declared.
        let mut binding_index = 0;
        for (bucket_index, bucket) in tile_buckets.iter().enumerate() {
            // The same predicate `bindings_for` skipped on, so `binding_index` stays aligned
            // with the bindings it produced. Two different answers here would pair a bucket's
            // drawables with another bucket's ids.
            if !bucket.content.is_encodable(fonts.is_some()) {
                continue;
            }
            let drawables = bucket.drawable_count();
            for _ in 0..drawables {
                let Some(binding) = bindings.get(binding_index) else {
                    break;
                };
                binding_index += 1;
                source.insert(binding.geometry.0, (index, bucket_index, raster_texture));
            }
        }

        for binding in bindings {
            by_layer
                .entry(binding.layer_index)
                .or_default()
                .push(binding);
            draw_order.bind(binding);
            bound.push(binding);
        }
    }

    // Every layer's matrices and clip masks follow the camera and the cover, and a frame that
    // moved neither has already established that both are where they were. `scene_changed` is
    // the cover half: a drawable arriving, one this view had not bound, or one leaving.
    let scene_changed = registry.as_deref().is_none_or(|registry| {
        !fresh.is_empty() || !unbound.is_empty() || !registry.released().is_empty()
    });

    // The geometry, packed in the order it will be drawn.
    //
    // Nothing about the wire requires this: a `GeometryAdd` names its own slab, so a consumer
    // reads the same scene whichever order the buckets were packed in. What it requires is a
    // slab per drawable, because the packing order was the tile loop above and the draw order
    // is by layer — so a layer's forty-two tiles land in forty-two different slabs, and a
    // consumer that wanted to draw them together cannot: one draw call reads one vertex buffer.
    //
    // Packing in `resolve()`'s order instead puts a layer's tiles adjacent in the arena, where
    // they share a slab whenever one holds them. That is the whole of what the producer can do
    // for batching, and it is worth doing here rather than leaving the consumer to copy the
    // buckets into a buffer of its own — a copy per frame of every byte of geometry.
    // Which buckets have anything new in them. The skip below is bucket-scoped rather than
    // drawable-scoped because a bucket's drawables share an encoding: a fill's outline is built
    // from the fill's own vertices, out of the `packed_bytes` entry the fill's encode left
    // behind. Skipping the fill and then encoding the outline would find no entry and take the
    // fresh path, which encodes a *fill* under the outline's id — the corruption is silent,
    // because the record is well formed and simply draws the wrong thing.
    //
    // In practice a bucket's drawables are always fresh or known together: they enter the
    // registry in the same binding pass and leave the cover in the same frame. Scoping the
    // skip to the bucket means nothing has to rely on that.
    let fresh_buckets: BTreeSet<(usize, usize)> = source
        .iter()
        .filter(|(geometry, _)| keyed.get(geometry).is_some_and(|key| fresh.contains(key)))
        .map(|(_, &(tile_index, bucket_index, _))| (tile_index, bucket_index))
        .collect();

    // Built once for the frame and handed to every bucket, so the labels compete with each other
    // rather than each layer of each tile competing with itself alone.
    // Borrowed, not built. `PlacementState` is the map's and outlives the frame, which is what
    // lets a fade run: see its documentation for why both the identity and the fade had to stop
    // being per-frame together.
    let placement = core::cell::RefCell::new(placement);
    placement.borrow_mut().symbols.begin();

    // Resolved once and used twice: placement walks it backwards, encoding forwards.
    let order = draw_order.resolve();
    let prepared = place_symbols(
        &order,
        &source,
        buckets,
        frame.origins,
        layouts,
        tiles,
        fonts,
        patterns,
        style,
        view,
        &placement,
    );

    let mut packed: BTreeSet<u64> = BTreeSet::new();
    let mut open: Option<u32> = None;
    for entry in order.iter().copied() {
        // A drawable whose pass is a mask appears once per pass; its geometry is packed once.
        if !packed.insert(entry.geometry.0) {
            continue;
        }
        // One slab per (view, layer), which is DR-16's consolidated buffer — and per layer
        // rather than per sub-layer, because a bucket's drawables share their geometry and so
        // land on both sides of a sub-layer boundary. They still batch separately: the run is
        // keyed on sub-layer too, since what differs between them is render state.
        if open.is_some_and(|previous| previous != entry.layer_index) {
            arena.seal();
        }
        open = Some(entry.layer_index);
        let Some(&(tile_index, bucket_index, raster_texture)) = source.get(&entry.geometry.0)
        else {
            continue;
        };
        let Some(bucket) = buckets
            .get(tile_index)
            .and_then(|(_, tile_buckets)| tile_buckets.get(bucket_index))
        else {
            continue;
        };

        // A bucket's bytes go into the arena once, however many drawables it produces.
        //
        // Two of the seven kinds produce two: a fill's triangles and its outline, and a
        // translucent extrusion's depth pass and its colour pass. Neither pair differs in
        // anything a `GeometryAdd` carries — the record is the buffer description, and view,
        // layer, tile, pass and flags are all on `ViewUse`. What separates the drawables is
        // render state and `ubo_index`, which are per drawable already.
        //
        // Encoding per drawable meant a translucent extrusion's vertices, indices and
        // interleaved attributes were all copied twice: on a forty-two tile cover of a city
        // that is 15.8 MB of a 36.6 MB frame, and it is the largest single cost in `emit`.
        //
        // The second drawable gets its own id rather than sharing the first's, and the two
        // records name the same slab ranges. Sharing the id would save a record and cost
        // clarity: `ViewRelease` is keyed by (geometry, view), so one release would drop both
        // drawables with nothing in the stream saying so. Nothing requires two geometries'
        // ranges to be disjoint — a slab reference is an offset.
        //
        // A fill is the exception, and the oracle is what says so: its two drawables take
        // *different* shaders over different index buffers — `FillShader` on earcut's triangles
        // and `FillOutlineShader` on a line loop. So the record cannot be reused, only the
        // buffers under it. Copying the record for a fill is what made the outline draw the
        // interior a second time and `fill-outline-color` render nothing at all.
        //
        // An extrusion is the same exception and then the first case again, which is why this
        // caches a *list*. It has two records — the roof and the instanced walls — and each is
        // used by two drawables, the depth pass and the colour pass. So the part is chosen by
        // sub-layer and the record for it copied, rather than there being a "first" record and
        // a "second" one.
        // Geometry the consumer already has is not re-encoded. This test has to come before the
        // encoding, not after it: `encode` writes into the arena, so deciding late meant a
        // second view drawing a tile the first already holds packed a whole second copy of its
        // vertices, indices and attributes that no registry entry ever referenced. Dead bytes
        // in proportion to cover times views, every frame — §11.5's allocation churn, arriving
        // by the one path retention was supposed to close.
        // A symbol is the exception: its dynamic and opacity buffers are rewritten every frame
        // from a placement that is global, so a bucket held back here keeps opacity decided
        // against whatever cover existed when it was announced. That is what made the frame a
        // function of tile arrival order -- placement agreed run to run, emission did not.
        let per_frame = matches!(bucket.content, Content::Symbol(_));
        if registry.is_some() && !per_frame && !fresh_buckets.contains(&(tile_index, bucket_index))
        {
            continue;
        }

        let records = match packed_bytes.get(&(tile_index, bucket_index)) {
            Some(records) => records,
            None => {
                let Some(fresh) = encode_parts(
                    arena,
                    bucket,
                    &Encoding {
                        patterns,
                        raster_texture,
                        zoom: view.zoom,
                        stacks: &stacks,
                        prepared: &prepared,
                        key: (tile_index, bucket_index),
                    },
                ) else {
                    continue;
                };
                packed_bytes
                    .entry((tile_index, bucket_index))
                    .or_insert(fresh)
            }
        };
        // Which of the bucket's records this drawable draws. Out of range is a disagreement
        // between `drawable_count` and the encoder about how many a bucket makes, and drawing
        // the wrong part would be worse than drawing none.
        let Some(record) = records.get(part_of(&bucket.content, entry.sub_layer_index)) else {
            continue;
        };
        let mut encoded = record.clone();
        encoded.record.geometry = entry.geometry;

        // And a *drawable* the consumer already has is not announced again, even where its
        // bucket had to be encoded for a sibling's sake. `fresh` is empty without a registry, so
        // the unregistered path announces everything, which is what `GeometryId` documents.
        //
        // A symbol is the same exception it is above, and for the same reason carried one step
        // further. Its vertices are a function of the camera -- `write_line_positions` walks
        // each label along its *projected* road and `write_opacity` bakes the fade in -- so the
        // bytes this frame encoded are the only ones that describe this frame. Announcing them
        // once left the consumer drawing glyph positions computed for whatever camera the tile
        // arrived under: settled frames were exact and a moving one carried its labels off
        // their streets, which is the "flying labels" a zoom sweep shows. Re-encoding without
        // re-announcing also wrote those bytes into the arena every frame with nothing holding
        // them, so this closes that too.
        let key = keyed.get(&entry.geometry.0).copied();
        if registry.is_some() && !per_frame && key.is_some_and(|key| !fresh.contains(&key)) {
            continue;
        }

        // What this record names, for the consumer's half to compare against. Only the symbol
        // fade attribute, which is the one under investigation.
        if crate::watch::watching() {
            for desc in encoded.attributes() {
                if desc.attr_id
                    == tessella_capture_abi::generated::ubo_slots::ID_SYMBOL_FADE_OPACITY_VERTEX_ATTRIBUTE
                {
                    crate::watch::sent(
                        view_id.0,
                        encoded.record.geometry.0,
                        desc.attr_id,
                        desc.source.slab,
                        desc.source.offset,
                        desc.source.length,
                        arena.resolve(desc.source).and_then(|bytes| {
                            bytes.get(..4).map(|four| {
                                f32::from_le_bytes([four[0], four[1], four[2], four[3]])
                            })
                        }),
                    );
                }
            }
        }
        emit::write(producer, &encoded)?;
        emitted.geometries += 1;

        // The bytes are wanted until the drawable leaves, and the registry remembers where they
        // are because nothing else survives the frame that encoded them.
        if let (Some(registry), Some(key)) = (registry.as_deref_mut(), key) {
            let refs = emit::slab_refs(&encoded);
            for reference in &refs {
                arena.retain(*reference);
            }
            // Where the announcement landed, for §13.2's acknowledgement. Inside an open group
            // `head` is where the record really is rather than what has been published, which is
            // the position the consumer's own counter will eventually pass.
            registry.record_refs(key, refs, producer.head());
        }
    }

    // The last layer's slab, which the loop above never closed: it seals on a *change* of
    // layer, and there is no change after the last one. An open slab is in none of the arena's
    // sealed list, so nothing could sweep it, measure its live fraction, or resolve a reference
    // into it across a mapping — it was invisible to retention entirely.
    arena.seal();

    // Every geometry is announced before any drawable names one.
    //
    // A `ViewUse` is as durable as the geometry it names — the view, layer, sub-layer, tile,
    // pass and flags do not change while a drawable is in the cover — so with a registry it is
    // sent once and released when the drawable goes. Without one it is sent every frame, beside
    // the `GeometryAdd` it accompanies.
    for binding in bound {
        let key = DrawableKey {
            tile: binding.tile,
            layer_index: binding.layer_index,
            sub_layer_index: binding.sub_layer_index,
        };
        if registry.is_some() && !unbound.contains(&key) {
            emitted.drawables += 1;
            continue;
        }
        session
            .use_geometry(producer, binding)
            .map_err(FrameError::from)?;
        emitted.drawables += 1;
        emitted.uses += 1;
    }

    // What left the cover: released, removed, and its bytes handed back.
    if let Some(registry) = registry {
        // This view stops using them: one `ViewRelease` each, whoever else still draws them.
        for (_, geometry) in registry.released() {
            session
                .release_geometry(producer, view_id, geometry)
                .map_err(FrameError::from)?;
        }
        // And the bytes go only for those no view holds afterwards — §5.3's "removed when the
        // last view releases". A tile leaving one view's cover while another still draws it
        // keeps its geometry and loses only that view's use.
        for (_, geometry) in registry.retired() {
            // The record first, then the bytes. The arena hands a released range back to the
            // next geometry that fits it, and the consumer is holding the old one's id against
            // that same range: without the removal it reads whatever was written over it. Every
            // release for this geometry has already gone out above, so nothing is drawing it
            // when it goes.
            emit::remove(producer, geometry).map_err(FrameError::from)?;
            emitted.removed += 1;
        }

        // DR-21's compaction. A slab whose live fraction has fallen far enough is mostly holding
        // bytes nobody wants, and the way to empty it is to re-announce its survivors: they land
        // in the current slab and the old one sweeps. Displacing is what makes that happen — the
        // drawable is forgotten here and announced afresh on the next frame that draws it.
        //
        // After the retirements, so a slab this frame just emptied is swept rather than
        // compacted, and its survivors are not moved for nothing.
        for (key, geometry) in registry.displaceable(arena, COMPACTION_THRESHOLD) {
            session
                .release_geometry(producer, view_id, geometry)
                .map_err(FrameError::from)?;
            registry.displace(&key);
            emitted.displaced += 1;
        }
    }

    if camera_moved || scene_changed || declare {
        // Packed in the order the slots were handed out, which is not the order the tiles arrived
        // in.
        //
        // `ubo_index` is assigned by walking the *resolved* order -- pass, depth slot, sub-layer,
        // sort key, then tile -- while `by_layer` collects bindings as the cover is walked. For a
        // vector layer the two coincide, because the cover is walked in the order the sort puts
        // it, and nothing here noticed the difference. A raster source is looked up at its own
        // zoom by a second walk with its own traversal, and there they diverge: the drawable in
        // slot 1 was tile (35206, 21492) while the matrix in slot 1 belonged to (35207, 21491).
        //
        // So every raster tile drew a different tile's picture on a grid that was itself correct.
        // That is why the tile borders landed on the oracle's columns to the pixel and the imagery
        // between them did not match -- and why it read as a covering-zoom problem when the zoom
        // was right all along.
        //
        // Taken from the order rather than re-sorted here, so there is one definition of the slot
        // numbering instead of two that have to agree.
        let slot_of: BTreeMap<(i32, u64, i32), u32> = order
            .iter()
            .map(|entry| {
                (
                    (
                        i32::try_from(entry.layer_index).unwrap_or(-1),
                        entry.geometry.0,
                        entry.sub_layer_index,
                    ),
                    entry.ubo_index,
                )
            })
            .collect();
        for (layer_index, bindings) in &by_layer {
            let mut ordered = bindings.clone();
            ordered.sort_by_key(|binding| {
                slot_of
                    .get(&(
                        binding.layer_index,
                        binding.geometry.0,
                        binding.sub_layer_index,
                    ))
                    .copied()
                    .unwrap_or(u32::MAX)
            });
            write_layer_state(producer, frame, *layer_index, &ordered, tiles)?;
        }
    }

    // The order, then the camera naming its epoch — never the other way round.
    let order = draw_order.emit(producer, view_id)?;
    emitted.epoch = order.epoch;

    // A camera that has not moved is not sent. The order is gated by `DrawOrder` itself and
    // always was; this is the other half, and together they are what makes a parked view
    // silent — §10's exit criterion, and DR-8's rule about camera-rate traffic.
    //
    // Except when the order changed: the camera names an epoch, and a consumer holding a camera
    // that names an order it no longer has cannot draw. So a new order forces a camera whatever
    // the camera did.
    //
    // And except when this frame wrote anything at all, which is the rule the other two are
    // special cases of. The reader says it plainly: "A frame opens at its first record and closes
    // at its camera, which is the commit point ... nothing is emitted after it." A frame that
    // writes a record and returns without a camera never closes -- the consumer has already
    // cleared its scene at `beginFrame` and its `endFrame` never runs, so it draws nothing at all
    // and the map goes black. Silence is only silent if it is total.
    //
    // Reachable whenever a frame is emitted for a reason the camera key cannot see: a fade in
    // flight is the one that found this, since symbol vertices carry the opacities and have to be
    // re-sent while nothing about the camera or the cover has moved.
    if !camera_moved && !order.changed && producer.head() == opened_at {
        return Ok(emitted);
    }

    CameraBlock::new(view, light, order.epoch, 0, draw_order.opaque_cutoff())
        .map_err(|error| FrameError::Camera(alloc::format!("{error}")))?
        .for_view(view_id)
        .write(producer)?;

    Ok(emitted)
}

/// Every font stack the frame's symbol layers shape with, once each.
///
/// A stack rather than a font: `text-font` is a list, and the glyphs a label draws come from the
/// first entry that has each codepoint. The atlas is keyed by the whole stack for that reason,
/// so asking for one font's atlas would miss every label that fell through to the second.
fn symbol_stacks(buckets: &[(TileId, Vec<LayerBucket>)]) -> Vec<Vec<alloc::string::String>> {
    let mut stacks: Vec<Vec<alloc::string::String>> = Vec::new();
    for (_, tile_buckets) in buckets {
        for bucket in tile_buckets {
            let Content::Symbol(layout) = &bucket.content else {
                continue;
            };
            for stack in layout.stacks() {
                if !stacks.contains(&stack) {
                    stacks.push(stack);
                }
            }
        }
    }
    stacks
}

/// Tears a view down: releases what it held, removes what nothing holds, and undeclares it.
///
/// # What teardown has to do that a frame does not
///
/// R4 calls for a teardown protocol, and until the lifecycle existed there was nothing to
/// protocol: nothing was ever retained, so nothing had to be let go. Now a view holds uses and
/// geometry holds bytes, and dropping a view without saying so leaves a consumer with buffers
/// nothing will mention again and a declared view nothing will draw.
///
/// So this is the eviction path with an empty cover, plus the undeclaration: every use released,
/// every geometry no *other* view holds removed, its bytes handed back, and the view forgotten.
/// A view sharing geometry with another leaves that geometry alone, which is §5.3's rule and
/// exactly the rule an ordinary frame follows.
///
/// # Whole or not at all
///
/// Grouped like a frame, and for the same reason: a teardown that failed halfway would leave a
/// consumer holding some releases and not others, with no record saying which. On failure the
/// session is untouched and the caller may try again.
///
/// # Errors
///
/// [`FrameError::Full`] when the ring cannot take the records, and [`FrameError::View`] when the
/// view was never declared — tearing down a view that does not exist is a caller fault, not a
/// silent no-op.
pub fn teardown_view(
    producer: &mut Producer,
    arena: &mut SlabArena,
    session: &mut Session,
    view_id: ViewId,
) -> Result<Emitted, FrameError> {
    producer.begin();
    match teardown_group(producer, session, view_id) {
        Ok(emitted) => {
            for reference in session.registry().retire() {
                arena.release(reference);
            }
            arena.sweep();
            session.forget(view_id);
            producer.commit();
            Ok(emitted)
        }
        Err(error) => {
            producer.abort();
            session.registry().rollback();
            Err(error)
        }
    }
}

fn teardown_group(
    producer: &mut Producer,
    session: &mut Session,
    view_id: ViewId,
) -> Result<Emitted, FrameError> {
    // A view this session never declared cannot be torn down: there is nothing to release and
    // nothing to undeclare, and saying so is more useful than a silent no-op.
    if session.needs_declaring(view_id) {
        return Err(FrameError::View(alloc::format!(
            "view {} was never declared",
            view_id.0
        )));
    }

    let mut view = ViewSession::new();
    // Legitimate as far as this session is concerned; the consumer was told when the view first
    // drew, or it would be holding nothing to release.
    view.declare_if(producer, view_id, CameraMode::Producer, false)
        .map_err(FrameError::from)?;

    // An empty frame for this view: everything it held is now unseen, so `released` is its whole
    // set and `retired` is whatever no other view keeps.
    session.registry().begin_frame(view_id);

    let mut emitted = Emitted::default();
    for (_, geometry) in session.registry().released() {
        view.release_geometry(producer, view_id, geometry)
            .map_err(FrameError::from)?;
    }
    for (_, geometry) in session.registry().retired() {
        emit::remove(producer, geometry).map_err(FrameError::from)?;
        emitted.removed += 1;
    }

    view.undeclare(producer, view_id)
        .map_err(FrameError::from)?;
    Ok(emitted)
}

/// How empty a slab has to be before its survivors are moved out of it.
///
/// A quarter: below that, three of every four bytes the slab occupies are holding nothing, and
/// re-announcing what is left costs one upload of a small fraction of it. Above it, moving is
/// the more expensive of the two.
///
/// Not tuned against a workload — DR-21 records that the trade wants measuring, and a threshold
/// that proves hard to pick is evidence for the whole-layer re-emit the record weighed against.
const COMPACTION_THRESHOLD: f64 = 0.25;

/// The camera fields damage is decided on.
/// The camera reduced to what a change in it would mean.
///
/// Public because the frame loop asks the same question before deciding whether to emit at all,
/// and asking it a second way would let the two disagree about what "moved" means.
#[must_use]
pub fn camera_key_of(view: &ViewTransform) -> crate::damage::CameraKey {
    camera_key(view)
}

fn camera_key(view: &ViewTransform) -> crate::damage::CameraKey {
    crate::damage::CameraKey {
        center_zoom0: tessella_tile::camera::center_zoom0(view),
        zoom: view.zoom,
        bearing: view.bearing,
        pitch: view.pitch,
        pixels_per_meter: tessella_tile::camera::pixels_per_meter(view),
        viewport: [view.width, view.height],
    }
}

/// What every bucket's encoding reads from the frame around it.
///
/// Grouped rather than passed one by one: they are all "what this frame has fetched and where
/// its camera is", and a bucket picks the ones its kind needs.
#[derive(Clone, Copy)]
struct Encoding<'a> {
    /// Sprites, for a layer with a pattern.
    patterns: Option<&'a Patterns<'a>>,
    /// The texture this tile's raster picture went to.
    raster_texture: tessella_capture_abi::envelope::TextureId,
    /// The camera's zoom, which a pattern's fade is chosen at.
    zoom: f64,
    /// The frame's font stacks, in the order their atlases were published.
    ///
    /// A symbol drawable names the atlas holding *its* glyphs, and the only thing that ties the
    /// two together is this order. Passed rather than recomputed so the upload and the reference
    /// cannot drift apart.
    stacks: &'a [alloc::vec::Vec<alloc::string::String>],
    /// Every symbol bucket of the frame, shaped and placed. See [`place_symbols`].
    prepared: &'a BTreeMap<(usize, usize), PreparedSymbols>,
    /// Which bucket this is, to address `prepared` with.
    key: (usize, usize),
}

/// How long a fade takes, in milliseconds.
///
/// mbgl's `util::DEFAULT_TRANSITION_DURATION`.
pub const FADE_DURATION_MILLIS: f64 = 300.0;

/// What a label keeps between frames: its identity, and the fade keyed by it.
///
/// # Why this outlives the frame
///
/// Because a fade is a function of time and the thing fading has to be recognisable from one
/// frame to the next. Both halves of that were missing. `ViewSymbols` was constructed inside
/// `emit_group`, so every frame started with no fades at all; and the identity a fade is keyed by
/// was `base + index`, an ordinal into whatever order this frame happened to walk its buckets in.
/// Neither survived a frame, so the fades were decorative: a label's opacity was decided against
/// a history one tick long.
///
/// The ordinal is worse than merely unstable. At a zoom crossing a tile is replaced by four
/// children, and the label that was "Detroit" in the parent is a different instance at a
/// different index in the child — so the ordinal that meant "Detroit" last frame means whatever
/// sorts into that slot now, and a label inherits a stranger's fade. Nothing about the two
/// labels is related except their position in a list.
///
/// [`CrossTileIndex`] is what makes the identity real: it matches by text and by rounded world
/// position, so the same label keeps its number across the crossing. One index per layer, because
/// mbgl keeps one per layer and identities are only ever compared within one.
///
/// [`CrossTileIndex`]: tessella_place::cross_tile::CrossTileIndex
#[derive(Debug, Default)]
pub struct PlacementState {
    symbols: crate::symbols::ViewSymbols,
    /// One index per layer. Identities are compared within a layer and never across.
    indexes: BTreeMap<u32, tessella_place::cross_tile::CrossTileIndex>,
    /// What each indexed bucket was given, so an unchanged one is not re-indexed.
    ///
    /// `CrossTileIndex::add_bucket` returns early for a bucket it has already seen and leaves the
    /// symbols it was handed untouched, because mbgl keeps the identities on the bucket itself
    /// and has nothing to fill in. Here the laid-out symbols are shared and immutable, so the
    /// assignment is remembered here instead.
    indexed: BTreeMap<(u32, DataTileId), Indexed>,
    /// The bucket number the index tells parses apart by.
    next_bucket: u32,
    /// How far a fade moves on the next frame.
    ///
    /// One is *instant*, and it is the right answer for a still picture: mbgl's
    /// `symbolFadeChange` returns exactly that in static map mode, which is the mode
    /// `mbgl-render` runs, so every parity render on both sides has been comparing maps with no
    /// crossfade at all. It is the wrong answer for a map somebody is looking at, where a label
    /// that stops being placed has to fade rather than vanish -- and a label that vanishes at one
    /// anchor while another appears further along the same road is read as the text having
    /// *moved*. See [`Self::advance`].
    increment: f32,
}

/// One bucket's assignment, and what it was assigned for.
#[derive(Debug)]
struct Indexed {
    /// The bucket list this came from, *held*.
    ///
    /// Held rather than compared by address alone: an `Arc` that dies frees its address for the
    /// next allocation, and a memo keyed on a recycled address would hand a new tile the previous
    /// occupant's identities. Keeping the reference keeps the address unique for as long as the
    /// memo can be consulted.
    origin: Option<alloc::sync::Arc<Vec<LayerBucket>>>,
    /// The number this bucket was indexed under, for `remove_stale_buckets`.
    bucket: u32,
    /// One identity per laid-out symbol, in the order they were laid out.
    ids: Vec<u32>,
}

impl PlacementState {
    /// State with nothing placed and nothing named.
    #[must_use]
    pub fn new() -> Self {
        Self {
            increment: 1.0,
            ..Self::default()
        }
    }

    /// How far the fades move on the next frame, from the time that has passed.
    ///
    /// mbgl's `symbolFadeChange`: elapsed over the transition duration, which is
    /// `DEFAULT_TRANSITION_DURATION` -- 300 ms -- in continuous mode and zero in static mode,
    /// where the whole expression short-circuits to one. Both are reachable here, and the default
    /// is the static one so that nothing measuring against `mbgl-render` changes.
    pub fn advance(&mut self, elapsed_millis: f64) {
        #[allow(clippy::cast_possible_truncation)]
        let step = tessella_place::fade::increment(
            (elapsed_millis / 1000.0) as f32,
            (FADE_DURATION_MILLIS / 1000.0) as f32,
        );
        // Clamped here rather than there: `fade::increment` is the ratio and says nothing about
        // what a frame may do with it, while a fade that moved by more than its whole range, or
        // backwards, is a clock that went wrong rather than an instruction.
        self.increment = if step.is_finite() {
            step.clamp(0.0, 1.0)
        } else {
            1.0
        };
    }

    /// How far a fade moves on the next frame, which a test reads.
    #[must_use]
    pub const fn increment(&self) -> f32 {
        self.increment
    }

    /// How many labels are part way through a fade.
    ///
    /// A frame that is not emitted is a fade that does not move, so this is what makes a fade a
    /// reason to draw on a map where nothing else is happening — the camera stopped mid-fade and
    /// the labels still have somewhere to get to.
    #[must_use]
    pub fn fading(&self) -> usize {
        self.symbols.fading()
    }

    /// Fades that finish in one step, which is what a still picture wants.
    pub const fn settle_at_once(&mut self) {
        self.increment = 1.0;
    }

    /// Forgets every identity and every fade.
    ///
    /// For a change that re-lays out the labels themselves — new fonts, a new sprite sheet. The
    /// symbols on the other side of it are not the ones this named, and a fade carried across
    /// would be a fade of something else. Cheaper than being wrong: the cost is one frame of
    /// labels fading in.
    pub fn invalidate(&mut self) {
        self.indexes.clear();
        self.indexed.clear();
    }

    /// The identity of every label currently named, by layer and tile.
    ///
    /// What a test reads, and it is the numbers rather than a count of them: a counter is reset
    /// by anything that drops an index, so "how many were issued" stays still for a build that
    /// re-names every label every frame. The numbers do not.
    #[must_use]
    pub fn identities(&self) -> BTreeMap<(u32, DataTileId), Vec<u32>> {
        self.indexed
            .iter()
            .map(|(key, held)| (*key, held.ids.clone()))
            .collect()
    }
}

/// One symbol bucket, shaped and placed, waiting to be encoded.
///
/// Placement is a decision about the *frame* -- a road name and a shop name compete for the same
/// screen whatever layer or tile each came from -- so it cannot happen inside a walk that visits
/// one bucket at a time and encodes as it goes. It happens before that walk, and this is what it
/// leaves behind.
struct PreparedSymbols {
    /// The glyphs, with their opacities and their along-line positions already written.
    buffers: SymbolBuffers,
    /// The sprites, when the layer resolved any.
    icons: Option<SymbolBuffers>,
}

/// How a layer competes for space, as its style states it.
///
/// `text-allow-overlap` and the five flags beside it are *layout* properties, and none of them
/// reached placement: `FrameOptions` was built with `Rules::default()` and `Padding::uniform(2)`
/// whatever the style said, so a layer asking to overlap competed anyway and a layer asking for
/// wider padding got the default. mbgl reads all eight per layer.
///
/// `text-padding` is a number of screen pixels and is used as written. mbgl multiplies it by
/// `tilePixelRatio` because its collision boxes are in tile units; these are in screen pixels
/// already.
fn placement_rules(
    layer: &tessella_style::Layer,
    zoom: f64,
) -> (
    tessella_place::placement::Rules,
    tessella_place::feature::Padding,
    tessella_place::feature::Padding,
) {
    let flag = |key: &str| {
        tessella_style::property::layout_value(layer, key, zoom, None)
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    };
    #[allow(clippy::cast_possible_truncation)]
    let pad = |key: &str, fallback: f32| {
        tessella_style::property::layout_value(layer, key, zoom, None)
            .as_ref()
            .and_then(tessella_style::value::Value::as_number)
            .map_or(fallback, |value| value as f32)
    };
    (
        tessella_place::placement::Rules {
            text_allow_overlap: flag("text-allow-overlap"),
            icon_allow_overlap: flag("icon-allow-overlap"),
            text_optional: flag("text-optional"),
            icon_optional: flag("icon-optional"),
            text_ignore_placement: flag("text-ignore-placement"),
            icon_ignore_placement: flag("icon-ignore-placement"),
        },
        tessella_place::feature::Padding::uniform(pad("text-padding", 2.0)),
        tessella_place::feature::Padding::uniform(pad("icon-padding", 2.0)),
    )
}

/// Shapes and places every symbol bucket of a frame, competing them in one grid.
///
/// # Why it is a pass of its own, and why it runs backwards
///
/// mbgl places a frame in one collision grid, in *reverse* render order: the topmost label claims
/// its space first and the layers beneath take what is left. That is what makes a place name beat
/// a house number rather than the other way round.
///
/// Placement used to happen inside the encode walk, a grid per bucket, so a layer could only
/// compete against itself -- every label of every other layer and tile was invisible to it, and
/// the frame drew far more than it should. Sharing one grid *inside* that walk was tried and is
/// worse than not sharing at all: the walk runs in painter order, bottom first, so the lowest
/// label layer -- 2,256 house numbers at z16 -- filled the grid before a single place name was
/// offered. The order is the whole point, and painter order is the wrong one, so the pass has to
/// be separate.
///
/// # Why it also writes
///
/// A fade takes its direction from the previous frame's decision, so the opacities cannot be
/// written until every bucket has been offered and the fades have settled. That is one more
/// reason this cannot be folded back into the encode walk: the first bucket's opacity depends on
/// the last bucket's placement.
#[allow(clippy::too_many_arguments)]
fn place_symbols(
    order: &[tessella_capture_abi::envelope::OrderEntry],
    source: &BTreeMap<u64, (usize, usize, tessella_capture_abi::envelope::TextureId)>,
    buckets: &[(TileId, Vec<LayerBucket>)],
    origins: &[Option<alloc::sync::Arc<Vec<LayerBucket>>>],
    layouts: &mut SymbolCache,
    tiles: &[TileCoord],
    fonts: Option<&Fonts>,
    patterns: Option<&Patterns<'_>>,
    style: &tessella_style::Style,
    view: &ViewTransform,
    placement: &core::cell::RefCell<&mut PlacementState>,
) -> BTreeMap<(usize, usize), PreparedSymbols> {
    let empty;
    let fonts = match fonts {
        Some(fonts) => fonts,
        None => {
            empty = Fonts::new("");
            &empty
        }
    };
    #[allow(clippy::cast_possible_truncation)]
    let viewport = (view.width as f32, view.height as f32);
    let increment = placement.borrow().increment;

    // The frame's grid, and the whole reason this function exists.
    // The viewport with mbgl's margin around it, and its cell size. `project_with` offsets every
    // point into the margin, so the two have to agree.
    let grid_padding = crate::symbols::viewport_padding(view.pitch);
    let mut grid: tessella_place::grid::GridIndex<u32> = tessella_place::grid::GridIndex::new(
        viewport.0.max(1.0) + 2.0 * grid_padding,
        viewport.1.max(1.0) + 2.0 * grid_padding,
        25,
    );

    // The camera's distance to the centre of the screen, which the perspective ratio divides by.
    let camera_to_center = tessella_tile::camera::camera_to_center_distance(view.height);

    // A bucket appears once per drawable it produces; it is shaped once.
    let mut seen: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut keys: Vec<(usize, usize)> = Vec::new();
    let mut prepared: BTreeMap<(usize, usize), PreparedSymbols> = BTreeMap::new();
    let mut shaped: BTreeMap<(usize, usize), Shaped> = BTreeMap::new();

    // `order` is already top layer first, which is the order mbgl places in.
    //
    // `Placement::placeLayers` walks its layers `crbegin` to `crend` over a list in render order,
    // so the topmost symbol layer is offered space first. Here the equivalent is free: `sort_key`
    // orders by `depth_slot`, which runs opposite the style index, so walking `order` forwards
    // already descends the style. Reversing it to "match mbgl" inverts a match that was already
    // there -- Washington doubles its differing pixels and the all-families scene multiplies them
    // by twenty-six.
    // Each layer's tiles by row, which is the order a symbol layer places in.
    //
    // `RenderSymbolLayer::prepare` takes `getRenderTilesSortedByYPosition()`, and no other layer
    // does -- everything else keeps the plain render tiles, which come out of a map keyed by
    // `OverscaledTileID` and so run x-major. `sort_key` already matches that, which is why the
    // draw order is left alone here and only the placement walk is resorted.
    //
    // The comparator is `tie(b.z, par.y, par.x) < tie(a.z, pbr.y, pbr.x)`, where `par` is *a*'s
    // rotated position and `pbr` is *b*'s. Mixing the two sides like that looks like a slip, but
    // with one zoom on screen the `z` terms are equal and it reduces to ascending `(y, x)`: row
    // by row, left to right. At a bearing the rotation turns it into "by distance down the
    // screen", which is the point of the sort.
    //
    // It decides contention. Two tiles' labels compete for the same strip of screen along their
    // shared edge, and whichever tile is offered first keeps it.
    let mut walk: Vec<(&tessella_capture_abi::envelope::OrderEntry, u8, u32, u32)> = Vec::new();
    for entry in order {
        let key = source
            .get(&entry.geometry.0)
            .and_then(|&(tile_index, _, _)| tiles.get(tile_index))
            .map_or((0, 0, 0), |coord| (coord.z, coord.y, coord.x));
        walk.push((entry, key.0, key.1, key.2));
    }
    // Stable, and within one layer's contiguous run only: `sort_key` has already put the layers
    // in the order they place in, and that must not move.
    // Which buckets this frame actually offered, per layer, so the index can drop the rest. A
    // tile that has left the map must give its identities back: mbgl's `removeStaleBuckets`, and
    // without it a parent lends a label once and never gets it back, so the child that replaces
    // the child that replaced it is given a fresh number and starts its fade again.
    let mut live: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    let mut seen_keys: BTreeSet<(u32, DataTileId)> = BTreeSet::new();

    let mut at = 0;
    while at < walk.len() {
        let layer = walk[at].0.layer_index;
        let end = walk[at..]
            .iter()
            .position(|(entry, ..)| entry.layer_index != layer)
            .map_or(walk.len(), |offset| at + offset);
        walk[at..end].sort_by_key(|&(_, z, y, x)| (z, y, x));
        at = end;
    }

    for (entry, ..) in walk {
        let Some(&(tile_index, bucket_index, _)) = source.get(&entry.geometry.0) else {
            continue;
        };
        if !seen.insert((tile_index, bucket_index)) {
            continue;
        }
        let Some((tile, bucket)) = buckets
            .get(tile_index)
            .and_then(|(id, list)| list.get(bucket_index).map(|bucket| (*id, bucket)))
        else {
            continue;
        };
        let Content::Symbol(layout) = &bucket.content else {
            continue;
        };
        let wrap = tiles.get(tile_index).map_or(0, |coord| coord.wrap);
        let Ok(to_clip) = tessella_tile::camera::tile_to_clip(view, tile.z, tile.x, tile.y, wrap)
        else {
            continue;
        };

        // Laid out once per bucket and held, not once per frame: none of shaping, bidi, glyph
        // resolution or quad building depends on the camera. See `SymbolCache`.
        let bucket_laid = layouts.get_or_lay_out(
            origins.get(tile_index).and_then(Option::as_ref),
            bucket_index,
            || {
                let (buffers, laid) = layout.lay_out(fonts, patterns.map(|p| p.positions));
                // Shaped with the label, not after it, so an icon competes for space the way its
                // label does. While it came later, `FrameLabel::icon` was always `None`, no icon
                // was ever offered to the grid, and every anchor along a road kept its shield.
                let icons =
                    patterns.map(|patterns| layout.lay_out_icons(patterns.positions, &laid));
                Laid {
                    buffers,
                    laid,
                    icons,
                }
            },
        );
        // The frame writes line positions and opacities into the vertices, so it works on its
        // own copy; `laid` is read only and is shared.
        let mut buffers = bucket_laid.buffers.clone();
        let laid = &bucket_laid.laid;
        if buffers.vertices.is_empty() && !layout.has_icons() {
            continue;
        }
        let icons = bucket_laid.icons.clone();

        let plane = tessella_tile::camera::label_plane_matrix(&to_clip, view.width, view.height);
        let mut held = placement.borrow_mut();

        // The identity each label carries into placement and out to the fades.
        //
        // Matched by text and by rounded world position against every tile the index already
        // holds, so the label that was "Detroit" in a parent tile is the same number in the child
        // that replaces it. What this replaces was `base + index`, an ordinal into this frame's
        // walk, under which a label inherited the fade of whatever sorted into its slot.
        //
        // The `overscaled_z` is the coordinate the tile is *drawn* at and `z` its own: above a
        // source's maxzoom one tile answers several cover coordinates, and the index has to tell
        // a parent standing in for a child from the child itself.
        // `overscaled_z` is `z`: `placed` carries the tile that is *drawn*, so a parent standing
        // in for a missing child arrives here at its own coordinate rather than the child's, and
        // there is no second zoom to record.
        #[allow(clippy::cast_possible_truncation)]
        let data_tile = DataTileId {
            overscaled_z: tile.z,
            wrap: wrap.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            z: tile.z,
            x: tile.x,
            y: tile.y,
        };
        let origin = origins.get(tile_index).and_then(Option::as_ref);
        let indexed_key = (entry.layer_index, data_tile);
        // An unchanged bucket keeps what it was given. Re-indexing one would be harmless for the
        // ids and not for the claims: `add_bucket` releases and re-takes them, and a parent that
        // has already lent a label to one child must not lend it again to another.
        let reusable = held.indexed.get(&indexed_key).is_some_and(|held| {
            held.ids.len() == laid.len()
                && match (&held.origin, origin) {
                    (Some(was), Some(now)) => alloc::sync::Arc::ptr_eq(was, now),
                    (None, None) => true,
                    _ => false,
                }
        });
        let ids: Vec<u32> = if reusable {
            held.indexed[&indexed_key].ids.clone()
        } else {
            let bucket_id = held.next_bucket.wrapping_add(1);
            held.next_bucket = bucket_id;
            let mut symbols: Vec<tessella_place::cross_tile::Symbol> = laid
                .iter()
                .map(|instance| {
                    // The text is the key, and it lives on the *pending* symbol rather than on
                    // the instance: a line label is one pending symbol and one instance per
                    // anchor, so several instances of one road name share a key and are told
                    // apart by position, which is exactly what the index compares.
                    let key = layout
                        .pending
                        .get(instance.pending)
                        .map_or("", |pending| pending.text.as_str());
                    tessella_place::cross_tile::Symbol::new(key, instance.anchor)
                })
                .collect();
            held.indexes
                .entry(entry.layer_index)
                .or_default()
                .add_bucket(data_tile, bucket_id, &mut symbols);
            let ids: Vec<u32> = symbols.iter().map(|symbol| symbol.cross_tile_id).collect();
            held.indexed.insert(
                indexed_key,
                Indexed {
                    origin: origin.cloned(),
                    bucket: bucket_id,
                    ids: ids.clone(),
                },
            );
            ids
        };
        live.entry(entry.layer_index)
            .or_default()
            .insert(held.indexed[&indexed_key].bucket);
        seen_keys.insert(indexed_key);
        // Per layer, because everything in it is: the scale a shaped extent competes at is the
        // layer's `text-size`, and how it competes is the layer's own six flags.
        let (rules, padding, icon_padding) = usize::try_from(entry.layer_index)
            .ok()
            .and_then(|index| style.layers.get(index))
            .map_or_else(
                || {
                    let default = crate::symbols::FrameOptions::default();
                    (default.rules, default.padding, default.icon_padding)
                },
                |layer| placement_rules(layer, view.zoom),
            );
        let options = crate::symbols::FrameOptions {
            viewport,
            font_scale: layout.symbol.size / tessella_glyph::text::ONE_EM,
            rules,
            padding,
            icon_padding,
            ..crate::symbols::FrameOptions::default()
        };
        let labels = frame_labels(
            laid,
            &buffers,
            icons.as_ref(),
            &ids,
            perspective_with(&plane, camera_to_center),
        );
        // Where each glyph lands along its road, *before* the label is offered any space.
        //
        // A label whose road runs out before its name does is not drawn, and a label that is not
        // drawn must not hold space against the ones that are. Deciding this after placement --
        // which is what writing the positions later amounted to -- left every one of them
        // reserving a run of collision circles along a road it was never going to be printed on,
        // and with one grid for the frame that is space taken from a label that would have fit.
        // mbgl decides the two together for the same reason.
        let mut without_room: Vec<u32> = Vec::new();
        let units = tessella_tile::camera::pixels_to_tile_units(tile.z, view.zoom);
        if units.abs() > f64::EPSILON {
            #[allow(clippy::cast_possible_truncation)]
            let scale = (1.0 / units) as f32;
            without_room = held.symbols.write_line_positions(
                &labels,
                |point| (point.0 * scale, point.1 * scale),
                layout.symbol.size,
                &mut buffers,
            );
        }
        let offered: Vec<crate::symbols::FrameLabel<'_>> = labels
            .iter()
            .filter(|label| !without_room.contains(&label.cross_tile_id))
            .cloned()
            .collect();
        held.symbols.frame_in(
            &offered,
            project_with(&plane, grid_padding),
            &options,
            &mut grid,
        );
        drop(held);

        keys.push((tile_index, bucket_index));
        shaped.insert(
            (tile_index, bucket_index),
            Shaped {
                buffers,
                shaped: alloc::sync::Arc::clone(&bucket_laid),
                icons,
                ids,
                without_room,
            },
        );
    }

    // Every bucket has been offered, so the fades can reach their resting values and the
    // opacities they decide can be written.
    let mut held = placement.borrow_mut();

    // And the index gives back what is no longer on the map. A tile whose bucket this frame did
    // not offer has left, so the identities it was holding are released and its memo goes with
    // them -- otherwise a parent that lent a label to a child keeps it lent for the life of the
    // map, and every later child of that ground is a new label with a fade starting from nothing.
    for (layer, index) in &mut held.indexes {
        let current = live.get(layer).cloned().unwrap_or_default();
        index.remove_stale_buckets(&current);
    }
    held.indexed.retain(|key, _| seen_keys.contains(key));

    held.symbols.settle(increment);

    for key in keys {
        let Some(entry) = shaped.remove(&key) else {
            continue;
        };
        let Shaped {
            mut buffers,
            shaped,
            icons,
            ids,
            without_room,
        } = entry;
        let laid = &shaped.laid;
        let Some((tile, bucket)) = buckets
            .get(key.0)
            .and_then(|(id, list)| list.get(key.1).map(|bucket| (*id, bucket)))
        else {
            continue;
        };
        let Content::Symbol(layout) = &bucket.content else {
            continue;
        };
        let labels = frame_labels(laid, &buffers, icons.as_ref(), &ids, |_| 1.0);
        // A label placement never offered has no fade entry, which reads as hidden -- so the
        // ones whose road ran out stay hidden without being special-cased here.
        held.symbols.write_opacity(&labels, &mut buffers);

        // And hide again the ones whose road ran out, because the line above just un-hid them.
        //
        // `write_line_positions` hides a label it could not walk: what `lay_out` left in the
        // dynamic buffer is the anchor in *tile* units and the shader reads that buffer as
        // label-plane coordinates, so a label drawn without a walk lands thousands of pixels from
        // where it belongs. `write_opacity` then writes every label's fade over the whole opacity
        // buffer, hidden ones included.
        //
        // That was harmless while the fades were rebuilt every frame: a label never offered to
        // placement had no fade entry, which reads as hidden, so the overwrite wrote the same
        // zero. Once the fades persist across frames it is not. A label that was placed last
        // frame and whose road runs out this one *has* an entry -- it is fading out -- so the
        // overwrite gives it an opacity, and it is drawn at a position nothing wrote this frame.
        // That is the label that flies across the map.
        //
        // The icon half below has always done this. The text half is what was missing.
        for label in &labels {
            if !without_room.contains(&label.cross_tile_id) {
                continue;
            }
            let range = label.laid_out.vertices.clone();
            if range.end > buffers.opacity.len() {
                continue;
            }
            let hidden = tessella_layout::symbol_bucket::opacity_vertex(false, 0.0);
            for slot in &mut buffers.opacity[range] {
                *slot = hidden;
            }
        }

        // After every write that can touch an opacity, so what is recorded is what the shader
        // will read. See `crate::watch`.
        for label in &labels {
            let text = layout
                .pending
                .get(label.laid_out.pending)
                .map_or("", |pending| pending.text.as_str());
            if !crate::watch::follows(text) {
                continue;
            }
            crate::watch::note(
                text,
                &crate::watch::Sighting {
                    cross_tile_id: label.cross_tile_id,
                    anchor: label.laid_out.anchor,
                    tile: (tile.z, tile.x, tile.y),
                    has_room: !without_room.contains(&label.cross_tile_id),
                    fade: held
                        .symbols
                        .opacity(label.cross_tile_id)
                        .map(|joint| joint.text.opacity),
                    vertex: buffers.opacity.get(label.laid_out.vertices.start).copied(),
                },
            );
        }

        // The icon half, which is its own drawable rather than an option: a symbol is a label, an
        // icon, or both, and the two go through different shaders -- an SDF for glyphs, a plain
        // sampler for a sprite -- so they cannot share a vertex buffer.
        //
        // The placement is the text's. The two halves are decided together -- that is what
        // `text-optional` and `icon-optional` are about -- so an icon takes the opacity its own
        // label was given, addressed through the icon's vertex ranges.
        let icons = icons.and_then(|(shaped, placed)| {
            let mut shaped = shaped;
            if shaped.vertices.is_empty() {
                return None;
            }
            let paired: Vec<crate::symbols::FrameLabel<'_>> = placed
                .iter()
                .filter_map(|icon| {
                    labels
                        .iter()
                        .find(|label| label.laid_out.pending == icon.pending)
                        .map(|label| crate::symbols::FrameLabel {
                            cross_tile_id: label.cross_tile_id,
                            laid_out: icon.clone(),
                            icon: None,
                            line: &[],
                            // Opacity only; this pairing never reaches placement.
                            perspective: 1.0,
                            // Opacity only; this pairing never reaches placement.
                            glyph_reach: None,
                        })
                })
                .collect();
            held.symbols.write_icon_opacity(&paired, &mut shaped);
            // And hide the ones whose text could not be placed. A shield is drawn for its
            // number; without the number it is an empty box, and strung along a road at every
            // anchor it is worse than nothing there.
            for icon in &paired {
                if !without_room.contains(&icon.cross_tile_id) {
                    continue;
                }
                let range = icon.laid_out.vertices.clone();
                if range.end > shaped.opacity.len() {
                    continue;
                }
                let hidden = tessella_layout::symbol_bucket::opacity_vertex(false, 0.0);
                for slot in &mut shaped.opacity[range] {
                    *slot = hidden;
                }
            }
            Some(shaped)
        });

        prepared.insert(key, PreparedSymbols { buffers, icons });
    }
    prepared
}

/// One bucket between being shaped and being written.
struct Shaped {
    buffers: SymbolBuffers,
    shaped: alloc::sync::Arc<Laid>,
    icons: Option<(SymbolBuffers, Vec<tessella_layout::symbol_bucket::LaidOut>)>,
    /// The identity of each laid-out symbol, from the layer's cross-tile index.
    ids: Vec<u32>,
    /// The labels whose road ran out before their name did, decided before placement.
    without_room: Vec<u32>,
}

/// What `SymbolLayout::lay_out` produced for one bucket.
///
/// `buffers` and `icons` are the frame's to write into -- line positions and opacities are
/// camera-dependent and go into the vertices -- so a frame takes a copy of each. `laid` is read
/// only and is shared.
#[derive(Debug)]
pub struct Laid {
    buffers: SymbolBuffers,
    laid: Vec<tessella_layout::symbol_bucket::LaidOut>,
    icons: Option<(SymbolBuffers, Vec<tessella_layout::symbol_bucket::LaidOut>)>,
}

/// Symbol layout, kept between the frames that draw it.
///
/// `SymbolLayout::lay_out` shapes text, runs bidi, resolves every glyph and builds its quads.
/// None of that depends on the camera -- mbgl does it once, when the tile is parsed -- and it
/// was being redone for every symbol bucket of every frame, which on a moving quad was the
/// largest single item in the producer.
///
/// Keyed by the identity of the tile's bucket list rather than by its coordinate. A tile
/// re-parsed at the same coordinate is a new `Arc`, so it misses rather than resolving to the
/// layout of the geometry it replaced, and the `Arc` is held here so its address cannot be
/// reused by something else while the entry stands.
///
/// The other two inputs are the fonts and the sprite sheet, which are replaced wholesale rather
/// than mutated; `epoch` is bumped when either is, and every entry from before it misses.
#[derive(Debug, Default)]
pub struct SymbolCache {
    entries: BTreeMap<(usize, usize), CacheEntry>,
    epoch: u64,
    frame: u64,
}

#[derive(Debug)]
struct CacheEntry {
    epoch: u64,
    /// Last frame this was asked for, so an entry for a tile that has left the cover goes.
    used: u64,
    /// Held for its address, which is the key.
    origin: alloc::sync::Arc<Vec<LayerBucket>>,
    laid: alloc::sync::Arc<Laid>,
}

impl SymbolCache {
    /// Invalidates everything, for a change to the fonts or the sprite sheet.
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Starts a frame, and drops what the last few did not use.
    ///
    /// Two frames of grace rather than one: a bucket that is not drawn this frame because its
    /// tile is momentarily covered by an ancestor should not have to be laid out again when the
    /// substitution swaps back.
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        let frame = self.frame;
        self.entries
            .retain(|_, entry| frame.saturating_sub(entry.used) <= 2);
    }

    /// How many buckets are held, which a test reads.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// This bucket's layout, computing it if this is the first frame to want it.
    fn get_or_lay_out(
        &mut self,
        origin: Option<&alloc::sync::Arc<Vec<LayerBucket>>>,
        bucket_index: usize,
        build: impl FnOnce() -> Laid,
    ) -> alloc::sync::Arc<Laid> {
        // A bucket list built on the spot rather than taken from the store has no stable
        // identity, so it is laid out every frame. Only the sourceless background reaches that, and
        // it carries no symbols.
        let Some(origin) = origin else {
            return alloc::sync::Arc::new(build());
        };
        let key = (alloc::sync::Arc::as_ptr(origin) as usize, bucket_index);
        let epoch = self.epoch;
        let frame = self.frame;
        if let Some(entry) = self.entries.get_mut(&key)
            && entry.epoch == epoch
            && alloc::sync::Arc::ptr_eq(&entry.origin, origin)
        {
            entry.used = frame;
            return alloc::sync::Arc::clone(&entry.laid);
        }
        let laid = alloc::sync::Arc::new(build());
        self.entries.insert(
            key,
            CacheEntry {
                epoch,
                used: frame,
                origin: alloc::sync::Arc::clone(origin),
                laid: alloc::sync::Arc::clone(&laid),
            },
        );
        laid
    }
}

/// Takes an anchor in tile units to the pixel of the label plane it lands on.
/// How much a pitched camera shrinks a box at this point, which is mbgl's `projectAnchor`.
///
/// `0.5 + 0.5 * cameraToCenterDistance / w`, with `w` the clip-space fourth component the same
/// matrix `project_with` divides by. The label plane matrix is the coordinate matrix times the
/// tile matrix and the coordinate matrix is affine, so its `w` is the `p[3]` mbgl reads off
/// `posMatrix` -- the two agree without projecting twice.
///
/// One at pitch zero: every ground point then shares a `w` equal to the camera distance, so the
/// ratio is `0.5 + 0.5`. That is what keeps this off the flat path entirely.
fn perspective_with(plane: &[f64; 16], camera_to_center: f64) -> impl Fn((f32, f32)) -> f32 + '_ {
    move |point: (f32, f32)| -> f32 {
        let (x, y) = (f64::from(point.0), f64::from(point.1));
        let w = plane[3] * x + plane[7] * y + plane[15];
        if w.abs() < f64::EPSILON {
            return 1.0;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            (0.5 + 0.5 * camera_to_center / w) as f32
        }
    }
}

fn project_with(plane: &[f64; 16], padding: f32) -> impl Fn((f32, f32)) -> (f32, f32) + '_ {
    move |point: (f32, f32)| -> (f32, f32) {
        let (x, y) = (f64::from(point.0), f64::from(point.1));
        let w = plane[3] * x + plane[7] * y + plane[15];
        if w.abs() < f64::EPSILON {
            return (0.0, 0.0);
        }
        // Offset into the padded grid, as mbgl offsets by `viewportPadding`: screen (0, 0) is
        // the grid's (padding, padding), so a label off the left edge keeps a real position
        // instead of being clamped onto the boundary cells.
        #[allow(clippy::cast_possible_truncation)]
        (
            ((plane[0] * x + plane[4] * y + plane[12]) / w) as f32 + padding,
            ((plane[1] * x + plane[5] * y + plane[13]) / w) as f32 + padding,
        )
    }
}

/// The labels of one bucket, numbered from `base`.
///
/// Built twice per frame -- once to place and once to write -- because what is expensive is
/// `lay_out`, which is done once and held; this is references and a clone of each instance's box.
fn frame_labels<'a>(
    laid: &'a [tessella_layout::symbol_bucket::LaidOut],
    buffers: &SymbolBuffers,
    icons: Option<&(SymbolBuffers, Vec<tessella_layout::symbol_bucket::LaidOut>)>,
    ids: &[u32],
    perspective: impl Fn((f32, f32)) -> f32,
) -> Vec<crate::symbols::FrameLabel<'a>> {
    laid.iter()
        .enumerate()
        .map(|(index, instance)| crate::symbols::FrameLabel {
            // One per laid-out symbol, in the order they were laid out, from the layer's
            // cross-tile index. Zero for a symbol the index did not reach, which reads as an
            // unplaced label rather than as another label's identity.
            cross_tile_id: ids.get(index).copied().unwrap_or(0),
            laid_out: instance.clone(),
            // Its icon's box, so the pair is decided together: `text-optional` and
            // `icon-optional` are about exactly this, and a shield that cannot have its number
            // should not keep its shield.
            icon: icons.and_then(|(_, placed)| {
                placed
                    .iter()
                    .find(|icon| icon.pending == instance.pending)
                    .cloned()
            }),
            perspective: perspective(instance.anchor),
            // The instance's own run, not the feature's whole line.
            //
            // `LaidOut::segment` is an index into the run `get_anchors` walked, so it only means
            // anything paired with that run. Reading the line back off `pending` handed the walk
            // the unclipped geometry: on any road the tile boundary cut -- which at this zoom is
            // most of the long ones -- the segment index then named a different pair of vertices
            // and the glyphs marched off along the wrong stretch.
            line: instance.line.as_slice(),
            // Four vertices to a glyph, and one offset per glyph, which is how
            // `write_line_positions` indexes the same buffer.
            glyph_reach: {
                let quads = instance.vertices.start / 4..instance.vertices.end / 4;
                buffers
                    .glyph_offsets
                    .get(quads)
                    .and_then(|offsets| Some((*offsets.first()?, *offsets.last()?)))
            },
        })
        .collect()
}

/// Encodes one bucket for the wire.
///
/// Every kind carries a vertex buffer now, the background included: it used to be the exception,
/// on the grounds that a viewport-filling quad is the consumer's to synthesize, and the
/// consequence was a `ViewUse` naming an id no `GeometryAdd` declared. The oracle sends the quad
/// too — four vertices and six indices, static across every capture.
/// Which of a bucket's records a drawable draws.
///
/// The sub-layer says it, because the sub-layer is what `DrawOrder` assigns and it is already
/// what separates the drawables. A fill's are one and two — its triangles and its outline. An
/// extrusion's are zero to three, roof and walls in the depth pass then roof and walls in the
/// colour pass, so the part alternates and the pass does not change which record is drawn.
fn part_of(content: &Content, sub_layer_index: i32) -> usize {
    let sub = usize::try_from(sub_layer_index).unwrap_or(0);
    match content {
        Content::Fill(_) => sub.saturating_sub(1),
        Content::Fill3d(_) => sub % 2,
        // Zero is the glyphs and one the sprites, in the order the encoder returns them.
        Content::Symbol(_) => sub,
        _ => 0,
    }
}

/// The id every part is encoded with, before the caller stamps each drawable's own.
const PLACEHOLDER: tessella_capture_abi::envelope::GeometryId =
    tessella_capture_abi::envelope::GeometryId(0);

/// Every distinct geometry record a bucket produces, in part order.
///
/// One for most kinds. Two for a fill — earcut's triangles and the outline's line loop, which
/// take different shaders over different index buffers — and two for an extrusion: the roof and
/// the walls raised over it. A drawable then names the part it draws rather than the encoder
/// being called once per drawable, which is what stopped a translucent extrusion copying its
/// vertices, indices and interleaved attributes twice.
///
/// The ids are placeholders. A record is cloned per drawable and stamped with that drawable's
/// own id, because `ViewRelease` is keyed by (geometry, view) and sharing one id across two
/// drawables would drop both with nothing in the stream saying so.
fn encode_parts(
    arena: &mut SlabArena,
    bucket: &LayerBucket,
    context: &Encoding<'_>,
) -> Option<alloc::vec::Vec<emit::Encoded>> {
    let &Encoding {
        patterns,
        raster_texture,
        zoom,
        stacks,
        prepared,
        key,
    } = context;
    let bind = |family: &[BuiltIn], shader: BuiltIn| {
        let ids = attribute_ids(family);
        let key = permutation_key(&bucket.paint, &ids);
        let vertex_layout = layout(&bucket.binder, &ids, |attr_id| {
            declared_for(shader, attr_id).map(|a| (a.binding, a.declared))
        });
        (vertex_layout, key)
    };

    // Set by the arms whose second part is built from the first's buffers.
    let mut fill_shared = None;
    let mut fill_atlas = None;
    let mut extrusion_shared = None;
    let mut extrusion_atlas = None;
    let encoded = match &bucket.content {
        Content::Fill(fill) => {
            let (vertex_layout, key) = bind(FILL_FAMILY, BuiltIn::FillShader);
            // A pattern binds the atlas and a different shader; without one the layer draws
            // as a plain fill, which is what a frame with no sprites fetched should do.
            let atlas = patterns
                .filter(|patterns| {
                    patterns
                        .placement(&bucket.paint, "fill-pattern", zoom)
                        .is_some()
                })
                .map(|patterns| patterns.texture);
            fill_atlas = atlas;
            let (encoded, buffers) = emit::encode_fill(arena, PLACEHOLDER, fill, &{
                let draw =
                    emit::FillDraw::new(&vertex_layout, bucket.binder.data(), key, None, atlas);
                // A data-driven pattern's rectangles, when the bucket build resolved any.
                if bucket.pattern_vertices.covers(fill.vertices.len()) {
                    draw.with_pattern_vertices(&bucket.pattern_vertices)
                } else {
                    draw
                }
            });
            fill_shared = Some(buffers);
            Some(encoded)
        }
        Content::Line(line) => {
            let (vertex_layout, key) = bind(LINE_FAMILY, BuiltIn::LineShader);
            let atlas = patterns
                .filter(|patterns| {
                    patterns
                        .placement(&bucket.paint, "line-pattern", zoom)
                        .is_some()
                })
                .map(|patterns| patterns.texture);
            Some(emit::encode_line(
                arena,
                PLACEHOLDER,
                line,
                &emit::LineDraw {
                    layout: &vertex_layout,
                    attributes: bucket.binder.data(),
                    permutation_key: key,
                    pattern_atlas: atlas,
                    // Only where the atlas resolved: a pattern the sprite sheet does not hold
                    // draws as a plain line, and rectangles for a pattern nothing will bind are
                    // bytes on the wire that no shader reads.
                    pattern_vertices: atlas.and(Some(&bucket.pattern_vertices)),
                },
            ))
        }
        Content::Circle(circle) => {
            let (vertex_layout, key) = bind(CIRCLE_FAMILY, BuiltIn::CircleShader);
            Some(emit::encode_circle(
                arena,
                PLACEHOLDER,
                circle,
                &vertex_layout,
                bucket.binder.data(),
                key,
            ))
        }
        Content::Fill3d(extrusion) => {
            let (vertex_layout, key) = bind(FILL_EXTRUSION_FAMILY, BuiltIn::FillExtrusionShader);
            let atlas = patterns
                .filter(|patterns| {
                    patterns
                        .placement(&bucket.paint, "fill-extrusion-pattern", zoom)
                        .is_some()
                })
                .map(|patterns| patterns.texture);
            // The walls stand on these, and are not emitted yet: the drawable dispatch here
            // encodes one record per bucket and copies it for the second pass, where an
            // extrusion needs two *different* records — the roof and the instanced walls — each
            // used by both passes. That is the next change; the encoder for the walls exists and
            // is checked against the capture.
            extrusion_atlas = atlas;
            let (roof, buffers) = emit::encode_extrusion(
                arena,
                PLACEHOLDER,
                extrusion,
                &vertex_layout,
                bucket.binder.data(),
                key,
                atlas,
            );
            extrusion_shared = Some(buffers);
            Some(roof)
        }
        Content::Symbol(layout) => {
            // Shaped and placed already, by `place_symbols`. Not here, and the reason is the
            // grid: placement decides a *frame* -- a road name and a shop name compete for the
            // same screen whatever layer or tile each came from -- and this walk visits one
            // bucket at a time, in painter order, which is both too narrow a view and the wrong
            // order to decide in.
            let PreparedSymbols { buffers, icons } = prepared.get(&key)?;
            let (buffers, icons) = (buffers, icons.as_ref());
            let ids = attribute_ids(SYMBOL_FAMILY);
            let key = permutation_key(&bucket.paint, &ids);
            // Text is always SDF. An icon may be either, and the flag is already packed into
            // each vertex's size field, so this only decides which shader is named — unless the
            // label draws a sprite inline, which needs the shader that samples both atlases.
            // The sprite atlas the labels' images came out of, which is the one the pattern
            // and icon layers already share. Only bound when a label actually draws one.
            let sprites = buffers
                .icons_in_text
                .then(|| patterns.map(|patterns| patterns.texture))
                .flatten();
            // The atlas this bucket's own glyphs were packed into. A bucket drawing more than
            // one stack can name only one texture, so it takes the first; splitting such a bucket
            // per stack is what mbgl does and is not done here yet.
            let atlas = layout
                .stacks()
                .first()
                .and_then(|stack| stacks.iter().position(|held| held == stack))
                .filter(|index| *index < GLYPH_ATLAS_CAP)
                .map_or_else(|| glyph_atlas_id(0), glyph_atlas_id);
            let text = emit::encode_symbol(
                arena,
                PLACEHOLDER,
                buffers,
                key,
                true,
                atlas,
                sprites,
                // Glyphs are always sampled linearly, whatever the icons do.
                tessella_capture_abi::envelope::TextureFilter::Linear,
            );

            match icons {
                Some(shaped) => {
                    // Two records, like an extrusion's roof and walls: returned here rather than
                    // falling through, because what follows expects one.
                    let sheet = patterns.map_or(atlas, |patterns| patterns.texture);
                    // An icon drawn at its own size is sampled nearest, which is what keeps its
                    // edges on pixel boundaries. See `SymbolLayout::icons_need_linear` for the
                    // half of the test that reads the style, and the sprite sheet for the other
                    // half: a sprite packed at a different pixel ratio from the map's is being
                    // rescaled whatever the style says.
                    let scaled = layout.icons_need_linear
                        || patterns.is_some_and(|patterns| {
                            layout.icons().iter().any(|name| {
                                patterns.positions.get(name).is_some_and(|position| {
                                    // The map's, which this frontend renders at one.
                                    (position.pixel_ratio - 1.0).abs() > f64::EPSILON
                                })
                            })
                        });
                    let filter = if scaled {
                        tessella_capture_abi::envelope::TextureFilter::Linear
                    } else {
                        tessella_capture_abi::envelope::TextureFilter::Nearest
                    };
                    return Some(alloc::vec![
                        text,
                        emit::encode_symbol(
                            arena,
                            PLACEHOLDER,
                            shaped,
                            key,
                            false,
                            sheet,
                            None,
                            filter,
                        ),
                    ]);
                }
                None => Some(text),
            }
        }
        Content::Raster(raster) => Some(emit::encode_raster(
            arena,
            PLACEHOLDER,
            &raster.bucket,
            raster_texture,
        )),
        Content::Background => {
            let atlas = patterns
                .filter(|patterns| {
                    patterns
                        .placement(&bucket.paint, "background-pattern", zoom)
                        .is_some()
                })
                .map(|patterns| patterns.texture);
            Some(emit::encode_background(arena, PLACEHOLDER, atlas))
        }
    }?;
    let mut parts = alloc::vec![encoded];
    // The second part, where there is one. A fill's outline is built from the first's buffers;
    // an extrusion's walls stand on the roof's outline.
    if let Some(shared) = fill_shared {
        let (vertex_layout, key) = bind(FILL_FAMILY, BuiltIn::FillOutlineShader);
        parts.push(
            emit::encode_fill(
                arena,
                PLACEHOLDER,
                match &bucket.content {
                    Content::Fill(fill) => fill,
                    _ => return Some(parts),
                },
                &emit::FillDraw {
                    layout: &vertex_layout,
                    attributes: bucket.binder.data(),
                    permutation_key: key,
                    shared: Some(shared),
                    pattern_atlas: fill_atlas,
                    pattern_vertices: None,
                },
            )
            .0,
        );
    }
    if let Some(shared) = extrusion_shared {
        let (wall_layout, key) = bind(FILL_EXTRUSION_FAMILY, BuiltIn::FillExtrusionInstancedShader);
        parts.push(emit::encode_extrusion_walls(
            arena,
            PLACEHOLDER,
            shared,
            &wall_layout,
            key,
            extrusion_atlas,
        ));
    }
    Some(parts)
}

/// Writes one layer's clip masks and uniform blocks.
///
/// # Why the blocks are per kind and not one shape
///
/// Each layer kind has its own drawable block, its own tile-properties block and its own
/// evaluated-properties block, at its own slots and strides. Writing a fill's for every tiled
/// layer — which this did while the line layer did not exist — puts a line layer's uniforms into
/// the shape a fill shader reads, and the shader has no way to know.
fn write_layer_state(
    producer: &mut Producer,
    frame: &Frame<'_>,
    layer_index: i32,
    bindings: &[GeometryBinding],
    tiles: &[TileCoord],
) -> Result<(), FrameError> {
    let Frame {
        style,
        view,
        view_id,
        patterns,
        ..
    } = *frame;

    let tiled = bindings.iter().any(|binding| {
        binding
            .flags
            .contains(tessella_capture_abi::envelope::DrawFlags::ENABLE_STENCIL)
    });
    if tiled {
        // This layer's own tiles, not the frame's.
        //
        // mbgl sets the stencil per layer group -- `tileLayerGroup->setStencilTiles(renderTiles)`
        // -- and the distinction only shows when two layers draw at different zooms. A raster
        // source is looked up at its own zoom, so a style with one puts z16 tiles in the frame
        // beside the vector layers' z15. Masking every layer with all of them wrote the z16
        // masks over the same screen area, and a z15 drawable's reference no longer survived: the
        // water and the pattern vanished outright, 75,340 pixels of river reduced to none, while
        // the frame still issued every one of their drawables. It reads as the imagery painting
        // over what is under it, which is how it was first described and why it was looked for in
        // painter order.
        let used: alloc::collections::BTreeSet<(u8, u32, u32)> = bindings
            .iter()
            .filter_map(|binding| binding.tile)
            .map(|tile| (tile.z, tile.x, tile.y))
            .collect();
        let mine: Vec<TileCoord> = tiles
            .iter()
            .copied()
            .filter(|coord| used.contains(&(coord.z, coord.x, coord.y)))
            .collect();
        let set = stencil::clip_set(view, layer_index, &mine)
            .map_err(|error| FrameError::Camera(alloc::format!("{error}")))?;
        stencil::write(producer, view_id, &set)?;
    }

    let Some(layer) = usize::try_from(layer_index)
        .ok()
        .and_then(|index| style.layers.get(index))
    else {
        return Ok(());
    };
    let Ok(paint) = tessella_style::property::resolve_paint(layer) else {
        return Ok(());
    };

    let matrices = |sub_layer_index: i32| {
        bindings
            .iter()
            .filter(move |binding| binding.sub_layer_index == sub_layer_index)
            .filter_map(|binding| binding.tile)
    };
    let entries = |sub_layer_index: i32| -> Vec<DrawableEntry> {
        matrices(sub_layer_index)
            .filter_map(|tile| {
                DrawableEntry::for_tile_with(
                    view,
                    tile.z,
                    tile.x,
                    tile.y,
                    i32::from(tile.wrap),
                    layer_index,
                    sub_layer_index,
                    ubo::fill_interpolations(&paint, f64::from(tile.z), view.zoom, sub_layer_index),
                )
                .ok()
            })
            .collect()
    };

    match layer.kind {
        LayerKind::Background => {
            // A viewport background has no tile placement: its matrix is the clip cube and the
            // same for every frame. Counted off the bindings rather than assumed to be one, so
            // the buffer stays the length the drawables address it at whatever the cover did.
            let drawables: Vec<DrawableEntry> =
                if crate::tile::background_covers_viewport(style, view.zoom) {
                    matrices(0)
                        .map(|_| DrawableEntry::for_viewport(layer_index, 0))
                        .collect()
                } else {
                    entries(0)
                };
            let buffer = ubo::pack_drawable_buffer(
                &drawables,
                ubo_layouts::BACKGROUND_DRAWABLE_UNION_UBO.stride,
            );
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo::drawable_slot(),
                &buffer,
            )?;

            // Through the same reader every other uniform colour uses. Evaluating the
            // expression here and asking the result for a *string* gets `None` for every style
            // ever written: the property boundary coerces a colour-typed property to a colour,
            // so the value is already `Value::Color` — and the fallback that catches is black,
            // which is a background nobody chose and one that looks deliberate.
            // A background with a pattern writes a different block at the same slot: sixty-four
            // bytes of corners, display sizes and the crossfade where a plain one writes
            // thirty-two of colour and opacity. The two are told apart by their size, which is
            // why this slot is not a union the way a fill's is.
            let opacity = ubo::uniform_number(&paint, "background-opacity", view.zoom);
            let props = match patterns
                .and_then(|patterns| patterns.background_placement(&paint, view.zoom, opacity))
            {
                Some(placement) => ubo::pack_background_pattern_props(&placement),
                None => ubo::background_props_from_paint(&paint, view.zoom),
            };
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_BACKGROUND_PROPS_UBO,
                &props,
            )?;
        }
        LayerKind::Fill => {
            // Triangles then outline, which is the order the oracle's buffer is in.
            let mut all = entries(1);
            all.extend(entries(2));
            let placement =
                patterns.and_then(|patterns| patterns.placement(&paint, "fill-pattern", view.zoom));

            // A patterned fill takes its own drawable layout, not the plain one.
            //
            // The two share the union's stride, so nothing about the buffer's length says which
            // is meant, and `FillPatternDrawableUBO` puts the tile's pixel origin and ratio
            // exactly where `FillDrawableUBO` puts its zoom-mix factors. Written as a plain fill,
            // `tile_ratio` arrived as zero -- and a zero ratio drops the world position out of
            // `patternPos`, so every fragment samples one point of the sprite. That point is the
            // rectangle's own corner, which in the atlas is the padding around it, so the layer
            // drew at a seventh of its strength: a wash the shape of the parks rather than a
            // texture in them.
            let buffer = if placement.is_some() {
                // Borrowed, not moved: the closures below are `move` so the factor's source has
                // to be something they can copy.
                let paint_ref = &paint;
                let pattern: Vec<ubo::PatternDrawableEntry> = [1, 2]
                    .into_iter()
                    .flat_map(|sub| {
                        matrices(sub).filter_map(move |tile| {
                            ubo::PatternDrawableEntry::for_tile(
                                view,
                                tile.z,
                                tile.x,
                                tile.y,
                                i32::from(tile.wrap),
                                layer_index,
                                sub,
                                // A data-driven pattern is not implemented, so the two crossfade
                                // factors are zero; the opacity's is the one a fill already has.
                                [
                                    0.0,
                                    0.0,
                                    ubo::fill_interpolations(
                                        paint_ref,
                                        f64::from(tile.z),
                                        view.zoom,
                                        sub,
                                    )[1],
                                ],
                            )
                            .ok()
                        })
                    })
                    .collect();
                ubo::pack_fill_pattern_drawable_buffer(
                    &pattern,
                    ubo_layouts::FILL_DRAWABLE_UNION_UBO.stride,
                )
            } else {
                ubo::pack_drawable_buffer(&all, ubo_layouts::FILL_DRAWABLE_UNION_UBO.stride)
            };
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo::drawable_slot(),
                &buffer,
            )?;

            // Where the pattern's two images sit, when the layer has one. The buffer is the
            // same length either way — `FillPatternTilePropsUBO` is the union's stride, and a
            // layer with no pattern writes the zeroes the shader ignores.
            //
            // One placement repeated, not one per drawable computed separately: a pattern that
            // is not data-driven resolves to the same pair of rectangles for every tile, which
            // is what the capture carries — twelve identical blocks over twelve drawables.
            let tile_props = match placement {
                Some(placement) => ubo::pack_pattern_tile_props(&alloc::vec![placement; all.len()]),
                None => ubo::pack_tile_props_buffer(
                    all.len(),
                    ubo_layouts::FILL_TILE_PROPS_UNION_UBO.stride,
                ),
            };
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_FILL_TILE_PROPS_UBO,
                &tile_props,
            )?;

            let props = ubo::fill_props_from_paint(&paint, view.zoom);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_FILL_EVALUATED_PROPS_UBO,
                &props,
            )?;
        }
        LayerKind::Line => {
            let line: Vec<ubo::LineDrawableEntry> = matrices(0)
                .filter_map(|tile| {
                    ubo::LineDrawableEntry::for_tile(
                        view,
                        tile.z,
                        tile.x,
                        tile.y,
                        i32::from(tile.wrap),
                        layer_index,
                        0,
                        ubo::line_interpolations(&paint, f64::from(tile.z), view.zoom),
                    )
                    .ok()
                })
                .collect();
            let buffer =
                ubo::pack_line_drawable_buffer(&line, ubo_layouts::LINE_DRAWABLE_UNION_UBO.stride);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_LINE_DRAWABLE_UBO,
                &buffer,
            )?;

            // A line's pattern block is wider than a fill's — it carries the scale and the
            // fade — so it is packed by its own function, not by the fill's with a different
            // stride. The union's stride is the line's sixty-four either way.
            let tile_props = match patterns.and_then(|patterns| {
                Some((
                    patterns,
                    patterns.placement(&paint, "line-pattern", view.zoom)?,
                ))
            }) {
                Some((patterns, placement)) => {
                    let entry = ubo::LinePatternPlacement {
                        placement,
                        pixel_ratio: 1.0,
                        // Tile units per pixel at the tile's own level, inverted. Every tile of
                        // a cover is at the same level, so one value serves the layer.
                        #[allow(clippy::cast_possible_truncation)]
                        units_per_pixel: tiles.first().map_or(1.0, |tile| {
                            1.0 / tessella_tile::camera::pixels_to_tile_units(
                                tile.z,
                                f64::from(tile.z),
                            ) as f32
                        }),
                        crossfade: patterns.crossfade(view.zoom),
                    };
                    ubo::pack_line_pattern_tile_props(&alloc::vec![entry; line.len()])
                }
                None => ubo::pack_tile_props_buffer(
                    line.len(),
                    ubo_layouts::LINE_TILE_PROPS_UNION_UBO.stride,
                ),
            };
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_LINE_TILE_PROPS_UBO,
                &tile_props,
            )?;

            let props = ubo::line_props_from_paint(&paint, view.zoom);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_LINE_EVALUATED_PROPS_UBO,
                &props,
            )?;
        }
        LayerKind::Circle => {
            let pitch_with_map = false;
            let circles: Vec<ubo::CircleDrawableEntry> = matrices(0)
                .filter_map(|tile| {
                    ubo::CircleDrawableEntry::for_tile(
                        view,
                        tile.z,
                        tile.x,
                        tile.y,
                        i32::from(tile.wrap),
                        layer_index,
                        0,
                        ubo::circle_extrude_scale(pitch_with_map, tile.z, view),
                        ubo::circle_interpolations(&paint, f64::from(tile.z), view.zoom),
                    )
                    .ok()
                })
                .collect();
            let buffer =
                ubo::pack_circle_drawable_buffer(&circles, ubo_layouts::CIRCLE_DRAWABLE_UBO.stride);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_CIRCLE_DRAWABLE_UBO,
                &buffer,
            )?;

            // No tile-properties block: a circle has no pattern variant to need one, which is
            // why the oracle writes two blocks for this layer where a fill gets three.
            let props = ubo::circle_props_from_paint(&paint, view.zoom);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_CIRCLE_EVALUATED_PROPS_UBO,
                &props,
            )?;
        }
        LayerKind::FillExtrusion => {
            // Its own entry shape, not a fill's. An extrusion's block carries the height factor
            // and the tile's split pixel coordinate where a fill's carries mix factors, so a
            // fill entry packed into it reads the colour interpolation as `height_factor` --
            // zero, for a constant colour -- and draws every building flat on the ground.
            //
            // Both passes: a translucent extrusion takes a depth pass in front of its colour
            // pass, and both read the same buffer.
            // `FillExtrusionTilePropsUBO` is `FillPatternTilePropsUBO`'s fields exactly —
            // two rectangles, an atlas size, two pads, forty-eight bytes — so it takes the same
            // packer rather than one of its own.
            let extrusion_pattern = patterns.and_then(|patterns| {
                patterns.placement(&paint, "fill-extrusion-pattern", view.zoom)
            });

            let interpolations = ubo::extrusion_interpolations(&paint, view.zoom, view.zoom);
            let entry = |sub_layer_index: i32| -> Vec<ubo::ExtrusionDrawableEntry> {
                matrices(sub_layer_index)
                    .filter_map(|tile| {
                        ubo::ExtrusionDrawableEntry::for_tile(
                            view,
                            tile.z,
                            tile.x,
                            tile.y,
                            i32::from(tile.wrap),
                            interpolations,
                        )
                        .ok()
                    })
                    .collect()
            };
            // Every sub-layer the extrusion emits, in the order the indices are handed out.
            //
            // Four of them when the layer needs a depth pass -- 0 and 1 draw depth, 2 and 3 draw
            // colour -- and `ubo_index` is numbered per *layer* across all four. Packing only the
            // first two left the colour pass indexing past the end of its own buffer, where the
            // consumer counts it `unplaced` and skips it: eighteen drawables of thirty-six on a
            // twelve-tile frame. What was on screen was the *depth* pass, drawn with colour
            // because the consumer did not honour `ENABLE_COLOR` either, which is why the
            // buildings looked like flat footprints with the roof's shade.
            //
            // `matrices` yields nothing for a sub-layer that has no bindings, so chaining all
            // four is also right for an opaque extrusion, which emits 2 and 3 alone.
            let mut all = entry(0);
            all.extend(entry(1));
            all.extend(entry(2));
            all.extend(entry(3));
            let buffer = ubo::pack_extrusion_drawable_buffer(
                &all,
                ubo_layouts::FILL_EXTRUSION_DRAWABLE_UBO.stride,
            );
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_FILL_EXTRUSION_DRAWABLE_UBO,
                &buffer,
            )?;

            // Only when the layer has a pattern. A fill and a line write a zero-filled block
            // whatever their paint, because their slot is a union and the stride is the same
            // either way; an extrusion without a pattern has written nothing here, and adding a
            // buffer to a case the goldens already pin is a change to make deliberately rather
            // than in passing.
            if let Some(placement) = extrusion_pattern {
                let tile_props = ubo::pack_pattern_tile_props(&alloc::vec![placement; all.len()]);
                ubo::write(
                    producer,
                    view_id,
                    layer_index,
                    ubo_slots::ID_FILL_EXTRUSION_TILE_PROPS_UBO,
                    &tile_props,
                )?;
            }

            let props = ubo::fill_extrusion_props_from_paint(&paint, view.zoom, frame.light);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_FILL_EXTRUSION_PROPS_UBO,
                &props,
            )?;
        }
        LayerKind::Symbol => {
            // A symbol's drawable block is three matrices, not one: the clip matrix, the matrix
            // of the plane the label was laid out in, and that plane back to clip. A label
            // placed along a line is positioned in the label plane and only then projected, so
            // a consumer given the clip matrix alone can place a point label and nothing else.
            #[allow(clippy::cast_precision_loss)]
            let sheet_size = patterns.map_or([0.0, 0.0], |patterns| {
                [f32::from(patterns.size[0]), f32::from(patterns.size[1])]
            });
            // A glyph atlas, or a stand-in for a layer that needs none.
            //
            // Returning here when the frame held no fonts left the layer with *no uniform blocks
            // at all*, and a drawable whose layer has none is skipped by the consumer before it
            // is placed -- so an icon-only layer got as far as batches arriving with their meshes
            // built, and drew nothing. It never asks for glyphs, so it never had fonts to return
            // on.
            //
            // The size is only ever divided into a glyph's texture coordinates. A layer with no
            // glyphs has none to divide, and its icons take `sheet_size` instead; one, rather
            // than zero, because the shader divides by it.
            let atlas_size = frame
                .fonts
                .and_then(|fonts| symbol_atlas_size(style, layer, fonts))
                .unwrap_or([1.0, 1.0]);
            let zoom = view.zoom;
            let placement = Placement::of(layer, zoom);
            let alignments = Alignments::of(layer, zoom, placement, "text");
            // The layer-wide `text-size`, which is what the shader scales every glyph's corners
            // by. A data-driven one is in the vertex instead and this is then the fallback the
            // constant path never reads.
            //
            // Read the way the layout read it, not out of the spec tables. `resolve_layout`
            // covers the layer kinds whose layout feeds an interleaved buffer and answers an
            // empty map for a symbol layer, so this was the spec default for every style ever
            // loaded: labels drew at 16 pixels whatever the style asked for, which is most of
            // what made our type a different size from the oracle's.
            let size = tessella_style::property::layout_value(layer, "text-size", zoom, None)
                .and_then(|value| value.as_number())
                .unwrap_or(16.0);
            #[allow(clippy::cast_possible_truncation)]
            let size = size as f32;

            // `icon-size` is a multiplier and defaults to one, where `text-size` names a size in
            // pixels. The shader divides by `ONE_EM` for text and does not for an icon, so the
            // two halves need their own value as well as their own flag.
            let icon_size = tessella_style::property::layout_value(layer, "icon-size", zoom, None)
                .and_then(|value| value.as_number())
                .unwrap_or(1.0);
            #[allow(clippy::cast_possible_truncation)]
            let icon_size = icon_size as f32;

            // Both halves, in sub-layer order, the way a fill packs its triangles and its
            // outline. A symbol layer that draws sprites has two drawables per tile and each
            // needs its own matrix slot: packing only the glyphs left the icon drawable pointing
            // one slot past the end of the buffer, where it was counted `unplaced` and skipped,
            // and a highway shield drew its number over nothing.
            let entry = |sub_layer_index: i32| {
                let sub = sub_layer_index;
                matrices(sub).filter_map(move |tile| {
                    ubo::SymbolDrawableEntry::for_tile(
                        view,
                        tile.z,
                        tile.x,
                        tile.y,
                        i32::from(tile.wrap),
                        layer_index,
                        sub,
                        atlas_size,
                        // The sprite sheet's size, where a zero used to be. An icon's texture
                        // coordinates are sheet pixels and the shader divides by this to get
                        // them into 0..1, so a zero here is a division the consumer has to guard
                        // -- and guarding it with one leaves the coordinates in the hundreds,
                        // wrapping the sampler round to whatever is at the origin. That is why an
                        // icon drew as a flat black square rather than as its sprite.
                        sheet_size,
                        // Sub-layer 1 is the sprite half -- see `bindings_for`.
                        if sub == 1 { icon_size } else { size },
                        sub != 1,
                        alignments,
                        placement,
                    )
                    .ok()
                })
            };
            let entries: Vec<ubo::SymbolDrawableEntry> = entry(0).chain(entry(1)).collect();
            let buffer =
                ubo::pack_symbol_drawable_buffer(&entries, ubo_layouts::SYMBOL_DRAWABLE_UBO.stride);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_SYMBOL_DRAWABLE_UBO,
                &buffer,
            )?;

            let gamma = ubo::symbol_gamma_scale(view, alignments.pitch);
            let tile_props = ubo::pack_symbol_tile_props(entries.len(), true, false, gamma);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_SYMBOL_TILE_PROPS_UBO,
                &tile_props,
            )?;

            let props = ubo::symbol_props_from_paint(&paint, zoom);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_SYMBOL_EVALUATED_PROPS_UBO,
                &props,
            )?;
        }
        LayerKind::Raster => {
            // The smallest drawable block of any layer: a matrix and nothing else. A raster tile
            // carries no per-feature anything, so there is nothing to interpolate and nothing to
            // bind — the picture is the tile.
            let matrices: Vec<[f32; 16]> = matrices(0)
                .filter_map(|tile| {
                    DrawableEntry::for_tile(
                        view,
                        tile.z,
                        tile.x,
                        tile.y,
                        i32::from(tile.wrap),
                        layer_index,
                        0,
                    )
                    .ok()
                    .map(|entry| entry.matrix)
                })
                .collect();
            let buffer = ubo::pack_raster_drawable_buffer(
                &matrices,
                ubo_layouts::RASTER_DRAWABLE_UBO.stride,
            );
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_RASTER_DRAWABLE_UBO,
                &buffer,
            )?;

            let props = ubo::raster_props_from_paint(&paint, view.zoom);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_RASTER_EVALUATED_PROPS_UBO,
                &props,
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// The glyph atlas a symbol layer samples, in pixels.
///
/// From the atlas itself rather than from a constant: the shader divides a vertex's texture
/// coordinates by this to reach `0..1`, so a size that disagrees with the texture stretches
/// every glyph by the ratio between them — legible, wrong, and easy to mistake for a font.
fn symbol_atlas_size(
    style: &Style,
    layer: &tessella_style::Layer,
    fonts: &Fonts,
) -> Option<[f32; 2]> {
    let _ = style;
    let stack = layer
        .layout
        .get("text-font")
        .and_then(|value| match value {
            tessella_style::PropertyValue::Literal(literal) => literal.as_array(),
            tessella_style::PropertyValue::Expression(_) => None,
        })
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(alloc::string::ToString::to_string))
                .collect::<Vec<_>>()
        })?;
    let atlas = fonts.atlas(&stack)?;
    let (width, height) = atlas.size();
    #[allow(clippy::cast_precision_loss)]
    Some([width as f32, height as f32])
}
