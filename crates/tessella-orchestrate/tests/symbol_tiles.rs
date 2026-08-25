//! A symbol layer through the tile builder, in the two phases it actually has.
//!
//! Every other layer type turns features into vertices in one pass. A symbol layer cannot:
//! shaping needs glyph metrics, and the glyph URL is not known until `text-field` has been
//! evaluated against every feature. So the builder produces text and dependencies, and the
//! vertices come later — which is what these check, along with the part that is easy to get
//! wrong once the phases are separate: that the same tile, laid out twice, says the same thing.

use std::cell::RefCell;

use tessella_glyph::fonts::Fonts;
use tessella_layout::symbol_layout::{Anchoring, Placement};
use tessella_orchestrate::tile::{TileId, build_mvt_tile};
use tessella_source::mvt::Tile;
use tessella_storage::source::{FetchError, FileSource, Response};
use tessella_style::Style;

const STREETS: &[u8] = include_bytes!("../../../tests/mvt-fixtures/streets-10-163-395.mvt");
const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

/// An origin serving the vendored font, and counting what was asked of it.
struct Origin {
    asked: RefCell<Vec<String>>,
}

impl Origin {
    fn new() -> Self {
        Self {
            asked: RefCell::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

impl FileSource for Origin {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        self.asked.borrow_mut().push(url.to_string());
        // One font, one range. Anything else is a 404, which is a response and not an error.
        let body = if url.contains("TestFont") && url.contains("0-255") {
            GLYPHS.to_vec()
        } else {
            Vec::new()
        };
        Ok(Response {
            status: 200,
            body,
            ..Response::default()
        })
    }
}

// `FileSource` is `Send + Sync`; the `RefCell` here is single-threaded test bookkeeping.
unsafe impl Sync for Origin {}
unsafe impl Send for Origin {}

/// A store with this layout's glyphs fetched and packed.
fn fonts_for(layout: &tessella_layout::symbol_layout::SymbolLayout) -> (Fonts, Origin) {
    let origin = Origin::new();
    let mut fonts = Fonts::new("https://example.com/fonts/{fontstack}/{range}.pbf");
    fonts
        .fetch(&layout.dependencies(), &origin)
        .expect("the origin answers");
    (fonts, origin)
}

/// A style labelling the fixture's roads by their type, since the fixture's roads have no name.
fn road_style(placement: &str) -> Style {
    road_style_spaced(placement, 400.0)
}

fn road_style_spaced(placement: &str, spacing: f32) -> Style {
    serde_json::from_str(&format!(
        r#"{{"version": 8, "sources": {{"v": {{"type": "vector", "tiles": []}}}},
            "layers": [{{"id": "road-labels", "type": "symbol", "source": "v",
                         "source-layer": "road",
                         "layout": {{"text-field": "{{type}}", "text-font": ["TestFont"],
                                     "text-size": 14, "symbol-placement": "{placement}",
                                     "symbol-spacing": {spacing}}}}}]}}"#
    ))
    .expect("a style")
}

fn tile() -> Tile {
    Tile::decode(STREETS).expect("the fixture decodes")
}

const ID: TileId = TileId::new(10, 163, 395);

/// The builder resolves the text and stops there.
///
/// The assertion that says the phases are real: a bucket with labels in it and no vertices
/// anywhere, from a builder that was never given a font.
#[test]
fn the_builder_resolves_text_without_glyphs() {
    let buckets = build_mvt_tile(&road_style("point"), "v", ID, &tile()).expect("the tile builds");
    assert_eq!(buckets.len(), 1, "one symbol layer");

    let layout = buckets[0]
        .content
        .as_symbol()
        .expect("a symbol layer builds a symbol layout");
    assert!(!layout.is_empty(), "the fixture's roads carry a type");
    assert_eq!(layout.placement, Placement::Point);

    // Real values from the tile, not the template.
    assert!(
        layout
            .pending
            .iter()
            .all(|pending| !pending.text.contains('{')),
        "a token survived into a label"
    );
    assert!(
        layout
            .pending
            .iter()
            .any(|pending| pending.text == "primary"),
        "no road resolved to a type this fixture has"
    );

    // And the layer's `text-size` reached the options, rather than the default.
    assert!(
        (layout.symbol.size - 14.0).abs() < 0.01,
        "{:?}",
        layout.symbol
    );
}

