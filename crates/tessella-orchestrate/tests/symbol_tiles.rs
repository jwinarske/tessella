//! A symbol layer through the tile builder, in the two phases it actually has.
//!
//! Every other layer type turns features into vertices in one pass. A symbol layer cannot:
//! shaping needs glyph metrics, and the glyph URL is not known until `text-field` has been
//! evaluated against every feature. So the builder produces text and dependencies, and the
//! vertices come later — which is what these check, along with the part that is easy to get
//! wrong once the phases are separate: that the same tile, laid out twice, says the same thing.

use std::cell::RefCell;
use std::collections::BTreeSet;

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

/// Icon positions in an atlas, as the packer would produce them.
///
/// The rectangle is *padded*: a sixteen-pixel icon occupies eighteen, one pixel of which is the
/// border the quad samples. Building it here rather than parsing an index is the point — an
/// index gives sheet rectangles, and the sheet is not the texture.
fn positions(icons: &[(&str, bool)]) -> tessella_glyph::sprite::Positions {
    icons
        .iter()
        .enumerate()
        .map(|(index, (name, sdf))| {
            #[allow(clippy::cast_possible_truncation)]
            let position = tessella_glyph::sprite::IconPosition {
                padded_rect: tessella_glyph::atlas::Rect {
                    x: index as u32 * 20 + 1,
                    y: 1,
                    width: 18,
                    height: 18,
                },
                pixel_ratio: 1.0,
                sdf: *sdf,
                content: None,
                text_fit_width: None,
                text_fit_height: None,
            };
            ((*name).to_string(), position)
        })
        .collect()
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
    let (buffers, laid) = layout.lay_out(&fonts, None);

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

    let (first, first_laid) = layout.lay_out(&fonts, None);
    let (second, second_laid) = layout.lay_out(&fonts, None);
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
    let (_, point_laid) = point.lay_out(&fonts, None);
    let (_, line_laid) = line.lay_out(&fonts, None);
    assert_eq!(point_laid.len(), point.pending.len());
    assert!(
        !line_laid.is_empty(),
        "no road was long enough for its name"
    );

    // Halving the spacing is what shows it: the same roads, twice as often along each.
    let closer =
        build_mvt_tile(&road_style_spaced("line", 100.0), "v", ID, &tile()).expect("builds");
    let closer = closer[0].content.as_symbol().expect("a symbol layout");
    let (_, closer_laid) = closer.lay_out(&fonts, None);
    assert!(
        closer_laid.len() > line_laid.len() * 2,
        "{} at spacing 400 and {} at 100",
        line_laid.len(),
        closer_laid.len()
    );

    // And a line-placed label records where along its road each glyph sits.
    let (buffers, _) = line.lay_out(&fonts, None);
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
    let (buffers, laid) = again.lay_out(&fonts, None);
    assert!(!laid.is_empty());
    assert!(!buffers.is_empty());
}

/// A data-driven `text-size` gives each feature its own size.
///
/// The spec allows it and styles use it — a capital set larger than a town on the same layer.
/// Held per label rather than per layer because that is the granularity the spec gives it, and
/// because the vertex already carries a size per quad: what was missing was a size per *label*,
/// not anything about the encoding.
#[test]
fn text_size_can_vary_per_feature() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"text-field": "{type}", "text-font": ["TestFont"],
                                   "text-size": ["match", ["get", "type"],
                                                 "motorway", 24, "primary", 18, 10]}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    let size_of = |text: &str| {
        layout
            .pending
            .iter()
            .find(|pending| pending.text == text)
            .map(|pending| pending.symbol.size)
    };
    assert_eq!(size_of("motorway"), Some(24.0));
    assert_eq!(size_of("primary"), Some(18.0));
    assert_eq!(size_of("tertiary"), Some(10.0), "the match fallback");

    // And the sizes reach the vertices, packed the way the shader reads them.
    let (fonts, _) = fonts_for(layout);
    let (buffers, laid) = layout.lay_out(&fonts, None);
    let unpack = |vertex: &tessella_layout::symbol_bucket::SymbolVertex| {
        // The minimum size is the size times 128, shifted up one with `isSDF` in the low bit.
        f32::from(vertex.data[2] >> 1) / 128.0
    };

    let mut seen: BTreeSet<u32> = BTreeSet::new();
    for (entry, pending) in laid.iter().zip(&layout.pending) {
        let drawn = unpack(&buffers.vertices[entry.vertices.start]);
        assert!(
            (drawn - pending.symbol.size).abs() < 0.01,
            "{:?} was laid out at {drawn} and asked for {}",
            pending.text,
            pending.symbol.size
        );
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        seen.insert(drawn as u32);
    }
    assert!(
        seen.len() >= 3,
        "only {seen:?} distinct sizes, so the match collapsed"
    );
}

