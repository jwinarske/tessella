//! A running map: the loop that turns a camera into frames.
//!
//! # What was missing
//!
//! Every piece of a frame existed and nothing composed them. `frame::emit_incremental` sends
//! what a consumer does not already have, `DamageTracker` says whether anything changed,
//! `cover` says which tiles a camera wants, the cache and the pool build them — and the only
//! code that put those together was thirteen test files, each by hand. So tessella could emit a
//! frame and had never driven a *sequence* of them.
//!
//! This is that loop, and it lives here rather than in a binding layer because everything it
//! composes lives here. A consumer's job is to draw what arrives, not to know the order in which
//! a producer decides things.
//!
//! # What a tick costs when nothing happened
//!
//! Nothing. That is the whole design and it is worth stating as a cost rather than a feature:
//!
//! - The camera has not moved and no tile landed: [`DamageTracker::begin_frame`] reports idle
//!   and the tick returns without touching the cover, the cache, the arena or the ring. §9.3's
//!   claim that traffic is proportional to change is this line.
//! - The camera moved: the cover is recomputed, but a tile already built is *found*, not rebuilt
//!   — that is what the cache is — and a tile still in view keeps the geometry id it had, so its
//!   bytes are not re-announced. Only the tiles that entered are new records, and the ones that
//!   left become releases.
//! - A tile landed: its buckets are encoded and announced. Nothing else in the frame moves.
//!
//! The dirty flag is consumed rather than sticky, which `damage.rs` calls the difference between
//! a frame's worth of traffic and the `AttributesModified` storm §6.1 names as a visible bug.
//!
//! # Why the session and the arena outlive the tick
//!
//! Because that is what makes the emission incremental at all. The registry gives a drawable an
//! id that survives a pan, and the arena holds a retained geometry's bytes until a
//! `GeometryRemove` says they can go. Handing either a fresh one per frame reduces
//! `emit_incremental` to `emit` with more steps, which the function's own documentation says.

use alloc::sync::Arc;
use alloc::vec::Vec;

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::Producer;
use tessella_glyph::fonts::Fonts;
use tessella_glyph::sprite::IconPosition;
use tessella_style::Style;
use tessella_style::crossfade::ZoomHistory;
use tessella_style::light::Light;
use tessella_tile::cover::{TileCoord, ViewTransform};
use tessella_tile::renderables::{DataTileId, Necessity, Pyramid, RenderTileId, TileState};

use crate::SlabArena;
use crate::damage::DamageTracker;
use crate::frame::{self, Emitted, Frame, FrameError, Patterns};
use crate::registry::Session;
use crate::tile::{Content, LayerBucket, TileId};
use crate::viewcover::{Update, ViewCover};

/// A packed sprite sheet, as the frame needs to see it.
///
/// The pieces rather than the store. `tessella_glyph::sprite::Sprites` is behind the `image`
/// feature because it decodes a PNG, and a frame loop has no business depending on an image
/// decoder — a caller that already has a packed atlas, from a cache or an offline bundle, should
/// not have to reconstitute one to hand it over.
#[derive(Debug, Clone)]
pub struct SpriteAtlas {
    /// The texture id the atlas was uploaded as.
    pub texture: tessella_capture_abi::envelope::TextureId,
    /// Its dimensions, which the shader turns a rectangle into texture coordinates with.
    pub size: [u16; 2],
    /// Where each sprite was packed, by name.
    pub positions: alloc::collections::BTreeMap<alloc::string::String, IconPosition>,
    /// The pixels, RGBA. Uploaded before any drawable names the texture.
    pub pixels: Vec<u8>,
}

/// What one tick did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Nothing had changed, so nothing was sent.
    ///
    /// Not a failure and not a frame skipped under load: the map is settled and a consumer
    /// redraws what it already holds. §13.1's invariant is that this is the common case.
    Idle,
    /// A frame was emitted.
    Emitted(Emitted),
}

/// Where a tick's tiles come from.
///
/// A trait rather than a concrete cache because the map does not care *how* a tile is found —
/// only whether it is ready. A caller with a warm cache answers from memory; one with a pool
/// answers for what has landed and reports the rest as absent, and the map emits what it has.
///
/// Absent is not an error. A map that refused to draw until every tile arrived would show
/// nothing during a pan into new ground, and §13.1 forbids exactly that.
pub trait Tiles {
    /// The buckets for a tile, if they are built.
    ///
    /// Called once per cover entry per frame *that emits*, and never on an idle tick.
    fn buckets(&self, tile: TileId) -> Option<Arc<Vec<LayerBucket>>>;

