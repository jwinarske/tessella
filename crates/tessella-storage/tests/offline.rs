//! Sizing an offline region — what a user is told before they agree to a download.

use tessella_storage::offline::{
    Estimate, Region, SourceContribution, SourceKind, StyleAssets, covering_zoom_range, estimate,
};
use tessella_storage::url::ZoomRange;
use tessella_tile::cover::Bounds;

fn berlin(min_zoom: f64, max_zoom: f64) -> Region {
    Region {
        style_url: "https://host/style.json".into(),
        bounds: Bounds::new(13.0, 52.3, 13.8, 52.7),
        min_zoom,
        max_zoom,
        pixel_ratio: 1.0,
        include_ideographs: false,
    }
}

/// A region takes the zooms it and the source have in common, not the ones it asked for.
///
/// Asking a source that stops at zoom 6 for zoom 14 tiles produces a download of 404s, and a
/// progress bar that never completes.
#[test]
fn the_zoom_range_is_an_intersection() {
    let region = berlin(0.0, 14.0);
    let shallow = ZoomRange { min: 0, max: 6 };
    assert_eq!(
        covering_zoom_range(&region, SourceKind::Vector, shallow),
        (0, 6)
    );

    let deep = ZoomRange { min: 10, max: 20 };
    assert_eq!(
        covering_zoom_range(&region, SourceKind::Vector, deep),
        (10, 14)
    );
}

/// A source with no overlap contributes nothing, and is not an error.
///
/// A style may carry a source that simply has nothing at the zooms asked for — a building
/// layer that starts at 14 in a region capped at 10.
#[test]
fn a_source_with_no_overlap_contributes_nothing() {
    let region = berlin(0.0, 10.0);
    let deep = ZoomRange { min: 14, max: 16 };
    assert_eq!(region.tile_count(SourceKind::Vector, deep), 0);
    assert!(
        region
            .tiles(SourceKind::Vector, deep, 1_000)
            .expect("no error")
            .is_empty()
    );
}

/// A 256-pixel raster source is shifted a whole level, at both ends.
///
/// mbgl's `coveringZoomLevel` adds `log2(512 / tileSize)` and *rounds* for raster where it
/// floors for vector, and `coveringZoomRange` applies it to `minZoom` and `maxZoom` alike. So a
/// 256 source asked for map zooms 0..10 downloads tile zooms 1..11, not 0..11: at map zoom 0 a
/// 256-pixel tile covers a quarter of what the view needs, and z1 is the level that fills it.
///
/// Getting the shift wrong makes a raster basemap consistently one level too coarse, which
/// looks like a blurry map rather than like a download bug.
#[test]
fn raster_tile_size_shifts_the_zoom() {
    let region = berlin(0.0, 10.0);
    let all = ZoomRange { min: 0, max: 22 };

    assert_eq!(
        covering_zoom_range(&region, SourceKind::Vector, all),
        (0, 10)
    );
    assert_eq!(
        covering_zoom_range(&region, SourceKind::Raster { tile_size: 512 }, all),
        (0, 10),
        "a 512 raster matches vector"
    );
    assert_eq!(
        covering_zoom_range(&region, SourceKind::Raster { tile_size: 256 }, all),
        (1, 11),
        "a 256 raster is shifted a level at both ends"
    );
}

/// The count and the enumeration agree across the whole pyramid.
#[test]
fn the_count_matches_what_is_enumerated() {
    let region = berlin(0.0, 12.0);
    let zooms = ZoomRange { min: 0, max: 14 };
    let counted = region.tile_count(SourceKind::Vector, zooms);
    let listed = region
        .tiles(SourceKind::Vector, zooms, 1_000_000)
        .expect("enumerates")
        .len() as u64;
    assert_eq!(counted, listed);
}