/// Labels stay in the order the layer offered them, whatever varies between them.
///
/// A layer's labels share one buffer, and per-frame state is written into the slice layout
/// recorded for each — so the buffer's order is part of the contract, and the golden pins it.
/// Laying out by *grouping* labels that share a size or a font would produce identical geometry
/// in a different order, which is byte-for-byte wrong against the oracle and looks like nothing
/// at all until a second size appears.
#[test]
fn a_varying_size_does_not_reorder_the_buffer() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"text-field": "{type}", "text-font": ["TestFont"],
                                   "text-size": ["match", ["get", "type"],
                                                 "motorway", 24, "primary", 18, 10]}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");
    let (fonts, _) = fonts_for(layout);
    let (buffers, laid) = layout.lay_out(&fonts, None);

    // The sizes are interleaved rather than sorted, which is what says nothing was gathered.
    let sizes: Vec<f32> = layout
        .pending
        .iter()
        .map(|pending| pending.symbol.size)
        .collect();
    assert!(
        sizes.windows(2).any(|pair| pair[1] < pair[0]),
        "every size is in ascending order, so the run was sorted"
    );

    // And the ranges still tile the buffer in order, without gaps.
    let mut next = 0usize;
    for entry in &laid {
        assert_eq!(entry.vertices.start, next, "a gap or an overlap");
        next = entry.vertices.end;
    }
    assert_eq!(next, buffers.vertices.len());
}

/// A symbol can be an icon with no label.
///
/// Most markers on a map are exactly that, and the builder dropped every one of them: it
/// resolved `text-field` first and returned early when a feature had no name, so a layer with
/// `icon-image` and no `text-field` produced nothing at all. A symbol needs one half or the
/// other, not the text half specifically.
#[test]
fn an_icon_without_a_label_is_still_a_symbol() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"icon-image": "{type}-marker"}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    assert!(!layout.is_empty(), "an icon-only layer produced no symbols");
    assert!(
        layout.pending.iter().all(|pending| pending.text.is_empty()),
        "a layer with no text-field produced text"
    );
    assert_eq!(
        layout.dependencies().len(),
        0,
        "an icon asked for glyph ranges"
    );

    let icons = layout.icons();
    assert!(icons.contains("primary-marker"), "{icons:?}");
    assert!(
        icons.iter().all(|name| name.ends_with("-marker")),
        "a token was left unresolved: {icons:?}"
    );
}

/// A feature with neither a name nor an icon produces nothing.
///
/// The other side of the same rule. Most features of a symbol source have neither, so this is
/// the common path rather than an edge: a layer that emitted a symbol per feature regardless
/// would put an empty collision box on every road in the tile.
#[test]
fn a_feature_with_neither_half_produces_nothing() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"text-field": "{name}", "icon-image": "{name}",
                                   "text-font": ["TestFont"]}}]}"#,
    )
    .expect("a style");

    // The fixture's roads carry `type` and not `name`, so both halves resolve to nothing.
    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");
    assert!(layout.is_empty(), "{} symbols", layout.pending.len());
    assert!(layout.icons().is_empty());
}

