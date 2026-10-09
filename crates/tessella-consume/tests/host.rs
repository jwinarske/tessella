//! Reading a real ring and planning what it says to draw.
//!
//! These drive the producer half directly rather than a fixture, so what is under test is the
//! whole path: bytes on a ring, dispatched, joined, paired with a camera, collapsed into batches.

use tessella_capture_abi::envelope::DrawFlags;
use tessella_capture_abi::envelope::{
    CameraUpdate, GeometryAdd, GeometryId, GeometryRemove, OrderEntry, OrderEpoch, OrderUpdate,
    Span, StencilTile, StencilTiles, TileId, ViewDeclare, ViewId, ViewRelease, ViewUndeclare,
    ViewUse, WireRecord,
};
use tessella_capture_abi::ring::{Producer, Ring};
use tessella_capture_abi::{CameraMode, EnvelopeKind, RenderPass};
use tessella_consume::host::Host;

const CAPACITY: usize = 1 << 16;

fn geometry(id: u64, shader: i32) -> GeometryAdd {
    GeometryAdd {
        geometry: GeometryId(id),
        permutation_key: 0,
        indexes: Default::default(),
        vertex_count: 3,
        attrs: Span::default(),
        instance_attrs: Span::default(),
        segments: Span::default(),
        texture_refs: Span::default(),
        builtin_shader: shader,
        vertex_type: 0,
        reason: 0,
        topology: 0,
        _pad: [0; 1],
    }
}

fn use_of(id: u64, view: u32, layer: i32) -> ViewUse {
    ViewUse {
        geometry: GeometryId(id),
        view: ViewId(view),
        layer_index: layer,
        sub_layer_index: 0,
        tile: TileId::default(),
        render_pass: RenderPass::TRANSLUCENT,
        draw_flags: DrawFlags::default(),
        has_tile: 0,
        _pad: [0; 5],
    }
}

/// A tile, as the producer names one.
fn tile(z: u8, x: u32, y: u32) -> TileId {
    TileId {
        z,
        x,
        y,
        overscaled_z: z,
        wrap: 0,
    }
}

/// One tile's mask, with a matrix that names the tile so a mix-up is visible.
fn clip(z: u8, x: u32, y: u32) -> StencilTile {
    let mut matrix = [0.0f32; 16];
    matrix[0] = x as f32;
    matrix[5] = y as f32;
    matrix[15] = 1.0;
    StencilTile {
        matrix,
        tile: tile(z, x, y),
    }
}

/// Writes one layer group's clip set.
fn stencil_tiles(producer: &mut Producer, view: u32, layer: i32, tiles: &[StencilTile]) {
    let mut payload = Vec::new();
    for one in tiles {
        payload.extend_from_slice(one.as_bytes());
    }
    let update = StencilTiles {
        view: ViewId(view),
        layer_index: layer,
        tiles: Span {
            offset: 0,
            count: u32::try_from(tiles.len()).expect("a small cover"),
        },
    };
    producer
        .write(EnvelopeKind::StencilTiles, update.as_bytes(), &payload)
        .expect("room");
}

fn entry(id: u64, layer: u32, ubo: u32) -> OrderEntry {
    OrderEntry {
        geometry: GeometryId(id),
        draw_priority: 0,
        layer_index: layer,
        sub_layer_index: 0,
        ubo_index: ubo,
        pass: RenderPass::TRANSLUCENT,
        _pad: [0; 3],
    }
}

fn order(view: u32, epoch: u64, entries: &[OrderEntry]) -> (OrderUpdate, Vec<u8>) {
    let mut payload = Vec::new();
    for entry in entries {
        payload.extend_from_slice(entry.as_bytes());
    }
    (
        OrderUpdate {
            order_epoch: OrderEpoch(epoch),
            view: ViewId(view),
            _pad: 0,
            entries: Span {
                offset: 0,
                count: u32::try_from(entries.len()).unwrap(),
            },
        },
        payload,
    )
}