/// An estimate counts the style, each source's manifest, its tiles, the glyphs and the sprites.
#[test]
fn an_estimate_adds_up() {
    let region = berlin(0.0, 8.0);
    let zooms = ZoomRange { min: 0, max: 14 };
    let tiles = region.tile_count(SourceKind::Vector, zooms);

    let assets = StyleAssets {
        font_stacks: 2,
        font_faces: 0,
        sprites: 1,
        has_glyphs: true,
    };
    let estimated = estimate(
        &region,
        &[SourceContribution::Tiles {
            kind: SourceKind::Vector,
            zooms,
            from_manifest: true,
        }],
        assets,
    );

    assert!(estimated.precise);
    assert_eq!(estimated.tiles, tiles);
    // The style, the manifest, the tiles, two font stacks of five ranges, four sprite files.
    assert_eq!(estimated.resources, 1 + 1 + tiles + 2 * 5 + 4);
}

/// Ideographs are most of a glyph download, which is why including them is a choice.
#[test]
fn ideographs_dominate_the_glyph_count() {
    let assets = StyleAssets {
        font_stacks: 3,
        font_faces: 0,
        sprites: 0,
        has_glyphs: true,
    };
    let without = estimate(&berlin(0.0, 4.0), &[], assets);

    let mut region = berlin(0.0, 4.0);
    region.include_ideographs = true;
    let with = estimate(&region, &[], assets);

    assert_eq!(without.resources, 1 + 3 * 5);
    assert_eq!(with.resources, 1 + 3 * 256);
    assert!(with.resources > without.resources * 10);
}

/// A source whose manifest has not arrived makes the estimate a lower bound, and says so.
///
/// A total that silently grows as a download proceeds is confusing; one that claims a precision
/// it does not have is worse. mbgl reports the same thing as
/// `requiredResourceCountIsPrecise`.
#[test]
fn an_unfetched_manifest_makes_the_estimate_imprecise() {
    let region = berlin(0.0, 8.0);
    let known = SourceContribution::Tiles {
        kind: SourceKind::Vector,
        zooms: ZoomRange { min: 0, max: 14 },
        from_manifest: false,
    };

    let all_known = estimate(&region, std::slice::from_ref(&known), StyleAssets::default());
    assert!(all_known.precise);

    let one_unknown = estimate(
        &region,
        &[known, SourceContribution::Unknown],
        StyleAssets::default(),
    );
    assert!(!one_unknown.precise, "a lower bound, and it says so");
    assert_eq!(
        one_unknown.resources,
        all_known.resources + 1,
        "the manifest is counted; its tiles are not yet knowable"
    );
    assert_eq!(one_unknown.tiles, all_known.tiles);
}

/// An inline source costs no manifest fetch.
#[test]
fn an_inline_source_costs_no_manifest() {
    let region = berlin(0.0, 6.0);
    let zooms = ZoomRange { min: 0, max: 14 };
    let inline = estimate(
        &region,
        &[SourceContribution::Tiles {
            kind: SourceKind::Vector,
            zooms,
            from_manifest: false,
        }],
        StyleAssets::default(),
    );
    let remote = estimate(
        &region,
        &[SourceContribution::Tiles {
            kind: SourceKind::Vector,
            zooms,
            from_manifest: true,
        }],
        StyleAssets::default(),
    );
    assert_eq!(remote.resources, inline.resources + 1);
    assert_eq!(remote.tiles, inline.tiles);
}

/// A region with no sources is the style and its assets, not nothing.
#[test]
fn an_empty_style_still_costs_its_own_document() {
    assert_eq!(
        estimate(&berlin(0.0, 4.0), &[], StyleAssets::default()),
        Estimate {
            tiles: 0,
            resources: 1,
            precise: true,
        }
    );
}

/// Sizing a country at street zoom is answerable without attempting it.
///
/// The question is asked so it can be declined. Answering by building the list would make
/// asking as expensive as agreeing.
#[test]
fn a_large_region_is_countable_but_not_enumerable() {
    let france = Region {
        style_url: "https://host/style.json".into(),
        bounds: Bounds::new(-5.0, 41.0, 9.0, 51.0),
        min_zoom: 0.0,
        max_zoom: 16.0,
        pixel_ratio: 1.0,
        include_ideographs: false,
    };
    let zooms = ZoomRange { min: 0, max: 16 };

    let count = france.tile_count(SourceKind::Vector, zooms);
    assert!(count > 1_000_000, "{count} tiles");
    assert!(
        france.tiles(SourceKind::Vector, zooms, 100_000).is_err(),
        "refused rather than attempted"
    );
}
