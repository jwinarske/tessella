# mbgl oracle converters

Turns mbgl's own unit-test expectations into Rust tests, for the cases where the expectations
*are* the specification and there are too many to transcribe by hand.

## `update_renderables.py`

Converts `test/algorithm/update_renderables.test.cpp` — eighteen tests, fourteen hundred lines
of tile-id literals — into `crates/tessella-tile/tests/renderables_oracle.rs`.

```
python3 tools/mbgl-codegen/oracles/update_renderables.py crates/tessella-tile/tests/generated.rs
```

It writes the test bodies only; the harness at the top of `renderables_oracle.rs` is
hand-written and stable.

### Why a converter and not a transcription

An id off by one in an expectation does not fail loudly. It makes a *wrong* implementation look
right, which is the one failure a golden test cannot afford, and fourteen hundred lines of
`{2, 0, {2, 1, 3}}` is exactly the material that produces one.

The converter is written to fail rather than guess. Every statement in every test body must
match a known rule or it refuses to emit — nothing is skipped silently — and after conversion it
audits the result by counting: every `createTileData`, `idealTiles.emplace`, `EXPECT_EQ` and
`updateRenderables` in the C++ must appear in the Rust.

That audit earned its place immediately. The statement splitter originally ended a statement
only at a semicolon, so a `for` block ran on into the declaration after it, the combined text
matched the block's own rule, and one `createTileData` vanished. The generated test still
compiled and still asserted something — it just set up a different world than mbgl did.

mbgl's own trailing comments are carried across onto the entries they annotate, so the
generated file can be read against the original line by line.
