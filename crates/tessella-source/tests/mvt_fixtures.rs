//! The MVT conformance fixtures (§10 R1).
//!
//! The spec's own corpus, vendored from maplibre-native's `vector-tile` vendor tree: 14 tiles
//! that must decode and 24 that are invalid in a named way. Like the expression suite, it checks
//! agreement with the *specification* rather than with mbgl, and needs no C++ build.
//!
//! # "Invalid" does not uniformly mean "must be rejected"
//!
//! Several of the invalid fixtures are invalid *as vector tiles* while being perfectly
//! well-formed protobuf — an unknown field, for instance. Protobuf requires a decoder to skip
//! what it does not recognize, which is what makes the encoding extensible, so refusing those
//! would reject tiles a newer writer is entitled to produce.
//!
//! So this records an outcome per fixture rather than asserting one rule over all of them, and
//! the outcomes are committed. A fixture changing side is then a visible diff with a reason
//! attached, rather than a count moving.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tessella_source::mvt::Tile;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/mvt-fixtures")
}

/// Every fixture under a directory, by name.
fn fixtures(kind: &str) -> Vec<(String, Vec<u8>)> {
    let dir = fixture_root().join(kind);
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "mvt"))
        .collect();
    found.sort();

    found
        .into_iter()
        .map(|path| {
            let name = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            (name, std::fs::read(&path).expect("reads"))
        })
        .collect()
}

