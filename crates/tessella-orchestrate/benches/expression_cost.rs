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
//! Over the fixture's 17154-feature `admin` layer: 6.8 ms data-driven against 2.1 ms constant,
//! so having data-driven paint is about two thirds of bucket build. §12.1's claim holds here.
//!
//! "Data-driven" is not the same as "evaluation", and the gap between the two arms is the first
//! and not the second. It was 8.5 ms against 2.2 ms until the binder stopped allocating a
//! scratch vector per feature and two more per slot inside `encode` — a quarter of the
//! surcharge, in the path that runs only when a property is data-driven and so was easy to read
//! as evaluation cost.
//!
//! The first version of this measured the 48-feature `water` layer instead and read 19 %, which
//! would have argued against building the VM at all. The layer a benchmark picks is not a
//! detail.
//!
//! The per-expression numbers say where the cost is, and the first two lines are there so it is
//! attributable rather than inferred.
//!
//! `Feature::property` called directly — the same dyn-dispatched call `["get", k]` makes, with
//! the same scan and the same owned `Value` built — is 2 ns per feature, present or absent. The
//! data access is not the cost. A bare literal number is 3 ns, which is the loop and one
//! `evaluate` call; a literal string is 7 ns, the extra 4 being the `String` clone that
//! `Expr::Literal` does on every evaluation.
//!
//! `["get", "admin_level"]` is 26 ns. Two of those are the lookup and about seven are evaluating
//! the key — itself a string literal, cloned per feature — which leaves most of it in the walk:
//! recursive non-inlined `evaluate` calls returning a 40-byte
//! `Result<Value, EvaluationError>` by memory, to carry what is nearly always an 8-byte `f64`,
//! plus the `Option`/`Result` wrapping and the drops on the way back out.
//!
//! That is what DR-11's bytecode VM has to fix, and it is a claim about the walk rather than
//! about any one part of it. An earlier reading of these numbers named the 40-byte `Result` as
//! *the* cause; that was inference from a lookup cost nobody had measured, and measuring it at
//! 2 ns is what made the rest attributable.
//!
//! Dependency-free and percentile-reporting for the reasons `four_view_sweep` gives.

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

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");
const LIVE: &str = include_str!("../../tessella-style/tests/live_style.json");

/// A style whose paint properties are all data-driven, to put evaluation at its worst.
const DATA_DRIVEN: &str = r##"{
  "version": 8,
  "sources": { "src": { "type": "vector", "tiles": ["http://x/{z}/{x}/{y}.mvt"] } },
  "layers": [
    { "id": "water", "type": "fill", "source": "src", "source-layer": "water",
      "paint": {
        "fill-color": ["interpolate", ["linear"], ["zoom"],
           0, ["match", ["get", "class"], "lake", "#001133", "#002244"],
           16, ["match", ["get", "class"], "lake", "#0044aa", "#0055bb"]],
        "fill-opacity": ["case", ["has", "class"], 0.9, 0.5]
      } },
    { "id": "admin", "type": "line", "source": "src", "source-layer": "admin",
      "paint": {
        "line-color": ["match", ["get", "admin_level"], 2, "#ffcc00", "#ffffff"],
        "line-width": ["interpolate", ["linear"], ["zoom"],
           10, ["match", ["get", "admin_level"], 2, 2.0, 0.5],
           16, ["match", ["get", "admin_level"], 2, 8.0, 2.0]],
        "line-opacity": ["case", ["==", ["get", "admin_level"], 2], 1.0, 0.7]
      } }
  ]
}"##;

/// The same layers with every paint property a constant.
const CONSTANT: &str = r##"{
  "version": 8,
  "sources": { "src": { "type": "vector", "tiles": ["http://x/{z}/{x}/{y}.mvt"] } },
  "layers": [
    { "id": "water", "type": "fill", "source": "src", "source-layer": "water",
      "paint": { "fill-color": "#002244", "fill-opacity": 0.9 } },
    { "id": "admin", "type": "line", "source": "src", "source-layer": "admin",
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

    println!("Tile: real-world-0-0-0.mvt");
    for layer in &decoded.layers {
        println!("  {} : {} features", layer.name, layer.features.len());
    }
    println!();

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
        .map(|layer| layer.features.len())
        .sum();
    allocations("data-driven paint", features, || {
        let _ = build(&data_driven, &decoded, tile);
    });
    allocations("constant paint", features, || {
        let _ = build(&constant, &decoded, tile);
    });

    println!();
    println!("One expression, evaluated once per feature over the same 17154:");
    per_expression(&decoded);
}

/// Times single expressions over the dense layer's features, to say where the evaluation cost
/// actually is before anything is built to make it cheaper.
fn per_expression(decoded: &mvt::Tile) {
    use tessella_style::expression::Expression;

    let layer = decoded
        .layers
        .iter()
        .find(|layer| layer.name == "admin")
        .expect("the dense layer");

    let cases: &[(&str, &str)] = &[
        ("literal number", "4.0"),
        ("literal string", r#""admin_level""#),
        ("literal short string", r#""a""#),
        ("get", r#"["get", "admin_level"]"#),
        ("has", r#"["has", "admin_level"]"#),
        (
            "match on number",
            r#"["match", ["get", "admin_level"], 2, 2.0, 0.5]"#,
        ),
        (
            "match on string",
            r#"["match", ["get", "class"], "boundary", 2.0, 0.5]"#,
        ),
        (
            "case",
            r#"["case", ["==", ["get", "admin_level"], 2], 1.0, 0.7]"#,
        ),
        (
            "interpolate (zoom only)",
            r#"["interpolate", ["linear"], ["zoom"], 10, 1.0, 16, 4.0]"#,
        ),
        (
            "interpolate over match",
            r#"["interpolate", ["linear"], ["zoom"],
                10, ["match", ["get", "admin_level"], 2, 2.0, 0.5],
                16, ["match", ["get", "admin_level"], 2, 8.0, 2.0]]"#,
        ),
        ("rgb", r#"["rgb", 255, 204, 0]"#),
        ("rgb over get", r#"["rgb", ["get", "admin_level"], 204, 0]"#),
        (
            "colour match",
            r##"["match", ["get", "admin_level"], 2, "#ffcc00", "#ffffff"]"##,
        ),
    ];

    // The lookup on its own, with no expression around it: the same call `["get", k]` makes,
    // timed directly, so what the expression costs above and beyond it is attributable.
    {
        use tessella_style::expression::Feature as StyleFeature;
        for (label, key) in [
            ("property (present)", "admin_level"),
            ("property (absent)", "zzz"),
        ] {
            let run = || {
                let mut sink = 0usize;
                for feature in &layer.features {
                    if StyleFeature::property(feature, key).is_some() {
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
            let each = p50.as_nanos() as u64 / layer.features.len() as u64;
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
            for _ in 0..layer.features.len() {
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
        let each = p50.as_nanos() as u64 / layer.features.len() as u64;
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
            for feature in &layer.features {
                if expression.evaluate(Some(13.5), Some(feature)).is_ok() {
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
        let each = p50.as_nanos() as u64 / layer.features.len() as u64;
        println!("  {label:<26} {p50:>9.2?} total  {each:>5} ns/feature");
    }
}
