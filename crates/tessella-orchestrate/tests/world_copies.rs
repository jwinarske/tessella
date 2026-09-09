//! A globe view asks for one world copy, through the surface a consumer sets it on.
//!
//! # What this is
//!
//! The producer's whole part in the globe (§13.4). A globe bends Mercator geometry per vertex in
//! the consumer's material, so *placement* is not the producer's business and what travels on
//! the wire is the ordinary flat placement either way. **Selection** is: a Mercator plane repeats
//! horizontally and a sphere does not, so every wrap of a tile bends to the same patch and a
//! globe drawing a flat cover draws that patch once per copy — z-fighting on the surface, and
//! subdivision paid twice at the levels where subdivision is dearest, since a z1 tile splits to
//! ninety segments an edge.
//!
//! `globe_cover` measured the size of it: at zoom 0 four of five cover tiles are copies and at
//! zoom 1 four of eight. Most of the cover, not an edge case.
//!
//! # Why folded and not filtered
//!
//! A tile visible *only* at `wrap: -1` is still a patch of the sphere. Filtering to `wrap == 0`
//! would drop exactly those, which is a hole rather than a saving, and the view centred on the
//! antimeridian below is the case that shows it: its western half has no `wrap: 0` entry at all.

use std::collections::BTreeSet;

use tessella_orchestrate::viewcover::{Update, ViewCover};
use tessella_tile::cover::{TileCoord, ViewTransform, WorldCopies};
use tessella_tile::store::Surface;

fn at(zoom: f64, longitude: f64) -> ViewTransform {
    ViewTransform {
        longitude,
        latitude: 0.0,
        zoom,
        // Wide enough at a low zoom to hold more than one world, which is where the copies are.
        width: 1280.0,
        height: 720.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

/// Every tile a globe is given is a distinct patch of the sphere.
#[test]
fn a_globe_cover_holds_no_copies() {
    // Zero and one only, and the guard at the bottom is why: by z2 a 1280-pixel viewport holds
    // less than one world, there are no copies to fold, and the zoom would assert nothing. That
    // is §13.4's shape — the copies are concentrated below z2 rather than spread over a sweep.
    for zoom in [0.0, 1.0] {
        let flat =
            ViewCover::new(&at(zoom, 0.0), WorldCopies::Repeated, Surface::Plane).expect("covers");
        let globe =
            ViewCover::new(&at(zoom, 0.0), WorldCopies::One, Surface::Plane).expect("covers");

        assert!(
            globe.tiles().iter().all(|tile| tile.wrap == 0),
            "z{zoom}: a globe was given a tile on another world copy: {:?}",
            globe.tiles()
        );

        // Distinct patches, which is the property the fold exists for: the flat cover names the
        // same (z, x, y) several times over and the globe names each once.
        let patches: BTreeSet<(u8, u32, u32)> = globe
            .tiles()
            .iter()
            .map(|tile| (tile.z, tile.x, tile.y))
            .collect();
        assert_eq!(
            patches.len(),
            globe.tiles().len(),
            "z{zoom}: a globe cover repeats a patch"
        );

        // And the flat cover is the thing being fixed rather than already correct: at these
        // zooms it holds copies, so the assertion above is not vacuous.
        let flat_patches: BTreeSet<(u8, u32, u32)> = flat
            .tiles()
            .iter()
            .map(|tile| (tile.z, tile.x, tile.y))
            .collect();
        assert!(
            flat_patches.len() < flat.tiles().len(),
            "z{zoom}: the flat cover holds no copies, so this zoom proves nothing"
        );
    }
}

/// The fold keeps the ground, which filtering to `wrap == 0` would not.
///
/// Centred on the antimeridian, half the visible world is reached only at a non-zero wrap. The
/// globe must still be given those patches.
#[test]
fn a_view_on_the_antimeridian_keeps_its_western_half() {
    let flat =
        ViewCover::new(&at(2.0, 180.0), WorldCopies::Repeated, Surface::Plane).expect("covers");
    let globe = ViewCover::new(&at(2.0, 180.0), WorldCopies::One, Surface::Plane).expect("covers");

    let wanted: BTreeSet<(u8, u32, u32)> = flat
        .tiles()
        .iter()
        .map(|tile| (tile.z, tile.x, tile.y))
        .collect();
    let held: BTreeSet<(u8, u32, u32)> = globe
        .tiles()
        .iter()
        .map(|tile| (tile.z, tile.x, tile.y))
        .collect();

    assert!(
        flat.tiles().iter().any(|tile| tile.wrap != 0),
        "this view does not cross the antimeridian, so it proves nothing"
    );
    assert_eq!(
        held,
        wanted,
        "the globe lost ground the plane covers: {:?}",
        wanted.difference(&held).collect::<Vec<_>>()
    );
}

/// Switching surface recomputes on the frame it switches, by the path a pan takes.
///
/// The policy is passed per frame rather than held, so there is no invalidation to forget: the
/// cover is rebuilt every frame anyway and the comparison against the previous one is what
/// reports the change.
#[test]
fn changing_the_surface_moves_the_cover() {
    let view = at(1.0, 0.0);
    let mut state = ViewCover::new(&view, WorldCopies::Repeated, Surface::Plane).expect("covers");
    let flat: Vec<TileCoord> = state.tiles().to_vec();

    assert_eq!(
        state
            .update(&view, WorldCopies::One, Surface::Plane)
            .expect("covers"),
        Update::Changed,
        "the surface changed and the cover did not"
    );
    assert_ne!(state.tiles(), flat.as_slice(), "the cover did not fold");
    assert!(
        !state.left().is_empty(),
        "the copies left the cover without being reported, so nothing downstream releases them"
    );

    assert_eq!(
        state
            .update(&view, WorldCopies::One, Surface::Plane)
            .expect("covers"),
        Update::Unchanged,
        "a settled globe view is not settled"
    );
    assert_eq!(
        state
            .update(&view, WorldCopies::Repeated, Surface::Plane)
            .expect("covers"),
        Update::Changed,
        "switching back did not move the cover"
    );
    assert_eq!(state.tiles(), flat.as_slice(), "switching back lost tiles");
}
