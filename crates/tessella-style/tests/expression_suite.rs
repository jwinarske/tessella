//! The style-spec expression conformance suite (§15's R-3 mitigation).
//!
//! # What this checks that the capture diff cannot
//!
//! Every other comparison in this repository runs against `mbgl-capture-probe`, which makes
//! them statements that this frontend agrees with *one implementation*. These cases come from
//! the specification, so they check agreement with the spec itself — and where mbgl and the
//! spec disagree, that is a thing worth discovering rather than averaging over.
//!
//! They also cost no C++ build, which is what makes them the R1 workstream that can start
//! immediately.
//!
//! # A baseline, not a pass-or-fail gate
//!
//! The suite is 350 cases over the whole expression language, and this evaluator implements
//! the part R0 needed. Asserting "all pass" would fail on day one and stay failing, which
//! makes a test that nobody reads. Asserting nothing would let the number silently fall.
//!
//! So the pass set is committed, and the test asserts two things: every case in the baseline
//! still passes, and the current pass count is at least the baseline's. Adding an operator
//! moves cases from failing to passing and the baseline is regenerated; breaking one moves a
//! case the other way and the test names it. What it will not do is quietly rot.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tessella_style::Value;
use tessella_style::expression::{Dependency, Expression, Feature, PropertySpec, Type};

/// The vendored suite.
fn suite_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/expression-suite")
}

/// A case's name, as its directory path spells it: `case/basic`.
fn case_name(path: &Path, root: &Path) -> String {
    path.parent()
        .and_then(|dir| dir.strip_prefix(root).ok())
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// Every case, sorted so a run is reproducible.
fn cases() -> Vec<(String, Value)> {
    let root = suite_root();
    let mut found = Vec::new();
    collect(&root, &mut found);
    found.sort();

    found
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            let value: Value = serde_json::from_str(&text).ok()?;
            Some((case_name(&path, &root), value))
        })
        .collect()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.file_name().is_some_and(|name| name == "test.json") {
            out.push(path);
        }
    }
}

/// A feature as the suite describes one.
struct SuiteFeature {
    properties: Value,
    geometry_type: String,
    id: Option<Value>,
}

impl Feature for SuiteFeature {
    fn property(&self, key: &str) -> Option<Value> {
        self.properties.get(key).cloned()
    }

    fn geometry_type(&self) -> &str {
        &self.geometry_type
    }

    fn id(&self) -> Option<Value> {
        self.id.clone()
    }

    fn properties(&self) -> Value {
        self.properties.clone()
    }
}

fn suite_feature(input: Option<&Value>) -> Option<SuiteFeature> {
    let feature = input?;
    Some(SuiteFeature {
        properties: feature
            .get("properties")
            .cloned()
            .unwrap_or(Value::Object(Default::default())),
        geometry_type: feature
            .get("geometry")
            .and_then(|g| g.get("type"))
            .or_else(|| feature.get("geometry_type"))
            .and_then(Value::as_str)
            .unwrap_or("Point")
            .to_string(),
        id: feature.get("id").cloned(),
    })
}

