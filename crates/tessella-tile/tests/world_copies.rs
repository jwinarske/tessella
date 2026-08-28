//! `WorldCopies::One`, which is the producer's whole part in the globe (§13.4).
//!
//! # What the policy is for
//!
//! A Mercator plane repeats horizontally, and a viewport straddling the antimeridian sees the
//! same tile in two copies. Drawing both is the map being *right*: the two copies are different
//! places on the screen.
//!
//! A sphere has one of everything. Every wrap of a tile bends to the same patch, so a globe view
//! drawing a repeated cover draws that patch twice — z-fighting on the surface, and paying
//! subdivision twice at the zooms where an edge splits into ninety segments.
//!
//! # Why folding rather than filtering
//!
//! The obvious implementation keeps the tiles whose `wrap` is zero. That is wrong at exactly the
//! case the policy exists for: a view centred near the antimeridian can see a tile *only* at
//! `wrap: -1`, and filtering would leave a hole in the globe where the patch belongs.

use tessella_tile::cover::{ViewTransform, WorldCopies, cover, cover_with};

fn view(longitude: f64, zoom: f64) -> ViewTransform {
    ViewTransform {
        longitude,
        latitude: 0.0,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

/// At the zooms where the world fits on the screen, most of a flat cover is copies.
#[test]
fn a_low_zoom_cover_is_mostly_copies() {
    for zoom in [0.0, 1.0] {
        let flat = cover(&view(0.0, zoom)).expect("covers");
        let globe = cover_with(&view(0.0, zoom), WorldCopies::One).expect("covers");
        assert!(
            globe.len() < flat.len(),
            "z{zoom}: {} tiles either way, so there was nothing to fold",
            flat.len()
        );
        assert!(
            globe.iter().all(|tile| tile.wrap == 0),
            "z{zoom}: a single-world cover has one copy of everything"
        );
    }
}

/// The same tiles, once each: folding removes copies and nothing else.
#[test]
fn folding_keeps_every_distinct_patch() {
    let flat = cover(&view(0.0, 1.0)).expect("covers");
    let globe = cover_with(&view(0.0, 1.0), WorldCopies::One).expect("covers");

    let mut expected: Vec<(u8, u32, u32)> = flat.iter().map(|t| (t.z, t.x, t.y)).collect();
    expected.sort_unstable();
    expected.dedup();

    let got: Vec<(u8, u32, u32)> = globe.iter().map(|t| (t.z, t.x, t.y)).collect();
    assert_eq!(got, expected, "every distinct patch survives, exactly once");
}

/// A patch visible only in a far copy is kept, not dropped.
///
/// This is what separates folding from filtering, and it is the case the policy exists for: a
/// globe centred on the antimeridian sees tiles whose only cover entry has a non-zero wrap.
#[test]
fn a_patch_seen_only_in_a_far_copy_survives() {
    // Near the antimeridian, so the viewport straddles it and half the cover wraps.
    let at = view(179.9, 2.0);
    let flat = cover(&at).expect("covers");
    let globe = cover_with(&at, WorldCopies::One).expect("covers");

    assert!(
        flat.iter().any(|tile| tile.wrap != 0),
        "the fixture is pointless unless this view actually wraps"
    );

    let mut expected: Vec<(u8, u32, u32)> = flat.iter().map(|t| (t.z, t.x, t.y)).collect();
    expected.sort_unstable();
    expected.dedup();
    let got: Vec<(u8, u32, u32)> = globe.iter().map(|t| (t.z, t.x, t.y)).collect();
    assert_eq!(
        got, expected,
        "a patch whose only entry wrapped is still part of the sphere"
    );
}

/// Above z3 the two policies agree, which is why §13.3's sweep says nothing about the globe.
///
/// `globe_cover` measured the same boundary from the other side: the horizon stops cutting
/// anything by z3, and the copies stop appearing at about the same place, because both are
/// consequences of the world no longer fitting on the screen.
#[test]
fn the_policies_agree_once_the_world_is_larger_than_the_screen() {
    for zoom in [3.0, 5.0, 8.0, 13.0, 16.0] {
        let flat = cover(&view(-0.11, zoom)).expect("covers");
        let globe = cover_with(&view(-0.11, zoom), WorldCopies::One).expect("covers");
        assert_eq!(
            flat, globe,
            "z{zoom}: a globe and a plane want the same tiles here"
        );
    }
}

/// The default is the plane, so nothing that does not ask for a globe changes.
#[test]
fn the_default_is_the_repeated_world() {
    assert_eq!(WorldCopies::default(), WorldCopies::Repeated);
    let at = view(179.9, 2.0);
    assert_eq!(
        cover(&at).expect("covers"),
        cover_with(&at, WorldCopies::Repeated).expect("covers")
    );
}
