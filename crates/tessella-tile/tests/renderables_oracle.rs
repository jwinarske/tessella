//! mbgl's own expectations for `algorithm::updateRenderables`, all eighteen.
//!
//! The bodies below are converted from `test/algorithm/update_renderables.test.cpp` by rule
//! rather than by hand. Fourteen hundred lines of `{2, 0, {2, 1, 3}}` transcribed by eye would
//! introduce exactly the class of quiet error these tests exist to catch — an id off by one in
//! an expectation does not fail loudly, it makes a wrong implementation look right. The
//! converter refuses to emit on any statement it does not recognise, so nothing is silently
//! dropped, and mbgl's own comments are carried across so a reader can check each line against
//! the original.
//!
//! # What the mock is
//!
//! mbgl's `MockSource`: a map of tiles, each with three booleans, and an action log recording
//! every lookup, creation, retention and render in order. Comparing whole logs rather than final
//! state is the point — this algorithm's contract is as much about what it *does not* ask for
//! (the ascent it skips because a sibling already walked it, the network request it declines to
//! make for a substitute) as about what it draws.

use std::collections::{BTreeMap, BTreeSet};

use tessella_tile::renderables::{
    DataTileId, Necessity, Pyramid, RenderTileId, TileState, update_renderables,
};

/// A data tile id, in mbgl's argument order.
const fn d(overscaled_z: u8, wrap: i16, z: u8, x: u32, y: u32) -> DataTileId {
    DataTileId::overscaled(overscaled_z, wrap, z, x, y)
}

/// A render tile id.
const fn r(wrap: i16, z: u8, x: u32, y: u32) -> RenderTileId {
    RenderTileId { wrap, z, x, y }
}

/// One entry of the action log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// A lookup, and whether it found anything.
    Get(DataTileId, bool),
    Create(DataTileId),
    Retain(DataTileId, Necessity),
    Render(RenderTileId, DataTileId),
}

/// mbgl's `MockSource`.
struct Source {
    zooms: core::ops::RangeInclusive<u8>,
    data: BTreeMap<DataTileId, TileState>,
    ideal: BTreeSet<DataTileId>,
    log: Vec<Action>,
}

impl Source {
    fn new() -> Self {
        Self {
            zooms: 0..=16,
            data: BTreeMap::new(),
            ideal: BTreeSet::new(),
            log: Vec::new(),
        }
    }

    /// mbgl's `createTileData`: replaces any existing entry, and does not log.
    fn create_data(&mut self, id: DataTileId) {
        self.data.insert(id, TileState::default());
    }

    fn set(&mut self, id: DataTileId, change: impl FnOnce(&mut TileState)) {
        change(self.data.get_mut(&id).expect("a tile to change"));
    }

    fn erase(&mut self, id: DataTileId) {
        self.data.remove(&id);
    }

    /// Runs the algorithm over the current state.
    ///
    /// mbgl passes `source.dataTiles` as the prefetched map, so every tile the mock holds is a
    /// prefetch candidate — which is what makes the fallback reachable in these tests at all.
    fn run(&mut self, max_parent_overscale: Option<u8>) {
        let ideal: Vec<DataTileId> = self.ideal.iter().copied().collect();
        let prefetched: Vec<(DataTileId, TileState)> =
            self.data.iter().map(|(id, state)| (*id, *state)).collect();
        let zooms = self.zooms.clone();
        update_renderables(self, &ideal, &prefetched, zooms, max_parent_overscale);
    }
}

impl Pyramid for Source {
    fn get(&mut self, id: DataTileId) -> Option<TileState> {
        let found = self.data.get(&id).copied();
        self.log.push(Action::Get(id, found.is_some()));
        found
    }

    fn create(&mut self, id: DataTileId) -> Option<TileState> {
        self.log.push(Action::Create(id));
        self.data.insert(id, TileState::default());
        Some(TileState::default())
    }

    fn retain(&mut self, id: DataTileId, necessity: Necessity) {
        self.log.push(Action::Retain(id, necessity));
    }

    fn render(&mut self, render: RenderTileId, data: DataTileId) {
        self.log.push(Action::Render(render, data));
    }
}

/// mbgl `UpdateRenderables.SingleTile`.
#[test]
fn single_tile() {
    let mut source = Source::new();
    source.ideal.insert(d(1, 0, 1, 1, 1));
    source.create_data(d(1, 0, 1, 1, 1));
    source.set(d(1, 0, 1, 1, 1), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 1, 1), true), // found ideal tile
            Action::Retain(d(1, 0, 1, 1, 1), Necessity::Required),
            Action::Render(r(0, 1, 1, 1), d(1, 0, 1, 1, 1)), // render ideal tile
        ],
    );
    source.log.clear();
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 1, 1), true), // found ideal tile
            Action::Retain(d(1, 0, 1, 1, 1), Necessity::Required),
            Action::Render(r(0, 1, 1, 1), d(1, 0, 1, 1, 1)), // render ideal tile
        ],
    );
    source.log.clear();
    source.ideal.insert(d(1, 0, 1, 0, 1));
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 1), false), // missing ideal tile
            Action::Create(d(1, 0, 1, 0, 1)),     // create ideal tile
            Action::Retain(d(1, 0, 1, 0, 1), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 2), false), // four child tiles
            Action::Get(d(2, 0, 2, 0, 3), false), // ...
            Action::Get(d(2, 0, 2, 1, 2), false), // ...
            Action::Get(d(2, 0, 2, 1, 3), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // parent tile
            Action::Get(d(1, 0, 1, 1, 1), true),  // found ideal tile
            Action::Retain(d(1, 0, 1, 1, 1), Necessity::Required),
            Action::Render(r(0, 1, 1, 1), d(1, 0, 1, 1, 1)), // render found tile
        ],
    );
    source.log.clear();
    source.set(d(1, 0, 1, 0, 1), |s| s.tried_cache = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 1), true), // missing ideal tile
            Action::Retain(d(1, 0, 1, 0, 1), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 2), false), // four child tiles
            Action::Get(d(2, 0, 2, 0, 3), false), // ...
            Action::Get(d(2, 0, 2, 1, 2), false), // ...
            Action::Get(d(2, 0, 2, 1, 3), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // parent tile
            Action::Create(d(0, 0, 0, 0, 0)),     // load parent tile
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Get(d(1, 0, 1, 1, 1), true), // found ideal tile
            Action::Retain(d(1, 0, 1, 1, 1), Necessity::Required),
            Action::Render(r(0, 1, 1, 1), d(1, 0, 1, 1, 1)), // render found tile
        ],
    );
    source.log.clear();
    source.create_data(d(1, 0, 1, 0, 1));
    source.set(d(1, 0, 1, 0, 1), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 1), true), // newly added tile
            Action::Retain(d(1, 0, 1, 0, 1), Necessity::Required),
            Action::Render(r(0, 1, 0, 1), d(1, 0, 1, 0, 1)), // render ideal tile
            Action::Get(d(1, 0, 1, 1, 1), true),             // ideal tile
            Action::Retain(d(1, 0, 1, 1, 1), Necessity::Required),
            Action::Render(r(0, 1, 1, 1), d(1, 0, 1, 1, 1)), // render found tile
        ],
    );
    source.log.clear();
    source.ideal.insert(d(1, 0, 1, 0, 0));
    source.create_data(d(1, 0, 1, 0, 0));
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 0), true), // found tile, not ready
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 0), false), // four child tiles
            Action::Get(d(2, 0, 2, 0, 1), false), // ...
            Action::Get(d(2, 0, 2, 1, 0), false), // ...
            Action::Get(d(2, 0, 2, 1, 1), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), true),  // parent tile
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Get(d(1, 0, 1, 0, 1), true), // ideal tile
            Action::Retain(d(1, 0, 1, 0, 1), Necessity::Required),
            Action::Render(r(0, 1, 0, 1), d(1, 0, 1, 0, 1)), // render ideal tile
            Action::Get(d(1, 0, 1, 1, 1), true),             // ideal tile
            Action::Retain(d(1, 0, 1, 1, 1), Necessity::Required),
            Action::Render(r(0, 1, 1, 1), d(1, 0, 1, 1, 1)), // render ideal tile
        ],
    );
    source.log.clear();
    source.set(d(1, 0, 1, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 0), true), // found tile, now ready
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required),
            Action::Render(r(0, 1, 0, 0), d(1, 0, 1, 0, 0)),
            Action::Get(d(1, 0, 1, 0, 1), true), // ideal tile
            Action::Retain(d(1, 0, 1, 0, 1), Necessity::Required),
            Action::Render(r(0, 1, 0, 1), d(1, 0, 1, 0, 1)),
            Action::Get(d(1, 0, 1, 1, 1), true), // ideal tile
            Action::Retain(d(1, 0, 1, 1, 1), Necessity::Required),
            Action::Render(r(0, 1, 1, 1), d(1, 0, 1, 1, 1)),
        ],
    );
}