/// A token in an icon name resolves to *something* even when its property is absent.
///
/// `{name}-marker` on a feature with no name is the sprite `-marker`, not nothing — the token is
/// a `get` and an absent property is an empty string, so the surrounding literal survives. mbgl
/// does the same and then misses at lookup, which is why `icons()` is what the layer *asked for*
/// rather than what the sheet has.
///
/// Worth pinning because the obvious reading is the other one: that a token failing should void
/// the whole name. It does for `text-field`, where an empty label is nothing to draw — and the
/// two rules look the same until a style writes `{name}-marker` and gets an icon it did not mean
/// rather than no icon at all.
#[test]
fn a_token_that_resolves_to_nothing_still_leaves_its_literal() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"icon-image": "{name}-marker"}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");
    assert_eq!(
        layout.icons().into_iter().collect::<Vec<_>>(),
        vec!["-marker".to_string()],
        "the literal did not survive the empty token"
    );
}

/// A symbol with both halves carries both.
#[test]
fn a_symbol_can_have_both_halves() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"text-field": "{type}", "icon-image": "{type}-shield",
                                   "text-font": ["TestFont"]}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    let with_both = layout
        .pending
        .iter()
        .filter(|pending| !pending.text.is_empty() && pending.icon.is_some())
        .count();
    assert_eq!(with_both, layout.pending.len(), "a half went missing");

    // The text half still asks for its glyphs, and the icon half for its sprites.
    assert_eq!(layout.dependencies().len(), 1, "one font stack");
    assert!(layout.icons().contains("primary-shield"));

    // And the text still lays out, unaffected by the icon beside it.
    let (fonts, _) = fonts_for(layout);
    let (buffers, laid) = layout.lay_out(&fonts, None);
    assert!(!laid.is_empty());
    assert!(!buffers.is_empty());
}

/// A tile's icons lay out against the style's sprite index.
///
/// The icon half end to end: resolve `icon-image` per feature, ask the index for each name, and
/// emit a quad per icon that the sheet has. Nothing packs — unlike glyphs, the sprite sheet is
/// already an atlas and the index gives rectangles into it.
#[test]
fn icons_lay_out_against_the_sprite_index() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"icon-image": "{type}", "icon-size": 2}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    // An atlas holding two of the names the layer asked for, and not the third.
    let sprites = positions(&[("primary", false), ("motorway", false)]);

    let asked = layout.icons();
    assert!(asked.len() > 2, "{asked:?} is too few to prove a miss");

    let (fonts, _) = fonts_for(layout);
    let (_, instances) = layout.lay_out(&fonts, None);
    let (buffers, laid) = layout.lay_out_icons(&sprites, &instances);
    assert!(!laid.is_empty(), "nothing was laid out");
    assert_eq!(buffers.vertices.len(), laid.len() * 4, "one quad an icon");

    // Only the names the sheet has drew. A missing icon is skipped, not an error: a style with
    // one absent sprite still draws the rest.
    let drawn = laid.len();
    let present = layout
        .pending
        .iter()
        .filter(|pending| {
            pending
                .icon
                .as_deref()
                .is_some_and(|name| sprites.contains_key(name))
        })
        .count();
    assert_eq!(
        drawn, present,
        "a missing sprite was drawn or a present one was not"
    );
    assert!(drawn < layout.pending.len(), "nothing was skipped");

    // The ranges tile the buffer in order, the way the text ones do.
    let mut next = 0usize;
    for entry in &laid {
        assert_eq!(entry.vertices.start, next, "a gap or an overlap");
        next = entry.vertices.end;
    }
    assert_eq!(next, buffers.vertices.len());
}

/// `icon-size` is a multiplier, and `text-size` is a size.
///
/// The two look alike and default differently — one to 1 and the other to 16 — because
/// `icon-size` scales a sprite that is already the size its author drew it. Reading one as the
/// other draws every marker sixteen times too large, which is the kind of wrong that looks like
/// a broken sprite sheet.
#[test]
fn icon_size_is_a_multiplier_not_a_size() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"icon-image": "{type}"}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    assert!(
        layout
            .pending
            .iter()
            .all(|pending| (pending.icon_options.size - 1.0).abs() < f32::EPSILON),
        "icon-size defaulted to something other than one"
    );
    assert!(
        (layout.symbol.size - 16.0).abs() < f32::EPSILON,
        "text-size defaulted to something other than sixteen"
    );
}

