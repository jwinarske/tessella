//! Heatmap buckets: points in, one quad each — the circle bucket's geometry under another name.
//!
//! Transcribed from mbgl's `HeatmapBucket::addFeature` and `HeatmapBucket::vertex`
//! (`renderer/buckets/heatmap_bucket.{hpp,cpp}`).
//!
//! # Why this is an alias and not a copy
//!
//! `HeatmapBucket::vertex` and `CircleBucket::vertex` are the same expression —
//! `(p.x * 2) + ((ex + 1) / 2)` — over the same four corners in the same order, emitting the
//! same `1,2,3 / 1,4,3` pair, dropping points outside `0..EXTENT` by the same test, and
//! splitting segments at the same `u16` ceiling. mbgl has two classes because each carries its
//! own `PaintPropertyBinders` as a member: seven properties for a circle, two for a heatmap.
//!
//! In this build the binders are not in the bucket — they are [`crate::paint::PaintBinder`],
//! built beside it from the layer's resolved paint. With the one difference between the two
//! classes living outside both of them, what is left is one bucket, and copying it would give
//! the fill-rate work two places to drift.
//!
//! # What the geometry does not carry
//!
//! `heatmap-radius` never reaches a vertex. The quad is a unit square; the vertex shader scales
//! the interpolated corner sign by the radius and by `extrude_scale`, exactly as a circle does.
//! One consequence is worth naming, because it reads like a defect in a profile: the quad is
//! about `2.56 * radius` pixels across — `S` for unit weight and intensity is 1.2817 — so a
//! large radius produces a quad far wider than the render target. That is not unbounded work.
//! The rasterizer clips it, and per-point fill cost is therefore bounded by the target's area,
//! which mbgl halves in each dimension for this pass. The cost that does scale is the point
//! count, and no clamp on the radius would reduce it.
//!
//! A clamp was considered here and is not present. There is no value it could take that leaves
//! the image alone: the shader's falloff is in units of radius, so capping the radius flattens
//! the kernel rather than cropping it, and every point on screen changes. Bounding the pass
//! belongs to the pass, not to the geometry.

pub use crate::circle::{CircleBucket as HeatmapBucket, build};
