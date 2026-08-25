//! Keeping a label's identity when the tile under it changes.
//!
//! A transcription of mbgl's `CrossTileSymbolLayerIndex`. At a zoom crossing every tile is
//! replaced by four of its children, and the label that was "Detroit" in the parent is a
//! *different* symbol instance in the child — different tile, different buffer, different
//! index. Nothing about it says it is the same label.
//!
//! Which matters because [`crate::fade`] keys opacity by identity. Without this, every label on
//! the map restarts its fade at every crossing: the whole map blinks. §13.3 asks for zero symbol
//! pops, and this is what buys them.
//!
//! # Matching is by key and by *rounded* position
//!
//! A label does not sit at exactly the same tile coordinate in a parent and a child — the
//! anchor is quantised differently, and the geometry it was placed against was simplified at a
//! different zoom. So positions are scaled into one common space and rounded onto a grid of
//! roughly four pixels, and two symbols with the same text within a grid unit are the same
//! label.
//!
//! Rounding is what makes it work and also what bounds it: two genuinely distinct labels with
//! the same text within four pixels of each other will be treated as one. That is the trade
//! mbgl makes, and it is the right one — two identical labels that close together are a data
//! error, and treating them as one is nicer than blinking.
//!
//! # A parent may only lend each label once
//!
//! Four children of one parent all match against it. Without a guard, all four would claim the
//! same parent label's id and four separate labels would share one fade state. So an id claimed
//! at a zoom level is struck off for that level — mbgl's issue #10844.

use std::collections::{BTreeMap, BTreeSet};

use tessella_tile::renderables::DataTileId;

/// The tile extent symbol anchors are expressed in.
const EXTENT: f64 = 8192.0;

/// How far positions are rounded before comparison: a grid of about four pixels.
///
/// `512 / EXTENT / 2` in mbgl's spelling, which is the tile's screen size over its coordinate
/// space, halved.
const ROUNDING: f64 = 512.0 / EXTENT / 2.0;

/// One symbol as this index sees it: what it says, and where.
#[derive(Debug, Clone, PartialEq)]
pub struct Symbol {
    /// The label's text, which is what makes two symbols candidates for being the same one.
    pub key: String,
    /// Its anchor, in tile coordinates.
    pub anchor: (f32, f32),
    /// The identity assigned by this index. Zero until it has one.
    pub cross_tile_id: u32,
}

impl Symbol {
    /// A symbol with no identity yet.
    #[must_use]
    pub fn new(key: impl Into<String>, anchor: (f32, f32)) -> Self {
        Self {
            key: key.into(),
            anchor,
            cross_tile_id: 0,
        }
    }
}

/// A symbol's position, rounded into the shared grid.
type Scaled = (i64, i64);

/// One tile's symbols, indexed by key.
#[derive(Debug)]
struct TileIndex {
    tile: DataTileId,
    bucket: u32,
    by_key: BTreeMap<String, Vec<(u32, Scaled)>>,
}

/// Scales a symbol's anchor into the grid shared by `index_tile` and `symbol_tile`.
///
/// The tile's own origin is folded in — `x * EXTENT + anchor` — so two tiles at different zooms
/// over the same ground produce the same number. Without that, every tile's coordinates start at
/// zero and every label in every tile would match every other.
fn scaled(index_tile: DataTileId, symbol_tile: DataTileId, anchor: (f32, f32)) -> Scaled {
    let levels = i32::from(symbol_tile.z) - i32::from(index_tile.z);
    let scale = ROUNDING / 2f64.powi(levels);
    #[allow(clippy::cast_possible_truncation)]
    let x = ((f64::from(symbol_tile.x) * EXTENT + f64::from(anchor.0)) * scale).floor() as i64;
    #[allow(clippy::cast_possible_truncation)]
    let y = ((f64::from(symbol_tile.y) * EXTENT + f64::from(anchor.1)) * scale).floor() as i64;
    (x, y)
}

impl TileIndex {
    fn new(tile: DataTileId, bucket: u32, symbols: &[Symbol]) -> Self {
        let mut by_key: BTreeMap<String, Vec<(u32, Scaled)>> = BTreeMap::new();
        for symbol in symbols {
            if symbol.cross_tile_id == 0 {
                continue;
            }
            by_key
                .entry(symbol.key.clone())
                .or_default()
                .push((symbol.cross_tile_id, scaled(tile, tile, symbol.anchor)));
        }
        Self {
            tile,
            bucket,
            by_key,
        }
    }

