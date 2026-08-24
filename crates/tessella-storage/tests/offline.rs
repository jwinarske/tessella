//! Sizing an offline region — what a user is told before they agree to a download.

use tessella_storage::offline::{
    Area, Estimate, Region, SourceContribution, SourceKind, StyleAssets, covering_zoom_range,
    estimate,
};
use tessella_storage::url::ZoomRange;
use tessella_tile::cover::Bounds;

fn berlin(min_zoom: f64, max_zoom: f64) -> Region {
    Region {
        style_url: "https://host/style.json".into(),
        area: Area::Box(Bounds::new(13.0, 52.3, 13.8, 52.7)),
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

    let all_known = estimate(
        &region,
        std::slice::from_ref(&known),
        StyleAssets::default(),
    );
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
        area: Area::Box(Bounds::new(-5.0, 41.0, 9.0, 51.0)),
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

// --- Planning: from a style and a region to the URLs a download fetches. ---

use std::collections::BTreeMap;

use tessella_storage::offline::{Plan, font_stacks, plan};
use tessella_storage::tileset::TileSet;
use tessella_storage::url::Scheme;

fn style(json: &str) -> tessella_style::Style {
    serde_json::from_str(json).expect("a style")
}

fn manifest(template: &str, min: u8, max: u8) -> TileSet {
    TileSet {
        templates: vec![template.to_string()],
        zooms: ZoomRange { min, max },
        scheme: Scheme::Xyz,
    }
}

fn tiny() -> Region {
    Region {
        style_url: "https://host/style.json".into(),
        // A box inside one tile at every zoom it is planned for.
        area: Area::Box(Bounds::new(13.40, 52.51, 13.41, 52.52)),
        min_zoom: 4.0,
        max_zoom: 5.0,
        pixel_ratio: 1.0,
        include_ideographs: false,
    }
}

/// A plan names the manifest, the tiles, the glyph ranges and both sprite densities.
#[test]
fn a_plan_names_everything_the_style_needs() {
    let style = style(
        r#"{
          "version": 8,
          "sprite": "https://host/sprite",
          "glyphs": "https://host/fonts/{fontstack}/{range}.pbf",
          "sources": {
            "base": { "type": "vector", "url": "https://host/base.json" }
          },
          "layers": [
            { "id": "labels", "type": "symbol", "source": "base",
              "layout": { "text-font": ["Noto Sans Regular"] } }
          ]
        }"#,
    );
    let manifests = BTreeMap::from([(
        "base".to_string(),
        manifest("https://host/{z}/{x}/{y}.mvt", 0, 14),
    )]);

    let plan = plan(&style, &tiny(), &manifests);
    assert!(plan.complete);

    assert!(plan.assets.contains(&"https://host/base.json".to_string()));
    // Five non-ideograph ranges for the one stack.
    assert!(
        plan.assets
            .contains(&"https://host/fonts/Noto%20Sans%20Regular/0-255.pbf".to_string())
    );
    assert!(
        plan.assets
            .contains(&"https://host/fonts/Noto%20Sans%20Regular/1024-1279.pbf".to_string())
    );
    // Both densities of both sprite files: a region downloaded on one display scale may be
    // viewed at another, and a missing sheet is a map with no icons.
    for expected in [
        "https://host/sprite.json",
        "https://host/sprite.png",
        "https://host/sprite@2x.json",
        "https://host/sprite@2x.png",
    ] {
        assert!(plan.assets.contains(&expected.to_string()), "{expected}");
    }

    // Zooms 4 and 5, one tile each for a box this small.
    assert_eq!(
        plan.tiles,
        vec![
            "https://host/4/8/5.mvt".to_string(),
            "https://host/5/17/10.mvt".to_string(),
        ]
    );
}

/// A source whose manifest has not been resolved contributes no tiles, and says so.
///
/// Its zoom range lives in that manifest, so planning without it would either invent a range or
/// silently omit the source. The plan omits it and reports itself incomplete, which is what
/// makes "resolve manifests, then plan" the required order rather than a preference.
#[test]
fn an_unresolved_manifest_makes_the_plan_incomplete() {
    let style = style(
        r#"{
          "version": 8,
          "sources": { "base": { "type": "vector", "url": "https://host/base.json" } },
          "layers": []
        }"#,
    );
    let plan = plan(&style, &tiny(), &BTreeMap::new());
    assert!(!plan.complete);
    assert!(plan.tiles.is_empty());
    assert_eq!(plan.assets, vec!["https://host/base.json".to_string()]);
}