/// A camera, built from zeros because every bit pattern of a record is a legal value and only two
/// of its fields matter here.
fn camera(view: u32, epoch: u64) -> CameraUpdate {
    let zeros = vec![0u8; core::mem::size_of::<CameraUpdate>()];
    let mut update = CameraUpdate::from_bytes(&zeros).expect("a camera reads from zeros");
    update.view = ViewId(view);
    update.order_epoch = OrderEpoch(epoch);
    update
}

/// Declares a view, which has to come before anything that names it.
///
/// DR-18: `ViewDeclare` carries the per-view state once and is "ordered ahead of any `ViewUse`
/// naming the view". A use that arrives first is dropped and counted in `Progress::undeclared`.
fn declare(producer: &mut Producer, view: u32) {
    let record = ViewDeclare {
        view: ViewId(view),
        camera_mode: CameraMode::Producer as u8,
        _reserved: [0; 3],
    };
    producer
        .write(EnvelopeKind::ViewDeclare, record.as_bytes(), &[])
        .expect("room");
}

/// Writes the usual shape: a view, a geometry, a use of it, an order, then the camera committing it.
fn write_frame(producer: &mut Producer, view: u32, epoch: u64, ids: &[u64]) {
    declare(producer, view);
    for id in ids {
        producer
            .write(EnvelopeKind::GeometryAdd, geometry(*id, 11).as_bytes(), &[])
            .expect("room");
        producer
            .write(EnvelopeKind::ViewUse, use_of(*id, view, 0).as_bytes(), &[])
            .expect("room");
    }
    let entries: Vec<OrderEntry> = ids
        .iter()
        .enumerate()
        .map(|(at, id)| entry(*id, 0, u32::try_from(at).unwrap()))
        .collect();
    let (update, payload) = order(view, epoch, &entries);
    producer
        .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
        .expect("room");
    producer
        .write(
            EnvelopeKind::CameraUpdate,
            camera(view, epoch).as_bytes(),
            &[],
        )
        .expect("room");
}

/// A frame read off a ring plans the draws its order names.
#[test]
fn a_frame_plans_what_its_order_names() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        write_frame(producer, 0, 1, &[1, 2, 3]);
    }
    let progress = host.read(ring.consumer());

    assert_eq!(progress.unknown, 0, "every kind here is known");
    assert_eq!(progress.malformed, 0, "and every record parses");
    assert!(host.ready(ViewId(0)), "camera and order agree");

    let frame = host.plan(ViewId(0)).expect("a plan");
    assert_eq!(frame.plan.view, ViewId(0));
    assert_eq!(frame.plan.epoch, OrderEpoch(1));
    assert_eq!(frame.batches.len(), 1, "three like drawables collapse");
    assert_eq!(
        frame.batches.get(0).expect("a batch").geometries,
        [GeometryId(1), GeometryId(2), GeometryId(3)]
    );
}

/// A camera whose epoch the held order does not establish does not commit a frame.
///
/// §11.7: hold `CameraUpdate` until its `orderEpoch` is held. Drawing anyway is this frame's
/// geometry through the last frame's matrices, which is a picture rather than an error.
#[test]
fn a_camera_ahead_of_its_order_does_not_commit() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::GeometryAdd, geometry(1, 11).as_bytes(), &[])
            .expect("room");
        producer
            .write(EnvelopeKind::ViewUse, use_of(1, 0, 0).as_bytes(), &[])
            .expect("room");
        let (update, payload) = order(0, 1, &[entry(1, 0, 0)]);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        // The camera names an epoch the order does not establish.
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 2).as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    assert!(!host.ready(ViewId(0)), "the epochs disagree");
    assert!(host.plan(ViewId(0)).is_none());

    // The order that establishes the epoch arrives, and the frame commits.
    {
        let (producer, _) = ring.split();
        let (update, payload) = order(0, 2, &[entry(1, 0, 0)]);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
    }
    host.read(ring.consumer());
    assert!(host.ready(ViewId(0)));
    assert_eq!(
        host.plan(ViewId(0)).expect("a plan").plan.epoch,
        OrderEpoch(2)
    );
}

