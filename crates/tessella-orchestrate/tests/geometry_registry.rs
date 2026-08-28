//! Ids that belong to a drawable rather than to a position in this frame's cover.
//!
//! # The failure it exists to remove
//!
//! `geometry_ids.rs` pins what the producer does today: ids dense from zero every frame, handed
//! out in cover order, so a pan gives the same id to a different tile. A consumer told the ids
//! are process-scoped — which the ABI said until recently — caches on one and draws one tile's
//! geometry under another's matrix.
//!
//! Everything here is about the case that breaks: a cover that *changes* while overlapping. A
//! registry keyed on anything positional passes a test that only pans by a whole cover.

use tessella_orchestrate::registry::{DrawableKey, GeometryRegistry};
use tessella_orchestrate::tile::TileId;

fn key(x: u32, layer: i32, sub: i32) -> DrawableKey {
    DrawableKey {
        tile: Some(TileId::new(3, x, 4)),
        layer_index: layer,
        sub_layer_index: sub,
    }
}

/// A tile that stays in the cover keeps its id, even as its neighbours change.
#[test]
fn a_tile_that_stays_keeps_its_id() {
    let mut registry = GeometryRegistry::new();

    registry.begin_frame();
    let first: Vec<_> = [1, 2, 3].map(|x| registry.id_for(key(x, 0, 1))).to_vec();

    // Pan by one: tile 1 leaves, tile 4 arrives, tiles 2 and 3 stay.
    registry.begin_frame();
    let second: Vec<_> = [2, 3, 4].map(|x| registry.id_for(key(x, 0, 1))).to_vec();

    assert_eq!(
        (&first[1], &first[2]),
        (&second[0], &second[1]),
        "the tiles present in both covers keep their ids"
    );
    assert!(
        !first.contains(&second[2]),
        "the tile that arrived gets an id nothing had"
    );
}

/// A fill's triangles and its outline are two drawables over one tile.
#[test]
fn sub_layers_are_separate_drawables() {
    let mut registry = GeometryRegistry::new();
    registry.begin_frame();
    let triangles = registry.id_for(key(1, 0, 1));
    let outline = registry.id_for(key(1, 0, 2));
    assert_ne!(triangles, outline);
    assert_eq!(registry.len(), 2);
}

/// A drawable the frame did not ask for is reported, then dropped.
///
/// Reporting and dropping are separate calls so a frame that could not be written retires
/// nothing — the same reason `frame::emit` rewinds its arena rather than trusting a partial
/// pass.
#[test]
fn what_a_frame_stops_using_is_reported_before_it_is_dropped() {
    let mut registry = GeometryRegistry::new();
    registry.begin_frame();
    let leaving = registry.id_for(key(1, 0, 1));
    registry.id_for(key(2, 0, 1));

    registry.begin_frame();
    registry.id_for(key(2, 0, 1));

    let retired = registry.retired();
    assert_eq!(retired.len(), 1, "one drawable left the cover: {retired:?}");
    assert_eq!(retired[0].1, leaving);
    assert_eq!(registry.len(), 2, "reporting does not drop");

    registry.retire();
    assert_eq!(registry.len(), 1, "retiring does");
}

/// An id is never handed out twice, even after its drawable is gone.
///
/// A consumer told to remove a geometry and then handed the same id for something else has no
/// way to tell the second from a duplicate of the first. Retiring the number costs eight bytes
/// of counter and removes the ambiguity.
#[test]
fn an_id_is_never_reused() {
    let mut registry = GeometryRegistry::new();
    registry.begin_frame();
    let first = registry.id_for(key(1, 0, 1));
    registry.begin_frame();
    registry.retire();
    assert!(registry.is_empty());

    registry.begin_frame();
    let second = registry.id_for(key(9, 0, 1));
    assert_ne!(first, second, "a retired id is not handed out again");

    // Not even to the very drawable that had it.
    registry.begin_frame();
    let again = registry.id_for(key(1, 0, 1));
    assert_ne!(first, again, "the key came back, the id did not");
}

/// A drawable with no tile — a background — is keyed like any other.
#[test]
fn a_viewport_drawable_has_a_key_too() {
    let mut registry = GeometryRegistry::new();
    registry.begin_frame();
    let background = registry.id_for(DrawableKey {
        tile: None,
        layer_index: 0,
        sub_layer_index: 0,
    });
    let tiled = registry.id_for(key(1, 0, 0));
    assert_ne!(background, tiled);

    registry.begin_frame();
    assert_eq!(
        registry.id_for(DrawableKey {
            tile: None,
            layer_index: 0,
            sub_layer_index: 0,
        }),
        background,
        "it is stable across frames like anything else"
    );
}

/// `is_new` answers before the id is asked for, which is what gates the emission.
#[test]
fn a_new_drawable_is_distinguishable_from_a_known_one() {
    let mut registry = GeometryRegistry::new();
    let first = key(1, 0, 1);

    registry.begin_frame();
    assert!(registry.is_new(&first));
    registry.id_for(first);
    assert!(!registry.is_new(&first), "asking for it makes it known");

    registry.begin_frame();
    assert!(!registry.is_new(&first), "and it stays known across frames");
}
