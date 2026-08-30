//! Which tiles actually get drawn when the ideal ones are not ready — §13.2's never-blank.
//!
//! A transcription of mbgl's `algorithm::updateRenderables`. The ideal cover says which tiles a
//! view *wants*; this says what to draw *now*, given that a tile takes a fetch, a decode and a
//! build to become drawable and a zoom crossing changes the whole cover at once.
//!
//! # Why substitution and not simply waiting
//!
//! At an integer crossing every tile in the cover is replaced by four of its children. If a view
//! drew only what it had, the crossing would blank the map for as long as the slowest child took
//! to arrive — over a real link, several hundred milliseconds of white. So an ideal tile that is
//! not ready falls back: to its children if *all four* are ready, otherwise to the nearest ready
//! ancestor, which is almost always the tile that was on screen a moment ago. The map goes
//! momentarily blurry instead of momentarily empty.
//!
//! # The order the fallbacks are tried is not arbitrary
//!
//! Children first, and only if *every* child is ready. A partial set of children would leave
//! holes, so a single missing child sends the whole tile to the parent search instead. Then
//! ancestors, nearest first, stopping at the first ready one. Both directions are bounded by the
//! source's zoom range, and the ascent additionally stops at any ancestor another ideal tile has
//! already searched through — four siblings share a parent, and without that check a cover of a
//! thousand tiles would walk the same ancestors a thousand times.
//!
//! # Necessity is not the same as retention
//!
//! Every tile visited is retained, but with a distinction that decides what may be *fetched*.
//! An ideal tile is [`Necessity::Required`]: go to the network for it. A substitute is
//! [`Necessity::Optional`]: use it if the cache already has it, but do not spend a request on a
//! tile that exists only to paper over a gap — the request would compete with the ideal tile
//! that would make it unnecessary. The exception is an ancestor searched *after* the children
//! are known unloadable, which is required: at that point it is the only thing that can be
//! drawn.
//!
//! # Where this build will diverge, and where it does not yet
//!
//! §13.2 says ancestors are held until every covering descendant is consumer-*acknowledged*,
//! where mbgl holds until *built* — the gap between building a bucket and the consumer having
//! uploaded it is exactly where mbgl's single-frame holes come from. That divergence lives
//! entirely in what [`TileState::renderable`] means, which is the caller's to define, so the
//! algorithm here is mbgl's unmodified and is checked against mbgl's own expectations.
//!
//! R4 made that definition available: `tessella-orchestrate`'s `GeometryRegistry::is_acknowledged`
//! answers it from the reverse channel's acked position. The algorithm did not change, but
//! [`TileState::loaded`] has to move with it, and the reason is written there.

use std::collections::BTreeSet;

/// A tile as the pyramid stores it: a canonical tile, plus the zoom it is standing in for.
///
/// `overscaled_z` exceeds `z` when a tile is drawn beyond its source's maximum zoom — the same
/// bytes magnified, which is why the pair and not the canonical id alone is the store's key.
/// mbgl's `OverscaledTileID`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DataTileId {
    /// The zoom this tile is being used at.
    pub overscaled_z: u8,
    /// How many worlds east or west of the primary one.
    pub wrap: i16,
    /// The tile's own zoom.
    pub z: u8,
    /// Column.
    pub x: u32,
    /// Row.
    pub y: u32,
}

impl DataTileId {
    /// A tile at its own zoom, in the primary world.
    #[must_use]
    pub const fn new(z: u8, x: u32, y: u32) -> Self {
        Self {
            overscaled_z: z,
            wrap: 0,
            z,
            x,
            y,
        }
    }

    /// A tile standing in at a zoom above its own.
    ///
    /// # Panics
    ///
    /// When `overscaled_z` is below the tile's own zoom, which is not overscaling but a
    /// different tile.
    #[must_use]
    pub const fn overscaled(overscaled_z: u8, wrap: i16, z: u8, x: u32, y: u32) -> Self {
        assert!(
            overscaled_z >= z,
            "overscaled_z is below the tile's own zoom"
        );
        Self {
            overscaled_z,
            wrap,
            z,
            x,
            y,
        }
    }

    /// The id this tile is drawn under: the canonical tile and its wrap, without the
    /// overscaling.
    ///
    /// mbgl's `toUnwrapped`. Two tiles that differ only in `overscaled_z` are drawn in the same
    /// place, which is what makes an overscaled child a usable substitute.
    #[must_use]
    pub const fn render_id(self) -> RenderTileId {
        RenderTileId {
            wrap: self.wrap,
            z: self.z,
            x: self.x,
            y: self.y,
        }
    }