/// The position to acknowledge is the highest at which the plan's geometry was announced.
#[test]
fn a_plan_reports_where_to_acknowledge() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        write_frame(producer, 0, 1, &[1, 2]);
    }
    host.read(ring.consumer());

    let plan = host.plan(ViewId(0)).expect("a plan").plan;
    assert!(plan.announced_through > 0, "something was announced");
    assert!(
        plan.announced_through <= host.read_through(),
        "and it was announced within what has been read"
    );
}

/// An unknown kind is counted and does not stop the read.
///
/// The ABI is revisioned; a newer producer may send a kind this consumer predates. A stream that
/// is mostly unknown is a version mismatch presenting as a blank map, which is why it is counted
/// rather than ignored.
#[test]
fn an_unknown_kind_is_counted_and_survived() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::GeometryAdd, geometry(1, 11).as_bytes(), &[])
            .expect("room");
        declare(producer, 0);
        producer
            .write(EnvelopeKind::ViewUse, use_of(1, 0, 0).as_bytes(), &[])
            .expect("room");
        // A kind this host does not dispatch. `MeshAdd` is the one left -- `ViewDeclare` was, until
        // it was read -- so whoever implements #93's mesh item changes this to `ViewTarget` and the
        // last of them turns this case into one that can no longer be written.
        producer
            .write(EnvelopeKind::MeshAdd, &[0u8; 16], &[])
            .expect("room");
        let (update, payload) = order(0, 1, &[entry(1, 0, 0)]);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 1).as_bytes(), &[])
            .expect("room");
    }
    let progress = host.read(ring.consumer());

    assert_eq!(progress.unknown, 1, "the undispatched kind is counted");
    assert_eq!(progress.records, 6, "and the read did not stop at it");
    assert!(host.plan(ViewId(0)).is_some(), "the frame still draws");
}

/// A known kind whose bytes will not parse is counted apart from an unknown one.
///
/// An unknown kind is a newer producer; a known kind that will not parse is a corrupt stream or a
/// size disagreement, and the two want different answers from whoever is looking.
#[test]
fn a_malformed_record_is_counted_apart() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::GeometryAdd, &[0u8; 4], &[])
            .expect("room");
    }
    let progress = host.read(ring.consumer());

    assert_eq!(progress.malformed, 1);
    assert_eq!(progress.unknown, 0, "it is not an unknown kind");
    assert_eq!(progress.records, 1);
}

/// Retiring and releasing reach the joiner.
#[test]
fn a_retire_takes_the_geometry_out_of_the_plan() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        write_frame(producer, 0, 1, &[1, 2]);
    }
    host.read(ring.consumer());
    let batches = host.plan(ViewId(0)).expect("a plan").batches;
    assert_eq!(batches.get(0).expect("a batch").geometries.len(), 2);

    {
        let (producer, _) = ring.split();
        let remove = GeometryRemove {
            geometry: GeometryId(1),
        };
        producer
            .write(EnvelopeKind::GeometryRemove, remove.as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    let batches = host.plan(ViewId(0)).expect("a plan").batches;
    assert_eq!(
        batches.get(0).expect("a batch").geometries,
        [GeometryId(2)],
        "the retired geometry is skipped, not drawn with stale bindings"
    );
}

/// Releasing one view's hold leaves the other view drawing.
#[test]
fn a_release_leaves_the_other_view() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::GeometryAdd, geometry(1, 11).as_bytes(), &[])
            .expect("room");
        for view in 0..2 {
            declare(producer, view);
            producer
                .write(EnvelopeKind::ViewUse, use_of(1, view, 0).as_bytes(), &[])
                .expect("room");
            let (update, payload) = order(view, 1, &[entry(1, 0, 0)]);
            producer
                .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
                .expect("room");
            producer
                .write(EnvelopeKind::CameraUpdate, camera(view, 1).as_bytes(), &[])
                .expect("room");
        }
        let release = ViewRelease {
            geometry: GeometryId(1),
            view: ViewId(0),
            _pad: 0,
        };
        producer
            .write(EnvelopeKind::ViewRelease, release.as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    {
        let batches = host.plan(ViewId(0)).expect("a plan").batches;
        assert!(batches.is_empty(), "the released view draws nothing");
    }
    let batches = host.plan(ViewId(1)).expect("a plan").batches;
    assert_eq!(
        batches.get(0).expect("a batch").geometries,
        [GeometryId(1)],
        "the other still does"
    );
}