/// An SDF sprite is marked as one in the vertex, and a plain image is not.
///
/// The sprite decides, not the layer. A shield drawn as a distance field is recolourable by
/// `icon-color`; a photographic icon is not, and putting a plain image through the SDF shader
/// draws its alpha as a coverage ramp.
#[test]
fn the_sprite_decides_whether_it_is_a_field() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"icon-image": "{type}"}}]}"#,
    )
    .expect("a style");
    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    let field = positions(&[("primary", true)]);
    let plain = positions(&[("primary", false)]);

    // The flag rides in the low bit of the packed minimum size.
    let is_sdf = |buffers: &tessella_layout::symbol_bucket::SymbolBuffers| {
        buffers.vertices[0].data[2] & 1 == 1
    };

    let (fonts, _) = fonts_for(layout);
    let (_, instances) = layout.lay_out(&fonts, None);
    let (as_field, _) = layout.lay_out_icons(&field, &instances);
    let (as_plain, _) = layout.lay_out_icons(&plain, &instances);
    assert!(is_sdf(&as_field), "an sdf sprite was drawn as an image");
    assert!(!is_sdf(&as_plain), "an image was drawn as a field");
}

/// A road's segments are joined before it is labelled.
///
/// The street fixture is 1773 road *features*, which is not 1773 roads: a tile cuts a street at
/// its edges and a source cuts it wherever an attribute changes, so one street arrives as a run
/// of stubs laid end to end. Labelling them separately puts a copy of the name on each — and
/// drops most of them, because a stub shorter than its own label cannot hold one.
///
/// The measurable consequence is that merging *raises* the number of labels placed while
/// *lowering* the number of features, which is the shape that says stubs became roads rather
/// than that geometry went missing.
#[test]
fn a_roads_segments_are_joined_before_it_is_labelled() {
    let style = road_style("line");
    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let merged = buckets[0].content.as_symbol().expect("a symbol layout");

    assert!(
        merged.pending.len() < 1773,
        "{} features survived a merge of 1773",
        merged.pending.len()
    );

    // And the surviving lines are longer than the stubs they came from: at least one is longer
    // than any single feature could be, which only a join produces.
    let longest = merged
        .pending
        .iter()
        .filter_map(|pending| match &pending.anchoring {
            Anchoring::Line(line) => Some(line.len()),
            Anchoring::Point(_) => None,
        })
        .max()
        .expect("some roads");
    assert!(longest > 2, "every road is still a two-point stub");

    // A second pass joins more, and that is not a bug in either implementation. The index holds
    // one entry per (text, endpoint), so where two roads of the same name start at the same
    // point only one of them is reachable — this tile has fifty such junctions. mbgl's index is
    // an `unordered_map` assigned into, which overwrites the same way. One greedy pass is what
    // mbgl does and what this does; running it to a fixed point would be a divergence, and a
    // silent one, since the extra joins look like better labelling.
    let mut again = merged.clone();
    again.merge_lines();
    assert!(
        again.pending.len() < merged.pending.len(),
        "a second pass changed nothing, so the junction case has gone away"
    );

    // Every merged line is still a line: no empties left behind, and no duplicated joints.
    for pending in &merged.pending {
        let Anchoring::Line(line) = &pending.anchoring else {
            continue;
        };
        assert!(line.len() >= 2, "an empty line survived the merge");
        for pair in line.windows(2) {
            assert_ne!(pair[0], pair[1], "a zero-length segment at a joint");
        }
    }
}