    /// This tile as it would be at `zoom`.
    ///
    /// Above the tile's own zoom this only changes `overscaled_z` — the canonical tile is
    /// unchanged, because overscaling magnifies rather than subdivides. mbgl's `scaledTo`.
    #[must_use]
    pub const fn scaled_to(self, zoom: u8) -> Self {
        if zoom >= self.z {
            Self {
                overscaled_z: zoom,
                wrap: self.wrap,
                z: self.z,
                x: self.x,
                y: self.y,
            }
        } else {
            let shift = self.z - zoom;
            Self {
                overscaled_z: zoom,
                wrap: self.wrap,
                z: zoom,
                x: self.x >> shift,
                y: self.y >> shift,
            }
        }
    }

    /// The four canonical children of this tile's canonical id, at `overscaled_z`.
    #[must_use]
    fn children_at(self, overscaled_z: u8) -> [Self; 4] {
        let (z, x, y) = (self.z + 1, self.x * 2, self.y * 2);
        let child = |x, y| Self {
            overscaled_z,
            wrap: self.wrap,
            z,
            x,
            y,
        };
        [
            child(x, y),
            child(x, y + 1),
            child(x + 1, y),
            child(x + 1, y + 1),
        ]
    }

    /// Whether this tile lies within `parent`.
    ///
    /// mbgl's `OverscaledTileID::isChildOf`: the same world, strictly deeper, and either the
    /// same canonical tile — an overscaled stand-in — or canonically inside it.
    #[must_use]
    pub fn is_child_of(self, parent: Self) -> bool {
        if self.wrap != parent.wrap || self.overscaled_z <= parent.overscaled_z {
            return false;
        }
        if self.z == parent.z && self.x == parent.x && self.y == parent.y {
            return true;
        }
        // Guarded on zero first, as mbgl does, to avoid a shift by the full width.
        parent.z == 0
            || (parent.z < self.z
                && parent.x == self.x >> (self.z - parent.z)
                && parent.y == self.y >> (self.z - parent.z))
    }
}

/// Where a tile is drawn: the canonical id and its wrap, with no overscaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RenderTileId {
    /// How many worlds east or west of the primary one.
    pub wrap: i16,
    /// The tile's own zoom.
    pub z: u8,
    /// Column.
    pub x: u32,
    /// Row.
    pub y: u32,
}

/// Whether a tile may be fetched, or only used if already held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Necessity {
    /// The view needs this tile; go to the network for it.
    Required,
    /// A substitute. Use it if it is already there, but do not spend a request on it — the
    /// request would compete with the ideal tile that would make the substitute unnecessary.
    Optional,
}

/// The three things the algorithm asks about a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TileState {
    /// Its buckets can be drawn now.
    ///
    /// §13.2 asks that this mean consumer-*acknowledged* rather than merely built, and it is the
    /// caller's to define: `tessella-orchestrate`'s `GeometryRegistry::is_acknowledged` answers
    /// it from the position the consumer publishes on the reverse channel. mbgl retains an
    /// ancestor until its descendants are built, and the gap between built and uploaded is where
    /// its single-frame holes come from.
    pub renderable: bool,
    /// Nothing more is coming that would make this tile renderable.
    ///
    /// Distinct from `renderable`: a tile the origin answered 404 for is loaded and will never
    /// be renderable, and the ascent must not keep waiting on it.
    ///
    /// # It has to move with `renderable`
    ///
    /// The obvious reading is "loading has finished", and under `renderable = built` the two are
    /// the same moment so the difference never shows. Under `renderable = acknowledged` they are
    /// not: a tile whose buckets are built but not yet uploaded has finished *loading* and is
    /// still going to become drawable without anyone fetching anything.
    ///
    /// Calling that loaded tells the ascent below that the tile is finished and cannot be drawn,
    /// which is its cue to spend a `Necessity::Required` request on an ancestor — one per upload
    /// gap, at every crossing, for tiles no view covers. So a caller that defines `renderable` as
    /// acknowledged must define this as acknowledged-or-failed, not built-or-failed.
    pub loaded: bool,
    /// The cache has already been consulted for it.
    pub tried_cache: bool,
}