/// An unchanged frame is planned once and served from the cache after.
///
/// The reason the host keeps the batches. A still map re-collapsed an identical order every frame
/// -- 371 microseconds at the quad's entry count, for byte-identical output, while the producer
/// feeding it emitted nothing.
#[test]
fn an_unchanged_frame_is_not_replanned() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        write_frame(producer, 0, 1, &[1, 2, 3]);
    }
    host.read(ring.consumer());

    assert!(host.stale(ViewId(0)), "nothing planned yet");
    host.plan(ViewId(0)).expect("a plan");
    assert!(!host.stale(ViewId(0)), "and now it is cached");

    for _ in 0..8 {
        let batches = host.plan(ViewId(0)).expect("a plan").batches;
        assert_eq!(batches.len(), 1, "the same batches every time");
        assert!(!host.stale(ViewId(0)), "and no replanning");
    }
}

/// Anything that changes what a view draws makes its plan stale.
///
/// The order's epoch alone is not enough: a geometry re-announced with a different permutation is
/// a different program, which moves where the collapse breaks, and it arrives without a new order.
#[test]
fn a_re_announcement_makes_the_plan_stale() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        write_frame(producer, 0, 1, &[1, 2]);
    }
    host.read(ring.consumer());
    host.plan(ViewId(0)).expect("a plan");
    assert!(!host.stale(ViewId(0)));

    // The same geometry again, a different family: same order, same epoch, different collapse.
    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::GeometryAdd, geometry(1, 20).as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    assert!(
        host.stale(ViewId(0)),
        "the order did not change and what it draws did"
    );
    let batches = host.plan(ViewId(0)).expect("a plan").batches;
    assert_eq!(batches.len(), 2, "the two families no longer collapse");
}

/// Retiring geometry a view draws makes its plan stale too.
#[test]
fn a_retire_makes_the_plan_stale() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        write_frame(producer, 0, 1, &[1, 2]);
    }
    host.read(ring.consumer());
    host.plan(ViewId(0)).expect("a plan");

    {
        let (producer, _) = ring.split();
        let remove = GeometryRemove {
            geometry: GeometryId(1),
        };
        producer
            .write(EnvelopeKind::GeometryRemove, remove.as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());
    assert!(host.stale(ViewId(0)));
}

/// A view nobody has ordered has no plan, and asking is not an error.
#[test]
fn an_unknown_view_has_no_plan() {
    let mut host = Host::new();
    assert!(host.plan(ViewId(9)).is_none());
    assert!(!host.ready(ViewId(9)));
}

/// A frame's batches and its joiner are held at the same time.
///
/// Which is the whole of what `Frame` is for, and the assertion is that this *compiles*. Recording
/// a draw needs both -- the batches say which program and which drawables, the joiner says which
/// tile each drawable is in and so which stencil reference it draws with -- and when `plan`
/// returned only the batches, a caller asking for the joiner afterwards got
///
/// ```text
/// error[E0502]: cannot borrow `host` as immutable because it is also borrowed as mutable
/// ```
///
/// There was no way round it from outside: cloning the batches defeats the cache, and taking the
/// joiner first conflicts the other way. So this is a compile-time guard with a run-time shape,
/// and it would have failed to build rather than failed to pass.
#[test]
fn a_frame_holds_its_batches_beside_its_joiner() {
    let mut ring = Ring::new(CAPACITY);
    let (producer, consumer) = ring.split();
    write_frame(producer, 0, 1, &[1, 2]);

    let mut host = Host::new();
    host.read(consumer);

    let frame = host.plan(ViewId(0)).expect("a plan");
    // Both at once, which is what a backend recording a frame does.
    let resolved = frame
        .batches
        .iter()
        .flat_map(|batch| batch.geometries.to_vec())
        .filter(|geometry| frame.joiner.drawable(*geometry, frame.plan.view).is_some())
        .count();
    assert_eq!(
        resolved, 2,
        "the joiner has to resolve every drawable the batches name"
    );
}

