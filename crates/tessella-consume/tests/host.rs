//! Reading a real ring and planning what it says to draw.
//!
//! These drive the producer half directly rather than a fixture, so what is under test is the
//! whole path: bytes on a ring, dispatched, joined, paired with a camera, collapsed into batches.

use tessella_capture_abi::envelope::DrawFlags;
use tessella_capture_abi::envelope::{
    AddReason, CameraUpdate, Extent, GeometryAdd, GeometryId, GeometryRemove, MeshAdd, MeshFormat,
    OrderEntry, OrderEpoch, OrderUpdate, SlabRef, Span, StencilTile, StencilTiles, TextureId,
    TileId, ViewDeclare, ViewId, ViewRelease, ViewTarget, ViewUndeclare, ViewUse, WireRecord,
};
use tessella_capture_abi::generated::mbgl_enums::BuiltIn;
use tessella_capture_abi::ring::{Producer, Ring};
use tessella_capture_abi::{
    CameraMode, EnvelopeKind, ProjectionMode, RenderPass, TextureChannelDataType, TexturePixelType,
};
use tessella_consume::batch::AUTHORED_MESH;
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

/// An authored mesh, as the producer announces one: an id in the geometry space and a slab.
fn mesh(id: u64, format: u8) -> MeshAdd {
    MeshAdd {
        mesh: GeometryId(id),
        bytes: SlabRef {
            slab: 7,
            offset: 128,
            length: 4096,
        },
        format,
        reason: AddReason::Created as u8,
        _pad: [0; 2],
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
            // A style of four layers. Every slot a fixture here asks for is measured against
            // this, and zero would be a producer that does not send one.
            layer_count: 4,
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

/// Makes a declared view draw into a texture, sized against its parent.
fn target(producer: &mut Producer, view: u32, parent: u32, num: u16, den: u16) {
    let record = ViewTarget {
        view: ViewId(view),
        parent: ViewId(parent),
        texture: TextureId(70),
        scale_num: num,
        scale_den: den,
        format: TexturePixelType::RGBA as u8,
        channel_type: TextureChannelDataType::HalfFloat as u8,
        _pad: [0; 2],
    };
    producer
        .write(EnvelopeKind::ViewTarget, record.as_bytes(), &[])
        .expect("room");
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

/// A raised drawable and a flat one of the same layer are two batches, read off a real ring.
///
/// `draw_flags` is the only thing that differs: one layer, one family, one permutation, no
/// textures, adjacent in the order. The flag lives on the `ViewUse` -- the same geometry drawn by
/// two views can be raised in one and flat in the other -- so this is the only place that join can
/// be checked. `tests/batch.rs` checks the key; what it cannot check is whether anything fills the
/// field in, and a plan that passed a constant there collapsed the two and passed every test.
///
/// Two tiles of one layer differing this way is the ordinary transient state of a terrain style,
/// not a contrived one: `ON_TERRAIN` is set where a ground covers the drawable's tile, and the DEM
/// arrives on its own schedule.
#[test]
fn a_raised_drawable_does_not_merge_with_a_flat_one() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        declare(producer, 0);
        for id in [1u64, 2] {
            producer
                .write(EnvelopeKind::GeometryAdd, geometry(id, 11).as_bytes(), &[])
                .expect("room");
            let mut use_ = use_of(id, 0, 0);
            if id == 2 {
                use_.draw_flags = DrawFlags::ON_TERRAIN;
            }
            producer
                .write(EnvelopeKind::ViewUse, use_.as_bytes(), &[])
                .expect("room");
        }
        let entries = [entry(1, 0, 0), entry(2, 0, 1)];
        let (update, payload) = order(0, 1, &entries);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 1).as_bytes(), &[])
            .expect("room");
    }
    let progress = host.read(ring.consumer());
    assert_eq!(progress.malformed, 0, "every record parses");

    let frame = host.plan(ViewId(0)).expect("a plan");
    assert_eq!(
        frame.batches.len(),
        2,
        "one is raised and one is not, so they are two programs"
    );
    let flags: Vec<u8> = frame
        .batches
        .iter()
        .map(|batch| batch.key.draw_flags)
        .collect();
    assert_eq!(
        flags,
        vec![0, DrawFlags::ON_TERRAIN.bits()],
        "the flags reached the key in the order the drawables were used"
    );

    // The control: the same frame with both flat is one batch, so what split them is the
    // difference and not the second drawable.
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        write_frame(producer, 0, 1, &[1, 2]);
    }
    host.read(ring.consumer());
    let frame = host.plan(ViewId(0)).expect("a plan");
    assert_eq!(
        frame.batches.len(),
        1,
        "two flat drawables are one renderable"
    );
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

