//! Where bucket-build time goes, and how much of it is expression evaluation.
//!
//! Run with `cargo bench -p tessella-orchestrate --bench expression_cost`.
//!
//! # Why this exists before the bytecode VM
//!
//! §12.1 calls expression evaluation "the largest pure-CPU line item after tessellation" and
//! DR-11 schedules a bytecode VM for it. Both are claims about mbgl, made before this port
//! existed. Building the VM without checking whether they hold *here* would be optimising a
//! number nobody has looked at — and if evaluation turns out to be a few percent of build, the
//! VM is a large piece of machinery bought for nothing.
//!
//! So this measures the same tile built twice — with every paint property data-driven, and with
//! every one a constant — over a layer that actually has features. The difference is what an
//! infinitely fast evaluator could win. It then times single expressions per feature, so the
//! cost has a shape rather than a total.
//!
//! # What it found
//!
//! On a real zoom-14 tile: decode 455 us, build 1.12 ms with every paint property data-driven
//! against 711 us with them constant. So the data-driven surcharge is about a third of the
//! build, and the build is about two and a half times the decode.
//!
//! Those proportions are the second set. The first were taken against
//! `real-world-0-0-0.mvt` — a zoom-0 view of the whole world, 17 202 features with 17 153 of
//! them in one dense `admin` layer — where the same measurement said the surcharge was three
//! quarters of the build. Both tiles are valid; only one is shaped like the thing being
//! optimised, and the difference is a factor of two in what the numbers recommend. The world
//! tile is still decoded here, beside the real one, so the gap stays visible.
//!
//! The per-expression numbers say where the cost is, and the first two lines are there so it is
//! attributable rather than inferred.
//!
//! `Feature::property` called directly — the same dyn-dispatched call `["get", k]` makes, with
//! the same scan and the same owned `Value` built — is a few nanoseconds per feature. The data
//! access is not the cost. A bare literal number is the loop and one `evaluate` call; a literal
//! string costs more, the difference being the `String` clone that `Expr::Literal` does on every
//! evaluation.
//!
//! `["get", k]` costs several times a literal. A little of that is the lookup and a little the
//! key, which leaves most of it in the walk: recursive non-inlined `evaluate` calls returning a
//! 40-byte `Result<Value, EvaluationError>` by memory, to carry what is nearly always an 8-byte
//! `f64`, plus the `Option`/`Result` wrapping and the drops on the way back out.
//!
//! That was taken as DR-11's bytecode VM's target. Building one showed it is not: a flat
//! evaluator over an operand stack of `Value` measured slower than the walk at every frame size,
//! because `Value` has a destructor and a frame is therefore initialised and dropped per
//! evaluation. §12.1 records the conclusion: a compact `Copy` value comes before any VM.
//!
//! An earlier reading of these numbers named the 40-byte `Result` as *the* cause; that was
//! inference from a lookup cost nobody had measured, and measuring it at 2 ns is what made the
//! rest attributable.
//!
//! Dependency-free and percentile-reporting for the reasons `four_view_sweep` gives.
//!
//! # Read ratios, not absolutes
//!
//! The same decode measured 455 us and 812 us an hour apart on this machine, and the world tile
//! 2.6 ms and 6.4 ms, with nothing in between but somebody else's build: load average was 25 on
//! 32 cores. Nothing here pins a core or asks for one.
//!
//! So a figure quoted from a single run means very little. What survives is a comparison taken
//! *within* one run, or a with-and-without pair alternated across several — which is how every
//! change this file drove was measured, and why those held while the absolutes wandered by a
//! factor of two. A number from here belongs in a commit message with the thing it was compared
//! against, never on its own.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Counts allocations, so "how much churn is left" is a number rather than a grep.
///
/// Two of this session's wins in the build path were allocations nobody had counted — a key
/// string cloned twice per `get`, and three vectors per feature in the binder.
///
/// # Why the counting is switched off for the timed sections
///
/// Two atomic read-modify-writes on every allocation are not free, and they are least free
/// exactly where allocation is the thing under test: with the counter unconditionally on, the
/// `literal string` case read 12 ns against its true 7, because the `String` clone it exists to
/// show now carried the counter as well. A benchmark that changes what it measures in
/// proportion to what it is measuring is worse than no benchmark. The flag is checked instead,
/// which is a predictable branch and stays off for every timed run.
struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static COUNTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// SAFETY: every method forwards to `System` unchanged; the counters are the only addition and
// they touch no allocator state.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `body` once and reports what it allocated.
fn allocations(label: &str, features: usize, mut body: impl FnMut()) {
    let before = (
        ALLOCATIONS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
    );
    COUNTING.store(true, Ordering::Relaxed);
    body();
    COUNTING.store(false, Ordering::Relaxed);
    let count = ALLOCATIONS.load(Ordering::Relaxed) - before.0;
    let bytes = BYTES.load(Ordering::Relaxed) - before.1;
    println!(
        "  {label:<26} {count:>9} allocations  {:>7} KiB  {:>5.1} per feature",
        bytes / 1024,
        count as f64 / features as f64
    );
}