/// A view with no `StencilTiles` clips nothing, and says so rather than having no answer.
///
/// `Partition`'s own words: "a tile absent here has no mask; its geometry is left unclipped". So the
/// absence is a usable answer and a consumer needs no special case -- which is why `Frame` carries
/// a `Clips` rather than an `Option<&Clips>`.
#[test]
fn a_view_with_no_masks_clips_nothing() {
    let mut ring = Ring::new(CAPACITY);
    write_frame(ring.producer(), 0, 1, &[1]);
    let mut host = Host::new();
    host.read(ring.consumer());

    let frame = host.plan(ViewId(0)).expect("a plan");
    assert!(frame.clips.is_empty(), "no masks were sent");
    assert_eq!(frame.clips.len(), 0);
    assert!(
        frame.clips.partition().tiles.is_empty(),
        "and so no tile has an assignment"
    );
    assert!(
        !frame.clips.partition().partitioned,
        "an empty partition is the fallback, not a fitted one"
    );
}

/// The tiles a layer group names become assignments, and the masks keep their matrices.
///
/// `stencil::partition` was written and tested and nothing fed it from the stream: `StencilTiles`
/// was one of five envelope kinds the read did not match, so `record::content` had no partition to
/// look a drawable's tile up in and every layer drew unclipped.
#[test]
fn a_stencil_set_becomes_a_partition() {
    let mut ring = Ring::new(CAPACITY);
    write_frame(ring.producer(), 0, 1, &[1]);
    let cover = [clip(14, 8000, 5000), clip(14, 8001, 5000)];
    stencil_tiles(ring.producer(), 0, 0, &cover);

    let mut host = Host::new();
    let progress = host.read(ring.consumer());
    assert_eq!(progress.unknown, 0, "StencilTiles is a kind this reads");
    assert_eq!(progress.malformed, 0);

    let frame = host.plan(ViewId(0)).expect("a plan");
    assert_eq!(frame.clips.len(), 2, "one mask per tile");
    let partition = frame.clips.partition();
    assert_eq!(partition.tiles.len(), 2, "and one assignment per tile");

    // Two tiles at one zoom take distinct values in a shared field, which is what the whole scheme
    // is for -- the same value for both would clip each to the other's shape as well.
    let first = partition.tiles[&cover[0].tile];
    let second = partition.tiles[&cover[1].tile];
    assert_ne!(
        first.value, second.value,
        "two tiles of one cover must not share a reference"
    );
    assert_eq!(first.read_mask, second.read_mask, "one zoom, one field");

    // The matrices come back beside their tiles, because the mask quad is drawn with them.
    let masks: Vec<(TileId, [f32; 16])> = frame
        .clips
        .masks()
        .map(|(tile, matrix)| (tile, *matrix))
        .collect();
    assert_eq!(masks.len(), 2);
    for sent in &cover {
        assert!(
            masks.contains(&(sent.tile, sent.matrix)),
            "the matrix for {:?} did not survive the decode",
            sent.tile
        );
    }
}