/// `icon-text-fit` stretches a shield around its number, end to end.
///
/// The whole chain in one: resolve both halves per feature, shape the text, fit the icon to it,
/// correct the aspect against the sprite's own limits, and hand placement a box that reserves
/// the drawn picture rather than the text inside it.
#[test]
fn a_shield_stretches_around_its_number() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"text-field": "{type}", "text-font": ["TestFont"],
                                   "icon-image": "shield", "icon-text-fit": "both",
                                   "icon-text-fit-padding": [2, 4, 2, 4]}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    // A shield with a content box: 40x20 drawn, text sits in 4,4..36,16.
    let sprites: tessella_glyph::sprite::Positions = [(
        "shield".to_string(),
        tessella_glyph::sprite::IconPosition {
            padded_rect: tessella_glyph::atlas::Rect {
                x: 1,
                y: 1,
                width: 42,
                height: 22,
            },
            pixel_ratio: 1.0,
            sdf: true,
            content: Some(tessella_glyph::sprite::Content {
                left: 4.0,
                top: 4.0,
                right: 36.0,
                bottom: 16.0,
            }),
            text_fit_width: Some(tessella_glyph::sprite::TextFit::StretchOnly),
            text_fit_height: Some(tessella_glyph::sprite::TextFit::StretchOnly),
        },
    )]
    .into_iter()
    .collect();

    let (fonts, _) = fonts_for(layout);
    let (_, text) = layout.lay_out(&fonts, None);
    assert_eq!(
        text.len(),
        layout.pending.len(),
        "the text list must be one-to-one for icons to find their labels"
    );

    let (icons, laid) = layout.lay_out_icons(&sprites, &text);
    assert!(!laid.is_empty(), "no shields were drawn");
    assert_eq!(icons.vertices.len(), laid.len() * 4);

    // Every shield is as wide as its own label plus the fit padding, rather than the sprite's
    // 40 — which is what "stretched around the text" means and what an unfitted icon would not
    // do. Widths differ between labels because the labels differ.
    let widths: BTreeSet<u32> = laid
        .iter()
        .map(|entry| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                (entry.extent.3 - entry.extent.2) as u32
            }
        })
        .collect();
    assert!(
        widths.len() > 1,
        "every shield came out the same width: {widths:?}"
    );

    // And each carries the margins its sprite's border needs, so collision reserves the picture.
    for entry in &laid {
        let margins = entry
            .content_margins
            .expect("a fitted shield with a content box carries margins");
        assert_eq!(
            margins,
            (4.0, 4.0, 4.0, 4.0),
            "the border is four all round"
        );
    }
}

/// An icon with no label is not stretched, and reserves no border twice.
///
/// `icon-text-fit` on a feature with no text has nothing to stretch to. The icon keeps its own
/// size, and — the part that is easy to miss — it must *not* carry content margins either: its
/// extent is already the whole picture, so adding them would reserve the border a second time.
#[test]
fn an_icon_with_no_label_is_not_stretched() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"icon-image": "shield", "icon-text-fit": "both"}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    let sprites: tessella_glyph::sprite::Positions = [(
        "shield".to_string(),
        tessella_glyph::sprite::IconPosition {
            padded_rect: tessella_glyph::atlas::Rect {
                x: 1,
                y: 1,
                width: 42,
                height: 22,
            },
            pixel_ratio: 1.0,
            sdf: true,
            content: Some(tessella_glyph::sprite::Content {
                left: 4.0,
                top: 4.0,
                right: 36.0,
                bottom: 16.0,
            }),
            text_fit_width: None,
            text_fit_height: None,
        },
    )]
    .into_iter()
    .collect();

    let (fonts, _) = fonts_for(layout);
    let (_, instances) = layout.lay_out(&fonts, None);
    let (_, laid) = layout.lay_out_icons(&sprites, &instances);
    assert!(!laid.is_empty());
    for entry in &laid {
        assert_eq!(
            (entry.extent.3 - entry.extent.2),
            40.0,
            "an icon with no label was stretched"
        );
        assert!(
            entry.content_margins.is_none(),
            "an unfitted icon reserved its border twice"
        );
    }
}