    /// The tile actually serving `cover`, and its buckets.
    ///
    /// Defaults to a coordinate serving itself, which is right for any source that can produce
    /// the zoom asked for. A source with a maxzoom cannot, above it, and answers with the coarser
    /// tile standing in -- whose local frame is the one the geometry is really in, and so the one
    /// that has to place it.
    fn serving(&self, cover: TileId) -> Option<(TileId, Arc<Vec<LayerBucket>>)> {
        self.buckets(cover).map(|buckets| (cover, buckets))
    }

    /// Layers that draw from no source — a background — for a tile of the cover.
    fn sourceless(&self, tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
        let _ = tile;
        None
    }

    /// Zooms this store holds tiles at that the view's own cover does not address.
    ///
    /// A 256-pixel raster source covers the screen at one zoom *more* than a vector one --
    /// mbgl's `coveringZoomLevel` shifts by `log2(512 / tileSize)` -- so its tiles are at
    /// coordinates the frame's cover never asks about. They were fetched, decoded and stored,
    /// and then nothing looked them up: the layer drew nothing at all whenever a vector source
    /// sat beside it, and drew normally when it was alone and the cover happened to be walked at
    /// its zoom.
    ///
    /// Empty for a store whose tiles are all at the view's zoom, which is every vector one.
    fn extra_zooms(&self, view: &ViewTransform) -> alloc::vec::Vec<u8> {
        let _ = view;
        alloc::vec::Vec::new()
    }
}

/// A map being drawn: one style, one view, and the state that makes a frame incremental.
///
/// Not `Send`: it owns a `Producer`, which is one end of a single-producer ring. A second thread
/// writing to it would be the one thing the ring's lock-free discipline does not survive.
pub struct Map {
    style: Style,
    view: ViewTransform,
    view_id: ViewId,
    light: Light,
    /// Survives every tick, which is what makes an id belong to a drawable rather than to a
    /// frame.
    session: Session,
    /// Holds a retained geometry's bytes until it is removed.
    arena: SlabArena,
    /// Symbol layout, kept between the frames that draw it. See [`frame::SymbolCache`].
    layouts: frame::SymbolCache,
    damage: DamageTracker,
    /// The per-view cover, with the zoom latch and the entered/left deltas.
    ///
    /// Not `cover::cover` per frame. The cover itself is cheap — 0.10 µs for a nine-tile
    /// viewport — and what it gates is not: retaining and releasing against the shared store,
    /// rebuilt bindings, and the damage that follows. §12.7.
    cover: Option<ViewCover>,
    /// The tiles to draw this frame, after substitution. Kept so a tick that changes nothing
    /// does not rebuild it.
    drawn: Vec<TileCoord>,
    /// Tiles the cover wants that are not built. What a fetch loop would ask for.
    wanted: Vec<TileCoord>,
    /// Ideal tiles the last frame left as holes — nothing at any resolution over them.
    uncovered: usize,
    /// How many ancestor levels to ask for alongside the ideal cover.
    prefetch: u8,
    /// The zoom the previous frame drew at, for the prefetch's velocity.
    ///
    /// `None` until a frame has been drawn: the first has no previous to differ from, and
    /// guessing a velocity for it would deepen the very fetch a cold start can least afford.
    last_zoom: Option<f64>,
    /// The part of [`Self::wanted`] that is speculation rather than cover.
    ///
    /// Ancestors asked for because the camera is heading their way. Correct to starve: a tile
    /// the frame is drawing now outranks one it might draw in half a second, and without the
    /// distinction a fast zoom fills the pool with levels it has already left.
    speculative: Vec<TileCoord>,
    /// Levels of zoom crossed since the previous frame. Negative is zooming out.
    zoom_velocity: f64,
    /// Glyphs, once a caller has fetched them.
    ///
    /// Owned rather than borrowed because a map outlives any one frame and the glyph set grows
    /// as new labels come into view — a borrow would tie the map's lifetime to whichever fetch
    /// happened to be first.
    fonts: Option<Fonts>,
    /// The sprite atlas, once a caller has one. Patterns *and* icons: one sheet serves both.
    sprites: Option<SpriteAtlas>,
    /// Which way the camera last crossed an integer zoom.
    ///
    /// Kept by the map because it is a property of the camera's *path*, not of any frame: a
    /// pattern's crossfade chooses its `from` image by which direction the zoom was crossed, and
    /// a frame that recomputed it from the current zoom alone could not tell a zoom-in from a
    /// zoom-out that landed on the same number.
    zoom: ZoomHistory,
}

