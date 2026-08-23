# Expression test suite

The MapLibre style-spec expression conformance tests, vendored from maplibre-native at
`metrics/integration/expression-tests`. 350 cases, BSD-2-Clause, © MapLibre contributors —
the same license this repository carries.

## Why these are vendored rather than referenced

They are the mitigation §15 names for R-3, expression semantics drift, and a mitigation that
only runs when someone happens to have a maplibre-native checkout is not one. Vendoring costs
200 KB of JSON and makes the suite a CI gate rather than a thing that could be run.

They are also the one R1 oracle that needs no C++ build. Every other comparison here runs
against `mbgl-capture-probe`; these run against the specification, so they check something the
capture diff cannot — that the evaluator agrees with the *spec* rather than with one
implementation of it. Where the two disagree, that is worth knowing rather than averaging.

## What each case carries

- `expression` — the expression under test.
- `inputs` — `[globals, feature]` pairs. Globals carry `zoom`; the feature carries
  `properties`, `geometry_type` and `id`.
- `expected.compiled` — `result`, `type`, and crucially `isFeatureConstant` / `isZoomConstant`,
  which are DR-11's classification lattice stated by the spec rather than inferred here.
- `expected.outputs` — one value per input, or `{"error": ...}` where evaluation must fail.
- `expected.serialized` — the round-tripped expression. Not checked: serializing expressions is
  not something this frontend does, and asserting it would be asserting a feature we lack.

## Updating

Re-copy from a maplibre-native checkout. The cases are upstream's and nothing here is edited,
which is what keeps a failure a statement about this evaluator rather than about a fixture
somebody adjusted to make it pass.