/// The pyramid the algorithm walks: lookups, creation, and the two outputs.
///
/// A trait rather than four closures because the implementations share state — mbgl passes four
/// lambdas over one map — and four `FnMut`s over one structure is a borrow-checker fight with no
/// upside.
pub trait Pyramid {
    /// The state of a tile, if the pyramid holds it.
    fn get(&mut self, id: DataTileId) -> Option<TileState>;

    /// Creates a tile and returns its state.
    ///
    /// `None` when the tile is outside the source's bounds, which is not a failure: a TileJSON
    /// `bounds` narrower than the world means tiles outside it are never created, and the
    /// algorithm skips them rather than treating them as pending.
    fn create(&mut self, id: DataTileId) -> Option<TileState>;

    /// Marks a tile as wanted for this frame, at the given necessity.
    fn retain(&mut self, id: DataTileId, necessity: Necessity);

    /// Reports that an ideal tile ended the walk with nothing drawn over it.
    ///
    /// A hole. Neither the tile, nor children covering it, nor any ancestor was renderable, so
    /// this patch of the viewport has nothing on it — which is what §13.2's never-blank rule
    /// forbids and what a prefetch exists to prevent.
    ///
    /// Reported rather than returned because the walk is a visitor: a caller wanting the count
    /// keeps one, and a caller that does not pays nothing. Defaulted empty, so an implementation
    /// that does not care is unchanged.
    ///
    /// The complement is what §12.10 calls a legible frame — every ideal tile covered at some
    /// resolution — and it is the difference between a map that is loading and one that is
    /// blank.
    fn uncovered(&mut self, ideal: DataTileId) {
        let _ = ideal;
    }

    /// Draws `data` in the place `render` names.
    ///
    /// The two ids differ for every substitution, which is the whole point: a parent drawn in
    /// place of a missing child is that parent's bytes at that parent's position, covering the
    /// child's ground because it contains it.
    fn render(&mut self, render: RenderTileId, data: DataTileId);
}