use tessella_source::mvt;
use tessella_style::Style;

/// mbgl's own benchmark tile: zoom 10, Mapbox Streets schema, 593 features over fourteen layers
/// and 28 156 points.
///
/// Chosen because `benchmark/parse/vector_tile.benchmark.cpp` decodes exactly this file and
/// touches every feature's geometries and properties, so a number here and a number there are
/// about the same work on the same bytes. Anything else makes a comparison an argument about
/// fixtures.
const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/streets-10-163-395.mvt");

/// A zoom-14 Protomaps tile over Berlin: 934 features, seven layers, 20 980 points.
///
/// Kept beside the streets tile because the two are dense in different things — Berlin carries
/// 3.4 properties per feature against 2.0, and the streets tile 47 points per feature against
/// 22. An optimisation that helps one need not help the other, which is the same lesson the
/// world tile taught more expensively.
const BERLIN_TILE: &[u8] =
    include_bytes!("../../../tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt");

/// The zoom-0 world tile this used to measure, kept for contrast.
///
/// 17 202 features, 17 153 of them in one `admin` layer. Every decision in the optimisation
/// thread above was weighed against it before anyone checked what a real tile looks like — the
/// wins were real, but their relative sizes were not. Reported beside the real tile so the
/// difference stays visible rather than being a thing to rediscover.
const WORLD_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");
const LIVE: &str = include_str!("../../tessella-style/tests/live_style.json");

/// A style whose paint properties are all data-driven, to put evaluation at its worst.
const DATA_DRIVEN: &str = r##"{
  "version": 8,
  "sources": { "src": { "type": "vector", "tiles": ["http://x/{z}/{x}/{y}.mvt"] } },
  "layers": [
    { "id": "landuse", "type": "fill", "source": "src", "source-layer": "landuse",
      "paint": {
        "fill-color": ["interpolate", ["linear"], ["zoom"],
           0, ["match", ["get", "class"], "lake", "#001133", "#002244"],
           16, ["match", ["get", "class"], "lake", "#0044aa", "#0055bb"]],
        "fill-opacity": ["case", ["has", "class"], 0.9, 0.5]
      } },
    { "id": "roads", "type": "line", "source": "src", "source-layer": "road",
      "paint": {
        "line-color": ["match", ["get", "class"], "highway", "#ffcc00", "#ffffff"],
        "line-width": ["interpolate", ["linear"], ["zoom"],
           10, ["match", ["get", "class"], "highway", 2.0, 0.5],
           16, ["match", ["get", "class"], "highway", 8.0, 2.0]],
        "line-opacity": ["case", ["==", ["get", "class"], "highway"], 1.0, 0.7]
      } }
  ]
}"##;

/// The same layers with every paint property a constant.
const CONSTANT: &str = r##"{
  "version": 8,
  "sources": { "src": { "type": "vector", "tiles": ["http://x/{z}/{x}/{y}.mvt"] } },
  "layers": [
    { "id": "landuse", "type": "fill", "source": "src", "source-layer": "landuse",
      "paint": { "fill-color": "#002244", "fill-opacity": 0.9 } },
    { "id": "roads", "type": "line", "source": "src", "source-layer": "road",
      "paint": { "line-color": "#ffffff", "line-width": 2.0, "line-opacity": 1.0 } }
  ]
}"##;