/// mbgl `UpdateRenderables.UseParentTile`.
#[test]
fn use_parent_tile() {
    let mut source = Source::new();
    source.ideal.insert(d(1, 0, 1, 0, 1));
    source.ideal.insert(d(1, 0, 1, 1, 0));
    source.ideal.insert(d(1, 0, 1, 1, 1));
    source.create_data(d(0, 0, 0, 0, 0));
    source.set(d(0, 0, 0, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 1), false), // missing ideal tile
            Action::Create(d(1, 0, 1, 0, 1)),
            Action::Retain(d(1, 0, 1, 0, 1), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 2), false), // child tile
            Action::Get(d(2, 0, 2, 0, 3), false), // ...
            Action::Get(d(2, 0, 2, 1, 2), false), // ...
            Action::Get(d(2, 0, 2, 1, 3), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), true),  // parent found!
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Render(r(0, 0, 0, 0), d(0, 0, 0, 0, 0)), // render parent
            Action::Get(d(1, 0, 1, 1, 0), false),            // missing ideal tile
            Action::Create(d(1, 0, 1, 1, 0)),
            Action::Retain(d(1, 0, 1, 1, 0), Necessity::Required),
            Action::Get(d(2, 0, 2, 2, 0), false), // child tile
            Action::Get(d(2, 0, 2, 2, 1), false), // ...
            Action::Get(d(2, 0, 2, 3, 0), false), // ...
            Action::Get(d(2, 0, 2, 3, 1), false), // ...
            Action::Get(d(1, 0, 1, 1, 1), false), // missing tile
            Action::Create(d(1, 0, 1, 1, 1)),
            Action::Retain(d(1, 0, 1, 1, 1), Necessity::Required),
            Action::Get(d(2, 0, 2, 2, 2), false), // child tile
            Action::Get(d(2, 0, 2, 2, 3), false), // ...
            Action::Get(d(2, 0, 2, 3, 2), false), // ...
            Action::Get(d(2, 0, 2, 3, 3), false), // ...
        ],
    );
}

/// mbgl `UpdateRenderables.DontUseWrongParentTile`.
#[test]
fn dont_use_wrong_parent_tile() {
    let mut source = Source::new();
    source.ideal.insert(d(2, 0, 2, 0, 0));
    source.create_data(d(1, 0, 1, 1, 0));
    source.set(d(1, 0, 1, 1, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(2, 0, 2, 0, 0), false), // missing ideal tile
            Action::Create(d(2, 0, 2, 0, 0)),
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(3, 0, 3, 0, 0), false), // child tile
            Action::Get(d(3, 0, 3, 0, 1), false), // ...
            Action::Get(d(3, 0, 3, 1, 0), false), // ...
            Action::Get(d(3, 0, 3, 1, 1), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), false), // parent tile, missing
            Action::Get(d(0, 0, 0, 0, 0), false), // parent tile, missing
        ],
    );
    source.log.clear();
    source.set(d(2, 0, 2, 0, 0), |s| s.tried_cache = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(2, 0, 2, 0, 0), true), // non-ready ideal tile
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(3, 0, 3, 0, 0), false), // child tile
            Action::Get(d(3, 0, 3, 0, 1), false), // ...
            Action::Get(d(3, 0, 3, 1, 0), false), // ...
            Action::Get(d(3, 0, 3, 1, 1), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), false), // parent tile, missing
            Action::Create(d(1, 0, 1, 0, 0)),     // find optional parent
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Optional),
            Action::Get(d(0, 0, 0, 0, 0), false), // parent tile, missing
        ],
    );
    source.log.clear();
    source.ideal.insert(d(2, 0, 2, 2, 0));
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(2, 0, 2, 0, 0), true), // non-ready ideal tile
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(3, 0, 3, 0, 0), false), // child tile
            Action::Get(d(3, 0, 3, 0, 1), false), // ...
            Action::Get(d(3, 0, 3, 1, 0), false), // ...
            Action::Get(d(3, 0, 3, 1, 1), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), true),  // parent tile not ready
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Optional),
            Action::Get(d(0, 0, 0, 0, 0), false), // missing parent tile
            Action::Get(d(2, 0, 2, 2, 0), false), // missing ideal tile
            Action::Create(d(2, 0, 2, 2, 0)),
            Action::Retain(d(2, 0, 2, 2, 0), Necessity::Required),
            Action::Get(d(3, 0, 3, 4, 0), false), // child tile
            Action::Get(d(3, 0, 3, 4, 1), false), // ...
            Action::Get(d(3, 0, 3, 5, 0), false), // ...
            Action::Get(d(3, 0, 3, 5, 1), false), // ...
            Action::Get(d(1, 0, 1, 1, 0), true),  // found parent tile
            Action::Retain(d(1, 0, 1, 1, 0), Necessity::Optional),
            Action::Render(r(0, 1, 1, 0), d(1, 0, 1, 1, 0)), // render parent tile
        ],
    );
}

