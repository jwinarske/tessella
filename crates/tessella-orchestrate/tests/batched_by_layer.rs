//! A layer's tiles land in one slab, so a consumer can draw them in one call.
//!
//! # What a batch is, on this wire
//!
//! Nothing in the envelope says "batch". A consumer forms one by reading the draw order and
//! taking a run of consecutive entries that agree on everything a pipeline binding is made of:
//! the layer and sub-layer, the pass, the shader and its permutation, the vertex layout, and —
//! the one that is not a property of the style — the slab their attributes live in. That last
//! one is the hinge. A draw call reads one vertex buffer, so two tiles of the same layer are
//! one draw if and only if their geometry sits in the same slab.
//!
//! Each sub-draw then needs its own transform, which is what `ubo_index` on the order entry
//! already is: DR-16's consolidated per-(view, layer) buffer, indexed per drawable. So the run
//! becomes one indirect draw whose commands carry a base vertex from each geometry's slab
//! offset and an index from each order entry.
//!
//! # Why this is a test and not a comment
//!
//! The property is not local to any one function. It holds because the arena packs in the order
//! `resolve()` returns and seals on a layer change, and it would be silently lost by packing a
//! bucket anywhere else — with no visible symptom, because the frame renders identically either
//! way. The only thing that changes is that a consumer's draw count goes back up by the tile
//! count, which no assertion on pixels or on values would catch.

use std::collections::BTreeMap;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{
    AttributeDesc, GeometryAdd, OrderEntry, OrderUpdate, ViewId, WireRecord,
};
use tessella_capture_abi::ring::Ring;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

const STYLE: &str = r##"{
  "version": 8,
  "sources": {"src": {"type": "vector", "tiles": []}},
  "layers": [
    {"id": "bg", "type": "background", "paint": {"background-color": "#101418"}},
    {"id": "sea", "type": "fill", "source": "src", "source-layer": "water",
     "paint": {"fill-color": "#20344c"}},
    {"id": "banks", "type": "line", "source": "src", "source-layer": "water",
     "paint": {"line-color": "#88a", "line-width": 1.5}},
    {"id": "blocks", "type": "fill-extrusion", "source": "src", "source-layer": "water",
     "paint": {"fill-extrusion-height": 20}}
  ]
}"##;

/// What a consumer would key a pipeline binding on.
#[derive(PartialEq, Eq, Clone, Debug)]
struct BatchKey {
    layer: u32,
    sub_layer: i32,
    pass: u8,
    shader: i32,
    permutation: u64,
    slab: u32,
    stride: u32,
    offsets: Vec<(u32, u32)>,
}

/// How a geometry is bound: everything but the order entry's own fields.
#[derive(Clone, Debug)]
struct Binding {
    shader: i32,
    permutation: u64,
    slab: u32,
    /// Where the position buffer begins, so two geometries naming one buffer are visible.
    at: u32,
    stride: u32,
    offsets: Vec<(u32, u32)>,
}

