//! The guarantee §13.2 is named for: during a zoom crossing, no frame has a hole in it.
//!
//! `renderables_oracle.rs` checks the algorithm against mbgl action for action. This checks the
//! property that algorithm exists to deliver, which is a different question — a faithful port of
//! a wrong algorithm passes the first and fails this.
//!
//! # The guarantee is conditional, and the condition is the interesting part
//!
//! Nothing can cover a viewport when the pyramid is empty; a genuinely cold start draws nothing
//! and there is no arrangement of substitutions that changes it. What never-blank actually
//! promises is that a view which *was* covered stays covered: cross from a fully drawn z13 to
//! z14 and every frame in between is complete, however the fourteen children happen to arrive.
//! So the tests below establish a covered level first and assert across the transition, and the
//! last one asserts the converse — that from nothing, nothing follows — because a coverage
//! predicate that returns true for an empty render list would make every other test here pass.
//!
//! # Coverage is decided on the quadtree, not by sampling
//!
//! A rendered tile covers an ideal one when it is that tile, contains it, or its own children
//! cover it between them. That recursion is exact and terminates on the quadtree, which beats
//! rasterising both sets and comparing pixels: a sampling test passes for an implementation that
//! leaves a hole thinner than the sample spacing, and a hairline of background between two tiles
//! is precisely the artefact this is meant to rule out.

use std::collections::{BTreeMap, BTreeSet};

use tessella_tile::renderables::{
    DataTileId, Necessity, Pyramid, RenderTileId, TileState, update_renderables,
};

/// A pyramid whose tiles are renderable exactly when they have been marked so.
#[derive(Default)]
struct World {
    ready: BTreeSet<DataTileId>,
    known: BTreeMap<DataTileId, TileState>,
    rendered: Vec<DataTileId>,
}

impl World {
    /// Marks a tile as known but still in flight: fetched or fetching, not yet drawable.
    ///
    /// The state that makes the readiness half of `assert_no_holes` bite. A pyramid whose tiles
    /// are only ever absent or ready cannot catch an implementation that draws a pending tile,
    /// because there is never a pending tile to draw — and mid-crossing, pending is what most
    /// of the pyramid is.
    fn begin(&mut self, id: DataTileId) {
        self.known.insert(id, TileState::default());
    }

    /// Marks a tile as built and drawable.
    fn arrive(&mut self, id: DataTileId) {
        self.ready.insert(id);
        self.known.insert(
            id,
            TileState {
                renderable: true,
                loaded: true,
                tried_cache: true,
            },
        );
    }

    /// Runs one frame and returns what would be drawn.
    fn frame(
        &mut self,
        ideal: &[DataTileId],
        zooms: core::ops::RangeInclusive<u8>,
    ) -> Vec<DataTileId> {
        self.rendered.clear();
        update_renderables(self, ideal, &[], zooms, None);
        self.rendered.clone()
    }
}

impl Pyramid for World {
    fn get(&mut self, id: DataTileId) -> Option<TileState> {
        self.known.get(&id).copied()
    }

    fn create(&mut self, id: DataTileId) -> Option<TileState> {
        let state = TileState::default();
        self.known.insert(id, state);
        Some(state)
    }

    fn retain(&mut self, _id: DataTileId, _necessity: Necessity) {}

    fn render(&mut self, _render: RenderTileId, data: DataTileId) {
        self.rendered.push(data);
    }
}

/// Whether `drawn` covers every part of `tile`'s ground.
///
/// Exact on the quadtree: a cell is covered when it or an ancestor was drawn, or when all four
/// of its children are covered. `depth` bounds the descent at the deepest tile anyone drew,
/// below which no further subdivision can help.
fn covers(drawn: &BTreeSet<RenderTileId>, tile: RenderTileId, depth: u8) -> bool {
    let mut ancestor = tile;
    loop {
        if drawn.contains(&ancestor) {
            return true;
        }
        if ancestor.z == 0 {
            break;
        }
        ancestor = RenderTileId {
            wrap: ancestor.wrap,
            z: ancestor.z - 1,
            x: ancestor.x / 2,
            y: ancestor.y / 2,
        };
    }
    if tile.z >= depth {
        return false;
    }
    let (z, x, y) = (tile.z + 1, tile.x * 2, tile.y * 2);
    [(x, y), (x, y + 1), (x + 1, y), (x + 1, y + 1)]
        .into_iter()
        .all(|(x, y)| {
            covers(
                drawn,
                RenderTileId {
                    wrap: tile.wrap,
                    z,
                    x,
                    y,
                },
                depth,
            )
        })
}