/// mbgl `UpdateRenderables.UseParentTileWhenChildNotReady`.
#[test]
fn use_parent_tile_when_child_not_ready() {
    let mut source = Source::new();
    source.ideal.insert(d(1, 0, 1, 0, 1));
    source.create_data(d(0, 0, 0, 0, 0));
    source.set(d(0, 0, 0, 0, 0), |s| s.renderable = true);
    source.create_data(d(1, 0, 1, 0, 1));
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 1), true), // found, but not ready
            Action::Retain(d(1, 0, 1, 0, 1), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 2), false), // child tile
            Action::Get(d(2, 0, 2, 0, 3), false), // ...
            Action::Get(d(2, 0, 2, 1, 2), false), // ...
            Action::Get(d(2, 0, 2, 1, 3), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), true),  // parent tile, ready
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Render(r(0, 0, 0, 0), d(0, 0, 0, 0, 0)), // render parent tile
        ],
    );
    source.log.clear();
    source.set(d(1, 0, 1, 0, 1), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 1), true), // found and ready
            Action::Retain(d(1, 0, 1, 0, 1), Necessity::Required),
            Action::Render(r(0, 1, 0, 1), d(1, 0, 1, 0, 1)), // render ideal tile
        ],
    );
}

/// mbgl `UpdateRenderables.UseOverlappingParentTile`.
#[test]
fn use_overlapping_parent_tile() {
    let mut source = Source::new();
    source.ideal.insert(d(1, 0, 1, 0, 0));
    source.ideal.insert(d(1, 0, 1, 0, 1));
    source.create_data(d(0, 0, 0, 0, 0));
    source.set(d(0, 0, 0, 0, 0), |s| s.renderable = true);
    source.create_data(d(1, 0, 1, 0, 1));
    source.set(d(1, 0, 1, 0, 1), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 0), false), // ideal tile not found
            Action::Create(d(1, 0, 1, 0, 0)),
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 0), false), // child tile
            Action::Get(d(2, 0, 2, 0, 1), false), // ...
            Action::Get(d(2, 0, 2, 1, 0), false), // ...
            Action::Get(d(2, 0, 2, 1, 1), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), true),  // parent tile found
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Render(r(0, 0, 0, 0), d(0, 0, 0, 0, 0)),
            Action::Get(d(1, 0, 1, 0, 1), true), // ideal tile found
            Action::Retain(d(1, 0, 1, 0, 1), Necessity::Required),
            Action::Render(r(0, 1, 0, 1), d(1, 0, 1, 0, 1)),
        ],
    );
}

/// mbgl `UpdateRenderables.UseChildTiles`.
#[test]
fn use_child_tiles() {
    let mut source = Source::new();
    source.ideal.insert(d(0, 0, 0, 0, 0));
    source.create_data(d(1, 0, 1, 0, 0));
    source.set(d(1, 0, 1, 0, 0), |s| s.renderable = true);
    source.create_data(d(1, 0, 1, 1, 0));
    source.set(d(1, 0, 1, 1, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(0, 0, 0, 0, 0), false), // ideal tile, missing
            Action::Create(d(0, 0, 0, 0, 0)),
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Required),
            Action::Get(d(1, 0, 1, 0, 0), true), // child tile found
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Optional),
            Action::Render(r(0, 1, 0, 0), d(1, 0, 1, 0, 0)), // render child tile
            Action::Get(d(1, 0, 1, 0, 1), false),            // child tile not found
            Action::Get(d(1, 0, 1, 1, 0), true),             // child tile found
            Action::Retain(d(1, 0, 1, 1, 0), Necessity::Optional),
            Action::Render(r(0, 1, 1, 0), d(1, 0, 1, 1, 0)), // render child tile
            Action::Get(d(1, 0, 1, 1, 1), false),            // child tile not found
        ],
    );
    source.log.clear();
    source.erase(d(1, 0, 1, 0, 0));
    source.erase(d(1, 0, 1, 1, 0));
    source.create_data(d(2, 0, 2, 0, 0));
    source.set(d(2, 0, 2, 0, 0), |s| s.renderable = true);
    source.create_data(d(2, 0, 2, 1, 0));
    source.set(d(2, 0, 2, 1, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(0, 0, 0, 0, 0), true), // ideal tile not ready
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Required),
            Action::Get(d(1, 0, 1, 0, 0), false), // child tile not found
            Action::Get(d(1, 0, 1, 0, 1), false), // child tile not found
            Action::Get(d(1, 0, 1, 1, 0), false), // child tile not found
            Action::Get(d(1, 0, 1, 1, 1), false), // child tile not found
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Optional), // no parent or child tile found, check prefetched tiles
            Action::Render(r(0, 2, 0, 0), d(2, 0, 2, 0, 0)), // render child from 2 levels down
            Action::Retain(d(2, 0, 2, 1, 0), Necessity::Optional),
            Action::Render(r(0, 2, 1, 0), d(2, 0, 2, 1, 0)), // render child from 2 levels down
        ],
    );
}

