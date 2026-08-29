//! §13.3's four-view sweep, timed, with ring occupancy — the half that needs the board.
//!
//! `sweep_never_blank` asserts the sweep is correct: no holes, each tile fetched once. This
//! measures what it costs. Both halves are §13.3; only this one is meaningless off the target,
//! which is why it is `#[ignore]`d and reports rather than asserts.
//!
//! ```sh
//! cargo test -p tessella-orchestrate --test sweep_budget -- --ignored --nocapture
//! ```
//!
//! # What a frame here is
//!
//! Per view, per frame: recompute the cover through [`ViewCover`] — latch, delta, never-blank
//! draw list — then emit what a frame owes for it. Geometry is built once and shared, as §5.1
//! requires, so what is timed each frame is the *per-view* work: the cover, the clip masks, the
//! drawable matrices and the uniform writes, which §5.2 calls irreducible. That is the number a
//! tick budget is spent against.
//!
//! Byte-exactness is `whole_stream`'s job and is not re-checked here. What this asserts about
//! correctness is only that nothing overflowed and every frame drained.
//!
//! # Ring occupancy is measured against a consumer that drains once a frame
//!
//! §13.3 asks for bounded occupancy through simultaneous crossings, and "bounded" only means
//! something relative to a drain rate. A consumer that never drains fills any ring; one that
//! drains continuously empties any ring. Once per frame is the tick model of §3.2, and the peak
//! it leaves is the high-water mark a ring has to be sized for.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tessella_capture_abi::CameraMode;
use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::Ring;
use tessella_orchestrate::order::{self, DrawOrder};
use tessella_orchestrate::tile::{LayerBucket, TileId as BuildTile, build_sourceless, build_tile};
use tessella_orchestrate::ubo::{self, DrawableEntry, GlobalPaintParams};
use tessella_orchestrate::view::{GeometryBinding, ViewSession};
use tessella_orchestrate::viewcover::ViewCover;
use tessella_orchestrate::{stencil, sweep};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};
use tessella_tile::cover::ViewTransform;

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");

/// The shared tile store: built once, drawn by every view (§5.1).
struct Shared {
    style: Style,
    features: Vec<tessella_source::GeoJsonFeature>,
    built: BTreeMap<(u8, u32, u32), Vec<LayerBucket>>,
    builds: usize,
}

impl Shared {
    fn new() -> Self {
        let style = Style::parse(HERMETIC).expect("style parses");
        let Some(Source::Geojson(source)) = style.source("probe") else {
            panic!("a geojson source");
        };
        let features = geojson::read(&source.data).expect("features");
        Self {
            style,
            features,
            built: BTreeMap::new(),
            builds: 0,
        }
    }

    /// A tile's buckets, building them the first time anyone asks.
    fn tile(&mut self, z: u8, x: u32, y: u32) -> &[LayerBucket] {
        if !self.built.contains_key(&(z, x, y)) {
            let id = BuildTile::new(z, x, y);
            let mut buckets = build_tile(
                &self.style,
                "probe",
                id,
                &self.features,
                TilingOptions::default(),
            )
            .expect("tile builds");
            buckets.extend(build_sourceless(&self.style, id).expect("background builds"));
            buckets.sort_by_key(|bucket| bucket.layer_index);
            self.built.insert((z, x, y), buckets);
            self.builds += 1;
        }
        &self.built[&(z, x, y)]
    }
}

/// One view's per-frame producer work, written into the ring.
///
/// Returns the number of envelopes emitted.
fn emit_view(
    shared: &mut Shared,
    session: &mut ViewSession,
    producer: &mut tessella_capture_abi::ring::Producer,
    view_id: ViewId,
    view: &ViewTransform,
    state: &ViewCover,
    next_id: &mut u64,
) -> usize {
    let mut emitted = 0;

    let global = GlobalPaintParams::for_view(view, [64.0, 64.0], 1.0).pack();
    ubo::write(
        producer,
        view_id,
        ubo::FRAME_WIDE,
        tessella_capture_abi::generated::ubo_slots::ID_GLOBAL_PAINT_PARAMS_UBO,
        &global,
    )
    .expect("the ring takes the frame-wide block");
    emitted += 1;

    let mut order = DrawOrder::new(shared.style.layers.len() as u32);
    let mut by_layer: BTreeMap<i32, Vec<GeometryBinding>> = BTreeMap::new();
    let tiles: Vec<_> = state.tiles().to_vec();

    for tile in &tiles {
        let buckets = shared.tile(tile.z, tile.x, tile.y).to_vec();
        for binding in order::bindings_for(
            view_id,
            order::tile_of(tile.z, tile.x, tile.y),
            &buckets,
            next_id,
            true,
        ) {
            by_layer
                .entry(binding.layer_index)
                .or_default()
                .push(binding);
            session
                .use_geometry(producer, binding)
                .expect("the ring takes a use");
            order.bind(binding);
            emitted += 1;
        }
    }

    for (layer_index, bindings) in &by_layer {
        let tiled = bindings.iter().any(|binding| {
            binding
                .flags
                .contains(tessella_capture_abi::envelope::DrawFlags::ENABLE_STENCIL)
        });
        if tiled {
            let set = stencil::clip_set(view, *layer_index, &tiles).expect("clips");
            stencil::write(producer, view_id, &set).expect("the ring takes a clip set");
            emitted += 1;
        }

        // The drawable matrices: one per tile per sublayer, which is the bulk of a frame's
        // per-view arithmetic and the part that scales with the cover.
        let entries: Vec<DrawableEntry> = bindings
            .iter()
            .filter_map(|binding| binding.tile)
            .map(|tile| {
                DrawableEntry::for_tile(
                    view,
                    tile.z,
                    tile.x,
                    tile.y,
                    i32::from(tile.wrap),
                    *layer_index,
                    0,
                )
                .expect("an unrotated camera")
            })
            .collect();
        if !entries.is_empty() {
            let buffer = ubo::pack_drawable_buffer(&entries, 80);
            ubo::write(
                producer,
                view_id,
                *layer_index,
                ubo::drawable_slot(),
                &buffer,
            )
            .expect("the ring takes a drawable buffer");
            emitted += 1;
        }
    }

    emitted
}