/// Reads one frame's `GeometryAdd`s and its draw order back off the ring.
fn frame_stream() -> (BTreeMap<u64, Binding>, Vec<OrderEntry>) {
    let style = Style::parse(STYLE).expect("the style parses");
    // Zoom three, so the cover is more than one tile and a layer has something to batch.
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 3.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    assert!(tiles.len() > 1, "the cover has to have tiles to batch");
    let decoded = Tile::decode(REAL_TILE).expect("the fixture decodes");

    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
        built.extend(build_sourceless(&style, id).expect("the background builds"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, built));
    }

    let mut ring = Ring::new(1 << 24);
    let (producer, consumer) = ring.split();
    let mut arena = SlabArena::new();
    frame::emit(
        producer,
        &mut arena,
        &Frame {
            style: &style,
            view: &view,
            view_id: ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
    )
    .expect("the frame emits");

    let mut bindings = BTreeMap::new();
    let mut order = Vec::new();
    while let Some(record) = consumer.peek() {
        match record.kind {
            EnvelopeKind::GeometryAdd => {
                if let Some(add) = GeometryAdd::from_bytes(record.record) {
                    let size = size_of::<AttributeDesc>();
                    let start = add.attrs.offset as usize;
                    let attrs: Vec<AttributeDesc> = (0..add.attrs.count as usize)
                        .filter_map(|index| {
                            record
                                .payload
                                .get(start + index * size..)
                                .and_then(AttributeDesc::from_bytes)
                        })
                        .collect();
                    // Attribute zero is the position, and the buffer it names is the one a draw
                    // call binds.
                    if let Some(position) = attrs.iter().find(|attr| attr.attr_id == 0) {
                        bindings.insert(
                            add.geometry.0,
                            Binding {
                                shader: add.builtin_shader,
                                permutation: add.permutation_key,
                                slab: position.source.slab,
                                at: position.source.offset,
                                stride: position.stride,
                                offsets: attrs.iter().map(|a| (a.attr_id, a.offset)).collect(),
                            },
                        );
                    }
                }
            }
            EnvelopeKind::OrderUpdate => {
                if let Some(update) = OrderUpdate::from_bytes(record.record) {
                    let size = size_of::<OrderEntry>();
                    let start = update.entries.offset as usize;
                    order = (0..update.entries.count as usize)
                        .filter_map(|index| {
                            record
                                .payload
                                .get(start + index * size..)
                                .and_then(OrderEntry::from_bytes)
                        })
                        .collect();
                }
            }
            _ => {}
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    assert!(!order.is_empty(), "the frame emitted an order");
    (bindings, order)
}

/// Groups the order into the runs a consumer would draw, returning each run's key and length.
fn batches(bindings: &BTreeMap<u64, Binding>, order: &[OrderEntry]) -> Vec<(BatchKey, usize)> {
    let mut runs: Vec<(BatchKey, usize)> = Vec::new();
    let mut previous: Option<BatchKey> = None;
    for entry in order {
        // A background has no geometry; it is its own draw and it breaks the run.
        let Some(binding) = bindings.get(&entry.geometry.0) else {
            previous = None;
            continue;
        };
        let key = BatchKey {
            layer: entry.layer_index,
            sub_layer: entry.sub_layer_index,
            pass: entry.pass.bits(),
            shader: binding.shader,
            permutation: binding.permutation,
            slab: binding.slab,
            stride: binding.stride,
            offsets: binding.offsets.clone(),
        };
        match runs.last_mut() {
            Some((last, count)) if previous.as_ref() == Some(&key) && *last == key => *count += 1,
            _ => runs.push((key.clone(), 1)),
        }
        previous = Some(key);
    }
    runs
}

/// No run is broken by a slab boundary alone.
///
/// The style's other fields are allowed to break one — that is a real pipeline change. A slab
/// that changes while everything else stays the same is the packing failing to follow the draw
/// order, and it costs a draw call per tile.
#[test]
fn a_slab_never_splits_a_batch() {
    let (bindings, order) = frame_stream();
    let runs = batches(&bindings, &order);

    for pair in runs.windows(2) {
        let (before, after) = (&pair[0].0, &pair[1].0);
        let same_pipeline = before.layer == after.layer
            && before.sub_layer == after.sub_layer
            && before.pass == after.pass
            && before.shader == after.shader
            && before.permutation == after.permutation
            && before.stride == after.stride
            && before.offsets == after.offsets;
        assert!(
            !(same_pipeline && before.slab != after.slab),
            "layer {} sub-layer {} was split across slabs {} and {}, \
             which is one draw call per tile instead of one for the layer",
            before.layer,
            before.sub_layer,
            before.slab,
            after.slab
        );
    }
}

/// A batch really does span tiles, rather than the assertion above holding vacuously.
///
/// A cover of N tiles that draws a layer N times satisfies "no slab splits a batch" perfectly
/// well if every one of those draws is a run of one. What is being claimed is stronger: the
/// tiles of a layer are *in* one run.
#[test]
fn a_batch_spans_the_cover() {
    let (bindings, order) = frame_stream();
    let runs = batches(&bindings, &order);

    let drawables = order
        .iter()
        .filter(|entry| bindings.contains_key(&entry.geometry.0))
        .count();
    let longest = runs.iter().map(|(_, count)| *count).max().unwrap_or(0);

    assert!(
        longest > 1,
        "every run is a single drawable: {drawables} drawables in {} runs",
        runs.len()
    );
    assert!(
        runs.len() < drawables,
        "{drawables} drawables became {} runs, which is no batching at all",
        runs.len()
    );
}

/// A slab holds one layer, which is DR-16's "consolidated buffer per (view, layer)".
///
/// Batching does not need this — packing everything into a single frame-wide slab batches just
/// as well, and the two tests above would pass. It is granularity that needs it. A slab is the
/// unit §5.3 refcounts, so a layer whose tiles did not change keeps its buffer while a
/// neighbour's is replaced; one slab for the frame would make every frame rebuild every byte.
/// It is also the unit a consumer allocates, and a driver's maximum buffer size is a real
/// number.
///
/// The layer, and not the sub-layer within it: a bucket's drawables share one geometry, so a
/// fill's triangles and its outline name the same bytes from either side of a sub-layer
/// boundary. Sealing between them would have to copy the bytes to keep them apart.
#[test]
fn a_slab_holds_one_layer() {
    let (bindings, order) = frame_stream();

    let mut owner: BTreeMap<u32, u32> = BTreeMap::new();
    for entry in &order {
        let Some(binding) = bindings.get(&entry.geometry.0) else {
            continue;
        };
        let layer = entry.layer_index;
        match owner.get(&binding.slab) {
            Some(previous) => assert_eq!(
                *previous, layer,
                "slab {} holds both layer {previous} and layer {layer}",
                binding.slab
            ),
            None => {
                owner.insert(binding.slab, layer);
            }
        }
    }
    assert!(owner.len() > 1, "the frame used more than one slab");
}

/// A bucket's bytes reach the arena once, however many drawables it produces.
///
/// A translucent extrusion draws twice — a depth-only pass and then a colour pass — and the two
/// differ in render state and `ubo_index`, neither of which a `GeometryAdd` carries. Encoding
/// per drawable copied every vertex, index and interleaved attribute a second time, which on a
/// city-sized cover was the largest single cost in `emit`.
///
/// The two records keep separate ids and name the same bytes. Separate ids because
/// `ViewRelease` is keyed by (geometry, view): sharing one would mean a single release dropped
/// both drawables, with nothing in the stream to say it had.
#[test]
fn a_bucket_is_encoded_once() {
    let (bindings, order) = frame_stream();

    // Distinct geometry ids whose position buffer begins at the same place in the same slab.
    // Every bucket in this fixture is built from the same tile, so equal *bytes* prove nothing —
    // equal offsets prove the bytes were written once.
    let mut at_offset: BTreeMap<(u32, u32), Vec<u64>> = BTreeMap::new();
    for (geometry, binding) in &bindings {
        at_offset
            .entry((binding.slab, binding.at))
            .or_default()
            .push(*geometry);
    }

    let shared: usize = at_offset.values().filter(|ids| ids.len() > 1).count();
    let drawables = order
        .iter()
        .filter(|entry| bindings.contains_key(&entry.geometry.0))
        .count();

    assert!(
        shared > 0,
        "no bucket's drawables shared their bytes: {drawables} drawables over {} buffers",
        at_offset.len()
    );
    assert!(
        at_offset.len() < bindings.len(),
        "every geometry got its own buffer, so nothing was shared"
    );
}