impl Map {
    /// A map at a camera, with nothing emitted yet.
    ///
    /// The arena owns its slabs, so the geometry has to be copied out before a consumer that
    /// does not share this address space can read it. [`Self::with_arena`] over a region is the
    /// path that does not.
    #[must_use]
    pub fn new(style: Style, view: ViewTransform, view_id: ViewId) -> Self {
        Self::with_arena(style, view, view_id, SlabArena::new())
    }

    /// A map whose geometry is allocated out of `arena`.
    ///
    /// The point of it is [`SlabArena::in_region`]: an arena over a mapping writes the bytes
    /// where the consumer already reads them, so there is no pack step and no copy. With an
    /// owned arena every frame has to serialise the whole arena again, which is the whole of
    /// [`SlabArena::pack`] and, on a moving map, most of the frame.
    #[must_use]
    pub fn with_arena(
        style: Style,
        view: ViewTransform,
        view_id: ViewId,
        arena: SlabArena,
    ) -> Self {
        Self {
            style,
            view,
            view_id,
            light: Light::default(),
            session: Session::new(),
            arena,
            layouts: frame::SymbolCache::default(),
            damage: DamageTracker::new(),
            cover: None,
            drawn: Vec::new(),
            wanted: Vec::new(),
            uncovered: 0,
            prefetch: DEFAULT_PREFETCH,
            last_zoom: None,
            speculative: Vec::new(),
            zoom_velocity: 0.0,
            fonts: None,
            sprites: None,
            zoom: ZoomHistory::new(),
        }
    }

    /// Hands the map the glyphs its symbol layers need.
    ///
    /// Set rather than fetched here, for the reason `Frame::fonts` gives: which glyphs a style
    /// wants is discovered by evaluating `text-field` against a tile's own features, so it is a
    /// round trip the caller has already had to make. A map without them draws no labels, which
    /// is a legitimate frame rather than an error.
    pub fn set_fonts(&mut self, fonts: Fonts) {
        self.fonts = Some(fonts);
        // Everything laid out so far was laid out against the fonts this replaces, and a label
        // shaped without its glyphs is a label with holes in it.
        self.layouts.invalidate();
        // Not `session.forget(view_id)`, which would be the direct way to re-tell the consumer
        // everything against the new atlas: it makes the next frame re-announce geometry the
        // consumer still holds, and the retire path then frees a texture something is still
        // using -- "Handle (Texture) is being used after it has been freed", on the all-families
        // scene, immediately. The staleness this would close is measured instead, by
        // `FilamentRenderer::atlasMismatched`, and worked around where it does harm: the
        // consumer takes the atlas size from the texture it has bound rather than from the
        // drawable that named it.
        self.mark_dirty();
    }

    /// Hands the map the sprite atlas its patterns and icons draw from.
    pub fn set_sprites(&mut self, sprites: SpriteAtlas) {
        self.sprites = Some(sprites);
        // An icon laid out before the sheet arrived has no rectangle to sample.
        self.layouts.invalidate();
        self.mark_dirty();
    }

    /// Moves the camera.
    ///
    /// Does not emit. Whether the move is worth a frame is the tick's question, and answering it
    /// here would mean answering it again when a tile lands in the same frame.
    pub fn look_at(&mut self, view: ViewTransform) {
        self.view = view;
    }

    /// The camera as it stands.
    #[must_use]
    pub const fn view(&self) -> &ViewTransform {
        &self.view
    }

    /// Reports that a source has new tiles, so the next tick emits.
    ///
    /// Called by whatever owns the fetching. The map does not poll: a tick that asked every
    /// source whether anything had landed would do work proportional to the sources rather than
    /// to the change, which is the thing this design refuses.
    pub fn mark_dirty(&mut self) {
        self.damage.mark_dirty(self.view_id);
    }

    /// Tiles the cover wants that no source has built.
    ///
    /// The fetch list, and the map's whole part in fetching: it says what is missing and does not
    /// go and get it. What fetches is above this — a map that issued its own requests would need
    /// to own the network, the cache and the priority, and §5.5 puts all three outside a view.
    #[must_use]
    pub fn wanted(&self) -> &[TileCoord] {
        &self.wanted
    }