/// An inline source needs no manifest fetch and still yields tiles.
#[test]
fn an_inline_source_needs_no_manifest() {
    let style = style(
        r#"{
          "version": 8,
          "sources": {
            "base": { "type": "vector", "tiles": ["https://host/{z}/{x}/{y}.mvt"], "maxzoom": 14 }
          },
          "layers": []
        }"#,
    );
    let manifests = BTreeMap::from([(
        "base".to_string(),
        manifest("https://host/{z}/{x}/{y}.mvt", 0, 14),
    )]);
    let plan = plan(&style, &tiny(), &manifests);
    assert!(plan.complete);
    assert!(plan.assets.is_empty(), "nothing to fetch but the tiles");
    assert_eq!(plan.tiles.len(), 2);
}

/// A sharded source is fetched once per tile, not once per host.
///
/// Several templates exist so a browser can open more connections, not because the tile differs
/// between them. Fetching each would download the region two or three times over.
#[test]
fn a_sharded_source_downloads_each_tile_once() {
    let style = style(
        r#"{
          "version": 8,
          "sources": { "base": { "type": "vector", "url": "https://host/base.json" } },
          "layers": []
        }"#,
    );
    let manifests = BTreeMap::from([(
        "base".to_string(),
        TileSet {
            templates: vec![
                "https://a.host/{z}/{x}/{y}.mvt".into(),
                "https://b.host/{z}/{x}/{y}.mvt".into(),
                "https://c.host/{z}/{x}/{y}.mvt".into(),
            ],
            zooms: ZoomRange { min: 0, max: 14 },
            scheme: Scheme::Xyz,
        },
    )]);

    let plan = plan(&style, &tiny(), &manifests);
    assert_eq!(plan.tiles.len(), 2, "two zooms, one tile each");
    let unique: std::collections::BTreeSet<_> = plan
        .tiles
        .iter()
        .map(|url| url.rsplit_once("host/").expect("a host").1.to_string())
        .collect();
    assert_eq!(
        unique.len(),
        2,
        "two distinct tiles, however they are shared"
    );
}

/// A GeoJSON source by URL is one document; one written into the style is free.
#[test]
fn geojson_costs_a_document_only_when_it_is_remote() {
    let remote = style(
        r#"{
          "version": 8,
          "sources": { "points": { "type": "geojson", "data": "https://host/points.json" } },
          "layers": []
        }"#,
    );
    assert_eq!(
        plan(&remote, &tiny(), &BTreeMap::new()).assets,
        vec!["https://host/points.json".to_string()]
    );

    let inline = style(
        r#"{
          "version": 8,
          "sources": {
            "points": { "type": "geojson",
                        "data": { "type": "FeatureCollection", "features": [] } }
          },
          "layers": []
        }"#,
    );
    let plan = plan(&inline, &tiny(), &BTreeMap::new());
    assert!(plan.assets.is_empty());
    assert!(plan.complete);
}

/// A data-driven `text-font` names fonts only the features reveal, so the plan is a lower bound.
///
/// Shipping a region whose labels have no glyphs, and calling it complete, is the failure worth
/// avoiding here — the map renders, and every label is missing.
#[test]
fn a_data_driven_font_makes_the_plan_incomplete() {
    let style = style(
        r#"{
          "version": 8,
          "glyphs": "https://host/fonts/{fontstack}/{range}.pbf",
          "sources": {},
          "layers": [
            { "id": "a", "type": "symbol", "layout": { "text-font": ["get", "font"] } }
          ]
        }"#,
    );
    let (stacks, all_found) = font_stacks(&style);
    assert!(stacks.is_empty());
    assert!(!all_found);
    assert!(!plan(&style, &tiny(), &BTreeMap::new()).complete);
}

/// `["literal", [...]]` states its fonts, so it is enumerable where `["get", …]` is not.
#[test]
fn a_literal_font_expression_is_enumerable() {
    let style = style(
        r#"{
          "version": 8,
          "glyphs": "https://host/fonts/{fontstack}/{range}.pbf",
          "sources": {},
          "layers": [
            { "id": "a", "type": "symbol",
              "layout": { "text-font": ["literal", ["Noto Sans Bold", "Arial Unicode MS Bold"]] } }
          ]
        }"#,
    );
    let (stacks, all_found) = font_stacks(&style);
    assert!(all_found);
    assert_eq!(
        stacks,
        vec!["Noto Sans Bold,Arial Unicode MS Bold".to_string()]
    );
}

