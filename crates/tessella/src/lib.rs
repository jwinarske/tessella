//! A Rust frontend for the MapLibre style spec, emitting a renderer-agnostic capture stream.
//!
//! **Status: name-reservation stub (plan.md §16, DR-15).** This crate is published at
//! `0.0.0` to hold the name and carries no API. It will become the facade over the
//! `tessella-*` workspace crates (plan.md §7) once those crates carry content:
//!
//! | crate | contents | descends from |
//! |---|---|---|
//! | `tessella-style` | style JSON, expression evaluator, property types, transitions | `style/` |
//! | `tessella-source` | vector/raster/geojson (+clustering) sources | `style/sources`, `renderer/sources` |
//! | `tessella-tile` | pyramid, cover/retain, shared tile store | `tile/`, `algorithm/` |
//! | `tessella-storage` | online + cache file sources, request coalescing | `storage/` |
//! | `tessella-layout` | buckets: fill, line, circle, pattern; symbol shaping/quads | `layout/`, `text/` |
//! | `tessella-place` | collision index, cross-tile index, placement, fades | `text/` |
//! | `tessella-orchestrate` | render layers, tweakers, binders, order, UBO packing, damage gates | `renderer/` |
//! | `tessella-capture-abi` | envelope structs, ring, coalescing | `capture/` |
//! | `tessella-glyph` | glyph manager, PBF path, local SDF rasterization | `text/`, `sprite/` |
//!
//! The renderer is on the far side of the capture stream and is not part of this project.

#![forbid(unsafe_code)]
#![no_std]
