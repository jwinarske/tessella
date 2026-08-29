//! Painter order, checked against the golden dump's order section (§6.3, §9.1).
//!
//! # Two sections, only one of which is a draw order
//!
//! The dump has a `drawable` section and an `order` section, and they are not the same list. The
//! drawable section is a canonical listing: the probe groups by structural key and sorts, so the
//! golden file is stable across runs. Sorting it happens to produce layer-then-sublayer-then-tile,
//! which is close enough to painter order to be mistaken for it.
//!
//! The `order` section is the draw order. It differs from the listing in three ways that matter,
//! and every one of them is invisible if the listing is compared instead:
//!
//! - Pass groups before layer. Opaque entries all precede translucent ones.
//! - Within a pass, the topmost layer comes first, because mbgl orders by a depth slot that runs
//!   opposite the style index.
//! - A drawable whose pass is a mask appears once per pass. The oracle's order has forty-three
//!   entries for thirty-seven drawables; the six extra are the background, which is
//!   `Opaque | Translucent` and is genuinely drawn twice.
//!
//! So this compares against `order`, and a separate test pins the listing as the listing.

use std::collections::BTreeMap;

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::Ring;
use tessella_orchestrate::order::{self, DrawOrder};
use tessella_orchestrate::tile::{TileId as BuildTile, build_sourceless, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};
use tessella_tile::cover::{self, ViewTransform};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");
const DUMP: &str = include_str!("../../../tests/golden/hermetic_style.dump");

