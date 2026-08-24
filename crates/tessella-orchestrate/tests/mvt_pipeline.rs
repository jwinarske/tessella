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
