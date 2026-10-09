//! Planning a frame twice allocates nothing the second time.
//!
//! Asserted by counting, because the obvious test does not test it. `Vec::clear` keeps the outer
//! capacity while dropping every element, so a `Vec<Batch>` whose batches each owned two `Vec`s
//! passed a capacity check while giving both buffers back on every clear and taking them again on
//! every refill — 15,000 allocations a frame at the quad's entry count, which is what sent the
//! representation flat.
//!
//! A counting allocator is the only thing that catches that, so it is what is here.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use tessella_capture_abi::envelope::DrawFlags;
use tessella_capture_abi::envelope::{
    CameraUpdate, GeometryAdd, GeometryId, OrderEntry, OrderEpoch, OrderUpdate, SlabRef, Span,
    TileId, ViewDeclare, ViewId, ViewUse, WireRecord,
};
use tessella_capture_abi::ring::Ring;
use tessella_capture_abi::{CameraMode, EnvelopeKind, RenderPass};
use tessella_consume::host::Host;

thread_local! {
    /// Allocations on this thread.
    ///
    /// Per thread rather than global, and that is not fussiness: cargo runs a test binary's tests
    /// in parallel, so a global counter reports the sibling test's allocations inside this one's
    /// window. Measured that way first, this read six allocations across sixteen warm plans that
    /// were all made by another thread.
    ///
    /// `const` initializer so the slot needs no lazy allocation, which inside an allocator would
    /// recurse.
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn allocations() -> u64 {
    ALLOCS.with(|count| count.get())
}

fn counted() {
    // A thread tearing down may have dropped its slot already; missing a count then is harmless
    // and panicking would not be.
    let _ = ALLOCS.try_with(|count| count.set(count.get() + 1));
}

struct Counting;

// SAFETY: every method forwards to the system allocator unchanged; the counter is the only
// addition and it does not touch the allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        counted();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        counted();
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn geometry(id: u64, shader: i32) -> GeometryAdd {
    GeometryAdd {
        geometry: GeometryId(id),
        permutation_key: 0,
        indexes: SlabRef::default(),
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

fn use_of(id: u64) -> ViewUse {
    ViewUse {
        geometry: GeometryId(id),
        view: ViewId(0),
        layer_index: 0,
        sub_layer_index: 0,
        tile: TileId::default(),
        render_pass: RenderPass::TRANSLUCENT,
        draw_flags: DrawFlags::default(),
        has_tile: 0,
        _pad: [0; 5],
    }
}

fn camera() -> CameraUpdate {
    let zeros = vec![0u8; core::mem::size_of::<CameraUpdate>()];
    let mut update = CameraUpdate::from_bytes(&zeros).expect("a camera reads from zeros");
    update.view = ViewId(0);
    update.order_epoch = OrderEpoch(1);
    update
}

/// A frame of `n` drawables, every one its own batch: the shape that allocated most.
fn frame(n: u64) -> Host {
    let mut ring = Ring::new(1 << 20);
    {
        let (producer, _) = ring.split();
        // The view first: a `ViewUse` naming one that was never declared is dropped, which the ABI
        // calls a protocol fault and `Progress::undeclared` counts.
        let declare = ViewDeclare {
            view: ViewId(0),
            camera_mode: CameraMode::Producer as u8,
            _reserved: [0; 3],
        };
        producer
            .write(EnvelopeKind::ViewDeclare, declare.as_bytes(), &[])
            .expect("room");
        for id in 0..n {
            // A different family each time, so nothing collapses and every entry is a batch.
            let shader = 11 + i32::try_from(id % 16).unwrap();
            producer
                .write(
                    EnvelopeKind::GeometryAdd,
                    geometry(id, shader).as_bytes(),
                    &[],
                )
                .expect("room");
            producer
                .write(EnvelopeKind::ViewUse, use_of(id).as_bytes(), &[])
                .expect("room");
        }
        let mut payload = Vec::new();
        for id in 0..n {
            let entry = OrderEntry {
                geometry: GeometryId(id),
                draw_priority: 0,
                layer_index: 0,
                sub_layer_index: 0,
                ubo_index: u32::try_from(id).unwrap(),
                pass: RenderPass::TRANSLUCENT,
                _pad: [0; 3],
            };
            payload.extend_from_slice(entry.as_bytes());
        }
        let update = OrderUpdate {
            order_epoch: OrderEpoch(1),
            view: ViewId(0),
            _pad: 0,
            entries: Span {
                offset: 0,
                count: u32::try_from(n).unwrap(),
            },
        };
        producer
            .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
            .expect("room");
        producer
            .write(EnvelopeKind::CameraUpdate, camera().as_bytes(), &[])
            .expect("room");
    }
    let mut host = Host::new();
    host.read(ring.consumer());
    host
}

/// Once the buffers have grown, planning the same frame again allocates nothing.
#[test]
fn planning_a_warm_frame_allocates_nothing() {
    let mut host = frame(512);

    // Warm: the first call grows every buffer, which is the cost this is not about. Scoped, so
    // the borrow on the host ends before the measured calls need it mutably -- the constraint the
    // cache trades for, visible here.
    {
        let batches = host.plan(ViewId(0)).expect("a plan").batches;
        assert_eq!(batches.len(), 512, "every entry its own batch");
    }

    let before = allocations();
    for _ in 0..16 {
        host.plan(ViewId(0)).expect("a plan");
    }
    let during = allocations() - before;

    assert_eq!(
        during, 0,
        "sixteen plans of a warm frame allocated {during} times"
    );
    let batches = host.plan(ViewId(0)).expect("a plan").batches;
    assert_eq!(
        batches.len(),
        512,
        "and produced the same batches each time"
    );
}

/// Growing is bounded: a frame's first plan does not allocate per batch.
///
/// Five buffers grow, so the count is a handful of doublings rather than anything proportional to
/// the batch count. Pinned loosely on purpose -- the exact number of doublings is an allocator
/// detail, and what matters is that it is not 512 of anything.
#[test]
fn the_first_plan_grows_a_handful_of_buffers() {
    let mut host = frame(512);

    let before = allocations();
    host.plan(ViewId(0)).expect("a plan");
    let during = allocations() - before;

    assert!(
        during < 64,
        "a cold plan of 512 batches allocated {during} times, which is per-batch rather than \
         per-buffer"
    );
}
