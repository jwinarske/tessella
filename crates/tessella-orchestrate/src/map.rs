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
use crate::tile::{LayerBucket, TileId};
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

    /// Layers that draw from no source — a background — for a tile of the cover.
    fn sourceless(&self, tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
        let _ = tile;
        None
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
    #[must_use]
    pub fn new(style: Style, view: ViewTransform, view_id: ViewId) -> Self {
        Self {
            style,
            view,
            view_id,
            light: Light::default(),
            session: Session::new(),
            arena: SlabArena::new(),
            damage: DamageTracker::new(),
            cover: None,
            drawn: Vec::new(),
            wanted: Vec::new(),
            uncovered: 0,
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
        self.mark_dirty();
    }

    /// Hands the map the sprite atlas its patterns and icons draw from.
    pub fn set_sprites(&mut self, sprites: SpriteAtlas) {
        self.sprites = Some(sprites);
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
            self.wanted = pass.wanted;
            self.uncovered = pass.uncovered;
        }

        // Found, not built. A tile the source already holds costs a lookup, and one that has not
        // arrived is left out rather than waited for — a map that blocked on the slowest tile
        // would stall the whole frame for ground nobody has looked at yet. What fills the hole in
        // the meantime is the substitution above, not a wait.
        let mut buckets: Vec<(TileId, Vec<LayerBucket>)> = Vec::with_capacity(self.drawn.len());
        for entry in &self.drawn {
            let id = TileId::new(entry.z, entry.x, entry.y);
            let mut built: Vec<LayerBucket> = Vec::new();
            if let Some(ready) = tiles.buckets(id) {
                built.extend(ready.iter().cloned());
            }
            if let Some(sourceless) = tiles.sourceless(id) {
                built.extend(sourceless.iter().cloned());
            }
            if !built.is_empty() {
                built.sort_by_key(|bucket| bucket.layer_index);
                buckets.push((id, built));
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

        let emitted = frame::emit_incremental(
            producer,
            &mut self.arena,
            &Frame {
                style: &self.style,
                view: &self.view,
                view_id: self.view_id,
                tiles: &self.drawn,
                buckets: &buckets,
                light: &self.light,
                fonts: self.fonts.as_ref(),
                patterns: patterns.as_ref(),
            },
            &mut self.session,
        )?;
        Ok(Tick::Emitted(emitted))
    }
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