/// Runs one case, returning `Ok(())` when it conforms.
fn run_case(case: &Value) -> Result<(), String> {
    let expression = case.get("expression").ok_or("no expression")?;
    let expected = case.get("expected").ok_or("no expected")?;
    let compiled = expected.get("compiled").ok_or("no compiled")?;
    let wants_success = compiled.get("result").and_then(Value::as_str) == Some("success");

    // Pre-expression functions need both halves of the property spec: the default they fall
    // back to, and the type `identity` checks its property against. Neither is in the style.
    let spec = case.get("propertySpec");
    let property = PropertySpec {
        default: spec.and_then(|spec| spec.get("default")).cloned(),
        expected: spec
            .and_then(|spec| spec.get("type"))
            .and_then(Value::as_str)
            .and_then(|name| match name {
                "number" => Some(Type::Number),
                "string" => Some(Type::String),
                "boolean" => Some(Type::Boolean),
                "color" => Some(Type::Color),
                "array" => Some(Type::Array),
                // `enum` is a string with a value list, which this does not check yet;
                // treating it as a string is right about the type and silent about the list.
                "enum" => Some(Type::String),
                "formatted" => Some(Type::Formatted),
                _ => None,
            }),
    };

    let parsed = match Expression::parse_for(expression, &property) {
        Ok(parsed) => {
            if !wants_success {
                return Err("parsed, but the spec expects a compile error".to_string());
            }
            parsed
        }
        Err(err) => {
            // A case the spec says must fail to compile, which we also fail to compile *because
            // the operator is not implemented*, is not conformance — it is two unrelated
            // failures agreeing by accident. Counting it as a pass inflates the baseline and,
            // worse, means implementing the operator can move a case from passing to failing.
            let unimplemented = matches!(
                err,
                tessella_style::expression::ParseError::UnknownOperator(_)
            );
            return if wants_success || unimplemented {
                Err(format!("parse failed: {err}"))
            } else {
                Ok(())
            };
        }
    };

    // The classification lattice, which the spec states as two booleans.
    let feature_constant = compiled
        .get("isFeatureConstant")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let zoom_constant = compiled
        .get("isZoomConstant")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let wanted = match (feature_constant, zoom_constant) {
        (true, true) => Dependency::None,
        (true, false) => Dependency::Zoom,
        (false, true) => Dependency::Feature,
        (false, false) => Dependency::ZoomAndFeature,
    };
    if parsed.dependency() != wanted {
        return Err(format!(
            "classified {:?}, spec says {wanted:?}",
            parsed.dependency()
        ));
    }

    let inputs = expected
        .get("outputs")
        .and_then(Value::as_array)
        .map(<[Value]>::to_vec)
        .unwrap_or_default();
    let given = case
        .get("inputs")
        .and_then(Value::as_array)
        .map(<[Value]>::to_vec)
        .unwrap_or_default();

    for (index, want) in inputs.iter().enumerate() {
        // Each input is a `[globals, feature]` pair.
        let pair = given.get(index).and_then(Value::as_array);
        let globals = pair.and_then(|pair| pair.first());
        let zoom = globals
            .and_then(|g| g.get("zoom"))
            .and_then(Value::as_number);
        let feature = suite_feature(pair.and_then(|pair| pair.get(1)));

        let got = parsed.evaluate(zoom, feature.as_ref().map(|f| f as &dyn Feature));
        let wants_error = want.get("error").is_some();

        match (got, wants_error) {
            (Ok(value), false) => {
                if !values_match(&value, want) {
                    return Err(format!("input {index}: got {value:?}, want {want:?}"));
                }
            }
            (Err(_), true) => {}
            (Ok(value), true) => {
                return Err(format!("input {index}: got {value:?}, want an error"));
            }
            (Err(err), false) => {
                return Err(format!("input {index}: {err}, want {want:?}"));
            }
        }
    }
    Ok(())
}

/// Compares a produced value against the suite's expectation.
///
/// Numbers compare with a tolerance, because the suite's expectations are decimal literals and
/// several operators are transcendental. Everything else compares exactly.
///
/// # Why the tolerance is two units and not one
///
/// The fixtures carry six significant digits and are *truncated* to them, not rounded:
/// `interpolate/exponential` expects `3.33333` for a value that is `3.3333333…`. So a correct
/// result can be a full unit of the sixth digit away — a relative `1e-6` — and a tolerance of
/// exactly `1e-6` puts that case on the boundary, where one ULP in the last bit of the
/// computation decides whether it passes. It did exactly that: `a + (b - a) * t` landed just
/// inside and mbgl's own `a * (1 - t) + b * t` just outside, which would have read as the
/// correct formula regressing.
const TOLERANCE: f64 = 2e-6;

fn values_match(got: &Value, want: &Value) -> bool {
    match (got, want) {
        (Value::Number(a), Value::Number(b)) => (a - b).abs() <= TOLERANCE * a.abs().max(1.0),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values_match(x, y))
        }
        _ => got == want,
    }
}

/// The committed pass set.
fn baseline() -> BTreeSet<String> {
    include_str!("expression_baseline.txt")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToString::to_string)
        .collect()
}

