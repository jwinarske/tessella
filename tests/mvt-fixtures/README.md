# MVT conformance fixtures

## The redistribution rule

**Mapbox-origin tile data ships here only if maplibre-native ships it too.** Not "only if it is
openly licensed" and not "only if it carries no identifier" — the test is presence upstream.
maplibre-native is BSD-2-Clause with Mapbox's copyright acknowledged (© 2014–2020), so a file it
publishes is a file already redistributed on terms this repository can meet. A tile fetched from
a Mapbox endpoint with an account's credentials is not, whatever it contains, and does not come
in here.

The rule is not about what a tile exposes. An MVT body is layer names, property keys, property
values and packed geometry: no URL, no token, no account id — grep a real Mapbox Streets tile for
any of them and it returns nothing. What identifies a customer lives in the *request URL*, which
is why extracted glyphs are kept outside this repository and tiles need not be. Redistribution is
a licensing question, and it is answered upstream or not at all.

Data that is not Mapbox's answers to its own licence instead: `protomaps-berlin-14-8802-5373.mvt`
and everything in `../live-fixtures/` are OpenStreetMap under ODbL, cut from a Protomaps planet
extract, and are ours to ship on those terms.

### Checking it

Hash every committed fixture and look for the hash in a maplibre-native checkout:

```sh
find "$MLN" -type f -not -path '*/.git/*' -exec md5sum {} + | awk '{print $1}' | sort -u > /tmp/mln
git ls-files | grep -vE '\.(rs|toml|md|yml|yaml|h|c|sh|lock|gitignore|txt)$' | while read -r f; do
  grep -qx "$(md5sum "$f" | awk '{print $1}')" /tmp/mln || echo "not upstream: $f"
done
```

Everything it names must be ours or ODbL. As of the last audit that is the golden dumps, the
style JSONs, the codegen oracles, `LICENSE`/`NOTICE`, and the Protomaps tiles — every
Mapbox-origin fixture, `real-world-0-0-0.mvt` and `streets-10-163-395.mvt` included, is
byte-identical to a file maplibre-native commits.

It is a review step rather than a test because it needs a maplibre-native checkout, which CI does
not have and which DR-6's generated tables also need. Run it when adding a fixture.

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

## A real tile, for measurement rather than conformance

`protomaps-berlin-14-8802-5373.mvt` is not from the conformance suite. It is one zoom-14 tile
over Berlin, cut from a Protomaps planet extract (Protomaps schema; OpenStreetMap data, ODbL),
inflated from the gzip the archive stores it as.

It exists because the benchmark had been measuring `real-world-0-0-0.mvt`, and that tile is a
zoom-0 view of the whole world: 17 202 features, of which 17 153 are one `admin` layer. A real
tile at a zoom anyone looks at has 934 to 3 160 features spread over seven layers, and two to
three properties each rather than three on a single dense layer. Both are valid tiles; only one
is shaped like the thing being optimised.

Measured against it, decode is about 0.21 us per feature plus 0.016 us per point — so on a tile
of dense polygons the geometry is roughly sixty per cent of the work and on a tile of scattered
points it is twenty. The world tile suggests neither ratio.

Conformance still runs against the vendored suite. This one is for numbers.

## `streets-10-163-395.mvt` — the tile maplibre-native benchmarks

Copied from `test/fixtures/api/assets/streets/10-163-395.vector.pbf`, which is the file
`benchmark/parse/vector_tile.benchmark.cpp` decodes in `Parse_VectorTile`. Zoom 10, Mapbox
Streets schema, 593 features over fourteen layers, 28 156 points.

It is here so a decode number on this side and a decode number on that side are about the same
work on the same bytes. Comparing against anything else turns a performance question into an
argument about fixtures.

Worth noting how the earlier mistake happened: `real-world-0-0-0.mvt` is byte-identical to
maplibre-native's `0-0-0.vector.pbf`, its *test* fixture. They benchmark the zoom-10 tile and
test with the zoom-0 one. We vendored the test tile and benchmarked against it, which is how a
zoom-0 view of the whole world came to weigh every optimisation decision.

## `../live-fixtures/` — the real-style parity corpus

Nine zoom-5 tiles over London and the TileJSON that names them, cut from a Protomaps planet
extract (Protomaps schema; OpenStreetMap data, ODbL) and stored inflated. They are the exact
tiles `live_parity.rs` covers, and the golden `live_protomaps_z5.dump` was captured against the
same bytes.

They are vendored for the reason a test corpus usually is, and the reason arrived the hard way.
Those parity assertions used to require a 187 MB archive and `pmtiles serve` running, so they
were `#[ignore]`d. Then the MVT decoder was rewritten — flat geometry, per-layer buffers, a
varint fast path — and the byte-exact test that would have caught a regression was the one test
not running. It passed when finally run, which is luck rather than process.

434 KB, less than the world tile beside them, and `tools/tile-server` already ships in this
repository — so the test starts its own origin and runs wherever `cargo test` does. Setting
`TESSELLA_LIVE_ORIGIN` still points it at a real `pmtiles serve`, which is how the fixtures were
checked to agree with the archive.