/// The oracle's probe: the transform the golden dump was captured at.
fn probe() -> ViewTransform {
    ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

/// `(pass, layer, sublayer, x, y)` for one entry of the draw order.
type Slot = (u8, u32, i32, u32, u32);

/// Pulls `Lnnnnn.Snnnnn.tZZ_XXXXXXXX_YYYYYYYY` out of a drawable key.
fn parse_key(key: &str) -> (u32, i32, u32, u32) {
    let mut parts = key.strip_prefix('L').expect("a layer prefix").split('.');
    let layer: u32 = parts.next().expect("layer").parse().expect("layer number");
    let sub: i32 = parts
        .next()
        .and_then(|s| s.strip_prefix('S'))
        .expect("sublayer")
        .parse()
        .expect("sublayer number");
    let mut fields = parts
        .next()
        .and_then(|t| t.strip_prefix('t'))
        .expect("a tile field")
        .split('_');
    let _z = fields.next();
    let x: u32 = fields.next().expect("x").parse().expect("x number");
    let y: u32 = fields.next().expect("y").parse().expect("y number");
    (layer, sub, x, y)
}

/// The oracle's draw order, from the `order` section.
/// Every bucket a tile produces: the source's layers and the source-less ones, in style order.
///
/// Spelled out rather than hidden behind a library helper, because the merge is only this
/// simple for a *single*-source style. Two sources build separately, and a background belongs
/// to neither — it must be taken once, not once per source.
fn build_all(
    style: &Style,
    source: &str,
    tile: BuildTile,
    features: &[tessella_source::GeoJsonFeature],
) -> Vec<tessella_orchestrate::LayerBucket> {
    let mut buckets =
        build_tile(style, source, tile, features, TilingOptions::default()).expect("tile builds");
    buckets.extend(build_sourceless(style, tile).expect("background builds"));
    buckets.sort_by_key(|bucket| bucket.layer_index);
    buckets
}

fn oracle_order() -> Vec<Slot> {
    DUMP.lines()
        .filter_map(|line| line.strip_prefix("draw "))
        .map(|rest| {
            let mut fields = rest.split(' ');
            let _index = fields.next();
            let key = fields.next().expect("a drawable key");
            let pass: u8 = fields
                .next()
                .and_then(|f| f.strip_prefix("pass="))
                .expect("a pass")
                .parse()
                .expect("pass number");
            let (layer, sub, x, y) = parse_key(key);
            (pass, layer, sub, x, y)
        })
        .collect()
}

/// The order this crate resolves for the same view.
///
/// `OrderEntry` carries no tile — the tile rides on `ViewUse`, because geometry is shared across
/// views while a use is per view — so the tile is recovered through the geometry id the binding
/// was given. That indirection is the ABI's, not the test's.
fn resolved_order() -> Vec<Slot> {
    let style = Style::parse(HERMETIC).expect("style parses");
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    let features = geojson::read(&source.data).expect("features");

    let view = ViewId(0);
    #[allow(clippy::cast_possible_truncation)]
    let mut order = DrawOrder::new(style.layers.len() as u32);
    let mut next_id = 0;
    let mut tile_of_geometry = BTreeMap::new();

    // Bound in cover order, which is not draw order. Sorting is the module's job, and feeding it
    // pre-sorted input would test nothing.
    for tile in cover::cover(&probe()).expect("covers") {
        let buckets = build_all(
            &style,
            "probe",
            BuildTile::new(tile.z, tile.x, tile.y),
            &features,
        );
        for binding in order::bindings_for(
            view,
            order::tile_of(tile.z, tile.x, tile.y),
            &buckets,
            &mut next_id,
            true,
        ) {
            tile_of_geometry.insert(binding.geometry.0, (tile.x, tile.y));
            order.bind(binding);
        }
    }

    order
        .resolve()
        .iter()
        .map(|entry| {
            let (x, y) = tile_of_geometry[&entry.geometry.0];
            (
                entry.pass.bits(),
                entry.layer_index,
                entry.sub_layer_index,
                x,
                y,
            )
        })
        .collect()
}

/// The resolved order is the oracle's draw order, now that every layer of the style is drawn.
///
/// It was a subsequence while the circle layer was unimplemented — the layers left out were
/// interleaved among the ones drawn, the circle being topmost and so drawn before every fill —
/// and the filter is kept rather than dropped because it is what states which layers are
/// claimed. With all five listed it selects everything, and the comparison is now the whole
/// draw order of the whole style.
#[test]
fn painter_order_matches_the_oracle() {
    let implemented = [0u32, 1, 2, 3, 4];
    let oracle: Vec<Slot> = oracle_order()
        .into_iter()
        .filter(|(_, layer, ..)| implemented.contains(layer))
        .collect();
    let mine = resolved_order();

    // Asserted before the comparison, because an empty `mine` compares equal to an empty filter.
    assert_eq!(
        mine.len(),
        43,
        "six background twice for its two passes, two fills at two drawables per tile, \
         one line per tile, and a single circle"
    );
    assert_eq!(oracle.len(), mine.len(), "same drawables, same count");
    assert_eq!(oracle, mine, "draw order diverges from the oracle");
}

/// The background is drawn in both passes, which is why the order is longer than the drawables.
#[test]
fn a_multi_pass_drawable_appears_once_per_pass() {
    let mine = resolved_order();
    let background: Vec<&Slot> = mine.iter().filter(|(_, layer, ..)| *layer == 0).collect();
    assert_eq!(background.len(), 12, "six tiles in two passes");
    assert_eq!(background.iter().filter(|(pass, ..)| *pass == 1).count(), 6);
    assert_eq!(background.iter().filter(|(pass, ..)| *pass == 2).count(), 6);

    // And the whole oracle order is longer than its drawable list by exactly that much.
    let drawables = DUMP.lines().filter(|l| l.starts_with("drawable ")).count();
    assert_eq!(oracle_order().len(), drawables + 6);
}

/// Pass groups before layer: every opaque entry precedes every translucent one.
#[test]
fn the_opaque_pass_precedes_the_translucent_one() {
    let mine = resolved_order();
    let last_opaque = mine.iter().rposition(|(pass, ..)| *pass == 1);
    let first_translucent = mine.iter().position(|(pass, ..)| *pass == 2);
    assert!(matches!(
        (last_opaque, first_translucent),
        (Some(a), Some(b)) if a < b
    ));
}

/// Within a pass, the topmost layer is drawn first — the depth slot runs opposite the style
/// index. Ordering by the style index directly would draw the background over the fills.
#[test]
fn within_a_pass_the_topmost_layer_comes_first() {
    let mine = resolved_order();
    let translucent: Vec<u32> = mine
        .iter()
        .filter(|(pass, ..)| *pass == 2)
        .map(|(_, layer, ..)| *layer)
        .collect();
    let first = translucent.first().copied().expect("a translucent entry");
    let last = translucent.last().copied().expect("a translucent entry");
    assert!(
        first > last,
        "layer {first} before layer {last}: the order runs top to bottom"
    );
    assert_eq!(last, 0, "and the background is drawn last");
}

/// The drawable section is a canonical listing, not a draw order. Pinned so that a future reader
/// comparing against it knows what it is.
#[test]
fn the_drawable_section_is_sorted_not_ordered() {
    let listing: Vec<(u32, i32, u32, u32)> = DUMP
        .lines()
        .filter_map(|line| line.strip_prefix("drawable "))
        .map(|rest| parse_key(rest.split(' ').next().expect("a key")))
        .collect();
    let mut sorted = listing.clone();
    sorted.sort_unstable();
    assert_eq!(listing, sorted, "the listing is in sorted order");

    let order: Vec<(u32, i32, u32, u32)> = oracle_order()
        .into_iter()
        .map(|(_, layer, sub, x, y)| (layer, sub, x, y))
        .collect();
    let mut order_sorted = order.clone();
    order_sorted.sort_unstable();
    assert_ne!(order, order_sorted, "the draw order is not");
}

/// The permutation key groups the hermetic style's layers exactly as the oracle's `pk` does.
///
/// The oracle's raw key is a hash of the uniform-property set together with the engine's
/// compiled-in defines, so its value is a build artifact and the dump renumbers it. What
/// survives renumbering is the *grouping* — which drawables want the same shader variant — and
/// that is the whole of what a consumer needs. So the comparison is between two partitions,
/// not two numbers.
///
/// The oracle groups by `(shader family, permutation)`; a bare permutation would compare the
/// wrong thing, because keys are only meaningful within a family — mbgl's hash does not include
/// the shader, and neither does this one.
#[test]
fn the_permutation_grouping_matches_the_oracle() {
    use std::collections::{BTreeMap, BTreeSet};
    use tessella_orchestrate::binder::{FILL_FAMILY, LINE_FAMILY, attribute_ids, permutation_key};

    // The oracle's own partition: layer index -> the (shader, pk) pairs it drew under.
    let mut oracle: BTreeMap<u32, BTreeSet<(u32, u32)>> = BTreeMap::new();
    for line in DUMP.lines().filter(|l| l.starts_with("drawable L")) {
        let key = line.split_whitespace().nth(1).expect("a key");
        let layer: u32 = key[1..6].parse().expect("a layer index");
        let shader: u32 = key[key.find(".sh").expect("sh") + 3..][..4]
            .parse()
            .expect("a shader");
        let permutation: u32 = key[key.find(".pk").expect("pk") + 3..][..4]
            .parse()
            .expect("a permutation");
        oracle
            .entry(layer)
            .or_default()
            .insert((shader, permutation));
    }

    // Two layers of the same family sharing a permutation there must share one here, and two
    // that differ must differ. The style's two fill layers are the pair that matters: same
    // family, same shaders, different paint.
    let style = Style::parse(HERMETIC).expect("style parses");
    let key_for = |id: &str, family: &[tessella_capture_abi::BuiltIn]| {
        let layer = style.layer(id).expect("the layer");
        let paint = tessella_style::property::resolve_paint(layer).expect("resolves");
        permutation_key(&paint, &attribute_ids(family))
    };

    let constant = key_for("fill-constant", FILL_FAMILY);
    let driven = key_for("fill-datadriven", FILL_FAMILY);

    // The oracle drew them under different permutations, and under the same two shaders.
    let oracle_constant: BTreeSet<u32> = oracle[&1].iter().map(|(_, p)| *p).collect();
    let oracle_driven: BTreeSet<u32> = oracle[&2].iter().map(|(_, p)| *p).collect();
    assert_eq!(
        oracle_constant.len(),
        1,
        "one permutation across both shaders"
    );
    assert_eq!(oracle_driven.len(), 1);
    assert_ne!(oracle_constant, oracle_driven, "the oracle separates them");
    assert_ne!(constant, driven, "and so does this build");

    // Both of the fill layer's shaders take that one key, which is the property that says the
    // key belongs to the layer's paint rather than to a shader.
    let shaders: BTreeSet<u32> = oracle[&2].iter().map(|(s, _)| *s).collect();
    assert_eq!(
        shaders.len(),
        2,
        "triangles and outline are different shaders"
    );

    // The line layer is its own family, so its key is not comparable with the fills' — but it
    // must still separate a driven line from a constant one.
    let line_driven = key_for("line-datadriven", LINE_FAMILY);
    let plain = Style::parse(
        r#"{"version": 8, "sources": {}, "layers": [
             {"id": "l", "type": "line", "source": "s", "paint": {"line-width": 3.0}}]}"#,
    )
    .expect("style parses");
    let line_constant = permutation_key(
        &tessella_style::property::resolve_paint(plain.layer("l").expect("l")).expect("resolves"),
        &attribute_ids(LINE_FAMILY),
    );
    assert_ne!(line_driven, line_constant);
}

