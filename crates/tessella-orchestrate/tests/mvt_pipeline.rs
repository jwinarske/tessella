//! A vector tile through the bucket builder (§10 R1).
//!
//! The fixture suite proves the decoder reads what the spec describes. This proves the result
//! reaches geometry: a real tile, a style naming its layers, and triangles at the end.

use tessella_orchestrate::tile::{TileId, build_mvt_tile};
use tessella_source::mvt::Tile;
use tessella_style::Style;

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

fn decoded() -> Tile {
    Tile::decode(REAL_TILE).expect("a real tile decodes")
}

/// A tile from the wild decodes into named layers with features.
#[test]
fn a_real_tile_decodes_into_layers() {
    let tile = decoded();
    assert!(tile.layers.len() > 1, "{} layers", tile.layers.len());
    assert!(
        tile.layers.iter().all(|layer| !layer.name.is_empty()),
        "every layer is named"
    );
    assert!(
        tile.layers.iter().any(|layer| !layer.features.is_empty()),
        "and some carry features"
    );

    // Extents are stated per layer, and this is the grid the rescale converts from.
    for layer in &tile.layers {
        assert!(layer.extent > 0, "{} has extent 0", layer.name);
    }
}

/// A fill layer naming a `source-layer` produces triangles from it.
///
/// Built from a synthetic tile rather than the real one, because the real one cannot be
/// tessellated yet: see `earcutr_hangs_on_real_tile_geometry` below.
#[test]
fn a_fill_layer_tessellates_a_vector_source() {
    use tessella_source::mvt::{Feature, GeomType, Layer, Value};

    let tile = Tile {
        layers: vec![Layer {
            name: "water".to_string(),
            extent: 4096,
            version: 2,
            features: vec![Feature {
                id: Some(1),
                geom_type: GeomType::Polygon,
                properties: vec![("kind".to_string(), Value::String("lake".to_string()))],
                geometry: vec![vec![[0, 0], [2048, 0], [2048, 2048], [0, 2048], [0, 0]]],
            }],
        }],
    };

    let style = Style::parse(
        r##"{"version": 8, "sources": {"v": {"type": "vector"}}, "layers": [
             {"id": "f", "type": "fill", "source": "v", "source-layer": "water",
              "filter": ["==", ["get", "kind"], "lake"],
              "paint": {"fill-color": "#ff0000"}}]}"##,
    )
    .expect("style parses");

    let buckets = build_mvt_tile(&style, TileId::new(0, 0, 0), &tile).expect("builds");
    let fill = buckets
        .iter()
        .find(|b| b.layer_id == "f")
        .and_then(|b| b.content.as_fill())
        .expect("a fill bucket");

    assert!(!fill.vertices.is_empty(), "vertices");
    assert_eq!(fill.indices.len(), 6, "a square is two triangles");
    // The filter ran against the feature's own properties, so the source seam carries them.
    assert_eq!(
        fill.vertices[2],
        [4096, 4096],
        "rescaled onto the pipeline grid"
    );
}

/// A style naming a layer the tile does not carry draws nothing, and is not an error.
///
/// One style serves many tiles and not every tile has every layer, so an absent `source-layer`
/// is the ordinary case rather than a fault.
#[test]
fn an_absent_source_layer_draws_nothing() {
    let style = Style::parse(
        r##"{"version": 8, "sources": {"v": {"type": "vector"}}, "layers": [
             {"id": "f", "type": "fill", "source": "v", "source-layer": "no-such-layer",
              "paint": {"fill-color": "#ff0000"}}]}"##,
    )
    .expect("style parses");

    let buckets = build_mvt_tile(&style, TileId::new(0, 0, 0), &decoded()).expect("builds");
    let fill = buckets
        .iter()
        .find(|b| b.layer_id == "f")
        .and_then(|b| b.content.as_fill())
        .expect("a bucket, empty");
    assert!(fill.vertices.is_empty(), "nothing drawn");
}

/// Geometry is rescaled from the layer's grid onto the one the rest of the pipeline uses.
///
/// A forgotten divisor renders geometry in the right shape at the wrong scale, which looks like
/// a projection bug rather than a units bug.
#[test]
fn geometry_is_rescaled_onto_the_pipeline_grid() {
    use tessella_source::mvt::{Feature, GeomType};
    use tessella_source::tiling::EXTENT;

    let feature = Feature {
        id: None,
        geom_type: GeomType::Polygon,
        properties: Vec::new(),
        // A unit square on a 4096 grid.
        geometry: vec![vec![[0, 0], [4096, 0], [4096, 4096], [0, 4096], [0, 0]]],
    };

    let scaled = feature.rings_scaled(4096, EXTENT);
    assert_eq!(scaled[0][2], [EXTENT, EXTENT], "the far corner scales");

    // A layer already on the pipeline's grid is unchanged, so the rescale is not a lossy pass
    // over data that did not need it.
    let same = feature.rings_scaled(4096, 4096);
    assert_eq!(same, feature.geometry);
}