    /// How deep to ask, given how fast the camera is zooming.
    ///
    /// Only when zooming *out*, because only that direction arrives somewhere it has nothing for.
    /// Zooming in, the level being left is the ancestor of the level being reached, so
    /// substitution draws it -- coarse, but there. Zooming out, what is held is a *descendant* of
    /// what is wanted, and there is no substitution from below: the screen is empty until the
    /// coarse tile lands.
    ///
    /// Measured on the quad's sweep, which crosses thirteen levels in five seconds: the resting
    /// four levels are a third of a second of warning, and the first pass out ran out of tiles
    /// below zoom 1.2 while the second, from cache, did not.
    fn prefetch_levels(&self) -> u8 {
        if self.zoom_velocity >= 0.0 {
            return self.prefetch;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ahead = (-self.zoom_velocity * PREFETCH_LEAD_FRAMES).ceil().min(255.0) as u8;
        self.prefetch.saturating_add(ahead).min(MAX_PREFETCH)
    }

    /// Which of [`Self::wanted`] are speculation, for a caller that can rank its fetches.
    ///
    /// A subset, not a separate list: everything here is also wanted. What it says is that these
    /// are the levels the camera is *heading* for rather than the one it is on, so a source with
    /// a queue should serve them last.
    #[must_use]
    pub fn speculative(&self) -> &[TileCoord] {
        &self.speculative
    }

    /// How many ancestor levels to fetch alongside the ideal cover.
    ///
    /// Zero asks only for what the cover names, which is a map with no prefetch at all.
    ///
    /// This is the onion, and it is not mbgl's prefetch. mbgl covers a second time at
    /// `zoom - prefetchZoomDelta` and requests that whole cover; this asks for the *ancestors of
    /// the tiles it is missing*, which is a different set and a much smaller one — four siblings
    /// share a parent, so a cover of nine collapses to one or two per level, and the coarsest
    /// level is very often a single tile covering the entire viewport.
    pub const fn set_prefetch(&mut self, levels: u8) {
        self.prefetch = levels;
    }

    /// How much of the last frame was a hole.
    ///
    /// Ideal tiles with nothing drawn over them at any resolution. Zero is §12.10's *legible*
    /// frame, and it is a different question from [`Self::wanted`]: a map can want every tile it
    /// asked for and still be perfectly legible, drawn entirely from ancestors. The two together
    /// are what a prefetch is judged on — how fast this reaches zero, and how much was fetched
    /// to get there.
    #[must_use]
    pub const fn uncovered(&self) -> usize {
        self.uncovered
    }

    /// The style being drawn.
    #[must_use]
    pub const fn style(&self) -> &Style {
        &self.style
    }

    /// The arena holding retained geometry. A consumer's acknowledgement is checked against it.
    #[must_use]
    pub const fn arena(&self) -> &SlabArena {
        &self.arena
    }

    /// Emits a frame, if anything changed.
    ///
    /// # Errors
    ///
    /// [`FrameError`] as `emit_incremental` reports it. A frame that fails retires nothing and
    /// retains nothing, so a retry sees the state the failed attempt started from — which is
    /// what makes `RingFull` a "drain and call again" rather than a lost frame.
    pub fn tick<T: Tiles>(
        &mut self,
        producer: &mut Producer,
        tiles: &T,
    ) -> Result<Tick, FrameError> {
        let key = crate::frame::camera_key_of(&self.view);
        let work = self.damage.begin_frame(self.view_id, key);
        if work.is_idle() {
            return Ok(Tick::Idle);
        }

        // Before the frame allocates, and only when the region has enough dead space under the
        // cursor to be worth the memmove. A moving map sweeps from the bottom and allocates at
        // the top, so without this the cursor climbs for as long as the camera does and no
        // region is large enough. See `SlabArena::compact_region`.
        if self.arena.region_waste() >= COMPACTION_FLOOR
            && self.arena.region_waste() >= self.arena.region_used() / 2
        {
            self.arena.compact_region();
        }

        // The cover is recomputed every frame and its *change* is what gates the rest. §12.7's
        // measurement is why round that way: the cover costs 0.10 µs and predicting when it
        // moves costs more than it saves, while what it gates — substitution, retain, release,
        // rebuilt bindings — is where the frame's money goes.
        let moved = match &mut self.cover {
            Some(cover) => cover.update(&self.view).unwrap_or(Update::Unchanged),
            None => {
                self.cover = ViewCover::new(&self.view).ok();
                Update::Changed
            }
        };
        let Some(cover) = self.cover.as_ref() else {
            return Ok(Tick::Idle);
        };
        let velocity = self.last_zoom.map_or(0.0, |last| self.view.zoom - last);
        self.last_zoom = Some(self.view.zoom);
        self.zoom_velocity = velocity;

        // Substitution runs when the cover moved *or* when a tile landed: a tile arriving turns a
        // stand-in ancestor into the real thing at the same cover, which is precisely the case a
        // cover-only gate would miss and the one that leaves a map permanently blurry.
        if moved == Update::Changed || work.geometry || self.drawn.is_empty() {
            let mut pass = Substitution {
                tiles,
                drawn: Vec::new(),
                wanted: Vec::new(),
                uncovered: 0,
            };
            cover.draw(&mut pass, 0..=tessella_tile::cover::MAX_ZOOM);
            self.drawn = pass.drawn;
            self.uncovered = pass.uncovered;
            // Ancestors already held are not wanted. `onion` builds the chain without knowing
            // what exists — it has the addresses and not the store — so the filter is here,
            // where the source is. Without it a zoomed-in map re-asks every frame for the coarse
            // levels it is already drawing from.
            let ideal: alloc::collections::BTreeSet<TileCoord> =
                pass.wanted.iter().copied().collect();
            self.wanted = onion(&pass.wanted, self.prefetch_levels())
                .into_iter()
                .filter(|tile| tiles.buckets(TileId::new(tile.z, tile.x, tile.y)).is_none())
                .collect();
            self.speculative = self
                .wanted
                .iter()
                .filter(|tile| !ideal.contains(tile))
                .copied()
                .collect();
        }

        // Found, not built. A tile the source already holds costs a lookup, and one that has not
        // arrived is left out rather than waited for — a map that blocked on the slowest tile
        // would stall the whole frame for ground nobody has looked at yet. What fills the hole in
        // the meantime is the substitution above, not a wait.
        let mut buckets: Vec<(TileId, Vec<LayerBucket>)> = Vec::with_capacity(self.drawn.len());
        // The cover entry each bucket set is drawn for, kept alongside because the frame reads the
        // two by index. Built here rather than reusing `self.drawn` because above a source's
        // maxzoom they are not the same list -- several cover entries share one tile, and the
        // tile is drawn once.
        let mut placed: Vec<TileCoord> = Vec::with_capacity(self.drawn.len());
        // The store's own list each entry came from, for the symbol layout cache to key on.
        // `None` where the frame built the list itself and there is no identity to key on.
        let mut origins: Vec<Option<Arc<Vec<LayerBucket>>>> = Vec::with_capacity(self.drawn.len());
        // Keyed by the world copy as well as the tile. A `TileId` is canonical -- it carries no
        // wrap -- so at low zoom every copy of the world is the same key, and deduping on it
        // alone drew one copy and dropped the rest. At zoom 0 a 1280-pixel viewport holds two
        // and a half worlds and drew half of one: the rest of the screen was bare background.
        //
        // What the dedup is for is a *coarser* tile standing in for several cover coordinates,
        // which is one tile drawn once. Two copies of the world are two draws of one tile under
        // two matrices, which is a different thing and the reason `wrap` exists.
        let mut served: alloc::collections::BTreeSet<(TileId, i32)> =
            alloc::collections::BTreeSet::new();
        for entry in &self.drawn {
            let cover = TileId::new(entry.z, entry.x, entry.y);
            // What is standing in for this coordinate, which above a maxzoom is a coarser tile.
            // Its own coordinate is what the rest of the frame uses: the geometry is in *its*
            // local frame, so it is what places it, clips it, and identifies it. Drawing a z14
            // tile as though it were the z16 tile that asked for it shrinks it to a sixteenth and
            // puts sixteen times too much world on screen.
            let holding = tiles.serving(cover);
            let id = holding.as_ref().map_or(cover, |(id, _)| *id);
            // One z14 tile answers all sixteen z16 coordinates inside it. Drawn once: a second
            // draw is the same geometry under the same matrix, which blends twice and darkens
            // every translucent fill it touches.
            if !served.insert((id, entry.wrap)) {
                continue;
            }
            let mut built: Vec<LayerBucket> = Vec::new();
            if let Some((_, ready)) = &holding {
                built.extend(ready.iter().cloned());
            }
            if !built.is_empty() {
                built.sort_by_key(|bucket| bucket.layer_index);
                buckets.push((id, built));
                origins.push(holding.as_ref().map(|(_, ready)| Arc::clone(ready)));
                placed.push(TileCoord {
                    z: id.z,
                    x: id.x,
                    y: id.y,
                    wrap: entry.wrap,
                });
            }
        }

        // And the tiles a source holds at a zoom of its own.
        //
        // A 256-pixel raster source covers the screen at one zoom more than a vector one, so its
        // tiles sit at coordinates the walk above never asks about. They were fetched, decoded and
        // stored, and nothing looked them up: the layer drew nothing whenever a vector source sat
        // beside it, and drew normally when it was alone -- which is what made it look like an
        // interaction between sources rather than a cover that addresses one zoom.
        //
        // `served` carries over, so a tile already drawn by the walk above is not drawn twice; a
        // second draw is the same geometry under the same matrix, blended again.
        for z in tiles.extra_zooms(&self.view) {
            let Ok(extra) = tessella_tile::cover::cover_at(&self.view, z) else {
                continue;
            };
            for entry in &extra {
                let cover = TileId::new(entry.z, entry.x, entry.y);
                let Some((id, ready)) = tiles.serving(cover) else {
                    continue;
                };
                if !served.insert((id, entry.wrap)) {
                    continue;
                }
                // The raster buckets alone, which is the whole reason this walk exists.
                //
                // Taking every bucket on the tile draws the vector layers a second time. These
                // tiles are at the raster's zoom, not the view's, so `served` does not dedupe
                // them against the walk above -- they are different tiles -- and a z16 cover
                // holds four tiles for every z15 one. Water, roads and buildings were each drawn
                // at both zooms and composited over themselves: with the raster layer in the
                // style the frame carried 128 water drawables where it should carry 20, and 256
                // background where it should carry 40. What that looks like is the imagery
                // washing out everything under it, which is how it was first described.
                let mut built: Vec<LayerBucket> = ready
                    .iter()
                    .filter(|bucket| matches!(bucket.content, Content::Raster(_)))
                    .cloned()
                    .collect();
                if built.is_empty() {
                    continue;
                }
                built.sort_by_key(|bucket| bucket.layer_index);
                buckets.push((id, built));
                // Filtered to the raster buckets, so it is not the store's list -- and a raster
                // layer has no symbols to lay out.
                origins.push(None);
                placed.push(TileCoord {
                    z: id.z,
                    x: id.x,
                    y: id.y,
                    wrap: entry.wrap,
                });
            }
        }

        // And the background, whose tiles are the view's own cover rather than any source's.
        //
        // mbgl says this in as many words -- `renderTiles is always empty, we use tileCover
        // instead` -- and computes `util::tileCover` at the integer zoom for this layer alone.
        // Taking the background off whatever tiles a source happened to serve was wrong twice
        // over. A style with no vector source has nothing renderable, so substitution records no
        // coordinates and no background was drawn at all: the frame came out the clear colour,
        // which is black, and a raster-only basemap is exactly that style. And where a source
        // *was* present but an ancestor stood in for a missing tile, the background went onto the
        // ancestor's coordinate and so covered four or sixteen times the ground it should.
        //
        // Deduped against `served` like the walks above, because a background keyed to a cover
        // coordinate a walk already placed would blend over itself.
        //
        // Unless the oracle would not draw it at all. A solid first-layer background is mbgl's
        // clear colour, and a clear covers the whole renderable rather than the cover: see
        // `tile::background_covers_viewport`. One drawable stands in for it, on a fixed
        // coordinate so the registry keeps it across a pan -- the quad is the viewport and does
        // not move with the camera, so nothing about it is per tile.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let integer_zoom = self.view.zoom.floor().max(0.0) as u8;
        if crate::tile::background_covers_viewport(&self.style, self.view.zoom) {
            let anchor = TileId::new(0, 0, 0);
            let built: Vec<LayerBucket> = crate::tile::build_sourceless(&self.style, anchor)
                .unwrap_or_default()
                .into_iter()
                .filter(|bucket| matches!(bucket.content, Content::Background))
                .collect();
            if !built.is_empty() {
                buckets.push((anchor, built));
                origins.push(None);
                placed.push(TileCoord {
                    z: anchor.z,
                    x: anchor.x,
                    y: anchor.y,
                    wrap: 0,
                });
            }
        } else if let Ok(background) = tessella_tile::cover::cover_at(&self.view, integer_zoom) {
            for entry in &background {
                let cover = TileId::new(entry.z, entry.x, entry.y);
                // Built here when the store has not got to it, because a background is a
                // function of the style and the coordinate and of nothing else.
                //
                // The store fills `sourceless` for whatever coordinates the planner visited, on
                // the planning thread. Depending on that made a raster-only style render a black
                // frame at random: the planner had not filled the view's coordinates yet, or had
                // filled the raster's own zoom instead, and the frame found nothing to draw. A
                // background that has to wait for a planner is the one layer that never should --
                // it is what a map shows *before* anything has arrived.
                // An *empty* stored entry is not an answer either. The store's sourceless map is
                // a cache of `build_sourceless`, not an authority over it: `TileSource` inserts
                // whatever the build returned for the coordinates its planner visited, and a
                // coordinate the planner reached before the style had anything to say about it
                // is stored empty. Taking that as the answer is a frame with no drawables at
                // all -- and the consumer rebuilds its scene from the frame's order, so it is a
                // screen that goes black. On the quad's zoom sweep, 777 of 930 emitted frames.
                let built: Vec<LayerBucket> = match tiles.sourceless(cover) {
                    Some(held) if !held.is_empty() => held.iter().cloned().collect(),
                    _ => crate::tile::build_sourceless(&self.style, cover).unwrap_or_default(),
                };
                if built.is_empty() {
                    continue;
                }
                buckets.push((cover, built));
                // A background carries no symbols, and half the time this list was built here.
                origins.push(None);
                placed.push(TileCoord {
                    z: cover.z,
                    x: cover.x,
                    y: cover.y,
                    wrap: entry.wrap,
                });
            }
        }

        // Updated before the frame reads it, so a pattern crossing an integer zoom this tick
        // fades from the image it was actually showing rather than from the one it is arriving
        // at. `update` reports whether a crossing happened, which nothing here needs — the
        // history itself carries the direction.
        self.zoom.update(self.view.zoom, None);

        // Borrowed out of the map for the call. `Patterns` holds references, so it cannot be a
        // field: building it here is what keeps the atlas owned by the map and the frame's view
        // of it borrowed.
        let patterns = self.sprites.as_ref().map(|sprites| Patterns {
            texture: sprites.texture,
            size: sprites.size,
            positions: &sprites.positions,
            pixels: &sprites.pixels,
            history: self.zoom,
        });

        self.layouts.begin_frame();
        let emitted = frame::emit_incremental(
            producer,
            &mut self.arena,
            &mut self.layouts,
            &Frame {
                style: &self.style,
                view: &self.view,
                view_id: self.view_id,
                tiles: &placed,
                buckets: &buckets,
                origins: &origins,
                light: &self.light,
                fonts: self.fonts.as_ref(),
                patterns: patterns.as_ref(),
            },
            &mut self.session,
        )?;
        Ok(Tick::Emitted(emitted))
    }
}

/// How many ancestor levels are asked for alongside the ideal cover by default.
///
/// Four, which is mbgl's `prefetchZoomDelta`. Matching the number is deliberate: the *set*
/// differs enough that the depth should not, or a comparison would be measuring how far each
/// reaches rather than which set it asks for.
const DEFAULT_PREFETCH: u8 = 4;

/// The deepest onion a moving camera asks for.
///
/// Eight, which is four levels of speculation beyond the resting depth. Each level up is a
/// quarter of the tiles of the one below, so the cost of the extra four is a fraction of the
/// cover itself -- the reason to bound it at all is that a level nine steps away is a different
/// map, not a preview of this one.
const MAX_PREFETCH: u8 = 8;

/// How many frames of runway the prefetch aims to keep.
///
/// Thirty, half a second at sixty. That is the span a tile has to arrive in for a camera not to
/// reach its level empty-handed, and it is what turns a depth in *levels* into a depth in time:
/// the same four levels are a second and a half of warning at a gentle zoom and a third of a
/// second at a sweep's rate.
const PREFETCH_LEAD_FRAMES: f64 = 30.0;


/// Dead region bytes below which compaction is not worth the memmove.
///
/// Four mebibytes. Under this the copy costs more than the space is worth, and the trigger also
/// wants the waste to be at least half the region in use -- so a map that is merely large does
/// not compact, and one that is churning does.
const COMPACTION_FLOOR: usize = 4 << 20;

/// The ideal misses, with their ancestors, coarsest first.
///
/// # Why ancestors of the misses rather than a second cover
///
/// A cover at `zoom - 4` is computed from the camera and names every tile at that level the
/// viewport touches. The ancestors of the *missing* tiles are a subset of that and usually a tiny
/// one: siblings share a parent, so nine ideal tiles have at most nine ancestors per level and in
/// practice one or two — and four levels up, very often exactly one, the tile that covers the
/// whole viewport by itself.
///
/// Asking for what is missing also means asking for nothing when nothing is missing: a settled
/// map prefetches no tiles at all, where a second cover would have to be computed and diffed to
/// discover the same.
///
/// # The order is the point
///
/// Coarsest first, because every real fetcher has a bounded queue and the order decides what
/// occupies it. Ideal-first spends the queue on detail tiles, each covering a ninth of the
/// screen; coarse-first spends the first slot on the tile that covers all of it. The same
/// requests and the same bytes — the difference is *when* the map becomes legible.
fn onion(ideal: &[TileCoord], levels: u8) -> Vec<TileCoord> {
    if levels == 0 || ideal.is_empty() {
        return ideal.to_vec();
    }

    // Deduplicated as they are built: four siblings collapse to one parent, and without this a
    // cover of a thousand tiles would ask for the same ancestor a thousand times.
    let mut seen: alloc::collections::BTreeSet<(u8, u32, u32, i32)> = ideal
        .iter()
        .map(|tile| (tile.z, tile.x, tile.y, tile.wrap))
        .collect();
    let mut out: Vec<TileCoord> = Vec::with_capacity(ideal.len() + usize::from(levels));

    // Coarsest first: `levels` steps up, then `levels - 1`, down to the ideal level itself.
    for step in (1..=levels).rev() {
        for tile in ideal {
            let Some(z) = tile.z.checked_sub(step) else {
                continue;
            };
            let ancestor = TileCoord {
                z,
                x: tile.x >> step,
                y: tile.y >> step,
                wrap: tile.wrap,
            };
            if seen.insert((ancestor.z, ancestor.x, ancestor.y, ancestor.wrap)) {
                out.push(ancestor);
            }
        }
    }
    out.extend_from_slice(ideal);
    out
}

/// The pass that turns an ideal cover into what can actually be drawn.
///
/// mbgl's `updateRenderables` asks four things of a pyramid — does this tile exist, create it,
/// retain it, draw it — and answers them against a store it owns. Here the store is the caller's
/// `Tiles`, so the pass is a borrow of it: `get` answers from what is built, `create` records
/// what is missing rather than starting a fetch, and `render` collects what to draw.
///
/// Creating without fetching is the split that matters. §5.5 puts the network, the cache and the
/// priority outside a view, so a map that issued its own requests would have to own all three.
/// It reports what it wants instead, and something above it decides what that is worth.
struct Substitution<'a, T: Tiles + ?Sized> {
    tiles: &'a T,
    /// What to draw, in the order the algorithm chose it. Duplicates are possible — one ancestor
    /// can stand in for several missing children — and are collapsed when it finishes.
    drawn: Vec<TileCoord>,
    /// What the cover wanted and did not have.
    wanted: Vec<TileCoord>,
    /// Ideal tiles left with nothing over them at any resolution.
    uncovered: usize,
}