/// The tiles of a square block at `z`, starting at `(x0, y0)`.
fn block(z: u8, x0: u32, y0: u32, side: u32) -> Vec<DataTileId> {
    (y0..y0 + side)
        .flat_map(|y| (x0..x0 + side).map(move |x| DataTileId::new(z, x, y)))
        .collect()
}

/// Asserts every ideal tile is fully covered by tiles that actually have data.
///
/// The readiness half is not decoration. A coverage predicate that only reads tile *ids* is
/// satisfied by an implementation that draws whatever is nearby regardless of whether it has
/// arrived — the holes are filled with empty tiles, the assertion passes, and the map is blank
/// in exactly the places it claimed to have covered. Found by mutation: dropping the renderable
/// check on the child substitution left every test here passing until this was added.
fn assert_no_holes(world: &World, drawn: &[DataTileId], ideal: &[DataTileId], when: &str) {
    for tile in drawn {
        assert!(
            world.ready.contains(tile),
            "{when}: drew {tile:?}, which has no data"
        );
    }
    let set: BTreeSet<RenderTileId> = drawn.iter().map(|id| id.render_id()).collect();
    let depth = set.iter().map(|id| id.z).max().unwrap_or(0);
    for tile in ideal {
        assert!(
            covers(&set, tile.render_id(), depth),
            "{when}: a hole at {tile:?}; drawn {set:?}"
        );
    }
}

/// The crossing itself: a fully covered z13, then z14, with children arriving one at a time.
///
/// Every intermediate frame must be complete. The frames in the middle are the ones that matter
/// — some children have landed and some have not, which is the state a view is actually in
/// during a crossing and the state a naive implementation draws a checkerboard for.
#[test]
fn a_crossing_never_leaves_a_hole() {
    let parents = block(13, 4090, 2720, 2);
    let children: Vec<DataTileId> = block(14, 8180, 5440, 4);

    let mut world = World::default();
    for &parent in &parents {
        world.arrive(parent);
    }
    let drawn = world.frame(&parents, 0..=16);
    assert_no_holes(&world, &drawn, &parents, "before the crossing");

    // Now the camera is at z14 and every child is missing. Land them one by one.
    for (landed, &child) in children.iter().enumerate() {
        let drawn = world.frame(&children, 0..=16);
        assert_no_holes(
            &world,
            &drawn,
            &children,
            &format!("with {landed} of 16 children"),
        );
        world.arrive(child);
    }
    let drawn = world.frame(&children, 0..=16);
    assert_no_holes(&world, &drawn, &children, "fully landed");
}

/// And in the other direction, which is not symmetric.
///
/// Zooming out, the substitute is a *child* — and children only stand in when all four are
/// present, so the fallback that saves an outward crossing is the one with the stricter
/// condition. Coming from a covered z14 every child is present, which is exactly why it works.
#[test]
fn crossing_outward_never_leaves_a_hole() {
    let children = block(14, 8180, 5440, 4);
    let parents = block(13, 4090, 2720, 2);

    let mut world = World::default();
    for &child in &children {
        world.arrive(child);
    }
    let drawn = world.frame(&children, 0..=16);
    assert_no_holes(&world, &drawn, &children, "before");

    for (landed, &parent) in parents.iter().enumerate() {
        let drawn = world.frame(&parents, 0..=16);
        assert_no_holes(
            &world,
            &drawn,
            &parents,
            &format!("with {landed} of 4 parents"),
        );
        world.arrive(parent);
    }
}

