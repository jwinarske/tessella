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
use tessella_layout::symbol_layout::{Alignments, Placement};
use tessella_style::crossfade::ZoomHistory;
use tessella_style::light::Light;
use tessella_style::property::ResolvedProperty;
use tessella_style::{LayerKind, Style};
use tessella_tile::cover::{TileCoord, ViewTransform};

use crate::binder::{
    CIRCLE_FAMILY, FILL_EXTRUSION_FAMILY, FILL_FAMILY, LINE_FAMILY, SYMBOL_FAMILY, attribute_ids,
    layout, permutation_key,
};
use crate::camera::CameraBlock;
use crate::emit::SlabArena;
use crate::order::{self, DrawOrder};
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
    pub drawables: usize,
    /// The order epoch the camera names.
    pub epoch: OrderEpoch,
}

impl Default for Emitted {
    fn default() -> Self {
        Self {
            geometries: 0,
            drawables: 0,
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

/// The texture a symbol drawable samples.
///
/// One texture whichever kind of symbol it is. mbgl's `DrawableAtlasesTweaker` is explicit:
/// a shader declaring no separate icon sampler gets the glyph atlas for a text drawable and the
/// *icon* atlas for an icon drawable, at the same slot either way.
const GLYPH_ATLAS: tessella_capture_abi::envelope::TextureId =
    tessella_capture_abi::envelope::TextureId(2);

/// The first texture id a raster tile's picture takes.
///
/// One per tile rather than one per layer: a raster tile *is* its picture, and two raster layers
/// over one source are two buckets sharing one image (§11.5). Numbered above the atlases so a
/// raster texture and a glyph atlas never collide.
const RASTER_TEXTURE_BASE: u64 = 16;

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
    producer.begin();
    let mark = arena.mark();
    match emit_group(producer, arena, frame) {
        Ok(emitted) => {
            producer.commit();
            Ok(emitted)
        }
        Err(error) => {
            producer.abort();
            // The arena as well as the ring. The discarded records were the only things that
            // would ever have named these slabs.
            arena.rewind(mark);
            Err(error)
        }
    }
}

fn emit_group(
    producer: &mut Producer,
    arena: &mut SlabArena,
    frame: &Frame<'_>,
) -> Result<Emitted, FrameError> {
    let Frame {
        style,
        view,
        view_id,
        tiles,
        buckets,
        light,
        fonts,
        patterns,
    } = *frame;

    let mut session = ViewSession::new();
    session
        .declare(producer, view_id, CameraMode::Producer)
        .map_err(FrameError::from)?;

    // Frame-wide state the shaders read whatever the style says. The placeholders matter: a
    // shader samples its texture slots unconditionally, so a drawable whose layer binds none
    // still reads whatever was last at that slot.
    for upload in texture::placeholders() {
        texture::write(producer, &upload)?;
    }
    // The glyph atlas, before any drawable names it. A symbol geometry carries a texture
    // reference, and a reference to a texture the consumer has not been given is a drawable that
    // samples whatever was last at that slot.
    if let Some(fonts) = fonts {
        for stack in symbol_stacks(buckets) {
            if let Some(atlas) = fonts.atlas(&stack) {
                let (width, height) = atlas.size();
                let whole = [tessella_glyph::atlas::Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                }];
                if let Some(upload) = texture::glyph_atlas(GLYPH_ATLAS, atlas, &whole) {
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

    let global = ubo::GlobalPaintParams::for_view(view, [64.0, 64.0], 1.0).pack();
    ubo::write(
        producer,
        view_id,
        ubo::FRAME_WIDE,
        ubo_slots::ID_GLOBAL_PAINT_PARAMS_UBO,
        &global,
    )?;

    #[allow(clippy::cast_possible_truncation)]
    let mut draw_order = DrawOrder::new(style.layers.len() as u32);
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
    let mut packed_bytes: BTreeMap<(usize, usize), (emit::Encoded, Option<emit::FillShared>)> =
        BTreeMap::new();

    for (index, (tile, tile_buckets)) in buckets.iter().enumerate() {
        // A raster tile's picture goes up before any drawable names it, for the reason the glyph
        // atlas does: a texture reference the consumer has not been given samples whatever was
        // last at that slot.
        #[allow(clippy::cast_possible_truncation)]
        let raster_texture =
            tessella_capture_abi::envelope::TextureId(RASTER_TEXTURE_BASE + index as u64);
        for bucket in tile_buckets {
            if let Content::Raster(raster) = &bucket.content
                && let Some(upload) = texture::raster_tile(raster_texture, &raster.image)
            {
                texture::write(producer, &upload)?;
                break;
            }
        }

        let at = order::tile_of(tile.z, tile.x, tile.y);
        let bindings = order::bindings_for(view_id, at, tile_buckets, &mut next_id);

        // A binding per drawable, and a bucket may produce two of them — a fill's triangles and
        // its outline. Each gets its own geometry id from `bindings_for`, so each is announced
        // separately rather than one being bound to an id nothing declared.
        let mut binding_index = 0;
        for (bucket_index, bucket) in tile_buckets.iter().enumerate() {
            if !bucket.content.has_data() {
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
    let mut packed: BTreeSet<u64> = BTreeSet::new();
    let mut open: Option<u32> = None;
    for entry in draw_order.resolve() {
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
        // buffers under it, which is what `FillShared` carries. Copying the record for a fill
        // is what made the outline draw the interior a second time and `fill-outline-color`
        // render nothing at all.
        let encoded = match packed_bytes.get(&(tile_index, bucket_index)) {
            Some((_, Some(shared))) => {
                let shared = *shared;
                let Some((outline, _)) = encode(
                    arena,
                    bucket,
                    entry.geometry,
                    &Encoding {
                        fonts,
                        patterns,
                        raster_texture,
                        zoom: view.zoom,
                    },
                    Some(shared),
                ) else {
                    continue;
                };
                outline
            }
            Some((first, None)) => {
                let mut copy: emit::Encoded = first.clone();
                copy.record.geometry = entry.geometry;
                copy
            }
            None => {
                let Some(fresh) = encode(
                    arena,
                    bucket,
                    entry.geometry,
                    &Encoding {
                        fonts,
                        patterns,
                        raster_texture,
                        zoom: view.zoom,
                    },
                    None,
                ) else {
                    continue;
                };
                packed_bytes.insert((tile_index, bucket_index), fresh.clone());
                fresh.0
            }
        };
        emit::write(producer, &encoded)?;
        emitted.geometries += 1;
    }

    // Every geometry is announced before any drawable names one.
    for binding in bound {
        session
            .use_geometry(producer, binding)
            .map_err(FrameError::from)?;
        emitted.drawables += 1;
    }

    for (layer_index, bindings) in &by_layer {
        write_layer_state(producer, frame, *layer_index, bindings, tiles)?;
    }

    // The order, then the camera naming its epoch — never the other way round.
    let order = draw_order.emit(producer, view_id)?;
    emitted.epoch = order.epoch;
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

/// What every bucket's encoding reads from the frame around it.
///
/// Grouped rather than passed one by one: they are all "what this frame has fetched and where
/// its camera is", and a bucket picks the ones its kind needs.
#[derive(Clone, Copy)]
struct Encoding<'a> {
    /// Glyphs, for a symbol layer.
    fonts: Option<&'a Fonts>,
    /// Sprites, for a layer with a pattern.
    patterns: Option<&'a Patterns<'a>>,
    /// The texture this tile's raster picture went to.
    raster_texture: tessella_capture_abi::envelope::TextureId,
    /// The camera's zoom, which a pattern's fade is chosen at.
    zoom: f64,
}

/// Encodes one bucket for the wire.
///
/// Every kind carries a vertex buffer now, the background included: it used to be the exception,
/// on the grounds that a viewport-filling quad is the consumer's to synthesize, and the
/// consequence was a `ViewUse` naming an id no `GeometryAdd` declared. The oracle sends the quad
/// too — four vertices and six indices, static across every capture.
fn encode(
    arena: &mut SlabArena,
    bucket: &LayerBucket,
    geometry: tessella_capture_abi::envelope::GeometryId,
    context: &Encoding<'_>,
    shared: Option<emit::FillShared>,
) -> Option<(emit::Encoded, Option<emit::FillShared>)> {
    let &Encoding {
        fonts,
        patterns,
        raster_texture,
        zoom,
    } = context;
    let bind = |family: &[BuiltIn], shader: BuiltIn| {
        let ids = attribute_ids(family);
        let key = permutation_key(&bucket.paint, &ids);
        let vertex_layout = layout(&bucket.binder, &ids, |attr_id| {
            declared_for(shader, attr_id).map(|a| (a.binding, a.declared))
        });
        (vertex_layout, key)
    };

    // Set by the fill arm, which is the only kind whose two drawables share buffers.
    let mut fill_shared = None;
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
            let (encoded, buffers) = emit::encode_fill(
                arena,
                geometry,
                fill,
                &emit::FillDraw::new(&vertex_layout, bucket.binder.data(), key, shared, atlas),
            );
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
                geometry,
                line,
                &vertex_layout,
                bucket.binder.data(),
                key,
                atlas,
            ))
        }
        Content::Circle(circle) => {
            let (vertex_layout, key) = bind(CIRCLE_FAMILY, BuiltIn::CircleShader);
            Some(emit::encode_circle(
                arena,
                geometry,
                circle,
                &vertex_layout,
                bucket.binder.data(),
                key,
            ))
        }
        Content::Fill3d(extrusion) => {
            let (vertex_layout, key) = bind(FILL_EXTRUSION_FAMILY, BuiltIn::FillExtrusionShader);
            Some(emit::encode_extrusion(
                arena,
                geometry,
                extrusion,
                &vertex_layout,
                bucket.binder.data(),
                key,
            ))
        }
        Content::Symbol(layout) => {
            // Shaping is where a symbol layer's geometry comes from, and it cannot happen
            // earlier: the quads are a function of the glyphs, which are a function of the
            // shaped text, which is a function of the tile's features. So the bucket carries a
            // *layout* and the vertices are made here.
            let (buffers, _laid) = layout.lay_out(fonts?);
            if buffers.vertices.is_empty() {
                return None;
            }
            let ids = attribute_ids(SYMBOL_FAMILY);
            let key = permutation_key(&bucket.paint, &ids);
            // Text is always SDF. An icon may be either, and the flag is already packed into
            // each vertex's size field, so this only decides which shader is named.
            Some(emit::encode_symbol(
                arena,
                geometry,
                &buffers,
                key,
                true,
                GLYPH_ATLAS,
            ))
        }
        Content::Raster(raster) => Some(emit::encode_raster(
            arena,
            geometry,
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
            Some(emit::encode_background(arena, geometry, atlas))
        }
    }?;
    Some((encoded, fill_shared))
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
        let set = stencil::clip_set(view, layer_index, tiles)
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
            let buffer = ubo::pack_drawable_buffer(
                &entries(0),
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
            let buffer =
                ubo::pack_drawable_buffer(&all, ubo_layouts::FILL_DRAWABLE_UNION_UBO.stride);
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
            let tile_props = match patterns
                .and_then(|patterns| patterns.placement(&paint, "fill-pattern", view.zoom))
            {
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
                            layer_index,
                            sub_layer_index,
                            interpolations,
                        )
                        .ok()
                    })
                    .collect()
            };
            let mut all = entry(0);
            all.extend(entry(1));
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
            let Some(fonts) = frame.fonts else {
                return Ok(());
            };
            let Some(atlas_size) = symbol_atlas_size(style, layer, fonts) else {
                return Ok(());
            };
            let zoom = view.zoom;
            let placement = Placement::of(layer, zoom);
            let alignments = Alignments::of(layer, zoom, placement, "text");
            // The layer-wide `text-size`, which is what the shader interpolates against. A
            // data-driven one is in the vertex instead and this is then the fallback the
            // constant path never reads.
            let size = tessella_style::property::resolve_layout(layer)
                .ok()
                .and_then(|layout| {
                    let property = layout.get("text-size")?;
                    property.expression.evaluate(Some(zoom), None).ok()
                })
                .and_then(|value| value.as_number())
                .unwrap_or(16.0);
            #[allow(clippy::cast_possible_truncation)]
            let size = size as f32;

            let entries: Vec<ubo::SymbolDrawableEntry> = matrices(0)
                .filter_map(|tile| {
                    ubo::SymbolDrawableEntry::for_tile(
                        view,
                        tile.z,
                        tile.x,
                        tile.y,
                        i32::from(tile.wrap),
                        layer_index,
                        0,
                        atlas_size,
                        [0.0, 0.0],
                        size,
                        alignments,
                        placement,
                    )
                    .ok()
                })
                .collect();
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
