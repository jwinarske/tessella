//! Glyph manager, PBF range path and local SDF rasterization — plan.md §7, descends from the
//! glyph half of mbgl `text/` and from `sprite/`.
//!
//! Owns the shared atlases: one glyph atlas per fontstack, one sprite atlas per style,
//! emitted once regardless of view count (§5.1). `contentHash` is retired from the protocol
//! precisely because this ownership makes consumer-side dedup unnecessary (§6.2).
//!
//! Two process-wide caches sit here (§12.3): a shaped-run cache keyed
//! `(fontstack, text, layout params)` — label text is massively repetitive across tiles,
//! zooms and views — and a glyph-SDF rasterization cache one level below it. Atlases are R8
//! single-channel, not RGBA: 4x on the largest persistent texture (§12.4). The shelf
//! allocator keeps insertions clustered so the §6.4 dirty-rect list rarely spills to union.
//!
//! Status: the glyph range format is read. Shaping, the atlas and local rasterization are
//! not implemented.

#![forbid(unsafe_code)]

pub mod atlas;
pub mod fonts;
pub mod generated;
pub mod manager;
pub mod pbf;
pub mod quads;
pub mod shaping;
pub mod text;

/// What a symbol layer needs to know about a glyph.
///
/// A trait rather than the glyph itself so a caller can answer from a manager, an atlas, or a
/// test's table, and so that layout does not decide how glyphs are stored. The two questions are
/// separate because they are answered at different times: the advance is known as soon as the
/// range is parsed, and the rectangle only once the glyph is packed.
pub trait Glyphs {
    /// How far the pen moves for this codepoint, and whether it has anything to draw.
    ///
    /// `None` when the font stack does not have it at all, which the shaper treats as a
    /// zero-width blank rather than as a reason to abandon the label.
    fn metrics(&self, codepoint: u32) -> Option<(crate::pbf::Metrics, bool)>;

    /// Where it sits in the atlas, once it is packed.
    fn rect(&self, codepoint: u32) -> Option<crate::atlas::Rect>;
}