/// mbgl `UpdateRenderables.PreferChildTiles`.
#[test]
fn prefer_child_tiles() {
    let mut source = Source::new();
    source.ideal.insert(d(1, 0, 1, 0, 0));
    source.create_data(d(0, 0, 0, 0, 0));
    source.set(d(0, 0, 0, 0, 0), |s| s.renderable = true);
    source.create_data(d(2, 0, 2, 0, 0));
    source.set(d(2, 0, 2, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 0), false), // ideal tile, not found
            Action::Create(d(1, 0, 1, 0, 0)),
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 0), true), // child tile, found
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Optional),
            Action::Render(r(0, 2, 0, 0), d(2, 0, 2, 0, 0)),
            Action::Get(d(2, 0, 2, 0, 1), false), // child tile, not found
            Action::Get(d(2, 0, 2, 1, 0), false), // ...
            Action::Get(d(2, 0, 2, 1, 1), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), true),  // parent tile, found
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Render(r(0, 0, 0, 0), d(0, 0, 0, 0, 0)),
        ],
    );
    source.log.clear();
    source.create_data(d(2, 0, 2, 0, 1));
    source.set(d(2, 0, 2, 0, 1), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required), // ideal tile was added in previous invocation, but is not yet ready
            Action::Get(d(2, 0, 2, 0, 0), true),                   // child tile, found
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Optional),
            Action::Render(r(0, 2, 0, 0), d(2, 0, 2, 0, 0)),
            Action::Get(d(2, 0, 2, 0, 1), true), // ...
            Action::Retain(d(2, 0, 2, 0, 1), Necessity::Optional), // ...
            Action::Render(r(0, 2, 0, 1), d(2, 0, 2, 0, 1)),
            Action::Get(d(2, 0, 2, 1, 0), false), // child tile, not found
            Action::Get(d(2, 0, 2, 1, 1), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), true),  // parent tile, found
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Render(r(0, 0, 0, 0), d(0, 0, 0, 0, 0)),
        ],
    );
    source.log.clear();
    source.create_data(d(2, 0, 2, 1, 0));
    source.set(d(2, 0, 2, 1, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required), // ideal tile was added in first invocation, but is not yet ready
            Action::Get(d(2, 0, 2, 0, 0), true),                   // child tile, found
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Optional),
            Action::Render(r(0, 2, 0, 0), d(2, 0, 2, 0, 0)),
            Action::Get(d(2, 0, 2, 0, 1), true), // ...
            Action::Retain(d(2, 0, 2, 0, 1), Necessity::Optional),
            Action::Render(r(0, 2, 0, 1), d(2, 0, 2, 0, 1)),
            Action::Get(d(2, 0, 2, 1, 0), true), // ...
            Action::Retain(d(2, 0, 2, 1, 0), Necessity::Optional),
            Action::Render(r(0, 2, 1, 0), d(2, 0, 2, 1, 0)),
            Action::Get(d(2, 0, 2, 1, 1), false), // child tile, not found
            Action::Get(d(0, 0, 0, 0, 0), true),  // parent tile, found
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Render(r(0, 0, 0, 0), d(0, 0, 0, 0, 0)),
        ],
    );
    source.log.clear();
    source.create_data(d(2, 0, 2, 1, 1));
    source.set(d(2, 0, 2, 1, 1), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required), // ideal tile was added in first invocation, but is not yet ready
            Action::Get(d(2, 0, 2, 0, 0), true),                   // child tile, found
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Optional),
            Action::Render(r(0, 2, 0, 0), d(2, 0, 2, 0, 0)),
            Action::Get(d(2, 0, 2, 0, 1), true), // ...
            Action::Retain(d(2, 0, 2, 0, 1), Necessity::Optional),
            Action::Render(r(0, 2, 0, 1), d(2, 0, 2, 0, 1)),
            Action::Get(d(2, 0, 2, 1, 0), true), // ...
            Action::Retain(d(2, 0, 2, 1, 0), Necessity::Optional),
            Action::Render(r(0, 2, 1, 0), d(2, 0, 2, 1, 0)),
            Action::Get(d(2, 0, 2, 1, 1), true), // ...
            Action::Retain(d(2, 0, 2, 1, 1), Necessity::Optional),
            Action::Render(r(0, 2, 1, 1), d(2, 0, 2, 1, 1)),
        ],
    );
}

/// mbgl `UpdateRenderables.UseParentAndChildTiles`.
#[test]
fn use_parent_and_child_tiles() {
    let mut source = Source::new();
    source.ideal.insert(d(1, 0, 1, 0, 0));
    source.create_data(d(0, 0, 0, 0, 0));
    source.set(d(0, 0, 0, 0, 0), |s| s.renderable = true);
    source.create_data(d(2, 0, 2, 0, 0));
    source.set(d(2, 0, 2, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 0), false), // ideal tile, missing
            Action::Create(d(1, 0, 1, 0, 0)),
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 0), true), // child tile
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Optional),
            Action::Render(r(0, 2, 0, 0), d(2, 0, 2, 0, 0)),
            Action::Get(d(2, 0, 2, 0, 1), false),
            Action::Get(d(2, 0, 2, 1, 0), false),
            Action::Get(d(2, 0, 2, 1, 1), false),
            Action::Get(d(0, 0, 0, 0, 0), true), // parent tile
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Render(r(0, 0, 0, 0), d(0, 0, 0, 0, 0)),
        ],
    );
    source.log.clear();
    source.erase(d(2, 0, 2, 0, 0));
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, 0, 1, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 0), false),
            Action::Get(d(2, 0, 2, 0, 1), false),
            Action::Get(d(2, 0, 2, 1, 0), false),
            Action::Get(d(2, 0, 2, 1, 1), false),
            Action::Get(d(0, 0, 0, 0, 0), true), // parent tile
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Render(r(0, 0, 0, 0), d(0, 0, 0, 0, 0)),
        ],
    );
}

/// mbgl `UpdateRenderables.DontUseTilesLowerThanMinzoom`.
#[test]
fn dont_use_tiles_lower_than_minzoom() {
    let mut source = Source::new();
    source.zooms = 2..=*source.zooms.end();
    source.ideal.insert(d(2, 0, 2, 0, 0));
    source.create_data(d(1, 0, 1, 0, 0));
    source.set(d(1, 0, 1, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(2, 0, 2, 0, 0), false), // ideal tile, missing
            Action::Create(d(2, 0, 2, 0, 0)),
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(3, 0, 3, 0, 0), false),
            Action::Get(d(3, 0, 3, 0, 1), false),
            Action::Get(d(3, 0, 3, 1, 0), false),
            Action::Get(d(3, 0, 3, 1, 1), false),
        ],
    );
}

