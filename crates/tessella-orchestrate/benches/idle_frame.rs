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
//!
//! # The sweep is the honest half
//!
//! Idle cost measures who skips better. A zoom sweep measures who does the *unavoidable* work
//! faster: the cover really has changed, so the per-frame re-derivation mbgl pays for is work
//! that had to happen this frame. If the two are close there, the idle number is a story about
//! gating rather than about the pipeline — which is worth knowing either way.

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

    sweep(&tiles, &mut producer, &mut map);
}

/// The same z8-z16-z8 sweep the probe runs, timed per frame.
///
/// Up and back down, because a cover that grows and one that shrinks are different work and the
/// expensive one is not always the same.
fn sweep(tiles: &Built, producer: &mut tessella_capture_abi::ring::Producer, map: &mut Map) {
    const STEPS: usize = 33;
    const LOW: f64 = 8.0;
    const HIGH: f64 = 16.0;

    let mut times = Vec::with_capacity(STEPS * 2);
    for direction in 0..2 {
        for step in 0..STEPS {
            #[allow(clippy::cast_precision_loss)]
            let fraction = step as f64 / (STEPS - 1) as f64;
            let at = if direction == 0 {
                LOW + (HIGH - LOW) * fraction
            } else {
                HIGH - (HIGH - LOW) * fraction
            };
            map.look_at(camera::settled(&ViewTransform {
                zoom: at,
                ..*map.view()
            }));
            let started = Instant::now();
            let tick = map.tick(producer, tiles).expect("a sweep tick");
            times.push(started.elapsed());
            std::hint::black_box(&tick);
        }
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
        "\n=== sweep frame cost (tessella), {} frames z{LOW}-z{HIGH} ===",
        times.len()
    );
    println!("sweep_p50_us {:.2}", at(0.50).as_secs_f64() * 1e6);
    println!("sweep_p95_us {:.2}", at(0.95).as_secs_f64() * 1e6);
    println!("sweep_p99_us {:.2}", at(0.99).as_secs_f64() * 1e6);
    println!(
        "sweep_max_us {:.2}",
        times[times.len() - 1].as_secs_f64() * 1e6
    );
    println!("sweep_mean_us {mean:.2}");
}
