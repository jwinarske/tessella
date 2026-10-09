//! Reading a real ring and planning what it says to draw.
//!
//! These drive the producer half directly rather than a fixture, so what is under test is the
//! whole path: bytes on a ring, dispatched, joined, paired with a camera, collapsed into batches.

use tessella_capture_abi::envelope::DrawFlags;
use tessella_capture_abi::envelope::{
    CameraUpdate, GeometryAdd, GeometryId, GeometryRemove, OrderEntry, OrderEpoch, OrderUpdate,
    Span, TileId, ViewId, ViewRelease, ViewUse, WireRecord,
};
use tessella_capture_abi::ring::{Producer, Ring};
use tessella_capture_abi::{EnvelopeKind, RenderPass};
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

/// Writes the usual shape: a geometry, a view using it, an order, then the camera committing it.
fn write_frame(producer: &mut Producer, view: u32, epoch: u64, ids: &[u64]) {
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
        producer
            .write(EnvelopeKind::ViewUse, use_of(1, 0, 0).as_bytes(), &[])
            .expect("room");
        // A kind this host does not dispatch.
        producer
            .write(EnvelopeKind::ViewDeclare, &[0u8; 16], &[])
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
    assert_eq!(progress.records, 5, "and the read did not stop at it");
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