/// mbgl `UpdateRenderables.UseOverzoomedTileAfterMaxzoom`.
#[test]
fn use_overzoomed_tile_after_maxzoom() {
    let mut source = Source::new();
    source.zooms = *source.zooms.start()..=2;
    source.ideal.insert(d(2, 0, 2, 0, 0));
    source.create_data(d(3, 0, 3, 0, 0));
    source.set(d(3, 0, 3, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(2, 0, 2, 0, 0), false), // ideal tile, missing
            Action::Create(d(2, 0, 2, 0, 0)),
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(3, 0, 2, 0, 0), false), // overzoomed tile, not children!
            Action::Get(d(1, 0, 1, 0, 0), false),
            Action::Get(d(0, 0, 0, 0, 0), false),
        ],
    );
    source.log.clear();
    source.set(d(2, 0, 2, 0, 0), |s| s.tried_cache = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(2, 0, 2, 0, 0), true), // ideal tile, missing
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(3, 0, 2, 0, 0), false), // overzoomed tile, not children!
            Action::Get(d(1, 0, 1, 0, 0), false),
            Action::Create(d(1, 0, 1, 0, 0)),
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Optional),
            Action::Get(d(0, 0, 0, 0, 0), false),
        ],
    );
    source.log.clear();
    source.ideal.clear();
    source.ideal.insert(d(3, 0, 2, 0, 0));
    source.create_data(d(2, 0, 2, 0, 0));
    source.set(d(2, 0, 2, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(3, 0, 2, 0, 0), false), // ideal tile, missing
            Action::Create(d(3, 0, 2, 0, 0)),
            Action::Retain(d(3, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(4, 0, 2, 0, 0), false),
            Action::Get(d(2, 0, 2, 0, 0), true),
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Optional),
            Action::Render(r(0, 2, 0, 0), d(2, 0, 2, 0, 0)),
        ],
    );
    source.log.clear();
    source.create_data(d(3, 0, 2, 0, 0));
    source.set(d(3, 0, 2, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(3, 0, 2, 0, 0), true),
            Action::Retain(d(3, 0, 2, 0, 0), Necessity::Required),
            Action::Render(r(0, 2, 0, 0), d(3, 0, 2, 0, 0)),
        ],
    );
    source.log.clear();
    source.ideal.clear();
    source.ideal.insert(d(2, 0, 2, 0, 0));
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(2, 0, 2, 0, 0), true),
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Required),
            Action::Render(r(0, 2, 0, 0), d(2, 0, 2, 0, 0)),
        ],
    );
    source.log.clear();
    source.erase(d(2, 0, 2, 0, 0));
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(2, 0, 2, 0, 0), false),
            Action::Create(d(2, 0, 2, 0, 0)),
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(3, 0, 2, 0, 0), true), // use overzoomed tile!
            Action::Retain(d(3, 0, 2, 0, 0), Necessity::Optional),
            Action::Render(r(0, 2, 0, 0), d(3, 0, 2, 0, 0)),
        ],
    );
}

/// mbgl `UpdateRenderables.AscendToNonOverzoomedTiles`.
#[test]
fn ascend_to_non_overzoomed_tiles() {
    let mut source = Source::new();
    source.zooms = *source.zooms.start()..=2;
    source.ideal.insert(d(3, 0, 2, 0, 0));
    source.create_data(d(3, 0, 2, 0, 0));
    source.set(d(3, 0, 2, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(3, 0, 2, 0, 0), true),
            Action::Retain(d(3, 0, 2, 0, 0), Necessity::Required),
            Action::Render(r(0, 2, 0, 0), d(3, 0, 2, 0, 0)),
        ],
    );
    source.log.clear();
    source.erase(d(3, 0, 2, 0, 0));
    source.create_data(d(2, 0, 2, 0, 0));
    source.set(d(2, 0, 2, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(3, 0, 2, 0, 0), false),
            Action::Create(d(3, 0, 2, 0, 0)),
            Action::Retain(d(3, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(4, 0, 2, 0, 0), false), // prefer using a child first
            Action::Get(d(2, 0, 2, 0, 0), true),
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Optional),
            Action::Render(r(0, 2, 0, 0), d(2, 0, 2, 0, 0)),
        ],
    );
    source.log.clear();
    source.erase(d(2, 0, 2, 0, 0));
    source.create_data(d(1, 0, 1, 0, 0));
    source.set(d(1, 0, 1, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(3, 0, 2, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(3, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(4, 0, 2, 0, 0), false),
            Action::Get(d(2, 0, 2, 0, 0), false),
            Action::Get(d(1, 0, 1, 0, 0), true),
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Optional),
            Action::Render(r(0, 1, 0, 0), d(1, 0, 1, 0, 0)),
        ],
    );
    source.log.clear();
    source.set(d(3, 0, 2, 0, 0), |s| s.tried_cache = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(3, 0, 2, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(3, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(4, 0, 2, 0, 0), false),
            Action::Get(d(2, 0, 2, 0, 0), false),
            Action::Create(d(2, 0, 2, 0, 0)),
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Optional),
            Action::Get(d(1, 0, 1, 0, 0), true),
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Optional),
            Action::Render(r(0, 1, 0, 0), d(1, 0, 1, 0, 0)),
        ],
    );
}

/// mbgl `UpdateRenderables.DoNotAscendMultipleTimesIfNotFound`.
#[test]
fn do_not_ascend_multiple_times_if_not_found() {
    let mut source = Source::new();
    source.ideal.insert(d(8, 0, 8, 0, 0));
    source.ideal.insert(d(8, 0, 8, 1, 0));
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(8, 0, 8, 0, 0), false), // ideal tile
            Action::Create(d(8, 0, 8, 0, 0)),
            Action::Retain(d(8, 0, 8, 0, 0), Necessity::Required),
            Action::Get(d(9, 0, 9, 0, 0), false), // child tile
            Action::Get(d(9, 0, 9, 0, 1), false), // ...
            Action::Get(d(9, 0, 9, 1, 0), false), // ...
            Action::Get(d(9, 0, 9, 1, 1), false), // ...
            Action::Get(d(7, 0, 7, 0, 0), false), // ascent
            Action::Get(d(6, 0, 6, 0, 0), false), // ...
            Action::Get(d(5, 0, 5, 0, 0), false), // ...
            Action::Get(d(4, 0, 4, 0, 0), false), // ...
            Action::Get(d(3, 0, 3, 0, 0), false), // ...
            Action::Get(d(2, 0, 2, 0, 0), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // ...
            Action::Get(d(8, 0, 8, 1, 0), false), // ideal tile
            Action::Create(d(8, 0, 8, 1, 0)),
            Action::Retain(d(8, 0, 8, 1, 0), Necessity::Required),
            Action::Get(d(9, 0, 9, 2, 0), false), // child tile
            Action::Get(d(9, 0, 9, 2, 1), false), // ...
            Action::Get(d(9, 0, 9, 3, 0), false), // ...
            Action::Get(d(9, 0, 9, 3, 1), false), // ...
        ],
    );
    source.log.clear();
    source.create_data(d(4, 0, 4, 0, 0));
    source.set(d(4, 0, 4, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(8, 0, 8, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(8, 0, 8, 0, 0), Necessity::Required),
            Action::Get(d(9, 0, 9, 0, 0), false), // child tile
            Action::Get(d(9, 0, 9, 0, 1), false), // ...
            Action::Get(d(9, 0, 9, 1, 0), false), // ...
            Action::Get(d(9, 0, 9, 1, 1), false), // ...
            Action::Get(d(7, 0, 7, 0, 0), false), // ascent
            Action::Get(d(6, 0, 6, 0, 0), false), // ...
            Action::Get(d(5, 0, 5, 0, 0), false), // ...
            Action::Get(d(4, 0, 4, 0, 0), true),  // stops ascent
            Action::Retain(d(4, 0, 4, 0, 0), Necessity::Optional),
            Action::Render(r(0, 4, 0, 0), d(4, 0, 4, 0, 0)),
            Action::Get(d(8, 0, 8, 1, 0), true), // ideal tile, not ready
            Action::Retain(d(8, 0, 8, 1, 0), Necessity::Required),
            Action::Get(d(9, 0, 9, 2, 0), false), // child tile
            Action::Get(d(9, 0, 9, 2, 1), false), // ...
            Action::Get(d(9, 0, 9, 3, 0), false), // ...
            Action::Get(d(9, 0, 9, 3, 1), false), // ...
        ],
    );
}