/// A frame carries the camera its plan was gated on, and the host carries one besides.
///
/// The gate means the two agree whenever a frame exists, so what this pins is that the frame's
/// camera is whole and is that one -- not the epoch alone, which is all `host` kept of a camera
/// until it kept the record. `depth_range_size` and `opaque_pass_cutoff` arrive nowhere else, so
/// a consumer that cannot reach them cannot honor the opaque/translucent split at all.
#[test]
fn a_frame_draws_under_the_camera_it_was_gated_on() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        declare(producer, 0);
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
        let mut sent = camera(0, 1);
        sent.depth_range_size = 1.5;
        sent.opaque_pass_cutoff = 3;
        sent.pixels_per_meter = 2.25;
        producer
            .write(EnvelopeKind::CameraUpdate, sent.as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    let frame = host.plan(ViewId(0)).expect("a frame");
    assert_eq!(
        frame.camera.order_epoch, frame.plan.epoch,
        "the frame's camera is the one its plan was gated on"
    );
    assert_eq!(frame.camera.depth_range_size, 1.5);
    assert_eq!(frame.camera.opaque_pass_cutoff, 3);
    assert_eq!(
        frame.camera.pixels_per_meter, 2.25,
        "which a consumer owning the camera cannot get from the projection"
    );
    assert_eq!(frame.camera.projection(), Some(ProjectionMode::Mercator));

    // The other question: a camera ahead of its order commits no frame, and is still a camera the
    // view has. `Host::camera` answers that; `Frame::camera` cannot, there being no frame.
    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 9).as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());
    assert!(host.plan(ViewId(0)).is_none(), "the epochs disagree");
    assert_eq!(
        host.camera(ViewId(0)).map(|held| held.order_epoch),
        Some(OrderEpoch(9)),
        "and the camera is held, which is what the gate means"
    );
    assert!(host.camera(ViewId(1)).is_none(), "a view with no camera");
}