impl<T: Tiles + ?Sized> Substitution<'_, T> {
    fn coord(id: DataTileId) -> TileCoord {
        TileCoord {
            z: id.z,
            x: id.x,
            y: id.y,
            wrap: i32::from(id.wrap),
        }
    }
}

impl<T: Tiles + ?Sized> Pyramid for Substitution<'_, T> {
    fn get(&mut self, id: DataTileId) -> Option<TileState> {
        // Renderable means the buckets are here. §13.2 asks that it eventually mean
        // consumer-*acknowledged* rather than merely built, which is where mbgl's single-frame
        // holes come from — it retains an ancestor until its descendants are built, and built is
        // not uploaded. The registry can answer that; wiring it is the next turn of this screw.
        self.tiles
            .buckets(TileId::new(id.z, id.x, id.y))
            .map(|_| TileState {
                renderable: true,
                ..TileState::default()
            })
    }

    fn create(&mut self, id: DataTileId) -> Option<TileState> {
        // Recorded, not fetched. A tile that does not exist yet is a request someone else makes.
        self.wanted.push(Self::coord(id));
        Some(TileState::default())
    }

    fn retain(&mut self, _id: DataTileId, _necessity: Necessity) {}

    fn uncovered(&mut self, _ideal: DataTileId) {
        self.uncovered += 1;
    }

    fn render(&mut self, _render: RenderTileId, data: DataTileId) {
        // The data tile's buckets, at the data tile's own position. A parent standing in for a
        // missing child is drawn where the parent is, covering the child's ground because it
        // contains it — so what reaches the frame is simply "draw this tile".
        let coord = Self::coord(data);
        if !self.drawn.contains(&coord) {
            self.drawn.push(coord);
        }
    }
}
