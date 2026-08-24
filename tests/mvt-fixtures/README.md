# MVT conformance fixtures

The Mapbox Vector Tile fixture suite, vendored from maplibre-native's `vector-tile` vendor tree
(`test/mvt-fixtures`, version 2.1.0, ISC, © Mapbox). 14 valid tiles and 24 invalid ones, tracking
MVT spec 2.1.

## Why vendored

Same reason as the expression suite: a corpus that only runs when someone happens to have a
maplibre-native checkout is not a gate. 2 KB of fixtures makes it one.

These check agreement with the *specification* rather than with mbgl, and need no C++ build —
which matters here because MVT decoding is the gateway to every real style, and getting it wrong
is not subtle at the top and very subtle underneath: a mis-stepped geometry cursor produces
coordinates thousands of units away, but a silently defaulted `extent` produces a layer at
exactly half scale.

## "Invalid" is not "must be rejected"

Several invalid fixtures are invalid *as vector tiles* while being well-formed protobuf — an
unknown field, for instance. Protobuf requires a decoder to skip what it does not recognize,
which is what makes the encoding extensible, so refusing those would reject tiles a newer writer
is entitled to produce.

So `mvt_fixtures.rs` records an outcome per fixture with the reason, rather than asserting one
rule over all of them.

## Fixtures demonstrate more than their names

Three of them carry a second defect beyond the one they are named for, and this decoder trips on
that one first:

- `Layer-extent-none` and `Layer-unknown_field_type` both also carry a one-element tag list,
  where their siblings carry two, so the odd-tags rule fires before anything about extents or
  unknown fields.
- `Value-unknown-field-type` is refused, but not because the field is unknown: skipping it as
  protobuf requires leaves the `Value` empty, and the spec says exactly one of its seven
  alternatives must be set.

Recorded as they behave rather than as their names read. A fixture name is a description, not a
specification.

## Updating

Re-copy from a maplibre-native checkout. Nothing here is edited, which is what keeps a failure a
statement about this decoder rather than about a fixture someone adjusted.