/// mbgl `UpdateRenderables.DontRetainUnusedNonIdealTiles`.
#[test]
fn dont_retain_unused_non_ideal_tiles() {
    let mut source = Source::new();
    source.ideal.insert(d(2, 0, 2, 0, 0));
    source.create_data(d(1, 0, 1, 0, 0));
    source.create_data(d(2, 0, 2, 0, 0));
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(2, 0, 2, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(3, 0, 3, 0, 0), false),
            Action::Get(d(3, 0, 3, 0, 1), false),
            Action::Get(d(3, 0, 3, 1, 0), false),
            Action::Get(d(3, 0, 3, 1, 1), false),
            Action::Get(d(1, 0, 1, 0, 0), true), // parent tile, not ready
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Optional),
            Action::Get(d(0, 0, 0, 0, 0), false),
        ],
    );
}

/// mbgl `UpdateRenderables.WrappedTiles`.
#[test]
fn wrapped_tiles() {
    let mut source = Source::new();
    source.ideal.insert(d(1, -1, 1, 1, 0));
    source.ideal.insert(d(1, 0, 1, 0, 0));
    source.ideal.insert(d(1, 0, 1, 1, 0));
    source.ideal.insert(d(1, 1, 1, 0, 0));
    source.create_data(d(0, 0, 0, 0, 0));
    source.set(d(0, 0, 0, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(1, -1, 1, 1, 0), false), // ideal tile 1/-1/0 (wrapped to -1)
            Action::Create(d(1, -1, 1, 1, 0)),
            Action::Retain(d(1, -1, 1, 1, 0), Necessity::Required),
            Action::Get(d(2, -1, 2, 2, 0), false),
            Action::Get(d(2, -1, 2, 2, 1), false),
            Action::Get(d(2, -1, 2, 3, 0), false),
            Action::Get(d(2, -1, 2, 3, 1), false),
            Action::Get(d(0, -1, 0, 0, 0), false), // { 0, 0, 0 } exists, but not the version wrapped to -1
            Action::Get(d(1, 0, 1, 0, 0), false),  // ideal tile 1/0/0
            Action::Create(d(1, 0, 1, 0, 0)),
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 0), false),
            Action::Get(d(2, 0, 2, 0, 1), false),
            Action::Get(d(2, 0, 2, 1, 0), false),
            Action::Get(d(2, 0, 2, 1, 1), false),
            Action::Get(d(0, 0, 0, 0, 0), true),
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Render(r(0, 0, 0, 0), d(0, 0, 0, 0, 0)),
            Action::Get(d(1, 0, 1, 1, 0), false), // ideal tile 1/1/0, doesn't match 1/-/1/0
            Action::Create(d(1, 0, 1, 1, 0)),
            Action::Retain(d(1, 0, 1, 1, 0), Necessity::Required),
            Action::Get(d(2, 0, 2, 2, 0), false),
            Action::Get(d(2, 0, 2, 2, 1), false),
            Action::Get(d(2, 0, 2, 3, 0), false),
            Action::Get(d(2, 0, 2, 3, 1), false),
            Action::Get(d(1, 1, 1, 0, 0), false), // ideal tile 1/2/0 (wrapped to 1)
            Action::Create(d(1, 1, 1, 0, 0)),
            Action::Retain(d(1, 1, 1, 0, 0), Necessity::Required),
            Action::Get(d(2, 1, 2, 0, 0), false),
            Action::Get(d(2, 1, 2, 0, 1), false),
            Action::Get(d(2, 1, 2, 1, 0), false),
            Action::Get(d(2, 1, 2, 1, 1), false),
            Action::Get(d(0, 1, 0, 0, 0), false), // { 0, 0, 0 } exists, but not the version wrapped to -1
        ],
    );
}