/// Decides what to draw for one source, one frame.
///
/// `ideal` is the cover. `prefetched` are tiles held from a lower zoom for exactly this purpose,
/// paired with their state because they come from a map the caller already holds rather than
/// from a lookup. `zooms` is the source's zoom range. `max_parent_overscale` caps how far the
/// ancestor search may climb, which a raster source uses to refuse a substitute so magnified it
/// reads as a blur rather than as a map.
///
/// # Panics
///
/// When an ideal tile is outside `zooms`, or its `overscaled_z` is below its own zoom — both of
/// which mean the cover and the source disagree, which is a caller error rather than a state to
/// recover from.
pub fn update_renderables<P: Pyramid + ?Sized>(
    pyramid: &mut P,
    ideal: &[DataTileId],
    prefetched: &[(DataTileId, TileState)],
    zooms: core::ops::RangeInclusive<u8>,
    max_parent_overscale: Option<u8>,
) {
    let (zoom_min, zoom_max) = (*zooms.start(), *zooms.end());
    // Ancestors already walked, shared across every ideal tile. Four siblings have one parent,
    // so without this a cover of a thousand tiles walks the same ancestry a thousand times —
    // and worse, retains it a thousand times.
    let mut checked: BTreeSet<DataTileId> = BTreeSet::new();
    // Ancestors this walk has actually drawn. Distinct from `checked`, which records ancestries
    // *visited*: an ancestry can be visited and cover nothing.
    //
    // Needed because the short-circuit below stops a second ideal tile from re-walking an
    // ancestry a sibling already walked — correct, and it loses the answer. The sibling's walk
    // may have rendered an ancestor, and that ancestor covers this tile too, since it contains
    // it. Without this the walk reports such a tile as a hole while it is plainly drawn.
    let mut rendered: BTreeSet<DataTileId> = BTreeSet::new();

    for &ideal_id in ideal {
        assert!(
            ideal_id.z >= zoom_min && ideal_id.z <= zoom_max,
            "an ideal tile outside the source's zoom range"
        );
        assert!(
            ideal_id.overscaled_z >= ideal_id.z,
            "an ideal tile overscaled below its own zoom"
        );

        let ideal_render_id = ideal_id.render_id();
        let state = match pyramid.get(ideal_id) {
            Some(state) => state,
            None => match pyramid.create(ideal_id) {
                Some(state) => state,
                // Outside the source's bounds. Not pending, not missing — absent by design.
                None => continue,
            },
        };

        if state.renderable {
            pyramid.retain(ideal_id, Necessity::Required);
            pyramid.render(ideal_render_id, ideal_id);
            continue;
        }

        // These follow the ascent, one level at a time: at each step they describe the tile
        // *below* the one being examined, which is what decides whether its parent may be
        // fetched or only looked up.
        let mut parent_tried_optional = state.tried_cache;
        let mut parent_is_loaded = state.loaded;

        // Retained even though it cannot be drawn: it is what the view actually wants, and
        // retention is what causes it to be fetched.
        pyramid.retain(ideal_id, Necessity::Required);

        let mut covered = true;
        let mut found = false;
        let child_z = i32::from(ideal_id.overscaled_z) + 1;

        if child_z > i32::from(zoom_max) {
            // Past the source's maximum zoom there are no real children, only the same tile
            // magnified one step further.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let child = ideal_id.scaled_to(child_z as u8);
            match pyramid.get(child) {
                Some(child_state) if child_state.renderable => {
                    pyramid.retain(child, Necessity::Optional);
                    pyramid.render(ideal_render_id, child);
                    found = true;
                }
                _ => covered = false,
            }
        } else {
            // All four, and every one must be renderable: three children and a hole is a hole.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            for child in ideal_id.children_at(child_z as u8) {
                match pyramid.get(child) {
                    Some(child_state) if child_state.renderable => {
                        pyramid.retain(child, Necessity::Optional);
                        pyramid.render(child.render_id(), child);
                        found = true;
                    }
                    _ => covered = false,
                }
            }
        }

        if covered {
            continue;
        }

        // The children do not cover it, so climb. Nearest ancestor first, stopping at the first
        // one that can be drawn.
        let mut level = i32::from(ideal_id.overscaled_z) - 1;
        while level >= i32::from(zoom_min) {
            if let Some(limit) = max_parent_overscale
                && i32::from(ideal_id.overscaled_z) - level > i32::from(limit)
            {
                break;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let parent = ideal_id.scaled_to(level as u8);

            // A sibling has already been up this chain; whatever it found, this tile would find
            // too.
            if !checked.insert(parent) {
                // Already walked for a sibling. If that walk drew this ancestor or a coarser
                // one, this tile is covered by it.
                let mut above = Some(parent);
                while let Some(id) = above {
                    if rendered.contains(&id) {
                        found = true;
                        break;
                    }
                    above = (id.overscaled_z > zoom_min).then(|| id.scaled_to(id.overscaled_z - 1));
                }
                break;
            }

            let mut parent_state = pyramid.get(parent);
            if parent_state.is_none() && (parent_tried_optional || parent_is_loaded) {
                // The tile below is finished, so this ancestor is worth bringing into being
                // rather than merely looked up.
                parent_state = pyramid.create(parent);
            }

            if let Some(parent_state) = parent_state {
                if parent_is_loaded {
                    // The child below is known unloadable, so this ancestor is now the only
                    // thing that can be drawn here and is worth a request.
                    pyramid.retain(parent, Necessity::Required);
                } else {
                    // The child may still arrive. Take this one from the cache if it is there,
                    // but do not race the tile that would make it unnecessary.
                    pyramid.retain(parent, Necessity::Optional);
                }

                parent_tried_optional = parent_state.tried_cache;
                parent_is_loaded = parent_state.loaded;

                if parent_state.renderable {
                    pyramid.render(parent.render_id(), parent);
                    rendered.insert(parent);
                    found = true;
                    break;
                }
            }

            level -= 1;
        }

        if !found {
            // Nothing above or below.
            pyramid.uncovered(ideal_id);
            // Anything prefetched that lies inside this tile is better than blank.
            for &(prefetched_id, prefetched_state) in prefetched {
                if prefetched_state.renderable
                    && prefetched_id.z <= zoom_max
                    && prefetched_id.is_child_of(ideal_id)
                {
                    pyramid.retain(prefetched_id, Necessity::Optional);
                    pyramid.render(prefetched_id.render_id(), prefetched_id);
                }
            }
        }
    }
}

/// What a run of [`update_renderables`] decided, for a caller that wants the answer rather than
/// the callbacks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Renderables {
    /// Every tile to draw, in the order the algorithm produced it, as (where, what).
    pub render: Vec<(RenderTileId, DataTileId)>,
    /// Every tile to keep, with the necessity that decides whether it may be fetched.
    pub retain: Vec<(DataTileId, Necessity)>,
}