/// A camera naming a projection this build does not know is refused.
///
/// The field says to: an unknown value "must refuse the camera rather than fall back to the
/// plane", because falling back draws a flat map where a round one was asked for and looks like a
/// bug in the style. Refused whole, so the view keeps the last camera it could read -- a map
/// missing the frames a newer producer names comes out stale rather than blank.
#[test]
fn a_camera_with_an_unknown_projection_is_refused() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        declare(producer, 0);
        let (update, payload) = order(0, 1, &[]);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 1).as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());
    assert_eq!(host.progress().unsupported, 0, "nothing refused yet");
    assert_eq!(host.progress().malformed, 0);
    assert_eq!(
        host.plan(ViewId(0)).expect("a frame").plan.epoch,
        OrderEpoch(1)
    );

    // A newer producer's projection, on an otherwise good camera.
    {
        let (producer, _) = ring.split();
        let mut newer = camera(0, 1);
        newer.projection = 2;
        assert_eq!(newer.projection(), None, "not a mode this build knows");
        producer
            .write(EnvelopeKind::CameraUpdate, newer.as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    let progress = host.progress();
    assert_eq!(
        progress.unsupported, 1,
        "the camera was refused -- counted as tessella#364 settled for this wire's other \
         discriminants, and as `ViewDeclare`'s camera_mode is"
    );
    assert_eq!(
        progress.malformed, 0,
        "and not as malformed: these bytes parse, and a producer one revision ahead is not a \
         corrupt stream"
    );
    assert_eq!(progress.unknown, 0, "not as unknown: the kind is known");
    assert_eq!(
        host.camera(ViewId(0)).map(|held| held.projection),
        Some(ProjectionMode::Mercator as u8),
        "the refused camera did not replace the one the view had"
    );
    assert_eq!(
        host.plan(ViewId(0)).expect("still a frame").plan.epoch,
        OrderEpoch(1),
        "so the view draws stale rather than blank"
    );
}

/// A frame places a layer in the depth range, and refuses to when the producer did not say.
///
/// `opaque_pass_cutoff` is a threshold on the depth slot, and the slot is measured against the
/// style's layer count -- which did not cross the wire at all until `OrderUpdate::layer_count`
/// (#373). The count cannot come from the order: a style layer contributing no drawable this
/// frame is absent from the entries, so counting them undercounts and shifts every slot.
#[test]
fn a_frame_places_a_layer_in_the_depth_range() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        declare(producer, 0);
        let (update, payload) = order(0, 1, &[]);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        let mut sent = camera(0, 1);
        // Two of the four layers are opaque, so the cutoff is the slot above the deeper one.
        sent.opaque_pass_cutoff = 3;
        producer
            .write(EnvelopeKind::CameraUpdate, sent.as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    let frame = host.plan(ViewId(0)).expect("a frame");
    // Four layers, so the style's first is slot 3 and its last is slot 0 -- opposite the index,
    // which is what the cutoff is compared against.
    assert_eq!(frame.depth_slot(0), Some(3));
    assert_eq!(frame.depth_slot(3), Some(0));
    assert_eq!(
        frame.depth_slot(99),
        Some(0),
        "an index past the style saturates rather than wrapping"
    );
    // `if (currentLayer < opaquePassCutoff)`, which is mbgl's own comparison.
    assert_eq!(frame.in_opaque_pass(1), Some(true), "slot 2 is under 3");
    assert_eq!(frame.in_opaque_pass(0), Some(false), "slot 3 is not");

    // A producer older than the field sends zero, and then there is nothing to measure against.
    {
        let (producer, _) = ring.split();
        let (mut update, payload) = order(0, 2, &[]);
        update.layer_count = 0;
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 2).as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    let frame = host.plan(ViewId(0)).expect("a frame at the new epoch");
    assert_eq!(
        frame.layer_count, None,
        "zero is the producer that does not send one"
    );
    assert_eq!(
        frame.depth_slot(0),
        None,
        "and a slot measured against nothing would cut the passes arbitrarily"
    );
    assert_eq!(
        frame.in_opaque_pass(0),
        None,
        "so neither is the pass known"
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

/// Every kind the ABI defines is dispatched, so nothing is counted unknown.
///
/// This case used to be the other way round: `MeshAdd` was the one kind the host did not read, and
/// the assertion was that an undispatched kind is counted and survives. Reading the mesh envelope
/// leaves the set empty, and what is worth pinning is that it *is* empty -- a kind added to the
/// ABI and not dispatched here would make this fail, which is the reminder the old case was.
///
/// `Progress::unknown` does not cover the case its name suggests. A kind a newer producer sends
/// and this build predates never reaches the host: `Consumer::peek` refuses the record and the
/// read stops there. tessella#423.
#[test]
fn every_kind_the_abi_defines_is_dispatched() {
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
        producer
            .write(
                EnvelopeKind::MeshAdd,
                mesh(2, MeshFormat::Glb as u8).as_bytes(),
                &[],
            )
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

    assert_eq!(progress.unknown, 0, "a kind here was not dispatched");
    assert_eq!(progress.malformed, 0);
    assert_eq!(progress.unsupported, 0);
    assert_eq!(progress.records, 6, "and the read did not stop at any");
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

/// A view that owns its camera is handed one whole, matrices and all.
///
/// `CameraMode::Consumer` means the consumer places its own tiles: §11.7's "a tile's placement
/// comes from its own id and every per-drawable matrix is advisory". It cannot do that from the
/// mode alone, and tessella#371 recorded what it was missing -- no matrices, no `center_zoom0`,
/// no bearing, no pitch, and no `pixels_per_meter`, which the field's own doc calls unobtainable
/// from the projection: "Leave it out and buildings come out too tall by its reciprocal."
///
/// Each of the five is given a value nothing else in the record shares, so a camera stitched
/// together from defaults fails rather than passes by coincidence.
#[test]
fn a_consumer_owned_camera_arrives_whole() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        let record = ViewDeclare {
            view: ViewId(0),
            camera_mode: CameraMode::Consumer as u8,
            _reserved: [0; 3],
        };
        producer
            .write(EnvelopeKind::ViewDeclare, record.as_bytes(), &[])
            .expect("room");
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
        let mut sent = camera(0, 1);
        sent.proj_matrix[0] = 3.5;
        sent.proj_matrix[15] = 4.5;
        sent.globe_matrix[0] = 5.5;
        sent.center_zoom0 = [6.5, 7.5];
        sent.bearing = 8.5;
        sent.pitch = 9.5;
        sent.pixels_per_meter = 10.5;
        producer
            .write(EnvelopeKind::CameraUpdate, sent.as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    assert_eq!(
        host.camera_mode(ViewId(0)),
        Some(CameraMode::Consumer),
        "the view owns its camera"
    );
    let frame = host.plan(ViewId(0)).expect("a frame");
    assert_eq!(frame.camera.proj_matrix[0], 3.5);
    assert_eq!(
        frame.camera.proj_matrix[15], 4.5,
        "the whole matrix, not its first cell"
    );
    assert_eq!(frame.camera.globe_matrix[0], 5.5);
    assert_eq!(frame.camera.center_zoom0, [6.5, 7.5]);
    assert_eq!(frame.camera.bearing, 8.5);
    assert_eq!(frame.camera.pitch, 9.5);
    assert_eq!(frame.camera.pixels_per_meter, 10.5);
}

/// A camera mode this build does not know is refused rather than defaulted.
///
/// The mode decides what a per-drawable matrix *means*, so guessing it is how geometry gets placed
/// through a camera that is not the one drawing. Refused the same way an unknown texture format is,
/// which tessella#364 settled for the other two discriminants on this wire -- and counted apart
/// from a malformed record, because the bytes parse.
#[test]
fn an_unknown_camera_mode_is_unsupported() {
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
    assert_eq!(progress.unsupported, 1);
    assert_eq!(progress.malformed, 0, "the bytes parse");
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

/// One view's target, and a frame knows to draw it first.
///
/// DR-25: a heatmap draws its kernels into a half-resolution target and then draws that target
/// through a color ramp. Two of the eighteen families are that shape, so a consumer with no
/// `ViewTarget` cannot draw them at all.
#[test]
fn an_offscreen_view_names_what_it_draws_into() {
    let mut ring = Ring::new(CAPACITY);
    {
        let (producer, _) = ring.split();
        declare(producer, 0);
        declare(producer, 1);
        target(producer, 1, 0, 1, 2);
    }
    let mut host = Host::new();
    let progress = host.read(ring.consumer());
    assert_eq!(progress.malformed, 0);
    assert_eq!(progress.undeclared, 0);

    let found = host.target(ViewId(1)).copied().expect("a target");
    assert_eq!(found.parent, ViewId(0));
    assert_eq!(found.texture, TextureId(70));
    // A heatmap target is `RGBA` and `HalfFloat` together, which is the pair the ABI calls out:
    // the kernel sum runs past one and eight bits clips it to a flat cap.
    assert_eq!(found.format, TexturePixelType::RGBA);
    assert_eq!(found.channel, TextureChannelDataType::HalfFloat);

    // Half of the parent, rounded *up*: a target one pixel short of its parent samples its own edge
    // where the parent's last column reads.
    assert_eq!(
        found.size(Extent {
            width: 64,
            height: 64
        }),
        Extent {
            width: 32,
            height: 32
        }
    );
    assert_eq!(
        found.size(Extent {
            width: 65,
            height: 1
        }),
        Extent {
            width: 33,
            height: 1
        },
        "an odd parent still gets a target that covers it"
    );

    // And the parent knows what to draw before itself.
    assert_eq!(host.feeding(ViewId(0)).collect::<Vec<_>>(), [ViewId(1)]);
    assert_eq!(
        host.feeding(ViewId(1)).count(),
        0,
        "the offscreen view feeds nothing, because nesting is refused"
    );
    assert!(
        host.target(ViewId(0)).is_none(),
        "the parent is on the screen"
    );
}

/// A target naming a view that was never declared is the fault a use would be.
#[test]
fn a_target_of_an_undeclared_view_is_dropped() {
    for (view, parent) in [(1, 0), (0, 1)] {
        let mut ring = Ring::new(CAPACITY);
        {
            let (producer, _) = ring.split();
            // Only one of the two is declared, whichever way round.
            declare(producer, 0);
            target(producer, view, parent, 1, 2);
        }
        let mut host = Host::new();
        let progress = host.read(ring.consumer());
        assert_eq!(
            progress.undeclared, 1,
            "view {view} parent {parent}: one of them is not declared"
        );
        assert!(host.target(ViewId(view)).is_none());
    }
}

/// A target whose format this build does not know is refused, and counted apart.
///
/// The fourth field on this wire with a discriminant, and the one with the least excuse for a
/// guess: the format decides what image the consumer creates for the pass to draw into, and a
/// guess is a render target the second pass then samples. Refused like the other three, and like
/// them counted as unsupported rather than malformed -- the record parses, and a pixel type this
/// build has no case for is a producer one revision ahead.
#[test]
fn a_target_whose_format_is_unknown_is_unsupported() {
    for (format, channel) in [
        (200, TextureChannelDataType::HalfFloat as u8),
        (TexturePixelType::RGBA as u8, 7),
    ] {
        let mut ring = Ring::new(CAPACITY);
        let mut host = Host::new();
        {
            let (producer, _) = ring.split();
            declare(producer, 0);
            declare(producer, 1);
            let record = ViewTarget {
                view: ViewId(1),
                parent: ViewId(0),
                texture: TextureId(70),
                scale_num: 1,
                scale_den: 2,
                format,
                channel_type: channel,
                _pad: [0; 2],
            };
            producer
                .write(EnvelopeKind::ViewTarget, record.as_bytes(), &[])
                .expect("room");
        }
        let progress = host.read(ring.consumer());
        assert_eq!(
            progress.unsupported, 1,
            "format {format}, channel {channel}: the target was not refused"
        );
        assert_eq!(progress.malformed, 0, "the record parses");
        assert!(
            host.target(ViewId(1)).is_none(),
            "a refused target was kept anyway"
        );
    }
}

/// The three shapes of target a consumer refuses.
///
/// A denominator of zero is a protocol fault the ABI names. A view that is its own parent is a
/// frame that draws before itself. And a target whose parent is *itself* offscreen is what
/// `ViewError::NestedTarget` refuses on the producer's side -- refused here too, because that is
/// what makes `feeding` a flat set and a frame's first pass recursion-free.
#[test]
fn a_nested_or_degenerate_target_is_malformed() {
    // A denominator of zero.
    let mut ring = Ring::new(CAPACITY);
    {
        let (producer, _) = ring.split();
        declare(producer, 0);
        declare(producer, 1);
        target(producer, 1, 0, 1, 0);
    }
    let mut host = Host::new();
    assert_eq!(
        host.read(ring.consumer()).malformed,
        1,
        "a zero denominator"
    );
    assert!(host.target(ViewId(1)).is_none());

    // Its own parent.
    let mut ring = Ring::new(CAPACITY);
    {
        let (producer, _) = ring.split();
        declare(producer, 2);
        target(producer, 2, 2, 1, 2);
    }
    let mut host = Host::new();
    assert_eq!(host.read(ring.consumer()).malformed, 1, "its own parent");
    assert!(host.target(ViewId(2)).is_none());

    // A parent that is itself a target.
    let mut ring = Ring::new(CAPACITY);
    {
        let (producer, _) = ring.split();
        for view in 0..3 {
            declare(producer, view);
        }
        target(producer, 1, 0, 1, 2);
        target(producer, 2, 1, 1, 2);
    }
    let mut host = Host::new();
    assert_eq!(host.read(ring.consumer()).malformed, 1, "a nested target");
    assert!(host.target(ViewId(1)).is_some(), "the first one stands");
    assert!(host.target(ViewId(2)).is_none(), "the nested one does not");
}

/// Undeclaring a parent takes the views that fed it, wholly.
///
/// An offscreen view exists to be sampled by its parent, so one whose parent is gone is a pass
/// nothing will ever read. Dropping only its target would leave it declared and drawing, onto a
/// screen it was never meant to reach.
#[test]
fn undeclaring_a_parent_takes_what_fed_it() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        declare(producer, 0);
        declare(producer, 1);
        target(producer, 1, 0, 1, 2);
        write_frame(producer, 1, 1, &[1]);
    }
    host.read(ring.consumer());
    assert!(host.declared(ViewId(1)));
    assert!(host.plan(ViewId(1)).is_some(), "the offscreen view draws");

    {
        let (producer, _) = ring.split();
        let record = ViewUndeclare { view: ViewId(0) };
        producer
            .write(EnvelopeKind::ViewUndeclare, record.as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());

    assert!(!host.declared(ViewId(0)), "the parent is gone");
    assert!(!host.declared(ViewId(1)), "and so is what fed it");
    assert!(host.target(ViewId(1)).is_none());
    assert!(host.plan(ViewId(1)).is_none(), "it draws nothing now");
    assert_eq!(host.feeding(ViewId(0)).count(), 0);
}

/// Two views' batches are held at once, which `Frame` alone cannot do.
///
/// DR-25's offscreen views are why: a backend records the child's pass and the parent's in one
/// submission, so it needs both views' batches alive together. `Host::plan` takes `&mut self` and
/// hands back references into the host, so the second call is refused while the first's `Frame`
/// lives -- planning each in turn and reading them back is the shape that works.
#[test]
fn two_views_batches_are_held_at_once() {
    let mut ring = Ring::new(CAPACITY);
    {
        let (producer, _) = ring.split();
        write_frame(producer, 0, 1, &[1]);
        write_frame(producer, 1, 1, &[2]);
    }
    let mut host = Host::new();
    host.read(ring.consumer());

    // Planned in turn, each `Frame` dropped before the next.
    for view in 0..2 {
        assert!(
            host.plan(ViewId(view)).is_some(),
            "view {view} has an order and a camera"
        );
    }

    let first = host.batches(ViewId(0)).expect("view zero was planned");
    let second = host.batches(ViewId(1)).expect("view one was planned");
    assert_eq!(
        first.get(0).expect("a batch").geometries,
        [GeometryId(1)],
        "and both are readable together"
    );
    assert_eq!(second.get(0).expect("a batch").geometries, [GeometryId(2)]);

    assert!(
        host.batches(ViewId(9)).is_none(),
        "a view never planned has none"
    );
}

/// A mesh is drawn from the order like any other layer, and says it is a mesh.
///
/// An authored model is not a `GeometryAdd`: there is no mbgl family for a PBR mesh and nothing
/// for the producer to describe but where the bytes are. What it is, on this side, is an id in the
/// geometry space with a `ViewUse` binding it and an order entry placing it -- so a batch key
/// carries [`AUTHORED_MESH`] where a family would be, and the asset is looked up by id.
///
/// The slab reference is what a loader reads from, so it is read back field by field: a mesh whose
/// bytes a consumer cannot find is a model layer that draws nothing, and the id alone would not
/// say so.
#[test]
fn a_mesh_is_planned_as_an_authored_batch() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        declare(producer, 0);
        producer
            .write(
                EnvelopeKind::MeshAdd,
                mesh(9, MeshFormat::Glb as u8).as_bytes(),
                &[],
            )
            .expect("room");
        producer
            .write(EnvelopeKind::ViewUse, use_of(9, 0, 2).as_bytes(), &[])
            .expect("room");
        let (update, payload) = order(0, 1, &[entry(9, 2, 0)]);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 1).as_bytes(), &[])
            .expect("room");
    }
    let progress = host.read(ring.consumer());
    assert_eq!(progress.unsupported, 0, "glTF is a format this build knows");

    let frame = host.plan(ViewId(0)).expect("a frame");
    let batches: Vec<_> = frame.batches.iter().collect();
    assert_eq!(batches.len(), 1, "one entry, one batch");
    assert_eq!(
        batches[0].key.builtin_shader, AUTHORED_MESH,
        "a mesh batch has no mbgl family"
    );
    // And the value cannot be read as one, whatever it is: a sentinel that collided with a family
    // would have the consumer bind that family's pipeline to an authored asset.
    assert_eq!(
        BuiltIn::from_repr(batches[0].key.builtin_shader),
        None,
        "the mesh sentinel names an mbgl family"
    );
    assert_eq!(batches[0].key.layer_index, 2, "and the order's layer");
    assert_eq!(batches[0].geometries, &[GeometryId(9)]);

    let held = frame.joiner.mesh(GeometryId(9)).expect("the mesh is held");
    assert_eq!(held.add.format, MeshFormat::Glb as u8);
    assert_eq!(held.add.bytes.slab, 7);
    assert_eq!(held.add.bytes.offset, 128);
    assert_eq!(held.add.bytes.length, 4096);
    assert_eq!(
        frame.joiner.drawable(GeometryId(9), ViewId(0)),
        None,
        "and it is not geometry, however the order names it"
    );
}