/// mbgl `UpdateRenderables.RepeatedRenderWithMissingOptionals`.
#[test]
fn repeated_render_with_missing_optionals() {
    let mut source = Source::new();
    source.ideal.insert(d(6, 0, 6, 0, 0));
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(6, 0, 6, 0, 0), false), // ideal tile, not found
            Action::Create(d(6, 0, 6, 0, 0)),
            Action::Retain(d(6, 0, 6, 0, 0), Necessity::Required),
            Action::Get(d(7, 0, 7, 0, 0), false), // children
            Action::Get(d(7, 0, 7, 0, 1), false), // ...
            Action::Get(d(7, 0, 7, 1, 0), false), // ...
            Action::Get(d(7, 0, 7, 1, 1), false), // ...
            Action::Get(d(5, 0, 5, 0, 0), false), // ascent
            Action::Get(d(4, 0, 4, 0, 0), false), // ...
            Action::Get(d(3, 0, 3, 0, 0), false), // ...
            Action::Get(d(2, 0, 2, 0, 0), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // ...
        ],
    );
    source.log.clear();
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(6, 0, 6, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(6, 0, 6, 0, 0), Necessity::Required),
            Action::Get(d(7, 0, 7, 0, 0), false), // children
            Action::Get(d(7, 0, 7, 0, 1), false), // ...
            Action::Get(d(7, 0, 7, 1, 0), false), // ...
            Action::Get(d(7, 0, 7, 1, 1), false), // ...
            Action::Get(d(5, 0, 5, 0, 0), false), // ascent
            Action::Get(d(4, 0, 4, 0, 0), false), // ...
            Action::Get(d(3, 0, 3, 0, 0), false), // ...
            Action::Get(d(2, 0, 2, 0, 0), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // ...
        ],
    );
    source.log.clear();
    source.set(d(6, 0, 6, 0, 0), |s| s.tried_cache = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(6, 0, 6, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(6, 0, 6, 0, 0), Necessity::Required),
            Action::Get(d(7, 0, 7, 0, 0), false), // children
            Action::Get(d(7, 0, 7, 0, 1), false), // ...
            Action::Get(d(7, 0, 7, 1, 0), false), // ...
            Action::Get(d(7, 0, 7, 1, 1), false), // ...
            Action::Get(d(5, 0, 5, 0, 0), false), // ascent
            Action::Create(d(5, 0, 5, 0, 0)),
            Action::Retain(d(5, 0, 5, 0, 0), Necessity::Optional),
            Action::Get(d(4, 0, 4, 0, 0), false), // ...
            Action::Get(d(3, 0, 3, 0, 0), false), // ...
            Action::Get(d(2, 0, 2, 0, 0), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // ...
        ],
    );
    source.log.clear();
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(6, 0, 6, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(6, 0, 6, 0, 0), Necessity::Required),
            Action::Get(d(7, 0, 7, 0, 0), false), // children
            Action::Get(d(7, 0, 7, 0, 1), false), // ...
            Action::Get(d(7, 0, 7, 1, 0), false), // ...
            Action::Get(d(7, 0, 7, 1, 1), false), // ...
            Action::Get(d(5, 0, 5, 0, 0), true),  // ascent
            Action::Retain(d(5, 0, 5, 0, 0), Necessity::Optional),
            Action::Get(d(4, 0, 4, 0, 0), false), // ...
            Action::Get(d(3, 0, 3, 0, 0), false), // ...
            Action::Get(d(2, 0, 2, 0, 0), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // ...
        ],
    );
    source.log.clear();
    source.set(d(5, 0, 5, 0, 0), |s| s.tried_cache = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(6, 0, 6, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(6, 0, 6, 0, 0), Necessity::Required),
            Action::Get(d(7, 0, 7, 0, 0), false), // children
            Action::Get(d(7, 0, 7, 0, 1), false), // ...
            Action::Get(d(7, 0, 7, 1, 0), false), // ...
            Action::Get(d(7, 0, 7, 1, 1), false), // ...
            Action::Get(d(5, 0, 5, 0, 0), true),  // ascent
            Action::Retain(d(5, 0, 5, 0, 0), Necessity::Optional),
            Action::Get(d(4, 0, 4, 0, 0), false), // ...
            Action::Create(d(4, 0, 4, 0, 0)),
            Action::Retain(d(4, 0, 4, 0, 0), Necessity::Optional),
            Action::Get(d(3, 0, 3, 0, 0), false), // ...
            Action::Get(d(2, 0, 2, 0, 0), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // ...
        ],
    );
    source.log.clear();
    source.set(d(4, 0, 4, 0, 0), |s| s.tried_cache = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(6, 0, 6, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(6, 0, 6, 0, 0), Necessity::Required),
            Action::Get(d(7, 0, 7, 0, 0), false), // children
            Action::Get(d(7, 0, 7, 0, 1), false), // ...
            Action::Get(d(7, 0, 7, 1, 0), false), // ...
            Action::Get(d(7, 0, 7, 1, 1), false), // ...
            Action::Get(d(5, 0, 5, 0, 0), true),  // ascent
            Action::Retain(d(5, 0, 5, 0, 0), Necessity::Optional),
            Action::Get(d(4, 0, 4, 0, 0), true), // ...
            Action::Retain(d(4, 0, 4, 0, 0), Necessity::Optional),
            Action::Get(d(3, 0, 3, 0, 0), false), // ...
            Action::Create(d(3, 0, 3, 0, 0)),
            Action::Retain(d(3, 0, 3, 0, 0), Necessity::Optional),
            Action::Get(d(2, 0, 2, 0, 0), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // ...
        ],
    );
    source.log.clear();
    source.set(d(3, 0, 3, 0, 0), |s| s.tried_cache = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(6, 0, 6, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(6, 0, 6, 0, 0), Necessity::Required),
            Action::Get(d(7, 0, 7, 0, 0), false), // children
            Action::Get(d(7, 0, 7, 0, 1), false), // ...
            Action::Get(d(7, 0, 7, 1, 0), false), // ...
            Action::Get(d(7, 0, 7, 1, 1), false), // ...
            Action::Get(d(5, 0, 5, 0, 0), true),  // ascent
            Action::Retain(d(5, 0, 5, 0, 0), Necessity::Optional),
            Action::Get(d(4, 0, 4, 0, 0), true), // ...
            Action::Retain(d(4, 0, 4, 0, 0), Necessity::Optional),
            Action::Get(d(3, 0, 3, 0, 0), true), // ...
            Action::Retain(d(3, 0, 3, 0, 0), Necessity::Optional),
            Action::Get(d(2, 0, 2, 0, 0), false), // ...
            Action::Create(d(2, 0, 2, 0, 0)),
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Optional),
            Action::Get(d(1, 0, 1, 0, 0), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // ...
        ],
    );
    source.log.clear();
    source.set(d(3, 0, 3, 0, 0), |s| s.renderable = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(6, 0, 6, 0, 0), true), // ideal tile, not ready
            Action::Retain(d(6, 0, 6, 0, 0), Necessity::Required),
            Action::Get(d(7, 0, 7, 0, 0), false), // children
            Action::Get(d(7, 0, 7, 0, 1), false), // ...
            Action::Get(d(7, 0, 7, 1, 0), false), // ...
            Action::Get(d(7, 0, 7, 1, 1), false), // ...
            Action::Get(d(5, 0, 5, 0, 0), true),  // ascent
            Action::Retain(d(5, 0, 5, 0, 0), Necessity::Optional),
            Action::Get(d(4, 0, 4, 0, 0), true), // ...
            Action::Retain(d(4, 0, 4, 0, 0), Necessity::Optional),
            Action::Get(d(3, 0, 3, 0, 0), true), // ...
            Action::Retain(d(3, 0, 3, 0, 0), Necessity::Optional),
            Action::Render(r(0, 3, 0, 0), d(3, 0, 3, 0, 0)),
        ],
    );
}