/// `auto` resolves in two steps, and the order is what makes a road name lie flat.
///
/// Rotation alignment goes first: `map` for a line-placed symbol, `viewport` for a point-placed
/// one — a road name follows its road, a town name stays upright. Pitch alignment then *inherits
/// what rotation became*. Resolving pitch first would give every line label a viewport pitch and
/// lay none of them flat on a tilted map, which is a plausible-looking map that is wrong.
#[test]
fn auto_alignment_resolves_from_placement_then_inherits() {
    use tessella_layout::symbol_layout::Alignment;

    let point = build_mvt_tile(&road_style("point"), "v", ID, &tile()).expect("builds");
    let point = point[0].content.as_symbol().expect("a symbol layout");
    assert_eq!(point.text_alignments.rotation, Alignment::Viewport);
    assert_eq!(
        point.text_alignments.pitch,
        Alignment::Viewport,
        "pitch did not inherit rotation"
    );

    let line = build_mvt_tile(&road_style("line"), "v", ID, &tile()).expect("builds");
    let line = line[0].content.as_symbol().expect("a symbol layout");
    assert_eq!(line.text_alignments.rotation, Alignment::Map);
    assert_eq!(
        line.text_alignments.pitch,
        Alignment::Map,
        "a road name would stand up on a tilted map"
    );
}

/// A style may state either alignment, and the other still inherits.
#[test]
fn a_stated_alignment_overrides_the_default() {
    use tessella_layout::symbol_layout::Alignment;

    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "l", "type": "symbol", "source": "v", "source-layer": "road",
                        "layout": {"text-field": "{type}", "text-font": ["TestFont"],
                                   "symbol-placement": "line",
                                   "text-pitch-alignment": "viewport"}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");

    // A road name that follows the road but stays upright when the map tilts: the case the two
    // properties are separate for.
    assert_eq!(layout.text_alignments.rotation, Alignment::Map);
    assert_eq!(layout.text_alignments.pitch, Alignment::Viewport);

    // The icons' pair is its own, and untouched by the text's.
    assert_eq!(layout.icon_alignments.pitch, Alignment::Map);
}

/// The alignments decide which matrices a drawable carries and who turns the symbol.
#[test]
fn the_alignments_decide_the_drawables_matrices() {
    use tessella_layout::symbol_layout::{Alignment, Alignments, Placement};
    use tessella_orchestrate::ubo::SymbolDrawableEntry;
    use tessella_tile::cover::ViewTransform;

    let view = ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: core::f64::consts::FRAC_PI_4,
        pitch: core::f64::consts::FRAC_PI_6,
    };

    let entry = |alignments, placement| {
        SymbolDrawableEntry::for_tile(
            &view,
            13,
            4093,
            2723,
            0,
            1,
            0,
            [512.0, 512.0],
            [0.0, 0.0],
            16.0,
            alignments,
            placement,
        )
        .expect("a viewport")
    };

    let upright = Alignments {
        rotation: Alignment::Viewport,
        pitch: Alignment::Viewport,
    };
    let flat = Alignments {
        rotation: Alignment::Map,
        pitch: Alignment::Map,
    };
    let turning_but_standing = Alignments {
        rotation: Alignment::Map,
        pitch: Alignment::Viewport,
    };

    // A label walked along a line gets the *identity* plane: the projection does the walk along
    // the projected road, and a plane here would bend the label before the walk bent it again.
    let walked = entry(flat, Placement::Line);
    assert_eq!(
        walked.label_plane_matrix,
        [
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0,
        ],
        "a walked label got a plane to bend it twice"
    );

    // A flat point label gets the map-aligned plane, which is not the viewport one.
    let lying = entry(flat, Placement::Point);
    let standing = entry(upright, Placement::Point);
    assert_ne!(lying.label_plane_matrix, standing.label_plane_matrix);
    assert!(lying.pitch_with_map);
    assert!(!standing.pitch_with_map);

    // Only the symbol that turns with the map while standing up is turned by the shader. The
    // other two are turned by the projection or by the walk, and turning them again would
    // double the rotation.
    assert!(entry(turning_but_standing, Placement::Point).rotate_symbol);
    assert!(!lying.rotate_symbol, "a flat label was turned twice");
    assert!(!walked.rotate_symbol, "a walked label was turned twice");
    assert!(!standing.rotate_symbol);
}