/// A use that arrives before its mesh is kept, and draws when the asset lands.
///
/// A use is durable -- the producer sends one when a drawable enters a view's cover and not again
/// while it stays -- and a model tile's bytes are hundreds of kilobytes, so the asset is the half
/// that arrives late. Dropping the use would leave the model undrawn until the camera moved.
#[test]
fn a_use_before_the_mesh_still_draws() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        declare(producer, 0);
        producer
            .write(EnvelopeKind::ViewUse, use_of(9, 0, 2).as_bytes(), &[])
            .expect("room");
        let (update, payload) = order(0, 1, &[entry(9, 2, 0)]);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 1).as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());
    assert_eq!(
        host.plan(ViewId(0))
            .expect("a frame")
            .batches
            .iter()
            .count(),
        0,
        "the order names an asset that has not arrived"
    );

    {
        let (producer, _) = ring.split();
        producer
            .write(
                EnvelopeKind::MeshAdd,
                mesh(9, MeshFormat::Glb as u8).as_bytes(),
                &[],
            )
            .expect("room");
    }
    host.read(ring.consumer());

    let frame = host.plan(ViewId(0)).expect("a frame");
    let batches: Vec<_> = frame.batches.iter().collect();
    assert_eq!(batches.len(), 1, "the asset landed and the use was waiting");
    assert_eq!(batches[0].key.builtin_shader, AUTHORED_MESH);
}