/// mbgl `UpdateRenderables.LoadRequiredIfIdealTileCantBeFound`.
#[test]
fn load_required_if_ideal_tile_cant_be_found() {
    let mut source = Source::new();
    source.zooms = *source.zooms.start()..=6;
    source.ideal.insert(d(6, 0, 6, 0, 0));
    source.create_data(d(6, 0, 6, 0, 0));
    source.set(d(6, 0, 6, 0, 0), |s| s.tried_cache = true);
    source.set(d(6, 0, 6, 0, 0), |s| s.loaded = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(6, 0, 6, 0, 0), true), // ideal tile, not found
            Action::Retain(d(6, 0, 6, 0, 0), Necessity::Required),
            Action::Get(d(7, 0, 6, 0, 0), false), // overzoomed child
            Action::Get(d(5, 0, 5, 0, 0), false), // ascent
            Action::Create(d(5, 0, 5, 0, 0)),
            Action::Retain(d(5, 0, 5, 0, 0), Necessity::Required),
            Action::Get(d(4, 0, 4, 0, 0), false), // ...
            Action::Get(d(3, 0, 3, 0, 0), false), // ...
            Action::Get(d(2, 0, 2, 0, 0), false), // ...
            Action::Get(d(1, 0, 1, 0, 0), false), // ...
            Action::Get(d(0, 0, 0, 0, 0), false), // ...
        ],
    );
}

/// mbgl `UpdateRenderables.LoadOverscaledMaxZoomTile`.
#[test]
fn load_overscaled_max_zoom_tile() {
    let mut source = Source::new();
    source.zooms = *source.zooms.start()..=2;
    source.ideal.insert(d(4, 0, 2, 0, 0));
    source.create_data(d(4, 0, 2, 0, 0));
    source.set(d(4, 0, 2, 0, 0), |s| s.renderable = false);
    source.set(d(4, 0, 2, 0, 0), |s| s.tried_cache = true);
    source.set(d(4, 0, 2, 0, 0), |s| s.loaded = true);
    source.create_data(d(3, 0, 2, 0, 0));
    source.set(d(3, 0, 2, 0, 0), |s| s.renderable = false);
    source.set(d(3, 0, 2, 0, 0), |s| s.tried_cache = true);
    source.set(d(3, 0, 2, 0, 0), |s| s.loaded = true);
    source.create_data(d(2, 0, 2, 0, 0));
    source.set(d(2, 0, 2, 0, 0), |s| s.renderable = false);
    source.set(d(2, 0, 2, 0, 0), |s| s.tried_cache = true);
    source.set(d(2, 0, 2, 0, 0), |s| s.loaded = true);
    source.create_data(d(1, 0, 1, 0, 0));
    source.set(d(1, 0, 1, 0, 0), |s| s.renderable = true);
    source.set(d(1, 0, 1, 0, 0), |s| s.tried_cache = true);
    source.set(d(1, 0, 1, 0, 0), |s| s.loaded = true);
    source.run(None);
    assert_eq!(
        source.log,
        [
            Action::Get(d(4, 0, 2, 0, 0), true),
            Action::Retain(d(4, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(5, 0, 2, 0, 0), false),
            Action::Get(d(3, 0, 2, 0, 0), true),
            Action::Retain(d(3, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(2, 0, 2, 0, 0), true),
            Action::Retain(d(2, 0, 2, 0, 0), Necessity::Required),
            Action::Get(d(1, 0, 1, 0, 0), true),
            Action::Retain(d(1, 0, 1, 0, 0), Necessity::Required),
            Action::Render(r(0, 1, 0, 0), d(1, 0, 1, 0, 0)),
        ],
    );
}

/// mbgl `UpdateRenderables.MaxParentOverscaleFactor`.
#[test]
fn max_parent_overscale_factor() {
    let mut source = Source::new();
    source.ideal.insert(d(4, 0, 4, 0, 0));
    source.ideal.insert(d(4, 0, 4, 1, 0));
    source.create_data(d(0, 0, 0, 0, 0));
    source.set(d(0, 0, 0, 0, 0), |s| s.renderable = true);
    source.run(Some(4));
    assert_eq!(
        source.log,
        [
            Action::Get(d(4, 0, 4, 0, 0), false), // ideal tile
            Action::Create(d(4, 0, 4, 0, 0)),
            Action::Retain(d(4, 0, 4, 0, 0), Necessity::Required),
            Action::Get(d(5, 0, 5, 0, 0), false), // child tiles
            Action::Get(d(5, 0, 5, 0, 1), false),
            Action::Get(d(5, 0, 5, 1, 0), false),
            Action::Get(d(5, 0, 5, 1, 1), false),
            Action::Get(d(3, 0, 3, 0, 0), false), // ascent
            Action::Get(d(2, 0, 2, 0, 0), false),
            Action::Get(d(1, 0, 1, 0, 0), false),
            Action::Get(d(0, 0, 0, 0, 0), true),
            Action::Retain(d(0, 0, 0, 0, 0), Necessity::Optional),
            Action::Render(r(0, 0, 0, 0), d(0, 0, 0, 0, 0)), // render tile 0,0,0
            Action::Get(d(4, 0, 4, 1, 0), false),            // ideal tile
            Action::Create(d(4, 0, 4, 1, 0)),
            Action::Retain(d(4, 0, 4, 1, 0), Necessity::Required),
            Action::Get(d(5, 0, 5, 2, 0), false), // child tiles
            Action::Get(d(5, 0, 5, 2, 1), false),
            Action::Get(d(5, 0, 5, 3, 0), false),
            Action::Get(d(5, 0, 5, 3, 1), false),
        ],
    );
    source.log.clear();
    source.run(Some(3));
    assert_eq!(
        source.log,
        [
            Action::Get(d(4, 0, 4, 0, 0), true), // ideal tile
            Action::Retain(d(4, 0, 4, 0, 0), Necessity::Required),
            Action::Get(d(5, 0, 5, 0, 0), false), // child tiles
            Action::Get(d(5, 0, 5, 0, 1), false),
            Action::Get(d(5, 0, 5, 1, 0), false),
            Action::Get(d(5, 0, 5, 1, 1), false),
            Action::Get(d(3, 0, 3, 0, 0), false), // ascent
            Action::Get(d(2, 0, 2, 0, 0), false),
            Action::Get(d(1, 0, 1, 0, 0), false),
            Action::Get(d(4, 0, 4, 1, 0), true), // ideal tile
            Action::Retain(d(4, 0, 4, 1, 0), Necessity::Required),
            Action::Get(d(5, 0, 5, 2, 0), false), // child tiles
            Action::Get(d(5, 0, 5, 2, 1), false),
            Action::Get(d(5, 0, 5, 3, 0), false),
            Action::Get(d(5, 0, 5, 3, 1), false),
        ],
    );
}