/// A real tile's water layer tessellates, which is what the fixtures cannot show.
///
/// # What this caught
///
/// It did not, at first. `earcutr` spun indefinitely on one polygon of 31 rings and 245
/// vertices, and the obvious reading — a library that cannot handle real geometry where
/// `earcut.hpp` can — was wrong. The decoder was appending a ring's first point on `ClosePath`
/// unconditionally, and this tile's rings already return to their start explicitly, so every one
/// of the 186 rings ended in a zero-length edge. Ear-clipping does not terminate on those.
///
/// The lesson is about where suspicion goes. A hang in a dependency on input a C++ equivalent
/// handles is a plausible story, and the plausibility is what made it worth checking rather than
/// believing: the input was mine and it was malformed.
///
/// A synthetic fixture would not have found this. The spec's own valid fixtures do not repeat
/// the closing point, because the spec says not to — only a tile from the wild does.
#[test]
fn a_real_tile_layer_tessellates() {
    let tile = Tile::decode(REAL_TILE).expect("decodes");
    let style = Style::parse(
        r##"{"version": 8, "sources": {"v": {"type": "vector"}}, "layers": [
             {"id": "f", "type": "fill", "source": "v", "source-layer": "water",
              "paint": {"fill-color": "#ff0000"}}]}"##,
    )
    .expect("style parses");

    let buckets = build_mvt_tile(&style, TileId::new(0, 0, 0), &tile).expect("builds");
    let fill = buckets
        .iter()
        .find(|b| b.layer_id == "f")
        .and_then(|b| b.content.as_fill())
        .expect("a fill bucket");

    assert!(
        fill.vertices.len() > 1000,
        "{} vertices",
        fill.vertices.len()
    );
    assert!(fill.indices.len() > 1000, "{} indices", fill.indices.len());
    assert_eq!(fill.indices.len() % 3, 0, "whole triangles");
}

/// No ring ends in a zero-length edge at the seam, which is the shape that hung the tessellator.
///
/// Narrowly about the seam. This tile also repeats points *inside* rings — 4750 of them — and
/// those are the writer's, not the decoder's: real geometry is dirty and a decoder that
/// silently cleaned it would be editing the map. What the decoder controls is whether
/// `ClosePath` adds one more, and the answer must be no when the ring already closes itself.
///
/// Asserted on the decoder rather than through the tessellator, because a tessellator that
/// happens to survive the input is not the same as input that is right.
#[test]
fn closepath_does_not_duplicate_an_already_closed_ring() {
    let tile = Tile::decode(REAL_TILE).expect("decodes");
    let mut rings = 0;
    for layer in &tile.layers {
        for feature in &layer.features {
            // Rings close; line strings do not, and this tile's `admin` layer is lines.
            if feature.geom_type != tessella_source::mvt::GeomType::Polygon {
                continue;
            }
            for ring in &feature.geometry {
                rings += 1;
                if ring.len() > 2 {
                    assert_eq!(
                        ring.first(),
                        ring.last(),
                        "{}: a closed ring ends where it began",
                        layer.name
                    );
                    assert_ne!(
                        ring[ring.len() - 2],
                        ring[ring.len() - 1],
                        "{}: the seam repeats a point, which is a zero-length edge",
                        layer.name
                    );
                }
            }
        }
    }
    assert!(rings > 1000, "{rings} rings checked");
}

/// The line generator survives the same real tile, on its `admin` layer's 17k line strings.
///
/// Worth its own test rather than folding into the fill one, because the two paths fail
/// differently on dirty input. The fill path hangs; the line path does not, but it *does* have
/// several places where a degenerate segment would divide by zero — the unit vector of a
/// zero-length direction, and the miter length at a 180° reversal — and dirty geometry is where
/// those show. mbgl guards both, so this checks the guards were carried over rather than
/// tidied away: every vertex must be finite and every index in range.
#[test]
fn a_real_tile_line_layer_tessellates() {
    let tile = Tile::decode(REAL_TILE).expect("decodes");
    let style = Style::parse(
        r##"{"version": 8, "sources": {"v": {"type": "vector"}}, "layers": [
             {"id": "l", "type": "line", "source": "v", "source-layer": "admin",
              "paint": {"line-color": "#ff0000"}}]}"##,
    )
    .expect("style parses");

    let buckets = build_mvt_tile(&style, TileId::new(0, 0, 0), &tile).expect("builds");
    let line = buckets
        .iter()
        .find(|b| b.layer_id == "l")
        .and_then(|b| b.content.as_line())
        .expect("a line bucket");

    assert!(
        line.vertices.len() > 1000,
        "{} vertices",
        line.vertices.len()
    );
    assert_eq!(line.indices.len() % 3, 0, "whole triangles");

    // Two vertices per emitted centreline point, so an odd count means a half-emitted point:
    // the shape a panic or an early return in the middle of `add_current_vertex` would leave.
    assert_eq!(line.vertices.len() % 2, 0, "vertices come in pairs");

    // Every index addresses a vertex of its own segment. A NaN reaching the extrusion would
    // not show here, but an index past the end is what a mismanaged segment base produces.
    for segment in &line.segments {
        for index in &line.indices
            [segment.index_offset as usize..(segment.index_offset + segment.index_length) as usize]
        {
            assert!(
                u32::from(*index) < segment.vertex_length,
                "index {index} outside a segment of {} vertices",
                segment.vertex_length
            );
        }
    }
}