// --- §9.3: OrderUpdate count == order-change count ---

/// Bindings for the probe's cover, so an order can be built and rebuilt.
fn probe_bindings() -> Vec<tessella_orchestrate::view::GeometryBinding> {
    let style = Style::parse(HERMETIC).expect("style parses");
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    let features = geojson::read(&source.data).expect("features");

    let mut next_id = 0u64;
    let mut out = Vec::new();
    for tile in cover::cover(&probe()).expect("covers") {
        let build = BuildTile::new(tile.z, tile.x, tile.y);
        let buckets = build_all(&style, "probe", build, &features);
        out.extend(order::bindings_for(
            ViewId(0),
            order::tile_of(tile.z, tile.x, tile.y),
            &buckets,
            &mut next_id,
            true,
        ));
    }
    out
}

/// Re-emitting an unchanged order writes nothing, and says so.
///
/// # Why this is the point of §6.3
///
/// Rev 1 sent the camera and the painter order in one record, so a view that merely moved
/// re-sent the whole order — every drawable, every frame, for a camera that had changed and an
/// order that had not. Splitting them (§6.3) is only worth anything if the order half then
/// stays quiet, and "stays quiet" is a property of this suppression rather than of the split.
///
/// The order is resolved and compared against what was last emitted, so the check is against the
/// bytes a consumer would receive rather than against a dirty flag somebody has to remember to
/// clear. A flag is what makes this kind of suppression wrong later: it is set correctly for a
/// year and then not, by a change nowhere near it.
#[test]
fn an_unchanged_order_emits_nothing() {
    let mut ring = Ring::new(1 << 20);
    let producer = ring.producer();

    let mut order = DrawOrder::new(8);
    for binding in probe_bindings() {
        order.bind(binding);
    }

    let first = order.emit(producer, ViewId(0)).expect("emits");
    assert!(first.changed, "the first order is new");
    assert!(first.entries > 0);
    let after_first = producer.head();
    let epoch = first.epoch;

    // Fifty frames in which nothing about the order changes.
    for frame in 0..50 {
        let again = order.emit(producer, ViewId(0)).expect("emits");
        assert!(!again.changed, "frame {frame} re-sent an unchanged order");
        assert_eq!(again.epoch, epoch, "frame {frame} bumped the epoch");
    }
    assert_eq!(
        producer.head(),
        after_first,
        "fifty frames of an unchanged order moved the ring"
    );
}