/// A mesh whose format this build does not know is refused, and nothing is drawn from it.
///
/// The ABI says it of this field: a consumer "must skip the mesh rather than guess at the bytes".
/// Guessing is worse than not drawing -- a glTF parser pointed at something else reads a header
/// that is not one -- so the record is refused whole and the order entry naming it plans nothing.
///
/// Counted `unsupported` rather than `malformed`: the bytes parse, and a producer one revision
/// ahead is not a corrupt stream.
#[test]
fn a_mesh_in_an_unknown_format_is_unsupported() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        declare(producer, 0);
        // A format no `MeshFormat` names, on an otherwise good record.
        let newer = mesh(9, 7);
        assert_eq!(MeshFormat::from_repr(newer.format), None, "not a known one");
        producer
            .write(EnvelopeKind::MeshAdd, newer.as_bytes(), &[])
            .expect("room");
        producer
            .write(EnvelopeKind::ViewUse, use_of(9, 0, 2).as_bytes(), &[])
            .expect("room");
        let (update, payload) = order(0, 1, &[entry(9, 2, 0)]);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 1).as_bytes(), &[])
            .expect("room");
    }
    let progress = host.read(ring.consumer());

    assert_eq!(progress.unsupported, 1);
    assert_eq!(progress.malformed, 0, "the bytes parse");
    let frame = host.plan(ViewId(0)).expect("a frame");
    assert_eq!(
        frame.batches.iter().count(),
        0,
        "a refused mesh is not stored, so nothing plans it"
    );
    assert!(frame.joiner.mesh(GeometryId(9)).is_none());
}

