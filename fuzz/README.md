# Fuzz targets

Coverage-guided fuzzing of the three inputs that arrive from somewhere else: a vector tile off a
network, a style document off a URL, and a capture ring written by another process.

## Running

libFuzzer needs nightly, and the workspace above is pinned to the stable the target's Yocto
release carries (DR-17). So this directory is its own workspace with its own toolchain file, and
nothing in a normal `cargo build` reaches it.

```
cd fuzz
cargo +nightly fuzz run mvt_tile          # until you stop it
cargo +nightly fuzz run capture_ring -- -runs=1000000
```

CI runs each for a minute. That is a smoke test rather than a campaign — what it buys is that the
targets still build and still return, which is the failure that actually happens to a fuzz target
nobody runs.

## What is already covered elsewhere

`tessella-orchestrate/tests/malformed_input.rs` mutates seeds deterministically against eight
parsers and runs with every commit. It is a weaker search that runs a thousand times more often.
Neither replaces the other, and the module doc there says so.

## The ring target earns its place differently

The other two go deeper into parsers that harness already covers. `capture_ring` is new ground:
the bytes are the one input another *process* writes, and every number the consumer walks is one
it reads rather than one it computed.

Its first version handed `attach` raw fuzz bytes and got four new coverage units in two hundred
thousand runs — every input failed the ABI-revision check at the front door and the walk under
test never ran. What is interesting is not that `attach` rejects garbage, which a unit test
asserts once; it is what the walk does with records. So the control block is well formed by
construction and the fuzzer owns the data region, which is where the records are.