/// The gamma scale corrects a flat label's distance field and leaves an upright one alone.
///
/// A label lying flat and pitched away covers fewer screen pixels than it was laid out for, so a
/// fixed ramp is sampled across too few of them and the text thins to nothing at the horizon.
/// One for a label standing up: its glyphs are the size they were laid out at.
#[test]
fn the_gamma_scale_corrects_only_a_flat_label() {
    use tessella_layout::symbol_layout::Alignment;
    use tessella_orchestrate::ubo::symbol_gamma_scale;
    use tessella_tile::cover::ViewTransform;

    let view = ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: core::f64::consts::FRAC_PI_6,
    };

    assert_eq!(symbol_gamma_scale(&view, Alignment::Viewport), 1.0);

    let flat = symbol_gamma_scale(&view, Alignment::Map);
    assert!(
        flat > 1.0,
        "{flat} would thin the text rather than widen it"
    );

    // Steeper pitch tips the glyphs further away, so the correction *falls* with the cosine.
    let steeper = ViewTransform {
        pitch: core::f64::consts::FRAC_PI_3,
        ..view
    };
    assert!(
        symbol_gamma_scale(&steeper, Alignment::Map) < flat,
        "the correction did not follow the pitch"
    );
}

/// A line-placed icon repeats along the line, as its label does.
///
/// # What this was
///
/// `lay_out_icons` returned `None` for a line-anchored symbol, under a comment saying that
/// placing it at the feature's first vertex "draws and is wrong". It was right about that and
/// the alternative it chose was to draw nothing, so a `symbol-placement: line` layer with an
/// `icon-image` — a road shield, a oneway arrow — drew its labels and none of its icons.
///
/// # Why it needed the instance list rather than the pending list
///
/// Because a line-placed symbol is one *pending* and one instance per anchor. Icons were built
/// from the pending list, which has one entry per feature, so there was nowhere for the second
/// repetition of a shield to come from. They are built from the instances now, and each
/// instance already knows the anchor `get_anchors` gave it.
#[test]
fn a_line_placed_icon_repeats_with_its_label() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "shields", "type": "symbol", "source": "v",
                        "source-layer": "road",
                        "layout": {"text-field": "{type}", "text-font": ["TestFont"],
                                   "text-size": 14, "symbol-placement": "line",
                                   "symbol-spacing": 250,
                                   "icon-image": "primary"}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");
    let (fonts, _) = fonts_for(layout);
    let (_, instances) = layout.lay_out(&fonts, None);
    let sprites = positions(&[("primary", false)]);

    assert!(
        instances.len() > layout.pending.len(),
        "the point of the test is a feature that repeats: {} instances of {} features",
        instances.len(),
        layout.pending.len()
    );

    let (buffers, laid) = layout.lay_out_icons(&sprites, &instances);
    assert!(!laid.is_empty(), "a line-placed icon drew nothing");
    assert_eq!(buffers.vertices.len(), laid.len() * 4, "one quad an icon");

    // One icon per instance whose feature asked for one, at that instance's own anchor — not
    // one per feature, and not all at the same place.
    let wanted = instances
        .iter()
        .filter(|instance| {
            layout
                .pending
                .get(instance.pending)
                .is_some_and(|pending| pending.icon.is_some())
        })
        .count();
    assert_eq!(laid.len(), wanted, "one icon per instance that asked for one");

    let anchors: std::collections::BTreeSet<(u32, u32)> = laid
        .iter()
        .map(|icon| (icon.anchor.0.to_bits(), icon.anchor.1.to_bits()))
        .collect();
    assert!(
        anchors.len() > 1,
        "every icon landed on the same point, which is the bug the stub was avoiding"
    );

    // And each sits on its label's anchor rather than near it.
    let label_anchors: std::collections::BTreeSet<(u32, u32)> = instances
        .iter()
        .map(|instance| (instance.anchor.0.to_bits(), instance.anchor.1.to_bits()))
        .collect();
    assert!(
        anchors.is_subset(&label_anchors),
        "an icon is drawn at the instance it belongs to"
    );
}

