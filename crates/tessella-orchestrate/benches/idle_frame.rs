//! What a settled frame costs, against mbgl's answer for the same one.
//!
//! # The comparison
//!
//! `mbgl-capture-probe --bench-idle=N` renders N frames after the map has stopped changing and
//! reports the same percentiles. This does the same through `Map::tick`. Same style, same
//! camera, same machine; the question is what each costs to be asked for a frame when nothing
//! has moved.
//!
//! # What is and is not being compared
//!
//! Not the same scope, and saying so is the difference between a measurement and a boast.
//! `Renderer::render` also evaluates paint properties and assembles draw calls, which `tick`
//! does not — the consumer does that. What both *do* include is the part this is about: deciding
//! what the frame contains. mbgl recomputes the cover, walks renderables and rebuilds a retain
//! set per source; tessella asks whether anything changed.
//!
//! So the honest claim is narrow: this is the cost of *deciding there is nothing to do*, and it
//! is the number §12.10 predicts an order of magnitude on. It says nothing about steady-state
//! throughput, where the same tiles decode into the same buckets either way.
//!
//! Percentiles and a maximum rather than a mean, for the reason §13.1 gives: a frame budget is a
//! promise about the worst frame.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, region_size};
use tessella_orchestrate::map::{Map, Tiles};
use tessella_orchestrate::tile::{LayerBucket, TileId, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;

/// The style the probe is pointed at, so both sides draw the same map.
const STYLE: &str = include_str!("../../tessella-style/tests/symbol_style.json");

/// Every cover tile answers from what the style's own features built.
struct Built {
    buckets: Arc<Vec<LayerBucket>>,
}

impl Tiles for Built {
    fn buckets(&self, _tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
        Some(Arc::clone(&self.buckets))
    }
}

fn main() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let style: Style =
        serde_json::from_str(&STYLE.replace("TESSELLA", root)).expect("the style parses");
    let Some(tessella_style::Source::Geojson(source)) = style.source("probe") else {
        panic!("one geojson source")
    };
    let features = geojson::read(&source.data).expect("features read");
    let built = build_tile(
        &style,
        "probe",
        TileId::new(13, 4093, 2723),
        &features,
        TilingOptions::default(),
    )
    .expect("the tile builds");
    let tiles = Built {
        buckets: Arc::new(built),
    };

    // The probe's own camera.
    let view = camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    });

    const CAPACITY: usize = 1 << 24;
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, outlives both halves.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut map = Map::new(style, view, ViewId(0));
    // Settle it: the first tick emits, and everything after has nothing to do.
    map.tick(&mut producer, &tiles).expect("the first frame");

    let rounds = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(2000usize);

    let mut times = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        let tick = map.tick(&mut producer, &tiles).expect("a settled tick");
        times.push(started.elapsed());
        // Kept so the call cannot be optimized away, and so a tick that started emitting would
        // be visible rather than silently changing what is being measured.
        assert!(
            matches!(tick, tessella_orchestrate::map::Tick::Idle),
            "the map emitted on a settled tick: this is no longer measuring an idle frame"
        );
    }

    times.sort_unstable();
    let at = |fraction: f64| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let index = ((times.len() as f64 - 1.0) * fraction) as usize;
        times[index]
    };
    let total: Duration = times.iter().sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = total.as_secs_f64() * 1e6 / times.len() as f64;

    println!(
        "=== idle frame cost (tessella), {} settled frames ===",
        times.len()
    );
    println!("idle_p50_us {:.2}", at(0.50).as_secs_f64() * 1e6);
    println!("idle_p95_us {:.2}", at(0.95).as_secs_f64() * 1e6);
    println!("idle_p99_us {:.2}", at(0.99).as_secs_f64() * 1e6);
    println!(
        "idle_max_us {:.2}",
        times[times.len() - 1].as_secs_f64() * 1e6
    );
    println!("idle_mean_us {mean:.2}");
}
