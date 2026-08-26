//! Clip-mask descriptions: `StencilTiles` (§2.2, §6.3).
//!
//! # The consumer synthesizes the mask; the stream describes it
//!
//! §2.2 is explicit that reference values are never carried. mbgl's own path here rasterizes
//! masks into a stencil buffer and hands drawables a reference number, which bakes a
//! stencil-buffer renderer into the protocol. What travels instead is the set of tiles a layer
//! group clips against and the matrix that places each one, and the consumer decides how to
//! turn that into clipping — a stencil buffer, a scissor per tile, or a clip plane, as its
//! backend prefers. DR-13 is that the stream must contain nothing accidentally shaped like one
//! renderer, and this is where that is easiest to get wrong.
//!
//! # Which layers get one
//!
//! Only layers drawn per tile. The oracle emits clip sets for the three tiled layers of the
//! hermetic style and none for the background or the circle layer — the background covers the
//! viewport and has nothing to clip against, and this crate's `background_flags` correspondingly
//! carries no stencil bit. That agreement is checked rather than assumed, because a background
//! clipped to a tile is a background with seams.
//!
//! # The matrix is per tile, not per layer
//!
//! `matrixForTile` is a function of the tile and the camera, so every layer clipping against the
//! same cover gets the same matrices. The oracle's three clip sets carry identical matrix hashes
//! for that reason, and a set that varied by layer would mean the placement had picked up a
//! dependency it should not have.

use alloc::vec::Vec;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{Span, StencilTile, StencilTiles, TileId, ViewId, WireRecord};
use tessella_capture_abi::ring::{Full, Producer};
use tessella_tile::camera::{self, CameraError};
use tessella_tile::cover::{TileCoord, ViewTransform};

/// A layer's clip set: which tiles it clips against, and where each one sits.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipSet {
    /// The layer group being clipped, in style order.
    pub layer_index: i32,
    /// The tiles, in cover order.
    pub tiles: Vec<StencilTile>,
}

/// Builds the clip set for one layer over one cover.
///
/// The matrices are `f32`, which is what the consumer's pipeline consumes and what mbgl casts to
/// before handing them over. The camera is computed in `f64` and narrowed here rather than being
/// computed in `f32`: the world coordinates at high zoom are large enough that the intermediate
/// precision matters, and narrowing at the end is the difference between a placement that is
/// right and one that shimmers.
///
/// # Errors
///
/// [`CameraError::EmptyViewport`] when the view has no area.
pub fn clip_set(
    view: &ViewTransform,
    layer_index: i32,
    cover: &[TileCoord],
) -> Result<ClipSet, CameraError> {
    let mut tiles = Vec::with_capacity(cover.len());
    for coord in cover {
        let matrix = camera::tile_to_clip(view, coord.z, coord.x, coord.y, coord.wrap)?;
        #[allow(clippy::cast_possible_truncation)]
        let narrowed = core::array::from_fn(|index| matrix[index] as f32);
        #[allow(clippy::cast_possible_truncation)]
        tiles.push(StencilTile {
            matrix: narrowed,
            tile: TileId {
                x: coord.x,
                y: coord.y,
                z: coord.z,
                overscaled_z: coord.z,
                wrap: coord.wrap as i16,
            },
        });
    }
    Ok(ClipSet { layer_index, tiles })
}

/// Writes a clip set to the ring.
///
/// # Errors
///
/// [`Full`] when the ring cannot take it.
pub fn write(producer: &mut Producer, view: ViewId, set: &ClipSet) -> Result<(), Full> {
    let mut payload = Vec::with_capacity(set.tiles.len() * core::mem::size_of::<StencilTile>());
    for tile in &set.tiles {
        payload.extend_from_slice(tile.as_bytes());
    }

    #[allow(clippy::cast_possible_truncation)]
    let record = StencilTiles {
        view,
        layer_index: set.layer_index,
        tiles: Span {
            offset: 0,
            count: set.tiles.len() as u32,
        },
    };
    producer.write(EnvelopeKind::StencilTiles, record.as_bytes(), &payload)
}

/// Tracks the last clip set emitted per layer, so an unchanged one is not re-sent.
///
/// §6.3 says clip sets are emitted on change only, and the change is common: every tile arrival
/// and every zoom step rewrites the cover. What is *not* common is a frame where nothing moved,
/// and that is the frame DR-8 requires to be silent.
#[derive(Debug, Default)]
pub struct ClipSets {
    emitted: alloc::collections::BTreeMap<i32, Vec<StencilTile>>,
}

impl ClipSets {
    /// No sets emitted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Emits a layer's clip set if it differs from the last one sent for that layer.
    ///
    /// Returns whether anything was written.
    ///
    /// # Errors
    ///
    /// [`Full`] when the ring cannot take it.
    pub fn emit(
        &mut self,
        producer: &mut Producer,
        view: ViewId,
        set: &ClipSet,
    ) -> Result<bool, Full> {
        if self.emitted.get(&set.layer_index).map(Vec::as_slice) == Some(set.tiles.as_slice()) {
            return Ok(false);
        }
        write(producer, view, set)?;
        self.emitted.insert(set.layer_index, set.tiles.clone());
        Ok(true)
    }

    /// How many layers have a clip set on the wire.
    #[must_use]
    pub fn len(&self) -> usize {
        self.emitted.len()
    }

    /// True when no layer has one.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.emitted.is_empty()
    }
}