    /// Lends this tile's identities to whichever of `symbols` match.
    fn find_matches(&self, symbols: &mut [Symbol], tile: DataTileId, claimed: &mut BTreeSet<u32>) {
        // Going down a level halves the grid, so a parent's rounding is coarser than a child's
        // and the match has to allow for it. Going up, one unit is enough.
        let tolerance = if self.tile.z < tile.z {
            1
        } else {
            1i64 << (self.tile.z - tile.z)
        };

        for symbol in symbols.iter_mut() {
            if symbol.cross_tile_id != 0 {
                continue;
            }
            let Some(candidates) = self.by_key.get(&symbol.key) else {
                continue;
            };
            let here = scaled(self.tile, tile, symbol.anchor);
            for (id, there) in candidates {
                if (there.0 - here.0).abs() <= tolerance
                    && (there.1 - here.1).abs() <= tolerance
                    && !claimed.contains(id)
                {
                    // Struck off for this zoom: four children of one parent must not all claim
                    // the same label's identity and end up sharing one fade.
                    claimed.insert(*id);
                    symbol.cross_tile_id = *id;
                    break;
                }
            }
        }
    }
}

/// Stable identities for the symbols of one layer, across tiles and zooms.
#[derive(Debug, Default)]
pub struct CrossTileIndex {
    /// Per overscaled zoom, the tiles held there.
    indexes: BTreeMap<u8, BTreeMap<DataTileId, TileIndex>>,
    /// Per zoom, the identities already claimed, so a parent lends each label once.
    claimed: BTreeMap<u8, BTreeSet<u32>>,
    next_id: u32,
}

impl CrossTileIndex {
    /// An index with nothing in it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Assigns identities to a tile's symbols, matching against every tile already held.
    ///
    /// Returns whether the index changed, which is what tells a caller placement has to run
    /// again.
    pub fn add_bucket(&mut self, tile: DataTileId, bucket: u32, symbols: &mut [Symbol]) -> bool {
        if let Some(held) = self
            .indexes
            .get(&tile.overscaled_z)
            .and_then(|zoom| zoom.get(&tile))
            && held.bucket == bucket
        {
            // The same bucket as last time: its symbols already have their identities.
            return false;
        }

        // Replacing a tile: release what its old bucket claimed before the new one asks, or the
        // new bucket cannot re-take the identities its own labels had a moment ago.
        if let Some(held) = self
            .indexes
            .get(&tile.overscaled_z)
            .and_then(|zoom| zoom.get(&tile))
        {
            let released: Vec<u32> = held
                .by_key
                .values()
                .flat_map(|entries| entries.iter().map(|(id, _)| *id))
                .collect();
            if let Some(claimed) = self.claimed.get_mut(&tile.overscaled_z) {
                for id in released {
                    claimed.remove(&id);
                }
            }
        }

        for symbol in symbols.iter_mut() {
            symbol.cross_tile_id = 0;
        }

        let mut claimed = self.claimed.remove(&tile.overscaled_z).unwrap_or_default();
        for (zoom, tiles) in &self.indexes {
            if *zoom > tile.overscaled_z {
                for (held, index) in tiles {
                    if held.is_child_of(tile) {
                        index.find_matches(symbols, tile, &mut claimed);
                    }
                }
            } else if let Some(index) = tiles.get(&tile.scaled_to(*zoom)) {
                index.find_matches(symbols, tile, &mut claimed);
            }
        }

        // Whatever is left is a label nothing has seen before.
        for symbol in symbols.iter_mut() {
            if symbol.cross_tile_id == 0 {
                self.next_id += 1;
                symbol.cross_tile_id = self.next_id;
                claimed.insert(symbol.cross_tile_id);
            }
        }
        self.claimed.insert(tile.overscaled_z, claimed);

        self.indexes
            .entry(tile.overscaled_z)
            .or_default()
            .insert(tile, TileIndex::new(tile, bucket, symbols));
        true
    }

    /// Drops tiles whose buckets are no longer current, releasing their identities.
    ///
    /// Returns whether anything went. A label whose tile is gone loses its identity, so if it
    /// comes back it comes back as new — which is correct: it left the map.
    pub fn remove_stale_buckets(&mut self, current: &BTreeSet<u32>) -> bool {
        let mut changed = false;
        for (zoom, tiles) in &mut self.indexes {
            let stale: Vec<DataTileId> = tiles
                .iter()
                .filter(|(_, index)| !current.contains(&index.bucket))
                .map(|(tile, _)| *tile)
                .collect();
            for tile in stale {
                if let Some(index) = tiles.remove(&tile)
                    && let Some(claimed) = self.claimed.get_mut(zoom)
                {
                    for entries in index.by_key.values() {
                        for (id, _) in entries {
                            claimed.remove(id);
                        }
                    }
                }
                changed = true;
            }
        }
        changed
    }

    /// How many identities have ever been handed out.
    #[must_use]
    pub const fn issued(&self) -> u32 {
        self.next_id
    }

    /// How many tiles are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.indexes.values().map(BTreeMap::len).sum()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