/// Every valid fixture decodes.
///
/// These are the spec's demonstrations of each geometry and value type, so a failure here is a
/// decoder that cannot read a tile any conforming writer may produce.
#[test]
fn every_valid_fixture_decodes() {
    let mut failures = Vec::new();
    for (name, bytes) in fixtures("valid") {
        if let Err(err) = Tile::decode(&bytes) {
            failures.push(format!("{name}: {err}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The valid fixtures carry the geometry their names promise.
///
/// Decoding without erroring is not the same as decoding correctly: a reader that returned an
/// empty tile for everything would pass the test above.
#[test]
fn the_valid_fixtures_carry_their_geometry() {
    use tessella_source::mvt::GeomType;

    let by_name: BTreeMap<String, Vec<u8>> = fixtures("valid").into_iter().collect();
    let decode = |name: &str| {
        Tile::decode(
            by_name
                .get(name)
                .unwrap_or_else(|| panic!("{name} is vendored")),
        )
        .unwrap_or_else(|err| panic!("{name}: {err}"))
    };

    let point = decode("Feature-single-point");
    let layer = point.layers.first().expect("a layer");
    let feature = layer.features.first().expect("a feature");
    assert_eq!(feature.geom_type, GeomType::Point);
    assert_eq!(feature.geometry.len(), 1, "one point");
    assert_eq!(feature.geometry[0].len(), 1);

    let line = decode("Feature-single-linestring");
    let feature = &line.layers[0].features[0];
    assert_eq!(feature.geom_type, GeomType::LineString);
    assert!(
        feature.geometry[0].len() >= 2,
        "a line needs two points: {:?}",
        feature.geometry
    );

    let polygon = decode("Feature-single-polygon");
    let feature = &polygon.layers[0].features[0];
    assert_eq!(feature.geom_type, GeomType::Polygon);
    let ring = &feature.geometry[0];
    assert_eq!(
        ring.first(),
        ring.last(),
        "ClosePath repeats the first point: {ring:?}"
    );

    let multipoint = decode("Feature-single-multipoint");
    let feature = &multipoint.layers[0].features[0];
    assert!(
        feature.geometry.len() > 1,
        "a multipoint is several geometries, not one with several points"
    );
}

/// Every value type resolves to the three a style can see.
#[test]
fn every_value_type_decodes() {
    use tessella_source::mvt::Value;

    let by_name: BTreeMap<String, Vec<u8>> = fixtures("valid").into_iter().collect();
    for name in [
        "Value-single-string-point",
        "Value-single-double-point",
        "Value-single-float-point",
        "Value-single-int64-point",
        "Value-single-uint64-point",
        "Value-single-sint64-point",
        "Value-single-bool-point",
    ] {
        let bytes = by_name.get(name).unwrap_or_else(|| panic!("{name}"));
        let tile = Tile::decode(bytes).unwrap_or_else(|err| panic!("{name}: {err}"));
        let properties = &tile.layers[0].features[0].properties;
        assert_eq!(properties.len(), 1, "{name}");

        // The spec has seven numeric encodings and a style has one number type, so all of the
        // numeric ones must arrive as numbers rather than as seven distinguishable kinds.
        let (_, value) = &properties[0];
        match name {
            "Value-single-string-point" => assert!(matches!(value, Value::String(_)), "{name}"),
            "Value-single-bool-point" => assert!(matches!(value, Value::Bool(_)), "{name}"),
            _ => assert!(matches!(value, Value::Number(_)), "{name}: {value:?}"),
        }
    }

    let all = Tile::decode(by_name.get("Values-all-types").expect("vendored")).expect("decodes");
    assert!(
        all.layers[0].features[0].properties.len() > 1,
        "the all-types fixture carries several"
    );
}

/// What this decoder does with each invalid fixture, recorded rather than asserted uniformly.
///
/// Run with `--nocapture` to read the table. The committed expectations below are what the
/// decoder currently does *and* what the spec's description of each fixture justifies; the two
/// are checked against each other by hand when a line changes.
#[test]
fn invalid_fixtures_are_handled_as_recorded() {
    // `true` means the decoder refuses the tile. `false` means it decodes it, which is correct
    // where the defect is one protobuf requires a reader to tolerate.
    let expected: BTreeMap<&str, bool> = [
        // Structure a vector tile cannot do without.
        ("Layer-name-none", true),
        ("Layer-name-none-version1", true),
        ("Layer-version-none", true),
        ("Layer-name-duplicates", true),
        ("Layer-version-invalid", true),
        // Mistyped fields: the wire type does not match the schema, so the value is not there.
        ("Layer-name-mistyped_uint32", true),
        ("Layer-extent-mistyped_string", true),
        ("Layer-version-mistyped_string", true),
        ("Key-mistyped_uint32", true),
        ("Value-string-mistyped_int64", true),
        // A tag list that cannot be read as pairs, or points outside the tables.
        ("Feature-odd_number_tags", true),
        ("Tags-nonexistant-values", true),
        // A `Value` must set exactly one of its seven alternatives.
        ("Value-no-fields", true),
        ("Value-multiple-fields", true),
        // Geometry the spec forbids.
        ("Feature-multiple-geometries", true),
        ("GeomType-invalid-type", false),
        ("Feature-missing-GeomType", false),
        ("Feature-no-geometry", false),
        // Unknown fields: protobuf requires skipping them, so these decode.
        ("Tile-unknown-tag", false),
        // Refused, and again not for the reason its name suggests: the unknown field is skipped
        // as protobuf requires, and this fixture also carries the one-element tag list.
        ("Layer-unknown_field_type", true),
        ("Feature-unknown_field_type", false),
        // Refused, and not because of the unknown field. This `Value` carries *only* field 10;
        // skipping it as protobuf requires leaves the message empty, and the spec says exactly
        // one of the seven alternatives must be set. The unknown field is tolerated and what
        // remains after tolerating it is not a value.
        ("Value-unknown-field-type", true),
        // An empty layer is a layer that draws nothing, not a broken tile.
        ("Layer-no-features", false),
        // Refused, and not for the reason its name suggests. A missing extent defaults to 4096
        // and is fine; this fixture also carries a one-element tag list where its siblings carry
        // two, so the odd-tags rule fires first. Recorded as it is rather than as the name reads
        // — a fixture may demonstrate more than the defect it is named for.
        ("Layer-extent-none", true),
    ]
    .into_iter()
    .collect();

    let mut surprises = Vec::new();
    for (name, bytes) in fixtures("invalid") {
        let refused = Tile::decode(&bytes).is_err();
        match expected.get(name.as_str()) {
            Some(want) if *want == refused => {}
            Some(want) => surprises.push(format!("{name}: refused={refused}, recorded={want}")),
            None => surprises.push(format!("{name}: not recorded (refused={refused})")),
        }
    }
    assert!(surprises.is_empty(), "{}", surprises.join("\n"));
}

/// The corpus is vendored whole.
#[test]
fn the_corpus_is_complete() {
    assert_eq!(fixtures("valid").len(), 14);
    assert_eq!(fixtures("invalid").len(), 24);
}