/// One removal record drops a mesh, as it drops geometry.
///
/// The ids are one space, which is the ABI's own choice: `ViewUse`, `ViewRelease` and
/// `GeometryRemove` bind, release and drop a mesh with no record of their own. A `GeometryRemove`
/// that reached only the geometry map would leave the asset held and drawn after the producer said
/// it was gone, with the slab it points into already handed back.
#[test]
fn a_removal_drops_a_mesh_too() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        declare(producer, 0);
        producer
            .write(
                EnvelopeKind::MeshAdd,
                mesh(9, MeshFormat::Glb as u8).as_bytes(),
                &[],
            )
            .expect("room");
        producer
            .write(EnvelopeKind::ViewUse, use_of(9, 0, 2).as_bytes(), &[])
            .expect("room");
        let (update, payload) = order(0, 1, &[entry(9, 2, 0)]);
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        producer
            .write(EnvelopeKind::CameraUpdate, camera(0, 1).as_bytes(), &[])
            .expect("room");
    }
    host.read(ring.consumer());
    assert_eq!(
        host.plan(ViewId(0))
            .expect("a frame")
            .batches
            .iter()
            .count(),
        1,
        "drawn before the removal"
    );

    {
        let (producer, _) = ring.split();
        producer
            .write(
                EnvelopeKind::GeometryRemove,
                GeometryRemove {
                    geometry: GeometryId(9),
                }
                .as_bytes(),
                &[],
            )
            .expect("room");
    }
    host.read(ring.consumer());

    let frame = host.plan(ViewId(0)).expect("a frame");
    assert!(
        frame.joiner.mesh(GeometryId(9)).is_none(),
        "the asset is still held after its removal"
    );
    assert_eq!(
        frame.batches.iter().count(),
        0,
        "and the order still names it, so this is what stops it being drawn"
    );
}