const ROUNDS: u32 = 40;

fn percentiles(mut samples: Vec<Duration>) -> (Duration, Duration, Duration) {
    samples.sort_unstable();
    let at = |fraction: f64| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let index = ((samples.len() as f64 - 1.0) * fraction) as usize;
        samples[index]
    };
    (at(0.5), at(0.95), samples[samples.len() - 1])
}

fn build(style: &Style, decoded: &mvt::Tile, tile: tessella_orchestrate::tile::TileId) -> usize {
    let built =
        tessella_orchestrate::tile::build_mvt_tile(style, "src", tile, decoded).expect("builds");
    built
        .iter()
        .map(|bucket| match &bucket.content {
            tessella_orchestrate::tile::Content::Fill(fill) => fill.vertices.len(),
            tessella_orchestrate::tile::Content::Line(line) => line.vertices.len(),
            tessella_orchestrate::tile::Content::Circle(circle) => circle.vertices.len(),
            tessella_orchestrate::tile::Content::Background => 0,
        })
        .sum()
}

fn time(label: &str, mut round: impl FnMut() -> usize) {
    // One untimed round so the first sample is not measuring a cold allocator.
    let vertices = round();
    let mut samples = Vec::with_capacity(ROUNDS as usize);
    for _ in 0..ROUNDS {
        let started = Instant::now();
        let _ = round();
        samples.push(started.elapsed());
    }
    let (p50, p95, max) = percentiles(samples);
    println!(
        "{label:<28} p50 {p50:>10.2?}  p95 {p95:>10.2?}  max {max:>10.2?}  ({vertices} vertices)"
    );
}

fn main() {
    let decoded = mvt::Tile::decode(TILE).expect("decodes");
    let tile = tessella_orchestrate::tile::TileId::new(0, 0, 0);

    println!("Tile: streets-10-163-395.mvt (maplibre-native's Parse_VectorTile fixture)");
    for layer in &decoded.layers {
        println!("  {} : {} features", layer.name, layer.len());
    }
    println!();

    // The first of these is what `Parse_VectorTile` measures in maplibre-native.
    time("decode (z10 streets)", || {
        mvt::Tile::decode(TILE).expect("decodes").layers.len()
    });
    time("decode (z14 Berlin)", || {
        mvt::Tile::decode(BERLIN_TILE)
            .expect("decodes")
            .layers
            .len()
    });
    time("decode (z0 world)", || {
        mvt::Tile::decode(WORLD_TILE).expect("decodes").layers.len()
    });

    let data_driven = Style::parse(DATA_DRIVEN).expect("parses");
    let constant = Style::parse(CONSTANT).expect("parses");
    time("data-driven paint", || build(&data_driven, &decoded, tile));
    time("constant paint", || build(&constant, &decoded, tile));

    let _ = LIVE;

    println!();
    println!("Allocations for one build:");
    let features: usize = decoded
        .layers
        .iter()
        .map(|layer| layer.len())
        .sum();
    allocations("data-driven paint", features, || {
        let _ = build(&data_driven, &decoded, tile);
    });
    allocations("constant paint", features, || {
        let _ = build(&constant, &decoded, tile);
    });

    allocations("decode alone", features, || {
        let _ = mvt::Tile::decode(TILE).expect("decodes");
    });

    println!();
    println!("One expression, evaluated once per feature over the layer's features:");
    per_expression(&decoded);
}