/// A line-placed symbol with an icon and no text draws its icons.
///
/// # The case
///
/// Oneway arrows, lane markings, any `symbol-placement: line` layer whose features carry an
/// `icon-image` and no `text-field`. It drew nothing, and the reason was an ordering: anchors
/// were computed from the shaped text alone, so a feature with no text had no extent, produced
/// no instances, and had nowhere to hang an icon.
///
/// mbgl computes a feature's anchors once from *both* extents — `getAnchors(…, shapedText.left,
/// shapedText.right, shapedIcon.left, shapedIcon.right, …)` — which means it shapes the icon
/// first. Laying out takes the sprite index now for exactly that, so the icon's own width is
/// what decides where its repetitions go.
#[test]
fn an_icon_only_line_layer_draws() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "arrows", "type": "symbol", "source": "v",
                        "source-layer": "road",
                        "layout": {"symbol-placement": "line", "symbol-spacing": 250,
                                   "icon-image": "primary"}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");
    let (fonts, _) = fonts_for(layout);
    let sprites = positions(&[("primary", false)]);

    let (_, instances) = layout.lay_out(&fonts, Some(&sprites));
    assert!(
        !instances.is_empty(),
        "a feature with an icon and no text still has anchors, from the icon"
    );
    assert!(
        instances.iter().all(|instance| instance.glyphs == 0),
        "and shapes no glyphs, having no text"
    );

    let (buffers, laid) = layout.lay_out_icons(&sprites, &instances);
    assert!(!laid.is_empty(), "an icon-only line layer drew nothing");
    assert_eq!(buffers.vertices.len(), laid.len() * 4, "one quad an icon");

    let anchors: std::collections::BTreeSet<(u32, u32)> = laid
        .iter()
        .map(|icon| (icon.anchor.0.to_bits(), icon.anchor.1.to_bits()))
        .collect();
    assert!(
        anchors.len() > 1,
        "the arrows repeat along the road rather than sharing a point"
    );
}

/// The icon's own width is what decides where its repetitions go.
///
/// Not decoration: `get_anchors` measures whether a label fits between two bends, and a wide
/// shield is rejected where a narrow one is accepted. Passing zero for the icon — which is what
/// this did before the sprite index reached the first pass — makes every icon look like a point
/// and places them where a point would go.
#[test]
fn a_wider_icon_places_differently() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"v": {"type": "vector", "tiles": []}},
            "layers": [{"id": "arrows", "type": "symbol", "source": "v",
                        "source-layer": "road",
                        "layout": {"symbol-placement": "line", "symbol-spacing": 250,
                                   "icon-image": "primary"}}]}"#,
    )
    .expect("a style");

    let buckets = build_mvt_tile(&style, "v", ID, &tile()).expect("the tile builds");
    let layout = buckets[0].content.as_symbol().expect("a symbol layout");
    let (fonts, _) = fonts_for(layout);

    let narrow = positions(&[("primary", false)]);
    let mut wide = narrow.clone();
    let entry = wide.get_mut("primary").expect("the sprite");
    entry.padded_rect.width = 400;

    let (_, with_narrow) = layout.lay_out(&fonts, Some(&narrow));
    let (_, with_wide) = layout.lay_out(&fonts, Some(&wide));

    assert_ne!(
        with_narrow.len(),
        with_wide.len(),
        "a shield four hundred pixels wide does not fit where an eighteen-pixel one does, and \
         passing zero for the icon's extent would place both the same"
    );
}