/// A kind this build predates is counted and skipped, and what follows it still draws.
///
/// The record is written by hand because the point is a discriminant no `EnvelopeKind` names, and
/// `Producer::write` takes one. Its framing is a producer's: a header whose lengths agree with the
/// bytes, at the offset a producer at head zero would use.
///
/// `peek` used to resolve the kind itself and return `None` for one it did not know, which is
/// also how the ring says *nothing to read* -- so a consumer one revision behind its producer did
/// not degrade, it stopped, at that record, with `Progress::unknown` never moved. The frame behind
/// the forged record is what says the stall is gone; the counter alone would not. tessella#423.
#[test]
fn a_kind_this_build_predates_is_counted_and_skipped() {
    use core::sync::atomic::Ordering;
    use tessella_capture_abi::ring::{RecordHeader, RingControl, record_size, region_size};

    const NEWER: u16 = 4242;
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, outlives both halves.
    let (producer, consumer) =
        unsafe { tessella_capture_abi::ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };
    let mut producer = producer;
    let mut consumer = consumer;

    let forged = [3u8; 8];
    let total = record_size(forged.len(), 0);
    let header = RecordHeader {
        kind: NEWER,
        flags: 0,
        record_len: u32::try_from(forged.len()).unwrap(),
        payload_len: 0,
        total_len: u32::try_from(total).unwrap(),
    };
    // SAFETY: the region is `region_size(CAPACITY)` bytes and nothing else writes it yet. The
    // header and its record go in the first `total` bytes of the data region, which is where a
    // producer at head zero puts them, and the head is published after they are written.
    unsafe {
        let base = region.as_mut_ptr().cast::<u8>();
        let data = base.add(core::mem::size_of::<RingControl>());
        core::ptr::copy_nonoverlapping(
            (&raw const header).cast::<u8>(),
            data,
            core::mem::size_of::<RecordHeader>(),
        );
        core::ptr::copy_nonoverlapping(
            forged.as_ptr(),
            data.add(core::mem::size_of::<RecordHeader>()),
            forged.len(),
        );
        (*base.cast::<RingControl>())
            .head
            .store(total as u64, Ordering::Release);
    }

    // And an ordinary frame after it, written by the producer, which picks up where the forged
    // record left the head.
    declare(&mut producer, 0);
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
    producer
        .write(EnvelopeKind::CameraUpdate, camera(0, 1).as_bytes(), &[])
        .expect("room");

    let mut host = Host::new();
    let progress = host.read(&mut consumer);

    assert_eq!(progress.unknown, 1, "the kind this build predates");
    assert_eq!(progress.malformed, 0, "its framing is sound");
    assert_eq!(progress.records, 6, "and the read did not stop at it");
    assert!(
        host.plan(ViewId(0)).is_some(),
        "the frame behind the unknown record draws, which is what the stall cost"
    );
}