/// Per-frame producer cost for four views over the sweep, and the ring high-water mark.
#[test]
#[ignore]
fn four_view_sweep_budget() {
    let zooms = sweep::sweep_zooms(33);
    let base = sweep::four_views();
    let mut shared = Shared::new();

    // Sized generously: what is being measured is the peak a consumer draining once a frame
    // leaves behind, not where backpressure begins. Backpressure under stall is R4.
    let mut ring = Ring::new(1 << 24);
    let (producer, consumer) = ring.split();
    let mut session = ViewSession::new();
    for index in 0..base.len() {
        session
            .declare(producer, ViewId(index as u32), CameraMode::Producer)
            .expect("declares");
    }

    let mut states: Vec<ViewCover> = base
        .iter()
        .map(|view| {
            ViewCover::new(&ViewTransform {
                zoom: zooms[0],
                ..*view
            })
            .expect("covers")
        })
        .collect();

    // Warm the tile store so the first frames are not paying to build what every later frame
    // reuses. A cold store is a cold-start measurement, which `worker_budget` reports.
    for &zoom in &zooms {
        for (view, state) in base.iter().zip(&mut states) {
            state
                .update(&ViewTransform { zoom, ..*view })
                .expect("covers");
            for tile in state.tiles().to_vec() {
                shared.tile(tile.z, tile.x, tile.y);
            }
        }
    }
    let warmed = shared.builds;

    let mut next_id = 0u64;
    let mut frame_times: Vec<Duration> = Vec::with_capacity(zooms.len());
    let mut peak_occupancy = 0usize;
    let mut peak_envelopes = 0usize;
    let mut drained = 0usize;

    for &zoom in &zooms {
        let started = Instant::now();
        let mut envelopes = 0;
        for (index, (view, state)) in base.iter().zip(&mut states).enumerate() {
            let at = ViewTransform { zoom, ..*view };
            state.update(&at).expect("covers");
            envelopes += emit_view(
                &mut shared,
                &mut session,
                producer,
                ViewId(index as u32),
                &at,
                state,
                &mut next_id,
            );
        }
        frame_times.push(started.elapsed());
        peak_occupancy = peak_occupancy.max(producer.occupancy());
        peak_envelopes = peak_envelopes.max(envelopes);

        // The consumer's tick: drain everything this frame produced.
        while let Some(record) = consumer.peek() {
            let (consumed, kind) = (record.consumed(), record.kind);
            assert!(kind as u32 > 0, "a drained record must name a kind");
            consumer.advance(consumed);
            drained += 1;
        }
    }

    frame_times.sort_unstable();
    let at = |q: f64| frame_times[((frame_times.len() - 1) as f64 * q) as usize];
    println!(
        "\n  four-view sweep, {} frames, hermetic style",
        zooms.len()
    );
    println!("    tiles built (shared)   {warmed}");
    println!("    envelopes drained      {drained}");
    println!("    frame min              {:>10.3?}", frame_times[0]);
    println!("    frame median           {:>10.3?}", at(0.5));
    println!("    frame p95              {:>10.3?}", at(0.95));
    println!(
        "    frame max              {:>10.3?}",
        frame_times[frame_times.len() - 1]
    );
    println!("    peak ring occupancy    {peak_occupancy} bytes");
    println!("    peak envelopes / frame {peak_envelopes}");
    println!();

    assert_eq!(shared.builds, warmed, "the timed pass must build nothing");
    assert!(drained > 0);
}
