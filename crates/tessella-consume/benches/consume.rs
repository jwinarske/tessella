//! What reading a frame and planning it costs, and how many allocations it takes.
//!
//! Two questions, and the second is the one this was written for. `plan_into` fills a buffer the
//! caller keeps, which is only worth doing if a steady state stops allocating — and `Vec::clear`
//! on a `Vec<Batch>` keeps the outer capacity while dropping every `Batch`, so each one's two
//! inner `Vec`s are freed and taken again. The counting allocator below says how often.
//!
//! Percentiles and a maximum rather than a mean, for the reason §13.1 gives: a frame budget is a
//! promise about the worst frame.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tessella_capture_abi::envelope::DrawFlags;
use tessella_capture_abi::envelope::{
    CameraUpdate, GeometryAdd, GeometryId, OrderEntry, OrderEpoch, OrderUpdate, Span, TileId,
    ViewId, ViewUse, WireRecord,
};
use tessella_capture_abi::ring::Ring;
use tessella_capture_abi::{EnvelopeKind, RenderPass};
use tessella_consume::host::Host;

/// Counts allocations, so a claim about not making any can be checked rather than asserted.
///
/// Global here because this binary is single-threaded; the equivalent in a test binary has to be
/// thread-local, since cargo runs tests in parallel and a global counter reports a sibling's work.
struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: every method forwards to the system allocator unchanged; the counters are the only
// addition and they do not touch the allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn allocations() -> u64 {
    ALLOCS.load(Ordering::Relaxed)
}

fn geometry(id: u64, shader: i32) -> GeometryAdd {
    GeometryAdd {
        geometry: GeometryId(id),
        permutation_key: 0,
        indexes: tessella_capture_abi::envelope::SlabRef::default(),
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

fn use_of(id: u64, view: u32) -> ViewUse {
    ViewUse {
        geometry: GeometryId(id),
        view: ViewId(view),
        layer_index: 0,
        sub_layer_index: 0,
        tile: TileId::default(),
        render_pass: RenderPass::TRANSLUCENT,
        draw_flags: DrawFlags::default(),
        has_tile: 0,
        _pad: [0; 5],
    }
}

fn camera(view: u32, epoch: u64) -> CameraUpdate {
    let zeros = vec![0u8; core::mem::size_of::<CameraUpdate>()];
    let mut update = CameraUpdate::from_bytes(&zeros).expect("a camera reads from zeros");
    update.view = ViewId(view);
    update.order_epoch = OrderEpoch(epoch);
    update
}

/// A frame of `entries` drawables, `families` of them so the order breaks into that many batches.
fn write(ring: &mut Ring, entries: usize, families: i32) {
    let (producer, _) = ring.split();
    for id in 0..entries as u64 {
        let shader = 11 + (id as i32 % families);
        producer
            .write(
                EnvelopeKind::GeometryAdd,
                geometry(id, shader).as_bytes(),
                &[],
            )
            .expect("room");
        producer
            .write(EnvelopeKind::ViewUse, use_of(id, 0).as_bytes(), &[])
            .expect("room");
    }
    let mut payload = Vec::new();
    for id in 0..entries as u64 {
        let entry = OrderEntry {
            geometry: GeometryId(id),
            draw_priority: 0,
            layer_index: 0,
            sub_layer_index: 0,
            ubo_index: id as u32,
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
            count: entries as u32,
        },
    };
    producer
        .write(EnvelopeKind::OrderUpdate, update.as_bytes(), &payload)
        .expect("room");
    producer
        .write(EnvelopeKind::CameraUpdate, camera(0, 1).as_bytes(), &[])
        .expect("room");
}

fn percentiles(mut samples: Vec<Duration>) -> (Duration, Duration, Duration) {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    (at(0.5), at(0.95), samples[samples.len() - 1])
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn main() {
    // Sized from the frames the plan documents: one view of liberty, and the four-view quad.
    for (name, entries, families) in [
        ("one view, 1 family", 1_882, 1),
        ("one view, 16 families", 1_882, 16),
        ("quad, 16 families", 7_500, 16),
    ] {
        let capacity = (entries * 256usize).next_power_of_two();
        let mut ring = Ring::new(capacity);
        let mut host = Host::new();
        write(&mut ring, entries, families);

        let before = allocations();
        let started = Instant::now();
        host.read(ring.consumer());
        let read = started.elapsed();
        let read_allocs = allocations() - before;

        // Warm: the first call plans, which is not what the cached figure is about.
        let produced = host.plan(ViewId(0)).expect("a plan").1.len();

        // Cached: the frame has not changed, so this is what a still map pays.
        let mut cached = Vec::with_capacity(200);
        let before = allocations();
        for _ in 0..200 {
            let started = Instant::now();
            host.plan(ViewId(0)).expect("a plan");
            cached.push(started.elapsed());
        }
        let plan_allocs = (allocations() - before) / 200;

        // Stale: marked every time, so this is the cost of actually planning.
        let mut fresh = Vec::with_capacity(200);
        for _ in 0..200 {
            host.invalidate(ViewId(0));
            let started = Instant::now();
            host.plan(ViewId(0)).expect("a plan");
            fresh.push(started.elapsed());
        }

        let (c50, _, _) = percentiles(cached);
        let (p50, p95, max) = percentiles(fresh);
        println!("{name}: {entries} entries -> {produced} batches");
        println!(
            "  read       {:8.1} us   {read_allocs:6} allocations",
            micros(read)
        );
        println!(
            "  plan       {:8.1} us p50  {:8.1} p95  {:8.1} max   {plan_allocs:6} allocations/call",
            micros(p50),
            micros(p95),
            micros(max)
        );
        println!("  cached     {:8.3} us p50", micros(c50));
    }
}