/// A later set for the same group replaces the earlier one rather than adding to it.
///
/// `StencilTiles` is "emitted on change only", so what arrives is the group's whole tile set. Tiles
/// the producer dropped must lose their masks: a matrix kept for a tile no group names any more is
/// a mask quad drawn over the map.
#[test]
fn a_later_set_replaces_the_earlier_one() {
    let mut ring = Ring::new(CAPACITY);
    write_frame(ring.producer(), 0, 1, &[1]);
    stencil_tiles(
        ring.producer(),
        0,
        0,
        &[clip(14, 8000, 5000), clip(14, 8001, 5000)],
    );
    let mut host = Host::new();
    host.read(ring.consumer());
    assert_eq!(host.clips(ViewId(0)).len(), 2);

    // The cover moves on: one tile stays, one is new, one is gone.
    stencil_tiles(
        ring.producer(),
        0,
        0,
        &[clip(14, 8001, 5000), clip(14, 8002, 5000)],
    );
    host.read(ring.consumer());

    let clips = host.clips(ViewId(0));
    assert_eq!(clips.len(), 2, "two tiles, not three");
    let tiles: Vec<TileId> = clips.masks().map(|(tile, _)| tile).collect();
    assert!(
        !tiles.contains(&tile(14, 8000, 5000)),
        "the dropped tile kept its mask"
    );
    assert!(
        tiles.contains(&tile(14, 8002, 5000)),
        "the new tile has none"
    );
}

/// A short tile run is refused rather than clipping a layer to some of its tiles.
#[test]
fn a_short_stencil_run_is_malformed() {
    let mut ring = Ring::new(CAPACITY);
    let sent = [clip(14, 8000, 5000), clip(14, 8001, 5000)];
    let mut payload = Vec::new();
    for one in &sent {
        payload.extend_from_slice(one.as_bytes());
    }
    // The record claims both and the payload carries one and a half.
    payload.truncate(payload.len() - core::mem::size_of::<StencilTile>() / 2);
    let update = StencilTiles {
        view: ViewId(0),
        layer_index: 0,
        tiles: Span {
            offset: 0,
            count: 2,
        },
    };
    ring.producer()
        .write(EnvelopeKind::StencilTiles, update.as_bytes(), &payload)
        .expect("room");

    let mut host = Host::new();
    let progress = host.read(ring.consumer());
    assert_eq!(progress.malformed, 1, "a short run is malformed");
    assert!(
        host.clips(ViewId(0)).is_empty(),
        "and nothing was taken from it"
    );
}

/// A use of a view that was never declared is dropped and counted.
///
/// The ABI's own words for `ViewUse`: it "carries nothing about the view itself -- that is
/// `ViewDeclare`'s job (DR-18)", and one "naming a view the consumer has not seen declared is a
/// protocol fault". Dropped rather than joined, because a view with no declaration has no camera
/// mode, and in consumer mode every per-drawable matrix on the wire is advisory -- a backend that
/// joined it anyway would not know what the matrices it is about to bind mean.
///
/// Counted apart from `malformed`: the bytes parse. A producer out of order and a producer sending
/// rubbish are different faults and the numbers should say which.
#[test]
fn a_use_of_an_undeclared_view_is_dropped() {
    let mut ring = Ring::new(CAPACITY);
    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::GeometryAdd, geometry(1, 11).as_bytes(), &[])
            .expect("room");
        // No `ViewDeclare` before it.
        producer
            .write(EnvelopeKind::ViewUse, use_of(1, 0, 0).as_bytes(), &[])
            .expect("room");
        let (update, payload) = order(0, 1, &[entry(1, 0, 0)]);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 1).as_bytes(), &[])
            .expect("room");
    }
    let mut host = Host::new();
    let progress = host.read(ring.consumer());

    assert_eq!(progress.undeclared, 1, "the use is counted as a fault");
    assert_eq!(
        progress.malformed, 0,
        "and not as malformed: the bytes parse"
    );
    assert_eq!(progress.records, 4, "the read did not stop at it");
    assert!(!host.declared(ViewId(0)));
    assert_eq!(host.camera_mode(ViewId(0)), None);

    // The order and the camera still arrived, so there is a plan -- with nothing in it, because the
    // use that would have put the geometry in this view was dropped.
    let frame = host.plan(ViewId(0)).expect("an order and a camera agree");
    assert!(
        frame.batches.is_empty(),
        "a dropped use must not leave a drawable in the plan"
    );
}