/// An order that does change emits once, and once per change.
///
/// The companion: suppression that never lifted would pass the test above and draw the first
/// frame forever.
#[test]
fn a_changed_order_emits_once_per_change() {
    let mut ring = Ring::new(1 << 20);
    let producer = ring.producer();
    let bindings = probe_bindings();
    assert!(bindings.len() > 2, "enough to drop one and notice");

    let mut order = DrawOrder::new(8);
    for binding in &bindings {
        order.bind(*binding);
    }
    let first = order.emit(producer, ViewId(0)).expect("emits");
    assert!(first.changed);
    let mut head = producer.head();
    let mut epoch = first.epoch;

    // Rebuild with one fewer drawable — a tile leaving the cover, in miniature.
    for dropped in 1..=3 {
        order.clear();
        for binding in &bindings[dropped..] {
            order.bind(*binding);
        }

        let changed = order.emit(producer, ViewId(0)).expect("emits");
        assert!(changed.changed, "dropping {dropped} changed nothing");
        assert!(changed.epoch > epoch, "the epoch did not advance");
        assert!(producer.head() > head, "nothing reached the ring");
        epoch = changed.epoch;
        head = producer.head();

        // And it settles again immediately.
        let settled = order.emit(producer, ViewId(0)).expect("emits");
        assert!(!settled.changed, "it did not settle after the change");
        assert_eq!(producer.head(), head);
    }
}

/// Rebuilding the same order from scratch is not a change.
///
/// A view recomputes its order every frame; that the *inputs* were rebuilt says nothing about
/// whether the result differs. Suppression keyed on "did anyone call `bind` again" rather than
/// on the resolved bytes would emit every frame while looking correct.
#[test]
fn rebuilding_an_identical_order_is_not_a_change() {
    let mut ring = Ring::new(1 << 20);
    let producer = ring.producer();
    let bindings = probe_bindings();

    let mut order = DrawOrder::new(8);
    for binding in &bindings {
        order.bind(*binding);
    }
    order.emit(producer, ViewId(0)).expect("emits");
    let head = producer.head();

    for frame in 0..10 {
        order.clear();
        for binding in &bindings {
            order.bind(*binding);
        }
        let again = order.emit(producer, ViewId(0)).expect("emits");
        assert!(!again.changed, "frame {frame} called a rebuild a change");
    }
    assert_eq!(producer.head(), head);
}