/// Two layers sharing a stack download its glyphs once.
#[test]
fn a_shared_font_stack_is_fetched_once() {
    let style = style(
        r#"{
          "version": 8,
          "glyphs": "https://host/fonts/{fontstack}/{range}.pbf",
          "sources": {},
          "layers": [
            { "id": "a", "type": "symbol", "layout": { "text-font": ["Noto Sans Regular"] } },
            { "id": "b", "type": "symbol", "layout": { "text-font": ["Noto Sans Regular"] } },
            { "id": "c", "type": "symbol", "layout": { "text-font": ["Noto Sans Bold"] } }
          ]
        }"#,
    );
    assert_eq!(font_stacks(&style).0.len(), 2);
    assert_eq!(plan(&style, &tiny(), &BTreeMap::new()).assets.len(), 2 * 5);
}

/// Ideographs multiply the glyph download by fifty, which is why they are opt-in.
#[test]
fn ideographs_multiply_the_glyph_plan() {
    let style = style(
        r#"{
          "version": 8,
          "glyphs": "https://host/fonts/{fontstack}/{range}.pbf",
          "sources": {},
          "layers": [
            { "id": "a", "type": "symbol", "layout": { "text-font": ["Noto Sans Regular"] } }
          ]
        }"#,
    );
    let mut region = tiny();
    region.include_ideographs = true;
    assert_eq!(plan(&style, &region, &BTreeMap::new()).assets.len(), 256);
    assert_eq!(plan(&style, &tiny(), &BTreeMap::new()).assets.len(), 5);
}

/// A style with nothing in it plans nothing, and that is complete rather than unknown.
#[test]
fn an_empty_style_plans_nothing() {
    let style = style(r#"{ "version": 8, "sources": {}, "layers": [] }"#);
    assert_eq!(
        plan(&style, &tiny(), &BTreeMap::new()),
        Plan {
            complete: true,
            ..Plan::default()
        }
    );
}

/// A plain font stack is not a call to an operator named after its first font.
///
/// `["Noto Sans Regular"]` is syntactically indistinguishable from an expression, and the style
/// crate classifies it as one by design. Reading it that way here would lose every glyph in the
/// style — a map that renders with no labels at all, which is the exact failure this planning
/// exists to prevent.
#[test]
fn a_plain_stack_is_not_mistaken_for_an_expression() {
    let style = style(
        r#"{
          "version": 8,
          "glyphs": "https://host/fonts/{fontstack}/{range}.pbf",
          "sources": {},
          "layers": [
            { "id": "a", "type": "symbol",
              "layout": { "text-font": ["Noto Sans Regular", "Arial Unicode MS Regular"] } }
          ]
        }"#,
    );
    let (stacks, all_found) = font_stacks(&style);
    assert!(all_found);
    assert_eq!(
        stacks,
        vec!["Noto Sans Regular,Arial Unicode MS Regular".to_string()]
    );
}

/// A stack and an expression are told apart by whether the head names an operator.
#[test]
fn an_operator_head_is_read_as_an_expression() {
    for (layout, expect_stacks, expect_complete) in [
        (r#"["step", ["zoom"], ["A"], 8, ["B"]]"#, 0, false),
        (r#"["match", ["get", "x"], "a", ["A"], ["B"]]"#, 0, false),
        (r#"["coalesce", ["get", "font"], ["A"]]"#, 0, false),
        (r#"["Open Sans Regular"]"#, 1, true),
        (r#"["literal", ["Open Sans Regular"]]"#, 1, true),
    ] {
        let style = style(&format!(
            r#"{{ "version": 8, "glyphs": "https://host/{{fontstack}}/{{range}}.pbf",
                  "sources": {{}},
                  "layers": [ {{ "id": "a", "type": "symbol",
                                 "layout": {{ "text-font": {layout} }} }} ] }}"#
        ));
        let (stacks, all_found) = font_stacks(&style);
        assert_eq!(stacks.len(), expect_stacks, "{layout}");
        assert_eq!(all_found, expect_complete, "{layout}");
    }
}