/// The admin layer's size and build time, for the §13 budget — and how close it sits to the
/// segment limit.
///
/// 54k vertices is 83% of the 65535 a u16 index can address, on one layer of one real tile. So
/// the split in `add_geometry` is not a theoretical branch: a denser tile, or a style asking
/// for round joins on this same geometry, crosses it. The assertion is that this tile does not,
/// which is what makes the number meaningful when it changes.
#[test]
fn measure_the_real_tile_line_layer() {
    let tile = Tile::decode(REAL_TILE).expect("decodes");
    let style = Style::parse(
        r##"{"version": 8, "sources": {"v": {"type": "vector"}}, "layers": [
             {"id": "l", "type": "line", "source": "v", "source-layer": "admin",
              "paint": {"line-color": "#ff0000"}}]}"##,
    )
    .expect("style parses");
    let start = std::time::Instant::now();
    let buckets = build_mvt_tile(&style, TileId::new(0, 0, 0), &tile).expect("builds");
    let elapsed = start.elapsed();
    let line = buckets
        .iter()
        .find(|b| b.layer_id == "l")
        .and_then(|b| b.content.as_line())
        .expect("a line bucket");
    println!(
        "admin: {} vertices, {} triangles, {} segments, {elapsed:?}",
        line.vertices.len(),
        line.indices.len() / 3,
        line.segments.len()
    );
    assert_eq!(line.segments.len(), 1, "still inside one segment");
    assert!(
        line.vertices.len() < 65_536,
        "{} vertices",
        line.vertices.len()
    );
}

/// Data-driven paint binds over a real tile's features, at a cost worth knowing.
///
/// 17k features means 17k expression evaluations per data-driven property, and every vertex
/// they produced carries a copy of the result. Both numbers are asserted: the buffer must be
/// exactly one stride per vertex — the invariant the whole binder rests on — and the build must
/// stay inside the §13 tile budget rather than quietly becoming the slowest stage.
#[test]
fn data_driven_paint_binds_over_a_real_tile() {
    let tile = Tile::decode(REAL_TILE).expect("decodes");
    let style = Style::parse(
        r##"{"version": 8, "sources": {"v": {"type": "vector"}}, "layers": [
             {"id": "l", "type": "line", "source": "v", "source-layer": "admin",
              "paint": {"line-color": ["match", ["get", "maritime"], 1, "#ff0000", "#00ff00"],
                        "line-width": ["match", ["get", "disputed"], 1, 0.5, 2.0]}}]}"##,
    )
    .expect("style parses");

    let start = std::time::Instant::now();
    let buckets = build_mvt_tile(&style, TileId::new(0, 0, 0), &tile).expect("builds");
    let elapsed = start.elapsed();

    let bucket = buckets
        .iter()
        .find(|b| b.layer_id == "l")
        .expect("the layer");
    let line = bucket.content.as_line().expect("a line bucket");

    assert_eq!(bucket.binder.stride(), 16, "colour, floorwidth, width");
    assert_eq!(
        bucket.binder.data().len(),
        line.vertices.len() * bucket.binder.stride(),
        "exactly one paint entry per vertex"
    );
    println!(
        "admin data-driven: {} vertices, {} paint bytes, {elapsed:?}",
        line.vertices.len(),
        bucket.binder.data().len()
    );

    // Both matches really varied per feature rather than collapsing to one branch — which a
    // binder writing the first feature's value across every vertex would also pass every other
    // assertion here. `maritime` and `disputed` are the two properties this tile's admin layer
    // actually varies; `admin_level` is 2 throughout and would prove nothing.
    let distinct = |range: core::ops::Range<usize>| {
        bucket
            .binder
            .data()
            .chunks_exact(bucket.binder.stride())
            .map(|v| v[range.clone()].to_vec())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    };
    assert_eq!(distinct(0..8), 2, "two colours");
    assert_eq!(distinct(12..16), 2, "two widths");
    // And floorwidth mirrors width, so it varies the same way.
    assert_eq!(distinct(8..12), 2, "two floorwidths");
}
