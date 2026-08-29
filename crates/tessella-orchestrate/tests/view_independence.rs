//! §9.2's first multi-view invariant: a view's stream is a function of its own camera.
//!
//! > Per-view stream ≡ a single-view run at the same camera, modulo the geometry namespace.
//!
//! # What it protects
//!
//! §5 exists to escape mbgl's arrangement, where N maps over one style meant N copies of
//! everything. The replacement shares geometry process-wide and binds it into each view's order
//! with a `ViewUse` (§5.3, DR-18). Sharing is where correctness gets subtle: a view's draw order
//! now depends on a structure other views are also writing to, and the failure it invites is a
//! view whose output changes because of what its *neighbours* asked for.
//!
//! That failure is quiet. Four views over overlapping covers would still each draw something
//! plausible; the primary display would just be drawing a cluster inset's idea of the order, or
//! missing a layer another view had already bound. So the invariant is not "the views agree" —
//! it is that each view's order is byte-identical to what it would have been alone.
//!
//! # Modulo the geometry namespace
//!
//! Geometry ids are handed out process-wide in the order tiles are built, so a view running
//! second sees different numbers for the same tiles. Comparing them directly would assert the
//! allocation order rather than the invariant. Each stream's ids are renumbered by first
//! appearance before comparison, which leaves every other field exact — the layer, the sublayer,
//! the tile, the pass, the flags, and crucially the *order*.

use std::collections::BTreeMap;

use tessella_capture_abi::envelope::ViewId;
use tessella_orchestrate::order;
use tessella_orchestrate::tile::{TileId as BuildTile, build_sourceless, build_tile};
use tessella_orchestrate::view::GeometryBinding;
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};
use tessella_tile::cover::{self, ViewTransform};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");

fn style() -> Style {
    Style::parse(HERMETIC).expect("style parses")
}

fn features(style: &Style) -> Vec<tessella_source::GeoJsonFeature> {
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    geojson::read(&source.data).expect("features")
}

fn at(zoom: f64, longitude: f64) -> ViewTransform {
    ViewTransform {
        longitude,
        latitude: 51.505,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

/// The bindings each view produces, with the geometry counter shared across all of them.
///
/// Shared on purpose: that is what makes the ids differ between a solo run and a group one, and
/// so what the renumbering below has to absorb.
fn bindings(views: &[(ViewId, ViewTransform)]) -> BTreeMap<ViewId, Vec<GeometryBinding>> {
    let style = style();
    let features = features(&style);
    let mut next_id = 0u64;
    let mut out: BTreeMap<ViewId, Vec<GeometryBinding>> = BTreeMap::new();

    for (id, view) in views {
        for tile in cover::cover(view).expect("covers") {
            let build = BuildTile::new(tile.z, tile.x, tile.y);
            let mut buckets =
                build_tile(&style, "probe", build, &features, TilingOptions::default())
                    .expect("tile builds");
            buckets.extend(build_sourceless(&style, build).expect("background builds"));
            buckets.sort_by_key(|bucket| bucket.layer_index);
            out.entry(*id).or_default().extend(order::bindings_for(
                *id,
                order::tile_of(tile.z, tile.x, tile.y),
                &buckets,
                &mut next_id,
                true,
            ));
        }
    }
    out
}

/// A binding with its geometry id replaced by when that id was first seen in this stream.
type Normalised = (
    u64,
    i32,
    i32,
    Option<tessella_capture_abi::envelope::TileId>,
    tessella_capture_abi::RenderPass,
    u8,
);

fn normalise(bindings: &[GeometryBinding]) -> Vec<Normalised> {
    let mut seen: BTreeMap<u64, u64> = BTreeMap::new();
    bindings
        .iter()
        .map(|binding| {
            let next = seen.len() as u64;
            let id = *seen.entry(binding.geometry.0).or_insert(next);
            (
                id,
                binding.layer_index,
                binding.sub_layer_index,
                binding.tile,
                binding.pass,
                binding.flags.bits(),
            )
        })
        .collect()
}

/// A view's bindings are the same alone as among four.
#[test]
fn a_views_order_does_not_depend_on_its_neighbours() {
    let camera = at(13.0, -0.11);

    let alone = bindings(&[(ViewId(0), camera)]);
    let together = bindings(&[
        (ViewId(0), camera),
        // A cluster inset at a shallower zoom, a second display panned away, and one that
        // overlaps the first exactly — so the group covers sharing, disjointness and partial
        // overlap rather than one of them.
        (ViewId(1), at(11.0, -0.11)),
        (ViewId(2), at(13.0, 2.35)),
        (ViewId(3), at(13.0, -0.11)),
    ]);

    let solo = normalise(alone.get(&ViewId(0)).expect("view 0 drew"));
    let grouped = normalise(together.get(&ViewId(0)).expect("view 0 drew"));

    assert!(!solo.is_empty(), "the invariant is vacuous on nothing");
    assert_eq!(
        solo, grouped,
        "view 0's order changed because other views existed"
    );
}

/// It holds for a view that is not the first one built, too.
///
/// Order of construction is exactly what a shared counter makes visible, so checking only the
/// view that happens to run first would test the easy case.
#[test]
fn the_invariant_holds_for_a_later_view() {
    let camera = at(13.0, 2.35);

    let alone = bindings(&[(ViewId(2), camera)]);
    let together = bindings(&[
        (ViewId(0), at(13.0, -0.11)),
        (ViewId(1), at(11.0, -0.11)),
        (ViewId(2), camera),
    ]);

    let solo = normalise(alone.get(&ViewId(2)).expect("view 2 drew"));
    let grouped = normalise(together.get(&ViewId(2)).expect("view 2 drew"));

    assert!(!solo.is_empty());
    assert_eq!(solo, grouped);
}

/// Two views at the same camera produce the same order as each other.
///
/// The symmetric half: the test above says a view matches itself, this says two views that
/// should agree do. A namespace that leaked between them would fail one or the other.
#[test]
fn two_views_at_one_camera_agree() {
    let camera = at(13.0, -0.11);
    let both = bindings(&[(ViewId(0), camera), (ViewId(3), camera)]);

    let first = normalise(both.get(&ViewId(0)).expect("view 0 drew"));
    let second = normalise(both.get(&ViewId(3)).expect("view 3 drew"));

    assert!(!first.is_empty());
    assert_eq!(first, second);
}