/// A declared view says which side owns its camera.
#[test]
fn a_declared_view_carries_its_camera_mode() {
    let mut ring = Ring::new(CAPACITY);
    {
        let (producer, _) = ring.split();
        let record = ViewDeclare {
            view: ViewId(3),
            camera_mode: CameraMode::Consumer as u8,
            _reserved: [0; 3],
        };
        producer
            .write(EnvelopeKind::ViewDeclare, record.as_bytes(), &[])
            .expect("room");
    }
    let mut host = Host::new();
    let progress = host.read(ring.consumer());
    assert_eq!(progress.malformed, 0);
    assert!(host.declared(ViewId(3)));
    assert_eq!(host.camera_mode(ViewId(3)), Some(CameraMode::Consumer));
}

/// A camera mode this build does not know is refused rather than defaulted.
///
/// The mode decides what a per-drawable matrix *means*, so guessing it is how geometry gets placed
/// through a camera that is not the one drawing. Refused the same way an unknown texture format is,
/// which tessella#364 settled for the other two discriminants on this wire.
#[test]
fn an_unknown_camera_mode_is_malformed() {
    let mut ring = Ring::new(CAPACITY);
    {
        let (producer, _) = ring.split();
        let record = ViewDeclare {
            view: ViewId(4),
            camera_mode: 7,
            _reserved: [0; 3],
        };
        producer
            .write(EnvelopeKind::ViewDeclare, record.as_bytes(), &[])
            .expect("room");
    }
    let mut host = Host::new();
    let progress = host.read(ring.consumer());
    assert_eq!(progress.malformed, 1);
    assert!(
        !host.declared(ViewId(4)),
        "a view whose mode did not decode is not declared"
    );
}

/// Undeclaring a view drops everything held for it, and leaves the other view alone.
///
/// Every map in the host keyed by a view: its order, its camera, its clips, its cached batches and
/// its uses. A consumer that dropped only the order would keep the rest for a view that no longer
/// exists, which on a cluster adding and removing insets is a leak per inset.
///
/// The announcements are deliberately not dropped: geometry is shared, and the other view here
/// still holds the same one.
#[test]
fn undeclaring_a_view_drops_what_was_held_for_it() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::GeometryAdd, geometry(1, 11).as_bytes(), &[])
            .expect("room");
        for view in 0..2 {
            declare(producer, view);
            producer
                .write(EnvelopeKind::ViewUse, use_of(1, view, 0).as_bytes(), &[])
                .expect("room");
            let (update, payload) = order(view, 1, &[entry(1, 0, 0)]);
            producer
                .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
                .expect("room");
            producer
                .write(EnvelopeKind::CameraUpdate, camera(view, 1).as_bytes(), &[])
                .expect("room");
        }
        stencil_tiles(producer, 0, 0, &[clip(14, 8000, 5000)]);
    }
    host.read(ring.consumer());
    assert!(host.plan(ViewId(0)).is_some(), "view zero draws");
    assert_eq!(host.clips(ViewId(0)).len(), 1, "and has a mask");

    {
        let (producer, _) = ring.split();
        let record = ViewUndeclare { view: ViewId(0) };
        producer
            .write(EnvelopeKind::ViewUndeclare, record.as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    assert!(!host.declared(ViewId(0)));
    assert!(host.plan(ViewId(0)).is_none(), "its order is gone");
    assert!(host.clips(ViewId(0)).is_empty(), "and its masks");
    assert!(
        host.joiner().drawable(GeometryId(1), ViewId(0)).is_none(),
        "and its use of the geometry"
    );

    // The other view is untouched, which is what says the drop is per view rather than wholesale.
    assert!(host.declared(ViewId(1)));
    let frame = host.plan(ViewId(1)).expect("the other view still draws");
    assert_eq!(
        frame.batches.get(0).expect("a batch").geometries,
        [GeometryId(1)],
        "through the announcement, which is shared and was not dropped"
    );
}
