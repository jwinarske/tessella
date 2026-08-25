//! The per-view cover state §5.2 calls irreducible, and the thing that owns the zoom latch.
//!
//! Two pieces have been sitting unwired for the same stated reason — `cover::ZoomLatch` and
//! `renderables::update_renderables` are both values a caller has to keep between frames, and
//! §13.2 recorded that which caller keeps them is the same question as where per-view cover
//! state lives. This is the answer: one object per view, held by the orchestrator, which §5.4's
//! single pass over views walks.
//!
//! # Recomputation is not what §12.7 is trying to avoid
//!
//! §12.7 asks that cover recompute only on crossing a tile boundary or an integer-zoom
//! threshold, which reads as an instruction to predict crossings and skip the computation. It is
//! not worth doing: measured, `cover()` is **0.10 µs** for a nine-tile z14 viewport and 0.12 µs
//! for twelve tiles at z16 — four views at sixty frames a second spend twenty-four microseconds
//! per *second* on it. A boundary predictor to save that would cost more than it saves and add a
//! way to be wrong about what is on screen.
//!
//! What is expensive is everything downstream: retaining and releasing against the shared store,
//! rebuilding bindings, and the damage that follows. So the cover is recomputed every frame and
//! the *change* is what gates the rest — [`Update::Unchanged`] means no work below this line,
//! and it is the common case by a wide margin, since a cover is constant across a whole integer
//! level of zoom and across all the panning that does not leave the current tiles.
//!
//! # The latch and the tile zoom are deliberately two different things
//!
//! `ViewTransform::tile_zoom` is a pure function of a camera: the oracle parity, the tile keys
//! and every matrix depend on it staying one. The latch has memory — it holds the level it is
//! on until the camera leaves a dead band either side. Pinching around an integer zoom must not
//! rebuild the cover at gesture rate, and that requires remembering which level is currently
//! held, which a pure function cannot.

use alloc::vec::Vec;
use std::collections::BTreeSet;

use tessella_tile::cover::{self, CoverError, TileCoord, ViewTransform, ZoomLatch};
use tessella_tile::renderables::{self, DataTileId, Pyramid};

/// Whether a frame's cover differs from the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// The same tiles as last frame. Nothing below this needs doing.
    Unchanged,
    /// The cover moved. [`ViewCover::entered`] and [`ViewCover::left`] say how.
    Changed,
}

/// One view's cover, across frames.
#[derive(Debug, Clone)]
pub struct ViewCover {
    latch: ZoomLatch,
    tiles: Vec<TileCoord>,
    entered: Vec<TileCoord>,
    left: Vec<TileCoord>,
    changes: u64,
    frames: u64,
}

impl ViewCover {
    /// The cover a view starts with.
    ///
    /// # Errors
    ///
    /// [`CoverError`] when the view is pitched, which cover does not yet handle.
    pub fn new(view: &ViewTransform) -> Result<Self, CoverError> {
        let latch = ZoomLatch::new(view.zoom);
        let tiles = cover::cover(view)?;
        Ok(Self {
            latch,
            entered: tiles.clone(),
            left: Vec::new(),
            tiles,
            changes: 1,
            frames: 1,
        })
    }

    /// Recomputes for a new camera.
    ///
    /// # Errors
    ///
    /// [`CoverError`] when the view is pitched.
    pub fn update(&mut self, view: &ViewTransform) -> Result<Update, CoverError> {
        self.frames += 1;
        // The latch decides the level; the camera decides everything else about the footprint.
        // Substituting the latched level into the transform rather than passing it alongside
        // keeps a single definition of what a cover is — the alternative is a second cover
        // function that takes a level, and two of those drift.
        let level = self.latch.update(view.zoom);
        let latched = ViewTransform {
            zoom: f64::from(level),
            ..*view
        };
        let tiles = cover::cover(&latched)?;

        if tiles == self.tiles {
            self.entered.clear();
            self.left.clear();
            return Ok(Update::Unchanged);
        }

        let before: BTreeSet<TileCoord> = self.tiles.iter().copied().collect();
        let after: BTreeSet<TileCoord> = tiles.iter().copied().collect();
        self.entered = after.difference(&before).copied().collect();
        self.left = before.difference(&after).copied().collect();
        self.tiles = tiles;
        self.changes += 1;
        Ok(Update::Changed)
    }

    /// The tiles this view wants.
    #[must_use]
    pub fn tiles(&self) -> &[TileCoord] {
        &self.tiles
    }

    /// Tiles the last change brought in — what to retain.
    #[must_use]
    pub fn entered(&self) -> &[TileCoord] {
        &self.entered
    }

    /// Tiles the last change dropped — what to release.
    ///
    /// Released rather than evicted: the store refcounts, and a tile another view still holds
    /// stays. That is §5.5's retain-chain unification, and it is why this reports a delta
    /// instead of touching a store itself — which store, under which source and style revision,
    /// is not a cover's business.
    #[must_use]
    pub fn left(&self) -> &[TileCoord] {
        &self.left
    }

    /// The zoom level being covered, which the latch holds across a dead band.
    #[must_use]
    pub const fn level(&self) -> u8 {
        self.latch.level()
    }

    /// How many frames changed the cover, against how many frames there were.
    ///
    /// The pair §12.7 is scored on. A pan that stays within its tiles and a pinch that stays
    /// within the dead band must both leave the first number alone.
    #[must_use]
    pub const fn churn(&self) -> (u64, u64) {
        (self.changes, self.frames)
    }

    /// What to draw this frame, with §13.2's never-blank substitution.
    ///
    /// The ideal cover is what the view *wants*; this is what it can have. An ideal tile that is
    /// not built yet falls back to its children or an ancestor rather than leaving a hole.
    pub fn draw<P: Pyramid + ?Sized>(&self, pyramid: &mut P, zooms: core::ops::RangeInclusive<u8>) {
        let (low, high) = (*zooms.start(), *zooms.end());
        let ideal: Vec<DataTileId> = self
            .tiles
            .iter()
            .map(|tile| {
                // A tile below the source's minimum is not covered at all; one above its maximum
                // is the deepest available tile standing in, which is what `overscaled_z`
                // records and what keeps the two apart in the store's key.
                let z = tile.z.min(high);
                #[allow(clippy::cast_possible_truncation)]
                DataTileId::overscaled(
                    tile.z.max(low),
                    tile.wrap as i16,
                    z,
                    tile.x >> (tile.z - z),
                    tile.y >> (tile.z - z),
                )
            })
            .collect();
        renderables::update_renderables(pyramid, &ideal, &[], zooms, None);
    }
}
