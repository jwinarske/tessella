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
//! Over the fixture's 17154-feature `admin` layer: 7.9 ms data-driven against 2.0 ms constant,
//! so evaluation is about three quarters of bucket build. §12.1's claim holds here, emphatically.
//!
//! The first version of this measured the 48-feature `water` layer instead and read 19 %, which
//! would have argued against building the VM at all. The layer a benchmark picks is not a
//! detail.
//!
//! The per-expression numbers say where the cost is. A bare literal is 3 ns per feature, which
//! is the loop and the dispatch; a `get` is 25 ns. The features here carry three properties and
//! the one being read sorts first, so the linear scan finds it immediately — meaning the 22 ns
//! difference is not lookup work. It is the value representation:
//! `Result<Value, EvaluationError>` is 40 bytes, moved through every node of the tree to carry
//! what is nearly always an 8-byte `f64`. That is the thing DR-11's bytecode VM has to fix, and
//! eliminating dispatch alone would not have.
//!
//! Dependency-free and percentile-reporting for the reasons `four_view_sweep` gives.

use std::time::{Duration, Instant};

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
        (
            "colour match",
            r##"["match", ["get", "admin_level"], 2, "#ffcc00", "#ffffff"]"##,
        ),
    ];

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