/// Every case the baseline says passes still passes.
///
/// This is the assertion that matters. A case moving from passing to failing is a regression
/// whatever the total does, and the message names it rather than reporting a count that dropped.
#[test]
fn no_case_in_the_baseline_regressed() {
    let baseline = baseline();
    assert!(!baseline.is_empty(), "the baseline is empty");

    let mut broken = Vec::new();
    let mut seen = BTreeSet::new();
    for (name, case) in cases() {
        seen.insert(name.clone());
        if baseline.contains(&name)
            && let Err(reason) = run_case(&case)
        {
            broken.push(format!("{name}: {reason}"));
        }
    }

    let missing: Vec<&String> = baseline.difference(&seen).collect();
    assert!(
        missing.is_empty(),
        "the baseline names cases the suite does not have: {missing:?}"
    );
    assert!(broken.is_empty(), "regressed:\n  {}", broken.join("\n  "));
}

/// The suite is present and whole.
#[test]
fn the_suite_is_vendored_whole() {
    let cases = cases();
    assert_eq!(cases.len(), 350, "350 cases were vendored");
}

/// Reports the current pass rate, and regenerates the baseline when asked.
///
/// `TESSELLA_REGENERATE_BASELINE=1 cargo test -p tessella-style --test expression_suite`
/// rewrites the committed list. Gated behind an environment variable rather than done
/// automatically, because a baseline that regenerated itself would absorb a regression as
/// readily as an improvement and the file would stop meaning anything.
#[test]
fn report_the_pass_rate() {
    let baseline = baseline();
    let mut passing = BTreeSet::new();
    let mut failures: Vec<(String, String)> = Vec::new();

    for (name, case) in cases() {
        match run_case(&case) {
            Ok(()) => {
                passing.insert(name);
            }
            Err(reason) => failures.push((name, reason)),
        }
    }

    println!(
        "expression suite: {}/{} passing ({} in baseline)",
        passing.len(),
        passing.len() + failures.len(),
        baseline.len()
    );

    let gained: Vec<&String> = passing.difference(&baseline).collect();
    if !gained.is_empty() {
        println!(
            "newly passing ({}) — regenerate the baseline:",
            gained.len()
        );
        for name in gained.iter().take(20) {
            println!("  + {name}");
        }
    }
    // Grouped by cause rather than listed, because 285 failures scroll past and the shape of
    // them is the actionable part: which operator to implement next is a question about counts.
    let mut by_cause: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (_, reason) in &failures {
        let cause =
            if let Some(rest) = reason.strip_prefix("parse failed: unknown expression operator ") {
                format!("unimplemented operator {rest}")
            } else if reason.starts_with("parse failed:") {
                "parse rejects a valid expression".to_string()
            } else if reason.contains("classified") {
                "classification differs".to_string()
            } else if reason.contains("want an error") {
                "evaluates where the spec requires an error".to_string()
            } else if reason.contains("parsed, but the spec expects") {
                "accepts an expression the spec rejects".to_string()
            } else {
                "wrong value".to_string()
            };
        *by_cause.entry(cause).or_default() += 1;
    }
    let mut ranked: Vec<(&String, &usize)> = by_cause.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    println!("failures by cause:");
    for (cause, count) in ranked.iter().take(30) {
        println!("  {count:4}  {cause}");
    }

    if let Ok(filter) = std::env::var("TESSELLA_SUITE_SHOW") {
        println!("failures matching {filter:?}:");
        for (name, reason) in failures.iter().filter(|(_, r)| r.contains(&filter)) {
            println!("  - {name}: {reason}");
        }
    }

    if std::env::var("TESSELLA_REGENERATE_BASELINE").is_ok() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/expression_baseline.txt");
        let mut text = String::from(
            "# Cases from tests/expression-suite that this evaluator passes.\n\
             # Regenerate with TESSELLA_REGENERATE_BASELINE=1; see expression_suite.rs.\n",
        );
        for name in &passing {
            text.push_str(name);
            text.push('\n');
        }
        std::fs::write(&path, text).expect("writes the baseline");
        println!("wrote {} entries to {}", passing.len(), path.display());
    }
}