/// Times single expressions over the dense layer's features, to say where the evaluation cost
/// actually is before anything is built to make it cheaper.
fn per_expression(decoded: &mvt::Tile) {
    use tessella_style::expression::Expression;

    let layer = decoded
        .layers
        .iter()
        .find(|layer| layer.name == "landcover")
        .expect("the dense layer");

    let cases: &[(&str, &str)] = &[
        ("literal number", "4.0"),
        ("literal string", r#""kind""#),
        ("literal short string", r#""a""#),
        ("get", r#"["get", "class"]"#),
        ("has", r#"["has", "kind"]"#),
        (
            "match on number",
            r#"["match", ["get", "class"], "highway", 2.0, 0.5]"#,
        ),
        (
            "match on string",
            r#"["match", ["get", "class"], "boundary", 2.0, 0.5]"#,
        ),
        (
            "case",
            r#"["case", ["==", ["get", "class"], "highway"], 1.0, 0.7]"#,
        ),
        (
            "interpolate (zoom only)",
            r#"["interpolate", ["linear"], ["zoom"], 10, 1.0, 16, 4.0]"#,
        ),
        (
            "interpolate over match",
            r#"["interpolate", ["linear"], ["zoom"],
                10, ["match", ["get", "class"], "highway", 2.0, 0.5],
                16, ["match", ["get", "class"], "highway", 8.0, 2.0]]"#,
        ),
        ("rgb", r#"["rgb", 255, 204, 0]"#),
        ("rgb over get", r#"["rgb", ["get", "class"], 204, 0]"#),
        (
            "colour match",
            r##"["match", ["get", "class"], "highway", "#ffcc00", "#ffffff"]"##,
        ),
    ];

    // The lookup on its own, with no expression around it: the same call `["get", k]` makes,
    // timed directly, so what the expression costs above and beyond it is attributable.
    {
        use tessella_style::expression::Feature as StyleFeature;
        for (label, key) in [
            ("property (present)", "class"),
            ("property (absent)", "zzz"),
        ] {
            let run = || {
                let mut sink = 0usize;
                for feature in layer.features() {
                    if StyleFeature::property(&feature, key).is_some() {
                        sink += 1;
                    }
                }
                sink
            };
            let _ = run();
            let mut samples = Vec::with_capacity(20);
            for _ in 0..20 {
                let started = Instant::now();
                let _ = run();
                samples.push(started.elapsed());
            }
            let (p50, _, _) = percentiles(samples);
            #[allow(clippy::cast_possible_truncation)]
            let each = p50.as_nanos() as u64 / layer.len() as u64;
            println!("  {label:<26} {p50:>9.2?} total  {each:>5} ns/feature");
        }
    }

    // The colour parse on its own. `encode` calls this per feature per colour slot, on a string
    // the style fixed at parse time, so it bounds what folding colours into a first-class value
    // could win — together with the clone that the `colour match` case above shows.
    {
        let value = tessella_style::Value::String("#ffcc00".into());
        let run = || {
            let mut sink = 0usize;
            for _ in 0..layer.len() {
                if tessella_style::property::as_color(&value).is_ok() {
                    sink += 1;
                }
            }
            sink
        };
        let _ = run();
        let mut samples = Vec::with_capacity(20);
        for _ in 0..20 {
            let started = Instant::now();
            let _ = run();
            samples.push(started.elapsed());
        }
        let (p50, _, _) = percentiles(samples);
        #[allow(clippy::cast_possible_truncation)]
        let each = p50.as_nanos() as u64 / layer.len() as u64;
        println!(
            "  {:<26} {p50:>9.2?} total  {each:>5} ns/feature",
            "as_color (hex parse)"
        );
    }

    for (label, source) in cases {
        let value: tessella_style::Value = serde_json::from_str(source).expect("a value");
        let expression = Expression::parse(&value).expect("parses");

        let run = || {
            let mut sink = 0usize;
            for feature in layer.features() {
                if expression.evaluate(Some(13.5), Some(&feature)).is_ok() {
                    sink += 1;
                }
            }
            sink
        };
        let _ = run();
        let mut samples = Vec::with_capacity(20);
        for _ in 0..20 {
            let started = Instant::now();
            let _ = run();
            samples.push(started.elapsed());
        }
        let (p50, _, _) = percentiles(samples);
        #[allow(clippy::cast_possible_truncation)]
        let each = p50.as_nanos() as u64 / layer.len() as u64;
        println!("  {label:<26} {p50:>9.2?} total  {each:>5} ns/feature");
    }
}