/// Arrival order must not matter, so try every order that fits.
///
/// A crossing where children land in raster order is the easy case. Real arrivals are whatever
/// the network returns first, and an implementation that happened to work for tidy orders would
/// pass a single-order test.
#[test]
fn no_arrival_order_leaves_a_hole() {
    let parents = block(13, 4090, 2720, 2);
    let children = block(14, 8180, 5440, 4);

    // A cheap deterministic shuffle: step through the ring by a stride coprime with its size,
    // which visits every element in an order that is not the natural one.
    for stride in [1usize, 3, 5, 7, 9, 11, 13, 15] {
        let mut world = World::default();
        for &parent in &parents {
            world.arrive(parent);
        }
        for step in 0..children.len() {
            let drawn = world.frame(&children, 0..=16);
            assert_no_holes(
                &world,
                &drawn,
                &children,
                &format!("stride {stride}, step {step}"),
            );
            world.arrive(children[(step * stride) % children.len()]);
        }
    }
}

/// A partial set of children is not a cover, and the parent has to carry the frame.
///
/// The case that distinguishes never-blank from wishful thinking: three of four children ready.
/// Drawing them and leaving the fourth blank is the tempting implementation — it draws the
/// freshest data available — and it is a quarter-tile hole.
#[test]
fn three_of_four_children_still_draws_the_parent() {
    let ideal = vec![DataTileId::new(13, 4090, 2720)];
    let mut world = World::default();
    world.arrive(DataTileId::new(12, 2045, 1360));
    for child in [(8180, 5440), (8181, 5440), (8180, 5441)] {
        world.arrive(DataTileId::new(14, child.0, child.1));
    }

    let drawn = world.frame(&ideal, 0..=16);
    assert_no_holes(&world, &drawn, &ideal, "three of four children");
    assert!(
        drawn.contains(&DataTileId::new(12, 2045, 1360)),
        "the ancestor has to be drawn, since the children do not cover: {drawn:?}"
    );
}

/// From nothing, nothing follows — and the coverage predicate has to say so.
///
/// Without this the tests above would all pass for a `covers` that returned true vacuously,
/// which is the way a suite of coverage assertions usually goes wrong.
#[test]
fn an_empty_pyramid_covers_nothing() {
    let ideal = block(13, 4090, 2720, 2);
    let mut world = World::default();
    let drawn = world.frame(&ideal, 0..=16);
    assert!(drawn.is_empty(), "nothing is available to draw");

    let set: BTreeSet<RenderTileId> = BTreeSet::new();
    assert!(
        !covers(&set, ideal[0].render_id(), 16),
        "an empty render list must not read as covering anything"
    );
}

/// Zooming out onto children that are known but not yet built.
///
/// The state a real outward crossing is in: the deeper level was being fetched when the gesture
/// reversed, so its tiles exist in the pyramid and are not drawable. An implementation that
/// substitutes a child without checking readiness draws them, reports a full cover, and shows
/// blank ground — which is why coverage is asserted over tiles that have data rather than over
/// tile ids.
#[test]
fn a_pending_child_is_not_a_substitute() {
    let ideal = vec![DataTileId::new(13, 4090, 2720)];
    let mut world = World::default();
    world.arrive(DataTileId::new(12, 2045, 1360));
    // All four children known, two of them still in flight.
    world.arrive(DataTileId::new(14, 8180, 5440));
    world.arrive(DataTileId::new(14, 8181, 5440));
    world.begin(DataTileId::new(14, 8180, 5441));
    world.begin(DataTileId::new(14, 8181, 5441));

    let drawn = world.frame(&ideal, 0..=16);
    assert_no_holes(&world, &drawn, &ideal, "two children in flight");
    assert!(
        drawn.contains(&DataTileId::new(12, 2045, 1360)),
        "the ancestor carries the frame: {drawn:?}"
    );
}

/// And the same for an ancestor that is known but not built.
///
/// The ascent creates ancestors as it climbs, so a pyramid mid-crossing is full of parent tiles
/// that exist and cannot be drawn. Rendering one covers nothing.
#[test]
fn a_pending_ancestor_is_not_a_substitute() {
    let ideal = vec![DataTileId::new(13, 4090, 2720)];
    let mut world = World::default();
    // z12 known but in flight; z11 is the one that can actually carry the frame.
    world.begin(DataTileId::new(12, 2045, 1360));
    world.arrive(DataTileId::new(11, 1022, 680));

    let drawn = world.frame(&ideal, 0..=16);
    assert_no_holes(&world, &drawn, &ideal, "the nearest ancestor is in flight");
    assert!(
        drawn.contains(&DataTileId::new(11, 1022, 680)),
        "the ascent has to pass the pending z12 and reach z11: {drawn:?}"
    );
}