/// The dependencies are what the glyph manager would fetch.
#[test]
fn the_layout_asks_for_the_glyphs_its_text_needs() {
    let buckets = build_mvt_tile(&road_style("point"), "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    let dependencies = layout.dependencies();
    assert_eq!(dependencies.len(), 1, "one font stack");
    let (stack, codepoints) = dependencies.iter().next().expect("one entry");
    assert_eq!(stack, &vec!["TestFont".to_string()]);

    // Every letter of every label, and nothing else.
    for pending in &layout.pending {
        for character in pending.text.chars() {
            assert!(
                codepoints.contains(&(character as u32)),
                "{character:?} of {:?} was not asked for",
                pending.text
            );
        }
    }
    assert!(
        codepoints.contains(&u32::from(b'p')),
        "the letters of 'primary' are missing"
    );
    assert!(
        !codepoints.contains(&u32::from(b'{')),
        "a template character was asked for"
    );
}

/// The second phase turns the same layout into vertices.
#[test]
fn the_glyphs_turn_the_layout_into_vertices() {
    let buckets = build_mvt_tile(&road_style("point"), "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    let (fonts, _) = fonts_for(layout);
    let (buffers, laid) = layout.lay_out(&fonts);

    assert!(!laid.is_empty(), "nothing was laid out");
    assert_eq!(buffers.vertices.len(), buffers.glyphs() * 4);
    assert_eq!(buffers.glyph_offsets.len(), buffers.glyphs());

    // Each label's range addresses its own vertices, tiling the buffer without gaps.
    let mut next = 0usize;
    for entry in &laid {
        assert_eq!(entry.vertices.start, next, "a gap or an overlap");
        next = entry.vertices.end;
    }
    assert_eq!(next, buffers.vertices.len());

    // A point-placed label sits still, so nothing walks along a line.
    assert!(
        buffers.glyph_offsets.iter().all(|offset| *offset == 0.0),
        "a point label recorded along-line distances"
    );
}

/// Laying the same layout out twice gives the same bytes.
///
/// The failure the split invites: phase two reading something phase one left behind, so the
/// second call differs from the first. A tile is laid out again whenever a font arrives late.
#[test]
fn laying_out_twice_gives_the_same_thing() {
    let buckets = build_mvt_tile(&road_style("point"), "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");
    let (fonts, _) = fonts_for(layout);

    let (first, first_laid) = layout.lay_out(&fonts);
    let (second, second_laid) = layout.lay_out(&fonts);
    assert_eq!(first, second, "the same layout produced different buffers");
    assert_eq!(first_laid, second_laid);
}

/// `symbol-placement` decides which builder runs, and the geometry it keeps.
#[test]
fn placement_decides_point_or_line() {
    let point = build_mvt_tile(&road_style("point"), "v", ID, &tile()).expect("builds");
    let line = build_mvt_tile(&road_style("line"), "v", ID, &tile()).expect("builds");

    let point = point[0].content.as_symbol().expect("a symbol layout");
    let line = line[0].content.as_symbol().expect("a symbol layout");
    assert_eq!(line.placement, Placement::Line);

    // A point-placed label keeps one anchor per ring; a line-placed one keeps the whole ring.
    assert!(
        point
            .pending
            .iter()
            .all(|pending| matches!(pending.anchoring, Anchoring::Point(_))),
        "a point layer kept a line"
    );
    assert!(
        line.pending
            .iter()
            .all(|pending| matches!(pending.anchoring, Anchoring::Line(_))),
        "a line layer kept a point"
    );

    // And `symbol-spacing` reached the line options.
    assert!((line.line.spacing - 400.0).abs() < 0.01, "{:?}", line.line);

    // A point-placed label is one per feature, always. A line-placed one is neither: a road too
    // short to hold its name gets none, and a long one gets several -- on this fixture at a
    // spacing of 400 the two effects together give 873 from 1773 roads, so a count alone says
    // nothing about whether repetition happens.
    let (fonts, _) = fonts_for(line);
    let (_, point_laid) = point.lay_out(&fonts);
    let (_, line_laid) = line.lay_out(&fonts);
    assert_eq!(point_laid.len(), point.pending.len());
    assert!(
        !line_laid.is_empty(),
        "no road was long enough for its name"
    );

    // Halving the spacing is what shows it: the same roads, twice as often along each.
    let closer =
        build_mvt_tile(&road_style_spaced("line", 100.0), "v", ID, &tile()).expect("builds");
    let closer = closer[0].content.as_symbol().expect("a symbol layout");
    let (_, closer_laid) = closer.lay_out(&fonts);
    assert!(
        closer_laid.len() > line_laid.len() * 2,
        "{} at spacing 400 and {} at 100",
        line_laid.len(),
        closer_laid.len()
    );

    // And a line-placed label records where along its road each glyph sits.
    let (buffers, _) = line.lay_out(&fonts);
    assert!(
        buffers.glyph_offsets.iter().any(|offset| *offset != 0.0),
        "a line label recorded no along-line distances"
    );
}

/// A layer reading a source-layer the tile does not have draws nothing, and is still a layer.
#[test]
fn a_missing_source_layer_is_empty_rather_than_absent() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v",
                        "source-layer": "nothing-here",
                        "layout": {"text-field": "{name}", "text-font": ["TestFont"]}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    assert_eq!(buckets.len(), 1, "the layer is in the style, so it is here");

    let layout = buckets[0].content.as_symbol().expect("a symbol layout");
    assert!(layout.is_empty());
    assert!(layout.dependencies().is_empty(), "nothing to fetch");
    assert_eq!(buckets[0].drawable_count(), 0, "and nothing to draw");
}

/// An unnamed feature produces no label rather than one reading its template.
#[test]
fn a_feature_without_the_property_produces_no_label() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"text-field": "{name}", "text-font": ["TestFont"]}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    // The fixture's roads carry `type` and not `name`, so every one resolves to nothing.
    assert!(
        layout.is_empty(),
        "{} roads were labelled with a name they do not have",
        layout.pending.len()
    );
}

/// A tile's whole symbol layer costs one request.
///
/// §5.1's store, asserted where it pays: the layer resolves 873 labels over 1773 roads and every
/// one of them is ASCII, so one range answers all of them. A store keyed per label — or per
/// tile — would ask once each, and the map would work while spending a round trip a label.
#[test]
fn a_tile_of_labels_costs_one_request() {
    let buckets = build_mvt_tile(&road_style("point"), "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");
    assert!(layout.pending.len() > 100, "too few labels to be a test");

    let (mut fonts, origin) = fonts_for(layout);
    assert_eq!(origin.asked().len(), 1, "{:?}", origin.asked());

    // And a second tile of the same style asks for nothing more.
    let again = build_mvt_tile(&road_style("line"), "v", ID, &tile()).expect("builds");
    let again = again[0].content.as_symbol().expect("a symbol layout");
    let fetched = fonts
        .fetch(&again.dependencies(), &origin)
        .expect("the origin answers");
    assert_eq!(fetched, 0, "the same letters were fetched again");
    assert_eq!(origin.asked().len(), 1);

    // The store answers for it without another byte off the network.
    let (buffers, laid) = again.lay_out(&fonts);
    assert!(!laid.is_empty());
    assert!(!buffers.is_empty());
}
